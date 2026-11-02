# Changelog

All notable changes to irisd are recorded here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project uses
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- Retained events: `POST /api/events/{id}/retain` with `{"until": TIME}` keeps the
  footage within 30 s of the event from every retention rule until then (the catalog's
  schema version 2 adds `retain_until_us`; a pass skips protected segments and moves on).
  Event listings carry `retain_until_us`. The viewer's playback view gains "Keep event"
  for the event nearest the playhead, and draws retained events distinctly.
- `GET /api/cameras/{camera}`: the camera with each recorded stream's health (the
  `stream` statistics line's counters, the last frame's time, the codec, the camera clock
  offset, live viewers), the per-camera view the clients' camera pages need.
- Low-latency HLS live: `GET /api/streams/{camera}/{kind}/hls/live.m3u8` with 0.5 s
  partial segments, `EXT-X-SERVER-CONTROL` blocking reload (`_HLS_msn`, `_HLS_part`), and
  preload hints, built from the live ring in memory; `init.mp4`, `seg/{msn}/media.m4s`,
  and `part/{msn}/{index}/part.m4s` beside it. The route the iOS client's live tab and AirPlay use
  (`06-clients.md`); `ffmpeg` plays it from a replayed camera.
- `iris-core::metrics`: a lock-free latency histogram with fixed logarithmic buckets and
  per-interval percentiles, and a peak gauge, for statistics that cost one atomic per sample.
- Soak instrumentation in the daemon: `writer`, `catalog`, and `root` statistics lines;
  stream lines gain regulariser holds, nominal rate, timeline error, fragment peaks, the
  camera clock offset from RTCP sender reports, gaps, and idle time; event lines gain
  messages, renewals, errors, idle time, and pull latency; the process line gains the CPU
  user/system split, threads, fds, loop lag, withheld watchdog pings, retention timing, and
  self-check counts; `gap` lines for reconnects and camera holds; retention passes log their
  duration, free space, and catalogued bytes against the quota; writes or syncs over a second
  are warnings.
- A rotating self-check (`irisd run --selfcheck-cycle`, default 24 h) that scans each
  stream's newest sealed file with CRC verification and compares it with the catalog row.
- `iris-tools soak-sample`: a read-only host-side sampler for soaks (cgroup, disk,
  pressure, NIC, TCP, NTP, journal export, API checks) meant for a systemd user timer.
- `iris-tools soak-report`: the soak figures and a criteria table from a sampler
  directory (runs, trends, latency peaks, gaps, retention, self-checks, write amplification,
  API checks).
- `iris-replay`: a minimal RTSP server that replays irisd recordings (or synthetic ones)
  as any number of H.265 cameras over TCP-interleaved RTP, re-packetized per RFC 7798 with
  in-band parameter sets, RTCP sender reports, decode-time pacing, and `--drop-every` and
  `--stall-after` fault injection; a loopback test receives the frames through the
  recorder's own RTSP client. `iris-tools replay-bench` runs the daemon against N
  replayed cameras and reports CPU, wake-ups per second, RSS, and cost per megabit.
- The scanner reports each fragment's sample payload offset (`FragmentInfo::payload_offset`).
- `ingest.read_interval_ms`: camera sockets are drained on a shared timer instead of on
  every packet (default 20 ms), which cuts the daemon's CPU by about a quarter at the same
  input; `docs/research/12-ingest-cost.md` records the measurements and the harness
  fidelity check. `IngestOptions::read_interval` carries it; an integration test records
  from the replay server in both modes.
- `iris-gui`: the start of the Linux viewer (phase 3): a GTK 4 window with one live tile
  per camera, each a GStreamer pipeline (`appsrc ! qtdemux ! decodebin3 ! videoconvert !
  gtk4paintablesink`) fed by a reconnecting WebSocket client from the daemon's live
  endpoint. Options: `--api`/`IRISD_API`, `--token`/`IRISD_TOKEN`, `--kind`, `--columns`.
  CI installs the GTK and GStreamer headers. Measured 2026-11-01 on the x86-64 development
  host with the software decoder (`avdec_h265`; the NVIDIA kernel module was not loaded
  after an OS upgrade): seven 1080p30 tiles on a virtual display cost about 390 % of one
  core and 830 MiB, with every tile decoding; the phase-3 target of 15 % needs the hardware
  decoder the design assumes.
