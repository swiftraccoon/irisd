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
    `--allow-insecure-http`. The rule is `iris_client::origin` (`crates/iris-client`), and the
    API client and every live feed are built from the origin it accepts.
  - `--ca-file PEM` (or `IRISD_CA_FILE`): the authorities of an `https` proxy with a private
    CA, trusted beside the platform's store by API reads and `wss` feeds alike.
  - The bearer token in `IRISD_TOKEN`, or `--token`, which other local users can read in
    process listings and the shell keeps in its history: the viewer warns about it and moves
    it into the environment when it re-executes itself to select the GL renderer. The viewer
    drops the white space around the value and takes an empty result as no token, as the daemon
    does with the token of a request. A value that then holds a character outside visible ASCII
    stops the viewer at startup, with a message that names the option and not the value
    (`iris_transport::Bearer`).
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
  camera with "Back to grid", "Playback", "Sound", and "Remove camera" buttons; and the
  playback view with "Back to grid", "Live", "Play / Pause", "Sound", "Mark in", "Mark out",
  "Export", and "Keep event" buttons, the picture, and the timeline. A click on a tile focuses its
  camera. Only the visible view decodes: leaving a view stops its pipelines and feeds.
- Playback of the main stream plays the camera's recorded audio with it, when the window's
  audio playlist (`/api/streams/{camera}/audio/hls/vod.m3u8`) exists. The viewer then checks and
  rewrites it like the video playlist and gives the player a local multivariant playlist
  that names the two (`docs/research/13-audio.md` §8). That playlist is the entry of the
  selection's `iris_client::playback::PlaybackSnapshot`. The sub stream plays without audio, whose
  timeline is the main stream's, and no fault of the audio keeps the picture away. `--mute`
  discards the audio instead of playing it on the default output, and the view's "Sound" button,
  shown while the window plays with audio and on at start, mutes the player.
- The focused live view plays the camera's sound with its picture, from the camera's audio
  feed (`/api/streams/{camera}/audio/live`); `--mute` discards it, and the grid plays none.
  Audio frames arrive in bursts, so both media play against the pipeline's clock half a
  second after their least-delay arrival (`iris_client::sound_clock::DELAY`), each feed placed by
  the least of arrival time less media time over ten seconds. The picture is held only from
  the sound's first frame on: a camera without audio answers the feed with 404, and its
  picture shows as it is decoded, as the grid's tiles do (`docs/research/13-audio.md` §10.4).
  The "Sound" button is shown for a camera the daemon lists with `has_audio`, and is on at
  start. It turns the sound and the hold off and on: the view starts over with the picture
  alone, or with both.
- The playback view loads the window's segments (`/api/streams/{camera}/{kind}/segments`)
  and events (`/api/events`) for the timeline, then fetches the HLS VOD playlist
  (`/api/streams/{camera}/{kind}/hls/vod.m3u8`) once for both its fragment timing and
  `playbin3`, one selection (`iris_client::playback::PlaybackSelection`). A private loopback
  endpoint serves that frozen selection to the player, each body of its snapshot under its name
  and nothing else. Init and media URIs resolve against the daemon's response URL and retain
  their queries, and every one must stay on the API origin (scheme, host, and port) without
  userinfo. A reference to another origin or scheme (`file:` and `data:` included), a tag the
  daemon's VOD writer does not emit (such as `EXT-X-KEY` or `EXT-X-MEDIA`), or a redirect rejects
  the playlist before GStreamer sees it. A click on the timeline seeks; the playhead follows the
  player four times a second. A refused play or pause shows in the status line.
- A strip of thumbnails above the timeline: one keyframe at the middle of each twelfth of the
  window that a segment holds (`iris_client::playback::thumbnail_times`). Each comes from
  `/api/streams/{camera}/{kind}/keyframe?t=` on the grid's stream kind (`--kind`), and a short
  pipeline on a worker thread decodes it. Across all playback views and replacements, at most two
  keyframe requests/decoders run together. Closing playback cancels queued work while any decoder
  already running keeps its slot until it exits, and generation-fences late results.
