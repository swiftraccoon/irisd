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
- The live WebSocket message bytes (`0x01` init, `0x02` fragment) are consumed by the clients
  in `docs/research/06-clients.md`.
- Time parameters accept Unix microseconds or RFC 3339 and are bounded to seven days
  (`query.rs`).

## Where things are

- `lib.rs` — router and `serve`.
- `state.rs` — `ApiState`, `LiveRegistry`, `SnapshotSource`, `ApiStats`.
- `auth.rs` — bearer-token middleware and the token extension playlists read.
- `query.rs` — `from`/`to` parsing, defaults, and bounds.
- `cameras.rs`, `segments.rs`, `events.rs`, `health.rs`, `metrics.rs` — the JSON and text
  endpoints.
- `hls.rs` — VOD playlist construction from sidecar indexes.
- `llhls.rs` — the low-latency live playlist: the per-stream feeder over the live ring, part
  cutting, playlist rendering, blocking reload, the segment and part responses.
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

- A playlist URI must carry the token the caller presented (the `AuthToken` extension), or
  a media player cannot fetch the segments.
- Byte ranges that span several fragments include the 24-byte trailers between them; the
  playlist math in `hls.rs` relies on sidecar records, not on file sizes.
- `export.rs` streams through a bounded channel; a slow client must not hold a blocking read
  on the runtime thread.
- An empty token list is accepted and logged as unauthenticated; tests cover both modes.
- `MAX_RANGE_US` bounds every range query; listings without `from` default to one hour.

## Related documents

- `docs/research/07-architecture.md` §5, §6, §9; `docs/research/04-storage-format.md` §10;
  `docs/research/06-clients.md`; `docs/design/phase2-interfaces.md`;
  `docs/research/10-phase2-results.md`.
