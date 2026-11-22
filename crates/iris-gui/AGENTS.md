# AGENTS.md — iris-gui

Scope: this crate only. Read the root `AGENTS.md` first.

## Invariants

- The viewer's Rust code never parses video: the daemon's byte stream goes straight into
  `appsrc` and `qtdemux`. The one thing it reads from a live payload is the media time of the
  `prft` box a fragment starts with. It reads that time through `iris_client::live::media_time`
  (`crates/iris-client`, covered by the `live_fragment` fuzz target), in 90 kHz ticks that
  `iris_client::sound_clock::SoundClock` converts to nanoseconds. GStreamer's native elements parse
  and decode camera-originated media inside the viewer process without a sandbox. Changing the live
  feed format is an API change (`crates/iris-api`, `docs/research/07-architecture.md §5`), not a
  viewer change.
- The focused view's sound and picture are placed by `iris_client::sound_clock::SoundClock`, the
  rule of `docs/research/13-audio.md` §10.4. `docs/research/15-apple-clients.md` §4 has the planned
  iOS and macOS viewer place its live sound by the same rule. `sound::Pairing` hands the clock each
  fragment's media time with the pipeline's running time. It sets the offsets the clock returns on
  the pads after the demuxers, when the clock returns them and as they are. A change of the rule
  belongs in `crates/iris-client/src/sound_clock.rs` and its tests, not here.
- The live message ceiling is 16 MiB + 1 bytes (`iris_client::live::LIVE_MESSAGE_BYTES`,
  `crates/iris-client`), shared with the daemon, which closes with 1009 when a sync frame
  exceeds it. Keep it: raising it multiplies worst-case memory per tile. A 1009 close or an
  oversized message is the tile's oversized state with a 2 s backoff doubling to 60 s, never a
  2 s reconnect loop. The policy is `iris_client::live::Reconnect`. The loop of
  `iris_transport::Feed::run` (`crates/iris-transport/src/feed.rs`) calls its `delivered` for a
  session whose sink took a fragment, then its `after` with how the session ended. It sends the
  tile the events the answer calls for. A change of the policy belongs in
  `crates/iris-client/src/live/reconnect.rs` and its tests, and a change of the loop in
  `crates/iris-transport/src/feed.rs` and its tests, not here.
- `--api` is the daemon's origin, `http` or `https`, without userinfo, path, query, or
  fragment. The request builders and the live URL set absolute paths (`/api/...`) on it, which
  would replace a path prefix and send the bearer elsewhere on the host. Plain HTTP to a host
  other than loopback needs `--allow-insecure-http`. The rule is `iris_client::origin::ApiOrigin`
  (`crates/iris-client`), and the `Client` and `Feed` of `iris_transport` take every URL they
  request from it. `main::api_origin` takes `--api` under the policy that `Args::plain_http`
  selects: `PlainHttp::Anywhere` with the flag, and `PlainHttp::LoopbackOnly` without it. It
  appends the viewer's advice to the userinfo and plain-HTTP refusals, which startup prints whole.
  The viewer builds one `iris-tls` configuration: the platform's store plus any `--ca-file`
  authorities. It gives the client a copy and shares it among the feeds, so API reads and `wss`
  feeds verify alike.
- The transport is the crate `iris-transport` (`crates/iris-transport`), which this crate alone
  depends on: its `Client`, the export download, and the feed loop `Feed::run`. The viewer awaits
  the client's calls on its runtime, and shows a failure as `anyhow` writes its chain
  (`main::failure_text`, and `anyhow::Error::from` before `{:#}` elsewhere). A `ClientError`
  formatted alone, even with `{:#}`, shows its text without its sources. A change of a request or
  of a refusal's text belongs in `crates/iris-client/src/api/` (`ApiError`). A change of the text
  of a failure of the transport (`Transport`, `Read`, and `Reserve` of `ClientError`, and
  `ExportError`), of a log line of the feed, or of the reconnect loop belongs in
  `crates/iris-transport/src/`. Each belongs there with its tests, not here.
- The grid starts at most `--max-tiles` feeds. Exports check `fstatvfs` free space before the
  first byte and every 64 MiB, and stop when it falls below the larger of 1 GiB and 2 % of
  the filesystem (`Client::export`).
- A `--token` argument is warned about and moved into `IRISD_TOKEN` by the renderer
  re-execution; never forward it on a command line. `main::bearer` reads the token once at
  startup, as the daemon reads a presented one. It trims the value, takes an empty result as no
  token, and refuses a value with a character outside visible ASCII (`iris_transport::Bearer`).
  The client and every feed take a clone of that `Bearer`, and the refusal names the option,
  never the value.
