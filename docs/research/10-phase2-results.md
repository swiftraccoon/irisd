# 10 — Phase 2 results (measurement record, 2026-10-31)

Dated record of the first integration of the phase-2 daemon against cam1 on the dev host
(described in the gitignored `AGENTS.override.md`). Build: `cargo build --release -p irisd`
from the working tree committed as the phase-2 change (12.2 MB binary). Configuration: one
camera, main + sub streams, Dahua CGI events, API on loopback with one bearer token.

## What ran

`irisd run --config <local toml>` for about five minutes, then `kill -9`, then a restart on
a rebuilt binary for another five minutes. Startup logged Landlock "fully enforced" on this
kernel, the catalog open, both streams recording within 2 s, the event feed connected within
60 ms, and the API listening.

## Endpoint checks (all against the live daemon)

| Endpoint | Result |
|---|---|
| `GET /api/health` | `{"status":"ok","uptime_s":…,"version":"0.1.0"}` without a token |
| any `/api/*` without or with a wrong token | 401 |
| `GET /api/cameras` | the configured camera with `has_snapshot: true` |
| `GET /api/streams/cam1/main/segments` | rows with wall range, fragment count, bytes, sealed flag, codecs, dimensions; no file paths |
| `GET /api/streams/cam1/main/hls/vod.m3u8` | byte-range playlist, `EXT-X-MAP` per segment; ffprobe over HTTP reads `hvc1` 3840×2160, ffmpeg decodes 180 frames in 6 s through its HLS demuxer (after the segment URL gained a `.mp4` extension, which ffmpeg's `allowed_segment_extensions` requires) |
| `GET /api/streams/cam1/sub/export.mp4` | 4.0 MB streamed fMP4, 1,067 frames, 36.8 s, `hvc1` |
| `GET /api/segments/{id}/file.mp4` with `Range: bytes=0-767` | 206, 768 bytes, `video/mp4` |
| `GET /api/cameras/cam1/snapshot.jpg` | 200, `image/jpeg`, 1.36 MB (4K still proxied from the camera with digest auth) |
| WebSocket `/api/streams/cam1/sub/live` | one `0x01` init message, then `0x02` per-frame fragments at the stream rate (204 in 6 s); the first attempt sent two inits, fixed |
| `GET /api/events` | 32 events after ten minutes: `time_change` pulses every minute, the camera's `NewFile` and `VideoMotionInfo` codes kept as `other`, three `disconnect` start/stop pairs, and one real `motion` start/stop pair |
| `GET /metrics` | Prometheus text with request, client, catalog, and uptime series |

## Crash and restart

| Step | Observed |
|---|---|
| `kill -9` with two files open (main 147 fragments, sub 12) | — |
| restart | `recovered open segment … truncated=false` for both within 0.3 s; `catalog reconciled recovered=2 adopted=0`; recording resumed in new files 3 s after start |
| `GET …/segments` after restart | the crashed file listed as sealed with its full fragment count; the new file unsealed and growing |

## Resource use (daemon with API, catalog, and event feed, one camera)

| Metric | Value |
|---|---|
| CPU | 1.47–1.70 % of one core (30 s samples) |
| RSS | 16.1–16.2 MiB (phase-1 recorder alone: 11–12 MiB) |
| Catalog | 9 segment rows and 32 events after ten minutes; batched notifications, no measurable cost |

## Camera-side observations

- The camera's sub stream stalled every 30–60 s for the whole session ("no frames for
  10 s"), while the main stream never did; the camera's own `RtspSessionDisconnect` events
  name the sub-stream sessions, so the stall is camera-side. Each stall cost one reconnect
  and a new run file; nothing was dropped. The root cause (a second session on the same
  camera stops its sub stream) and the decision to record the third stream instead are in
  `01-camera-probe.md` §5b and `11-multi-camera-results.md`.
- The Dahua `TimeChange` payload renders camera-local time in 12-hour form without a
  meridiem marker (`02:46:06` for 14:46); only `nvr_time` is used for placement.

## Supply chain

`cargo deny check`: advisories, bans, licenses, and sources all pass after allowing
`CDLA-Permissive-2.0` for the webpki root-certificate bundle pulled in by rustls.
`cargo audit`: no advisories against the 319 crates in the lock file.

## Fuzzing (2026-11-01 to 2026-11-02)

Ten targets each ran for 24 h of libFuzzer time (`cargo fuzz run <target> --
-max_total_time=86400` on the pinned nightly, niced beside the soak) without a finding. The
five crashes the targets had found in their first minutes on 2026-11-01 (two in the ONVIF
timestamp parser, two in the recording scanner, one in the replay server's RTSP parser) were
fixed before the run, and their artifacts did not reproduce on 2026-11-02.

| Target | Runs in 24 h |
|---|---|
| `access_unit` | 4.95 × 10⁹ |
| `box_header` | 1.80 × 10¹⁰ |
| `dahua_part` | 2.38 × 10⁹ |
| `digest_challenge` | 1.46 × 10⁹ |
| `index` | 5.22 × 10⁹ |
| `multipart` | 4.62 × 10⁸ |
| `onvif_utc` | 6.14 × 10⁹ |
| `rtsp_request` | 1.96 × 10⁹ |
| `scan` | 7.29 × 10⁹ |
| `trailer` | 1.93 × 10¹⁰ |

## Open items (phase-2 exit criteria in `08-roadmap.md`)

ONVIF PullPoint events, open on 2026-11-02, ran against hardware later
(`11-multi-camera-results.md`, the seventh camera).

- Seven-day soak: run the daemon as the packaged systemd service.
- `mediastreamvalidator` on the daemon's playlists and AVPlayer on an iPhone (needs a LAN
  `listen` address).
- Fuzzing: the targets added after 2026-11-02 (`api_time`, `api_range`, `api_auth`,
  `export_rebase`, `onvif_soap`, `onvif_notification`, `discovery_parts`, `tools_protocol`,
  `tools_logs`, `describe_diagnostics`, `rtsp_message`, `depacketize_h265`, `rtp_packet`,
  `vod_playlist`) lack a completed 24 h run. So does `rtsp_request` since it was extended to the
  replay server's `Transport` and `Session` parsers, and `tools_protocol` since it was extended
  to the probe's interleaved-stream, RTCP, H.265 payload, SDP, and digest parsers.
