# iris-gui

The Linux viewer for irisd: a GTK 4 window with one live tile per camera, a single-camera
view of the main stream, and a playback view with a timeline over one camera's recordings.
Live views decode the daemon's WebSocket feed; playback plays the daemon's HLS VOD playlist.
The grid, focused view, timeline, event retention, keyframe thumbnails, and export implement
the Linux client described in `docs/research/06-clients.md §3`.

## What it provides

- The `iris-gui` binary: `--api URL` (or `IRISD_API`), `--token` (or `IRISD_TOKEN`, which
  keeps it out of process listings), `--kind main|sub` for the grid tiles, positive
  `--columns N`,
  `--focus-kind main|sub` for the single-camera and playback views, `--focus CAMERA` to open
  the single-camera view at start, `--playback` to open playback instead,
  `--window MINUTES` (default 60) for the playback window, and `--export-to FILE` to export
  the whole window to a file as soon as playback opens (for scripts and tests).
- Three views in a stack: the grid; the focused live view of one camera with "Back to grid"
  and "Playback" buttons; and the playback view with "Back to grid", "Live", and
  "Play / Pause" buttons, the picture, and the timeline. A click on a tile focuses its
  camera. Only the visible view decodes: leaving a view stops its pipelines and feeds.
- The playback view loads the window's segments (`/api/streams/{camera}/{kind}/segments`)
  and events (`/api/events`) for the timeline, then fetches the HLS VOD playlist
  (`/api/streams/{camera}/{kind}/hls/vod.m3u8`) once for both its fragment timing and
  `playbin3`. A private loopback endpoint serves that frozen body to the player;
  init and media URIs resolve against the daemon's response URL and retain their queries.
  A click on the timeline
  seeks; the playhead follows the player four times a second.
- A strip of thumbnails above the timeline: one keyframe per twelfth of the window where
  something is recorded, from `/api/streams/{camera}/{kind}/keyframe?t=` on the grid's
  stream kind (`--kind`), decoded by a short pipeline on a worker thread. Across all playback
  views and replacements, at most two keyframe requests/decoders run together. Closing playback
  cancels queued work while any decoder already running keeps its slot until it exits, and
  generation-fences late results.
- "Keep event" in the playback view marks the event nearest the playhead (within five
  seconds) as retained for thirty days through `POST /api/events/{id}/retain`, or clears
  the mark of one already kept; retained events draw as taller white marks on the
  timeline and are named "(kept)" after a seek.
- Export from the playback view: "Mark in" and "Mark out" set the range at the playhead
  (drawn on the timeline), "Export" asks for a file with the system save dialog and streams
  `/api/streams/{camera}/{kind}/export.mp4` for the range into it, with the bytes written so
  far in the status line. The download uses an exclusive sibling temporary and exposes the
  final path only through an atomic rename after a complete, nonempty MP4 is synced. Progress
  retains one latest value, so completion cannot be overwritten by stale updates. The suggested
  file name is `<camera>-<kind>-<UTC start>.mp4`.
- `api::Client` — the reads of `/api/cameras`, the segment and event lists, and the VOD
  playlist body and fragment timing, with the root bearer in `Authorization`. The daemon's
  media references carry exact-path capabilities bounded to the selected playback horizon.
  The client applies a five-second connect deadline, a thirty-second body-idle deadline,
  endpoint byte/count/string limits, and requested-time-domain validation. Progressing exports
  have no short total deadline.
- `timeline::Timeline` and `timeline::Model` — the timeline widget (coverage bars, event
  marks, ticks, playhead) and the wall-clock to playlist-time mapping from the served
  fragments. Whole boundary GOPs count in full; gaps within and between files are skipped.
  Media ticks accumulate before conversion to microseconds, avoiding rounding drift.
  Playlist downloads and resolved bodies are each limited to 64 MiB.
- `playback::Playback` — the playback view: `playbin3` with `gtk4paintablesink`, seeks,
  play and pause, the position timer, the export marks and the export flow.
- `Client::export` — the streaming download of an export with a progress callback;
  `Client::keyframe` — the keyframe resource; `Client::retain_event` — the retain mark.
- `thumbs::decode` — a keyframe resource to an RGB `Image`, and `Image::texture` for GTK.
  Decoded dimensions are positive and at most 4,096 pixels per side, stride arithmetic is
  checked, and the copied RGB image is at most 64 MiB.
- `live::Feed` and `live::run` — a reconnecting WebSocket client per tile that pushes the
  daemon's byte stream (one init segment, then one fragment per frame) into the tile's
  `appsrc`, and asks the tile to restart its pipeline after a reconnection.
- `stage::Stage` — one GStreamer pipeline shared by every live tile, playing for the
  viewer's lifetime; tiles are bins added to and removed from it, so they share one GL
  context and one GL thread. Its bus watch routes errors to the branch they came from;
  failed additions restore the caller's name, state, parent, and listener ownership.
- `tile::Tile` — the widgets for one camera and, between `start` and `stop`, the branch
  `appsrc ! qtdemux ! decodebin3 ! glupload ! glcolorconvert ! gtk4paintablesink` on the
  stage with the feed task that fills it; `tile::Source` is the feed's thread-safe handle
  on the input. Readiness is the first buffer at the branch's sink pad.
  Its single producer rejects empty input and checks queued plus incoming bytes against a
  16 MiB limit and queued objects against 512 buffers before copying. WebSocket frames and
  assembled messages are limited to one type byte plus the 16 MiB payload. Overflow or a
  fixed-protocol type error fails the feed and reconnects with an init segment and sync frame;
  the runtime stays unblocked and no arbitrary byte chunks are discarded.
  Playback hands `playbin3` the same `glupload ! glcolorconvert ! gtk4paintablesink` bin
  as its video sink.
