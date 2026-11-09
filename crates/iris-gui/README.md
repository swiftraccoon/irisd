# iris-gui

The Linux viewer for irisd: a GTK 4 window with one live tile per camera, a single-camera
view of the main stream, and a playback view with a timeline over one camera's recordings.
Live views decode the daemon's WebSocket feed; playback plays the daemon's HLS VOD playlist.
The grid, focused view, timeline, event retention, keyframe thumbnails, export, and camera
provisioning implement the Linux client described in `docs/research/06-clients.md §3`.

## What it provides

- The `iris-gui` binary and its options:
  - `--api URL` (or `IRISD_API`): the daemon's origin, `http` or `https`, default
    `http://127.0.0.1:8554`. A path, query, fragment, or userinfo is refused: requests use
    absolute paths on the origin, so a path prefix would be dropped and the bearer sent to
    another path of the host. Plain HTTP to a host other than loopback needs
    `--allow-insecure-http`.
  - `--ca-file PEM` (or `IRISD_CA_FILE`): the authorities of an `https` proxy with a private
    CA, trusted beside the platform's store by API reads and `wss` feeds alike.
  - The bearer token in `IRISD_TOKEN`, or `--token`, which other local users can read in
    process listings and the shell keeps in its history: the viewer warns about it and moves
    it into the environment when it re-executes itself to select the GL renderer.
  - `--kind main|sub` (default `sub`) for the grid tiles and the playback thumbnails, and
    `--focus-kind main|sub` (default `main`) for the single-camera and playback views; any
    other kind is refused.
  - Positive `--columns N` (default 3), positive `--max-tiles N` (default 16) for the live
    tiles the grid decodes, and `--live-budget-mib MIB` (default 256, at least 16) for the
    live payload bytes alive in the viewer at once.
  - `--focus CAMERA` (a camera id) to open the single-camera view at start, `--playback` to
    open playback instead, `--window MINUTES` (default 60, 1 to 10,080, the API's seven-day
    range) for the playback window, and `--export-to FILE` to export the whole window to a
    file as soon as playback opens (for scripts and tests). A `--focus` camera the daemon does
    not list opens the grid with "unknown camera" in the status bar.
- Camera provisioning: the grid's "Add camera" button opens a form (id, name, RTSP URL, an
  optional sub URL, username, password, the streams to record, the event sources) and sends
  `POST /api/cameras`. Its Discover button lists the cameras the daemon's WS-Discovery probe
  found (`GET /api/onvif/devices`, 3 s); picking one fills an empty name and the ONVIF service
  row: the HTTPS box, the port, and "Verify the certificate" (offered with HTTPS). "Fetch
  streams via ONVIF" asks the picked camera, or the host of the typed RTSP URL, at that port and
  scheme with the entered credentials (`POST /api/onvif/streams`), fills the main and sub URLs,
  ticks the ONVIF pull-point event source, and unticks the Dahua CGI one, since the camera
  answered ONVIF. Ticking or unticking HTTPS moves a port at the other scheme's default (80 or
  443) with it, so a camera that advertises HTTPS but answers ONVIF only over HTTP is reached by
  unticking HTTPS and fetching again. The form refuses an id outside `[a-z0-9_-]{1,32}` and a
  stream URL that is not `rtsp://` with a host or that carries credentials. A problem with the
  typed values and the lookups' progress and errors show in the form, which stays open. The
  focused view's "Remove camera" button, shown only for a camera the API added (`provisioned`
  in the camera list), asks for confirmation and sends `DELETE /api/cameras/{camera}`. Adding
  and removing need `[provisioning] enabled` on the daemon and the root token; their outcome
  shows in the status bar, and the grid is rebuilt from `/api/cameras`. The dialogs' callbacks
  hold the dialog and its form weakly, so a closed dialog releases its widgets.
- Three views in a stack: the grid with its "Add camera" button; the focused live view of one
  camera with "Back to grid", "Playback", and "Remove camera" buttons; and the playback view
  with "Back to grid", "Live", "Play / Pause", "Mark in", "Mark out", "Export", and "Keep
  event" buttons, the picture, and the timeline. A click on a tile focuses its camera. Only the
  visible view decodes: leaving a view stops its pipelines and feeds.
