//! The requests a viewer sends to the irisd API, built without a transport. A
//! builder is a method of [`ApiOrigin`] named as the call. It gives the
//! method, the URL on the origin, and the JSON body of a call that sends one.
//! It also gives how long the transport waits for the answer to progress. A
//! builder refuses a request that breaks a rule before anything is sent.
//!
//! The transport sends the request with the bearer, which never enters this
//! crate. Once the answer's head is in, [`HttpRequest::body_to_read`] says
//! what to read of its body. The transport then hands what it holds to the
//! call's check in [`answer`](crate::api::answer). Both decide by the same
//! rules of the call, so the check never needs a body the transport did not
//! read.

use std::fmt;
use std::time::Duration;

use iris_core::id::{CameraId, StreamKind};
use iris_core::time::WallUs;
use url::Url;

use crate::api::{
    ApiError, Call, DISCOVERY_WAIT_MS, EventId, MAX_CAMERA_BODY_BYTES, MAX_CAMERAS_BODY_BYTES,
    MAX_DEVICES_BODY_BYTES, MAX_ERROR_BODY_BYTES, MAX_EVENTS, MAX_KEYFRAME_BYTES,
    MAX_LIST_BODY_BYTES, MAX_RETAIN_BODY_BYTES, MAX_STREAMS_BODY_BYTES, NewCamera, Retention, Rule,
    StreamUrlsRequest, Window, rtsp_url_problem, types, validate,
};
use crate::origin::ApiOrigin;
use crate::vod::MAX_VOD_PLAYLIST_BYTES;

/// How long a viewer waits for an answer's head or for its next body bytes.
pub const READ_IDLE: Duration = Duration::from_secs(30);
/// How long a viewer waits for a stream lookup's answer. The daemon answers
/// when the lookup ends, which can take up to its 60 s bound
/// (`iris_events::media::MEDIA_LOOKUP_DEADLINE`); the 10 s more cover the
/// request itself.
pub const STREAM_LOOKUP_READ_IDLE: Duration = Duration::from_secs(70);

/// The HTTP method of a request of the irisd API. The enum is
/// `#[non_exhaustive]`, so a match outside the crate needs a wildcard arm; a
/// transport converts a method into `http::Method` instead.
///
/// # Example
///
/// ```
/// use iris_client::api::Method;
///
/// assert_eq!(Method::Post.to_string(), "POST");
/// // A transport on the `http` crate sends the request with its method.
/// assert_eq!(http::Method::from(Method::Post), http::Method::POST);
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Method {
    /// Reads what the call names: a list, a keyframe, a VOD playlist, an
    /// export, or a discovery.
    Get,
    /// Sends a JSON body: a camera to add, a retain mark, or a stream lookup.
    Post,
    /// Removes a camera the API added.
    Delete,
}

/// The method as a request line names it: `GET`, `POST`, or `DELETE`.
impl fmt::Display for Method {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Get => "GET",
            Self::Post => "POST",
            Self::Delete => "DELETE",
        })
    }
}

/// The method of the same name in the `http` crate, `http::Method`. A
/// transport built on that crate, such as one on reqwest, sends a request with
/// it.
impl From<Method> for http::Method {
    fn from(method: Method) -> Self {
        match method {
            Method::Get => Self::GET,
            Method::Post => Self::POST,
            Method::Delete => Self::DELETE,
        }
    }
}

/// What a transport reads of an answer's body before the call's check, as
/// [`HttpRequest::body_to_read`] decides from the answer's head. A transport
/// holds a body only whole: it hands the check `Some` with all of it, or
/// `None`. The example on [`HttpRequest`] reads each.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum BodyRead {
    /// Read none of the body before the check, and hand the check `None`: the
    /// check decides from the head alone. The transport reads no byte of the
    /// body, and does not read and discard it. Once the check is done, it
    /// closes the answer, or, for an export the check passes, streams the body
    /// to its destination.
    Nothing,
    /// The body holds the daemon's message for a status the call does not
    /// expect. Hand the check the whole body when it is at most `max_bytes`.
    /// When it is longer, known from a `Content-Length` above `max_bytes` or
    /// from a byte past it, stop reading and hand the check `None`. A
    /// failure to read it hands `None` too: the refusal stands without a
    /// message.
    Message {
        /// The most bytes held: [`MAX_ERROR_BODY_BYTES`].
        max_bytes: u64,
    },
    /// The body holds the call's value. Hand the check the whole body when it
    /// is at most `max_bytes`. When it is longer, known from a
    /// `Content-Length` above `max_bytes` or from a byte past it, stop
    /// reading and hand the check `None`, which it refuses as too large. A
    /// failure to read it fails the call with the transport's error.
    Value {
        /// The most bytes held: the call's limit,
        /// [`HttpRequest::max_body_bytes`].
        max_bytes: u64,
    },
}

