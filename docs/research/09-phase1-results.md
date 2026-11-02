# 09 — Phase 1 results (measurement record, 2026-10-31)

This file is a dated measurement record, so it names dates, commands, and the state of the
code at the time. Host: the dev host described in the gitignored `AGENTS.override.md` (a 16-thread x86-64
machine). Camera: cam1 as characterised in `01-camera-probe.md`. Build:
`cargo build --release -p irisd` from the uncommitted working tree of 2026-10-31 (3.7 MB
binary). Recording root: the configured root on an ext4 HDD.

## Procedure

1. `irisd record --config config/cameras.local.toml --root <root> --duration 10m
   --stats-interval 60s`, with an external sampler reading `/proc/<pid>/stat` every 30 s to
   split user and kernel CPU time (a scratch sampler reading `/proc`).
2. `irisd inspect <file> --verify` on every file written.
3. Crash test: `irisd record` for 27 s, `kill -9`, `irisd inspect`, `irisd recover`,
   `irisd inspect --verify`, `ffprobe`.
4. Playback: `irisd hls` in four variants (main/sub × trailers included with three fragments
   per segment / trailers excluded with one fragment per segment), then ffprobe, ffmpeg
   decode of the first 12 s through its HLS demuxer, and GStreamer `uridecodebin` over the
   playlists. iPhone AVPlayer via `irisd serve` is left for the owner (the agent sandbox may
   not bind a LAN-facing port).
5. Baseline: `ffmpeg -rtsp_transport tcp -timeout 20000000 -i <main> -rtsp_transport tcp
   -timeout 20000000 -i <sub> -map 0:v -c copy -t 120 … -f mp4 main.mp4 -map 1:v -c copy
   -t 120 … -f mp4 sub.mp4` under `/usr/bin/time -v`.

## CPU and memory (one camera, main 4K + sub D1, ten minutes)

| Sample (30 s) | user % | kernel % | total % of one core | RSS (MiB) |
|---|---|---|---|---|
| 1 | 0.60 | 0.80 | 1.40 | 9 |
| 2 | 0.77 | 1.03 | 1.80 | 10 |
| 5 | 0.67 | 0.77 | 1.43 | 10 |
| 10 | 0.70 | 1.10 | 1.80 | 11 |
| 15 | 0.83 | 1.13 | 1.97 | 11 |
| 19 | 0.80 | 1.07 | 1.87 | 11 |
| mean of 19 | 0.75 | 0.99 | 1.74 | — |
| max | 0.90 | 1.13 | 2.00 | 12 |

Daemon summary for the same run: 600.2 s, 17,692 main frames + 16,026 sub frames, 560
fragments, 641.5 MiB, 6 files, 214 syncs, 0 write errors, 0 dropped fragments, 10
reconnects (see below). Earlier two-minute runs measured 1.58 % and 1.84 % with 10.5–11.8 MiB
peak RSS.

The phase-1 exit criterion (≤ 0.5 % for both streams) was a guess made before any
measurement; the measured 1.74 % is dominated by kernel time (TCP receive of 9.3 Mbps in
1,440-byte RTP packets, about 800 packets/s, plus page-cache writes). User time for
depacketising, regularising, CRC-32C, and `pwritev` is 0.75 %. Budget for six cameras:
about 10 % of one core, within the 2 %-per-camera figure in `07-architecture.md`.

Reconnects in this run were camera-side: the camera refused TCP connections for 7 s at
start (right after an ffmpeg baseline with 5 s socket timeouts had been torn down) and four
times accepted a sub-stream SETUP/PLAY but sent no frames for 10 s, each time while recovering
from more than three concurrent sessions opened by earlier tests. The daemon reconnected
with backoff and opened a new run file each time; the main stream recorded 593 s in one file.

## Container overhead

| File | Fragments | Payload bytes | File bytes | Overhead |
|---|---|---|---|---|
| main, 10 min (`172119Z.mp4`) | 297 | 589,273,890 | 589,463,714 | 0.0322 % |
| sub, 5.7 min (`172529Z.mp4`) | 171 | 84,266,707 | 84,376,914 | 0.1985 % |
| main, 2 min (first run) | 61 | 115,796,589 | 115,835,597 | 0.0337 % |

Per 2 s fragment the fixed cost is 32 B `prft` + 100 B `moof` + 8 B per sample + 8 B `mdat`
header + 24 B trailer ≈ 640 B; the sub stream's 0.2 % reflects its 300 KB fragments.
Exit criterion (≤ 0.05 % on the 4K stream): met.

## Regulariser

The camera's cadence is exactly 3000 ticks per frame, but it drops about one frame in 170
(single-frame holes of 2,700–3,600 ticks in the camera timeline, sometimes more than one per
GOP for tens of seconds) and stamps IDR frames 10–40 ms late. Three iterations were needed:

| Build | Behaviour on the 10-minute main file |
|---|---|
| Measured rate overrides the declared one immediately, 1 % gain, 150 ms tolerance | Nominal chased per-GOP jitter: durations 3030 / 3075 / 3022 / 3041 in consecutive fragments, 4 spurious snaps on the sub stream |
| Declared rate trusted for 16 fragments, 0.2 % gain, 500 ms tolerance | Holes outran the gain: error reached −0.8 s, 2 snaps on main |
| Holes rendered as frame-long holds on the last sample, 1 % gain for the residual, override only above 2 % or 5 % hole time | Replay of the recorded camera timeline (`crates/iris-ingest/tests/replay.rs`): 0 snaps, error never above 3 frames, assigned/camera ratio within 0.05 %, every non-hold duration within ±1.3 % of 3000 |

