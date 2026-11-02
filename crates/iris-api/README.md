# iris-api

The HTTP and WebSocket surface of irisd: catalog queries, byte-range HLS playlists, fMP4
export, live streaming, event listing and push, camera snapshots, health, and metrics. The
server never decodes video. Playback endpoints hand out byte ranges of the recording files
that the catalog describes, and the live endpoint wraps frames from the ingest live ring in
one `moof` + `mdat` each.

## What it provides

- `router(state)` — the axum router. `/api/health` and `/metrics` are public; every other
  route requires a bearer token when `ApiState::tokens` is not empty.
- `serve(listen, router, shutdown)` — binds the listener and serves until the cancellation
  token fires.
- `ApiState` — the catalog handle, the live-ring registry, the event broadcast channel, the
  accepted tokens, snapshot sources, request counters, and the shutdown token.
- `LiveRegistry` — live rings by stream, filled by the daemon as streams start.
- `SnapshotSource` — a camera's `snapshot.cgi` endpoint with digest credentials.
- `ApiStats` — request and client counters exposed on `/metrics`.
- `ApiError` — handler failures rendered as a JSON `{ "error": … }` body with the matching
  status.

| Route | Purpose |
|---|---|
| `GET /api/health` | liveness: status, uptime, version |
| `GET /metrics` | Prometheus text: requests, live and event clients, uptime, catalog totals |
| `GET /api/cameras` | the configured cameras |
| `GET /api/cameras/{camera}` | the camera with the health of each recorded stream: connected, frames, fragments, bytes, reconnects, loss events, gaps, last frame time, codec, nominal frame duration, camera clock offset, live viewers |
| `GET /api/cameras/{camera}/snapshot.jpg` | a JPEG fetched from the camera with digest authentication |
| `GET /api/streams/{camera}/{kind}/segments?from&to` | catalog rows in a time range |
| `GET /api/streams/{camera}/{kind}/hls/vod.m3u8?from&to` | a byte-range VOD playlist over the range |
| `GET /api/streams/{camera}/{kind}/export.mp4?from&to` | a stand-alone recording of the range, streamed: the init segment with an edit list trimming to `from` and `to`, then the range's fragments rebased onto a timeline from zero with fresh sequence numbers and trailers (`docs/research/04-storage-format.md §10`) |
| `GET /api/streams/{camera}/{kind}/live` | WebSocket: an init segment, then one fragment per frame |
| `GET /api/streams/{camera}/{kind}/hls/live.m3u8[?_HLS_msn&_HLS_part]` | the low-latency live playlist from the live ring: 0.5 s parts, blocking reload (waits up to 6 s, then 503), preload hints; relative URIs `init.mp4`, `seg/{msn}/media.m4s`, `part/{msn}/{index}/part.m4s` beside it, served from memory (`docs/research/04-storage-format.md §10`) |
| `GET /api/streams/{camera}/{kind}/keyframe?t=` | a playable MP4 of one frame: the init segment and a single-sample fragment holding the sync sample at or before `t` (default: the newest), looked up within the two minutes before `t`; `x-iris-wall-us` carries the sample's time |
| `GET /api/segments/{id}/file.mp4` | one recording file, with HTTP range support |
| `POST /api/events/{id}/retain` | body `{"until": TIME}` keeps the footage around the event (30 s each side) from retention until then; `{"until": null}` or no body clears it; returns the event with `retain_until_us` |
| `GET /api/events?camera&from&to&limit` | events in a range, or a WebSocket push when upgraded |

`from` and `to` accept Unix microseconds or RFC 3339; `from` defaults to an hour before
`to`, `to` to the present, and a range may span at most seven days.

## How it is used

A client presents the token as `Authorization: Bearer <token>`; playlists carry it as a
`token` query parameter in the URIs they emit so a media player can follow them. The live
WebSocket sends binary messages whose first byte is the type: `0x01` an initialization
segment, `0x02` a media fragment holding one frame. Event push sends one JSON event per
text message.

```sh
curl -H 'Authorization: Bearer TOKEN' 'http://127.0.0.1:8554/api/cameras'
curl -H 'Authorization: Bearer TOKEN' \
  'http://127.0.0.1:8554/api/streams/cam1/main/hls/vod.m3u8?from=2026-01-01T00:00:00Z&to=2026-01-01T00:10:00Z'
```

The daemon constructs `ApiState`, registers each stream's live ring, and spawns `serve`.

## Invariants and limits

- Authentication is a constant-time bearer-token comparison; an empty token list disables it
  and is logged as a warning.
- Playlists are byte ranges into the recording files, grouped three fragments per media
  segment with `EXT-X-INDEPENDENT-SEGMENTS`, an `EXT-X-MAP` per file, and a discontinuity at
  run boundaries (`docs/research/04-storage-format.md §10`).
- Export streams whole fragments; the start is aligned to a fragment, and sub-GOP trimming
  with `elst` is a design item in the same section.
- Segment DTOs never expose server paths.
- Snapshots are proxied from the camera with digest authentication and bounded in size.
- The API surface is fixed by `docs/research/07-architecture.md §5` and the crate contract in
  `docs/design/phase2-interfaces.md`.

## Tests and measurements

`tests/api.rs` builds a real recording through the store and the catalog, then exercises
health without a token, token rejection and acceptance, segment listing and playlist
construction, range serving and export over the recording, and rejection of unknown
segments and bad ranges. Live integration against real cameras, playback through ffprobe,
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