/// One request to the irisd API as a transport sends it: the call, the
/// method, the URL on the API origin, and the JSON body of a call that sends
/// one. It also gives how long to wait for the answer and, once its head is
/// in, what to read of its body ([`HttpRequest::body_to_read`]). A builder of
/// [`ApiOrigin`] makes each one. The method, the byte limit, and the read
/// deadline are the call's, so no part of a request disagrees with
/// [`HttpRequest::body_to_read`] or with the call's check. The URL never
/// carries the bearer, which the transport sends in `Authorization`.
///
/// Its [`Debug`](fmt::Debug) output shows the body's length and not its
/// bytes, which carry a camera's password for [`ApiOrigin::add_camera`] and
/// [`ApiOrigin::stream_urls`]. It shows the URL with its userinfo, query, and
/// fragment each as `<redacted>` and its path as it stands, so a builder's
/// window and limit show redacted too. A URL without a host shows as
/// `<redacted>` whole.
///
/// # Example
///
/// ```
/// # use std::error::Error;
/// #
/// use iris_client::api::{self, BodyRead, Call, EventId, Method, Retention};
/// use iris_client::origin::{ApiOrigin, PlainHttp};
/// use iris_core::id::{CameraId, StreamKind};
/// use iris_core::time::WallUs;
///
/// let address = "https://nvr.example:8443";
/// let origin = ApiOrigin::parse(address, PlainHttp::LoopbackOnly)?;
/// let release = origin.retain_event(EventId::try_new(7)?, Retention::Release);
/// assert_eq!(release.call(), Call::RetainEvent);
/// assert_eq!(release.method(), Method::Post);
/// assert_eq!(release.url().path(), "/api/events/7/retain");
/// // A transport sends the body as `application/json`.
/// assert_eq!(release.body(), Some(br#"{"until":null}"#.as_slice()));
/// assert_eq!(release.read_idle(), api::READ_IDLE);
///
/// let camera = CameraId::try_new("cam1")?;
/// let time = WallUs::new(1_000_000);
/// let keyframe = origin.keyframe(&camera, StreamKind::Sub, time);
/// assert_eq!(
///     keyframe.url().as_str(),
///     "https://nvr.example:8443/api/streams/cam1/sub/keyframe?t=1000000"
/// );
/// // The answer's head decides what a transport reads of its body.
/// let value = BodyRead::Value {
///     max_bytes: keyframe.max_body_bytes(),
/// };
/// assert_eq!(keyframe.body_to_read(200, Some("video/mp4")), value);
/// let html = keyframe.body_to_read(200, Some("text/html"));
/// assert_eq!(html, BodyRead::Nothing);
/// // A keyframe's 404 says that nothing is recorded there.
/// assert_eq!(keyframe.body_to_read(404, None), BodyRead::Nothing);
/// // A status the call does not expect is read for the daemon's message.
/// let message = BodyRead::Message {
///     max_bytes: u64::try_from(api::MAX_ERROR_BODY_BYTES)?,
/// };
/// assert_eq!(
///     keyframe.body_to_read(502, Some("application/json")),
///     message
/// );
/// # Ok::<(), Box<dyn Error>>(())
/// ```
#[derive(Clone, PartialEq, Eq, Hash)]
#[must_use]
pub struct HttpRequest {
    /// The call the request makes.
    call: Call,
    /// The URL, on the API origin.
    url: Url,
    /// The JSON body; `None` for a call that sends none.
    body: Option<Vec<u8>>,
}

impl HttpRequest {
    /// The call the request makes, which names it in refusals, whose rules
    /// decide what [`HttpRequest::body_to_read`] reads, and whose check in
    /// [`answer`](crate::api::answer) takes the answer. The example on
    /// [`HttpRequest`] reads it.
    #[must_use]
    pub fn call(&self) -> Call {
        self.call
    }

    /// The call's method: `POST` to add a camera, retain an event, or look up
    /// a camera's streams, `DELETE` to remove a camera, and `GET` for the
    /// rest. The example on [`HttpRequest`] reads it.
    #[must_use]
    pub fn method(&self) -> Method {
        self.call.method()
    }

    /// The URL on the API origin, with the query of a call that names one.
    /// The example on [`HttpRequest`] reads it.
    #[must_use]
    pub fn url(&self) -> &Url {
        &self.url
    }

    /// The body, which is always JSON, so a transport sends it with
    /// `Content-Type: application/json`; `None` for a call that sends none.
    /// The example on [`HttpRequest`] reads it.
    #[must_use]
    pub fn body(&self) -> Option<&[u8]> {
        self.body.as_deref()
    }

    /// The call's byte limit: the most bytes of its value a transport holds,
    /// the limit of [`BodyRead::Value`]. Zero for an export and a removal,
    /// whose checks take no body. The example on [`HttpRequest`] reads it.
    #[must_use]
    pub fn max_body_bytes(&self) -> u64 {
        self.call.max_body_bytes() as u64
    }

    /// How long the transport waits for the answer's head or its next body
    /// bytes: [`READ_IDLE`], and [`STREAM_LOOKUP_READ_IDLE`] for a stream
    /// lookup. The example on [`HttpRequest`] reads it.
    #[must_use]
    pub fn read_idle(&self) -> Duration {
        self.call.read_idle()
    }

    /// What a transport reads of the body of an answer whose head has
    /// `status` and `content_type`, by the rules the call's check applies.
    /// `status` is the HTTP status code as the transport received it, and any
    /// value is read. A keyframe's 404 is read no further than its head; any
    /// other status the call does not expect, 0 or 1000 among them, is read for
    /// its message. `content_type` is the `Content-Type` header's value, `None`
    /// when there is none that is text. The example on [`HttpRequest`] reads
    /// each answer below:
    ///
    /// - a keyframe's 404, which says nothing is recorded there:
    ///   [`BodyRead::Nothing`];
    /// - any other status the call does not expect (any `2xx`, but 201 alone
    ///   for an added camera, 204 alone for a removal, and 200 alone for an
    ///   export): [`BodyRead::Message`], up to [`MAX_ERROR_BODY_BYTES`];
    /// - a keyframe or an export whose content type is not `video/mp4`:
    ///   [`BodyRead::Nothing`], since the check refuses it from its head;
    /// - an export's 200 and a removal's 204, whose checks take no body:
    ///   [`BodyRead::Nothing`];
    /// - any other answer: [`BodyRead::Value`], up to the call's limit.
    #[must_use]
    pub fn body_to_read(&self, status: u16, content_type: Option<&str>) -> BodyRead {
        match self.call.head(status, content_type) {
            Head::Refused => BodyRead::Message {
                max_bytes: MAX_ERROR_BODY_BYTES as u64,
            },
            Head::NothingRecorded | Head::OtherMediaType(_) | Head::Whole => BodyRead::Nothing,
            Head::Value => BodyRead::Value {
                max_bytes: self.max_body_bytes(),
            },
        }
    }

    /// Sets the URL's query to `pairs`, in order.
    fn set_query(&mut self, pairs: &[(&str, String)]) {
        self.url.query_pairs_mut().clear().extend_pairs(pairs);
    }

    /// Sets the URL's query to the window's ends, as `from` and `to`.
    fn set_window(&mut self, window: Window) {
        self.set_query(&[
            ("from", window.from().get().to_string()),
            ("to", window.to().get().to_string()),
        ]);
    }

    /// Sets the body to `body`, a JSON document.
    fn set_body(&mut self, body: Vec<u8>) {
        self.body = Some(body);
    }
}

impl fmt::Debug for HttpRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HttpRequest")
            .field("call", &self.call)
            .field("method", &self.method())
            .field("url", &types::shown_url(&self.url))
            .field("body_bytes", &self.body.as_ref().map(Vec::len))
            .field("max_body_bytes", &self.max_body_bytes())
            .field("read_idle", &self.read_idle())
            .finish()
    }
}

