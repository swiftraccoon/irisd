# 06 — Clients: iOS app, Linux GUI, optional web UI

Full research notes with citations: `appendix/C-clients.md`. The daemon never decodes; clients
use hardware HEVC decode when available, and the Linux GUI falls back to software decode.

## 1. What the daemon exposes (shared by all clients)

| Output | Content | Latency | Use |
|---|---|---|---|
| WebSocket live | binary: type byte plus an init segment (`ftyp+moov`), then one single-sample `prft+moof+mdat`+trailer fragment per video frame; a sample-entry change sends a fresh init segment in-band | ≈ 100–300 ms glass-to-glass on a LAN (camera encode + network dominate) | GUI grid, planned iOS live view, web MSE |
| LL-HLS | in-memory fMP4 `hvc1` segments and 0.5 s parts from the live ring, blocking reload, preload hints | ≈ 1.5–3 s | planned iOS background/AirPlay, any HLS player |
| HLS VOD / timeline | byte-range playlists over the on-disk files | seek < 1 s | playback in all clients |
| MP4 export | assembled on the fly, `elst`-trimmed | — | downloads, sharing |
| Keyframe resource | init + one single-sample fragment (sub stream by default) | — | client-rendered thumbnails and scrubbing |
| Snapshot proxy | the camera's JPEG: the Dahua CGI endpoint, or the URL the camera's ONVIF Media service names (per `snapshot`) | — | zero-decode stills |
| JSON API + WS events | cameras, streams, segments, events, health | — | everything else |

WebRTC is outside the 0.1 alpha: it buys nothing on a LAN, needs its own stack, and native iOS HEVC
over WebRTC requires a custom VideoToolbox codec factory (upstream libwebrtc ships H.265 RTP
packetisation but no iOS H.265 codec). Revisit for remote access; Safari 18+ and Chrome 136+
decode H.265 in WebRTC (2026-10-31).

## 2. Planned iOS app (Swift)

No iOS target is present in the repository. This section records the phase-4 client design.

- **Live**: WebSocket → parse `moof/mdat` (or a raw-AU mode) →
  `CMVideoFormatDescriptionCreateFromHEVCParameterSets` (iOS 11+) → `CMSampleBuffer` →
  `AVSampleBufferDisplayLayer` (hardware decode, `kVTDecompressionPropertyKey_RealTime`,
  Picture-in-Picture via `AVPictureInPictureController.ContentSource(sampleBufferDisplayLayer:)`,
  iOS 15+). Transport via `URLSessionWebSocketTask`; `NWConnection` QUIC later if a VPN path
  needs it. Recreate decoder sessions on return from background (`kVTInvalidSessionErr`).
- **Playback / timeline**: `AVPlayer` with HLS (`hvc1` required in practice; Apple: fMP4
  MUST, `hvc1` SHOULD, IDR every 2 s SHOULD, `EXT-X-MAP` MUST, `EXT-X-INDEPENDENT-SEGMENTS`),
  AirPlay for free, LL-HLS for the live tab when the app is not using the WebSocket path.
- **Grid**: six sub-stream tiles (D1/720p), one `AVSampleBufferDisplayLayer` per visible
  tile, tear down off-screen tiles, switch the selected tile to the main stream. Apple
  publishes no concurrent-decoder limit (`VTDecompressionSession` ceiling is "high enough";
  `AVPlayer` observed ≈ 16 pipelines, paused players still hold decoders); budget ≈ 1.5× the
  decoded frame pool per tile (NV12 720p ≈ 1.4 MB/frame).
