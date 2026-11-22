//! The rules an answer of the irisd API passes before a viewer uses it, and
//! the rule of the time window a request names. A refusal is an
//! [`ApiError`] whose text is the message a viewer shows: an answer that
//! breaks a rule is [`ApiError::Invalid`] with the [`Rule`] it breaks.

use std::collections::HashSet;
use std::fmt;

use iris_core::time::WallUs;
use url::Url;

use crate::api::{
    ApiError, Camera, DiscoveredDevice, EventRecord, MAX_CAMERAS, MAX_DISCOVERED_DEVICES,
    MAX_DISPLAY_STRING_BYTES, MAX_ENDPOINT_CHARS, MAX_EVENTS, MAX_HOST_BYTES, MAX_LABEL_CHARS,
    MAX_PROFILE_STREAMS, MAX_RANGE_US, MAX_SEGMENTS, MAX_STREAM_URL_BYTES, ProfileStream, Rule,
    Segment,
};
use crate::vod::PlaybackFragment;

/// `Ok` when `holds`, and otherwise [`ApiError::Invalid`] with `rule`, the
/// rule that does not hold.
pub(super) fn require(holds: bool, rule: Rule) -> Result<(), ApiError> {
    if holds {
        Ok(())
    } else {
        Err(ApiError::Invalid(rule))
    }
}

/// A time window `[from, to)` as the API takes one: it ends after it starts,
/// and spans at most [`MAX_RANGE_US`], seven days. The example of the
/// [`api`](crate::api) module makes one and refuses a reversed one.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Window {
    from: WallUs,
    to: WallUs,
}

impl Window {
    /// The window `[from, to)`. The example of the [`api`](crate::api)
    /// module makes one.
    ///
    /// # Errors
    ///
    /// [`ApiError::Invalid`] with [`Rule::TimeRange`], "invalid time range",
    /// when `to` is not after `from`, and with [`Rule::TimeRangeTooLong`],
    /// "time range exceeds seven days", when it is more than [`MAX_RANGE_US`]
    /// after it.
    pub fn try_new(from: WallUs, to: WallUs) -> Result<Self, ApiError> {
        let span = i128::from(to.get()) - i128::from(from.get());
        require(span > 0, Rule::TimeRange)?;
        require(span <= MAX_RANGE_US, Rule::TimeRangeTooLong)?;
        Ok(Self { from, to })
    }

    /// The window's first instant. The example of the [`api`](crate::api)
    /// module reads it.
    #[must_use]
    pub fn from(&self) -> WallUs {
        self.from
    }

    /// The instant the window ends before. The example of the
    /// [`api`](crate::api) module reads it.
    #[must_use]
    pub fn to(&self) -> WallUs {
        self.to
    }
}

/// Whether `text` is a display string, the text a viewer shows as a camera's
/// name or an event's kind: nonblank, at most [`MAX_DISPLAY_STRING_BYTES`],
/// and without control characters.
///
/// # Example
///
/// ```
/// use iris_client::api;
///
/// assert!(api::is_valid_display_string("Camera 1"));
/// assert!(!api::is_valid_display_string("   "));
/// assert!(!api::is_valid_display_string("Camera\u{1b}[31m"));
/// ```
#[must_use]
pub fn is_valid_display_string(text: &str) -> bool {
    !text.trim().is_empty()
        && text.len() <= MAX_DISPLAY_STRING_BYTES
        && !text.chars().any(char::is_control)
}

/// Whether `text` stays within the daemon's bounds on a camera's text
/// (`iris_events::text`): at most `max_chars` characters and no control
/// characters. It may be empty.
///
/// # Example
///
/// ```
/// use iris_client::api::{self, MAX_LABEL_CHARS};
///
/// // A profile the camera left unnamed.
/// assert!(api::is_valid_camera_text("", MAX_LABEL_CHARS));
/// assert!(!api::is_valid_camera_text("Main\u{7}", MAX_LABEL_CHARS));
/// let long = "x".repeat(MAX_LABEL_CHARS + 1);
/// assert!(!api::is_valid_camera_text(&long, MAX_LABEL_CHARS));
/// ```
#[must_use]
pub fn is_valid_camera_text(text: &str, max_chars: usize) -> bool {
    text.chars().count() <= max_chars && !text.chars().any(char::is_control)
}

/// Whether `host` is a device service host as the daemon lists it: a name or
/// a bare address of at most [`MAX_HOST_BYTES`], without whitespace or
/// control characters.
///
/// # Example
///
/// ```
/// use iris_client::api;
///
/// assert!(api::is_valid_device_host("192.0.2.20"));
/// assert!(api::is_valid_device_host("2001:db8::7"));
/// assert!(!api::is_valid_device_host(""));
/// assert!(!api::is_valid_device_host("camera example"));
/// ```
#[must_use]
pub fn is_valid_device_host(host: &str) -> bool {
    (1..=MAX_HOST_BYTES).contains(&host.len())
        && !host.chars().any(|c| c.is_whitespace() || c.is_control())
}

/// What is wrong with a camera's stream URL, as [`rtsp_url_problem`] finds
/// it. Its text completes a sentence that names the URL: "the RTSP URL names
/// no host".
///
/// # Example
///
/// ```
/// use iris_client::api::{self, StreamUrlProblem};
///
/// let problem = api::rtsp_url_problem("rtsp://viewer@192.0.2.10/stream");
/// assert_eq!(problem, Some(StreamUrlProblem::Userinfo));
/// let shown = problem.map(|problem| format!("the RTSP URL {problem}"));
/// let credentials = "the RTSP URL cannot carry credentials; \
///                    enter them as the username and password";
/// assert_eq!(shown.as_deref(), Some(credentials));
/// assert_eq!(api::rtsp_url_problem("rtsp://192.0.2.10/stream"), None);
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum StreamUrlProblem {
    /// The URL is longer than [`MAX_STREAM_URL_BYTES`]: "is longer than
    /// 2048 bytes".
    TooLong,
    /// The URL does not parse: "is not a URL".
    NotAUrl,
    /// The URL's scheme is not `rtsp`: "must start with rtsp://".
    OtherScheme,
    /// The URL names no host: "names no host".
    NoHost,
    /// The URL carries userinfo: "cannot carry credentials; enter them as
    /// the username and password".
    Userinfo,
    /// The URL carries a fragment: "cannot carry a fragment".
    Fragment,
}