- `iris-gui`: a single-camera view of the main stream, opened by a click on a tile or with
  `--focus CAMERA` (`--focus-kind` chooses the stream), with a button back to the grid.
  Only the visible view decodes: tiles stop their pipeline and feed when hidden and start
  again when shown. Measured 2026-11-01 as above, software decoder: one 4K main stream
  focused costs about 265 % of one core and 650 MiB; the seven-tile grid 435 % and
  790 MiB on this run.
- `iris-gui`: a playback view with a timeline. The "Playback" button of the focused view
  (or `--focus CAMERA --playback`, window length `--window MINUTES`) loads the window's
  segments and events, draws them on a timeline, and plays the daemon's HLS VOD playlist
  through `playbin3`; a click on the timeline seeks, with wall-clock time mapped to
  playlist time across the gaps between segments; "Play / Pause" and "Live" buttons.
  Measured 2026-11-01 on the software decoder: a 4K main-stream window plays at about
  325 % of one core and 1.1 GiB.
- `iris-api`: the keyframe resource of the design, `GET
  /api/streams/{camera}/{kind}/keyframe?t=`: the init segment plus one single-sample
  fragment holding the sync sample at or before `t`, re-encoded from the stored fragment's
  first sample (`iris_mp4::reader::parse_fragment` reads the stored fragment's sample
  table). Clients render thumbnails from it without decoding a GOP.
- `iris-gui`: the GPU path. The tile pipeline and the playback sink use `glupload !
  glcolorconvert ! gtk4paintablesink` with the sink's GL and DMA-BUF features, so frames
  decoded by NVDEC (or VA-API) never cross the CPU; the viewer selects GTK's GL renderer
  by re-executing itself with `GSK_RENDERER=gl` when nothing set it, because the Vulkan
  renderer copies GL textures through the CPU. Measured 2026-11-02 on the x86-64 host
  with the NVIDIA GPU (`nvh265dec`), on a headless Wayland compositor using the NVIDIA EGL
  at 60 Hz, against the live service (seven 1080p30 sub tiles; one 4K30 main stream):

  | View | Software decode, Xvfb | NVDEC, CPU conversion, Xvfb | NVDEC, GPU path, Vulkan renderer | NVDEC, GPU path, GL renderer |
  |---|---|---|---|---|
  | Seven-tile grid | 435 % | 326 % | 218 % | 122 % |
  | One 4K main, live | 265 % | 250 % | — | 46 % |
  | One 4K main, playback | 325 % | — | — | 75 % |

  Percentages are of one core. On the GL path the profile is the NVIDIA driver's own work
  (EGL rendering on the main thread, CUDA-GL interop on the GStreamer GL threads, and
  their kernel calls); no frame conversion remains in user code. The phase-3 criterion of
  15 % for six tiles plus a main view is not met on this driver; the design's zero-copy
  assumption was VA-API with DMA-BUF.
- `iris-gui`: one shared pipeline for the live tiles (the stage); each tile is a bin
  added to and removed from it, so the grid shares one GL context and one GL thread
  instead of running one per tile. Measured 2026-11-02 as above (headless Wayland, NVIDIA
  EGL, 60 Hz), seven 1080p30 tiles: 95–117 % of one core over three runs (median 111 %)
  against 115–122 % with a pipeline per tile, and 590 MiB resident against 1,215 MiB; one
  4K main stream 49 % against 46 %. On this driver the decoder's CUDA-GL interop is the
  expensive part: with the stage's GLES context it falls back to system memory and costs
  less than forcing desktop GL to keep the interop (177 % for the grid).
- `iris-gui`: a strip of twelve keyframe thumbnails above the playback timeline, fetched
  from the keyframe resource for the grid's stream kind and decoded off the main loop.
