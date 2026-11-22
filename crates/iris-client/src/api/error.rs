//! The calls a viewer makes to the irisd API, and the text of each refusal.

use std::fmt;

use http::StatusCode;
use thiserror::Error;

use crate::api::{MAX_PROFILE_STREAMS, StreamUrlProblem};
use crate::vod::PlaylistError;

/// A request a viewer makes to the irisd API. Its
/// [`Display`](fmt::Display) names the request the way a viewer's messages
/// do: "listing the cameras". The enum is `#[non_exhaustive]`, so a match
/// outside the crate needs a wildcard arm.
///
/// # Example
///
/// ```
/// # use std::error::Error;
/// #
/// use iris_client::api::Call;
/// use iris_client::origin::{ApiOrigin, PlainHttp};
///
/// let api = "https://nvr.example:8443";
/// let origin = ApiOrigin::parse(api, PlainHttp::LoopbackOnly)?;
/// let request = origin.cameras();
/// assert_eq!(request.call(), Call::Cameras);
/// // A refusal of the answer names the request in these words.
/// assert_eq!(request.call().to_string(), "listing the cameras");
/// # Ok::<(), Box<dyn Error>>(())
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Call {
    /// `GET /api/cameras`.
    Cameras,
    /// `POST /api/cameras`.
    AddCamera,
    /// `DELETE /api/cameras/{camera}`.
    RemoveCamera,
    /// `GET /api/streams/{camera}/{kind}/segments`.
    Segments,
    /// `GET /api/events`.
    Events,
    /// `GET /api/cameras/{camera}/export.mp4` for a main stream, whose export
    /// holds the camera's recorded audio, and
    /// `GET /api/streams/{camera}/{kind}/export.mp4` for another stream.
    Export,
    /// `POST /api/events/{id}/retain`.
    RetainEvent,
    /// `GET /api/streams/{camera}/{kind}/keyframe`.
    Keyframe,
    /// `GET /api/streams/{camera}/{kind}/hls/vod.m3u8`.
    VodPlaylist,
    /// `GET /api/onvif/devices`.
    Discover,
    /// `POST /api/onvif/streams`.
    StreamUrls,
}

impl fmt::Display for Call {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Cameras => "listing the cameras",
            Self::AddCamera => "adding the camera",
            Self::RemoveCamera => "removing the camera",
            Self::Segments => "listing the segments",
            Self::Events => "listing the events",
            Self::Export => "requesting the export",
            Self::RetainEvent => "retaining the event",
            Self::Keyframe => "requesting a keyframe",
            Self::VodPlaylist => "fetching the VOD playlist",
            Self::Discover => "discovering cameras",
            Self::StreamUrls => "asking the camera for its streams",
        })
    }
}

/// A status as a viewer shows it: its code and the reason phrase the `http`
/// crate gives it (`409 Conflict`, `599 <unknown status code>`). The Linux
/// viewer's HTTP client prints a status the same way. A value outside 100 to
/// 999, which is no status that crate holds, shows as its number alone.
struct ShownStatus(u16);

impl fmt::Display for ShownStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match StatusCode::from_u16(self.0) {
            Ok(status) => write!(f, "{status}"),
            Err(_) => write!(f, "{}", self.0),
        }
    }
}

/// The daemon's message after a refused status, as a viewer shows it: a
/// colon and the message, or nothing.
fn after_status(detail: Option<&str>) -> String {
    detail.map_or_else(String::new, |detail| format!(": {detail}"))
}

/// How a viewer says the answer to `call` came from another origin.
fn from_another_origin(call: Call) -> String {
    match call {
        Call::VodPlaylist => "VOD playlist response came from another origin".to_owned(),
        Call::Cameras
        | Call::AddCamera
        | Call::RemoveCamera
        | Call::Segments
        | Call::Events
        | Call::Export
        | Call::RetainEvent
        | Call::Keyframe
        | Call::Discover
        | Call::StreamUrls => format!("the answer to {call} came from another origin"),
    }
}

