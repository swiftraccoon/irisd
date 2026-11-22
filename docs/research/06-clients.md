# 06 — Clients: iOS and macOS app, Linux GUI, optional web UI

Full research notes with citations: `appendix/C-clients.md`. The daemon never decodes; clients
use hardware HEVC decode when available, and the Linux GUI falls back to software decode.

## 1. What the daemon exposes (shared by all clients)

| Output | Content | Latency | Use |
|---|---|---|---|
| WebSocket live | binary: type byte plus an init segment (`ftyp+moov`), then one single-sample `prft+moof+mdat`+trailer fragment per video frame; a sample-entry change sends a fresh init segment in-band | ≈ 100–300 ms glass-to-glass on a LAN (camera encode + network dominate) | GUI grid, planned iOS live view, web MSE |
| LL-HLS | in-memory fMP4 `hvc1` segments and 0.5 s parts from the live ring, blocking reload, preload hints | ≈ 1.5–3 s | any HLS player; the planned iOS and macOS app does not play it (`15-apple-clients.md` §4) |
| WebSocket live audio | the same protocol for a camera's recorded AAC: an `mp4a` init segment, then one fragment per audio frame, with the decode times the recording gives them (`13-audio.md` §10) | frames arrive in bursts, in steps up to 0.36 s; a player holds sound and picture about half a second | GUI focused view, planned iOS live view |
| HLS VOD / timeline | byte-range playlists over the on-disk files; a camera's multivariant playlist names its recorded audio as a rendition whose segments follow the video's (`13-audio.md` §8) | seek < 1 s | playback in all clients |
| MP4 export | assembled on the fly, `elst`-trimmed; a camera's export holds its recorded audio as a second track (`13-audio.md` §9) | — | downloads, sharing |
| Keyframe resource | init + one single-sample fragment (sub stream by default) | — | client-rendered thumbnails and scrubbing |
| Snapshot proxy | the camera's JPEG: the Dahua CGI endpoint, or the URL the camera's ONVIF Media service names (per `snapshot`) | — | zero-decode stills |
| JSON API + WS events | cameras, streams, segments, events, health | — | everything else |

WebRTC is outside the 0.1 alpha: it buys nothing on a LAN, needs its own stack, and native iOS HEVC
over WebRTC requires a custom VideoToolbox codec factory (upstream libwebrtc ships H.265 RTP
packetisation but no iOS H.265 codec). Revisit for remote access; Safari 18+ and Chrome 136+
decode H.265 in WebRTC (2026-10-31).

## 2. Planned iOS and macOS app (Swift)

No iOS or macOS app target is present in the repository. This section records the phase-4 client
design. `15-apple-clients.md` has its research and decisions, for iOS and macOS, with the Rust
library crate `iris-client`, which the Linux viewer uses (§3 below). Where the two documents differ,
15 decides.

- **Live**: WebSocket → `iris_client::live::LiveDecoder` →
  `CMVideoFormatDescriptionCreateFromHEVCParameterSets` (iOS 11+) → `CMSampleBuffer` →
  `AVSampleBufferVideoRenderer`. `LiveDecoder` reads each message with the daemon's fragment
  reader. It gives the decoder configuration of an init segment and the one sample of a fragment.
  Picture-in-Picture goes through the `AVSampleBufferDisplayLayer` whose `sampleBufferRenderer` is
  that renderer, via
  `AVPictureInPictureController.ContentSource(sampleBufferDisplayLayer:playbackDelegate:)`
  (iOS 15+). The transport is `URLSessionWebSocketTask`; `NWConnection` with QUIC is the
  alternative for a VPN path that needs it. The receiver that a render synchronizer gives for the
  renderer reports a lost decoder as `cancelledDueToFlushRequiredToResume` in the result of an
  `enqueue`. The receiver then needs a flush. The renderer's `requiresFlushToResumeDecoding` is
  deprecated for Swift from iOS 27 and macOS 27, the deployment targets. Recovery is a flush and
  then a sync frame, which the view takes from a fresh session (`15-apple-clients.md` §1, §4, §9.4).
- **Playback / timeline**: `AVPlayer` with HLS (`hvc1` required in practice; Apple: fMP4 MUST,
  `hvc1` SHOULD, IDR every 2 s SHOULD, `EXT-X-MAP` MUST, `EXT-X-INDEPENDENT-SEGMENTS`). The app
  fetches each playlist with the bearer, `iris-client` checks and rewrites it, and a resource loader
  hands it to `AVPlayer` (`15-apple-clients.md` §4).