- "Keep event" in the playback view marks the event nearest the playhead (within five
  seconds) as retained for thirty days through `POST /api/events/{id}/retain`, or clears
  the mark of one already kept; retained events draw as taller white marks on the
  timeline and are named "(kept)" after a seek.
- Export from the playback view: "Mark in" and "Mark out" set the range at the playhead
  (drawn on the timeline, and cleared when another window opens), "Export" asks for a file with
  the system save dialog and streams the range's export into it, with the bytes written so
  far in the status line: `/api/cameras/{camera}/export.mp4` for the main stream, which holds
  the camera's recorded audio as a second track, and `/api/streams/{camera}/{kind}/export.mp4`
  for another stream. A range over seven days is refused in the status line before the dialog
  opens. A dismissed dialog cancels; a
  failed dialog or a location without a local path shows in the status line. The download uses
  an exclusive sibling temporary and exposes the final path only through an atomic rename after
  a complete, nonempty MP4 is synced. Before the first byte and after every further 64 MiB it
  reads the destination filesystem's free space with `fstatvfs` on a duplicate of the
  temporary's descriptor, on the blocking pool, and stops, removing the temporary, when less
  than the larger of 1 GiB and 2 % of the filesystem remains. Progress retains one
  value, the last sent, so completion cannot be overwritten by stale updates. The suggested
  file name is `<camera>-<kind>-<UTC start>.mp4` (`iris_client::playback::export_name`).
- The API client, `iris_transport::Client` (`crates/iris-transport`, the viewer's transport over
  `crates/iris-client`), makes every call of the viewer, with the root bearer in `Authorization`
  when the viewer has one. The calls are the camera list, the segment and event lists, the VOD
  playlist body and fragment timing, keyframes (`Client::keyframe`), and exports (`Client::export`,
  a streaming download with a progress callback). They are also retain marks
  (`Client::retain_event`), camera provisioning (`add_camera`, `remove_camera`), and the ONVIF
  lookups (`discover`, `stream_urls`). The daemon's media references carry exact-path capabilities
  bounded to the selected playback horizon. The client applies a five-second connect deadline and a
  thirty-second deadline for response headers and body progress. A stream lookup waits up to 70 s
  instead, the daemon's 60 s lookup bound plus a margin. The client follows no redirects and applies
  endpoint-specific byte, row, string, and time-domain limits. It accepts at most 64 discovered
  devices, each with a host of at most 253 bytes and scopes of at most 128 characters without
  control characters. It accepts one to eight profiles from a stream lookup, each with a URL that is
  `rtsp://` with a host, without userinfo or a fragment, and at most 2,048 bytes. A refusal shows
  the status and the daemon's bounded message. A sealed segment row must span a positive interval
  that overlaps the window. The API treats an open row as extending to the present, so an open row
  needs only a start no later than its recorded end and before the window's end. Open rows that
  recorded nothing inside the window (a stale file of an offline camera, or a file before its first
  sync) are dropped, and a window holding only such rows shows nothing recorded. Progressing exports
  have no short total deadline. The client's `Debug` output redacts the token. The viewer shows a
  failure as `anyhow` writes its chain, the client's message and then each source after a colon.
- The requests, the checks of their answers, and the answer types with their limits and
  validators come from `iris_client::api` (`crates/iris-client`). The client builds each call
  through the API origin's builder and sends it with reqwest. It reads the part of the body that
  `HttpRequest::body_to_read` asks for, given the answer's head. It hands the answer to the call's
  check, which decodes and validates it before the viewer uses it. A keyframe's 404, a keyframe
  or an export of another media type, and a removal's 204 are answered from the head, with no
  read of the body. An export's body streams to its file after its head passes. The Add camera
  form refuses a stream URL by the same rule as a lookup's answer (`rtsp_url_problem`).
- The playlist rules, from `iris_client::vod` (`crates/iris-client`): the `#IRIS-FRAGMENT`
  timing and the rewrite to absolute API-origin references under the tag allow-list, which the
  `vod_playlist` fuzz target covers.
