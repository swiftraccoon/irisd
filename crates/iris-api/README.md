# iris-api

The HTTP and WebSocket surface of irisd: catalog queries, byte-range HLS playlists, fMP4
export, live streaming, event listing and push, camera snapshots, health, and metrics. The
server never decodes video. Playback endpoints hand out byte ranges of the recording files
that the catalog describes, and the live endpoint wraps frames from the ingest live ring in
one `moof` + `mdat` each.

## What it provides

- `router(state)` — the axum router. `/api/health` and `/metrics` are public; every other
  route requires a bearer token when `ApiState::tokens` is not empty.
- `bind(listen)` plus `serve_listener(listener, router, shutdown)` — separates successful
  listener admission from serving; `serve` combines them for callers without a readiness gate.
- `ApiState` — the catalog handle, the live-ring registry, the event broadcast channel, the
  accepted tokens, active configured cameras, snapshot sources, request counters, monotonic
  start instant, and the shutdown token.
- `LiveRegistry` — live rings by stream, filled by the daemon as streams start.
- `SnapshotSource` — a camera's `snapshot.cgi` endpoint with digest credentials. Its host
  is a bare name or IP address; the snapshot URL builder brackets IPv6 authorities.
- `ApiStats` — request, client, and event WebSocket lag counters exposed on `/metrics`.
- `ApiError` — handler failures rendered as a JSON `{ "error": … }` body with the matching
  status.

| Route | Purpose |
|---|---|
| `GET /api/health` | liveness: status, uptime, version |
| `GET /metrics` | Prometheus text: requests, clients, event WebSocket lag, uptime, catalog queue/state totals, failed batches, admission rejections, pruning, and database/WAL sizes |
| `GET /api/cameras` | the configured cameras |
| `GET /api/cameras/{camera}` | the camera with the health of each recorded stream: connected, frames, fragments, bytes, reconnects, loss events, gaps, last frame time, codec, nominal frame duration, camera clock offset, live viewers |
| `GET /api/cameras/{camera}/snapshot.jpg` | a size-capped JPEG fetched without redirects from the camera with digest authentication |
| `GET /api/streams/{camera}/{kind}/segments?from&to` | catalog rows in a time range |
| `GET /api/streams/{camera}/{kind}/hls/vod.m3u8?from&to` | a byte-range VOD playlist over the range |
| `GET /api/streams/{camera}/{kind}/export.mp4?from&to` | a stand-alone recording of the range, streamed: the init segment with an edit list for start preroll, then fragments rebased from zero with fresh sequence numbers and trailers; tail samples at or after `to` are omitted and a crossing sample's duration is shortened (`docs/research/04-storage-format.md §10`) |
| `GET /api/streams/{camera}/{kind}/live` | WebSocket: an init segment, then one fragment per frame |
| `GET /api/streams/{camera}/{kind}/hls/live.m3u8[?_HLS_msn&_HLS_part]` | the low-latency live playlist from the live ring: 0.5 s parts, blocking reload (waits up to 6 s, then 503), preload hints; relative URIs `init.mp4`, `seg/{msn}/media.m4s`, `part/{msn}/{index}/part.m4s` beside it, served from memory (`docs/research/04-storage-format.md §10`) |
| `GET /api/streams/{camera}/{kind}/keyframe?t=` | a playable MP4 of one frame: the init segment and a single-sample fragment holding the sync sample at or before `t` (default: the newest), looked up within the two minutes before `t`; `x-iris-wall-us` carries the sample's time |
| `GET /api/segments/{id}/file.mp4` | one recording file, with HTTP range support |
| `POST /api/events/{id}/retain` | body `{"until": TIME}` keeps the footage around the event (30 s each side) from retention until then; `{"until": null}` or no body clears it; returns the event with `retain_until_us` |
| `GET /api/events?camera&from&to&limit&after_us&after_id` | bounded events in a range after an optional exclusive `(time,id)` cursor, or a WebSocket push when upgraded |

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

A client presents the root token as `Authorization: Bearer <token>`; playlists mint bounded
`token` query capabilities scoped to each exact media path so a player can follow them without
receiving the root token. Rolling live-HLS capabilities last 15 minutes. An immutable VOD
playlist's children last for its selected span plus a 15-minute playback margin, capped at seven
days; inherited capabilities cannot extend their expiry. The live
WebSocket sends binary messages whose first byte is the type: `0x01` an initialization
segment, `0x02` a media fragment holding one frame. Event push sends one JSON event per
text message. If an event receiver falls behind the bounded broadcast ring, the server closes
it with code 1013 and a bounded gap/retry reason; the client reconnects and lists the relevant
catalog window, which may omit events rejected by catalog admission. Every WebSocket write has a
two-second deadline and observes daemon shutdown, so a peer that stops reading cannot retain its
client slot through the daemon's shutdown grace.

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
- Playlists are byte ranges into the recording files, grouped three fragments per media
  segment with `EXT-X-INDEPENDENT-SEGMENTS`, an `EXT-X-MAP` per file, and a discontinuity at
  run boundaries (`docs/research/04-storage-format.md §10`).