/// The phrase that completes a sentence naming the URL.
impl fmt::Display for StreamUrlProblem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooLong => write!(f, "is longer than {MAX_STREAM_URL_BYTES} bytes"),
            Self::NotAUrl => f.write_str("is not a URL"),
            Self::OtherScheme => f.write_str("must start with rtsp://"),
            Self::NoHost => f.write_str("names no host"),
            Self::Userinfo => {
                f.write_str("cannot carry credentials; enter them as the username and password")
            }
            Self::Fragment => f.write_str("cannot carry a fragment"),
        }
    }
}

/// What is wrong with `url` as a camera stream URL, or `None` for a usable one:
/// `rtsp://` with a host, without userinfo or a fragment, and at most
/// [`MAX_STREAM_URL_BYTES`]. The checks run in the order the
/// [`StreamUrlProblem`] variants are declared, and the first that fails is the
/// problem. The daemon applies the same rule to the URLs a camera advertises
/// and refuses credentials in configured URLs. The example on
/// [`StreamUrlProblem`] finds a problem with one URL and none with another.
#[must_use]
pub fn rtsp_url_problem(url: &str) -> Option<StreamUrlProblem> {
    parse_stream_url(url).err()
}

/// `url` parsed as a camera stream URL, or what is wrong with it, by the rule
/// of [`rtsp_url_problem`].
pub(super) fn parse_stream_url(url: &str) -> Result<Url, StreamUrlProblem> {
    if url.len() > MAX_STREAM_URL_BYTES {
        return Err(StreamUrlProblem::TooLong);
    }
    let Ok(parsed) = Url::parse(url) else {
        return Err(StreamUrlProblem::NotAUrl);
    };
    if parsed.scheme() != "rtsp" {
        return Err(StreamUrlProblem::OtherScheme);
    }
    if !parsed.has_host() {
        return Err(StreamUrlProblem::NoHost);
    }
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err(StreamUrlProblem::Userinfo);
    }
    if parsed.fragment().is_some() {
        return Err(StreamUrlProblem::Fragment);
    }
    Ok(parsed)
}

/// Checks the camera list: at most [`MAX_CAMERAS`] cameras, each named by a
/// display string ([`is_valid_display_string`]), and no id twice. The
/// example of the [`api`](crate::api) module checks a list.
///
/// # Errors
///
/// [`ApiError::Invalid`] with the first rule the list breaks:
/// [`Rule::TooManyCameras`], then for each camera in turn
/// [`Rule::CameraName`] and [`Rule::DuplicateCameraId`].
pub fn validate_cameras(cameras: &[Camera]) -> Result<(), ApiError> {
    require(cameras.len() <= MAX_CAMERAS, Rule::TooManyCameras)?;
    let mut ids = HashSet::with_capacity(cameras.len());
    for camera in cameras {
        require(is_valid_display_string(&camera.name), Rule::CameraName)?;
        require(ids.insert(&camera.id), Rule::DuplicateCameraId)?;
    }
    Ok(())
}

/// Checks a discovery answer: at most [`MAX_DISCOVERED_DEVICES`] devices,
/// each with a host that [`is_valid_device_host`] takes. A device's endpoint
/// reference is at most [`MAX_ENDPOINT_CHARS`], and each scope is nonempty and
/// at most [`MAX_LABEL_CHARS`]. The reference and each scope are text that
/// [`is_valid_camera_text`] takes.
///
/// # Errors
///
/// [`ApiError::Invalid`] with the first rule the answer breaks:
/// [`Rule::TooManyDevices`], then for each device in turn
/// [`Rule::DeviceHost`], [`Rule::DeviceEndpoint`], and [`Rule::DeviceScope`].
///
/// # Example
///
/// ```
/// # use std::error::Error;
/// #
/// use iris_client::api::{self, DiscoveredDevice};
///
/// let mut device = DiscoveredDevice {
///     endpoint: "urn:uuid:1".to_owned(),
///     host: "192.0.2.20".to_owned(),
///     port: 80,
///     https: false,
///     name: Some("Camera 1".to_owned()),
///     hardware: None,
///     location: None,
/// };
/// api::validate_devices(&[device.clone()])?;
/// device.location = Some(String::new());
/// let Err(refused) = api::validate_devices(&[device]) else {
///     return Err("an empty scope is refused".into());
/// };
/// assert_eq!(
///     refused.to_string(),
///     "the daemon listed a device with an invalid scope"
/// );
/// # Ok::<(), Box<dyn Error>>(())
/// ```
pub fn validate_devices(devices: &[DiscoveredDevice]) -> Result<(), ApiError> {
    require(
        devices.len() <= MAX_DISCOVERED_DEVICES,
        Rule::TooManyDevices,
    )?;
    for device in devices {
        require(is_valid_device_host(&device.host), Rule::DeviceHost)?;
        require(
            is_valid_camera_text(&device.endpoint, MAX_ENDPOINT_CHARS),
            Rule::DeviceEndpoint,
        )?;
        require(
            [&device.name, &device.hardware, &device.location]
                .into_iter()
                .flatten()
                .all(|scope| !scope.is_empty() && is_valid_camera_text(scope, MAX_LABEL_CHARS)),
            Rule::DeviceScope,
        )?;
    }
    Ok(())
}