- GTK widgets are touched only from the main loop (`glib::spawn_future_local`); GStreamer
  objects may be driven from the runtime threads (`tile::Source`, the tile's
  `iris_client::live::LiveSink`, is `Send` and `Sync`, with one producer).
- `Source::push_payload`, which the source's `LiveSink::push` calls, rejects a payload before
  copying if queued plus incoming bytes exceed 16 MiB. Keep `appsrc` nonblocking: a rejection
  fails the session, restarts the branch, and reconnects from init plus sync; dropping
  arbitrary byte chunks corrupts decoding.
- Playback mapping uses the VOD playlist's exact `#IRIS-FRAGMENT` timing, including whole boundary
  GOPs and intra-file gaps, through `iris_client::timeline_model::TimelineModel`
  (`crates/iris-client`), which accumulates 90 kHz ticks before converting to microseconds. Fetch
  the playlist once: the same selection (`iris_client::playback::PlaybackSelection`) feeds both the
  mapping and GStreamer. The playlist rules are `iris_client::vod` (`crates/iris-client`, covered by
  the `vod_playlist` fuzz target). Media and init references resolve against the playlist's
  response URL, keep their queries, and must stay on the API origin. References to other origins
  or schemes, userinfo, and `#EXT` tags outside `vod::VOD_TAGS` reject the playlist, and the
  viewer's client refuses redirects as well. Open segment rows follow the API's overlap rule
  (`iris_client::api::validate_segments`); drop those that recorded nothing inside the window
  (`recorded_in_window`) before deciding it is empty.
- A playback snapshot serves only the immutable bodies of its selection's
  `iris_client::playback::PlaybackSnapshot`, each at its name, on IPv4 loopback behind a 256-bit
  OS-random capability path. The names and their bodies come from `iris-client`, and so do the
  thumbnail times (`thumbnail_times`) and the export file name (`export_name`). A change of them
  belongs in `crates/iris-client/src/playback.rs` and its tests, not here. The snapshot's
  token-bearing body and capability stay out of the viewer's logs and temporary files; GStreamer's
  debug log at its verbose levels is the exception described under Pitfalls. Closing, replacing, or
  failing playback drops the snapshot listener; cancel pending loads without blocking the GTK main
  loop. Snapshots do not pin media files against retention, so missing media can fail playback. Bus
  errors defer cleanup until their callback returns; an error from a replaced player must not close
  its replacement.
- Only the visible view decodes. Any new view stops the pipelines it hides (`Tile::stop`)
  and starts what it shows (`Tile::start`); a feed task outliving its tile is a bug.
- Camera ids, stream kinds, and wall-clock times are `iris_core` types (`CameraId`,
  `StreamKind`, `WallUs`), parsed at the boundary: options, JSON answers, and the Add camera
  form. Playlist positions are `iris_client::timeline_model::PlaylistOffset` in the model and
  `gst::ClockTime` in the player. Keep wall-clock times and playlist positions apart, and convert
  between the two position types in `src/playback.rs` alone, at its two sites. The seek hands
  GStreamer the offset in nanoseconds, which hold the model's whole microseconds exactly. The
  position timer hands the model the player's nanoseconds, which the model rounds down to whole
  microseconds, as `ClockTime::useconds` does.
- Types that hold the token or a password redact it in `Debug` or have no `Debug` (the form
  values). The redacting types are `Args`; `Bearer`, `Client`, and `Feed` of `iris_transport`;
  and `NewCamera` and `StreamUrlsRequest` of `iris_client::api`.
- The root bearer never appears in URLs, logs, or window titles. Initial VOD and WebSocket
  requests send it in `Authorization`; playlist media references carry scoped capabilities
  bounded to the selected playback horizon instead. tungstenite writes the whole request of a
  WebSocket handshake, the bearer in it, at trace level. The log output layer therefore drops
  every event whose target `iris_transport::is_sensitive_log_target` names, whatever `RUST_LOG`
  says (`main::safe_log_metadata`). The guard and the test that pins its target are in
  `crates/iris-transport`, and `broad_trace_does_not_emit_websocket_authorization` checks that
  the viewer's layer applies the guard.