- `timeline::Timeline` — the timeline widget (coverage bars, event marks, ticks, playhead,
  export marks) over `iris_client::timeline_model::TimelineModel` (`crates/iris-client`). The
  model's methods map between wall-clock times and playlist positions through the served
  fragments. They also give the bar's geometry: `time_to_x`, `time_at_x`, `span_extent` (the
  extent of a segment bar and of the export range), and `tick_xs` (the ticks). The widget keeps
  the drawing. Wall-clock times are `iris_core::time::WallUs` and playlist positions
  `iris_client::timeline_model::PlaylistOffset`, so the two domains cannot be swapped. A seek
  converts an offset to the player's `gst::ClockTime`. The position timer hands the player's
  clock time back in nanoseconds, which the model rounds down to whole microseconds. Whole
  boundary GOPs count in full; gaps within and between files are skipped. Media ticks accumulate
  before conversion to microseconds, avoiding rounding drift. Playlist downloads and resolved
  bodies are each limited to 64 MiB. `Timeline::with_model` reads the model without copying it,
  which the position timer does four times a second. `timeline::clock_label` formats a time as
  local `HH:MM:SS` through GLib, and `timeline::nearest_event_label` names the event nearest a
  time with it.
- `playback::Playback` — the playback view: `playbin3` with `gtk4paintablesink`, seeks,
  play and pause, the position timer, the export marks and the export flow. A player whose
  start fails is set to NULL before it is dropped.
- `thumbs::decode` — a keyframe resource to an RGB `Image`, and `Image::into_texture`, which
  moves the bytes into a GTK texture. Decoded dimensions are positive and at most 4,096 pixels
  per side, stride arithmetic is checked, and the copied RGB image is at most 64 MiB; only
  `decode` builds an `Image`. The decode pipeline ends in NULL whatever the outcome.
- `iris_transport::Feed` and `Feed::run` — a reconnecting WebSocket client per tile.
  It reads each message's type byte (`iris_client::live::live_message`) and pushes the daemon's byte
  stream (one init segment, then one fragment per frame) into the tile's sink. After every session,
  closed or failed, it does what the feed's `iris_client::live::Reconnect` answers: unless that is
  to stop, it asks the tile to restart its branch, and connects again after the pause. An `https`
  API gives a `wss` feed. The viewer builds one `iris-tls` configuration (the platform's store plus
  any `--ca-file` authorities), gives the client a copy, and shares it among the feeds. The viewer
  therefore works through a TLS reverse proxy for both API reads and live video. The `Debug` output
  of a `Feed` redacts the token. An audio feed that the daemon answers with 404 ends: the camera
  records no audio.
- `sound::Pairing` and `sound::Pacer` — the pads that play a camera's audio feed and its
  picture against one clock. A pacer is told of every payload its feed pushes, and reads the
  fragment's media time in 90 kHz ticks (`iris_client::live::media_time`). That is the one thing
  the viewer's Rust code reads from a live payload, and the `live_fragment` fuzz target covers
  it. The pairing hands the media time, with the pipeline's running time, to
  `iris_client::sound_clock::SoundClock`, which converts it to nanoseconds and keeps each feed's
  least delay. The pairing then sets each running-time offset the clock returns on the pad after
  the feed's demuxer. The clock returns both feeds' offsets from the sound's first fragment on.
  An offset follows its feed's estimate through the feed's first half second, and after that
  only when the estimate has moved more than 80 ms, both feeds then together. `sound::mute`
  makes the viewer discard audio.
- `stage::Stage` — one GStreamer pipeline shared by every live tile, playing for the
  viewer's lifetime; tiles are bins added to and removed from it, so they share one GL
  context and one GL thread. Its bus watch routes errors to the branch they came from;
  failed additions restore the caller's name, state, parent, and listener ownership.