/// How a call's rules sort an answer by its head: what
/// [`HttpRequest::body_to_read`] reads and what the check makes of the head
/// both follow from it, so the two never disagree.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Head {
    /// A status the call does not expect: the check refuses it, with the
    /// daemon's message from the body.
    Refused,
    /// A content type other than the media type the call answers with: the
    /// check refuses it.
    OtherMediaType(&'static str),
    /// A keyframe's 404: the check answers that nothing is recorded there.
    NothingRecorded,
    /// The head of an export's or a removal's success, which is the whole
    /// answer: the check takes no body.
    Whole,
    /// The head of an answer whose body holds the call's value, which the
    /// check reads.
    Value,
}

/// Whether the value of a `Content-Type` header names `expected`: the media
/// type before any `;` parameter, trimmed, matches it without regard to ASCII
/// case. `None`, no header that is text, names none.
fn names_media_type(content_type: Option<&str>, expected: &str) -> bool {
    content_type
        .and_then(|value| value.split(';').next())
        .map(str::trim)
        .is_some_and(|media_type| media_type.eq_ignore_ascii_case(expected))
}

impl Call {
    /// How the call's rules sort an answer whose head has `status` and
    /// `content_type`. They look at a keyframe's 404 first, then a status the
    /// call does not expect, then the content type, and last whether the check
    /// reads a body.
    pub(super) fn head(self, status: u16, content_type: Option<&str>) -> Head {
        if self == Self::Keyframe && status == 404 {
            return Head::NothingRecorded;
        }
        if !self.expects(status) {
            return Head::Refused;
        }
        if let Some(expected) = self.media_type()
            && !names_media_type(content_type, expected)
        {
            return Head::OtherMediaType(expected);
        }
        match self {
            Self::Export | Self::RemoveCamera => Head::Whole,
            Self::Cameras
            | Self::AddCamera
            | Self::Segments
            | Self::Events
            | Self::RetainEvent
            | Self::Keyframe
            | Self::VodPlaylist
            | Self::Discover
            | Self::StreamUrls => Head::Value,
        }
    }

    /// The media type the call answers with, which its check requires the
    /// content type to name: `video/mp4` for a keyframe and an export, and
    /// none for a call whose answer is JSON or a playlist.
    fn media_type(self) -> Option<&'static str> {
        match self {
            Self::Keyframe | Self::Export => Some("video/mp4"),
            Self::Cameras
            | Self::AddCamera
            | Self::RemoveCamera
            | Self::Segments
            | Self::Events
            | Self::RetainEvent
            | Self::VodPlaylist
            | Self::Discover
            | Self::StreamUrls => None,
        }
    }

    /// Whether an answer to the call with `status` carries its value: any
    /// `2xx`, but 201 alone for an added camera, 204 alone for a removal, and
    /// 200 alone for an export. The checks refuse any other status.
    fn expects(self, status: u16) -> bool {
        match self {
            Self::AddCamera => status == 201,
            Self::RemoveCamera => status == 204,
            Self::Export => status == 200,
            Self::Cameras
            | Self::Segments
            | Self::Events
            | Self::RetainEvent
            | Self::Keyframe
            | Self::VodPlaylist
            | Self::Discover
            | Self::StreamUrls => (200..=299).contains(&status),
        }
    }

    /// The most bytes of the call's answer that a viewer holds
    /// ([`HttpRequest::max_body_bytes`]); the checks refuse a longer body.
    pub(super) fn max_body_bytes(self) -> usize {
        match self {
            Self::Cameras => MAX_CAMERAS_BODY_BYTES,
            Self::AddCamera => MAX_CAMERA_BODY_BYTES,
            Self::RemoveCamera | Self::Export => 0,
            Self::Segments | Self::Events => MAX_LIST_BODY_BYTES,
            Self::RetainEvent => MAX_RETAIN_BODY_BYTES,
            Self::Keyframe => MAX_KEYFRAME_BYTES,
            Self::VodPlaylist => MAX_VOD_PLAYLIST_BYTES,
            Self::Discover => MAX_DEVICES_BODY_BYTES,
            Self::StreamUrls => MAX_STREAMS_BODY_BYTES,
        }
    }

    /// How long a viewer waits for the call's answer to progress
    /// ([`HttpRequest::read_idle`]).
    fn read_idle(self) -> Duration {
        match self {
            Self::StreamUrls => STREAM_LOOKUP_READ_IDLE,
            Self::Cameras
            | Self::AddCamera
            | Self::RemoveCamera
            | Self::Segments
            | Self::Events
            | Self::Export
            | Self::RetainEvent
            | Self::Keyframe
            | Self::VodPlaylist
            | Self::Discover => READ_IDLE,
        }
    }

    /// The method of the call's request ([`HttpRequest::method`]).
    fn method(self) -> Method {
        match self {
            Self::AddCamera | Self::RetainEvent | Self::StreamUrls => Method::Post,
            Self::RemoveCamera => Method::Delete,
            Self::Cameras
            | Self::Segments
            | Self::Events
            | Self::Export
            | Self::Keyframe
            | Self::VodPlaylist
            | Self::Discover => Method::Get,
        }
    }
}

/// The request builders, one for each call, named as the call. The examples
/// on [`ApiOrigin`] and [`HttpRequest`] build them.
impl ApiOrigin {
    /// A request of `call` at `path` on the origin, without a query or a body.
    fn request(&self, call: Call, path: &str) -> HttpRequest {
        let mut url = self.url().clone();
        url.set_path(path);
        HttpRequest {
            call,
            url,
            body: None,
        }
    }

    /// `GET /api/cameras`: the cameras the daemon runs, which
    /// [`answer::cameras`](crate::api::answer::cameras) checks. The example
    /// on [`ApiOrigin`] builds it.
    pub fn cameras(&self) -> HttpRequest {
        self.request(Call::Cameras, "/api/cameras")
    }

