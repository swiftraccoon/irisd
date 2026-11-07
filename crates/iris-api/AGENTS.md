# AGENTS.md — iris-api

Scope: this crate only. Read the root `AGENTS.md` first.

## Invariants

- The route table in `lib.rs` is the API surface fixed by `docs/research/07-architecture.md §5`
  and `docs/design/phase2-interfaces.md`; adding or renaming a route updates both.
- Every route under `/api` except `/api/health` goes through `auth::require_token`;
  `/metrics` does too unless `ApiState::public_metrics` is set. Media capabilities never
  authorize `/metrics`. Tokens are compared in constant time.
- `server.rs` serves HTTP/1.1 only, takes a connection slot before each `accept` (the slot
  lives in the connection's stream, so upgraded WebSockets keep it), applies the 10 s
  request-head deadline through hyper's timer, and fails a write that makes no progress for
  the write-stall deadline. Shutdown drains in-flight responses through hyper's graceful
  shutdown and waits for every connection task.
- Playlist and export semantics follow `docs/research/04-storage-format.md §10`: byte ranges
  into recording files, three fragments per media segment, `EXT-X-MAP` per file,
  discontinuities at run boundaries. The server never decodes or re-muxes.
- Responses never contain server paths or credentials; `SegmentDto` is the only shape a
  segment takes on the wire.
- Snapshot sources carry bare hosts; the snapshot URL builder brackets IPv6 authorities.
- Snapshot fetches never follow redirects, stream through the 8 MiB limit, require `FF D8 FF`
  at the start and an `FF D9` within the last 64 KiB, serve through the final `FF D9`, run one
  at a time per camera, and return fixed `image/jpeg`, `nosniff`, and `no-store` headers
  without URL-bearing errors.
- The live WebSocket message bytes (`0x01` init, `0x02` fragment) are consumed by the clients
  in `docs/research/06-clients.md`. A message never exceeds `MAX_LIVE_MESSAGE_BYTES` (one type
  byte plus 16 MiB, the viewer's limit): an oversized dependent frame resyncs like lag, and an
  oversized sync frame closes with 1009. Init-segment decisions compare sample entries by content,
  never by CRC-32C alone.
- Time parameters accept Unix microseconds or RFC 3339 and are bounded to seven days
  (`query.rs`). RFC 3339 parsing rejects non-ASCII input before byte-indexed slicing; range
  spans and default starts use checked arithmetic. Gregorian dates, clocks, and offsets are
  validated; leap seconds are rejected. The parser is shared with `api_time` fuzzing.
- Event list continuation requires `after_us` and a positive `after_id` together and passes
  the exclusive `(nvr_time_us, id)` cursor to the catalog. A time-only cursor can omit tied rows.
- An event WebSocket that falls behind closes with code 1013 and a bounded gap/retry reason;
  the API counts the messages skipped by lagging receivers.
- Live and event WebSocket writes observe shutdown and have a two-second deadline. RAII guards
  release client gauges on cancellation, peer close, send failure, or timeout.
- WebSocket upgrades take their socket slots before `on_upgrade` (64 live, 16 per stream, 32
  event) and hold them for the session. Inbound messages, frames, and the read buffer are
  limited to 4 KiB (`websocket.rs`); any data message or read failure ends the session with
  close code 1008.
- Catalog admission overload and active event-retention capacity exhaustion map to HTTP 503;
  malformed cursor pairs map to HTTP 400. Full admission budgets are `ApiError::Busy`: HTTP 503
  with `Retry-After: 1`. HTTP 500 bodies are generic with a `request_id`; never put internal
  detail in a 500 body.
- Range clipping includes sync preroll, so VOD and export never start with a dependent
  fragment. Export uses `elst` for start preroll and removes encoded tail samples at the
  requested end, shortening a crossing sample's duration. The exclusive end rounds up to
  a 90 kHz tick; the edit duration derives from that clipped media and rounds up to a
  millisecond, preserving nonempty sub-tick requests. It compares actual initialization
  bytes before headers, rejecting incompatible files with HTTP 400 even without catalog hashes.
- VOD `#IRIS-FRAGMENT` comments list the selected records in order with Unix microseconds
  and 90 kHz duration ticks; GUI playback mapping consumes this metadata.
- Disk-backed serving opens media and sidecar together beneath the approved root that owns the
  row (`hls::published_index`), validates the bounded sidecar read from its descriptor against
  catalog/header/media facts, and captures one complete published extent plus the media's
  device and inode (`hls::ValidatedMedia`). Raw bodies read only the validated descriptor;
  exports and keyframes reopen beneath the root and require the same inode and extent. Never
  open recordings by pathname. Rows outside every root are 404 and counted. A zero header run
  identifier or sample-entry hash is unknown (rebuilt by recovery), never a mismatch. Raw
  full/range bodies, VOD ranges, exports, and keyframes never read past the extent; an early end
  of file aborts a raw body with an error; transforms cross-check source CRC, timing, sample
  count, and sync state.
- Serving admits at most 4,096 rows, `MAX_PERSISTED_INDEX_RECORDS` records per sidecar, 100,000
  selected records, a 16 MiB playlist, 4 MiB init, 66 MiB fragment, and one queued export
  fragment. Media admission has two classes (`MediaClass`): streaming (whole or open-ended raw
  files, ranges above 1 MiB, exports; default 8) and bounded (VOD, keyframes, LL-HLS
  resources, ranges of at most 1 MiB; default 32). Admission remains owned by detached
  sidecar/read/encode work and final response-buffer/body drop. Raw bodies reserve their
  capacity-one channel before each read and attach admission to every emitted chunk. Export
  reserves queue capacity before materializing the next fragment. Do not replace these with an
  unbounded scan fallback.
- Active-camera JSON comes from `ApiState::active_cameras`, not historical catalog identities.
  Per-stream viewers count external live WebSockets, never the shared LL-HLS feeder.
- Metrics catalog aggregates refresh at most once per second under one cache-owned single-flight
  that survives requesting-scrape cancellation and uses the last snapshot on refresh failure.
  API uptime uses `started_monotonic`.
- LL-HLS timestamp resets resume at sync with a discontinuity. Each GOP carries its
  discontinuity sequence so removing playlist entries preserves the epoch.

## Where things are

- `lib.rs` — router and listener binding.
- `server.rs` — `ServeLimits` and `serve_listener`: the HTTP/1.1 connection loop, connection
  admission, request-head and write-stall deadlines, and graceful shutdown.
- `websocket.rs` — the inbound limits and 1008 policy close shared by both WebSockets.
- `state.rs` — `ApiState`, `LiveRegistry`, `SnapshotSource`, `ApiStats`.
- `auth.rs` — bearer-token middleware, request auth context, and exact-path media-capability
  issuer/verifier.
- `query.rs` — `from`/`to` parsing, defaults, and bounds.
- `query/time.rs` — allocation-free timestamp parser, included by the `api_time` fuzz target.
- `cameras.rs`, `segments.rs`, `events.rs`, `health.rs`, `metrics.rs` — the JSON and text
  endpoints.
- `hls.rs` — VOD playlist construction from sidecar indexes.
- `llhls.rs` — the low-latency live playlist: the per-stream feeder over the live ring, part
  cutting, playlist rendering, blocking reload, the segment and part responses.
- `media_admission.rs` — shared permit ownership across detached media work and response bodies.
  The two media classes, the WebSocket caps, and snapshot single-flight live in `ApiStats`.
- `keyframe.rs` — the keyframe resource: one sync sample re-encoded behind the init
  segment, found through `hls::load_fragments` within two minutes before `t`.
- `export.rs` — streamed export of a range.
- `live.rs` — WebSocket live session over a `LiveRing`.
- `snapshot.rs` — digest-authenticated snapshot proxy.
- `error.rs` — `ApiError` and its HTTP mapping.

## How to verify a change

```sh
cargo fmt -p iris-api
cargo clippy -p iris-api --all-targets -- -D warnings
cargo test -p iris-api
RUSTDOCFLAGS="-D warnings" cargo doc -p iris-api --no-deps
```

`tests/api.rs` writes a real recording through `iris-store` into a temporary directory; a
playback change should also be checked with `ffprobe` against a playlist served by
`irisd serve` or the daemon, and against the replay server rather than live cameras.

## Pitfalls

- An authenticated playlist URI carries a bounded capability scoped to its exact media path; it
  never carries the caller's root bearer. Rolling live-HLS capabilities last 15 minutes; immutable
  VOD children cover the selected span plus a 15-minute margin, capped at seven days.
  Authentication-disabled playlists need no query credential.
- Byte ranges that span several fragments include the 24-byte trailers between them; the
  playlist math in `hls.rs` relies on sidecar records, not on file sizes.
- A skipped sidecar after emitted VOD media sets one pending discontinuity; a skipped leading
  sidecar does not add a gratuitous marker.
- `export.rs` streams through a bounded channel and reserves its one slot before reading the next
  fragment; a slow client must not hold a blocking read on the runtime thread.
- `ApiState` accepts an empty token list and logs that every endpoint is unauthenticated; the
  daemon's configuration accepts an empty list only with `[api] allow_unauthenticated = true`
  on a loopback listener (`iris_core::config`). Tests cover both modes.
- Tests that serve recordings admit a root (`iris_store::admit_recording_root` with
  `RootExpectation::Fresh` on an empty or absent directory) and record beneath it in the
  four-component layout; `ApiState::recording_roots` must list it. Do not plant FIFOs in API
  tests: the store's descriptor opens decide FIFO behaviour, and its tests own it.
- `MAX_RANGE_US` bounds every range query; listings without `from` default to one hour.
- Event pages also inherit the catalog's 10,000-row and 8 MiB approximate
  materialized-owned-row budgets; that estimate is not the encoded JSON response size.

## Related documents

- `docs/research/07-architecture.md` §5, §6, §9; `docs/research/04-storage-format.md` §10;
  `docs/research/06-clients.md`; `docs/design/phase2-interfaces.md`;
  `docs/research/10-phase2-results.md`.
