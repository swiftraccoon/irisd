# AGENTS.md — iris-gui

Scope: this crate only. Read the root `AGENTS.md` first.

## Invariants

- The viewer's Rust code never parses video: the daemon's byte stream goes straight into
  `appsrc` and `qtdemux`. GStreamer's native elements parse and decode camera-originated
  media inside the viewer process without a sandbox. Changing the live feed format is an API
  change (`crates/iris-api`, `docs/research/07-architecture.md §5`), not a viewer change.
- The live message ceiling is 16 MiB + 1 bytes (`live::LIVE_MESSAGE_BYTES`), shared with the
  daemon, which closes with 1009 when a sync frame exceeds it. Keep it: raising it multiplies
  worst-case memory per tile. A 1009 close or an oversized message is the tile's oversized
  state with a 2 s backoff doubling to 60 s, never a 2 s reconnect loop.
- `--api` is `http` or `https` without userinfo; plain HTTP to a host other than loopback
  needs `--allow-insecure-http`. `wss` verifies against the platform's native roots; tests
  inject a fixture root through `live::session`'s connector.
- The grid starts at most `--max-tiles` feeds. Exports check `fstatvfs` free space before the
  first byte and every 64 MiB, and stop when it falls below the larger of 1 GiB and 2 % of
  the filesystem.
- A `--token` argument is warned about and moved into `IRISD_TOKEN` by the renderer
  re-execution; never forward it on a command line.
- GTK widgets are touched only from the main loop (`glib::spawn_future_local`); GStreamer
  objects may be driven from the runtime threads (`tile::Source` is `Send`, with one producer).
- `Source::push` rejects a payload before copying if queued plus incoming bytes exceed
  16 MiB. Keep `appsrc` nonblocking: a rejection fails the session, restarts the branch,
  and reconnects from init plus sync; dropping arbitrary byte chunks corrupts decoding.
- Playback mapping uses the VOD playlist's exact `#IRIS-FRAGMENT` timing, including whole
  boundary GOPs and intra-file gaps. Accumulate 90 kHz ticks before converting to microseconds.
  Fetch the playlist once: the same selection feeds both the mapping and GStreamer.
  Resolve media and init references against the API origin, preserving queries; references
  to other origins or schemes, userinfo, `#EXT` tags outside `api::VOD_TAGS`, and redirects
  reject the playlist. Open segment rows follow the API's overlap rule; drop those that
  recorded nothing inside the window before deciding it is empty.
- A playback snapshot serves only its immutable playlist on IPv4 loopback behind a
  256-bit OS-random capability path. Its token-bearing body and capability stay out of
  logs and temporary files. Closing, replacing, or failing playback drops the snapshot
  listener; cancel pending loads without blocking the GTK main loop. Snapshots do not pin
  media files against retention, so missing media can fail playback.
  Bus errors defer cleanup until their callback returns; an error from a replaced player
  must not close its replacement.
- Only the visible view decodes. Any new view stops the pipelines it hides (`Tile::stop`)
  and starts what it shows (`Tile::start`); a feed task outliving its tile is a bug.
- The root bearer never appears in URLs, logs, or window titles. Initial VOD and WebSocket
  requests send it in `Authorization`; playlist media references carry scoped capabilities
  bounded to the selected playback horizon instead.

## Where things are

- `src/main.rs` — options, GStreamer and plugin initialisation, the window with its grid
  and focused views, the main-loop event handler (`show_grid`, `focus`).
- `src/api.rs` — the HTTP client (cameras).
- `src/live.rs` — the per-tile WebSocket feed and reconnection.
- `src/timeline.rs` — the timeline widget and the playlist-time mapping.
- `src/playback.rs` — the playback view over `playbin3`, with the thumbnail strip.
- `src/snapshot.rs` — the private immutable playlist endpoint, owned by a running player.
- `src/thumbs.rs` — keyframe decoding to an image, off the main loop.
- `src/stage.rs` — the shared live pipeline, its bus watch, branch add, restart, remove.
- `src/tile.rs` — the widgets, the branch lifecycle (`start`, `stop`), readiness, restarts.

## How to verify a change

```
cargo fmt -p iris-gui
cargo clippy -p iris-gui --all-targets -- -D warnings
cargo test -p iris-gui
cargo test -p iris-gui --bin iris-gui -- --ignored --test-threads=1  # run under xvfb-run
cargo test -p iris-gui --test playback_snapshot -- --ignored  # ffmpeg/libx264 + Xvfb + HLS/H.264 plugins
cargo test -p iris-gui --test playback_origin -- --ignored  # Xvfb
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
- The playback view holds its widgets in an `Rc`; button callbacks, the position timer,
  and the timeline's seek callback hold `Weak` references so dropping the view stops them.
- Thumbnails decode in `spawn_blocking` tasks (one short pipeline each) and reach the main
  loop as RGB images; the texture is created there. A slot whose time falls in a gap stays
  empty rather than showing a neighbouring frame.
- The export runs on the tokio runtime and reports progress through a channel; GTK labels
  are only touched from the main loop. The export response has no content length, so
  progress is bytes written, not a fraction.

## Related documents

- `docs/research/06-clients.md §3`, `08-roadmap.md` (phase 3), `crates/iris-api/README.md`.