    /// `POST /api/cameras` with `camera` as its body: the daemon starts
    /// recording the camera and answers 201 with it as the list shows it,
    /// which [`answer::add_camera`](crate::api::answer::add_camera) checks.
    /// The camera's stream URLs are checked with [`rtsp_url_problem`] first,
    /// the main URL before the sub URL, so no URL with userinfo enters the
    /// body. A URL that passes enters it as it stands, its query included,
    /// which a camera may need and which may hold its password. The request's
    /// [`Debug`](fmt::Debug) shows the body's length alone, and that of
    /// [`NewCamera`] shows the query as `?<redacted>`. The example on
    /// [`ApiOrigin`] asks it for a camera whose URL carries userinfo, and it
    /// refuses the camera.
    ///
    /// # Errors
    ///
    /// [`ApiError::Invalid`] for a stream URL that [`rtsp_url_problem`] finds
    /// a problem with. It holds the problem in the sentence the Linux viewer's
    /// form shows for it: [`Rule::RtspUrl`], "the RTSP URL {problem}", for
    /// [`rtsp_url`](NewCamera::rtsp_url), and [`Rule::SubUrl`], "the sub URL
    /// {problem}", for [`sub_url`](NewCamera::sub_url).
    ///
    /// # Panics
    ///
    /// Never: a [`NewCamera`] holds only strings and unit enums, which
    /// `serde_json` always encodes.
    pub fn add_camera(&self, camera: &NewCamera) -> Result<HttpRequest, ApiError> {
        if let Some(problem) = rtsp_url_problem(&camera.rtsp_url) {
            return Err(Rule::RtspUrl(problem).into());
        }
        if let Some(problem) = camera.sub_url.as_deref().and_then(rtsp_url_problem) {
            return Err(Rule::SubUrl(problem).into());
        }
        let body =
            serde_json::to_vec(camera).expect("a NewCamera of strings and unit enums encodes");
        let mut request = self.request(Call::AddCamera, "/api/cameras");
        request.set_body(body);
        Ok(request)
    }

    /// `DELETE /api/cameras/{camera}`: the daemon stops and forgets a camera
    /// the API added and answers 204, which
    /// [`answer::remove_camera`](crate::api::answer::remove_camera) checks.
    /// The example on [`ApiOrigin`] builds it.
    pub fn remove_camera(&self, camera: &CameraId) -> HttpRequest {
        self.request(Call::RemoveCamera, &format!("/api/cameras/{camera}"))
    }

    /// `GET /api/streams/{camera}/{kind}/segments?from&to`: the segment rows
    /// of one stream that overlap `window` by the API's rule, which
    /// [`answer::segments`](crate::api::answer::segments) checks. The example
    /// on [`ApiOrigin`] builds it.
    pub fn segments(&self, camera: &CameraId, kind: StreamKind, window: Window) -> HttpRequest {
        let mut request = self.request(
            Call::Segments,
            &format!("/api/streams/{camera}/{kind}/segments"),
        );
        request.set_window(window);
        request
    }

    /// `GET /api/events?camera&from&to&limit`: one camera's events inside
    /// `window`, oldest first, at most `limit` of them, which
    /// [`answer::events`](crate::api::answer::events) checks. The example on
    /// [`ApiOrigin`] asks it for a page of no events, and it refuses the page.
    ///
    /// # Errors
    ///
    /// [`ApiError::Invalid`] with [`Rule::EventPageLimit`], "invalid event page
    /// limit", when `limit` is not 1 to [`MAX_EVENTS`].
    pub fn events(
        &self,
        camera: &CameraId,
        window: Window,
        limit: usize,
    ) -> Result<HttpRequest, ApiError> {
        validate::require((1..=MAX_EVENTS).contains(&limit), Rule::EventPageLimit)?;
        let mut request = self.request(Call::Events, "/api/events");
        request.set_query(&[
            ("camera", camera.as_str().to_owned()),
            ("from", window.from().get().to_string()),
            ("to", window.to().get().to_string()),
            ("limit", limit.to_string()),
        ]);
        Ok(request)
    }

    /// `GET` of the MP4 export of `window`. A main stream is exported through
    /// `/api/cameras/{camera}/export.mp4`, with the camera's recorded audio as
    /// a second track when any was recorded. Any other stream is exported
    /// alone, through `/api/streams/{camera}/{kind}/export.mp4`.
    /// [`answer::export`](crate::api::answer::export)
    /// checks the answer's head; its body streams to the destination after.
    /// The example on [`ApiOrigin`] builds both routes.
    pub fn export(&self, camera: &CameraId, kind: StreamKind, window: Window) -> HttpRequest {
        let path = match kind {
            StreamKind::Main => format!("/api/cameras/{camera}/export.mp4"),
            StreamKind::Sub | StreamKind::Audio => {
                format!("/api/streams/{camera}/{kind}/export.mp4")
            }
        };
        let mut request = self.request(Call::Export, &path);
        request.set_window(window);
        request
    }

    /// `POST /api/events/{id}/retain`: keeps the footage around event `id`
    /// from retention until a time ([`Retention::Until`]), or clears the
    /// mark ([`Retention::Release`]); the daemon answers with the event as
    /// it lists it after the change, which
    /// [`answer::retain_event`](crate::api::answer::retain_event) checks. The
    /// body is `{"until": "<microseconds>"}`, or `{"until": null}` to
    /// release. The id is positive, as every [`EventId`] is, so no rule
    /// refuses the request. The example on [`HttpRequest`] builds it.
    pub fn retain_event(&self, id: EventId, retention: Retention) -> HttpRequest {
        let until = match retention {
            Retention::Until(until) => Some(until.get().to_string()),
            Retention::Release => None,
        };
        let body = serde_json::json!({ "until": until });
        let mut request = self.request(Call::RetainEvent, &format!("/api/events/{id}/retain"));
        request.set_body(body.to_string().into_bytes());
        request
    }

    /// `GET /api/streams/{camera}/{kind}/keyframe?t`: an init segment and one
    /// single-sample fragment holding the sync sample at or before `time`,
    /// which [`answer::keyframe`](crate::api::answer::keyframe) checks. The
    /// example on [`HttpRequest`] builds it.
    pub fn keyframe(&self, camera: &CameraId, kind: StreamKind, time: WallUs) -> HttpRequest {
        let mut request = self.request(
            Call::Keyframe,
            &format!("/api/streams/{camera}/{kind}/keyframe"),
        );
        request.set_query(&[("t", time.get().to_string())]);
        request
    }

    /// `GET /api/streams/{camera}/{kind}/hls/vod.m3u8?from&to`: the VOD
    /// playlist of `window`, whose media references carry scoped capabilities
    /// for a player to follow, which
    /// [`answer::vod_playlist`](crate::api::answer::vod_playlist) checks. The
    /// example on [`ApiOrigin`] builds it.
    pub fn vod_playlist(&self, camera: &CameraId, kind: StreamKind, window: Window) -> HttpRequest {
        let mut request = self.request(
            Call::VodPlaylist,
            &format!("/api/streams/{camera}/{kind}/hls/vod.m3u8"),
        );
        request.set_window(window);
        request
    }

