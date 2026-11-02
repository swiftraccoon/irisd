# 06 — Clients: iOS app, Linux GUI, optional web UI

Full research notes with citations: `appendix/C-clients.md`. The daemon never decodes; every
client hardware-decodes HEVC.

## 1. What the daemon exposes (shared by all clients)

| Output | Content | Latency | Use |
|---|---|---|---|
| WebSocket live | binary: init segment (`ftyp+moov`), then one `moof+mdat` per frame per track; control messages for sample-entry changes and gaps | ≈ 100–300 ms glass-to-glass on a LAN (camera encode + network dominate) | GUI grid, iOS live view, web MSE |
| LL-HLS | fMP4 `hvc1`, 2–6 s segments as byte ranges, 0.5 s parts from the live ring, blocking reload, preload hints, HTTP/2 | ≈ 1.5–3 s | iOS background/AirPlay, any HLS player |
| HLS VOD / timeline | byte-range playlists over the on-disk files | seek < 1 s | playback in all clients |
| MP4 export | assembled on the fly, `elst`-trimmed | — | downloads, sharing |
| Keyframe resource | init + one single-sample fragment (sub stream by default) | — | client-rendered thumbnails and scrubbing |
| Snapshot proxy | camera `snapshot.cgi` JPEG | — | zero-decode stills |
| JSON API + WS events | cameras, streams, segments, events, health | — | everything else |

WebRTC is not in v1: it buys nothing on a LAN, needs its own stack, and native iOS HEVC
over WebRTC requires a custom VideoToolbox codec factory (upstream libwebrtc ships H.265 RTP
packetisation but no iOS H.265 codec). Revisit for remote access; Safari 18+ and Chrome 136+
already decode H.265 in WebRTC.

## 2. iOS app (Swift)

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

**Choice: gtk4-rs (+ libadwaita; relm4 optional) with gstreamer-rs, one pipeline per tile.**

```
appsrc (hvc1 AUs from the WebSocket feed) ! h265parse ! {vah265dec | nvh265dec ! glcolorconvert | avdec_h265} ! gtk4paintablesink
```

- `gtk4paintablesink` comes from `gst-plugin-gtk4` 0.15.2, written in Rust and linkable as a
  crate, so the GUI stays a single Rust binary. It accepts DMA-BUF (`DMA_DRM`, GTK ≥ 4.14) for
  true zero-copy from `vah265dec` on Intel/AMD, and `GLMemory` from `nvh265dec` on NVIDIA
  (one GPU-side CUDA→GL copy, no CPU involvement; desktop GL only).
- Decoder selection by GStreamer rank: `vah265dec` (PRIMARY+1 since 1.24) on VA-API hosts,
  `nvh265dec` on NVIDIA, `avdec_h265` software fallback with a warning.
- A consumer NVIDIA GPU with a single NVDEC engine (the dev host's card): HEVC Main/Main10
  4:2:0, NVIDIA rates one engine at roughly 800–1,600 fps of 1080p HEVC depending on the
  generation, no session cap on decode. Six D1 tiles plus one 4K view ≈ 25 % of an engine;
  CPU ≈ 5–15 % of one core for parsing and compositing. On this box `gstreamer1-plugin-gtk4`
  is not installed and there is no `va` plugin (no iGPU); `nvcodec` 1.24.13 is present.
- As built (`crates/iris-gui`): the tile pipeline is `appsrc ! qtdemux ! decodebin3 !
  glupload ! glcolorconvert ! gtk4paintablesink`, one such branch per tile inside a single
  shared pipeline (one GL context for the grid), frames staying on the GPU from the
  decoder to GTK's GL renderer (which the viewer selects itself; GTK's Vulkan renderer
  copies GL textures through the CPU). The daemon's live feed is already a fragmented MP4
  byte stream, so `qtdemux` replaces the access-unit `appsrc` and `h265parse` above, and
  `decodebin3` does the rank-based decoder choice. The sink runs `sync=false` for a live
  view. The window is a stack of two views, the grid and one focused camera on its main
  stream; only the visible view decodes, so a focused camera costs one main-stream decode
  and the grid costs one sub-stream decode per tile. Playback is a third view: the HLS VOD
  playlist of the window through `playbin3` (`hlsdemux2` fetches the byte ranges), with a
  timeline drawn from the segment and event lists; a click seeks by mapping wall-clock time
  to playlist time across the gaps between segments. Export marks a range on that timeline
  and streams the daemon's MP4 export into a file chosen with the system save dialog. Seven 1080p tiles, one 4K main
  stream, and 4K playback on the software decoder and on NVDEC are measured in
  `CHANGELOG.md`; on the NVIDIA driver the GPU path leaves the driver's own GL and CUDA
  work as the cost (about 120 % of one core for seven 1080p tiles), well above the 15 %
  target set with VA-API zero-copy in mind.
- Fallback widget: `libmpv2` 7.0.0 (one `mpv_render_context` per core, `hwdec=nvdec|vaapi`),
  if a driver/GL combination misbehaves with the GStreamer path.
- Rejected: egui/iced/Slint (no maintained zero-copy video path; frame uploads through the
  CPU), Tauri/web (HEVC in Chromium on Linux needs VA-API; NVIDIA requires the third-party
  `nvidia-vaapi-driver`), hand-rolled FFmpeg + wgpu interop (weeks of unsafe plumbing that
  GStreamer already provides).

## 4. Web UI (optional, later)

Viable without transcoding only on hardware with an HEVC decoder: Chrome/Edge (≥ 107
desktop; Linux via VA-API, Intel-class GPUs), Safari (HLS and MSE; iPhone MSE needs iOS 17.1
`ManagedMediaSource`), Firefox ≥ 137 (VA-API on Linux). Detect with `MediaCapabilities` /
`isTypeSupported("video/mp4; codecs=hvc1…")` and show a clear "hardware HEVC required" state;
feed MSE from the WebSocket per-frame fragments; HLS `<source>` for iPhone Safari < 17.1.
WebCodecs HEVC is Chrome-only today.

## 5. Expectations

| Client path | Transport | Decode | Latency | CPU |
|---|---|---|---|---|
| iOS live | WS per-frame fMP4 | VideoToolbox via `AVSampleBufferDisplayLayer` | 150–300 ms | demux only |
| iOS playback | LL-HLS / HLS byte ranges | `AVPlayer` | 1.5–3 s live, < 1 s seek | none app-side |
| GUI grid (6 sub + 1 main) | WS per-frame fMP4 | NVDEC or VA-API, DMA-BUF/GL | 100–250 ms | 5–15 % of one core |
| Web | WS → MSE / HLS | browser hardware decode | 0.5–1 s / 2–4 s | browser |

## 6. Open questions

1. Whether a segment byte range spanning several on-disk fragments (with `free` trailers)
   plays in AVFoundation, or whether playlists must map one fragment per segment.
2. gtk4paintablesink on the NVIDIA proprietary driver: Wayland vs X11/EGL stability on
   the dev host's distribution and GTK version (needs a spike).
3. Real per-device concurrent `VTDecompressionSession` ceilings on target iPhones.
4. Whether Safari 18+ WebRTC H.265 is default-on for send and receive, if WebRTC is ever
   added for remote access.