- `tile::Tile` — the widgets for one camera and, between `start` and `stop`, the branch
  `appsrc ! qtdemux ! decodebin3 ! glupload ! glcolorconvert ! gtk4paintablesink` on the stage with
  the feed task that fills it. `tile::Source`, the feed's thread-safe handle on the input, is the
  tile's `iris_client::live::LiveSink`, and a payload it refuses fails the session with the text of
  the refusal. Readiness is the first buffer at the branch's sink pad after each start or restart;
  it restores the tile's label after a branch error. `Tile::start_with_sound` adds a second branch
  for the camera's audio feed, which ends in the default audio output or, muted, in a sink that
  discards it against the clock. A `tile::Source` has a single producer; it rejects empty input
  and checks queued plus incoming bytes against a 16 MiB limit
  (`iris_client::live::LIVE_PAYLOAD_BYTES`) and queued objects against 512 buffers before copying.
  WebSocket frames and assembled messages are limited to one type byte plus the 16 MiB payload:
  16 MiB + 1 bytes, the live protocol's message ceiling
  (`iris_client::live::LIVE_MESSAGE_BYTES`). The daemon honours it by skipping an oversized non-sync
  frame and closing with code 1009 (Message Too Big) when a sync frame exceeds it. Queue overflow or
  a fixed-protocol error (a text message, an empty message, or an unknown type byte) fails the feed
  and reconnects after 2 s with an init segment and sync frame. The runtime stays unblocked and no
  arbitrary byte chunks are discarded. A 1009 close or a message above the ceiling puts the tile in
  a labelled oversized state and retries after 2 s, doubling to 60 s, until the feed delivers video
  again. That is the policy of `iris_client::live::Reconnect`, whose `FeedEvent`s the tile shows.
  Playback hands `playbin3` the same `glupload ! glcolorconvert ! gtk4paintablesink` bin as its
  video sink.
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
(`RUST_LOG=iris_gui=debug,iris_transport=debug` for the feed, whose lines carry the target
`iris_transport::feed`, and `GST_DEBUG=2` for GStreamer warnings). The output layer drops every
event of the target that `iris_transport::is_sensitive_log_target` names, the client handshake of
tungstenite, even under a broad trace filter. Under that target, tungstenite writes the handshake
request with its Authorization header.

## Invariants and limits

- The viewer's Rust code never parses video. GStreamer's native elements (`qtdemux`, the parsers
  `decodebin3` inserts, the selected decoder and its driver, and `hlsdemux2` for playback) parse and
  decode the daemon's stream inside the viewer process without a sandbox. The viewer is therefore
  exposed to camera-originated media through them. The daemon's live feed is a fragmented MP4 byte
  stream (`docs/research/07-architecture.md §5`), and `qtdemux` consumes it as such, so `appsrc`
  carries no raw access units and the pipeline names no `h265parse`. `qtdemux` logs an unknown
  `prft` box per fragment at its warning level and skips it.
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
  (`TimelineModel::playlist_offset`, `iris_client::timeline_model`). The player's position maps
  back through `TimelineModel::wall_time`. Both mappings use the same frozen playlist selection
  that GStreamer receives. Recorder appends or retention between requests cannot silently change
  the selection. A snapshot does not retain media: a file removed before its byte range is
  fetched can fail playback.
- The snapshot binds only to IPv4 loopback and requires a 256-bit OS-random capability
  path; the playlist can contain authenticated media URLs. The viewer's own log output and
  temporary files never carry the endpoint or the body. GStreamer's debug log does at its
  verbose levels: `GST_DEBUG=4` (INFO) prints the snapshot URL with its capability path, and
  `GST_DEBUG=5` (DEBUG) and above dump the playlist, media capabilities included; keep the
  level at 3 or below where others can read the output. The endpoint serves the playlists alone,
  each body of the selection's `iris_client::playback::PlaybackSnapshot` under its name; media
  requests go directly to the daemon, and only to the API origin.
- The grid decodes at most `--max-tiles` live tiles (default 16), in camera order; the
  tiles after them are labelled as not started and still open their camera's single-camera
  view when clicked. Each live tile queues at most 16 MiB, so the default bounds the grid's
  queued live input at 256 MiB. Across every live tile and the focused view, the payload
  bytes alive in the viewer stay within `--live-budget-mib` (default 256 MiB), counted from the
  copy into `appsrc` until GStreamer frees the buffer. The bound is one
  `iris_client::live::LiveBudget` that every tile's source draws on. A payload the budget cannot
  admit fails its feed, which reconnects after 2 s as it does on a full queue.
- Seeks are key-unit seeks (`KEY_UNIT | FLUSH`): the player lands on the sync sample at or
  before the target, at most one fragment early.