    /// `GET /api/onvif/devices?wait_ms`: the cameras a WS-Discovery probe
    /// finds within `wait`, which
    /// [`answer::discover`](crate::api::answer::discover) checks. The request
    /// names `wait` in whole milliseconds, rounded down. The example on
    /// [`ApiOrigin`] builds it.
    ///
    /// # Errors
    ///
    /// [`ApiError::Invalid`] with [`Rule::DiscoveryWait`], "invalid discovery
    /// wait", when those milliseconds lie outside [`DISCOVERY_WAIT_MS`], 500 to
    /// 10,000, the daemon's range.
    pub fn discover(&self, wait: Duration) -> Result<HttpRequest, ApiError> {
        let wait_ms = u64::try_from(wait.as_millis()).unwrap_or(u64::MAX);
        validate::require(DISCOVERY_WAIT_MS.contains(&wait_ms), Rule::DiscoveryWait)?;
        let mut request = self.request(Call::Discover, "/api/onvif/devices");
        request.set_query(&[("wait_ms", wait_ms.to_string())]);
        Ok(request)
    }

    /// `POST /api/onvif/streams` with `request` as its body: the RTSP URLs of
    /// one camera's ONVIF media profiles, which
    /// [`answer::stream_urls`](crate::api::answer::stream_urls) checks. The
    /// answer may take the daemon's whole lookup bound, so the request waits
    /// [`STREAM_LOOKUP_READ_IDLE`]. The example on [`ApiOrigin`] builds it.
    ///
    /// # Panics
    ///
    /// Never: a [`StreamUrlsRequest`] holds only strings, a number, and
    /// booleans, which `serde_json` always encodes.
    pub fn stream_urls(&self, request: &StreamUrlsRequest) -> HttpRequest {
        let body = serde_json::to_vec(request)
            .expect("a StreamUrlsRequest of strings, a number, and booleans encodes");
        let mut built = self.request(Call::StreamUrls, "/api/onvif/streams");
        built.set_body(body);
        built
    }
}

#[cfg(test)]
mod tests {
    use iris_core::id::{CameraId, StreamKind};
    use iris_core::time::WallUs;

    use super::*;
    use crate::api::{EventId, MAX_EVENTS, NewCamera, StreamUrlsRequest, Window};
    use crate::origin::{ApiOrigin, PlainHttp};

    fn origin() -> ApiOrigin {
        ApiOrigin::parse("https://nvr.example:8443", PlainHttp::LoopbackOnly).unwrap()
    }

    fn camera(id: &str) -> CameraId {
        CameraId::try_new(id).unwrap()
    }

    fn time(micros: i64) -> WallUs {
        WallUs::new(micros)
    }

    fn event_id(id: i64) -> EventId {
        EventId::try_new(id).unwrap()
    }

    fn window() -> Window {
        Window::try_new(time(10), time(20)).unwrap()
    }

    fn new_camera() -> NewCamera {
        NewCamera {
            id: camera("cam8"),
            name: "Garage".into(),
            rtsp_url: "rtsp://192.0.2.10/x".into(),
            sub_url: None,
            username: "viewer".into(),
            password: "secret".into(),
            record: vec![StreamKind::Main],
            events: Vec::new(),
        }
    }

    fn lookup() -> StreamUrlsRequest {
        StreamUrlsRequest {
            host: "192.0.2.20".into(),
            http_port: 80,
            https: false,
            tls_verify: true,
            username: "viewer".into(),
            password: "secret".into(),
        }
    }

    /// Every request, one of each call and of each export route.
    fn every_request() -> [HttpRequest; 13] {
        let origin = origin();
        let cam = camera("cam");
        [
            origin.cameras(),
            origin.add_camera(&new_camera()).unwrap(),
            origin.remove_camera(&cam),
            origin.segments(&cam, StreamKind::Main, window()),
            origin.events(&cam, window(), 200).unwrap(),
            origin.export(&cam, StreamKind::Main, window()),
            origin.export(&cam, StreamKind::Sub, window()),
            origin.export(&cam, StreamKind::Audio, window()),
            origin.retain_event(event_id(7), Retention::Release),
            origin.keyframe(&cam, StreamKind::Sub, time(15)),
            origin.vod_playlist(&cam, StreamKind::Sub, window()),
            origin.discover(Duration::from_secs(3)).unwrap(),
            origin.stream_urls(&lookup()),
        ]
    }

    /// The path of `request` with its query.
    fn target(request: &HttpRequest) -> String {
        match request.url.query() {
            Some(query) => format!("{}?{query}", request.url.path()),
            None => request.url.path().to_owned(),
        }
    }

