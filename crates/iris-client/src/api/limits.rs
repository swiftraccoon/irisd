//! The bounds an answer of the irisd API is checked against before a viewer
//! uses it, and those of the requests it makes. They bound the bytes of a
//! body, the rows of a list, the length of a text, the longest time range,
//! and the wait of a discovery. Where the daemon has a limit, the constant has
//! its value, and its doc names the daemon's constant and file. The tests of
//! `crates/iris-api/src/client_bounds.rs` compare the two. A request beyond
//! such a limit is refused before it is sent, and so is an answer beyond one.
//! The daemon keeps to each limit in what it sends, but a stream URL it lists
//! can be longer than [`MAX_STREAM_URL_BYTES`].

use std::ops::RangeInclusive;

use iris_core::event::{MAX_EVENT_DATA_LEN, MAX_EVENT_KIND_LEN};

/// The `wait_ms` a discovery may name: 500 to 10,000 milliseconds, the
/// daemon's range (`DISCOVERY_WAIT_MS`, `crates/iris-api/src/onvif.rs`),
/// which refuses any other with 400.
pub const DISCOVERY_WAIT_MS: RangeInclusive<u64> = 500..=10_000;
/// Bounds the camera list answer before a viewer builds a view and a feed for
/// each camera.
pub const MAX_CAMERAS_BODY_BYTES: usize = 1 << 20;
/// Bounds the one camera `POST /api/cameras` answers with.
pub const MAX_CAMERA_BODY_BYTES: usize = 16 << 10;
/// Bounds a discovery answer: at most [`MAX_DISCOVERED_DEVICES`] devices of
/// bounded text.
pub const MAX_DEVICES_BODY_BYTES: usize = 512 << 10;
/// Bounds a stream lookup answer: at most [`MAX_PROFILE_STREAMS`] profiles.
pub const MAX_STREAMS_BODY_BYTES: usize = 64 << 10;
/// Bounds an error body read for its message.
pub const MAX_ERROR_BODY_BYTES: usize = 16 << 10;
/// Longest message of the daemon's that a viewer shows with a refusal.
pub const MAX_ERROR_MESSAGE_CHARS: usize = 200;
/// More cameras than a viewer can usefully show are refused before it builds
/// a view for any.
pub const MAX_CAMERAS: usize = 256;
/// Longest camera name or event kind a viewer shows, in bytes: the daemon's
/// bound on the unmapped vendor code it keeps as an event's kind
/// (`iris_core::event::MAX_EVENT_KIND_LEN`, `crates/iris-core/src/event.rs`).
pub const MAX_DISPLAY_STRING_BYTES: usize = 1024;
/// Most discovered devices accepted from the daemon, its limit
/// (`iris_events::discovery::MAX_DISCOVERED`,
/// `crates/iris-events/src/discovery.rs`).
pub const MAX_DISCOVERED_DEVICES: usize = 64;
/// Most profile streams accepted from the daemon, its limit
/// (`iris_events::media::MAX_STREAM_PROFILES`,
/// `crates/iris-events/src/media.rs`).
pub const MAX_PROFILE_STREAMS: usize = 8;
/// Longest discovery scope, profile token, or profile name the daemon sends
/// (`iris_events::discovery::MAX_TEXT_CHARS`, which `discovery` re-exports
/// from the private module of `crates/iris-events/src/text.rs`).
pub const MAX_LABEL_CHARS: usize = 128;
/// Longest endpoint reference the daemon sends
/// (`iris_events::discovery::MAX_ENDPOINT_CHARS`,
/// `crates/iris-events/src/discovery.rs`).
pub const MAX_ENDPOINT_CHARS: usize = 256;
/// Longest host name as text, the daemon's bound on a configured host
/// (`iris_core::config::MAX_HOST_BYTES`, `crates/iris-core/src/config.rs`).
pub const MAX_HOST_BYTES: usize = 253;
/// Longest stream URL a viewer takes, and the longest the daemon takes from a
/// camera (`iris_events::MAX_ADVERTISED_URL_LEN`,
/// `crates/iris-events/src/transport.rs`). The daemon checks the URL as the
/// camera advertises it, then parses it and moves it onto the host the
/// request names (`rebase_rtsp`, `crates/iris-events/src/media.rs`). A URL
/// it lists can therefore be longer, and the viewer refuses a list with one.
pub const MAX_STREAM_URL_BYTES: usize = 2_048;
/// Bounds a segment or event list answer.
pub const MAX_LIST_BODY_BYTES: usize = 16 << 20;
/// Most segment rows the daemon answers for one range (`MAX_SEGMENT_ROWS`,
/// `crates/iris-api/src/hls.rs`).
pub const MAX_SEGMENTS: usize = 4_096;
/// Most events the daemon answers in one page (`MAX_LIMIT`,
/// `crates/iris-api/src/events.rs`).
pub const MAX_EVENTS: usize = 10_000;
/// Longest time range the daemon takes, in microseconds (`MAX_RANGE_US`,
/// `crates/iris-api/src/query/range.rs`).
pub const MAX_RANGE_US: i128 = 7 * 86_400 * 1_000_000;
/// Bounds a keyframe answer: the daemon's largest init segment
/// (`MAX_INIT_BYTES`, 4 MiB) and one sample of its largest fragment
/// (`MAX_FRAGMENT_BYTES`, 66 MiB). A mebibyte more covers the boxes it writes
/// around the sample. Both constants are in `crates/iris-api/src/hls.rs`.
pub const MAX_KEYFRAME_BYTES: usize = 71 << 20;
/// Bounds a retain answer, 135 KiB: one event as the daemon lists it
/// (`EventDto`, `crates/iris-api/src/events.rs`). Its `data` is JSON text
/// that `serde_json` wrote, of at most [`MAX_EVENT_DATA_LEN`] bytes. That text
/// holds no raw control character, so escaping it as a string at most doubles
/// it. A vendor code as its kind is at most [`MAX_EVENT_KIND_LEN`] bytes, and
/// escaping makes each control character six. The other fields take less than
/// a kibibyte.
pub const MAX_RETAIN_BODY_BYTES: usize =
    2 * MAX_EVENT_DATA_LEN + 6 * MAX_EVENT_KIND_LEN + (1 << 10);