- Playback's widget callbacks hold weak references to the view, so closing and dropping
  it releases its pipeline, widgets, and runtime references. Opening and automatic export run
  outside the serial UI event receiver, so navigation remains able to cancel a pending
  playlist, thumbnail, or export request and abort its snapshot listener without waiting on
  GTK's main loop.
  A playback error releases the player and snapshot after its bus callback returns.

## How it is used

    IRISD_TOKEN=... iris-gui --api http://192.0.2.10:8554 --kind sub --columns 3

The grid fills as `/api/cameras` answers; each tile labels itself with the camera's name and
id and shows pipeline errors in that label. Logs go to stderr (`RUST_LOG=iris_gui=debug`
for the feed, `GST_DEBUG=2` for GStreamer warnings). The output layer suppresses tungstenite's
raw client-handshake target even under a broad trace filter because it serializes Authorization.

## Invariants and limits

- The viewer never parses video. The daemon's live feed is a fragmented MP4 byte stream
  (`docs/research/07-architecture.md §5`), and `qtdemux` consumes it as such; the design's
  `appsrc` of raw access units plus `h265parse` was not needed. `qtdemux` logs an unknown
  `prft` box per fragment at its warning level and skips it.
- When the decoder produces GL memory or DMA-BUF, frames stay on the GPU: `glupload` is a no-op,
  `glcolorconvert` converts to RGBA there, and the sink hands GTK a GL texture. Software decode
  and drivers without compatible interop upload system-memory frames. The viewer re-executes itself once
  with `GSK_RENDERER=gl` when nothing set it, because GTK's Vulkan renderer, the default on
  some drivers, cannot import those textures and copies every frame through the CPU; set
  the variable yourself to choose otherwise.
- `decodebin3` picks the highest-ranked decoder installed: `vah265dec` or `nvh265dec` where
  the drivers provide them, `avdec_h265` otherwise. The pipeline renders each frame when it is
  decoded (`sync=false`); the sink keeps the most recent frame when the window cannot draw
  them all.
- A reconnection restarts the tile's branch (down to Null and back) so the new init
  segment starts a clean demuxer. `Tile::stop` cancels the feed task and removes the
  branch; the last frame stays on screen.
- Branch sinks run `async=false`: a branch added to the playing stage has a live source
  that pushes nothing until the branch plays, so a sink waiting to preroll would hold it
  in paused forever.
- Playlist time versus wall time: the VOD playlist for `[from, to)` holds the window's
  selected fragments back to back, so a wall-clock time maps to the recorded duration before it
  (`Model::playlist_offset`) and the player's position maps back through `Model::wall_time`.
  Both mappings use the same frozen playlist selection that GStreamer receives. Recorder
  appends or retention between requests cannot silently change the selection. A snapshot
  does not retain media: a file removed before its byte range is fetched can fail playback.
- The snapshot binds only to IPv4 loopback and requires a 256-bit OS-random capability
  path; the playlist can contain authenticated media URLs. The endpoint and body never
  enter viewer logs or temporary files. It serves the manifest alone; media requests go
  directly to the daemon.
- Seeks are key-unit seeks (`KEY_UNIT | FLUSH`): the player lands on the sync sample at or
  before the target, at most one fragment early.
- The phase-3 exit criterion (`docs/research/08-roadmap.md`) is six tiles plus one main view
  at or under 15 % of one core with hardware decode. Measured costs, on software decode and
  on NVDEC with the GPU path, are in `docs/research/06-clients.md §3`; on the NVIDIA
  driver the remaining cost is the driver's own GL and CUDA work, not frame conversion.

## Tests and measurements

- Unit tests: the live URL derivation (scheme switch, bearer header), the playlist URL, the
  two serialised forms of an event kind, the playlist-time mapping across gaps, the nearest
  event lookup, the export file name, snapshot URI resolution, and private snapshot lifetime.
- `cargo test -p iris-gui --test playback_snapshot -- --ignored` runs a bounded synthetic
  GUI regression with ffmpeg (`libx264`), Xvfb, and GStreamer HLS/H.264 plugins. Its HTTP
  fixture removes the first fragment on a second VOD selection; the player must fetch the
  backend playlist once, fetch every fragment from that selection with authentication,
  and reach EOS.
- Smoke runs on a virtual display (`xvfb-run`, or `Xvfb` plus `xdotool` for clicks) against a
  running daemon show every tile's pipeline reaching Playing and decoding its first frame,
  the grid, focus, playback, and back transitions stopping and starting the right
  pipelines, timeline clicks seeking, and `--export-to` writing a file that `ffprobe` and
  `irisd inspect` read back; CPU is read from `/proc` while it runs.

The snapshot uses the workspace's `axum` (MIT) to serve HTTP and `getrandom`
(MIT OR Apache-2.0) for its private capability. Synthetic tests use `tempfile`
(MIT OR Apache-2.0) to remove generated media after the run.

## Further reading

- `docs/research/06-clients.md §3` (the GUI design), `07-architecture.md §5` (the live
  feed), `STANDARDS.md`.