    /// Each request of [`every_request`], in its order. The call, the method,
    /// and the path with its query come from the route table of
    /// `crates/iris-api/README.md`. The byte limit and the read deadline are
    /// the viewer's.
    fn route_table() -> [(Call, Method, &'static str, u64, Duration); 13] {
        let thirty = Duration::from_secs(30);
        [
            (Call::Cameras, Method::Get, "/api/cameras", 1 << 20, thirty),
            (
                Call::AddCamera,
                Method::Post,
                "/api/cameras",
                16 << 10,
                thirty,
            ),
            (
                Call::RemoveCamera,
                Method::Delete,
                "/api/cameras/cam",
                0,
                thirty,
            ),
            (
                Call::Segments,
                Method::Get,
                "/api/streams/cam/main/segments?from=10&to=20",
                16 << 20,
                thirty,
            ),
            (
                Call::Events,
                Method::Get,
                "/api/events?camera=cam&from=10&to=20&limit=200",
                16 << 20,
                thirty,
            ),
            (
                Call::Export,
                Method::Get,
                "/api/cameras/cam/export.mp4?from=10&to=20",
                0,
                thirty,
            ),
            (
                Call::Export,
                Method::Get,
                "/api/streams/cam/sub/export.mp4?from=10&to=20",
                0,
                thirty,
            ),
            (
                Call::Export,
                Method::Get,
                "/api/streams/cam/audio/export.mp4?from=10&to=20",
                0,
                thirty,
            ),
            (
                Call::RetainEvent,
                Method::Post,
                "/api/events/7/retain",
                135 << 10,
                thirty,
            ),
            (
                Call::Keyframe,
                Method::Get,
                "/api/streams/cam/sub/keyframe?t=15",
                71 << 20,
                thirty,
            ),
            (
                Call::VodPlaylist,
                Method::Get,
                "/api/streams/cam/sub/hls/vod.m3u8?from=10&to=20",
                64 << 20,
                thirty,
            ),
            (
                Call::Discover,
                Method::Get,
                "/api/onvif/devices?wait_ms=3000",
                512 << 10,
                thirty,
            ),
            (
                Call::StreamUrls,
                Method::Post,
                "/api/onvif/streams",
                64 << 10,
                Duration::from_secs(70),
            ),
        ]
    }

    #[test]
    fn each_builder_makes_the_request_of_its_call() {
        let requests = every_request();
        let cases = requests.iter().zip(route_table());
        for (request, (call, ..)) in cases {
            assert_eq!(request.call(), call);
        }
    }

    #[test]
    fn each_request_has_its_calls_method() {
        let requests = every_request();
        let cases = requests.iter().zip(route_table());
        for (request, (call, method, ..)) in cases {
            assert_eq!(request.method(), method, "{call:?}");
        }
    }

    #[test]
    fn each_builder_names_its_calls_route() {
        let requests = every_request();
        let cases = requests.iter().zip(route_table());
        for (request, (call, _, route, ..)) in cases {
            assert_eq!(target(request), route, "{call:?}");
        }
    }

    /// Setting a query replaces the one the request held, so a builder's
    /// query is the one it sets and no pair before it.
    #[test]
    fn a_query_set_twice_holds_only_the_second() {
        let mut request = origin().keyframe(&camera("cam"), StreamKind::Main, time(15));
        request.set_query(&[("t", "20".to_owned())]);
        assert_eq!(request.url.query(), Some("t=20"));
    }

    #[test]
    fn each_request_holds_its_calls_byte_limit() {
        let requests = every_request();
        let cases = requests.iter().zip(route_table());
        for (request, (call, _, _, limit, _)) in cases {
            assert_eq!(request.max_body_bytes(), limit, "{call:?}");
        }
    }

    #[test]
    fn a_stream_lookup_waits_70_s_where_every_other_request_waits_30_s() {
        let requests = every_request();
        let cases = requests.iter().zip(route_table());
        for (request, (call, .., idle)) in cases {
            assert_eq!(request.read_idle(), idle, "{call:?}");
        }
    }

    #[test]
    fn a_method_shows_as_a_request_line_names_it() {
        for (method, shown) in [
            (Method::Get, "GET"),
            (Method::Post, "POST"),
            (Method::Delete, "DELETE"),
        ] {
            assert_eq!(method.to_string(), shown, "{method:?}");
        }
    }

    #[test]
    fn a_method_converts_into_the_http_crates_method_of_its_name() {
        for (method, converted) in [
            (Method::Get, http::Method::GET),
            (Method::Post, http::Method::POST),
            (Method::Delete, http::Method::DELETE),
        ] {
            assert_eq!(http::Method::from(method), converted, "{method:?}");
        }
    }

    #[test]
    fn each_request_is_on_the_origin() {
        let origin = origin();
        for request in every_request() {
            assert_eq!(
                request.url.origin(),
                origin.url().origin(),
                "{:?}",
                request.call
            );
        }
    }

    /// The main stream is exported through its camera, with the recorded
    /// audio; the other streams are exported alone.
    #[test]
    fn the_main_stream_is_exported_through_its_camera() {
        let origin = origin();
        let cam = camera("cam1");
        for (kind, path) in [
            (StreamKind::Main, "/api/cameras/cam1/export.mp4"),
            (StreamKind::Sub, "/api/streams/cam1/sub/export.mp4"),
            (StreamKind::Audio, "/api/streams/cam1/audio/export.mp4"),
        ] {
            assert_eq!(
                origin.export(&cam, kind, window()).url.path(),
                path,
                "{kind}"
            );
        }
    }

    #[test]
    fn the_playlist_url_carries_the_window() {
        let origin = ApiOrigin::parse("http://192.0.2.10:8554", PlainHttp::Anywhere).unwrap();
        assert_eq!(
            origin
                .vod_playlist(&camera("cam1"), StreamKind::Sub, window())
                .url
                .as_str(),
            "http://192.0.2.10:8554/api/streams/cam1/sub/hls/vod.m3u8?from=10&to=20"
        );
    }

    /// A camera to add and a stream lookup are each sent as the JSON of the
    /// value the builder was given.
    #[test]
    fn a_request_body_is_the_json_of_its_value() {
        let origin = origin();
        assert_eq!(
            origin.add_camera(&new_camera()).unwrap().body(),
            Some(serde_json::to_vec(&new_camera()).unwrap().as_slice())
        );
        assert_eq!(
            origin.stream_urls(&lookup()).body(),
            Some(serde_json::to_vec(&lookup()).unwrap().as_slice())
        );
    }

    #[test]
    fn a_retain_mark_sends_until_as_a_string_of_microseconds() {
        let retain = origin().retain_event(event_id(7), Retention::Until(time(1_500)));
        assert_eq!(retain.body(), Some(br#"{"until":"1500"}"#.as_slice()));
    }

    #[test]
    fn a_release_sends_until_as_null() {
        let release = origin().retain_event(event_id(7), Retention::Release);
        assert_eq!(release.body(), Some(br#"{"until":null}"#.as_slice()));
    }

    #[test]
    fn only_a_post_sends_a_body() {
        for request in every_request() {
            assert_eq!(
                request.body().is_some(),
                request.method() == Method::Post,
                "{:?}",
                request.call
            );
        }
    }

    #[test]
    fn an_event_page_may_ask_for_one_to_the_most_events() {
        let cam = camera("cam");
        for limit in [0, MAX_EVENTS + 1] {
            assert_eq!(
                origin()
                    .events(&cam, window(), limit)
                    .unwrap_err()
                    .to_string(),
                "invalid event page limit",
                "{limit}"
            );
        }
        for limit in [1, MAX_EVENTS] {
            assert!(origin().events(&cam, window(), limit).is_ok(), "{limit}");
        }
    }

    /// The problem `rtsp_url_problem` finds with a URL that carries userinfo.
    const CREDENTIALS: &str = "cannot carry credentials; enter them as the username and password";

    /// The camera to add of the fixture with `url` as its main stream URL.
    fn with_main_url(url: &str) -> NewCamera {
        NewCamera {
            rtsp_url: url.into(),
            ..new_camera()
        }
    }

    /// The camera to add of the fixture with `url` as its sub stream URL.
    fn with_sub_url(url: &str) -> NewCamera {
        NewCamera {
            sub_url: Some(url.into()),
            ..new_camera()
        }
    }

    /// The text of the rule `add_camera` refuses `camera` for. A camera the
    /// builder takes fails the test at the line that asked for the text.
    #[track_caller]
    fn refused_camera(camera: &NewCamera) -> String {
        match origin().add_camera(camera) {
            Err(ApiError::Invalid(refused)) => refused.to_string(),
            other => panic!("{other:?}"),
        }
    }

    /// The refusal is in the sentence the Linux viewer's form shows for the
    /// main URL.
    #[test]
    fn a_camera_to_add_is_refused_for_a_problem_with_its_main_url() {
        for (url, problem) in [
            ("rtsp://viewer:secret@192.0.2.10/x", CREDENTIALS),
            ("rtsp://viewer@192.0.2.10/x", CREDENTIALS),
            ("http://192.0.2.10/x", "must start with rtsp://"),
        ] {
            assert_eq!(
                refused_camera(&with_main_url(url)),
                format!("the RTSP URL {problem}"),
                "{url}"
            );
        }
    }

    /// The refusal is in the sentence the Linux viewer's form shows for the
    /// sub URL.
    #[test]
    fn a_camera_to_add_is_refused_for_a_problem_with_its_sub_url() {
        for (url, problem) in [
            ("rtsp://viewer:secret@192.0.2.10/y", CREDENTIALS),
            ("rtsp:y", "names no host"),
        ] {
            assert_eq!(
                refused_camera(&with_sub_url(url)),
                format!("the sub URL {problem}"),
                "{url}"
            );
        }
    }

    /// A camera to add whose main and sub URL both name no host.
    #[test]
    fn the_main_url_of_a_camera_to_add_is_checked_before_its_sub_url() {
        let camera = NewCamera {
            sub_url: Some("rtsp:y".into()),
            ..with_main_url("rtsp:x")
        };
        assert_eq!(refused_camera(&camera), "the RTSP URL names no host");
    }

    #[test]
    fn a_camera_to_add_with_a_usable_sub_url_is_accepted() {
        assert!(
            origin()
                .add_camera(&with_sub_url("rtsp://192.0.2.10/y"))
                .is_ok()
        );
    }

    #[test]
    fn a_discovery_names_its_wait_in_whole_milliseconds_rounded_down() {
        for (wait, query) in [
            (Duration::from_micros(3_000_999), "wait_ms=3000"),
            (Duration::from_micros(10_000_999), "wait_ms=10000"),
        ] {
            let request = origin().discover(wait).unwrap();
            assert_eq!(request.url.query(), Some(query), "{wait:?}");
        }
    }

    /// The daemon takes a wait of 500 to 10,000 milliseconds, and a builder
    /// refuses any other.
    #[test]
    fn a_discovery_waits_500_to_10_000_milliseconds() {
        for wait in [Duration::from_millis(500), Duration::from_secs(10)] {
            assert!(origin().discover(wait).is_ok(), "{wait:?}");
        }
        for wait in [
            Duration::ZERO,
            Duration::from_micros(499_999),
            Duration::from_millis(10_001),
            Duration::MAX,
        ] {
            match origin().discover(wait) {
                Err(ApiError::Invalid(refused)) => {
                    assert_eq!(refused, Rule::DiscoveryWait, "{wait:?}");
                }
                other => panic!("{wait:?}: {other:?}"),
            }
        }
    }

    /// A request's `Debug` shows its URL without its userinfo, query, or
    /// fragment, whatever URL the request holds.
    #[test]
    fn a_request_debug_shows_its_url_without_userinfo_query_or_fragment() {
        let mut request = origin().cameras();
        request.url =
            Url::parse("https://viewer:hidden@nvr.example:8443/api/cameras?token=hidden#hidden")
                .unwrap();
        let shown = format!("{request:?}");
        assert!(
            shown.contains(
                r#"url: "https://<redacted>@nvr.example:8443/api/cameras?<redacted>#<redacted>""#
            ),
            "{shown}"
        );
        assert!(
            !shown.contains("hidden") && !shown.contains("viewer"),
            "{shown}"
        );
    }

    /// The window and limit a builder sets in the query show redacted.
    #[test]
    fn a_request_debug_redacts_a_builders_query() {
        let window = format!(
            "{:?}",
            origin().segments(&camera("cam"), StreamKind::Main, window())
        );
        assert!(
            window.contains(
                r#"url: "https://nvr.example:8443/api/streams/cam/main/segments?<redacted>""#
            ),
            "{window}"
        );
    }

    #[test]
    fn a_request_debug_hides_its_body() {
        let origin = origin();
        for request in [
            origin.add_camera(&new_camera()).unwrap(),
            origin.stream_urls(&lookup()),
        ] {
            let shown = format!("{request:?}");
            let length = request.body().map_or(0, <[u8]>::len);
            assert!(!shown.contains("secret"), "{shown}");
            assert!(
                shown.contains(&format!("body_bytes: Some({length})")),
                "{shown}"
            );
        }
        let empty = format!("{:?}", origin.cameras());
        assert!(empty.contains("body_bytes: None"), "{empty}");
    }

    /// What `body_to_read` reads of a refusal: the daemon's message, up to
    /// 16 KiB.
    const MESSAGE: BodyRead = BodyRead::Message {
        max_bytes: 16 << 10,
    };

    /// One request of each call that takes any `2xx`.
    fn any_success_requests() -> [HttpRequest; 8] {
        let origin = origin();
        let cam = camera("cam");
        [
            origin.cameras(),
            origin.segments(&cam, StreamKind::Main, window()),
            origin.events(&cam, window(), 200).unwrap(),
            origin.retain_event(event_id(7), Retention::Release),
            origin.keyframe(&cam, StreamKind::Main, time(15)),
            origin.vod_playlist(&cam, StreamKind::Main, window()),
            origin.discover(Duration::from_secs(3)).unwrap(),
            origin.stream_urls(&lookup()),
        ]
    }

    /// The request of each call that takes one status alone, with that
    /// status: an added camera, a removal, and an export.
    fn one_status_requests() -> [(HttpRequest, u16); 3] {
        let origin = origin();
        let cam = camera("cam");
        [
            (origin.add_camera(&new_camera()).unwrap(), 201),
            (origin.remove_camera(&cam), 204),
            (origin.export(&cam, StreamKind::Main, window()), 200),
        ]
    }

    /// Any success status holds the value of a call that takes any `2xx`, up
    /// to the call's limit.
    #[test]
    fn any_success_is_read_for_the_value_of_a_call_that_takes_any_2xx() {
        for request in any_success_requests() {
            let value = BodyRead::Value {
                max_bytes: request.max_body_bytes(),
            };
            for status in [200, 201, 204, 206, 299] {
                assert_eq!(
                    request.body_to_read(status, Some("video/mp4")),
                    value,
                    "{:?} {status}",
                    request.call
                );
            }
        }
    }

    /// The 201 of an added camera holds the camera, up to 16 KiB.
    #[test]
    fn an_added_camera_201_is_read_for_its_value() {
        let added = origin().add_camera(&new_camera()).unwrap();
        assert_eq!(
            added.body_to_read(201, Some("application/json")),
            BodyRead::Value {
                max_bytes: 16 << 10
            }
        );
    }

    /// The head of a removal's 204 and of an export's 200 is the whole
    /// answer.
    #[test]
    fn a_success_whose_head_is_the_whole_answer_is_read_no_further() {
        let origin = origin();
        let cam = camera("cam");
        for (request, status) in [
            (origin.remove_camera(&cam), 204),
            (origin.export(&cam, StreamKind::Main, window()), 200),
        ] {
            assert_eq!(
                request.body_to_read(status, Some("video/mp4")),
                BodyRead::Nothing,
                "{:?}",
                request.call
            );
        }
    }

    /// Any status a call does not expect is a refusal, read for its
    /// message: outside `2xx` for most calls, and any other status for the
    /// three that take one alone.
    #[test]
    fn a_status_the_call_does_not_expect_is_read_for_its_message() {
        for request in any_success_requests() {
            for status in [0, 101, 199, 300, 302, 304, 500, 1000] {
                assert_eq!(
                    request.body_to_read(status, Some("video/mp4")),
                    MESSAGE,
                    "{:?} {status}",
                    request.call
                );
            }
        }
        for (request, only) in one_status_requests() {
            for status in [199, 200, 201, 202, 203, 204, 205, 206, 299, 302, 404] {
                if status == only {
                    continue;
                }
                assert_eq!(
                    request.body_to_read(status, Some("video/mp4")),
                    MESSAGE,
                    "{:?} {status}",
                    request.call
                );
            }
        }
    }

    /// A keyframe's 404 says nothing is recorded from its head alone.
    #[test]
    fn a_keyframe_404_is_read_no_further_than_its_head() {
        let keyframe = origin().keyframe(&camera("cam"), StreamKind::Main, time(15));
        for content_type in [None, Some("application/json"), Some("video/mp4")] {
            assert_eq!(
                keyframe.body_to_read(404, content_type),
                BodyRead::Nothing,
                "{content_type:?}"
            );
        }
    }

    #[test]
    fn another_calls_404_is_read_for_its_message() {
        let origin = origin();
        let cam = camera("cam");
        for request in [
            origin.cameras(),
            origin.remove_camera(&cam),
            origin.export(&cam, StreamKind::Main, window()),
            origin.vod_playlist(&cam, StreamKind::Main, window()),
        ] {
            assert_eq!(
                request.body_to_read(404, None),
                MESSAGE,
                "{:?}",
                request.call
            );
        }
    }

    /// Content types other than `video/mp4`.
    const OTHER_MEDIA_TYPES: [Option<&str>; 5] = [
        None,
        Some(""),
        Some("text/html"),
        Some("video/mp4x"),
        Some("video"),
    ];

    /// A keyframe and an export, the calls whose answer is `video/mp4`.
    fn mp4_requests() -> [HttpRequest; 2] {
        let origin = origin();
        let cam = camera("cam");
        [
            origin.keyframe(&cam, StreamKind::Main, time(15)),
            origin.export(&cam, StreamKind::Main, window()),
        ]
    }

    /// A keyframe or an export answered with another media type is refused
    /// from its head.
    #[test]
    fn another_media_type_is_read_no_further_than_its_head() {
        for request in mp4_requests() {
            for content_type in OTHER_MEDIA_TYPES {
                assert_eq!(
                    request.body_to_read(200, content_type),
                    BodyRead::Nothing,
                    "{:?} {content_type:?}",
                    request.call
                );
            }
        }
    }

    #[test]
    fn a_refused_status_is_read_for_its_message_whatever_its_media_type() {
        for request in mp4_requests() {
            for content_type in OTHER_MEDIA_TYPES {
                assert_eq!(
                    request.body_to_read(500, content_type),
                    MESSAGE,
                    "{:?} {content_type:?}",
                    request.call
                );
            }
        }
    }

    /// A call whose answer is JSON or a playlist has no media type to check.
    #[test]
    fn a_call_without_a_media_type_reads_its_value_whatever_the_content_type() {
        let origin = origin();
        let cam = camera("cam");
        for request in [
            origin.cameras(),
            origin.vod_playlist(&cam, StreamKind::Main, window()),
        ] {
            for content_type in OTHER_MEDIA_TYPES {
                assert_eq!(
                    request.body_to_read(200, content_type),
                    BodyRead::Value {
                        max_bytes: request.max_body_bytes()
                    },
                    "{:?} {content_type:?}",
                    request.call
                );
            }
        }
    }

    #[test]
    fn a_content_type_names_its_media_type_before_any_parameter_in_any_case() {
        let keyframe = origin().keyframe(&camera("cam"), StreamKind::Main, time(15));
        let value = BodyRead::Value {
            max_bytes: 71 << 20,
        };
        for named in [
            "video/mp4",
            "Video/MP4",
            " video/mp4 ; codecs=\"hvc1\"",
            "video/mp4;",
        ] {
            assert_eq!(keyframe.body_to_read(200, Some(named)), value, "{named:?}");
        }
        for other in [
            None,
            Some(""),
            Some("text/plain"),
            Some("video/mp4x"),
            Some("video"),
            Some(";video/mp4"),
        ] {
            assert_eq!(
                keyframe.body_to_read(200, other),
                BodyRead::Nothing,
                "{other:?}"
            );
        }
    }
}