- `iris-gui`: export from the playback view. "Mark in" and "Mark out" set the range at the
  playhead, drawn on the timeline; "Export" opens the system save dialog and streams the
  daemon's MP4 export of the range into the chosen file with progress in the status line.
  `--export-to FILE` exports the whole playback window without a dialog, for scripts.
- Fuzz targets for every network-facing parser: `multipart`, `digest_challenge`,
  `dahua_part`, `onvif_utc`, `trailer`, and `rtsp_request` join `scan`, `box_header`,
  `index`, and `access_unit` under `fuzz/`.
- Documentation: a `README.md` and an `AGENTS.md` in every crate, crate-level rustdoc
  overviews with module maps, a rewritten root `README.md`, and CI workflows under
  `.github/workflows/` that run `just check` and `just audit` and publish rustdoc to GitHub
  Pages with a badge in the README. AGENTS.md rule 10 makes documentation part of every
  change.
- `iris_mp4::crc`: CRC-32C through `crc-fast` 1.10 (MIT OR Apache-2.0, carry-less-multiply
  folding), cross-checked against the `crc32c` crate in a test; the format is unchanged and
  the daemon's user time falls 17 % at 101 Mbit/s (`docs/research/12-ingest-cost.md` §5).
  `iris-tools replay-bench --perf` samples the daemon with `perf record -g`.
- `ingest.recv_low_water_kib` (default 32): `SO_RCVLOWAT` on each camera socket once
  `PLAY` succeeds, so the kernel reports it readable once per 32 KiB instead of once per
  packet. With the timer-driven reads the daemon's CPU halves against packet-driven reads
  (14 replayed 4K cameras at 101 Mbit/s: 14.8 % to 7.0 % of one core). retina has no such
  option, so `third_party/retina` carries the published 0.4.21 crate (MIT OR Apache-2.0)
  plus `PlayOptions::recv_low_water`, applied through `[patch.crates-io]`; the diff is kept
  beside it for upstream submission.
- The Landlock sandbox allows the RTSP ports the camera URLs name (`CameraConfig::rtsp_ports`),
  not only 554.
- Research and design documents under `docs/research/`: test-camera probe, prior art,
  language and runtime decision, on-disk format and time model, ingest and events, clients,
  architecture, roadmap, and the five cited research appendices.
- Vendored engineering standards under `docs/standards/` (Rust Style Guide, Rust API
  Guidelines, Google Rust style guide, Google timeless documentation, IETF RFCs 2326, 3550,
  6381, 7798, 8216 and the HLS bis draft) with `STANDARDS.md` as the adopted digest,
  `AGENTS.md` for agents working in the repository, and `tools/standards-refresh.sh`.
- Cargo workspace with release profile (`lto = "fat"`, `codegen-units = 1`,
  `panic = "abort"`), workspace lints (`unsafe_code = "forbid"`, `clippy::pedantic`),
  `rustfmt.toml`, `clippy.toml`, `deny.toml`, `rust-toolchain.toml`, and a `justfile`.
- `iris-core`: `CameraId`, `StreamKind`, `StreamKey`, `WallUs` (wall-clock microseconds
  with NTP and civil-time conversions), `MediaTicks` (90 kHz), and TOML configuration.
- `iris-h265`: NAL unit classification, length-prefixed access-unit iteration, and
  parameter-set stripping for `hvc1` samples.
- `iris-mp4`: initialization segment builder, fragment encoder (`prft` + `moof` + `mdat` +
  CRC-32C trailer), scanner with truncation, zero-fill, and CRC detection, the 40-byte
  sidecar index, and byte-range HLS playlist generation.
- `iris-ingest`: retina RTSP sessions over TCP, constant-frame-rate timestamp regulariser
  (declared rate trusted, camera-dropped frames rendered as frame-long holds, 1 % per-fragment
  lock, measured-rate override for mis-declaring cameras), fragment assembly at random access
  points with a quarter of slack over the fragment cap, and a regression test that replays
  the camera timeline of a real ten-minute recording.
