# iris-api

The HTTP and WebSocket surface of irisd: catalog queries, byte-range HLS playlists, fMP4
export, live streaming, event listing and push, camera snapshots, health, and metrics. The
server never decodes video. Playback endpoints hand out byte ranges of the recording files
that the catalog describes, and the live endpoint wraps frames from the ingest live ring in
one `moof` + `mdat` each.

## What it provides

- `router(state)` — the axum router. `/api/health` is public. `/metrics` is public only when
  `ApiState::public_metrics` is set (`[api] public_metrics`); otherwise it requires a root
  bearer token, and media capabilities never authorize it. Every other route requires a
  bearer token when `ApiState::tokens` is not empty.
- `bind(listen)` plus `serve_listener(listener, router, limits, shutdown)` — separates
  successful listener admission from serving; `serve` combines them for callers without a
  readiness gate. `ServeLimits` sets the connection cap, the request-head deadline, and the
  write-stall deadline.
- `ApiState` — the catalog handle, the live-ring registry, the event broadcast channel, the
  accepted tokens, the capability secret, the approved recording roots, the public-metrics
  switch, the running cameras (configured or added through the API), snapshot sources, the
  optional camera control, request counters, the wall and monotonic start instants, and the
  shutdown token. The daemon assembles it once; its fields are that assembly record.
- `CapabilitySecret` and `CAPABILITY_SECRET_BYTES` — the 32 random bytes that sign media
  capabilities, kept by the daemon in its state directory; `Debug` redacts them.
- `ActiveCamera` — one running camera as the camera routes present it: id, name, vendor, and
  whether the API added it.
- `CameraControl`, `ControlRequest`, and `ProvisionError` — the daemon's control contract for
  provisioning and the ONVIF lookups: the routes send requests through the control and the
  daemon answers each with a result or a `ProvisionError`, whose variants map to the statuses
  in the route table. `ControlRequest` stays exhaustive on purpose, so the daemon's match is
  compile-checked.
- `LiveRegistry` — live rings by stream, filled by the daemon as streams start.
- `SnapshotSource` — where a camera's snapshots come from, with digest credentials: the Dahua
  CGI endpoint or the URL its ONVIF Media service names, resolved on first use and shared by the
  source's clones. Built with `SnapshotSource::new`; its host is a bare name or IP address, and
  the snapshot URL builder brackets IPv6 authorities. `Debug` redacts both credentials.
- `ApiStats` — request, client, event WebSocket lag, and admission-rejection counters exposed
  on `/metrics`, plus the shared admission budgets; `ApiStats::new_with_media_limits` sizes the
  two media classes.
- Handler failures are rendered as a JSON `{ "error": … }` body with the matching status.
  HTTP 500 bodies are `{"error":"internal error","request_id":<n>}`; the detail is logged at
  warn level with the same id. A full admission budget is HTTP 503 with `Retry-After: 1`. A
  JSON body an extractor refuses (missing or mistyped field, wrong `Content-Type`) is HTTP 400
  with the same body shape; one over the 64 KiB limit is HTTP 413. Store and lifecycle
  failures of provisioning answer a fixed 503 body, with their detail in the log.
- `MSG_INIT`, `MSG_FRAGMENT`, and `MAX_LIVE_MESSAGE_BYTES` — the live WebSocket message type
  bytes and the largest message, one type byte plus a 16 MiB payload.