/// What a viewer says it was decoding when the answer to `call` is not the
/// JSON the call answers with.
fn decoding(call: Call) -> String {
    match call {
        Call::AddCamera => "decoding the camera".to_owned(),
        Call::RetainEvent => "decoding the event".to_owned(),
        Call::StreamUrls => "decoding the streams".to_owned(),
        Call::Cameras
        | Call::RemoveCamera
        | Call::Segments
        | Call::Events
        | Call::Export
        | Call::Keyframe
        | Call::VodPlaylist
        | Call::Discover => format!("decoding the answer to {call}"),
    }
}

/// Why a viewer refuses an answer of the irisd API, or a request it was
/// asked to make. The text of each is the message a viewer shows. A failure
/// to decode keeps the error of `serde_json` as its source. A refused playlist
/// has the text of its [`PlaylistError`], with that error's reason after it,
/// so that a viewer printing the chain of sources prints each once. The
/// example of the [`answer`](crate::api::answer) module shows the text of a
/// refused status.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum ApiError {
    /// The daemon answered with a status the call does not expect. A call
    /// expects any `2xx`, but 201 alone for an added camera, 204 alone for a
    /// removal, and 200 alone for an export. The text shows the status as the
    /// `http` crate writes it, "409 Conflict", and the daemon's message after
    /// it when it gave one: "adding the camera failed with 409 Conflict: ...".
    #[error(
        "{call} failed with {}{}",
        ShownStatus(*.status),
        after_status(.detail.as_deref())
    )]
    Refused {
        /// The call refused, which the text names first.
        call: Call,
        /// The answer's HTTP status code as the transport received it. Any
        /// value shows: one outside 100 to 999 shows as its number alone.
        status: u16,
        /// The message of the daemon's answer, when it gave one.
        detail: Option<String>,
    },
    /// The answer is longer than the call's byte limit.
    #[error("the answer to {call} exceeds its byte limit")]
    Oversized {
        /// The call whose answer is too long, which the text names.
        call: Call,
    },
    /// The answer came from an origin other than the API's. For a VOD
    /// playlist the text is "VOD playlist response came from another origin",
    /// and for any other call it is "the answer to {call} came from another
    /// origin".
    #[error("{}", from_another_origin(*.call))]
    ForeignOrigin {
        /// The call answered from another origin.
        call: Call,
    },
    /// The answer's `Content-Type` is not the media type the call answers
    /// with.
    #[error("response is not {expected}")]
    ContentType {
        /// The call answered with another media type. The text does not name
        /// it.
        call: Call,
        /// The media type the call answers with, such as `video/mp4`.
        expected: &'static str,
    },
    /// The answer is not the JSON the call answers with, and the source, an
    /// error of `serde_json`, says why. The text names what the viewer
    /// decoded. It is "decoding the camera" for an added camera, "decoding the
    /// event" for a retained one, and "decoding the streams" for a stream
    /// lookup. For any other call it is "decoding the answer to {call}".
    #[error("{}", decoding(*.call))]
    Decode {
        /// The call whose answer does not decode, which the text names.
        call: Call,
        /// Why the body does not decode.
        source: serde_json::Error,
    },
    /// An answer or a request breaks a rule, which the value names; the
    /// refusal has the text of the [`Rule`].
    #[error(transparent)]
    Invalid(#[from] Rule),
    /// A VOD playlist is refused; the refusal has the text of the
    /// [`PlaylistError`] it holds.
    #[error(transparent)]
    Playlist(#[from] PlaylistError),
}

/// A rule of the irisd API that an answer or a request breaks, which an
/// [`ApiError::Invalid`] holds. The text of each is the message a viewer
/// shows. A rule whose text names what is wrong with a stream URL holds it as
/// a [`StreamUrlProblem`], whose phrase completes the text.
///
/// # Example
///
/// ```
/// # use std::error::Error;
/// #
/// use iris_client::api::{ApiError, Rule, Window};
/// use iris_core::time::WallUs;
///
/// let start = WallUs::new(60_000_000);
/// let Err(ApiError::Invalid(rule)) = Window::try_new(start, start) else {
///     return Err("an empty window is refused".into());
/// };
/// assert_eq!(rule, Rule::TimeRange);
/// assert_eq!(rule.to_string(), "invalid time range");
/// # Ok::<(), Box<dyn Error>>(())
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Error)]
#[non_exhaustive]
pub enum Rule {
    /// A time window does not end after it starts
    /// ([`Window::try_new`](crate::api::Window::try_new)).
    #[error("invalid time range")]
    TimeRange,
    /// A time window spans more than seven days,
    /// [`MAX_RANGE_US`](crate::api::MAX_RANGE_US).
    #[error("time range exceeds seven days")]
    TimeRangeTooLong,
    /// An event id is not positive
    /// ([`EventId::try_new`](crate::api::EventId::try_new)).
    #[error("invalid event id")]
    EventId,
    /// The limit of an event page, in a request or in the check of its
    /// answer, is not 1 to [`MAX_EVENTS`](crate::api::MAX_EVENTS).
    #[error("invalid event page limit")]
    EventPageLimit,
    /// A discovery's wait, in whole milliseconds, lies outside
    /// [`DISCOVERY_WAIT_MS`](crate::api::DISCOVERY_WAIT_MS), the daemon's range.
    #[error("invalid discovery wait")]
    DiscoveryWait,
    /// The main stream URL of a camera to add has a problem, in the sentence
    /// the Linux viewer's form shows for it: "the RTSP URL names no host".
    #[error("the RTSP URL {0}")]
    RtspUrl(StreamUrlProblem),
    /// The sub stream URL of a camera to add has a problem: "the sub URL
    /// names no host".
    #[error("the sub URL {0}")]
    SubUrl(StreamUrlProblem),
    /// A camera list holds more than
    /// [`MAX_CAMERAS`](crate::api::MAX_CAMERAS) cameras.
    #[error("too many cameras in response")]
    TooManyCameras,
    /// A camera's name is not a display string
    /// ([`is_valid_display_string`](crate::api::is_valid_display_string)).
    #[error("invalid camera name in response")]
    CameraName,
    /// A camera list holds one id twice.
    #[error("duplicate camera id in response")]
    DuplicateCameraId,
    /// The camera the daemon added is not the one sent.
    #[error("the daemon answered with another camera")]
    AnotherCamera,
    /// A segment listing holds more than
    /// [`MAX_SEGMENTS`](crate::api::MAX_SEGMENTS) rows.
    #[error("too many segments in response")]
    TooManySegments,
    /// A sealed segment does not end after it starts.
    #[error("invalid segment time range in response")]
    SegmentRange,
    /// An open segment ends before it starts.
    #[error("invalid open segment time range in response")]
    OpenSegmentRange,
    /// A segment lies outside the window it was listed for.
    #[error("segment is outside the requested window")]
    SegmentOutsideWindow,
    /// A page holds more events than its limit.
    #[error("too many events in response")]
    TooManyEvents,
    /// An event's kind is not a display string
    /// ([`is_valid_display_string`](crate::api::is_valid_display_string)).
    #[error("invalid event kind in response")]
    EventKindLabel,
    /// An event lies outside the window it was listed for.
    #[error("event is outside the requested window")]
    EventOutsideWindow,
    /// A page holds one event id twice.
    #[error("duplicate event id in response")]
    DuplicateEventId,
    /// The event the daemon answered a retain mark with is not the one
    /// marked.
    #[error("retain response changed the event id")]
    AnotherEvent,
    /// A keyframe answer holds no byte.
    #[error("keyframe response is empty")]
    EmptyKeyframe,
    /// A VOD playlist's fragments do not start in order.
    #[error("VOD fragment times are not ordered")]
    FragmentOrder,
    /// No fragment of a VOD playlist overlaps the window it was asked for.
    #[error("VOD playlist has no fragment in the requested window")]
    NoFragmentInWindow,
    /// A discovery lists more than
    /// [`MAX_DISCOVERED_DEVICES`](crate::api::MAX_DISCOVERED_DEVICES)
    /// devices.
    #[error("the daemon listed more devices than one probe reports")]
    TooManyDevices,
    /// A device's host is not a host of the form the daemon lists
    /// ([`is_valid_device_host`](crate::api::is_valid_device_host)).
    #[error("the daemon listed a device with an invalid host")]
    DeviceHost,
    /// A device's endpoint reference is longer than
    /// [`MAX_ENDPOINT_CHARS`](crate::api::MAX_ENDPOINT_CHARS) characters or
    /// holds a control character.
    #[error("the daemon listed a device with an invalid endpoint reference")]
    DeviceEndpoint,
    /// A device's scope is empty, longer than
    /// [`MAX_LABEL_CHARS`](crate::api::MAX_LABEL_CHARS) characters, or holds a
    /// control character.
    #[error("the daemon listed a device with an invalid scope")]
    DeviceScope,
    /// A stream lookup lists no profile.
    #[error("the daemon listed no usable stream")]
    NoUsableStream,
    /// A stream lookup lists more than
    /// [`MAX_PROFILE_STREAMS`](crate::api::MAX_PROFILE_STREAMS) profiles, the
    /// most the daemon asks a camera about.
    #[error("the daemon listed more than {} streams", MAX_PROFILE_STREAMS)]
    TooManyStreams,
    /// A profile's token or name is longer than
    /// [`MAX_LABEL_CHARS`](crate::api::MAX_LABEL_CHARS) characters or holds a
    /// control character.
    #[error("the daemon listed a profile with an invalid token or name")]
    ProfileText,
    /// A profile's stream URL has a problem: "the daemon listed a stream URL
    /// that names no host".
    #[error("the daemon listed a stream URL that {0}")]
    ListedStreamUrl(StreamUrlProblem),
}