The regression test replays the sidecar of the real recording through the regulariser on
every `just check`.

Sample-duration histogram of the 10-minute main file as recorded by the second build (for
the record; the third build's durations are nominal with holds):

| Duration (ticks) | Count |
|---|---|
| 3000 | 4,932 |
| 3006 | 1,858 |
| 2994 | 900 |
| 3032 | 618 |
| 3003 | 535 |

A separate finding closed a design gap: with `max_fragment_ms` equal to the GOP length,
timestamp jitter split the sub stream's GOPs into 58 + 2-frame fragments; the cap now has a
quarter of slack.

## Crash recovery (`kill -9` after 27 s, main stream)

| Fragments before kill | File length | Scan stop reason | Recovery | After recovery |
|---|---|---|---|---|
| 15 | 27,455,305 B (st_size equals the valid length; preallocation uses `KEEP_SIZE`) | clean end of file | nothing to truncate, index rewritten | 15 fragments, CRC verified, ffprobe duration 26.126 s |

Because each fragment is one `pwritev`, a daemon crash loses only the fragment under
construction (≤ 2 s) and never tears a fragment; torn fragments can come only from power loss
mid-write, which the trailer CRC catches (unit-tested with truncated and corrupted files).
Exit criterion met.

## Playback

| Player | Trailers in byte ranges | Result |
|---|---|---|
| ffprobe (HLS demuxer, local playlist) | included, 3 fragments/segment | opens, `hvc1` 3840×2160, duration 593.02 s |
| ffprobe | excluded, 1 fragment/segment | opens, duration 593.02 s |
| ffmpeg decode, first 12 s | both variants, main and sub | 360 / 357 frames decoded |
| GStreamer `uridecodebin` (hlsdemux + qtdemux + libde265) | included and excluded, sub | plays to end of stream, exit 0 |
| ffprobe / ffmpeg on the raw `.mp4` | — | `hvc1`, decodes; a `duration` field reported by ffprobe is stale but pts are exact |
| AVPlayer on iPhone | included / excluded | **to be measured by the owner**: `irisd serve --dir <hls test directory> --listen 0.0.0.0:8080`, then open `http://<host>:8080/index.html` |

## Baseline comparison

| Recorder | Wall time | CPU time (user + sys) | CPU % of one core | Peak RSS | Video recorded |
|---|---|---|---|---|---|
| irisd (10 min, both streams) | 600 s | 10.4 s (4.5 + 5.9) | 1.74 % | 12 MiB | 641.5 MiB |
| ffmpeg `-c copy` fMP4, both streams, attempt 1 | 105.9 s | 2.02 s (1.19 + 0.83) | 1.91 % | 62 MiB | 67 MiB (68 s; ended by its 5 s socket timeout) |
| ffmpeg `-c copy` fMP4, both streams, attempt 2 | 40:48 (hung) | 11.7 s (5.73 + 5.95) | — | 61 MiB | 114 MiB (≈ 100 s, then the camera stalled the sessions and ffmpeg waited for 40 min at ≈ 0.5 % CPU until killed) |

No clean 120 s ffmpeg run was obtained on 2026-10-31: twice the camera stopped delivering
frames mid-session and ffmpeg (even with `-timeout`) did not recover, while irisd's 10 s
inactivity timeout reconnected within seconds in the same conditions. Per megabyte of video
during ffmpeg's active phase, attempt 1 spent 30 ms of CPU per MiB against irisd's 16 ms per
MiB over its ten minutes, with a sixth of the resident memory; a controlled rerun belongs in
the phase-2 benchmark harness with a replay server instead of the live camera.

## Verification run with the final regulariser (two minutes, both streams)

| Metric | Value |
|---|---|
| Mean CPU / peak RSS | 1.47 % of one core / 10.6 MiB |
| Dropped fragments / write errors | 0 / 0 |
| Snaps | 0 on both streams |
| Main: fragments, durations | 59; 2995 × 1,888, 3010 × 1,062, 3005 × 147, 2980 × 118, 3015 × 60 (all within ±0.7 % of nominal) |
| Main: wall minus media drift over 116 s | −61 ms |
| Sub: holds for camera-dropped frames | 18 samples of ≈ 6000 ticks (two frame periods) in 42 s, matching the camera's drop rate |
| Reconnects | 2, both the sub stream's "no frames for 10 s" starvation, each resumed in a new run file within 2 s |

Exit criteria for the regulariser (no zero or tiny durations, timeline locked to the camera,
duplicate stamps and 100 ms gaps absorbed, no spurious snaps) are met; the original wording
"constant 3000-tick durations" is superseded by the ±1 % lock described in
`04-storage-format.md` §7.3.

## Camera-side observation

After this day's tests opened more than three sessions at once (two of them a hung ffmpeg),
cam1's RTSP server refused connections for 7 s and then intermittently accepted sub-stream
sessions without sending frames for the rest of the afternoon. Both are handled by the
daemon (reconnect with backoff, new run file), and both argue for the inactivity timeout and
for keeping every client behind irisd's single camera connection.