- The playback view loads the window's segments (`/api/streams/{camera}/{kind}/segments`)
  and events (`/api/events`) for the timeline, then fetches the HLS VOD playlist
  (`/api/streams/{camera}/{kind}/hls/vod.m3u8`) once for both its fragment timing and
  `playbin3`. A private loopback endpoint serves that frozen body to the player; init and media
  URIs resolve against the daemon's response URL and retain their queries, and every one must
  stay on the API origin (scheme, host, and port) without userinfo. A reference to another
  origin or scheme (`file:` and `data:` included), a tag the daemon's VOD writer does not emit
  (such as `EXT-X-KEY` or `EXT-X-MEDIA`), or a redirect rejects the playlist before GStreamer
  sees it. A click on the timeline seeks; the playhead follows the player four times a second.
  A refused play or pause shows in the status line.
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
  (drawn on the timeline, and cleared when another window opens), "Export" asks for a file with
  the system save dialog and streams `/api/streams/{camera}/{kind}/export.mp4` for the range
  into it, with the bytes written so far in the status line. A dismissed dialog cancels; a
  failed dialog or a location without a local path shows in the status line. The download uses
  an exclusive sibling temporary and exposes the final path only through an atomic rename after
  a complete, nonempty MP4 is synced. Before the first byte and after every further 64 MiB it
  reads the destination filesystem's free space with `fstatvfs` on a duplicate of the
  temporary's descriptor, on the blocking pool, and stops, removing the temporary, when less
  than the larger of 1 GiB and 2 % of the filesystem remains. Progress retains one latest
  value, so completion cannot be overwritten by stale updates. The suggested file name is
  `<camera>-<kind>-<UTC start>.mp4`.
- `api::Client` — every call the viewer makes, with the root bearer in `Authorization`: the
  camera list, the segment and event lists, the VOD playlist body and fragment timing,
  keyframes (`Client::keyframe`), exports (`Client::export`, a streaming download with a
  progress callback), retain marks (`Client::retain_event`), camera provisioning
  (`add_camera`, `remove_camera`), and the ONVIF lookups (`discover`, `stream_urls`). The
  daemon's media references carry exact-path capabilities bounded to the selected playback
  horizon. The client applies a five-second connect deadline and a thirty-second deadline for
  response headers and body progress, except that a stream lookup waits up to 70 s, the
  daemon's 60 s lookup bound plus a margin. It follows no redirects and applies
  endpoint-specific byte, row, string, and time-domain limits: at most 64 discovered devices
  with a host of at most 253 bytes and scopes of at most 128 characters without control
  characters, and one to eight profiles whose URLs are `rtsp://` with a host, without userinfo
  or a fragment, and at most 2,048 bytes. A refusal shows the status and the daemon's bounded
  message. A sealed segment row must span a positive interval that overlaps the window. The API
  treats an open row as extending to the present, so an open row needs only a start no later
  than its recorded end and before the window's end; open rows that recorded nothing inside the
  window (a stale file of an offline camera, or a file before its first sync) are dropped, and a
  window holding only such rows shows nothing recorded. Progressing exports have no short total
  deadline. Its `Debug` output redacts the token.
- `vod` — the playlist rules: the `#IRIS-FRAGMENT` timing and the rewrite to absolute
  API-origin references under the tag allow-list. It depends on `anyhow`, `url`, and
  `iris-core` alone, and the `vod_playlist` fuzz target compiles it unchanged.
- `timeline::Timeline` and `timeline::Model` — the timeline widget (coverage bars, event
  marks, ticks, playhead, export marks) and the wall-clock to playlist-position mapping from
  the served fragments. Wall-clock times are `iris_core::time::WallUs` and playlist positions
  `gst::ClockTime`, so the two domains cannot be swapped. Whole boundary GOPs count in full;
  gaps within and between files are skipped. Media ticks accumulate before conversion to
  microseconds, avoiding rounding drift. Playlist downloads and resolved bodies are each limited
  to 64 MiB. `Timeline::with_model` reads the model without copying it, which the position
  timer does four times a second.
- `playback::Playback` — the playback view: `playbin3` with `gtk4paintablesink`, seeks,
  play and pause, the position timer, the export marks and the export flow. A player whose
  start fails is set to NULL before it is dropped.
- `thumbs::decode` — a keyframe resource to an RGB `Image`, and `Image::into_texture`, which
  moves the bytes into a GTK texture. Decoded dimensions are positive and at most 4,096 pixels
  per side, stride arithmetic is checked, and the copied RGB image is at most 64 MiB; only
  `decode` builds an `Image`. The decode pipeline ends in NULL whatever the outcome.
