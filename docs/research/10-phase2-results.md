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

## Fuzzing (2026-11-08 to 2026-11-09)

Six targets added after the first campaign ran the same command for 24 h each from
2026-11-08T17:18Z, without a finding. They were built from the tree of that hour, before the
standards audit of 2026-11-09 changed the event, export, and query-time code that
`onvif_soap`, `export_rebase`, and `api_time` reach, and `tools_protocol` held three parsers
then.

| Target | Runs in 24 h |
|---|---|
| `api_time` | 1.05 × 10¹⁰ |
| `describe_diagnostics` | 1.85 × 10⁸ |
| `export_rebase` | 4.81 × 10⁹ |
| `onvif_soap` | 2.73 × 10⁸ |
| `rtsp_message` | 3.02 × 10⁷ |
| `tools_protocol` (three parsers) | 3.59 × 10⁹ |

## Fuzzing (2026-11-09 to 2026-11-10)

Seven more targets ran the same command for 24 h each from 2026-11-09T05:07Z, without a
finding, on a tree that held the audit's changes to the code they reach. The civil-date
arithmetic the ONVIF timestamp parser calls moved to `iris-core` afterwards, unchanged.

| Target | Runs in 24 h |
|---|---|
| `api_auth` | 9.88 × 10⁹ |
| `api_range` | 1.47 × 10¹⁰ |
| `depacketize_h265` | 1.03 × 10⁸ |
| `discovery_parts` | 5.63 × 10⁷ |
| `onvif_notification` | 1.20 × 10⁷ |
| `rtp_packet` | 1.71 × 10¹⁰ |
| `vod_playlist` | 7.49 × 10⁷ |

## Mutation testing (2026-11-10)

`just mutants FILE...` (cargo-mutants 27.1.0, two workers, each crate's own tests) over the
format, timing, parsing, storage, catalog, API, and daemon code. A mutant that hangs the tests
counts as detected. The figures are of the first run over each set, before the tests written
from it; the second round reran the first two sets and added the export and LL-HLS handlers.

| Code | Mutants | Caught | Timed out | Missed | Unviable |
|---|---:|---:|---:|---:|---:|
| ingest: regulariser, assembler | 279 | 223 | 0 | 48 | 8 |
| mp4: fragment, index, reader, export | 502 | 407 | 12 | 58 | 25 |
| mp4: boxes, init, hls, crc | 113 | 89 | 1 | 19 | 4 |
| second round, with api export and llhls | 783 | 635 | 13 | 102 | 33 |
| h265 | 51 | 46 | 0 | 3 | 2 |
| core: time, id, event | 294 | 239 | 1 | 36 | 18 |
| core: config | 139 | 125 | 0 | 10 | 4 |
| events: onvif | 191 | 153 | 0 | 24 | 14 |
| events: feed, dahua, multipart, digest, backoff | 237 | 174 | 6 | 41 | 16 |
| store: recover, segment | 148 | 80 | 0 | 44 | 24 |
| store: writer, stream, root worker, layout, policy | 159 | 117 | 0 | 31 | 11 |
| catalog: db | 308 | 209 | 3 | 50 | 46 |
| catalog: handle; replay: synth | 200 | 120 | 2 | 41 | 37 |
| api: auth, query, segments | 267 | 199 | 7 | 30 | 31 |
| api: hls, keyframe, live, admission | 247 | 179 | 0 | 49 | 19 |
| api: state, events, cameras, provision, snapshot, websocket, server, metrics, health, error, onvif | 240 | 106 | 10 | 35 | 89 |
| ingest: live, stats, source, session; tls | 270 | 166 | 17 | 64 | 23 |
| irisd: retention | 122 | 80 | 0 | 38 | 4 |
| irisd: adopt, inspect, hls | 235 | 117 | 0 | 101 | 17 |
| irisd: run, record, selfcheck, capability, permissions, catalog observer, provision | 432 | 207 | 0 | 131 | 94 |

What the missed mutants showed, and what followed the same day:

- A regulariser rule no test needed, which measurement then showed to do harm
  (`11-multi-camera-results.md`, generated stalls); it was removed.
- Whole paths that no test ran: the Dahua feed task, the ONVIF retry loop, the daemon's
  recording path from camera to sealed catalog row, the offline `hls` and `inspect` commands,
  and every field of the initialization segment. Each has an end-to-end or field-by-field
  test.
- Settings that could be inverted unnoticed: certificate verification in the Dahua feed and
  snapshot clients (the ONVIF client was covered), and the watchdog's progress condition.
- Facts checked only in combination or only at one end: sidecar binding and fragment checks,
  capability expiry and path limits, the free-space floor, event maintenance at its age and
  recording-horizon boundaries, `tfhd` and `trun` layouts of other muxers.

Left as they are: mutants equivalent to the original (a second loop exit, flags that share no
bit, log-only branches), limits of 64 MiB and more that a test would have to allocate, and
error handling that needs injected faults (transient resource errors, `AlreadyExists` retries,
directory sync), which the full-filesystem test covers only for `ENOSPC`. `iris-tools`,
`iris-gui`, and the rest of `iris-replay` were not run.

## Open items (phase-2 exit criteria in `08-roadmap.md`)

ONVIF PullPoint events, open on 2026-11-02, ran against hardware later
(`11-multi-camera-results.md`, the seventh camera).

- Seven-day soak: run the daemon as the packaged systemd service.
- `mediastreamvalidator` on the daemon's playlists and AVPlayer on an iPhone (needs a LAN
  `listen` address).
- Fuzzing: `tools_logs` lacks a completed 24 h run. So do `rtsp_request` since it was extended
  to the replay server's `Transport` and `Session` parsers, `tools_protocol` since it was
  extended to the probe's interleaved-stream, RTCP, H.265 payload, SDP, and digest parsers, and
  `api_time`, `export_rebase`, and `onvif_soap` on the code the audit changed; runs of these
  six started on 2026-11-10T01:29Z. The ten targets of the first campaign ran on the 2026-11-01
  tree; the exit criterion is judged on a 24 h run of every target on the release tree.