#[cfg(test)]
mod tests {
    use std::error::Error;

    use super::*;
    use crate::vod::ReferenceError;

    #[test]
    fn each_call_has_the_text_a_viewer_names_the_request_by() {
        for (call, text) in [
            (Call::Cameras, "listing the cameras"),
            (Call::AddCamera, "adding the camera"),
            (Call::RemoveCamera, "removing the camera"),
            (Call::Segments, "listing the segments"),
            (Call::Events, "listing the events"),
            (Call::Export, "requesting the export"),
            (Call::RetainEvent, "retaining the event"),
            (Call::Keyframe, "requesting a keyframe"),
            (Call::VodPlaylist, "fetching the VOD playlist"),
            (Call::Discover, "discovering cameras"),
            (Call::StreamUrls, "asking the camera for its streams"),
        ] {
            assert_eq!(call.to_string(), text, "{call:?}");
        }
    }

    #[test]
    fn an_oversized_answer_names_its_call() {
        assert_eq!(
            ApiError::Oversized {
                call: Call::Segments
            }
            .to_string(),
            "the answer to listing the segments exceeds its byte limit"
        );
    }

    /// The text names the media type the call answers with, and not the
    /// call.
    #[test]
    fn an_answer_of_another_media_type_names_the_one_expected() {
        assert_eq!(
            ApiError::ContentType {
                call: Call::Keyframe,
                expected: "video/mp4"
            }
            .to_string(),
            "response is not video/mp4"
        );
    }