- `iris-store`: segment files with preallocation, vectored fragment writes, periodic
  `fdatasync`, rotation, a writer thread per storage root, and crash recovery by scan.
- `irisd` binary: `record`, `inspect`, `recover`, `hls`, and `serve` subcommands.
- `iris-catalog`: SQLite catalog (WAL) on its own thread with batched non-blocking
  notifications and async queries for cameras, segments, events, retention, and recovery.
- `iris-events`: Dahua CGI push feed with RFC 7616 digest auth and a streaming multipart
  reader, event normalisation, and ONVIF PullPoint subscriptions via oxvif.
- `iris-api`: bearer-token HTTP API with health, metrics, cameras, segment listing and
  Range-served files, byte-range HLS VOD playlists across segments, streamed fMP4 export,
  per-frame live WebSocket, event listing and push, and a snapshot proxy.
- `iris-ingest` live ring and `iris-store` observer, free-space, and segment removal helpers.
- `irisd run`: the daemon (catalog reconciliation and crash recovery at startup, writer with
  catalog observer, ingest and event tasks, API, Landlock sandbox, systemd readiness and
  watchdog, retention by free space and age) with systemd packaging under `packaging/`.
- `iris-tools rtsp-probe` and `onvif-call`: stand-alone RTSP/RTCP and ONVIF probes used for
  camera measurements.
- `fuzz/`: cargo-fuzz targets for the MP4 scanner, box headers, index codec, and H.265
  access-unit framing.
- `iris-tools daemon-stats`: summarises a daemon log (per-stream rates and stall
  intervals, process trend, deduplicated non-stats lines) for measurement records.
- `docs/research/11-multi-camera-results.md`: six cameras, twelve streams, 96 minutes.
- `storage.quota_gib`: a cap on the catalogued recordings for roots on a disk shared with
  other data; retention deletes the oldest sealed segments first.

### Changed

- Project tooling is Rust: `crates/iris-tools` replaces the Python scripts under `tools/`
  with one binary (`daemon-stats`, `soak-sample`, `soak-report`, `replay-bench`,
  `rtsp-probe`, `onvif-call`) and adds `gh-commit`, which publishes the local branch's
  difference as a GitHub-signed commit. The reports were checked against the Python
  output over one soak window before the scripts were removed.
- Project standards: the engineering references come from the Rust project reference pack
  under `.rust-reference/` (Rust style guide, API guidelines, Google's Rust and timeless
  documentation guidance, Conventional Commits 1.0.0, Semantic Versioning 2.0.0, with a
  manifest and a Rust CLI that verifies the snapshots); commit messages follow
  Conventional Commits with the house style in `STANDARDS.md §9`; the development toolchain
  is pinned and the minimum supported Rust version is 1.92, which CI tests; CI actions are
  pinned by commit and cargo runs locked.
- Ingest wake-ups: one `ReadTicker` task per runtime wakes every stream for the read
  tick instead of a timer per stream (each of which made tokio unpark its I/O driver), and
  with timer-driven reads the RTSP socket leaves tokio's reactor once `PLAY` succeeds
  (`PlayOptions::detach_from_reactor` in the vendored retina), so `epoll` no longer fires
  per arriving segment, and a detached read shorter than the space offered ends that
  tick's reads instead of paying one for `EAGAIN`. Measured 2026-11-02 with fourteen
  replayed 4K cameras at 132 Mbit/s: 4.7 % to 2.8 % of one core, 5,400 to 70 wake-ups per
  second, 6,400 to 930 system calls per second (`12-ingest-cost.md §6`).
- Per-stream syncs are staggered. Every stream's `fdatasync` fell on the same moment,
  since all files opened at start-up together; the first soak window showed the HDD
  stalling for a second at those moments. Each stream syncs at its own phase of the
  interval (one sixteenth per stream in order of first appearance), kept across
  rotations and skipped syncs (`iris-store`; `04-storage-format.md §9`).