/// Checks a stream lookup's answer: one to [`MAX_PROFILE_STREAMS`] profiles,
/// each with a token and a name of at most [`MAX_LABEL_CHARS`] that
/// [`is_valid_camera_text`] takes, and a URL that [`rtsp_url_problem`] finds
/// nothing wrong with.
///
/// # Errors
///
/// [`ApiError::Invalid`] with the first rule the answer breaks:
/// [`Rule::NoUsableStream`] for no profile, [`Rule::TooManyStreams`] for more
/// than [`MAX_PROFILE_STREAMS`], then for each profile in turn
/// [`Rule::ProfileText`] and [`Rule::ListedStreamUrl`]. The last holds what
/// [`rtsp_url_problem`] finds wrong with the profile's URL.
///
/// # Example
///
/// ```
/// # use std::error::Error;
/// #
/// use iris_client::api::{self, ProfileStream};
///
/// let listed = ProfileStream {
///     token: "profile-1".to_owned(),
///     name: "Main".to_owned(),
///     url: "rtsp://192.0.2.20/stream#main".to_owned(),
/// };
/// let Err(refused) = api::validate_streams(&[listed]) else {
///     return Err("a URL with a fragment is refused".into());
/// };
/// assert_eq!(
///     refused.to_string(),
///     "the daemon listed a stream URL that cannot carry a fragment"
/// );
/// # Ok::<(), Box<dyn Error>>(())
/// ```
pub fn validate_streams(streams: &[ProfileStream]) -> Result<(), ApiError> {
    require(!streams.is_empty(), Rule::NoUsableStream)?;
    require(streams.len() <= MAX_PROFILE_STREAMS, Rule::TooManyStreams)?;
    for stream in streams {
        require(
            is_valid_camera_text(&stream.token, MAX_LABEL_CHARS)
                && is_valid_camera_text(&stream.name, MAX_LABEL_CHARS),
            Rule::ProfileText,
        )?;
        if let Some(problem) = rtsp_url_problem(&stream.url) {
            return Err(Rule::ListedStreamUrl(problem).into());
        }
    }
    Ok(())
}

/// Checks the fragment timing of a VOD playlist asked for `window`: the
/// fragments start in order, and at least one of them overlaps the window.
///
/// # Errors
///
/// [`ApiError::Invalid`] with [`Rule::FragmentOrder`] for fragments out of
/// order, and with [`Rule::NoFragmentInWindow`] for none in the window.
///
/// # Example
///
/// ```
/// # use std::error::Error;
/// #
/// use iris_client::api::{self, Window};
/// use iris_client::vod::PlaybackFragment;
/// use iris_core::time::{MediaTicks, WallUs};
///
/// // One second of video from the start of the minute.
/// let fragments = [PlaybackFragment {
///     wall_start: WallUs::new(0),
///     duration: MediaTicks::from_secs(1),
/// }];
/// let minute = Window::try_new(WallUs::new(0), WallUs::new(60_000_000))?;
/// api::validate_playback_fragments(&fragments, minute)?;
/// let (from, to) = (WallUs::new(1_000_000), WallUs::new(2_000_000));
/// let later = Window::try_new(from, to)?;
/// let checked = api::validate_playback_fragments(&fragments, later);
/// let Err(refused) = checked else {
///     return Err("no fragment in the window is refused".into());
/// };
/// assert_eq!(
///     refused.to_string(),
///     "VOD playlist has no fragment in the requested window"
/// );
/// # Ok::<(), Box<dyn Error>>(())
/// ```
pub fn validate_playback_fragments(
    fragments: &[PlaybackFragment],
    window: Window,
) -> Result<(), ApiError> {
    let (from, to) = (window.from(), window.to());
    let mut previous: Option<WallUs> = None;
    let mut overlaps_window = false;
    for fragment in fragments {
        if let Some(previous) = previous {
            require(fragment.wall_start >= previous, Rule::FragmentOrder)?;
        }
        let start = i128::from(fragment.wall_start.get());
        // Ticks to microseconds as `MediaTicks::as_micros` rounds them (times
        // 100 / 9, toward negative infinity), in `i128`, where no tick count
        // overflows.
        let end = start + (i128::from(fragment.duration.get()) * 100).div_euclid(9);
        overlaps_window |= end > i128::from(from.get()) && start < i128::from(to.get());
        previous = Some(fragment.wall_start);
    }
    require(overlaps_window, Rule::NoFragmentInWindow)
}

/// Checks a segment listing for `window`, `[from, to)`, against the API's
/// overlap rule. A sealed row must span a positive interval that overlaps the
/// window. An open row needs only `start <= end` and `start < to`. The API
/// extends an open row to the time of the listing, or to a grace past its last
/// durable progress when that is earlier (`OPEN_SEGMENT_GRACE`,
/// `crates/iris-catalog/src/db.rs`). So its recorded end can precede `from`.
/// The check also takes an end equal to the start.
///
/// # Errors
///
/// [`ApiError::Invalid`] with [`Rule::TooManySegments`] for more than
/// [`MAX_SEGMENTS`] rows. For the first row that breaks a rule, it is
/// [`ApiError::Invalid`] with [`Rule::SegmentRange`] or
/// [`Rule::OpenSegmentRange`] for the row's span, and with
/// [`Rule::SegmentOutsideWindow`] for its place.
///
/// # Example
///
/// ```
/// # use std::error::Error;
/// #
/// use iris_client::api::{self, Segment, Window};
/// use iris_core::time::WallUs;
///
/// let window = Window::try_new(WallUs::new(100), WallUs::new(200))?;
/// // An open file whose camera went offline just before the window: the
/// // API lists it, and it recorded nothing in the window.
/// let stale = Segment {
///     start: WallUs::new(10),
///     end: WallUs::new(50),
///     sealed: false,
/// };
/// let recording = Segment {
///     start: WallUs::new(150),
///     end: WallUs::new(180),
///     sealed: false,
/// };
/// api::validate_segments(&[stale, recording], window)?;
/// let recorded = api::recorded_in_window(vec![stale, recording], window);
/// assert_eq!(recorded, [recording]);
/// # Ok::<(), Box<dyn Error>>(())
/// ```
pub fn validate_segments(segments: &[Segment], window: Window) -> Result<(), ApiError> {
    let (from, to) = (window.from(), window.to());
    require(segments.len() <= MAX_SEGMENTS, Rule::TooManySegments)?;
    for segment in segments {
        if segment.sealed {
            require(segment.start < segment.end, Rule::SegmentRange)?;
            require(
                segment.end > from && segment.start < to,
                Rule::SegmentOutsideWindow,
            )?;
        } else {
            require(segment.start <= segment.end, Rule::OpenSegmentRange)?;
            require(segment.start < to, Rule::SegmentOutsideWindow)?;
        }
    }
    Ok(())
}

/// Drops the open rows of `segments` that recorded nothing inside `window`. It
/// takes rows that [`validate_segments`] passed for that window, so each row it
/// keeps overlaps the window. When it drops every row, the window has nothing
/// recorded. The function reads the window's start alone: it keeps an open row
/// that starts at or after the window's end and has recorded something, which
/// `validate_segments` refuses. The example on [`validate_segments`] drops
/// one.
#[must_use]
pub fn recorded_in_window(mut segments: Vec<Segment>, window: Window) -> Vec<Segment> {
    let from = window.from();
    segments
        .retain(|segment| segment.sealed || (segment.end > from && segment.start < segment.end));
    segments
}