- **Grid**: six sub-stream tiles (D1/720p), one `AVSampleBufferDisplayLayer` per visible tile.
  Off-screen tiles are torn down, and the selected tile switches to the main stream. Apple publishes
  no concurrent-decoder limit, and an Apple engineer calls the `VTDecompressionSession` ceiling
  "high enough" (`appendix/C-clients.md` A5). A ceiling of about 16 `AVPlayer` pipelines, with
  paused players holding their decoders, is **unverified**. `appendix/C-clients.md` A5 cites a
  developer forum thread and a browser bug report for it, and no measurement with a date or a
  command is recorded. The budget is about 1.5× the decoded frame pool per tile (NV12 720p ≈ 1.4
  MB/frame).
- **Audio**: `AVSampleBufferAudioRenderer` on one `AVSampleBufferRenderSynchronizer` with the video
  renderer, fed from the camera's audio WebSocket. What the Linux viewer does applies to any player
  of the two feeds (`13-audio.md` §10), and `iris_client::sound_clock` has the placement rule. Audio
  frames arrive in bursts, so sound and picture are both held about half a second against one clock.
  Each feed is placed by the least of its arrival time less its media time, which needs no shared
  timeline (the audio's is the main stream's, a sub stream has its own). `GET /api/cameras` says
  which cameras have audio (`has_audio`).

Expected: decode on silicon, CPU spent only on demux; battery dominated by the display.

## 3. Linux GUI (Rust)

**Implemented choice: gtk4-rs with gstreamer-rs, one shared live pipeline with a branch per
tile.**

- `gtk4paintablesink` comes from `gst-plugin-gtk4` 0.15.2, written in Rust and linkable as a
  crate, so the GUI stays a single Rust binary. It accepts DMA-BUF (`DMA_DRM`, GTK ≥ 4.14) for
  true zero-copy from `vah265dec` on Intel/AMD, and `GLMemory` from `nvh265dec` on NVIDIA
  (one GPU-side CUDA→GL copy, no CPU involvement; desktop GL only).
- Decoder selection by GStreamer rank: `vah265dec` (PRIMARY+1 since 1.24) on VA-API hosts,
  `nvh265dec` on NVIDIA, `avdec_h265` as the software fallback; `decodebin3` chooses, and the
  viewer does not log the choice.
- A consumer NVIDIA GPU with a single NVDEC engine: HEVC Main/Main10
  4:2:0, NVIDIA rates one engine at roughly 800–1,600 fps of 1080p HEVC depending on the
  generation, no session cap on decode. Six D1 tiles plus one 4K view ≈ 25 % of an engine;
  CPU ≈ 5–15 % of one core for parsing and compositing. The viewer links `gst-plugin-gtk4`, so
  it needs no distribution package for it.
