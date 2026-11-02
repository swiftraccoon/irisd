# AGENTS.md — iris-gui

Scope: this crate only. Read the root `AGENTS.md` first.

## Invariants

- No video parsing in the viewer: the daemon's byte stream goes straight into `appsrc` and
  `qtdemux`. Changing the live feed format is an API change (`crates/iris-api`,
  `docs/research/07-architecture.md §5`), not a viewer change.
- GTK widgets are touched only from the main loop (`glib::spawn_future_local`); GStreamer
  objects may be driven from the runtime threads (`tile::Source` is `Clone + Send`).
- Only the visible view decodes. Any new view stops the pipelines it hides (`Tile::stop`)
  and starts what it shows (`Tile::start`); a feed task outliving its tile is a bug.
- The token never appears in logs or window titles; it travels as the `token` query parameter
  of the WebSocket URL because the API accepts it there.

## Where things are

- `src/main.rs` — options, GStreamer and plugin initialisation, the window with its grid
  and focused views, the main-loop event handler (`show_grid`, `focus`).
- `src/api.rs` — the HTTP client (cameras).
- `src/live.rs` — the per-tile WebSocket feed and reconnection.
- `src/timeline.rs` — the timeline widget and the playlist-time mapping.
- `src/playback.rs` — the playback view over `playbin3`, with the thumbnail strip.
- `src/thumbs.rs` — keyframe decoding to an image, off the main loop.
- `src/stage.rs` — the shared live pipeline, its bus watch, branch add, restart, remove.
- `src/tile.rs` — the widgets, the branch lifecycle (`start`, `stop`), readiness, restarts.

## How to verify a change

```
cargo fmt -p iris-gui
cargo clippy -p iris-gui --all-targets -- -D warnings
cargo test -p iris-gui
RUSTDOCFLAGS="-D warnings" cargo doc -p iris-gui --no-deps
cargo build --release -p iris-gui
IRISD_TOKEN=... timeout 45 xvfb-run -a target/release/iris-gui --api http://127.0.0.1:8554   # grid smoke run
IRISD_TOKEN=... timeout 45 xvfb-run -a target/release/iris-gui --focus cam1                   # focused view
IRISD_TOKEN=... timeout 45 xvfb-run -a target/release/iris-gui --focus cam1 --playback        # playback view
IRISD_TOKEN=... timeout 45 xvfb-run -a target/release/iris-gui --focus cam1 --playback --window 5 --export-to /tmp/x.mp4
```

The build needs the GTK 4 and GStreamer development libraries (`gtk4-devel`,
`gstreamer1-devel`, `gstreamer1-plugins-base-devel` on Fedora; `libgtk-4-dev`,
`libgstreamer1.0-dev`, `libgstreamer-plugins-base1.0-dev` on Debian, as CI installs). The
smoke run needs a daemon with at least one camera and `xorg-x11-server-Xvfb` or a session.

## Pitfalls

- GStreamer caches a plugin's feature list in `~/.cache/gstreamer-1.0/registry.*.bin`. If
  the NVIDIA plugin failed to initialise CUDA once (driver not loaded), `nvh265dec` stays
  absent until that file is deleted, even after the driver works.
- Keep the GL path intact: no `videoconvert` between the decoder and the sink, and the
  `gst-plugin-gtk4` features `wayland`, `x11egl`, `x11glx`, `dmabuf` enabled. A profile
  showing `video_chroma_up_*`, `pack_*`, or GTK's `r8g8b8*_to_float` means frames are
  crossing the CPU again.
- Messages that GStreamer bins aggregate over all sinks (stream-start, async-done) say
  nothing about one branch of the stage; readiness comes from a pad probe on the branch's
  sink, errors are attributed by walking the message source's parents to the branch bin.
- On the NVIDIA driver the stage's GL context is GLES, so `nvh265dec` falls back to
  system memory ("OpenGL context is not CUDA-compatible"); that path measured cheaper than
  CUDA-GL interop with desktop GL (`GST_GL_API=opengl3 GDK_DEBUG=gl-prefer-gl`), so the
  warning is expected and not worth chasing on that driver.
- GTK's Vulkan renderer copies the sink's GL textures through the CPU; `select_renderer`
  in `main.rs` forces the GL renderer by re-executing the process. `glib::setenv` is
  `unsafe` and the workspace forbids `unsafe`, hence the re-execution.
- Measuring without a desktop session: `weston --backend=headless --renderer=gl
  --refresh-rate=60000 --socket=wl-iris` uses the NVIDIA EGL, then run the viewer with
  `GDK_BACKEND=wayland WAYLAND_DISPLAY=wl-iris`. `xvfb-run` renders through llvmpipe and
  measures software compositing instead.

- `gst::parse::launch` returns an `Element`; downcast to `Pipeline` before `by_name`.
- Keep the `BusWatchGuard`; dropping it removes the watch silently.
- `tokio::runtime::Builder::new_multi_thread` needs the `rt-multi-thread` feature, which the
  workspace's tokio does not enable; this crate adds it.
- `reqwest` in the workspace has no `json` feature; decode bodies with `serde_json`.
- Under a virtual display the sink logs "Have too many pending frames" because nothing
  consumes the paintable at the frame rate; it is not a decode failure.
- `souphttpsrc` warns "No Accept-Ranges response header" once per playback for the playlist
  fetch; the segment files do answer ranges, and the warning is harmless.
- The API serialises an event kind either as a string or as `{"other": "..."}`;
  `api::EventKind` is untagged for that reason, and a listing with one vendor code in it
  fails to decode otherwise.
- The playback view holds its widgets in an `Rc`; the position timer and the timeline's
  seek callback hold `Weak` references so dropping the view stops them.
- Thumbnails decode in `spawn_blocking` tasks (one short pipeline each) and reach the main
  loop as RGB images; the texture is created there. A slot whose time falls in a gap stays
  empty rather than showing a neighbouring frame.
- The export runs on the tokio runtime and reports progress through a channel; GTK labels
  are only touched from the main loop. The export response has no content length, so
  progress is bytes written, not a fraction.

## Related documents

- `docs/research/06-clients.md §3`, `08-roadmap.md` (phase 3), `crates/iris-api/README.md`.
