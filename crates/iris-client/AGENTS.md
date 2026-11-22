# AGENTS.md — iris-client

Scope: this crate only. Read the root `AGENTS.md` first. The Linux viewer uses this crate, and
the crate is shaped for the planned iOS and macOS viewer that
`docs/research/15-apple-clients.md` designs. It does no input or output and depends on no
toolkit; keep it that way. This crate builds what a viewer sends; the viewer fetches; this crate
decides what the viewer may do with what it fetched. The Linux viewer fetches through
`crates/iris-transport`.

## Invariants

`README.md` states the crate's invariants, and the rustdoc of each item states its contract.
These rules say what a change must keep and what else it moves.

- The text of every refusal is a message a viewer shows or logs: `ApiError` with `Call` and
  `Rule`, `StreamUrlProblem`, `OriginError`, `PlaylistError` with `ReferenceError`, and
  `LiveError`. Keep each text byte for byte and `git grep` it before a change. In this crate,
  `each_rule_has_the_text_a_viewer_shows`, `each_decoder_refusal_has_its_text`, and
  `a_broken_messages_text_is_the_one_the_viewer_logs` pin the text of every `Rule` and of each
  refusal of the decoder. Tests of two other crates match texts:
  - the transport's tests in `crates/iris-transport/src/client.rs` match parts of a refused
    status: "409" with "already exists" or "configuration file", "502" with "could not be
    queried", and "302";
  - those tests match the whole of "response is not video/mp4" and of the `Call` text "listing
    the cameras";
  - the transport's export tests (`crates/iris-transport/src/export.rs`) match the whole of
    "invalid time range", "response is not video/mp4", and "requesting the export failed with
    206 Partial Content";
  - the transport's `an_unknown_message_type_fails_the_session`
    (`crates/iris-transport/src/feed.rs`) matches "unknown live message type";
  - the viewer's `the_add_camera_form_names_its_first_problem` (`crates/iris-gui/src/main.rs`)
    matches three texts of `rtsp_url_problem` after "the RTSP URL" and "the sub URL", the
    sentences in which `ApiOrigin::add_camera` refuses a stream URL;
  - the viewer's tests match part of the text of `OriginError::NotAnOrigin`, and the whole of
    the texts of `Userinfo` and `PlainHttp` with the viewer's hint after each;
  - the viewer's `playback_origin` display test finds a `PlaylistError` text and a
    `ReferenceError` text in the viewer's log.
- A viewer prints a refusal and then each source (`{:#}` of an `anyhow::Error`), so no text
  repeats its source. `ApiError::Invalid`, `ApiError::Playlist`, `LiveError::Config`, and
  `ReferenceError::Unresolved` are `#[error(transparent)]`. A refused reference keeps its reason
  as the `#[source]` of its variant and out of the text.
- No text of `OriginError` names a viewer's flags or variables, so the text of `PlainHttp` ends
  at "unencrypted". The Linux viewer appends its hint to the texts of `Userinfo` and `PlainHttp`
  (`api_origin` in `crates/iris-gui/src/main.rs`).
- A status prints through `http::StatusCode`, the type the Linux viewer's HTTP client prints, so
  a reason phrase changes only with that crate. The daemon's message comes only from
  `refusal_detail` (`src/api/answer.rs`).
- A call's rules are written once, in `src/api/request.rs`: `Call::expects` (the statuses),
  `Call::media_type`, `Call::max_body_bytes`, and `Call::head`, which sorts an answer's head
  into a `Head`. `HttpRequest::body_to_read` and the head checks of `src/api/answer.rs` both
  follow `Call::head`, so a check never needs a body that `body_to_read` left unread. The test
  `a_check_reads_only_the_body_that_body_to_read_reads` checks that for every call. Keep the
  order of a check too: a viewer shows the first refusal, and the transport's fixture tests
  match the texts of refusals that come before any body.
- Each validator refuses with `ApiError::Invalid` and the `Rule` broken, one variant for each
  rule, and so does each builder for a rule the request breaks. A request body always encodes, so
  `stream_urls` returns its request. A builder returns a `Result` only where it can refuse. No
  refusal of a builder's rule reaches the Linux viewer. Its form checks a stream URL with
  `rtsp_url_problem` before the builder sees it. Its discovery waits a fixed 3 s, and its event
  page limit is a fixed 2,000.