- `live::Feed` and `live::run` — a reconnecting WebSocket client per tile that pushes the
  daemon's byte stream (one init segment, then one fragment per frame) into the tile's
  `appsrc`, and asks the tile to restart its branch after every session, closed or failed. An
  `https` API gives a `wss` feed, verified with the shared `iris-tls` configuration (the
  platform's store plus any `--ca-file` authorities), so the viewer works through a TLS reverse
  proxy for both API reads and live video. `Feed`'s `Debug` output redacts the token.
- `stage::Stage` — one GStreamer pipeline shared by every live tile, playing for the
  viewer's lifetime; tiles are bins added to and removed from it, so they share one GL
  context and one GL thread. Its bus watch routes errors to the branch they came from;
  failed additions restore the caller's name, state, parent, and listener ownership.
- `tile::Tile` — the widgets for one camera and, between `start` and `stop`, the branch
  `appsrc ! qtdemux ! decodebin3 ! glupload ! glcolorconvert ! gtk4paintablesink` on the
  stage with the feed task that fills it; `tile::Source` is the feed's thread-safe handle
  on the input. Readiness is the first buffer at the branch's sink pad after each start or
  restart; it restores the tile's label after a branch error.
  Its single producer rejects empty input and checks queued plus incoming bytes against a
  16 MiB limit and queued objects against 512 buffers before copying. WebSocket frames and
  assembled messages are limited to one type byte plus the 16 MiB payload: 16 MiB + 1 bytes,
  the live protocol's message ceiling, which the daemon honours by skipping an oversized
  non-sync frame and closing with code 1009 (Message Too Big) when a sync frame exceeds it.
  Queue overflow or a fixed-protocol error (a text message, an empty message, or an unknown
  type byte) fails the feed and reconnects after 2 s with an init segment and sync frame; the
  runtime stays unblocked and no arbitrary byte chunks are discarded. A 1009 close or a message
  above the ceiling puts the tile in a labelled oversized state and retries after 2 s, doubling
  to 60 s, until the feed delivers video again. Playback hands `playbin3` the same
  `glupload ! glcolorconvert ! gtk4paintablesink` bin as its video sink.
- Playback's widget callbacks hold weak references to the view, so closing and dropping
  it releases its pipeline, widgets, and runtime references. Opening and automatic export run
  outside the serial UI event receiver, so navigation remains able to cancel a pending
  playlist, thumbnail, or export request and abort its snapshot listener without waiting on
  GTK's main loop.
  A playback error releases the player and snapshot after its bus callback returns.

## How it is used

    IRISD_TOKEN=... iris-gui --api https://nvr.example:8443 --kind sub --columns 3

The daemon speaks plain HTTP; for another machine, put a TLS reverse proxy (or a VPN) in
front of it and point `--api` at the proxy's `https` origin. The viewer checks the proxy's
certificate against one trust configuration (`iris-tls`) for API reads and for the `wss`
live feeds: the platform's store, plus the authorities in `--ca-file` for a proxy with a
private CA. On the daemon's own host, `--api http://127.0.0.1:8554` (the default) needs
nothing more; plain HTTP to any other host is refused unless `--allow-insecure-http` accepts
sending the token and video unencrypted.

The grid fills as `/api/cameras` answers; each tile labels itself with the camera's name and
id and shows pipeline errors in that label until video flows again. Logs go to stderr
(`RUST_LOG=iris_gui=debug` for the feed, `GST_DEBUG=2` for GStreamer warnings). The output
layer suppresses tungstenite's raw client-handshake target even under a broad trace filter
because it serializes Authorization.

## Invariants and limits

- The viewer's Rust code never parses video. GStreamer's native elements (`qtdemux`, the
  parsers `decodebin3` inserts, the selected decoder and its driver, and `hlsdemux2` for
  playback) parse and decode the daemon's stream inside the viewer process without a sandbox,
  so the viewer is exposed to camera-originated media through them. The daemon's live feed is
  a fragmented MP4 byte stream (`docs/research/07-architecture.md §5`), and `qtdemux` consumes
  it as such; the design's `appsrc` of raw access units plus `h265parse` was not needed.
  `qtdemux` logs an unknown `prft` box per fragment at its warning level and skips it.
- When the decoder produces GL memory or DMA-BUF, frames stay on the GPU: `glupload` is a no-op,
  `glcolorconvert` converts to RGBA there, and the sink hands GTK a GL texture. Software decode
  and drivers without compatible interop upload system-memory frames. The viewer re-executes
  itself once with `GSK_RENDERER=gl` when nothing set it, because GTK's Vulkan renderer, the
  default on some drivers, cannot import those textures and copies every frame through the CPU;
  set the variable yourself to choose otherwise.
- `decodebin3` picks the highest-ranked decoder installed: `vah265dec` or `nvh265dec` where
  the drivers provide them, `avdec_h265` otherwise. The pipeline renders each frame when it is
  decoded (`sync=false`); the sink keeps the most recent frame when the window cannot draw
  them all.
- The end of every feed session restarts the tile's branch (down to Null and back), so the next
  connection's init segment meets a clean demuxer. `Tile::stop` cancels the feed task and
  removes the branch; the last frame stays on screen.
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
  path; the playlist can contain authenticated media URLs. The viewer's own log output and
  temporary files never carry the endpoint or the body. GStreamer's debug log does at its
  verbose levels: `GST_DEBUG=4` (INFO) prints the snapshot URL with its capability path, and
  `GST_DEBUG=5` (DEBUG) and above dump the playlist, media capabilities included; keep the
  level at 3 or below where others can read the output. The endpoint serves the manifest alone;
  media requests go directly to the daemon, and only to the API origin.
- The grid decodes at most `--max-tiles` live tiles (default 16), in camera order; the
  tiles after them are labelled as not started and still open their camera's single-camera
  view when clicked. Each live tile queues at most 16 MiB, so the default bounds the grid's
  queued live input at 256 MiB. Across every live tile and the focused view, the payload
  bytes alive in the viewer, counted from the copy into `appsrc` until GStreamer frees the
  buffer, stay within `--live-budget-mib` (default 256 MiB): a payload the budget cannot admit
  fails its feed, which reconnects after 2 s as it does on a full queue.
- Seeks are key-unit seeks (`KEY_UNIT | FLUSH`): the player lands on the sync sample at or
  before the target, at most one fragment early.
- The phase-3 exit criterion (`docs/research/08-roadmap.md`) is six tiles plus one main view
  at or under 15 % of one core with hardware decode. Measured costs, on software decode and
  on NVDEC with the GPU path, are in `docs/research/06-clients.md §3`; on the NVIDIA
  driver the remaining cost is the driver's own GL and CUDA work, not frame conversion.

## Tests and measurements

- Unit tests: the live URL derivation (scheme switch, bearer header, redacted `Debug`), the
  playlist URL, the two serialised forms of an event kind, the playlist-time mapping across
  gaps, the nearest event lookup, the export file name, the mark status line, snapshot URI
  resolution, and private snapshot lifetime. A loopback TLS WebSocket with an `rcgen`
  certificate shows `live::session` refusing it under the platform's roots (`UnknownIssuer`)
  and streaming once its root is injected; loopback WebSockets show frames above the ceiling
  and a 1009 close ending a session as oversized, an unknown type byte or a text message
  failing it, and the feed backing off 2 s then 4 s. Further cases cover playlist references to
  other origins, schemes, userinfo, and every URI-bearing tag; refused redirects; open segment
  rows; the API URL rules (scheme, userinfo, path, plain HTTP) and `--token` handling; the
  option ranges and kinds; the start view for a listed or unlisted `--focus`; the Add camera
  form's validation and the stream lookup request; discovery and profile answers outside their
  bounds; the provisioning requests with the bearer and the daemon's refusals; a stream lookup
  outlasting the read deadline; and exports stopped by an injected low-space probe.
- `cargo test -p iris-gui --bin iris-gui -- --ignored --test-threads=1` under `xvfb-run`
  runs `gtk_lifecycle_checks`: the playback lifecycle, the `--max-tiles` grid cap, a window
  holding only a stale open segment showing nothing recorded, the Remove button shown only for
  provisioned cameras, and a closed Add camera dialog releasing itself and its form.
- `cargo test -p iris-gui --test playback_origin -- --ignored` runs the viewer on a virtual
  display against playlists that name a second loopback origin, `file:` URIs, or an
  `EXT-X-KEY` tag, and checks that the playlist is refused and the second origin receives no
  request.
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

Camera ids, stream kinds, and wall-clock times are the workspace's `iris-core` types. The
snapshot uses the workspace's `axum` (MIT) to serve HTTP, and `getrandom` (MIT OR Apache-2.0)
names its private capability and the export temporaries. Synthetic tests use `tempfile`
(MIT OR Apache-2.0) to remove generated media after the run. `wss` uses tokio-tungstenite's
`rustls-tls-native-roots` feature, whose `rustls` and `rustls-native-certs`
(Apache-2.0 OR ISC OR MIT) the lock already carries for `reqwest`; the export's `fstatvfs`
comes from the workspace's `rustix` (Apache-2.0 WITH LLVM-exception OR Apache-2.0 OR MIT);
the TLS fixture uses the workspace's `rcgen` and `tokio-rustls` (MIT OR Apache-2.0).

## Further reading

- `docs/research/06-clients.md §3` (the GUI design), `07-architecture.md §5` (the live
  feed), `fuzz/README.md` (`vod_playlist`), `STANDARDS.md`.