| Route | Purpose |
|---|---|
| `GET /api/health` | liveness: status, uptime, version |
| `GET /metrics` | Prometheus text: requests, clients, event WebSocket lag, API admission rejections (media classes, WebSocket caps, snapshots), rows outside the approved roots, oversized live closes, uptime, catalog queue/state totals, rows and bytes in deletion intent, failed batches, catalog admission rejections, pruning, and database/WAL sizes; root bearer token required unless `public_metrics` is set |
| `GET /api/cameras` | the running cameras, configured or added through the API; `provisioned` marks the added ones, which `DELETE` applies to |
| `POST /api/cameras` | adds a camera described like a `[[camera]]` table (JSON) and answers 201 with it as the list shows it; needs `[provisioning] enabled` on the daemon (403 otherwise) and the root token; 400 for an invalid camera, a port outside `provisioning.connect_ports`, or the `max_cameras` limit, 409 for an id in use, 503 when the daemon cannot write its store or start the camera |
| `DELETE /api/cameras/{camera}` | stops and forgets a camera the API added (204); 409 for a configured camera, 404 for an unknown one; recordings and catalog rows stay |
| `GET /api/onvif/devices?wait_ms` | the cameras a WS-Discovery probe finds within `wait_ms` (default 3000, 500 to 10000): device service host and port, and the name, hardware, and location scopes, bounded and sanitised; needs `[provisioning] enabled` (403 otherwise) and the root token; 503 when the probe cannot be sent or four lookups run already |
| `POST /api/onvif/streams` | body `{"host", "http_port"?, "https"?, "tls_verify"?, "username", "password"}`: the RTSP URLs of the camera's ONVIF media profiles (token, name, URL), each moved onto `host`; gated like discovery; 400 for a host that is not a bare name or address or a port outside `connect_ports`, 502 when the camera gives no usable answer |
| `GET /api/cameras/{camera}` | the camera with the health of each recorded stream: connected, frames, fragments, bytes, reconnects, loss events, gaps, last frame time, codec, nominal frame duration, camera clock offset, live viewers |
| `GET /api/cameras/{camera}/snapshot.jpg` | a size-capped JPEG fetched without redirects or a proxy from the camera with digest authentication, from the Dahua CGI endpoint or, for a camera whose `snapshot` is `onvif` (the default when its events come from ONVIF pull-point alone), from the URL its ONVIF Media service names for its first profile, resolved on first use, kept for an hour, forgotten after a failed fetch, and rebased onto the configured camera; one request per camera at a time, served through its final end-of-image marker |
| `GET /api/streams/{camera}/{kind}/segments?from&to` | catalog rows in a time range |
| `GET /api/streams/{camera}/{kind}/hls/vod.m3u8?from&to` | a byte-range VOD playlist over the range |
| `GET /api/streams/{camera}/{kind}/export.mp4?from&to` | a stand-alone recording of the range, streamed: the init segment with an edit list for start preroll, then fragments rebased from zero with fresh sequence numbers and trailers; tail samples at or after `to` are omitted and a crossing sample's duration is shortened (`docs/research/04-storage-format.md §10`) |
| `GET /api/streams/{camera}/{kind}/live` | WebSocket: an init segment, then one fragment per frame, each message at most 16 MiB + 1 byte |
| `GET /api/streams/{camera}/{kind}/hls/live.m3u8[?_HLS_msn&_HLS_part]` | the low-latency live playlist from the live ring: parts of up to 0.5 s cut by media time, blocking reload (waits up to three target durations, 12 s before the first segment completes, then 503; a directive more than two segments or three seconds of parts ahead is 400), preload hints; relative URIs `init.mp4`, `seg/{msn}/media.m4s`, `part/{msn}/{index}/part.m4s` beside it, served from memory (`docs/research/04-storage-format.md §10`) |
| `GET /api/streams/{camera}/{kind}/keyframe?t=` | a playable MP4 of one frame: the init segment and a single-sample fragment holding the sync sample at or before `t` (default: the newest), looked up within the two minutes before `t`; `x-iris-wall-us` carries the sample's time |
| `GET /api/segments/{id}/file.mp4` | one recording file, with HTTP range support: one `bytes` range, clamped to the validated extent; a range unit other than `bytes` is ignored and the whole file served; several ranges or an unsatisfiable one answer 416 |
| `POST /api/events/{id}/retain` | body `{"until": TIME}` keeps the footage around the event (30 s each side) from retention until then; `{"until": null}` or an empty body clears it; the body is read as JSON whatever its `Content-Type`, and an unknown field or an unparseable time is 400; returns the event with `retain_until_us` |
| `GET /api/events?camera&from&to&limit&after_us&after_id` | bounded events in a range after an optional exclusive `(time,id)` cursor, or a WebSocket push when upgraded; `limit` defaults to 200 and is clamped to 1–10,000, so a full page may continue from its last `(nvr_time, id)` |