- The answer types and the two request bodies are data-transfer types. Each derives serde's
  `Serialize` and `Deserialize` and decodes from the JSON it encodes to. `EventKind` is
  `#[serde(untagged)]`, because the API writes a kind as a bare string or as `{"other": "..."}`.
  Without the attribute neither form decodes, and an event listing fails. `VodPlaylist` derives
  neither: `answer::vod_playlist` alone makes one (`VodPlaylist::new` is `pub(crate)`), and
  `PlaybackSelection` keeps its fragments private. Keep both, so that a selection's timing and
  bodies are always those of checked playlists. A caller outside the crate, the Linux viewer's
  snapshot tests among them, makes a playlist through the check.
- A bound of `src/api/limits.rs` has the daemon's value where the daemon has a limit, and its
  doc names the daemon's constant: a change of a limit on either side changes the other.
  `LIVE_MESSAGE_BYTES` has the value of the daemon's `MAX_LIVE_MESSAGE_BYTES`
  (`crates/iris-api/src/live.rs`), a separate literal: change both together. The tests of
  `crates/iris-api/src/client_bounds.rs` compare each of these bounds, the type bytes of a live
  message, and the two bounds of `src/vod.rs` with the daemon's constant. They fail when one
  side changes alone. The Linux viewer queues at most `LIVE_PAYLOAD_BYTES` in a source, and its
  transport's WebSocket refuses a longer message, so a change of either constant moves the
  viewer's limits.
- Every `Debug` output prints a URL through `types::shown_url`, or through `shown_stream_url`
  for a camera's stream URL. The `Debug` output of `HttpRequest`, `Answer`, `VodPlaylist`,
  `PlaybackSelection`, `PlaybackSnapshot`, and `LiveMessage` prints the length of a body, never
  its bytes. That of `LiveItem::Config` prints the parameter sets or the `AudioSpecificConfig`
  of a stream, its public configuration. A secret written into a path prints; keep that the one
  part that does. A binding that exposes these types
  prints no more: a generated Swift struct prints every field
  (`docs/research/15-apple-clients.md` §3.5).
- `origin::PlainHttp` decides where an `http` origin may be. `LoopbackOnly` is the Linux
  viewer's default and `Anywhere` its `--allow-insecure-http`. `PrivateAddresses` admits the
  ranges that `docs/research/15-apple-clients.md` §4 gives the planned viewer. Widening a policy
  sends a bearer and video unencrypted to more hosts.
- The tests of `PlainHttp` (`src/origin.rs`) name addresses in the ranges of RFC 1918 and
  RFC 6598, which root `AGENTS.md` rule 9 keeps out of committed files otherwise. The policy
  decides on those ranges, so the tests name the first and the last address of each range and
  the address on either side of it. For the shared range they also name its first address with
  the first octet on either side. They never name an address inside a range, which a reader
  could take for a host of a network. Every other address the crate's tests name is a loopback
  address, an example address of RFC 5737 or RFC 3849, or the first or the last address of a
  range a test refuses.
- `TimelineModel` gives every mapping between a time and an x coordinate that the Linux viewer
  draws by. The viewer keeps the drawing: a bar's least width of one pixel, the sizes and
  offsets of its marks, and the colours. A change of the model's rounding moves the viewer's
  marks and seeks.
- `PlaylistOffset` wraps a `Duration` and no toolkit's clock time, so that a viewer on any
  toolkit maps its player's time through the model. The Linux viewer converts to and from
  GStreamer's clock time in `crates/iris-gui/src/playback.rs`. `playlist_offset` and
  `wall_time` sum the ticks and convert each sum with `media_micros`; never sum rounded
  fragments.
- The Linux viewer serves each body it moves out of a `PlaybackSnapshot` at its name, under its
  capability path. Its snapshot tests (`crates/iris-gui/src/snapshot.rs`) and the
  `playback_snapshot` display test fetch the bodies there. A change of a name, or of which names
  a snapshot holds, changes what the players fetch. The viewer passes `thumbnail_times` its
  twelve slots, so a change of that rule moves which keyframes the viewer requests.
- `SoundClock::fragment` returns `Some` exactly when a viewer sets offsets. The Linux viewer's
  `Pairing` (`crates/iris-gui/src/sound.rs`) sets the offsets on its pads as returned, so a
  change of what or when the clock returns moves the viewer's sound against its picture. The
  rule's constants are `WINDOW_SECONDS` (ten, with one least for each second of arrivals),
  `DELAY`, and `REANCHOR` (80 ms; a move of exactly 80 ms moves nothing).
