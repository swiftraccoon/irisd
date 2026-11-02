# 08 — Roadmap

Each phase has exit criteria that are measured, not asserted. Phase 0 is this research.

## Phase 1 — Spike: one camera, zero-decode recording, measured (1–2 weeks)

Build: `iris-h265`, `iris-mp4` (writer + scanner), `iris-ingest` (retina session,
regulariser, fragment builder), a minimal `irisd` that records cam1 main + sub to the fMP4
layout in 04, and a throwaway HTTP file server for playback tests.

Exit criteria:
- Files open in `ffprobe`, `mpv`, GStreamer, and play in AVPlayer on an iPhone via a static
  HLS playlist with `EXT-X-MAP` byte ranges (answers 04 §12.1 and the `prft` question).
- CPU ≤ 0.5 % of one core for the 4K stream + sub stream over 10 min; RSS ≤ 20 MB.
- `kill -9` during recording: recovery scan truncates at the last valid fragment; no
  corrupt output; loss ≤ one fragment.
- Regulariser produces constant 3000-tick durations on cam1 with the assigned timeline within
  ±100 ms of the camera RTP timeline over 1 h, and handles duplicate stamps and 100 ms gaps.
- Container overhead measured ≤ 0.05 %.

## Phase 2 — Daemon v0.1: six cameras, catalog, serving, events, hardening (3–5 weeks)

Build: `iris-store` (writer threads, preallocation, sync, sidecar, retention, recovery),
`iris-catalog`, `iris-api` (REST, HLS VOD, WS live, export, keyframes, snapshots),
`iris-events` (Dahua CGI feed + ONVIF PullPoint), systemd unit + Landlock, metrics, config.

Exit criteria:
- 7-day soak with 6 cameras × 2 streams: zero unexplained gaps; write amplification ≤ 1.01;
  CPU ≤ 2 % of one core; RSS ≤ 64 MB; recovery < 5 s after SIGKILL and after a power cut.
- Retention keeps the disk between thresholds with bounded deletion latency; no SQLite lock
  storms (catalog transactions < 10 ms p99).
- HLS playlists pass `mediastreamvalidator`; export of an arbitrary range is frame-exact at
  the end and keyframe-aligned at the start with `elst` trimming.
- Events from the CGI feed and PullPoint land in the catalog within 1 s and align with video
  within the measured camera latency.
- `cargo fuzz` runs for 24 h on each parser without findings; `systemd-analyze security`
  score "safe".

## Phase 3 — Linux GUI (3–4 weeks)

gtk4-rs + gstreamer-rs client: grid of sub streams with hardware decode, single-camera
full-res view, timeline with event markers and keyframe thumbnails, export dialog, camera
provisioning UI. Exit: 6 tiles + 1 main at ≤ 15 % of one core on the dev host; works on an
Intel VA-API box with DMA-BUF zero-copy.

## Phase 4 — iOS app (4–6 weeks)

Swift app: WebSocket live via `AVSampleBufferDisplayLayer`, HLS timeline via `AVPlayer`,
PiP, event list and push, export sharing. Exit: 150–300 ms live latency on the LAN, grid of
six tiles, background/foreground decoder recovery.

## Phase 5 — Breadth

ONVIF discovery and provisioning for non-Dahua cameras; ONVIF metadata-track events; audio
(AAC) end to end; optional on-NVR analytics on the sub stream (hardware decode, never in the
record path); WebRTC/QUIC for remote live view; multi-disk tiering; aarch64 SBC builds.

## Decisions deferred on purpose

- Fragment cap default (2 s vs 0.5 s) until phase-1 measurements of loss window vs write count.
- ext4 vs XFS for the video roots until the phase-2 soak.
- Audio codecs other than AAC.
- B-frame streams (composition offsets).