- The phase-3 exit criterion (`docs/research/08-roadmap.md`) is six tiles plus one main view
  at or under 15 % of one core with hardware decode. Measured costs, on software decode and
  on NVDEC with the GPU path, are in `docs/research/06-clients.md §3`; on the NVIDIA
  driver the remaining cost is the driver's own GL and CUDA work, not frame conversion.

## Tests and measurements

- Unit tests (`cargo test -p iris-gui`) cover the nearest event's label, the mark status line,
  snapshot URI resolution, and private snapshot lifetime. They cover the API URL rules (scheme,
  userinfo, path, plain HTTP), the whole text of the userinfo and plain-HTTP refusals, and
  `--token` handling. They cover the option ranges and kinds, the start view for a listed or
  unlisted `--focus`, the Add camera form's validation, and the stream lookup request. They show
  a broad trace filter that keeps the WebSocket handshake's Authorization out of the logs. They
  show a pairing that sets both pads from the sound's first fragment on, once, and a source that
  tells its pacer of each payload and of a fresh byte stream. They check a source's queue and
  buffer limits, and the budget across its sources. The tests of the transport are in
  `crates/iris-transport`, and the tests of the rules the viewer takes from `iris-client` are in
  `crates/iris-client`.
- `cargo test -p iris-gui --bin iris-gui -- --ignored --test-threads=1` under `xvfb-run`
  runs `gtk_lifecycle_checks`: the playback lifecycle, the `--max-tiles` grid cap, a window
  holding only a stale open segment showing nothing recorded, the Remove button shown only for
  provisioned cameras, the Sound button starting the focused view with and without the sound's
  branch, and a closed Add camera dialog releasing itself and its form. The same command
  runs the two checks of live sound that need GStreamer's plugins: both sound branches
  building, and two feeds on timelines of their own, one sent in bursts, reaching their sinks
  half a second after capture and together. `just display-test` runs this set and the two
  below.
- `cargo test -p iris-gui --test playback_origin -- --ignored` runs the viewer on a virtual
  display against playlists that name a second loopback origin, `file:` URIs, or an
  `EXT-X-KEY` tag, and checks that the playlist is refused and the second origin receives no
  request.
- `cargo test -p iris-gui --test playback_snapshot -- --ignored` runs a bounded synthetic
  GUI regression with ffmpeg (`libx264`), Xvfb, and GStreamer HLS/H.264 plugins. Its HTTP
  fixture camera has no audio and removes the first fragment on a second VOD selection; the
  player must fetch the backend playlist once, fetch every fragment from that selection
  with authentication, and reach EOS.
- Smoke runs on a virtual display (`xvfb-run`, or `Xvfb` plus `xdotool` for clicks) against a
  running daemon show every tile's pipeline reaching Playing and decoding its first frame,
  the grid, focus, playback, and back transitions stopping and starting the right
  pipelines, timeline clicks seeking, and `--export-to` writing a file that `ffprobe` and
  `irisd inspect` read back; CPU is read from `/proc` while it runs.

Camera ids, stream kinds, and wall-clock times are the workspace's `iris-core` types. The
snapshot uses the workspace's `axum` (MIT) to serve HTTP, and `getrandom` (MIT OR Apache-2.0)
names its private capability. The transport is the workspace's `iris-transport`, whose README
lists what it takes and under what license. The tests take six more crates. The sound tests
build their frames with the workspace's `iris-mp4` and `bytes` (MIT). The snapshot and playback
tests fetch with `reqwest` (MIT OR Apache-2.0). The form's tests encode a camera to add with
`serde_json` (MIT OR Apache-2.0). The synthetic tests use `tempfile` (MIT OR Apache-2.0) to
remove generated media after the run. The test of the log filter runs a handshake with
`tokio-tungstenite` (MIT).

## Further reading

- `docs/research/06-clients.md §3` (the GUI design), `07-architecture.md §5` (the live
  feed), `crates/iris-client/README.md` (the rules the viewer takes from `iris-client`),
  `crates/iris-transport/README.md` (the viewer's HTTP client, export download, and live feed
  loop), `fuzz/README.md` (`vod_playlist`, `live_fragment`, `live_message`, `client_answers`),
  `STANDARDS.md`.