/// Checks one event as the API lists it: a kind that is a display string
/// ([`is_valid_display_string`]). Its id is positive, as every
/// [`EventId`](crate::api::EventId) is. The example on [`validate_events`]
/// checks a page of events.
///
/// # Errors
///
/// [`ApiError::Invalid`] with [`Rule::EventKindLabel`], "invalid event kind
/// in response", for a kind that is not a display string.
pub fn validate_event(event: &EventRecord) -> Result<(), ApiError> {
    require(
        is_valid_display_string(event.kind.label()),
        Rule::EventKindLabel,
    )
}

/// Checks a page of events asked for `window` with `limit`: `limit` is 1 to
/// [`MAX_EVENTS`], and the page holds at most `limit` events. Each event
/// passes [`validate_event`], falls inside the window, and carries an id that
/// no other event does.
///
/// # Errors
///
/// [`ApiError::Invalid`] with the first rule the page breaks:
/// [`Rule::EventPageLimit`], [`Rule::TooManyEvents`], then for each event in
/// turn [`Rule::EventKindLabel`], [`Rule::EventOutsideWindow`], and
/// [`Rule::DuplicateEventId`].
///
/// # Example
///
/// ```
/// # use std::error::Error;
/// #
/// use iris_client::api::{self, EventId, EventKind, EventRecord, Window};
/// use iris_core::time::WallUs;
///
/// let window = Window::try_new(WallUs::new(0), WallUs::new(60_000_000))?;
/// let motion = EventRecord {
///     id: EventId::try_new(7)?,
///     kind: EventKind::Named("motion".to_owned()),
///     nvr_time: WallUs::new(5_000_000),
///     retain_until: None,
/// };
/// api::validate_events(&[motion.clone()], window, 100)?;
/// let twice = [motion.clone(), motion];
/// let Err(refused) = api::validate_events(&twice, window, 100) else {
///     return Err("an id twice is refused".into());
/// };
/// assert_eq!(refused.to_string(), "duplicate event id in response");
/// # Ok::<(), Box<dyn Error>>(())
/// ```
pub fn validate_events(
    events: &[EventRecord],
    window: Window,
    limit: usize,
) -> Result<(), ApiError> {
    let (from, to) = (window.from(), window.to());
    require((1..=MAX_EVENTS).contains(&limit), Rule::EventPageLimit)?;
    require(events.len() <= limit, Rule::TooManyEvents)?;
    let mut ids = HashSet::with_capacity(events.len());
    for event in events {
        validate_event(event)?;
        require(
            event.nvr_time >= from && event.nvr_time < to,
            Rule::EventOutsideWindow,
        )?;
        require(ids.insert(event.id), Rule::DuplicateEventId)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::slice;

    use iris_core::id::CameraId;
    use iris_core::time::MediaTicks;

    use super::*;
    use crate::api::{EventId, EventKind};

    fn camera(id: &str) -> CameraId {
        CameraId::try_new(id).unwrap()
    }

    fn time(micros: i64) -> WallUs {
        WallUs::new(micros)
    }

    fn event_id(id: i64) -> EventId {
        EventId::try_new(id).unwrap()
    }

    /// The window `[from, to)` in microseconds.
    fn window(from: i64, to: i64) -> Window {
        Window::try_new(time(from), time(to)).unwrap()
    }

    /// The text of the refusal that `result` holds. A result that is no
    /// refusal fails the test at the line that asked for its text.
    #[track_caller]
    fn refusal<T: fmt::Debug>(result: Result<T, ApiError>) -> String {
        result.unwrap_err().to_string()
    }

    fn listed(id: &str, name: &str) -> Camera {
        Camera {
            id: camera(id),
            name: name.into(),
            provisioned: false,
            has_audio: false,
        }
    }

    #[test]
    fn a_camera_list_that_meets_every_rule_is_accepted() {
        assert!(validate_cameras(&[listed("cam1", "Front"), listed("cam2", "Back")]).is_ok());
    }

    #[test]
    fn duplicate_camera_ids_are_refused() {
        assert_eq!(
            refusal(validate_cameras(&[
                listed("cam1", "Front"),
                listed("cam1", "Duplicate")
            ])),
            "duplicate camera id in response"
        );
    }

    /// A bell is a control character and not whitespace, so the name is
    /// neither blank nor too long: the control rule alone refuses it.
    #[test]
    fn camera_names_with_control_characters_are_refused() {
        assert!(validate_cameras(&[listed("cam2", "Front")]).is_ok());
        assert_eq!(
            refusal(validate_cameras(&[listed("cam2", "Front\u{7}")])),
            "invalid camera name in response"
        );
    }

    /// The cameras have distinct ids and valid names, so the count is the one
    /// rule the longer list breaks.
    #[test]
    fn more_than_max_cameras_are_refused() {
        let cameras: Vec<Camera> = (0..=MAX_CAMERAS)
            .map(|n| listed(&format!("cam{n}"), "Front"))
            .collect();
        assert!(validate_cameras(&cameras[..MAX_CAMERAS]).is_ok());
        assert_eq!(
            refusal(validate_cameras(&cameras)),
            "too many cameras in response"
        );
    }

    /// A space is short and no control character, so blankness is the one
    /// rule the name breaks.
    #[test]
    fn a_blank_camera_name_is_refused() {
        assert_eq!(
            refusal(validate_cameras(&[listed("cam2", " ")])),
            "invalid camera name in response"
        );
    }

    #[test]
    fn a_camera_name_a_byte_past_the_limit_is_refused() {
        let long = "x".repeat(MAX_DISPLAY_STRING_BYTES + 1);
        assert_eq!(
            refusal(validate_cameras(&[listed("cam2", &long)])),
            "invalid camera name in response"
        );
    }

    /// Every camera of the longer list has a blank name.
    #[test]
    fn the_camera_count_is_checked_before_any_camera() {
        let cameras: Vec<Camera> = (0..=MAX_CAMERAS)
            .map(|n| listed(&format!("cam{n}"), " "))
            .collect();
        assert_eq!(
            refusal(validate_cameras(&cameras)),
            "too many cameras in response"
        );
    }

    /// The second camera repeats the id of the first and has a blank name.
    #[test]
    fn a_camera_name_is_checked_before_its_id() {
        let cameras = [listed("cam1", "Front"), listed("cam1", " ")];
        assert_eq!(
            refusal(validate_cameras(&cameras)),
            "invalid camera name in response"
        );
    }

    /// An empty string and strings of spaces alone: each is short and holds
    /// no control character, so blankness is the one rule it breaks.
    #[test]
    fn a_blank_display_string_is_refused() {
        assert!(is_valid_display_string("Front"));
        for blank in ["", " ", "   "] {
            assert!(!is_valid_display_string(blank), "{blank:?}");
        }
    }

    /// A display string may be as long as the byte limit, and not a byte more.
    #[test]
    fn a_display_string_past_the_byte_limit_is_refused() {
        let longest = "x".repeat(MAX_DISPLAY_STRING_BYTES);
        assert!(is_valid_display_string(&longest));
        assert!(!is_valid_display_string(&format!("{longest}x")));
    }

    #[test]
    fn a_display_string_holding_a_control_character_is_refused() {
        for broken in ["Front\n", "Front\u{7}"] {
            assert!(!is_valid_display_string(broken), "{broken:?}");
        }
    }

    #[test]
    fn a_window_keeps_the_instants_it_is_made_of() {
        let window = Window::try_new(time(5), time(10)).unwrap();
        assert_eq!((window.from(), window.to()), (time(5), time(10)));
    }

    /// A window of one microsecond is one; an empty one and a reversed one
    /// are not.
    #[test]
    fn a_window_that_does_not_end_after_it_starts_is_refused() {
        assert!(Window::try_new(time(0), time(1)).is_ok());
        for (from, to) in [(5, 5), (5, 4)] {
            assert_eq!(
                refusal(Window::try_new(time(from), time(to))),
                "invalid time range",
                "[{from}, {to})"
            );
        }
    }

    /// A window of seven days exactly is one, and one a microsecond longer is
    /// not.
    #[test]
    fn a_window_longer_than_seven_days_is_refused() {
        let week = i64::try_from(MAX_RANGE_US).unwrap();
        assert!(Window::try_new(time(0), time(week)).is_ok());
        assert_eq!(
            refusal(Window::try_new(time(0), time(week + 1))),
            "time range exceeds seven days"
        );
    }

    #[test]
    fn sealed_segments_outside_the_window_are_refused() {
        let outside = Segment {
            start: time(20),
            end: time(30),
            sealed: true,
        };
        assert_eq!(
            refusal(validate_segments(&[outside], window(0, 10))),
            "segment is outside the requested window"
        );
    }

    fn event(id: i64, nvr_time: i64) -> EventRecord {
        EventRecord {
            id: event_id(id),
            kind: EventKind::Named("motion".into()),
            nvr_time: time(nvr_time),
            retain_until: None,
        }
    }

    #[test]
    fn events_inside_the_window_are_accepted() {
        assert!(validate_events(&[event(1, 5)], window(0, 10), 1).is_ok());
    }

    #[test]
    fn events_outside_the_window_are_refused() {
        assert_eq!(
            refusal(validate_events(&[event(1, i64::MAX)], window(0, 10), 1)),
            "event is outside the requested window"
        );
    }

    #[test]
    fn an_event_belongs_to_the_window_from_its_start_to_before_its_end() {
        assert!(validate_events(&[event(1, 0), event(2, 9)], window(0, 10), 2).is_ok());
        for outside in [-1, 10] {
            assert_eq!(
                refusal(validate_events(&[event(1, outside)], window(0, 10), 1)),
                "event is outside the requested window",
                "{outside}"
            );
        }
    }

    #[test]
    fn an_event_page_limit_outside_one_to_the_maximum_is_refused() {
        assert!(validate_events(&[], window(0, 10), MAX_EVENTS).is_ok());
        for limit in [0, MAX_EVENTS + 1] {
            assert_eq!(
                refusal(validate_events(&[], window(0, 10), limit)),
                "invalid event page limit",
                "{limit}"
            );
        }
    }

    #[test]
    fn more_events_than_the_page_limit_are_refused() {
        assert_eq!(
            refusal(validate_events(
                &[event(1, 5), event(2, 6)],
                window(0, 10),
                1
            )),
            "too many events in response"
        );
    }

    #[test]
    fn duplicate_event_ids_are_refused() {
        assert_eq!(
            refusal(validate_events(
                &[event(1, 5), event(1, 6)],
                window(0, 10),
                2
            )),
            "duplicate event id in response"
        );
    }

    /// A limit of zero for a page of one event.
    #[test]
    fn the_event_page_limit_is_checked_before_the_count() {
        assert_eq!(
            refusal(validate_events(&[event(1, 5)], window(0, 10), 0)),
            "invalid event page limit"
        );
    }

    /// Two events for a limit of one, the first of a blank kind.
    #[test]
    fn the_event_count_is_checked_before_any_event() {
        let blank = EventRecord {
            kind: EventKind::Named(String::new()),
            ..event(1, 5)
        };
        assert_eq!(
            refusal(validate_events(&[blank, event(2, 6)], window(0, 10), 1)),
            "too many events in response"
        );
    }

    /// An event of a blank kind outside the window.
    #[test]
    fn an_event_kind_is_checked_before_its_time() {
        let blank = EventRecord {
            kind: EventKind::Named(String::new()),
            ..event(1, 50)
        };
        assert_eq!(
            refusal(validate_events(&[blank], window(0, 10), 1)),
            "invalid event kind in response"
        );
    }

    /// The second event repeats the id of the first and lies outside the
    /// window.
    #[test]
    fn an_event_time_is_checked_before_its_id() {
        assert_eq!(
            refusal(validate_events(
                &[event(1, 5), event(1, 50)],
                window(0, 10),
                2
            )),
            "event is outside the requested window"
        );
    }

    #[test]
    fn events_whose_kind_is_not_a_display_string_are_refused() {
        for kind in [
            EventKind::Named(String::new()),
            EventKind::Other {
                other: "NewFile\n".into(),
            },
        ] {
            let broken = EventRecord {
                kind,
                ..event(1, 5)
            };
            assert_eq!(
                refusal(validate_events(slice::from_ref(&broken), window(0, 10), 1)),
                "invalid event kind in response",
                "{broken:?}"
            );
        }
    }

    fn fragment(wall_start: i64) -> PlaybackFragment {
        PlaybackFragment {
            wall_start: time(wall_start),
            duration: MediaTicks::new(900),
        }
    }

    #[test]
    fn ordered_playback_fragments_in_the_window_are_accepted() {
        assert!(validate_playback_fragments(&[fragment(-10), fragment(10)], window(0, 20)).is_ok());
    }

    #[test]
    fn unordered_playback_fragments_are_refused() {
        assert_eq!(
            refusal(validate_playback_fragments(
                &[fragment(10), fragment(-10)],
                window(0, 20)
            )),
            "VOD fragment times are not ordered"
        );
    }

    #[test]
    fn playlists_without_a_fragment_in_the_window_are_refused() {
        assert_eq!(
            refusal(validate_playback_fragments(
                &[fragment(-10), fragment(10)],
                window(20_000, 30_000)
            )),
            "VOD playlist has no fragment in the requested window"
        );
    }

    #[test]
    fn a_duration_past_the_range_of_microseconds_is_read_without_overflow() {
        let endless = PlaybackFragment {
            wall_start: time(0),
            duration: MediaTicks::new(i64::MAX),
        };
        assert!(validate_playback_fragments(&[endless], window(1, 2)).is_ok());
    }

    #[test]
    fn a_fragment_ends_at_its_ticks_in_microseconds_rounded_down() {
        // Ten ticks are 111.1 µs, so the fragment from 0 ends at 111.
        let short = PlaybackFragment {
            wall_start: time(0),
            duration: MediaTicks::new(10),
        };
        assert!(validate_playback_fragments(&[short], window(110, 200)).is_ok());
        assert_eq!(
            refusal(validate_playback_fragments(&[short], window(111, 200))),
            "VOD playlist has no fragment in the requested window"
        );
    }

    #[test]
    fn a_fragment_starting_at_the_window_end_is_outside_it() {
        assert!(validate_playback_fragments(&[fragment(199)], window(100, 200)).is_ok());
        assert_eq!(
            refusal(validate_playback_fragments(
                &[fragment(200)],
                window(100, 200)
            )),
            "VOD playlist has no fragment in the requested window"
        );
    }

    fn row(start: i64, end: i64, sealed: bool) -> Segment {
        Segment {
            start: time(start),
            end: time(end),
            sealed,
        }
    }

    /// An open row that ends before the window, one whose end equals its
    /// start, and one that overlaps the window.
    #[test]
    fn open_rows_the_api_overlap_rule_admits_are_accepted() {
        let accepted = [
            row(10, 50, /* sealed */ false),
            row(150, 150, /* sealed */ false),
            row(90, 150, /* sealed */ false),
        ];
        assert!(validate_segments(&accepted, window(100, 200)).is_ok());
    }

    #[test]
    fn an_open_row_that_ends_before_it_starts_is_refused() {
        assert_eq!(
            refusal(validate_segments(
                &[row(60, 50, /* sealed */ false)],
                window(100, 200)
            )),
            "invalid open segment time range in response"
        );
    }

    /// An open row that starts at the window's end, and one after it.
    #[test]
    fn an_open_row_that_starts_at_or_after_the_window_end_is_refused() {
        for refused in [
            row(200, 200, /* sealed */ false),
            row(250, 260, /* sealed */ false),
        ] {
            assert_eq!(
                refusal(validate_segments(&[refused], window(100, 200))),
                "segment is outside the requested window",
                "{refused:?}"
            );
        }
    }

    #[test]
    fn a_sealed_row_without_a_positive_span_is_refused() {
        for refused in [
            row(150, 150, /* sealed */ true),
            row(160, 150, /* sealed */ true),
        ] {
            assert_eq!(
                refusal(validate_segments(&[refused], window(100, 200))),
                "invalid segment time range in response",
                "{refused:?}"
            );
        }
    }

    /// A sealed row that ends before the window, and one that starts at its
    /// end.
    #[test]
    fn a_sealed_row_before_or_after_the_window_is_refused() {
        for refused in [
            row(10, 50, /* sealed */ true),
            row(200, 260, /* sealed */ true),
        ] {
            assert_eq!(
                refusal(validate_segments(&[refused], window(100, 200))),
                "segment is outside the requested window",
                "{refused:?}"
            );
        }
    }

    #[test]
    fn open_rows_that_recorded_nothing_in_the_window_are_dropped() {
        let rows = vec![
            row(10, 50, /* sealed */ false),
            row(150, 150, /* sealed */ false),
            row(90, 150, /* sealed */ false),
            row(100, 120, /* sealed */ true),
        ];
        assert_eq!(
            recorded_in_window(rows, window(100, 200)),
            [
                row(90, 150, /* sealed */ false),
                row(100, 120, /* sealed */ true)
            ]
        );
    }

    #[test]
    fn a_sealed_row_ending_at_the_window_start_is_outside_it() {
        assert!(validate_segments(&[row(50, 101, /* sealed */ true)], window(100, 200)).is_ok());
        assert_eq!(
            refusal(validate_segments(
                &[row(50, 100, /* sealed */ true)],
                window(100, 200)
            )),
            "segment is outside the requested window"
        );
    }

    #[test]
    fn an_open_row_ending_at_the_window_start_recorded_nothing_in_it() {
        assert_eq!(
            recorded_in_window(
                vec![
                    row(50, 100, /* sealed */ false),
                    row(50, 101, /* sealed */ false)
                ],
                window(100, 200)
            ),
            [row(50, 101, /* sealed */ false)]
        );
    }

    #[test]
    fn more_than_max_segments_are_refused() {
        let most = vec![row(100, 150, /* sealed */ true); MAX_SEGMENTS];
        assert!(validate_segments(&most, window(100, 200)).is_ok());
        let more = vec![row(100, 150, /* sealed */ true); MAX_SEGMENTS + 1];
        assert_eq!(
            refusal(validate_segments(&more, window(100, 200))),
            "too many segments in response"
        );
    }

    /// Every row of the longer listing is a sealed row that ends before it
    /// starts.
    #[test]
    fn the_segment_count_is_checked_before_any_row() {
        let reversed = vec![row(160, 150, /* sealed */ true); MAX_SEGMENTS + 1];
        assert_eq!(
            refusal(validate_segments(&reversed, window(100, 200))),
            "too many segments in response"
        );
    }

    /// A sealed row after the window that ends before it starts.
    #[test]
    fn a_sealed_row_span_is_checked_before_its_place() {
        assert_eq!(
            refusal(validate_segments(
                &[row(260, 250, /* sealed */ true)],
                window(100, 200)
            )),
            "invalid segment time range in response"
        );
    }

    /// An open row after the window that ends before it starts.
    #[test]
    fn an_open_row_span_is_checked_before_its_place() {
        assert_eq!(
            refusal(validate_segments(
                &[row(260, 250, /* sealed */ false)],
                window(100, 200)
            )),
            "invalid open segment time range in response"
        );
    }

    fn device(host: &str) -> DiscoveredDevice {
        DiscoveredDevice {
            endpoint: "urn:uuid:1".into(),
            host: host.into(),
            port: 80,
            https: false,
            name: Some("Garage".into()),
            hardware: None,
            location: None,
        }
    }

    #[test]
    fn devices_the_daemon_could_list_are_accepted() {
        let unnamed = DiscoveredDevice {
            endpoint: String::new(),
            name: None,
            ..device("2001:db8::7")
        };
        assert!(validate_devices(&[device("192.0.2.5"), unnamed]).is_ok());
    }

    #[test]
    fn devices_with_an_unusable_host_are_refused() {
        for host in ["", " ", "a b", "x\n", &"h".repeat(MAX_HOST_BYTES + 1)] {
            assert_eq!(
                refusal(validate_devices(&[device(host)])),
                "the daemon listed a device with an invalid host",
                "{host:?}"
            );
        }
    }

    /// A name past the limit, a hardware scope with a control character, and
    /// an empty location.
    #[test]
    fn a_device_with_an_invalid_scope_is_refused() {
        for broken in [
            DiscoveredDevice {
                name: Some("n".repeat(MAX_LABEL_CHARS + 1)),
                ..device("192.0.2.5")
            },
            DiscoveredDevice {
                hardware: Some("IPC\u{1b}[31m".into()),
                ..device("192.0.2.5")
            },
            DiscoveredDevice {
                location: Some(String::new()),
                ..device("192.0.2.5")
            },
        ] {
            assert_eq!(
                refusal(validate_devices(slice::from_ref(&broken))),
                "the daemon listed a device with an invalid scope",
                "{broken:?}"
            );
        }
    }

    /// An endpoint reference past the limit, and one with a control
    /// character.
    #[test]
    fn a_device_with_an_invalid_endpoint_reference_is_refused() {
        for endpoint in [
            "e".repeat(MAX_ENDPOINT_CHARS + 1),
            "urn:uuid:\u{7}".to_owned(),
        ] {
            let broken = DiscoveredDevice {
                endpoint,
                ..device("192.0.2.5")
            };
            assert_eq!(
                refusal(validate_devices(slice::from_ref(&broken))),
                "the daemon listed a device with an invalid endpoint reference",
                "{broken:?}"
            );
        }
    }

    #[test]
    fn more_devices_than_one_probe_reports_are_refused() {
        assert_eq!(
            refusal(validate_devices(&vec![
                device("192.0.2.5");
                MAX_DISCOVERED_DEVICES + 1
            ])),
            "the daemon listed more devices than one probe reports"
        );
    }

    #[test]
    fn the_most_devices_one_probe_reports_are_accepted() {
        let most = vec![device("192.0.2.5"); MAX_DISCOVERED_DEVICES];
        assert!(validate_devices(&most).is_ok());
    }

    #[test]
    fn a_device_host_as_long_as_the_limit_is_accepted() {
        let longest = "h".repeat(MAX_HOST_BYTES);
        assert!(validate_devices(&[device(&longest)]).is_ok());
    }

    #[test]
    fn a_device_endpoint_reference_as_long_as_the_limit_is_accepted() {
        let longest = DiscoveredDevice {
            endpoint: "e".repeat(MAX_ENDPOINT_CHARS),
            ..device("192.0.2.5")
        };
        assert!(validate_devices(&[longest]).is_ok());
    }

    #[test]
    fn a_device_scope_as_long_as_the_limit_is_accepted() {
        let longest = DiscoveredDevice {
            name: Some("n".repeat(MAX_LABEL_CHARS)),
            ..device("192.0.2.5")
        };
        assert!(validate_devices(&[longest]).is_ok());
    }

    /// Every device of the longer list has an empty host.
    #[test]
    fn the_device_count_is_checked_before_any_device() {
        let devices = vec![device(""); MAX_DISCOVERED_DEVICES + 1];
        assert_eq!(
            refusal(validate_devices(&devices)),
            "the daemon listed more devices than one probe reports"
        );
    }

    /// A device with an empty host and a control character in its endpoint
    /// reference.
    #[test]
    fn a_device_host_is_checked_before_its_endpoint_reference() {
        let broken = DiscoveredDevice {
            endpoint: "urn:uuid:\u{7}".into(),
            ..device("")
        };
        assert_eq!(
            refusal(validate_devices(&[broken])),
            "the daemon listed a device with an invalid host"
        );
    }

    /// A device with a control character in its endpoint reference and an
    /// empty location.
    #[test]
    fn a_device_endpoint_reference_is_checked_before_its_scopes() {
        let broken = DiscoveredDevice {
            endpoint: "urn:uuid:\u{7}".into(),
            location: Some(String::new()),
            ..device("192.0.2.5")
        };
        assert_eq!(
            refusal(validate_devices(&[broken])),
            "the daemon listed a device with an invalid endpoint reference"
        );
    }

    fn profile(url: &str) -> ProfileStream {
        ProfileStream {
            token: "profile-1".into(),
            name: "Main".into(),
            url: url.into(),
        }
    }

    #[test]
    fn a_listed_stream_url_without_a_problem_is_accepted() {
        assert!(validate_streams(&[profile("rtsp://192.0.2.20:554/live/1")]).is_ok());
    }

    /// A listed stream URL is refused for each problem `rtsp_url_problem`
    /// finds, which completes the refusal's sentence.
    #[test]
    fn a_listed_stream_url_with_a_problem_is_refused() {
        let credentials = "cannot carry credentials; enter them as the username and password";
        let too_long = format!("rtsp://192.0.2.20/{}", "x".repeat(MAX_STREAM_URL_BYTES));
        for (url, problem) in [
            ("not a url", "is not a URL"),
            ("http://192.0.2.20/live", "must start with rtsp://"),
            ("rtsp:live", "names no host"),
            ("rtsp://viewer:secret@192.0.2.20/live", credentials),
            ("rtsp://viewer@192.0.2.20/live", credentials),
            ("rtsp://192.0.2.20/live#x", "cannot carry a fragment"),
            (&too_long, "is longer than 2048 bytes"),
        ] {
            assert_eq!(
                refusal(validate_streams(&[profile(url)])),
                format!("the daemon listed a stream URL that {problem}"),
                "{url}"
            );
        }
    }

    #[test]
    fn a_stream_list_of_no_profile_is_refused() {
        assert_eq!(
            refusal(validate_streams(&[])),
            "the daemon listed no usable stream"
        );
    }

    #[test]
    fn a_stream_list_of_the_most_profiles_a_lookup_lists_is_accepted() {
        let most = vec![profile("rtsp://192.0.2.20/1"); MAX_PROFILE_STREAMS];
        assert!(validate_streams(&most).is_ok());
    }

    #[test]
    fn more_profiles_than_a_lookup_lists_are_refused() {
        let more = vec![profile("rtsp://192.0.2.20/1"); MAX_PROFILE_STREAMS + 1];
        assert_eq!(
            refusal(validate_streams(&more)),
            "the daemon listed more than 8 streams"
        );
    }

    /// A name with a control character, and a token with one.
    #[test]
    fn profiles_with_control_characters_in_their_text_are_refused() {
        for broken in [
            ProfileStream {
                name: "Main\n".into(),
                ..profile("rtsp://192.0.2.20/1")
            },
            ProfileStream {
                token: "profile\u{7}".into(),
                ..profile("rtsp://192.0.2.20/1")
            },
        ] {
            assert_eq!(
                refusal(validate_streams(slice::from_ref(&broken))),
                "the daemon listed a profile with an invalid token or name",
                "{broken:?}"
            );
        }
    }

    /// A token a character past the limit, and a name a character past it.
    #[test]
    fn profiles_with_text_past_the_limit_are_refused() {
        let long = "p".repeat(MAX_LABEL_CHARS + 1);
        for broken in [
            ProfileStream {
                token: long.clone(),
                ..profile("rtsp://192.0.2.20/1")
            },
            ProfileStream {
                name: long.clone(),
                ..profile("rtsp://192.0.2.20/1")
            },
        ] {
            assert_eq!(
                refusal(validate_streams(slice::from_ref(&broken))),
                "the daemon listed a profile with an invalid token or name",
                "{broken:?}"
            );
        }
    }

    #[test]
    fn profile_labels_as_long_as_the_limit_are_accepted() {
        let longest = ProfileStream {
            token: "p".repeat(MAX_LABEL_CHARS),
            name: "n".repeat(MAX_LABEL_CHARS),
            ..profile("rtsp://192.0.2.20/1")
        };
        assert!(validate_streams(&[longest]).is_ok());
    }

    /// Every profile of the longer list has a control character in its name.
    #[test]
    fn the_stream_count_is_checked_before_any_profile() {
        let broken = ProfileStream {
            name: "Main\n".into(),
            ..profile("rtsp://192.0.2.20/1")
        };
        assert_eq!(
            refusal(validate_streams(&vec![broken; MAX_PROFILE_STREAMS + 1])),
            "the daemon listed more than 8 streams"
        );
    }

    /// A profile with a control character in its name and a fragment in its
    /// URL.
    #[test]
    fn a_profile_text_is_checked_before_its_url() {
        let broken = ProfileStream {
            name: "Main\n".into(),
            ..profile("rtsp://192.0.2.20/1#x")
        };
        assert_eq!(
            refusal(validate_streams(&[broken])),
            "the daemon listed a profile with an invalid token or name"
        );
    }

    /// Three thousand bytes that do not parse.
    #[test]
    fn a_stream_url_is_measured_before_it_is_parsed() {
        assert_eq!(
            rtsp_url_problem(&"x".repeat(3_000)),
            Some(StreamUrlProblem::TooLong)
        );
    }

    /// A URL of another scheme without a host.
    #[test]
    fn a_stream_url_scheme_is_checked_before_its_host() {
        assert_eq!(
            rtsp_url_problem("data:x"),
            Some(StreamUrlProblem::OtherScheme)
        );
    }

    /// A URL with userinfo and a fragment.
    #[test]
    fn a_stream_url_userinfo_is_checked_before_its_fragment() {
        assert_eq!(
            rtsp_url_problem("rtsp://u@192.0.2.1/x#f"),
            Some(StreamUrlProblem::Userinfo)
        );
    }

    #[test]
    fn a_stream_url_is_too_long_only_past_the_limit() {
        let prefix = "rtsp://192.0.2.20/";
        let longest = format!(
            "{prefix}{}",
            "x".repeat(MAX_STREAM_URL_BYTES - prefix.len())
        );
        assert_eq!(rtsp_url_problem(&longest), None);
        assert_eq!(
            rtsp_url_problem(&format!("{longest}x")),
            Some(StreamUrlProblem::TooLong)
        );
    }

    #[test]
    fn each_stream_url_problem_completes_a_sentence_that_names_the_url() {
        let credentials = "cannot carry credentials; enter them as the username and password";
        let too_long = format!("rtsp://192.0.2.20/{}", "x".repeat(MAX_STREAM_URL_BYTES));
        for (url, problem) in [
            ("not a url", "is not a URL"),
            ("http://192.0.2.20/live", "must start with rtsp://"),
            ("rtsp:live", "names no host"),
            ("rtsp://viewer:secret@192.0.2.20/live", credentials),
            ("rtsp://viewer@192.0.2.20/live", credentials),
            ("rtsp://192.0.2.20/live#x", "cannot carry a fragment"),
            (&too_long, "is longer than 2048 bytes"),
        ] {
            assert_eq!(
                rtsp_url_problem(url)
                    .map(|found| found.to_string())
                    .as_deref(),
                Some(problem),
                "{url}"
            );
        }
    }
}