- `live::LiveDecoder` reads with the daemon's readers and no second implementation:
  `DecoderConfig::parse` for an init segment and `iris_mp4::reader::parse_fragment` for a
  fragment. `one_sample` takes `offset` as `1 + payload_offset`, `len` from `sample_sizes`,
  `duration` from `sample_durations`, and `is_sync` from `starts_with_sync`. The test
  `a_sync_fragment_gives_one_sample_at_its_place_in_the_message` is the authority on the offset.
- The design of `docs/research/15-apple-clients.md` §4 has the planned Swift caller build its
  sample buffer from the rustdoc of `LiveSample`. That rustdoc states four-byte NAL lengths, raw
  AAC frames, the decode time as the presentation time, and a wall time that is a label and not
  an order. Keep it true to the daemon's writer.
- The docs of `LiveDecoder::decode`, of each `LiveError` variant, and of `src/live.rs` each say
  which refusals a viewer drops and which end the session. A change of one changes the three.
- `live::Reconnect` is the policy of the transport's feed loop
  (`crates/iris-transport/src/feed.rs`). The transport's
  `an_oversized_stream_backs_off_instead_of_reconnecting_every_two_seconds` and
  `an_audio_feed_the_daemon_does_not_have_ends` drive the loop through the policy, so a change
  of an answer changes what the viewer waits for and shows.
- The crate has no input or output, no toolkit, no dependency that builds C, and no secret. A
  dependency added here, a dev-dependency among them, keeps
  `cargo tree -p iris-client -e normal,build,dev` free of `cc` and `-sys` crates and
  `cargo check --target aarch64-apple-ios -p iris-client` passing. `just cross-test` builds the
  crate's tests for musl without a C compiler.
- The fuzz targets `vod_playlist`, `live_fragment`, `live_message`, `live_sample`, and
  `client_answers` drive `src/vod.rs`, `src/live/media_time.rs`, `src/live/protocol.rs`,
  `src/live/decoder.rs`, and the files of `src/api/`. A change to one of those files runs
  `just fuzz-build`.

## Where things are

Each file holds its tests in `mod tests`.

- `src/lib.rs` — the crate documentation, the seven modules, and the item that compiles the
  example of `README.md` as a doctest.
- `src/api.rs` — the documentation of `api`, which declares and re-exports the six files of
  `src/api/`:
  - `types.rs` — the answer types with `EventId`, the two request bodies, `Retention`, and the
    redacting `Debug` with `shown_url`.
  - `limits.rs` — the bounds.
  - `validate.rs` — `Window`, the validators, the text rules, and `rtsp_url_problem` with
    `StreamUrlProblem` and `parse_stream_url`, the parse it shares with `Debug`.
  - `error.rs` — `Call`, `ApiError`, and `Rule`, with the text of each.
  - `request.rs` — `Method`, `BodyRead`, `HttpRequest`, the read deadlines, the builders on
    `ApiOrigin`, and `Head` with the rules of each call.
  - `answer.rs` — `Answer`, the daemon's message, and the check of each call.
- `src/origin.rs` — `ApiOrigin`, the `PlainHttp` policy, `OriginError`, and the live URL.
- `src/vod.rs` — fragment timing, the rewrite onto the API origin, the tag allow-list, the
  multivariant playlist, `PlaylistError`, and `ReferenceError`.
- `src/timeline_model.rs` — `TimelineModel`, `PlaylistOffset`, `SpanExtent`, and
  `MAX_TIMELINE_TICKS`.
- `src/playback.rs` — `PlaybackSelection`, `PlaybackSnapshot`, `ThumbnailTime`,
  `thumbnail_times`, and `export_name`.
- `src/sound_clock.rs` — `SoundClock`, `Placement`, `Engagement`, `Medium`, and `DELAY`, with
  the least delay of a feed and the anchors of both, which stay private.
- `src/live.rs` — the documentation of `live`, which declares and re-exports the five files of
  `src/live/`:
  - `protocol.rs` — the type bytes, the payload and message limits, `LiveMessage`,
    `live_message`, `LiveError`, `LiveSink`, and `SinkError`.
  - `decoder.rs` — `LiveDecoder` with its private state, `LiveItem`, and `LiveSample`.
  - `reconnect.rs` — `Reconnect`, `SessionEnd`, `Next`, and `FeedEvent`.
  - `budget.rs` — `LiveBudget` and `Reservation`.
  - `media_time.rs` — `media_time`, the media time of a live fragment.

