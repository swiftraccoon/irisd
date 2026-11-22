# iris-client

The rules a viewer of irisd applies to the daemon's API origin and to what the daemon serves.
The crate has the API's requests and the checks of their answers, the VOD playlist rules, the
timeline of a recorded window, and the playlists a player is handed. For live feeds, it has the
protocol with its reconnect policy and byte budget, the live decoder, and the rule that places
sound against picture.

The Linux viewer (`crates/iris-gui`) uses the crate, with its transport in
`crates/iris-transport`. The crate is shaped for the planned iOS and macOS viewer that
`docs/research/15-apple-clients.md` designs. It does no input or output: each function takes
what its caller was given or fetched and returns a value or a refusal. A viewer keeps its
transport and its bearer, and the crate depends on no toolkit and builds no C code.

## What it provides

The rustdoc of each item states its contract. Each type and function has an example, or names
the example that shows its use.

- `origin` — the API origin a viewer builds its requests and live feeds from.
  `ApiOrigin::parse(text, plain_http)` and `ApiOrigin::try_new(url, plain_http)` accept an `http`
  or `https` URL with a host and the path `/` alone. A `PlainHttp` policy names the hosts an
  `http` origin may be at: `LoopbackOnly`, `PrivateAddresses` (the loopback, and the IPv4 ranges
  of RFC 1918 and RFC 6598), or `Anywhere`. `live_url(camera, kind)` gives a stream's live
  WebSocket URL, and a refusal is an `OriginError`.