    /// The text a viewer shows for `rule`, and the rule after it in the walk
    /// of the test below. The text of `RtspUrl`, `SubUrl`, and
    /// `ListedStreamUrl` is the one of the problem the walk gives each:
    /// `NoHost`, `Fragment`, and `OtherScheme`. The `match` is exhaustive, so a
    /// variant added to `Rule` fails to compile here until it has an arm,
    /// which gives it a text and a successor. The walk reaches it only once
    /// another arm names it as the rule after.
    fn text_and_next(rule: Rule) -> (&'static str, Option<Rule>) {
        match rule {
            Rule::TimeRange => ("invalid time range", Some(Rule::TimeRangeTooLong)),
            Rule::TimeRangeTooLong => ("time range exceeds seven days", Some(Rule::EventId)),
            Rule::EventId => ("invalid event id", Some(Rule::EventPageLimit)),
            Rule::EventPageLimit => ("invalid event page limit", Some(Rule::DiscoveryWait)),
            Rule::DiscoveryWait => (
                "invalid discovery wait",
                Some(Rule::RtspUrl(StreamUrlProblem::NoHost)),
            ),
            Rule::RtspUrl(_) => (
                "the RTSP URL names no host",
                Some(Rule::SubUrl(StreamUrlProblem::Fragment)),
            ),
            Rule::SubUrl(_) => (
                "the sub URL cannot carry a fragment",
                Some(Rule::TooManyCameras),
            ),
            Rule::TooManyCameras => ("too many cameras in response", Some(Rule::CameraName)),
            Rule::CameraName => (
                "invalid camera name in response",
                Some(Rule::DuplicateCameraId),
            ),
            Rule::DuplicateCameraId => {
                ("duplicate camera id in response", Some(Rule::AnotherCamera))
            }
            Rule::AnotherCamera => (
                "the daemon answered with another camera",
                Some(Rule::TooManySegments),
            ),
            Rule::TooManySegments => ("too many segments in response", Some(Rule::SegmentRange)),
            Rule::SegmentRange => (
                "invalid segment time range in response",
                Some(Rule::OpenSegmentRange),
            ),
            Rule::OpenSegmentRange => (
                "invalid open segment time range in response",
                Some(Rule::SegmentOutsideWindow),
            ),
            Rule::SegmentOutsideWindow => (
                "segment is outside the requested window",
                Some(Rule::TooManyEvents),
            ),
            Rule::TooManyEvents => ("too many events in response", Some(Rule::EventKindLabel)),
            Rule::EventKindLabel => (
                "invalid event kind in response",
                Some(Rule::EventOutsideWindow),
            ),
            Rule::EventOutsideWindow => (
                "event is outside the requested window",
                Some(Rule::DuplicateEventId),
            ),
            Rule::DuplicateEventId => ("duplicate event id in response", Some(Rule::AnotherEvent)),
            Rule::AnotherEvent => (
                "retain response changed the event id",
                Some(Rule::EmptyKeyframe),
            ),
            Rule::EmptyKeyframe => ("keyframe response is empty", Some(Rule::FragmentOrder)),
            Rule::FragmentOrder => (
                "VOD fragment times are not ordered",
                Some(Rule::NoFragmentInWindow),
            ),
            Rule::NoFragmentInWindow => (
                "VOD playlist has no fragment in the requested window",
                Some(Rule::TooManyDevices),
            ),
            Rule::TooManyDevices => (
                "the daemon listed more devices than one probe reports",
                Some(Rule::DeviceHost),
            ),
            Rule::DeviceHost => (
                "the daemon listed a device with an invalid host",
                Some(Rule::DeviceEndpoint),
            ),
            Rule::DeviceEndpoint => (
                "the daemon listed a device with an invalid endpoint reference",
                Some(Rule::DeviceScope),
            ),
            Rule::DeviceScope => (
                "the daemon listed a device with an invalid scope",
                Some(Rule::NoUsableStream),
            ),
            Rule::NoUsableStream => (
                "the daemon listed no usable stream",
                Some(Rule::TooManyStreams),
            ),
            Rule::TooManyStreams => (
                "the daemon listed more than 8 streams",
                Some(Rule::ProfileText),
            ),
            Rule::ProfileText => (
                "the daemon listed a profile with an invalid token or name",
                Some(Rule::ListedStreamUrl(StreamUrlProblem::OtherScheme)),
            ),
            Rule::ListedStreamUrl(_) => (
                "the daemon listed a stream URL that must start with rtsp://",
                None,
            ),
        }
    }