- As built (`crates/iris-gui`): the tile pipeline is
  `appsrc ! qtdemux ! decodebin3 ! glupload ! glcolorconvert ! gtk4paintablesink`, one such branch
  per tile inside a single shared pipeline (one GL context for the grid). Frames stay on the GPU
  from the decoder to GTK's GL renderer, which the viewer selects itself. GTK's Vulkan renderer
  copies GL textures through the CPU. The daemon's live feed is a fragmented MP4 byte stream, so
  `qtdemux` demuxes it directly (no access-unit caps or `h265parse`), and `decodebin3` does the
  rank-based decoder choice. The sink runs `sync=false` for a live view; the focused view of a
  camera with audio also plays its audio feed, and then holds both media half a second against the
  pipeline's clock (`13-audio.md` §10.4). The window is a stack of two views, the grid and one
  focused camera on its main stream. Only the visible view decodes, so a focused camera costs one
  main-stream decode and the grid costs one sub-stream decode per tile. Playback is a third view:
  the HLS VOD playlist of the window through `playbin3` (`hlsdemux2` fetches the byte ranges), with
  a timeline drawn from the segment and event lists. Seeking and the playhead use the VOD playlist's
  `#IRIS-FRAGMENT` comments in served order: each carries the fragment's wall start and 90 kHz
  duration. This includes whole boundary GOPs, skips gaps within and between files, and omits files
  whose sidecars cannot be served. Cumulative ticks convert to microseconds at each playlist
  boundary, avoiding per-fragment rounding drift. The viewer fetches one VOD body for both timing
  and playback. It resolves init and media references against the fetched response URL, preserving
  authentication queries, requires every one to stay on the API origin (scheme, host, port) without
  userinfo, and refuses a body carrying any tag the daemon's VOD writer does not emit, so a hostile
  API or an on-path attacker cannot steer GStreamer to other hosts, `file:` URIs, or key and
  rendition fetches; the client follows no redirects. It serves that frozen playlist to GStreamer
  through a loopback endpoint behind a 256-bit OS-random capability path. Repeated player requests
  receive the same selection, even if appends or retention change a later daemon playlist. The
  endpoint serves only the manifest; media itself comes directly from the daemon's origin, and
  removal of a selected file can fail playback. A segment listing follows the API's overlap rule: an
  open row counts as extending to the present, and open rows that recorded nothing inside the window
  are dropped, so a camera offline for longer than the window, or a file before its first sync,
  reads as nothing recorded rather than an error. Opening and automatic export run outside the
  serial UI event receiver. Closing or replacing playback cancels pending playlist, thumbnail, and
  export work, drops its listener without waiting on the GTK main loop, and generation-fences a
  running thumbnail decode that cannot be interrupted. Across all playback views and replacements,
  two thumbnail fetches/decoders can run together; a running decoder retains its slot through
  cancellation. Decoded RGB is limited to 4,096 pixels per side and 64 MiB. A playback error also
  releases its pipeline and listener after the bus callback returns. Tokens and capability paths
  stay out of the viewer's own logs and temporary files; the log output layer rejects tungstenite's
  raw client-handshake target even when a broad trace filter is selected. GStreamer's debug log is
  the exception: at `GST_DEBUG=4` it prints the snapshot URL with its capability path, and at 5 and
  above it dumps the playlist, media capabilities included. Live WebSocket frames/messages are
  limited to the 16 MiB payload plus one type byte, the live protocol's 16 MiB + 1 ceiling: the
  daemon skips an oversized non-sync frame and closes with 1009 when a sync frame exceeds it, and
  the viewer shows that, or a message above its own limit, as an oversized tile state, retrying
  after 2 s and doubling to 60 s until video flows again. `appsrc` input has 16 MiB and 512-buffer
  queue limits enforced before copying each nonempty payload; overflow, a text message, or an
  unknown fixed-protocol type reconnects from init and sync after 2 s without blocking the runtime.
  The grid decodes at most `--max-tiles` live tiles (default 16, so at most 256 MiB of queued live
  input); the rest are labelled and open their single-camera view on a click. The grid's "Add
  camera" form sends `POST /api/cameras`; its Discover button lists the cameras
  `GET /api/onvif/devices` found, and picking one fills the form's ONVIF service row (HTTPS, port,
  and certificate verification). Its "Fetch streams" button fills the main and sub URLs from
  `POST /api/onvif/streams` at that port and scheme once a device or a host and the credentials are
  given. It waits up to 70 s, beyond the daemon's 60 s lookup bound. A device that advertises HTTPS
  but serves ONVIF only over HTTP (`01-camera-probe.md` §7) answers the lookup with an error.
  Unticking HTTPS, which moves port 443 to 80, and fetching again is the recourse, and unticking
  certificate verification reaches a camera with a self-signed certificate. The form refuses stream
  URLs that carry credentials. The focused view's "Remove camera" button, after confirmation, sends
  `DELETE /api/cameras/{camera}`. All need `[provisioning] enabled` on the daemon. Add and Remove
  report in the status bar, and a camera change rebuilds the grid from `/api/cameras`; Discover,
  Fetch streams, and a problem with the typed values report in the form, which stays open. A
  viewer-wide budget (`--live-budget-mib`, default 256) bounds the live payload bytes alive anywhere
  in the viewer, from the copy into `appsrc` until GStreamer frees the buffer. A payload it cannot
  admit fails its feed, which reconnects after 2 s. Playback widget callbacks hold weak references
  so dropping the view releases its resources. API reads apply connect/read-idle deadlines and
  endpoint-specific byte, row, string, and time-domain limits. Remote viewing goes through a TLS
  reverse proxy: `--api` names the proxy's origin alone (the client joins absolute paths, so a path
  prefix is refused), an `https` API gives `wss` live feeds, and both verify the proxy with one
  `iris-tls` configuration, the platform's trust store plus the authorities of an optional
  `--ca-file` for a private CA. Plain HTTP to a host other than loopback is refused unless
  `--allow-insecure-http` accepts an unencrypted bearer and video. The bearer belongs in
  `IRISD_TOKEN`; a `--token` argument draws a warning and moves into the environment when the viewer
  re-executes itself for the GL renderer. The viewer reads its token as the daemon reads a presented
  one: it trims the value, takes an empty result as no token, and refuses at startup a value with a
  character outside visible ASCII. Export marks a range on that timeline and streams the daemon's
  MP4 export into an exclusively created sibling temporary chosen with the system save dialog.
  Before the first byte and after every further 64 MiB it checks the destination's free space with
  `fstatvfs` and stops, removing the temporary, below the larger of 1 GiB and 2 % of the
  filesystem. A complete nonempty MP4 is synced and atomically renamed over the final path; progress
  keeps one value, the most recent. Export refuses a range over seven days in the status line before
  the save dialog opens, and leaves a running export running. The dated measurements below cover
  software decode, NVDEC, renderers, and the shared stage.