- `api` — the daemon's HTTP API as a viewer uses it, without a transport:
  - A method of `ApiOrigin` builds each call's `HttpRequest`: `cameras`, `add_camera`,
    `remove_camera`, `segments`, `events`, `export`, `retain_event`, `keyframe`, `vod_playlist`,
    `discover`, and `stream_urls`. A request gives its `Call`, its `Method`, its URL, the JSON
    body of a call that sends one, and the call's byte limit. Its read deadline is `READ_IDLE`
    (30 s), or `STREAM_LOOKUP_READ_IDLE` (70 s) for a stream lookup. A call that names a time
    range takes a `Window`, `[from, to)`, of at most seven days.
  - `HttpRequest::body_to_read(status, content_type)` says, from an answer's head, what a
    transport reads of its body. It gives a `BodyRead`: `Nothing`, `Message` (the daemon's
    message after a refused status, up to 16 KiB), or `Value` (up to the call's limit).
  - `answer` — the check of each call's answer, named as its builder. A check takes the `Answer`
    a transport received and returns the call's value or an `ApiError`.
  - The answer types are `Camera`, `Segment`, `EventRecord` with `EventId` and `EventKind`,
    `DiscoveredDevice`, `ProfileStream`, and `VodPlaylist`. The request bodies are `NewCamera`
    and `StreamUrlsRequest`. A `Retention` is what a retain mark does to an event's footage:
    keep it `Until` a time, or `Release` it.
  - The validators an answer passes: `validate_cameras`, `validate_segments` with
    `recorded_in_window`, `validate_events`, `validate_event`, `validate_playback_fragments`,
    `validate_devices`, and `validate_streams`. The text rules are `is_valid_display_string`,
    `is_valid_camera_text`, and `is_valid_device_host`, and `rtsp_url_problem(url)` gives the
    `StreamUrlProblem` of a camera's stream URL.
  - The bounds of each answer and request: the `MAX_*` constants and `DISCOVERY_WAIT_MS`.
  - `ApiError` — each refusal, with the text a viewer shows. `Call` names a request as a
    viewer's messages do ("listing the cameras"), and `Rule` is the rule that an answer or a
    request broke.
- `vod` — the VOD playlist rules. `playlist_fragments(text)` gives the `#IRIS-FRAGMENT` timing
  of a body in served order, as `PlaybackFragment` values.
  `absolute_playlist(text, base, origin)` rewrites the body for a player, with each reference an
  absolute URL on the API origin. `multivariant_playlist(video, audio)` gives the playlist that
  plays a video body with an audio body as its one rendition. It names the two bodies
  `VIDEO_PLAYLIST` and `AUDIO_PLAYLIST`. A refusal is a `PlaylistError`, and a refused reference
  carries a `ReferenceError` as its source. `VOD_TAGS` lists the tags the daemon's VOD writer
  emits.
- `timeline_model` — `TimelineModel`, what a timeline draws and maps through: the window, its
  segments, the playlist's fragments, its events, the playhead, and the export marks. It maps a
  wall-clock time to a `PlaylistOffset` and back, finds the event nearest a time, and places
  times, spans (`SpanExtent`), and ticks on a bar of a given width.
- `playback` — a recorded window as a viewer plays it. `PlaybackSelection::new(video, audio)`
  takes the window's checked playlists. `into_parts()` gives the timing a timeline maps through
  and the `PlaybackSnapshot`, the bodies a player is handed, each under its name.
  `thumbnail_times(window, segments, slots)` gives a `ThumbnailTime` for each slot of a strip of
  thumbnails that shows a recording. `export_name(camera, kind, from)` gives an export's file
  name: the camera, the kind, and the start in UTC to the second, as in
  `cam1-main-20261101T000000Z.mp4`.
- `live` — a live feed as a viewer reads it, without a transport:
  - `live_message(message)` reads a message's type byte and gives a `LiveMessage`: `Init` after
    `MSG_INIT`, or `Fragment` after `MSG_FRAGMENT`, with the payload. `LIVE_PAYLOAD_BYTES`
    (16 MiB) bounds a payload and `LIVE_MESSAGE_BYTES` a message.
  - `media_time(payload)` gives the media time of the `prft` box a live fragment starts with, in
    the box's 90 kHz ticks. The box must be the 32 bytes of a version-1 `prft`, as the daemon
    writes it, and ticks above `i64::MAX` give none.
  - `LiveSink` — where a feed puts what it receives. The feed calls `reset` as each session
    opens and `push` with each message. A sink that cannot take a message refuses it with a
    `SinkError`.
  - `LiveDecoder` — the feed read message by message, for a viewer that hands each sample to a
    decoder. `docs/research/15-apple-clients.md` §4 designs the planned iOS and macOS viewer
    that way. `decode(message)` gives a `LiveItem`: the `Config` of an init segment, an
    `iris_mp4::decoder_config::DecoderConfig`, or the one `Sample` of a fragment, a `LiveSample`
    placed in the message by its offset and length. The Linux viewer pushes its messages into
    GStreamer's demuxer and does not use the decoder.
  - `Reconnect` — when a feed connects again. `after(end)` takes a `SessionEnd` and answers
    with `Next`, and `FeedEvent` is what a feed tells its view.
  - `LiveBudget` — the bound on the live payload bytes alive in a viewer at once, across all
    its sinks. `reserve(bytes)` gives a `Reservation`, which returns its bytes when dropped.
  - `LiveError` — each refusal of the framing and of the decoder.
- `sound_clock` — `SoundClock`, the placement of one camera's picture and sound on one clock
  (`docs/research/13-audio.md` §10.4). `fragment(feed, media, running)` notes a fragment of
  either `Medium` and returns a `Placement` when offsets are to be put in force: the offset of
  each feed, and the `Engagement` of the placement. `DELAY` (500 ms) is how long after its
  least-delay arrival a frame plays.

## How it is used

A recorded window, from the request to the timeline and the player:

```rust
use std::error::Error;
use std::time::Duration;

use iris_client::api::{Answer, BodyRead, Window, answer};
use iris_client::origin::{ApiOrigin, PlainHttp};
use iris_client::playback::{PlaybackSelection, PlaybackSnapshot};
use iris_client::timeline_model::TimelineModel;
use iris_core::id::{CameraId, StreamKind};
use iris_core::time::WallUs;

fn main() -> Result<(), Box<dyn Error>> {
    let origin = ApiOrigin::parse("https://nvr.example:8443", PlainHttp::LoopbackOnly)?;
    let window = Window::try_new(WallUs::new(0), WallUs::new(60_000_000))?;
    let request = origin.vod_playlist(&CameraId::try_new("cam1")?, StreamKind::Sub, window);
    // A transport sends the request, and reads of the body what the head calls for.
    let content_type = Some("application/vnd.apple.mpegurl");
    let max_bytes = request.max_body_bytes();
    let read = request.body_to_read(200, content_type);
    assert_eq!(read, BodyRead::Value { max_bytes });
    // The daemon answers with two seconds recorded from 10 s and one second from 30 s.
    let body = "#EXTM3U\n\
                #IRIS-FRAGMENT:WALL-US=10000000,DURATION-TICKS=180000\n\
                #EXTINF:2.00000,\n/api/segments/1/file.mp4\n\
                #IRIS-FRAGMENT:WALL-US=30000000,DURATION-TICKS=90000\n\
                #EXTINF:1.00000,\n/api/segments/2/file.mp4\n#EXT-X-ENDLIST\n";
    let received = Answer {
        status: 200,
        content_type,
        final_url: request.url(),
        body: Some(body.as_bytes()),
    };
    let video = answer::vod_playlist(&origin, &received, window)?;
    // The timeline and the player each take their part of the one checked playlist.
    let (playback, snapshot) = PlaybackSelection::new(video, None).into_parts();
    let model = TimelineModel {
        from: window.from(),
        to: window.to(),
        playback,
        ..TimelineModel::new()
    };
    // A click at 30.5 s seeks 2.5 s into the playlist, which plays no gap.
    let offset = model.playlist_offset(WallUs::new(30_500_000));
    assert_eq!(offset.get(), Duration::from_millis(2_500));
    assert_eq!(model.wall_time(offset), WallUs::new(30_500_000));
    let entry = snapshot.body(PlaybackSnapshot::ENTRY).ok_or("the entry")?;
    assert!(entry.contains("\nhttps://nvr.example:8443/api/segments/2/file.mp4\n"));
    Ok(())
}
```

The Linux viewer sends each request through `iris-transport` with the bearer. It draws its
timeline widget over the model, and serves each body of the snapshot under its name on a
loopback endpoint, whose entry URL it gives GStreamer.

A viewer reads the media time of each fragment with `live::media_time` and hands it to
`SoundClock::fragment` with the running time of its arrival; the example of each item shows its
step. The Linux viewer reads the pipeline's running time as each payload is pushed. It sets each
offset the clock returns on the pad after the feed's demuxer, which counts the feed's running
time from its first fragment.

The feed loop of `iris-transport` reads each WebSocket message through `live_message` and pushes
it into the tile's `Source`, which implements `LiveSink` and draws on the viewer's
`LiveBudget`. After each session, unless no view is left to receive its events, it tells the
feed's `Reconnect` how the session ended.

## Invariants and limits

- Nothing in the crate panics on its input. The input of `vod`, `live`, and `api::answer` comes
  from the network, and the fuzz targets `vod_playlist`, `live_fragment`, `live_message`,
  `live_sample`, and `client_answers` run them on arbitrary bytes. A validator bounds the
  length of a list before it sizes a set by it.
- A check runs in one order, and the first rule an answer breaks is its refusal. The order is
  the origin, the status, the content type, the body's bound, decoding, the validator, then the
  rules of that call alone. A refused status comes before any rule of the body.
- A call expects any `2xx`, but 201 alone for an added camera, 204 alone for a removal, and 200
  alone for an export. A keyframe's 404 says that nothing is recorded, and its check answers
  `None`. `body_to_read` and the checks decide by the same rules of the call, so a check never
  looks at a body that `body_to_read` left unread. The checks of an export and of a removal
  take no body, so `max_body_bytes()` is zero for both.
- The text of a refused status is `{call} failed with {status}`, and `: {message}` follows when
  the body holds the daemon's message. The message is the string field `error` of a JSON body
  of at most `MAX_ERROR_BODY_BYTES`, each control character a space, cut to
  `MAX_ERROR_MESSAGE_CHARS` characters. The status shows as the `http` crate writes it
  (`409 Conflict`, `599 <unknown status code>`), and a value outside 100 to 999 as its number.
- Where the daemon has a limit, the constant has its value, and its rustdoc names the daemon's
  constant. The exception is the body of a VOD playlist: the viewer sets
  `vod::MAX_VOD_PLAYLIST_BYTES` (64 MiB) above the daemon's 16 MiB. Three builders refuse a
  request that breaks a rule, and nothing is sent. `events` refuses a page limit outside 1 to
  10,000, which the daemon would clamp to that range. `discover` refuses a wait outside 500 to
  10,000 ms, which the daemon would answer with 400. `add_camera` refuses a stream URL that
  `rtsp_url_problem` finds a problem with.
- The bearer never enters the crate. The `Debug` output of a `NewCamera` and of a
  `StreamUrlsRequest` redacts the password. The `Debug` output of a request, an answer, a
  playlist, a selection, a snapshot, and a live message shows the length of each body, never its
  bytes. The body of a request carries that password, and the references of a playlist carry
  media capabilities. The `Debug` output of a `LiveItem::Config` shows the parameter sets or the
  `AudioSpecificConfig` of a stream, its public configuration. A URL shows with its userinfo,
  its query, and its fragment each as `<redacted>`, since a camera may take its password in its
  query. A camera's stream URL that `rtsp_url_problem` refuses shows as `<redacted>` whole. A
  secret written into a URL's path shows.
- The text of an `ApiError`, a `PlaylistError`, an `OriginError`, and a `LiveError` is the
  message a viewer shows or logs. A caller that branches on a refusal matches the variant.
- A playlist's body and its rewrite are each at most `MAX_VOD_PLAYLIST_BYTES`, and its timing
  holds one to `MAX_PLAYBACK_FRAGMENTS` (100,000) fragments. Every init and media reference must
  resolve onto the API origin (scheme, host, and effective port) without userinfo; opaque
  origins such as `file:` and `data:` never match. A line that starts with `#EXT` in any letter
  case and names no tag of `VOD_TAGS` refuses the body. RFC 8216 §4.1 makes tags case
  sensitive; the check ignores case so that a player that reads tags in any case meets none
  outside `VOD_TAGS`. A carriage return anywhere but before a line feed refuses the body too.
- An origin's checks run in one order: the scheme, userinfo, the path, query, and fragment, then
  plain HTTP under the policy. `PrivateAddresses` admits no IPv6 address but `::1`
  and no name but `localhost`. The text of an `OriginError` holds no advice of a viewer, which
  may add its advice after it.
- The timeline maps through the fragments in served order. Their 90 kHz ticks accumulate before
  each conversion to microseconds, so a long window does not drift by a rounding per fragment.
  `playlist_offset` gives whole microseconds, and `wall_time` rounds a position down to whole
  microseconds. The arithmetic runs in wider integers or saturates.
- The geometry divides by a window of at least one microsecond, so a reversed or empty window
  draws without a division by zero. Any window, the whole range of `WallUs` included, draws at
  most `MAX_TIMELINE_TICKS` (4,096) ticks.
- A selection holds the bodies of one fetch of each playlist, so its timeline and its player
  use the same selection. Its snapshot holds one body, `vod.m3u8`, or with audio three: the
  multivariant entry, and `video.m3u8` and `audio.m3u8` beside it. `body` matches a name whole
  and in its letter case.
- A thumbnail's time is the middle of its slot's part of the window, in whole microseconds
  rounded down. A slot is kept when a segment holds that time, from the segment's start up to
  and not including its end.
- A frame of a live feed plays at its media time less that of the feed's first fragment, plus
  the feed's offset. The offset is the media time of that first fragment, plus the feed's least
  delay, plus half a second. The least delay is the smallest value of arrival time less media
  time over the last ten seconds. The offset follows the least delay through the feed's first
  half second. After that it moves only when the least delay differs by more than 80 ms from its
  value when the offset was set. The offsets of both feeds then move together.
- Nothing is placed before the sound's first fragment, so a camera without sound never holds
  its picture. A media time below zero, or a media or running time past `i64::MAX` nanoseconds
  (830,103,483,316,930 ticks and above), places and notes nothing. The arithmetic after that
  saturates, and offsets compare exactly however far apart they are.
- A live message is a type byte and a payload: `0x01` before an init segment and `0x02` before a
  fragment. `LIVE_MESSAGE_BYTES` is `LIVE_PAYLOAD_BYTES` and the type byte, the value of the
  daemon's `MAX_LIVE_MESSAGE_BYTES`. A payload at the ceiling therefore fits a sink that queues
  `LIVE_PAYLOAD_BYTES`; raising either raises the memory every feed may hold.
- A `LiveSample` lies inside the message it was read from, past the type byte:
  `offset() + len()` is at most the message's length and computes in `u32`. A fragment for
  which that would not hold is refused, never sliced, and so is one whose sample is empty. The
  decoder reads with the daemon's readers, `DecoderConfig::parse` and
  `iris_mp4::reader::parse_fragment`, so it takes no fragment the daemon's scanner would refuse.
- A sample comes only while the decoder has a configuration, and the first after each
  configuration or refused fragment is a sync sample. A refused init segment leaves the decoder
  without a configuration, and a message that `live_message` refuses leaves it as it was.
- `Reconnect` answers 2 s after any ending but two. After an oversized frame the pause starts at
  2 s and doubles to 60 s (2, 4, 8, 16, 32, 60, 60 s) until a session delivers a fragment. An
  audio feed answered 404 stops, since the camera records no audio; a video feed answered 404
  retries. A feed calls `delivered()` before `after()` for the same session.
- A `LiveBudget` admits a reservation only while the sum of the bytes reserved stays at or below
  its limit, and refuses one whose sum overflows; a refusal reserves nothing. It counts with one
  compare-and-swap loop and no lock, and a dropped reservation returns its bytes on any thread.
- The crate sends and receives nothing: `cargo tree -p iris-client -e normal` lists no reqwest,
  tokio, rustls, or aws-lc. With `-e normal,build,dev` it lists no `cc` and no `-sys` crate, so
  nothing the crate or its tests build compiles C. The `libc` crate, which `getrandom` brings
  through `iris-core`, declares the platform's C library and builds none of it.
  `cargo check --target aarch64-apple-ios -p iris-client` passes.
- The crate takes `http` (MIT OR Apache-2.0) for the reason phrase of a status and for the
  method a `Method` converts into, and the workspace's `iris-mp4` for the daemon's fragment
  reader and decoder configuration. Beyond `iris-core` and `thiserror`, `iris-mp4` brings
  `bytes`, `generic-array`, and `spin` (MIT), and `crc-fast`, `digest`, `crypto-common`, and
  `typenum` (MIT OR Apache-2.0), all Rust. `iris-transport` brings reqwest, tokio, and rustls,
  and with rustls a cryptography library built from C (`crates/iris-transport/README.md`).

## Tests and measurements

`cargo test -p iris-client` runs every test of the crate. None needs a network, a display, or a
camera.

- Unit tests, in the `mod tests` of each source file, one behaviour to a test. Where a rule has
  a bound, they check the rule at the bound and one past it. They check each refusal by its
  variant and by its text, because a viewer shows the text.
- The tests of `api` check each builder against the route table of `crates/iris-api/README.md`.
  For every call, they check that `body_to_read` and the call's check give one verdict on an
  answer's head, over statuses and content types at the edges of the rules.
- The tests of `live::LiveDecoder` run over messages framed as the daemon frames them, built
  with the init segment and fragment encoders of `iris-mp4`. Their `mp4a` entry holds no
  `SLConfigDescriptor`, which the daemon's does; the tests of `iris-mp4` read the daemon's
  layout.
- Doctests: every rustdoc example, and the example of this document, which `src/lib.rs` includes
  under `cfg(doctest)`. `cargo test -p iris-client --doc` runs them alone.
- `just cross-test` runs the tests as 64-bit ARM code (`aarch64-unknown-linux-musl`), on a
  64-bit ARM host or under `qemu-aarch64-static`.
- The fuzz targets `vod_playlist`, `live_fragment`, `live_message`, `live_sample`, and
  `client_answers` (`fuzz/README.md`) depend on the crate; `just fuzz-build` builds them.
- `just mutants FILE` runs mutation testing on a source file; `crates/iris-client/AGENTS.md`
  lists the files in which no mutant is missed.
- On 2026-11-16 the decoder read the live feeds of the test daemon, H.265 main and sub streams
  and AAC-LC audio, for 12 s each. It refused no message, and ffmpeg 9.0.2 decoded every sample
  it placed (`docs/research/15-apple-clients.md` §3.6). The scratch program that ran it is not
  in the tree.

## Further reading

- `docs/research/15-apple-clients.md` §4 (plain HTTP and the transport), §5, and §11 (what is
  built)
- `docs/research/13-audio.md` §8 (the multivariant playlist) and §10.4 (sound against picture)
- `crates/iris-api/README.md` (the routes the builders name, and the live WebSocket's messages
  and its 1009 close)
- `crates/iris-transport/README.md` (the Linux viewer's transport over the crate)
- `crates/iris-gui/README.md` (the Linux viewer)
- `fuzz/README.md` (the five fuzz targets over the crate)