    /// Each rule the walk visits has the text of the message a viewer shows, a
    /// stream URL's problem completing the sentence that names the URL. The
    /// walk visits each rule once and counts 31, the variants of `Rule`. A
    /// variant added that no arm names as the rule after is never walked, and
    /// the count shows it missing only once the 31 is raised to the number of
    /// variants.
    #[test]
    fn each_rule_has_the_text_a_viewer_shows() {
        let mut walked: Vec<Rule> = Vec::new();
        let mut next = Some(Rule::TimeRange);
        while let Some(rule) = next {
            assert!(!walked.contains(&rule), "the walk returns to {rule:?}");
            let (text, after) = text_and_next(rule);
            assert_eq!(ApiError::Invalid(rule).to_string(), text, "{rule:?}");
            walked.push(rule);
            next = after;
        }
        assert_eq!(walked.len(), 31, "the walk visits every rule");
    }

    /// Statuses with the text a viewer shows for each: a code with the
    /// reason phrase of the `http` crate, and a value outside 100 to 999 as
    /// its number.
    const SHOWN_STATUSES: [(u16, &str); 10] = [
        (409, "409 Conflict"),
        (403, "403 Forbidden"),
        (302, "302 Found"),
        (100, "100 Continue"),
        (599, "599 <unknown status code>"),
        (999, "999 <unknown status code>"),
        (99, "99"),
        (1000, "1000"),
        (0, "0"),
        (u16::MAX, "65535"),
    ];