## How to verify a change

```sh
cargo fmt -p iris-client
cargo clippy -p iris-client --all-targets -- -D warnings
cargo test -p iris-client       # the unit tests, and the rustdoc and README examples
cargo test -p iris-transport    # the Linux viewer's transport over the crate, on the loopback
RUSTDOCFLAGS="-D warnings" cargo doc -p iris-client --no-deps
cargo tree -p iris-client -e normal,build,dev    # no reqwest, tokio, rustls, aws-lc, or cc
cargo check --target aarch64-apple-ios -p iris-client    # with the iOS target installed
just cross-test        # the tests as 64-bit ARM code, as CI runs them
just fuzz-build        # after a change to a file a fuzz target drives
just mutants crates/iris-client/src/vod.rs    # the file changed: none missed
```

No mutant is missed in `vod.rs`, `timeline_model.rs`, `playback.rs`, `sound_clock.rs`, the files
of `api/` but `limits.rs`, and the files of `live/` but `media_time.rs`.

The Linux viewer builds on the crate, so a change here is checked there too, on a host with the
GTK 4 and GStreamer development libraries. `just check` runs the transport's tests and the
viewer's, whose tests of `sound::Pairing` set pads from the clock. `just display-test` runs the
viewer's display checks: `playback_origin` finds two refusals in the viewer's log, and
`feeds_captured_together_reach_their_sinks_together` plays two feeds through the clock.

## Pitfalls

- `ReferenceError::Unresolved` is `#[error(transparent)]`: its text is the `url` crate's, and it
  adds no source. A reference that does not parse therefore prints as "invalid VOD media URI:
  invalid port number", two messages and not three.
- `Url::join` takes a reference that has a scheme as absolute, so `file:` and `data:` references
  resolve; only the origin check refuses them.
- `str::lines` ends a line at a line feed and drops one carriage return before it. Any other
  carriage return stays in the line, where a reader that ended a line at it would see lines the
  rewrite did not check, so `absolute_playlist` refuses it.
- `Url::parse` normalises a host before `ApiOrigin` checks it. It lowercases a name, turns an
  IPv4 address written in hexadecimal or octal into the dotted address it stands for, and writes
  an IPv6 address in hexadecimal groups, an IPv4-mapped one included. The policy sees those
  forms, and a refusal names the host as the URL holds it.
- `Url::parse` gives every `http` and `https` URL a host, so `ApiOrigin::try_new` takes the host
  with an `expect` and has no refusal for a URL without one.
- `MediaTicks::as_micros` multiplies in `i64` and overflows past about 9.2 × 10^16 ticks, which
  panics where overflow is checked. `validate_playback_fragments` converts in `i128` with the
  same rounding for that reason.
- The offset of a `Placement` counts running time from the first fragment the feed received
  since it started. GStreamer's demuxer starts its segment there. A viewer calls
  `SoundClock::restart` after a feed's reconnection and before its next fragment. Otherwise the
  media times of the next run are placed against the first fragment of the run before it.
- An estimate falls at once and rises only as arrivals leave its window. A frame later than
  those of the last ten seconds moves nothing until they have left the window. A drift toward
  later arrivals therefore reaches the threshold up to ten seconds after it has grown past it.
- rustdoc builds with `--no-deps`, where an intra-doc link resolves only to an item of this
  workspace or of `std`. The docs therefore link `iris_core` and `iris_mp4` items, and name an
  item of a crates.io dependency (`http::Method`, `url::Url`, `serde_json::Error`) as code. A
  link into another crate of the workspace renders only when that crate's directory stands in
  `target/doc`, which `just doc` makes sure of. From an empty `target/doc`,
  `cargo doc -p iris-client --no-deps` passes with warnings denied and leaves such links as
  bracketed text. `just doc` empties `target/doc` and documents every crate, so read rendered
  links after `just doc`.

## Related documents

- `docs/research/15-apple-clients.md` §4, §5, and §11; `docs/research/13-audio.md` §8 and §10.4.
- Dependents: `iris-transport`, `iris-gui`, and the five fuzz targets named above.
- `crates/iris-mp4/README.md` (the readers the live decoder reads with),
  `crates/iris-transport/README.md` (the Linux viewer's transport over this crate), and
  `crates/iris-api/README.md` (the route table the builders name, and the live WebSocket's
  messages).