`from` and `to` accept Unix microseconds or RFC 3339; `from` defaults to an hour before
`to`, `to` to the present, and a range may span at most seven days. Malformed Unicode
timestamps, impossible Gregorian dates, out-of-range clocks or offsets, and leap seconds return
HTTP 400. Leap seconds are rejected because the recorder has no leap-second table. Ranges whose
span or default start cannot be represented also return HTTP 400.
`after_us` and a positive `after_id` must appear together. A client uses the final event's
`nvr_time` and `id` to continue through equal-timestamp rows without repeats or omissions.
Each listing returns at most 10,000 rows under the catalog's 8 MiB approximate
materialized-owned-row budget; JSON encoding can require more response bytes.

## How it is used

A client presents the root token as `Authorization: Bearer <token>`; playlists mint bounded `token`
query capabilities scoped to each exact media path so a player can follow them without receiving the
root token. A capability is an HMAC-SHA256 tag under `ApiState::capability_secret`, the 32-byte
secret the daemon keeps in its state directory; root tokens take no part in it, so a captured URL
reveals nothing about a token. Only the paths playlists issue capabilities for accept one: a VOD
playlist's recording files and the live playlist's init, segment, and part resources; playlists
themselves, the live WebSocket, and every other route take the root token. Rolling live-HLS
capabilities last 15 minutes, their expiry rounded down to the minute so every playlist reload
within it mints identical child URIs. An immutable VOD playlist's children last for its selected
span plus a 15-minute playback margin, capped at seven days; inherited capabilities cannot extend
their expiry. The live WebSocket sends binary messages
whose first byte is the type: `0x01` an initialization segment, `0x02` a media fragment holding one
frame. A message never exceeds `MAX_LIVE_MESSAGE_BYTES`, the type byte plus a 16 MiB payload, which
is what the viewer accepts. A dependent frame that would not fit is skipped, and the stream resumes
at the next sync frame with a fresh initialization segment, as after lag; a sync frame that would
not fit closes the socket with code 1009 (message too big), counted as
`irisd_live_oversized_closes_total`. Event push sends one JSON event per text message. If an event
receiver falls behind the bounded broadcast ring, the server closes it with code 1013 and a bounded
gap/retry reason; the client reconnects and lists the relevant catalog window, which may omit events
rejected by catalog admission. Every WebSocket write has a two-second deadline and observes daemon
shutdown, so a peer that stops reading cannot retain its client slot through the daemon's shutdown
grace. Clients send no data messages: a text or binary message, or anything beyond the 4 KiB inbound
message and frame limit, ends the session with close code 1008 (policy violation); pings and closes
are answered as usual.

```sh
curl -H 'Authorization: Bearer TOKEN' 'http://127.0.0.1:8554/api/cameras'
curl -H 'Authorization: Bearer TOKEN' \
  'http://127.0.0.1:8554/api/streams/cam1/main/hls/vod.m3u8?from=2026-01-01T00:00:00Z&to=2026-01-01T00:10:00Z'
```

The daemon constructs `ApiState`, registers each stream's live ring, binds the listener, and
spawns `serve_listener` before reporting readiness.

## Invariants and limits

- Authentication is a constant-time bearer-token comparison; an empty token list disables it
  and is logged as a warning.