    /// The text of the refusal of `call` with `status` and `detail`.
    fn refused(call: Call, status: u16, detail: Option<&str>) -> String {
        ApiError::Refused {
            call,
            status,
            detail: detail.map(str::to_owned),
        }
        .to_string()
    }

    /// A status shows as the viewer's HTTP client prints it.
    #[test]
    fn a_refused_status_shows_as_the_http_crate_writes_it() {
        for (status, shown) in SHOWN_STATUSES {
            assert_eq!(
                refused(Call::AddCamera, status, None),
                format!("adding the camera failed with {shown}")
            );
        }
    }

    #[test]
    fn the_daemons_message_follows_a_refused_status() {
        let message = "conflict: camera cam8 already exists";
        for (status, shown) in SHOWN_STATUSES {
            assert_eq!(
                refused(Call::AddCamera, status, Some(message)),
                format!("adding the camera failed with {shown}: {message}")
            );
        }
    }

    /// The transport's fixture tests match parts of this text.
    #[test]
    fn a_refused_stream_lookup_has_the_text_a_viewer_shows() {
        let message = "upstream: the camera could not be queried";
        assert_eq!(
            refused(Call::StreamUrls, 502, Some(message)),
            "asking the camera for its streams failed with 502 Bad Gateway: upstream: the camera \
             could not be queried"
        );
    }

    #[test]
    fn an_answer_from_another_origin_has_the_text_a_viewer_shows() {
        for (call, text) in [
            (
                Call::VodPlaylist,
                "VOD playlist response came from another origin",
            ),
            (
                Call::Cameras,
                "the answer to listing the cameras came from another origin",
            ),
            (
                Call::Keyframe,
                "the answer to requesting a keyframe came from another origin",
            ),
            (
                Call::Export,
                "the answer to requesting the export came from another origin",
            ),
        ] {
            assert_eq!(
                ApiError::ForeignOrigin { call }.to_string(),
                text,
                "{call:?}"
            );
        }
    }

    /// A refusal of `call` for JSON that does not decode, with the error of
    /// `serde_json`.
    fn undecodable(call: Call) -> ApiError {
        ApiError::Decode {
            call,
            source: serde_json::from_str::<Vec<u8>>("{").unwrap_err(),
        }
    }

    #[test]
    fn an_undecodable_answer_names_what_the_viewer_decoded() {
        for (call, text) in [
            (Call::Cameras, "decoding the answer to listing the cameras"),
            (
                Call::Segments,
                "decoding the answer to listing the segments",
            ),
            (Call::Events, "decoding the answer to listing the events"),
            (Call::Discover, "decoding the answer to discovering cameras"),
            (Call::AddCamera, "decoding the camera"),
            (Call::RetainEvent, "decoding the event"),
            (Call::StreamUrls, "decoding the streams"),
        ] {
            assert_eq!(undecodable(call).to_string(), text, "{call:?}");
        }
    }

    #[test]
    fn an_undecodable_answer_keeps_the_serde_json_error_as_its_source() {
        let refused = undecodable(Call::Cameras);
        assert_eq!(
            refused.source().map(ToString::to_string).as_deref(),
            Some("invalid type: map, expected a sequence at line 1 column 0"),
            "the error of serde_json is the source"
        );
    }

    /// A playlist refused because a media reference leaves the origin.
    fn refused_media_reference() -> ApiError {
        ApiError::from(PlaylistError::MediaUri(ReferenceError::OtherOrigin))
    }

    #[test]
    fn a_refused_playlist_has_the_text_of_its_playlist_error() {
        assert_eq!(
            refused_media_reference().to_string(),
            "invalid VOD media URI"
        );
    }

    #[test]
    fn a_refused_playlist_keeps_the_source_of_its_playlist_error() {
        assert_eq!(
            refused_media_reference()
                .source()
                .map(ToString::to_string)
                .as_deref(),
            Some("VOD playlist references another origin")
        );
    }
}