- Clipping a continuation fragment includes the preceding sync fragment and every fragment
  between them. VOD presents this preroll; exports use `elst` to hide start preroll while
  retaining the samples needed to decode it. Tail samples starting at or after the requested
  end are removed from the serialized sample table and payload; a crossing sample's duration
  is shortened. The exclusive end rounds up to a 90 kHz media tick, and the edit's positive
  presentation duration rounds up to a millisecond. Requests shorter than either timescale
  remain nonempty; end clipping does not depend on the player's edit-list support.
- VOD playlists carry one `#IRIS-FRAGMENT:WALL-US=<i64>,DURATION-TICKS=<u32>` comment per
  served fragment, in playback order, including sync preroll. `WALL-US` is Unix microseconds;
  `DURATION-TICKS` uses 90,000 ticks per second. Clients sum ticks before converting units to
  map playback time through wall-clock gaps without accumulating per-fragment rounding.
- Export compares the selected files' initialization bytes before sending headers and returns
  HTTP 400 when they differ, including adopted files without catalog codec hashes. A client
  must request one codec configuration at a time.
- LL-HLS marks a non-increasing decode timestamp with `EXT-X-DISCONTINUITY` and resumes at
  the next sync frame. `EXT-X-DISCONTINUITY-SEQUENCE` preserves the epoch as segments leave
  the window; lag and codec changes also advance the epoch.
- Segment DTOs never expose server paths.
- Raw files stop at the complete extent captured from a bounded, validated sidecar snapshot;
  later appends cannot lengthen an in-flight full or range response. Sidecar headers bind the
  fixed timescale, initialization length, known run/sample-entry facts, catalog start, and
  physical extent. Fragment transforms verify CRC, wall/decode time, duration, sample count,
  and sync state first. Skipped VOD rows produce one discontinuity before the next emitted row.
- One request admits at most 4,096 segment rows, 131,072 records per sidecar, 100,000 selected
  records, a 16 MiB playlist, a 4 MiB initialization section, and a 66 MiB stored fragment.
  At most eight disk-backed media responses run process-wide; each admission is held through
  detached sidecar/read/encode work and final response-buffer/body drop. Raw bodies reserve their
  capacity-one channel before each bounded read and attach that admission to every emitted chunk.
  Export reserves its sole queue slot before materializing the next bounded fragment.
- The keyframe lookup never selects a sync fragment before its two-minute lower bound. An omitted
  `t` is mutable and uses `private, no-store`; an explicit historical `t` may cache privately.
- `/api/cameras` and camera detail use the running configuration, while historical catalog rows
  remain queryable through recording routes. `live_viewers` counts external live WebSockets only.
- `/metrics` coalesces catalog aggregates into one cache-owned refresh per second. The refresh
  continues if its initiating scrape is cancelled and serves its last snapshot after a failure.
  Monotonic catalog counters include failed SQLite change batches as
  `irisd_catalog_batch_failures_total`. Health and metrics uptime comes from a monotonic process
  instant.
- Snapshots are streamed through an 8 MiB cap, must have JPEG start/end markers, and are served as
  `image/jpeg` with `nosniff` and `no-store`. Redirects are rejected, including same-origin
  redirects, and public failures do not include the configured camera URL.
- Catalog overload returns HTTP 503. Marking retained footage also returns 503 after the
  active-mark capacity reaches 100,000 rows or 128 MiB of represented event strings.
- The authoritative API surface is documented in `docs/research/07-architecture.md §5` and
  the crate contract in `docs/design/phase2-interfaces.md`.

## Tests and measurements

`tests/api.rs` builds a real recording through the store and the catalog, then exercises
health without a token, token rejection and acceptance, segment listing and playlist
construction, range serving and export over the recording, and rejection of unknown
segments and bad ranges. Regression coverage checks Unicode timestamps, integer bounds,
sync preroll, per-fragment timing metadata, mixed codec export rejection, and LL-HLS timestamp
resets. Export boundary tests verify checksum-valid fragments and positive edit durations
for sub-tick requests and ends around sample and fragment boundaries. The `api_time` fuzz
target compares the timestamp parser with an independent Gregorian/RFC 3339 semantic oracle.
Synthetic loopback tests cover snapshot size, content, redirect, truncation, and cancellation
boundaries plus WebSocket peer-close, backpressure-deadline, and shutdown cleanup. Live integration
against real cameras, playback through ffprobe,
ffmpeg, and GStreamer, and the hourly playlist-continuity check of the soak are recorded in
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