- The listener speaks HTTP/1.1 only: every client in the repository uses it, and cleartext
  HTTP/2 would add many streams per connection behind a protocol sniff no deadline covers. At
  most `[api] max_connections` (default 256, 16 to 65,536) connections are served at once,
  upgraded WebSockets included; the loop takes a slot before each `accept`, so further clients
  wait in the kernel's listen backlog instead of holding descriptors. A request head must
  arrive within 10 s of the connection becoming ready for one, which also closes idle
  kept-alive connections. A response write that makes no progress for 30 s closes its
  connection and drops the body, releasing its media admission; any accepted byte restarts
  that deadline, so slow readers are kept. Shutdown stops accepting and lets each connection
  finish its in-flight response.
- Playlists are byte ranges into the recording files. A media segment starts at a sync
  fragment and runs until the first sync fragment after its third fragment, continuation
  fragments always joining it, with `EXT-X-INDEPENDENT-SEGMENTS`, an `EXT-X-MAP` per file, and
  a discontinuity at run boundaries, after a skipped row, and where the sample entry changes
  (`docs/research/04-storage-format.md §10`).
- Clipping a continuation fragment includes the preceding sync fragment and every fragment
  between them. VOD presents this preroll; exports use `elst` to hide start preroll while
  retaining the samples needed to decode it. Tail samples starting at or after the requested
  end are removed from the serialized sample table and payload; a crossing sample's duration
  is shortened. Only the final fragment is cut, so a fragment before it whose wall interval
  reaches past the end stays whole. The exclusive end rounds up to a 90 kHz media tick, and
  the edit's positive presentation duration rounds up to a millisecond. Requests shorter
  than either timescale remain nonempty; end clipping does not depend on the player's
  edit-list support.
- VOD playlists carry one `#IRIS-FRAGMENT:WALL-US=<i64>,DURATION-TICKS=<u32>` comment per
  served fragment, in playback order, including sync preroll. `WALL-US` is Unix microseconds;
  `DURATION-TICKS` uses 90,000 ticks per second. Clients sum ticks before converting units to
  map playback time through wall-clock gaps without accumulating per-fragment rounding.
- Export compares the selected files' initialization bytes before sending headers and returns
  HTTP 400 when they differ, including adopted files without catalog codec hashes. A client
  must request one codec configuration at a time.
- LL-HLS follows the vendored HLS draft: `EXT-X-TARGETDURATION` is pinned per feeder when
  the first GOP completes (its length rounded up to whole seconds plus one second, at most the
  10 s GOP budget) and never changes; `PART-TARGET` is pinned from the first frame's duration
  (what the whole frames that fit 0.5 s add up to, at least one frame), and parts are cut by
  media time against it, so no part outlasts the target when frame durations change and every
  non-final part lasts exactly the target while they do not; complete segments stay listed
  until the complete segments after them cover three target durations, at most 12;
  `EXT-X-VERSION` is 6; each segment after a discontinuity
  carries `EXT-X-PROGRAM-DATE-TIME`; and a blocking reload waits three target durations. A
  GOP that would outlast the pinned target, or exceed 16 MiB or 600 frames, is cut where it is
  with `EXT-X-DISCONTINUITY`, as is the open segment on ring lag or a non-increasing decode
  timestamp, and dependent frames are dropped until the next sync frame. A new sample entry
  starts the window over with a new init segment. `EXT-X-DISCONTINUITY-SEQUENCE` preserves
  the epoch as segments leave the window. Cameras whose GOP length varies beyond one second
  therefore get cut segments; a fixed I-frame interval suits LL-HLS.
- Each watched stream keeps at most 12 complete GOPs of up to 16 MiB, 600 frames, and the
  pinned target each (about five GOPs, 15 to 30 MiB, at a 2 s 4K GOP), beyond the live ring's
  own windows; each bounded-class segment response encodes a copy of up to one GOP, so the
  default 32 bounded slots can hold about 0.5 GiB of response buffers.