- Start-up adoption summarises a sealed file from its sidecar index when the index matches
  the file (contiguous records, trailer at the end), reading kilobytes instead of scanning
  the whole file, and falls back to the scan otherwise; while adopting it reports progress
  to systemd and extends the start timeout.
- The regulariser re-times a stream to its delivered rate when the typical (median)
  fragment runs more than 1 % off the declared rate, instead of keeping the declared rate
  and rendering the shortfall as a hold at every GOP boundary; bursty drops still become
  holds because they leave the median on rate. Found by inspecting a soak recording whose
  camera delivered 29.29 fps under a 30 fps declaration, with a 66 to 133 ms step at
  nearly every GOP boundary.
- `iris-tools daemon-stats` reads the `writer`, `catalog`, `root`, and widened
  `stream`, `events`, and `process` lines, accepts a `journalctl -o cat` export, and lists
  gap, self-check, retention, and slow-I/O lines.
- Camera policy: the third stream serves as the secondary stream on cameras whose sub
  stream stops once a second RTSP session opens (`docs/research/01-camera-probe.md` §5b);
  the example configuration documents the `sub_url` choice.
- Ingest: 15 s timeout on DESCRIBE, SETUP, and PLAY, TEARDOWN on every exit, and a 30 s
  inactivity tolerance (was 10 s), recorded in `05-ingest-events.md` and the decision table.
- ONVIF pull-point: WS-Security tokens fall back to host time when the camera rejects the
  measured clock offset, and `PullMessages` asks for 5 s instead of 60 s because the `oxvif`
  HTTP transport caps every request at 10 s. `iris-tools onvif-call` takes an explicit
  clock offset.

### Fixed

- Exports are stand-alone recordings. An export copied fragments verbatim, so its timeline
  started at the stored decode time (hours into a run: `ffprobe` reported a 21-hour
  duration for a five-minute file) and sequence numbers restarted at every file boundary.
  The API now rebases every fragment onto a continuous timeline from zero, renumbers the
  fragments, rewrites the `prft` media time, recomputes each trailer, and splices an edit
  list into the init segment that trims the first and last fragments to the requested
  range (`iris_mp4::export`; `04-storage-format.md §10`). Checked 2026-11-02 against a
  replayed recording: a one-minute export of 31 fragments and 1,860 frames verifies with
  `irisd inspect --verify`, and `ffprobe` reports a 62 s duration with the edit list's
  0.65 s start offset; the tail trim is in the edit list, which `ffprobe` does not apply
  to fragmented files when reporting duration.
  A fuzz target (`export_rebase`) covers the rewrites; its first minute found that a
  corrupt fragment could be rewritten with a valid trailer, so a fragment is parsed whole
  before any byte of it changes.
- `iris-replay`: a recording that is not the first file of its run starts at a large
  decode time, and the paced sender waited for it (hours) before the first frame. Frame
  times are relative to the file's first fragment; `synth::Plan::start_ticks` reproduces
  the case.
- Five fuzz findings, each within a minute of its target's first runs. The ONVIF timestamp
  parser indexed a string by byte offset and panicked on multibyte input, and overflowed on
  an unbounded zone hour: it rejects non-ASCII text and bounds every field. The scanner's
  child-box walk overflowed on a declared size near the address-space limit, and its
  init-segment walk on a 64-bit box size past the end of the file: both bound checks
  subtract instead of adding. The replay server's RTSP parser overflowed on a huge
  `Content-Length`: bodies are capped at 64 KiB. Each has a regression test from the crash
  input.
- The writer dropped a fragment that did not start with a sync sample when the open file
  had reached its size or age limit; rotation now waits for the next sync sample and the
  fragment goes into the open file (two such drops in nineteen hours of the soak, on split
  long GOPs of the secondary streams). Covered by `size_rotation_waits_for_a_sync_sample`.
- `WallUs::from_ntp64` rounds the NTP fraction to the nearest microsecond, so a wall time
  written to a `prft` box reads back exactly; before, it came back 1 µs early about half the
  time (found by the self-check on its first run).
