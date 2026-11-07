# AGENTS.md — iris-api

Scope: this crate only. Read the root `AGENTS.md` first.

## Invariants

- The route table in `lib.rs` is the API surface fixed by `docs/research/07-architecture.md §5`
  and `docs/design/phase2-interfaces.md`; adding or renaming a route updates both.
- Every route under `/api` except `/api/health` goes through `auth::require_token`;
  `/metrics` is public. Tokens are compared in constant time.
- Playlist and export semantics follow `docs/research/04-storage-format.md §10`: byte ranges
  into recording files, three fragments per media segment, `EXT-X-MAP` per file,
  discontinuities at run boundaries. The server never decodes or re-muxes.
- Responses never contain server paths or credentials; `SegmentDto` is the only shape a
  segment takes on the wire.
- Snapshot sources carry bare hosts; the snapshot URL builder brackets IPv6 authorities.
- Snapshot fetches never follow redirects, stream through the 8 MiB limit, require JPEG markers,
  and return fixed `image/jpeg`, `nosniff`, and `no-store` headers without URL-bearing errors.
- The live WebSocket message bytes (`0x01` init, `0x02` fragment) are consumed by the clients
  in `docs/research/06-clients.md`.
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
- Catalog admission overload and active event-retention capacity exhaustion map to HTTP 503;
  malformed cursor pairs map to HTTP 400.
- Range clipping includes sync preroll, so VOD and export never start with a dependent
  fragment. Export uses `elst` for start preroll and removes encoded tail samples at the
  requested end, shortening a crossing sample's duration. The exclusive end rounds up to
  a 90 kHz tick; the edit duration derives from that clipped media and rounds up to a
  millisecond, preserving nonempty sub-tick requests. It compares actual initialization
  bytes before headers, rejecting incompatible files with HTTP 400 even without catalog hashes.
- VOD `#IRIS-FRAGMENT` comments list the selected records in order with Unix microseconds
  and 90 kHz duration ticks; GUI playback mapping consumes this metadata.
- Disk-backed serving validates a bounded sidecar against catalog/header/media facts and captures
  one complete published extent. Raw full/range bodies, VOD ranges, exports, and keyframes never
  read past it; transforms cross-check source CRC, timing, sample count, and sync state.
- Serving admits at most 4,096 rows, 131,072 records per sidecar, 100,000 selected records,
  a 16 MiB playlist, 4 MiB init, 66 MiB fragment, eight concurrent disk-backed responses, and
  one queued export fragment. Admission remains owned by detached sidecar/read/encode work and
  final response-buffer/body drop. Raw bodies reserve their capacity-one channel before each read
  and attach admission to every emitted chunk. Export reserves queue capacity before materializing
  the next fragment. Do not replace these with an unbounded scan fallback.
- Active-camera JSON comes from `ApiState::active_cameras`, not historical catalog identities.
  Per-stream viewers count external live WebSockets, never the shared LL-HLS feeder.
- Metrics catalog aggregates refresh at most once per second under one cache-owned single-flight
  that survives requesting-scrape cancellation and uses the last snapshot on refresh failure.
  API uptime uses `started_monotonic`.
- LL-HLS timestamp resets resume at sync with a discontinuity. Each GOP carries its
  discontinuity sequence so removing playlist entries preserves the epoch.

## Where things are

- `lib.rs` — router, listener binding, and serving.
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
- An empty token list is accepted and logged as unauthenticated; tests cover both modes.
- `MAX_RANGE_US` bounds every range query; listings without `from` default to one hour.
- Event pages also inherit the catalog's 10,000-row and 8 MiB approximate
  materialized-owned-row budgets; that estimate is not the encoded JSON response size.

## Related documents

- `docs/research/07-architecture.md` §5, §6, §9; `docs/research/04-storage-format.md` §10;
  `docs/research/06-clients.md`; `docs/design/phase2-interfaces.md`;
  `docs/research/10-phase2-results.md`.