- Segment DTOs never expose server paths.
- Media is read only through descriptors opened beneath the approved recording root that owns
  a row's path (`ApiState::recording_roots`), with `openat2` refusing symlinks, magic links,
  and mount crossings at every component. A row outside every root returns HTTP 404 and counts
  as `irisd_api_unapproved_rows_total`. The media file and its sidecar are opened together, the
  sidecar is read from its descriptor, and the media length comes from the open file. Raw full
  and range bodies are served from that same descriptor, so a later path swap cannot change the
  bytes; exports and keyframes reopen beneath the root and require the validated device, inode,
  and at least the validated extent. A file that ends early under a response aborts it with an
  error and a warning instead of ending it cleanly.
- Raw files stop at the complete extent captured from a bounded, validated sidecar snapshot;
  later appends cannot lengthen an in-flight full or range response. Sidecar headers bind the
  fixed timescale, initialization length, known run/sample-entry facts, catalog start, and
  physical extent. A header run identifier or sample-entry hash of zero, as crash recovery
  rebuilds them from the media scan, is unknown and accepted; a nonzero mismatch is rejected.
  Fragment transforms verify CRC, wall/decode time, duration, sample count,
  and sync state first. Skipped VOD rows produce one discontinuity before the next emitted row.
- One request admits at most 4,096 segment rows, 131,072 records per sidecar
  (`iris_mp4::index::MAX_PERSISTED_INDEX_RECORDS`), 100,000 selected records, a 16 MiB
  playlist, a 4 MiB initialization section, and a 66 MiB stored fragment.
- Media responses are admitted process-wide in two classes. Streaming: whole or open-ended
  recording files, ranges above 1 MiB, and exports (`[api] max_streaming_media`, default 8).
  Bounded: VOD playlists, keyframes, the memory-served LL-HLS initialization, segment, and
  part resources, and ranges of at most 1 MiB (`[api] max_bounded_media`, default 32). A full
  class answers HTTP 503 with `Retry-After: 1` and counts `irisd_api_media_rejected_total`;
  the other class is unaffected. Each admission is held through detached sidecar/read/encode
  work and final response-buffer/body drop, and a client that stops reading loses it at the
  30 s write-stall deadline; range-capable players resume with a `Range` request. Raw bodies
  reserve their capacity-one channel before each bounded read and attach that admission to
  every emitted chunk. Export reserves its sole queue slot before materializing the next
  bounded fragment.
- WebSockets admit at most 64 live sessions process-wide and 16 per stream, and 32 event
  sessions; an upgrade beyond a cap answers HTTP 503 with `Retry-After: 1` and counts
  `irisd_api_websocket_rejected_total`. Inbound messages and frames are limited to 4 KiB, and
  each socket's write buffer holds at most one outbound message beyond its 128 KiB target.
- A live sample entry is compared by content, not only by its CRC-32C, before the server
  decides whether to announce another initialization segment; LL-HLS applies the same rule.
- The keyframe lookup never selects a sync fragment before its two-minute lower bound; a GOP
  longer than that answers 404. An omitted `t`, or one within 30 s of the present, is mutable
  and uses `private, no-store`; an explicit historical `t` may cache privately for an hour.
- `/api/cameras` and camera detail use the running cameras, configured or added through the API,
  while historical catalog rows remain queryable through recording routes. `live_viewers` counts
  external live WebSockets only.
- Provisioning relays through `ApiState::control`, the daemon's `CameraControl`: the route
  validates the camera with `CameraConfig::validate`, the daemon applies the limit, the allowed
  ports, the store, and the task lifecycle, and answers within 20 s or the route reports 503.
  A media capability never authorizes these routes, and protected routes take at most 64 KiB
  of request body.
- `/metrics` coalesces catalog aggregates into one cache-owned refresh per second. The refresh
  continues if its initiating scrape is cancelled and serves its last snapshot after a failure.
  Monotonic catalog counters include failed SQLite change batches as
  `irisd_catalog_batch_failures_total`. Health and metrics uptime comes from a monotonic process
  instant.