- Imports: the GTK and GStreamer preludes (`gtk::prelude::*`, `gst::prelude::*`) are the only
  glob imports outside tests, the exception `STANDARDS.md` §4 records; everything else is
  imported one `use` per module.

## Where things are

- `src/sound.rs` — the pads that play a camera's audio feed and its picture against one clock
  in the focused view (`docs/research/13-audio.md` §10.4). The pairing reads the pipeline's
  running time for each fragment, hands it to the `SoundClock` of `iris-client`, and sets the
  offsets the clock returns on the pad after each demuxer. `sound::silent` is true in every
  test; no test or check may play audio on the machine it runs on, so run the viewer with
  `--mute`.

- `src/main.rs` — options, GStreamer and plugin initialisation, the window with its grid
  and focused views, the main-loop event handler (`show_grid`, `focus`, `start_view`), the
  Add camera form with its Discover and Fetch streams lookups, the Remove camera
  confirmation, and the display-dependent `gtk_lifecycle_checks`.
- `crates/iris-client` — the rules the viewer takes from that crate: the API origin, the
  requests and the checks of their answers, the VOD playlist rules, the timeline model, and the
  playback selection. For live views it gives the sound placement rule, and the live protocol
  with its reconnect policy and byte budget.
- `crates/iris-transport` — the viewer's transport over that crate: the HTTP client with its one
  path for every call, the export download, and the per-tile WebSocket feed with its session loop.
  `Client::fetch` reads the part of a body that `HttpRequest::body_to_read` asks for, given the
  head.
- `src/timeline.rs` — the timeline widget and its drawing over
  `iris_client::timeline_model::TimelineModel`, and the GLib labels of a time and of the nearest
  event.
- `src/playback.rs` — the playback view over `playbin3`, with the thumbnail strip.
- `src/snapshot.rs` — the private endpoint that serves a playback snapshot's immutable bodies
  under their names, owned by a running player.
- `src/thumbs.rs` — keyframe decoding to an image, off the main loop.
- `src/stage.rs` — the shared live pipeline, its bus watch, branch add, restart, remove.
- `src/tile.rs` — the widgets, the branch lifecycle (`start`, `stop`), readiness re-armed by
  each restart, restarts, and `Source`, the `LiveSink` over a branch's `appsrc`.

## How to verify a change

```
cargo fmt -p iris-gui
cargo clippy -p iris-gui --all-targets -- -D warnings
cargo test -p iris-gui
cargo test -p iris-transport   # the transport the viewer runs on
just display-test       # the three display-dependent sets below; `just check` leaves them out
cargo test -p iris-gui --bin iris-gui -- --ignored --test-threads=1  # xvfb-run: gtk_lifecycle_checks
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

- The checks of sound that need GStreamer's plugins are `#[ignore]`d display tests: CI
  installs the development packages alone. `just display-test` runs them.

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
- Under a virtual display the sink logs "Have too many pending frames" because nothing
  consumes the paintable at the frame rate; it is not a decode failure.
- `souphttpsrc` warns "No Accept-Ranges response header" once per playback for the playlist
  fetch; the segment files do answer ranges, and the warning is harmless.
- The API serialises an event kind either as a string or as `{"other": "..."}`;
  `iris_client::api::EventKind` is untagged for that reason, and a listing with one vendor code
  in it fails to decode otherwise.
- The playback view holds its widgets in an `Rc`; button callbacks, the position timer,
  and the timeline's seek callback hold `Weak` references so dropping the view stops them.
- A dialog's button callbacks hold the dialog as a `glib::WeakRef`, and the Add camera
  form's own widgets hold the form as a `Weak`; the dialog's Add button is the form's one
  owner. A strong clone of either in a callback leaks the dialog after it closes, which
  `gtk_lifecycle_checks` detects.
- GStreamer's debug log prints the snapshot's capability URL at `GST_DEBUG=4` and the whole
  playlist, media capabilities included, at 5 and above; the viewer's own logs never do.
- Thumbnails decode in `spawn_blocking` tasks (one short pipeline each) and reach the main
  loop as RGB images; the texture is created there. A slot whose time falls in a gap stays
  empty rather than showing a neighbouring frame.
- The export runs on the tokio runtime and reports progress through a channel; GTK labels
  are only touched from the main loop. The export response has no content length, so
  progress is bytes written, not a fraction.

## Related documents

- `docs/research/06-clients.md §3`, `08-roadmap.md` (phase 3), `crates/iris-api/README.md`,
  `crates/iris-client/README.md`, `crates/iris-transport/README.md`.