- The viewer uses the library crate `iris-client` (`crates/iris-client`), which is shaped for the
  planned Apple viewer that `15-apple-clients.md` designs. `iris-client` has the API origin with its
  plain-HTTP policy, the requests and the checks of their answers, and the answer types and
  validators. It has the VOD playlist rules, the timeline model, the playback selection, and the
  sound placement rule. For live feeds it has a fragment's media time and the live protocol with its
  reconnect policy and byte budget. The viewer's HTTP client, export download, and live feed loop
  are `iris-transport` (`crates/iris-transport`), which depends on `iris-client` and which only the
  viewer depends on. GTK, GStreamer, the timeline widget, and the loopback endpoint that serves a
  playback snapshot are in the viewer (`crates/iris-gui`). The viewer pushes its live feeds into
  `qtdemux` and does not use `iris_client::live::LiveDecoder`.
- Considered fallback, not implemented: `libmpv2` 7.0.0 (one `mpv_render_context` per core,
  `hwdec=nvdec|vaapi`), if a driver/GL combination misbehaves with the GStreamer path.
- Rejected: egui/iced/Slint (no maintained zero-copy video path; frame uploads through the
  CPU), Tauri/web (HEVC in Chromium on Linux needs VA-API; NVIDIA requires the third-party
  `nvidia-vaapi-driver`), hand-rolled FFmpeg + wgpu interop (weeks of unsafe plumbing that
  GStreamer already provides).

### GUI measurements (2026-11-01 to 2026-11-02)

Measurements used an x86-64 host and a headless display; the host had no VA-API plugin (no
integrated GPU) and GStreamer's `nvcodec` 1.24.13. Percentages are of one CPU core.
The Xvfb software path and a headless Wayland compositor with NVIDIA EGL at 60 Hz exercised
the same seven 1080p30 tiles and one 4K30 main stream:

| View | Software decode, Xvfb | NVDEC, CPU conversion, Xvfb | NVDEC, GPU path, Vulkan renderer | NVDEC, GPU path, GL renderer |
|---|---:|---:|---:|---:|
| Seven-tile grid | 435 % | 326 % | 218 % | 122 % |
| One 4K main, live | 265 % | 250 % | — | 46 % |
| One 4K main, playback | 325 % | — | — | 75 % |

The shared live stage measured 95–117 % (median 111 % over three runs) and 590 MiB RSS for
seven tiles, versus 115–122 % and 1,215 MiB for one pipeline per tile. A single 4K live view
measured 49 % with the stage versus 46 % separately. On this driver the stage's GLES context
makes the decoder use system memory; forcing desktop GL and CUDA–GL interop cost 177 % for the
grid. Software-only smoke runs varied from 390–435 % and 790–830 MiB for seven tiles.

The NVIDIA GL path is dominated by driver EGL, CUDA–GL, and kernel work. It remains above the
phase-3 target of 15 % for six tiles plus one main view, which assumed VA-API DMA-BUF zero-copy.

## 4. Optional web UI design

Viable without transcoding only on hardware with an HEVC decoder: Chrome/Edge (≥ 107
desktop; Linux via VA-API, Intel-class GPUs), Safari (HLS and MSE; iPhone MSE needs iOS 17.1
`ManagedMediaSource`), Firefox ≥ 137 (VA-API on Linux). Detect with `MediaCapabilities` /
`isTypeSupported("video/mp4; codecs=hvc1…")` and show a clear "hardware HEVC required" state;
feed MSE from the WebSocket per-frame fragments; HLS `<source>` for iPhone Safari < 17.1.
WebCodecs HEVC decoding is a Chromium-only capability.

## 5. Design targets

| Client path | Transport | Decode | Latency | CPU |
|---|---|---|---|---|
| iOS live | WS per-frame fMP4 | VideoToolbox via `AVSampleBufferDisplayLayer` | 150–300 ms | demux only |
| iOS playback | HLS byte ranges | `AVPlayer` | < 1 s seek | none app-side |
| GUI grid (6 sub + 1 main) | WS per-frame fMP4 | NVDEC or VA-API, DMA-BUF/GL | 100–250 ms | 5–15 % of one core |
| Web | WS → MSE / HLS | browser hardware decode | 0.5–1 s / 2–4 s | browser |

## 6. Open questions

1. Whether a segment byte range spanning several on-disk fragments (with `free` trailers) plays in
   AVFoundation, or whether playlists must map one fragment per segment. On macOS 27 such a segment
   played with every sample presented (`15-apple-clients.md` §3.2, a dated observation of
   2026-11-15), so playlists need not map one fragment per segment there. On iOS it is
   **unverified** (§10 of that document).
2. Real per-device concurrent `VTDecompressionSession` ceilings on target iPhones.
3. Whether Safari 18+ WebRTC H.265 is default-on for send and receive, if WebRTC is ever
   added for remote access.