- **Audio**: `AVSampleBufferAudioRenderer` sharing a timebase with the display layer.

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
- A consumer NVIDIA GPU with a single NVDEC engine (the dev host's card): HEVC Main/Main10
  4:2:0, NVIDIA rates one engine at roughly 800–1,600 fps of 1080p HEVC depending on the
  generation, no session cap on decode. Six D1 tiles plus one 4K view ≈ 25 % of an engine;
  CPU ≈ 5–15 % of one core for parsing and compositing. The viewer links `gst-plugin-gtk4`, so
  it needs no distribution package for it.
- As built (`crates/iris-gui`): the tile pipeline is `appsrc ! qtdemux ! decodebin3 ! glupload !
  glcolorconvert ! gtk4paintablesink`, one such branch per tile inside a single shared pipeline (one
  GL context for the grid), frames staying on the GPU from the decoder to GTK's GL renderer (which
  the viewer selects itself; GTK's Vulkan renderer copies GL textures through the CPU). The daemon's
  live feed is a fragmented MP4 byte stream, so `qtdemux` demuxes it directly (no access-unit caps
  or `h265parse`), and `decodebin3` does the rank-based decoder choice. The sink runs
  `sync=false` for a live view. The window is a stack of two views, the grid and one focused camera
  on its main stream; only the visible view decodes, so a focused camera costs one main-stream
  decode and the grid costs one sub-stream decode per tile. Playback is a third view: the HLS VOD
  playlist of the window through `playbin3` (`hlsdemux2` fetches the byte ranges), with a timeline
  drawn from the segment and event lists. Seeking and the playhead use the VOD playlist's
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
  endpoint serves only the manifest; media still comes directly from the daemon's origin, and
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
  camera" form sends `POST /api/cameras`; its Discover button lists the cameras `GET
  /api/onvif/devices` found, and picking one fills the form's ONVIF service row (HTTPS, port, and
  certificate verification). Its "Fetch streams" button fills the main and sub URLs from `POST
  /api/onvif/streams` at that port and scheme once a device or a host and the credentials are given,
  waiting up to 70 s, beyond the daemon's 60 s lookup bound. A device that advertises HTTPS but
  serves ONVIF only over HTTP (`01-camera-probe.md` §7) answers the lookup with an error; unticking
  HTTPS, which moves port 443 to 80, and fetching again is the recourse, and unticking certificate
  verification reaches a camera with a self-signed certificate. The form refuses stream URLs that
  carry credentials. The focused view's "Remove camera" button, after confirmation, sends `DELETE
  /api/cameras/{camera}`. All need `[provisioning] enabled` on the daemon. Add and Remove report in
  the status bar, and a camera change rebuilds the grid from `/api/cameras`; Discover, Fetch
  streams, and a problem with the typed values report in the form, which stays open. A viewer-wide
  budget (`--live-budget-mib`, default 256) bounds the live payload bytes alive anywhere in the
  viewer, from the copy into `appsrc` until GStreamer frees the buffer; a payload it cannot admit
  fails its feed, which reconnects after 2 s. Playback widget callbacks hold weak references so
  dropping the view releases its resources. API reads apply connect/read-idle deadlines and
  endpoint-specific byte, row, string, and time-domain limits. Remote viewing goes through a TLS
  reverse proxy: `--api` names the proxy's origin alone (the client joins absolute paths, so a path
  prefix is refused), an `https` API gives `wss` live feeds, and both verify the proxy with one
  `iris-tls` configuration, the platform's trust store plus the authorities of an optional
  `--ca-file` for a private CA. Plain HTTP to a host other than loopback is refused unless
  `--allow-insecure-http` accepts an unencrypted bearer and video. The bearer belongs in
  `IRISD_TOKEN`; a `--token` argument draws a warning and moves into the environment when the viewer
  re-executes itself for the GL renderer. Export marks a range on that timeline and streams the
  daemon's MP4 export into an exclusively created sibling temporary chosen with the system save
  dialog. Before the first byte and after every further 64 MiB it checks the destination's free
  space with `fstatvfs` and stops, removing the temporary, below the larger of 1 GiB and 2 % of the
  filesystem. A complete nonempty MP4 is synced and atomically renamed over the final path; progress
  keeps one value, the most recent. The dated measurements below cover software decode, NVDEC,
  renderers, and the shared stage.
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
| iOS playback | LL-HLS / HLS byte ranges | `AVPlayer` | 1.5–3 s live, < 1 s seek | none app-side |
| GUI grid (6 sub + 1 main) | WS per-frame fMP4 | NVDEC or VA-API, DMA-BUF/GL | 100–250 ms | 5–15 % of one core |
| Web | WS → MSE / HLS | browser hardware decode | 0.5–1 s / 2–4 s | browser |

## 6. Open questions

1. Whether a segment byte range spanning several on-disk fragments (with `free` trailers)
   plays in AVFoundation, or whether playlists must map one fragment per segment.
2. Real per-device concurrent `VTDecompressionSession` ceilings on target iPhones.
3. Whether Safari 18+ WebRTC H.265 is default-on for send and receive, if WebRTC is ever
   added for remote access.