- Snapshots are streamed through an 8 MiB cap and must start with the JPEG start-of-image and
  first segment markers (`FF D8 FF`) and contain an end-of-image marker (`FF D9`) within their
  last 64 KiB. The response ends at the final such marker, so camera padding and trailers are
  dropped, and is served as `image/jpeg` with `nosniff` and `no-store`. One request per camera
  is in flight at a time; another answers HTTP 503 with `Retry-After: 1` and counts
  `irisd_api_snapshot_rejected_total`. Redirects are rejected, including same-origin
  redirects, and public failures do not include the configured camera URL.
- Catalog overload returns HTTP 503 without `Retry-After`. Marking retained footage also returns
  503 after the active-mark capacity reaches 100,000 rows or 128 MiB of represented event
  strings. HTTP 500 bodies carry only `"internal error"` and a process-wide `request_id`; the
  failure is logged at warn level with that id.
- The authoritative API surface is documented in `docs/research/07-architecture.md §5` and
  the crate contract in `docs/design/phase2-interfaces.md`.

## Tests and measurements

`tests/api.rs` builds a real recording through the store and the catalog, then exercises health
without a token, token rejection and acceptance, segment listing and playlist construction, range
serving and export over the recording, and rejection of unknown segments and bad ranges. Regression
coverage checks Unicode timestamps, integer bounds, sync preroll, per-fragment timing metadata,
mixed codec export rejection, and LL-HLS timestamp resets. Export boundary tests verify
checksum-valid fragments and positive edit durations for sub-tick requests and ends around sample
and fragment boundaries. The `api_time` fuzz target compares the timestamp parser with a second
Gregorian/RFC 3339 implementation and a `Display` round trip. Synthetic loopback tests cover
snapshot size, content, padding, redirect, truncation, cancellation, and single-flight boundaries
plus WebSocket peer-close, backpressure-deadline, and shutdown cleanup. `tests/listener.rs` covers
the serving loop: the request-head deadline for silent, partial, and idle kept-alive connections, a
connection cap that admits exactly its limit, shutdown draining an in-flight response, and an HTTP/2
preface that is never answered with HTTP/2. `tests/provisioning.rs` drives both camera provisioning
routes and both ONVIF routes through a stand-in control loop: status mapping, the JSON rejection
statuses, the 64 KiB body cap, root-token and control gating. `tests/keyframe_bounds.rs` covers
extreme keyframe timestamps, `tests/metrics_cancellation.rs` cancelled scrapes sharing one refresh,
and `tests/websocket_lifetime.rs` peer close, the send deadline, shutdown, and viewer counts. The
`api_range` fuzz target drives the raw route's `Range` parser. `tests/media_cancellation.rs` stalls,
slows, and disconnects readers of multi-megabyte recordings through loopback sockets with small
buffers: a stalled reader keeps its streaming slot only until the write-stall deadline, a slow
reader is served to the end, and a disconnect returns its slot. `tests/media_binding.rs` covers a
symlinked recording and sidecar on every media route, a path swapped after validation, truncation
under a response, and rows outside every approved root. `tests/websocket_limits.rs` covers the 1008
policy close for inbound data and oversized messages, the live and event socket caps, the oversized
live frame skip and 1009 close, a message exactly at the limit, and a forged CRC-32C collision
between sample entries. Live integration against real cameras, playback through ffprobe, ffmpeg, and
GStreamer, and the hourly playlist-continuity check of the soak are recorded in
`docs/research/10-phase2-results.md` and `11-multi-camera-results.md`.

## Further reading

- [`docs/research/07-architecture.md`](../../docs/research/07-architecture.md) §5 (API
  surface), §6 (security model), §9 (observability).
- [`docs/research/04-storage-format.md`](../../docs/research/04-storage-format.md) §10
  (serving from the format).
- [`docs/research/06-clients.md`](../../docs/research/06-clients.md) — what the iOS and GUI
  clients consume.
- [`docs/design/phase2-interfaces.md`](../../docs/design/phase2-interfaces.md) — the crate
  contract.
- [`STANDARDS.md`](../../STANDARDS.md).
