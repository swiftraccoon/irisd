//! The checks an answer of the irisd API passes before a viewer uses it, one
//! for each call and named as its builder on [`ApiOrigin`]. A check takes the
//! [`Answer`] a transport received for the request the builder made, and
//! returns the call's value or the refusal a viewer shows. It runs in this
//! order, and the first rule an answer breaks is its refusal:
//!
//! 1. The answer came from the API origin, or [`ApiError::ForeignOrigin`].
//! 2. Its status is one the call expects, or [`ApiError::Refused`] with the
//!    daemon's message from the body.
//! 3. The content type of a keyframe or an export is `video/mp4`, or
//!    [`ApiError::ContentType`].
//! 4. The body is at most the call's byte limit
//!    ([`HttpRequest::max_body_bytes`]), or [`ApiError::Oversized`].
//! 5. The body decodes as the call's JSON, or [`ApiError::Decode`], and passes
//!    the call's validator, or [`ApiError::Invalid`] with the [`Rule`] it
//!    breaks.
//! 6. The rules of that call alone hold, or [`ApiError::Invalid`] with the
//!    [`Rule`] broken. The camera added is the one sent, the event retained
//!    the one marked, and a keyframe is not empty. A VOD playlist is UTF-8 with
//!    fragment timing in the window and references on the origin. A refusal
//!    of its text is [`ApiError::Playlist`].
//!
//! The status and the content type are the call's rules of the head, the
//! ones [`HttpRequest::body_to_read`] decides by. A check looks at the body
//! only of an answer whose body `body_to_read` reads. A refusal with a long
//! body therefore stays a refusal, and a head that the check refuses, or
//! answers without a body, needs no body at all. Nothing here panics on what
//! an answer holds; the fuzz target `client_answers` checks that.
//!
//! # Example
//!
//! ```
//! # use std::error::Error;
//! #
//! use iris_client::api::{Answer, BodyRead, MAX_CAMERAS_BODY_BYTES, answer};
//! use iris_client::origin::{ApiOrigin, PlainHttp};
//! use iris_core::id::{CameraId, StreamKind};
//! use iris_core::time::WallUs;
//!
//! let api = "https://nvr.example:8443";
//! let origin = ApiOrigin::parse(api, PlainHttp::LoopbackOnly)?;
//! let request = origin.cameras();
//! // The transport sends the request with the bearer, and reads the part of
//! // the body that `body_to_read` names.
//! let read = request.body_to_read(200, Some("application/json"));
//! let value = BodyRead::Value {
//!     max_bytes: u64::try_from(MAX_CAMERAS_BODY_BYTES)?,
//! };
//! assert_eq!(read, value);
//! let body = br#"[{"id":"cam1","name":"Front","provisioned":false}]"#;
//! let received = Answer {
//!     status: 200,
//!     content_type: Some("application/json"),
//!     final_url: request.url(),
//!     body: Some(body),
//! };
//! assert_eq!(answer::cameras(&origin, &received)?[0].name, "Front");
//! let forbidden = Answer {
//!     status: 403,
//!     body: Some(br#"{"error":"provisioning is disabled"}"#),
//!     ..received
//! };
//! let Err(refused) = answer::cameras(&origin, &forbidden) else {
//!     return Err("a 403 is refused".into());
//! };
//! assert_eq!(
//!     refused.to_string(),
//!     "listing the cameras failed with 403 Forbidden: \
//!      provisioning is disabled"
//! );
//!
//! // A keyframe's 404 says that nothing is recorded there.
//! let camera = CameraId::try_new("cam1")?;
//! let request = origin.keyframe(&camera, StreamKind::Sub, WallUs::new(0));
//! let nothing = Answer {
//!     status: 404,
//!     content_type: None,
//!     final_url: request.url(),
//!     body: None,
//! };
//! assert_eq!(answer::keyframe(&origin, &nothing)?, None);
//! # Ok::<(), Box<dyn Error>>(())
//! ```
//!
//! [`HttpRequest::body_to_read`]: crate::api::HttpRequest::body_to_read
//! [`HttpRequest::max_body_bytes`]: crate::api::HttpRequest::max_body_bytes

use std::{fmt, slice};

use iris_core::id::CameraId;
use serde::de::DeserializeOwned;
use serde_json::Value;
use url::Url;

use crate::api::request::Head;
use crate::api::{
    ApiError, Call, Camera, DiscoveredDevice, EventId, EventRecord, MAX_ERROR_BODY_BYTES,
    MAX_ERROR_MESSAGE_CHARS, ProfileStream, Rule, Segment, VodPlaylist, Window, recorded_in_window,
    types, validate, validate_cameras, validate_devices, validate_event, validate_events,
    validate_playback_fragments, validate_segments, validate_streams,
};
use crate::origin::ApiOrigin;
use crate::vod::{self, PlaylistError};

/// An answer of the irisd API as a transport received it, for the check of
/// the call that asked for it. The example of the [module](self) builds one.
///
/// Its [`Debug`](fmt::Debug) output shows the body's length and not its
/// bytes, since a VOD playlist's references carry media capabilities. It
/// shows the URL with its userinfo, query, and fragment each as `<redacted>`
/// and its path as it stands; a URL without a host shows as `<redacted>`
/// whole.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct Answer<'received> {
    /// The HTTP status code as the transport received it. Any value is
    /// checked. The check of a keyframe gives `None` for a 404, which says
    /// that nothing is recorded there. Any other status the call does not
    /// expect, 0 or 1000 among them, is refused, and its text shows one outside
    /// 100 to 999 as its number alone.
    pub status: u16,
    /// The value of the `Content-Type` header; `None` when the answer has
    /// none, or one that is not visible ASCII. The check of a keyframe and of
    /// an export refuses either as it refuses another media type.
    pub content_type: Option<&'received str>,
    /// The URL the answer came from, after any redirect the transport
    /// followed; that of the request for a transport that follows none.
    pub final_url: &'received Url,
    /// The body the transport holds, read the way the request's
    /// [`body_to_read`](crate::api::HttpRequest::body_to_read) said: `Some`
    /// only with the whole body, when that is at most the limit
    /// `body_to_read` gave. `None` when the transport holds none of it:
    /// `body_to_read` said to read nothing, the body is longer than the limit,
    /// or the read of a refusal's message failed. A body is longer when a
    /// `Content-Length` is above the limit or a byte comes past it. A check
    /// looks at the body only of an answer whose body `body_to_read` reads:
    /// it refuses a value's `None` as too large, and a refusal's `None` leaves
    /// it without a message.
    pub body: Option<&'received [u8]>,
}

impl fmt::Debug for Answer<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Answer")
            .field("status", &self.status)
            .field("content_type", &self.content_type)
            .field("final_url", &types::shown_url(self.final_url))
            .field("body_bytes", &self.body.map(<[u8]>::len))
            .finish()
    }
}

/// The daemon's message in the body of a refusal, as a viewer shows it: the
/// string field `error` of a JSON body of at most [`MAX_ERROR_BODY_BYTES`],
/// each control character a space, cut to [`MAX_ERROR_MESSAGE_CHARS`]
/// characters. None for any other body, and none for a message blank once cut.
fn refusal_detail(body: &[u8]) -> Option<String> {
    if body.len() > MAX_ERROR_BODY_BYTES {
        return None;
    }
    let json: Value = serde_json::from_slice(body).ok()?;
    let message: String = json
        .get("error")?
        .as_str()?
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .take(MAX_ERROR_MESSAGE_CHARS)
        .collect();
    if message.trim().is_empty() {
        None
    } else {
        Some(message)
    }
}

/// Refuses an answer that came from an origin other than `origin`.
fn same_origin(origin: &ApiOrigin, answer: &Answer<'_>, call: Call) -> Result<(), ApiError> {
    if answer.final_url.origin() == origin.url().origin() {
        Ok(())
    } else {
        Err(ApiError::ForeignOrigin { call })
    }
}

/// The checks of an answer's head, in order: its origin, then its status and
/// content type as `Call::head` sorts them into a `Head`, which
/// [`HttpRequest::body_to_read`](crate::api::HttpRequest::body_to_read) reads
/// by. A refused status takes the daemon's message from the body, which
/// `body_to_read` reads for it; a head the call takes is returned as its
/// `Head`.
fn head(origin: &ApiOrigin, answer: &Answer<'_>, call: Call) -> Result<Head, ApiError> {
    same_origin(origin, answer, call)?;
    match call.head(answer.status, answer.content_type) {
        Head::Refused => Err(ApiError::Refused {
            call,
            status: answer.status,
            detail: answer.body.and_then(refusal_detail),
        }),
        Head::OtherMediaType(expected) => Err(ApiError::ContentType { call, expected }),
        taken @ (Head::NothingRecorded | Head::Whole | Head::Value) => Ok(taken),
    }
}

/// The body of an answer to `call`, within the call's byte limit.
fn body<'received>(answer: &Answer<'received>, call: Call) -> Result<&'received [u8], ApiError> {
    match answer.body {
        Some(body) if body.len() <= call.max_body_bytes() => Ok(body),
        _ => Err(ApiError::Oversized { call }),
    }
}

/// The body of an answer to `call`, within its byte limit and decoded as `T`.
fn decoded<T: DeserializeOwned>(answer: &Answer<'_>, call: Call) -> Result<T, ApiError> {
    serde_json::from_slice(body(answer, call)?).map_err(|source| ApiError::Decode { call, source })
}

/// Checks the answer to [`ApiOrigin::cameras`]: any `2xx`, a body of at most
/// [`MAX_CAMERAS_BODY_BYTES`](crate::api::MAX_CAMERAS_BODY_BYTES), and a list
/// that [`validate_cameras`] accepts. The example of the [module](self) runs
/// it.
///
/// # Errors
///
/// The first rule of the [module](self) the answer breaks.
pub fn cameras(origin: &ApiOrigin, answer: &Answer<'_>) -> Result<Vec<Camera>, ApiError> {
    head(origin, answer, Call::Cameras)?;
    let cameras: Vec<Camera> = decoded(answer, Call::Cameras)?;
    validate_cameras(&cameras)?;
    Ok(cameras)
}

/// Checks the answer to [`ApiOrigin::add_camera`] for the camera whose id
/// was `sent`. The answer must be 201, with a body of at most
/// [`MAX_CAMERA_BODY_BYTES`](crate::api::MAX_CAMERA_BODY_BYTES) that holds a
/// camera [`validate_cameras`] accepts, and the camera's id must equal
/// `sent`. The example of the [module](self) runs [`cameras`], whose steps
/// this check takes too.
///
/// # Errors
///
/// The first rule of the [module](self) the answer breaks;
/// [`ApiError::Invalid`] with [`Rule::AnotherCamera`], "the daemon answered
/// with another camera", for another id.
pub fn add_camera(
    origin: &ApiOrigin,
    answer: &Answer<'_>,
    sent: &CameraId,
) -> Result<Camera, ApiError> {
    head(origin, answer, Call::AddCamera)?;
    let added: Camera = decoded(answer, Call::AddCamera)?;
    validate_cameras(slice::from_ref(&added))?;
    validate::require(added.id == *sent, Rule::AnotherCamera)?;
    Ok(added)
}

/// Checks the answer to [`ApiOrigin::remove_camera`]: 204, whose head is the
/// whole answer; the check takes no body. The example of the [module](self)
/// runs [`keyframe`] on a 404, a check that answers from the head alone as
/// this one does.
///
/// # Errors
///
/// [`ApiError::ForeignOrigin`], then [`ApiError::Refused`] for any status
/// but 204.
pub fn remove_camera(origin: &ApiOrigin, answer: &Answer<'_>) -> Result<(), ApiError> {
    head(origin, answer, Call::RemoveCamera)?;
    Ok(())
}

/// Checks the answer to [`ApiOrigin::segments`] for `window`: any `2xx`, a
/// body of at most [`MAX_LIST_BODY_BYTES`](crate::api::MAX_LIST_BODY_BYTES),
/// and rows that [`validate_segments`] accepts. The segments are those rows
/// less the open ones that recorded nothing in the window
/// ([`recorded_in_window`]). The example of the [module](self) runs
/// [`cameras`], whose steps this check takes too.
///
/// # Errors
///
/// The first rule of the [module](self) the answer breaks.
pub fn segments(
    origin: &ApiOrigin,
    answer: &Answer<'_>,
    window: Window,
) -> Result<Vec<Segment>, ApiError> {
    head(origin, answer, Call::Segments)?;
    let segments: Vec<Segment> = decoded(answer, Call::Segments)?;
    validate_segments(&segments, window)?;
    Ok(recorded_in_window(segments, window))
}

/// Checks the answer to [`ApiOrigin::events`] for `window` and `limit`: any
/// `2xx`, a body of at most
/// [`MAX_LIST_BODY_BYTES`](crate::api::MAX_LIST_BODY_BYTES), and a page that
/// [`validate_events`] accepts. The example of the [module](self) runs
/// [`cameras`], whose steps this check takes too.
///
/// # Errors
///
/// The first rule of the [module](self) the answer breaks.
pub fn events(
    origin: &ApiOrigin,
    answer: &Answer<'_>,
    window: Window,
    limit: usize,
) -> Result<Vec<EventRecord>, ApiError> {
    head(origin, answer, Call::Events)?;
    let events: Vec<EventRecord> = decoded(answer, Call::Events)?;
    validate_events(&events, window, limit)?;
    Ok(events)
}

/// Checks the head of the answer to [`ApiOrigin::export`]: 200 and the
/// content type `video/mp4`. The check takes no body:
/// [`HttpRequest::body_to_read`](crate::api::HttpRequest::body_to_read) reads
/// none of it, and the transport streams it to its destination once the head
/// passes. The example of the [module](self) runs [`keyframe`] on a 404, a
/// check that answers from the head alone as this one does.
///
/// # Errors
///
/// [`ApiError::ForeignOrigin`], then [`ApiError::Refused`] for any status
/// but 200, then [`ApiError::ContentType`].
pub fn export(origin: &ApiOrigin, answer: &Answer<'_>) -> Result<(), ApiError> {
    head(origin, answer, Call::Export)?;
    Ok(())
}

/// Checks the answer to [`ApiOrigin::retain_event`] for the event `id`. The
/// answer must be any `2xx`, with a body of at most
/// [`MAX_RETAIN_BODY_BYTES`](crate::api::MAX_RETAIN_BODY_BYTES) that holds an
/// event [`validate_event`] accepts, and the event's id must equal `id`. The
/// example of the [module](self) runs [`cameras`], whose steps this check
/// takes too.
///
/// # Errors
///
/// The first rule of the [module](self) the answer breaks;
/// [`ApiError::Invalid`] with [`Rule::AnotherEvent`], "retain response
/// changed the event id", for another id.
pub fn retain_event(
    origin: &ApiOrigin,
    answer: &Answer<'_>,
    id: EventId,
) -> Result<EventRecord, ApiError> {
    head(origin, answer, Call::RetainEvent)?;
    let event: EventRecord = decoded(answer, Call::RetainEvent)?;
    validate_event(&event)?;
    validate::require(event.id == id, Rule::AnotherEvent)?;
    Ok(event)
}

/// Checks the answer to [`ApiOrigin::keyframe`]. A 404 says nothing is
/// recorded there, and the check answers `None` from its head alone.
/// Otherwise the answer must be any `2xx` of the content type `video/mp4`,
/// with a body of one to [`MAX_KEYFRAME_BYTES`](crate::api::MAX_KEYFRAME_BYTES)
/// bytes. That body is the keyframe: an init segment and one single-sample
/// fragment. The keyframe is the answer's body itself, the whole slice it
/// holds, so a transport that owns the body may keep it rather than copy it.
/// The example of the [module](self) runs it on a 404.
///
/// # Errors
///
/// The first rule of the [module](self) the answer breaks;
/// [`ApiError::Invalid`] with [`Rule::EmptyKeyframe`], "keyframe response is
/// empty", for an empty body.
pub fn keyframe<'received>(
    origin: &ApiOrigin,
    answer: &Answer<'received>,
) -> Result<Option<&'received [u8]>, ApiError> {
    if head(origin, answer, Call::Keyframe)? == Head::NothingRecorded {
        return Ok(None);
    }
    let keyframe = body(answer, Call::Keyframe)?;
    validate::require(!keyframe.is_empty(), Rule::EmptyKeyframe)?;
    Ok(Some(keyframe))
}

/// Checks the answer to [`ApiOrigin::vod_playlist`] for `window`. The answer
/// must be any `2xx`, with a UTF-8 body of at most
/// [`MAX_VOD_PLAYLIST_BYTES`](vod::MAX_VOD_PLAYLIST_BYTES). Its fragment
/// timing ([`vod::playlist_fragments`]) must pass
/// [`validate_playback_fragments`] for the window. [`vod::absolute_playlist`]
/// must resolve its references onto the API origin, against the URL the
/// answer came from. The playlist holds the timing and the rewritten body from
/// the one answer. The example of the [`playback`](crate::playback) module
/// runs it.
///
/// # Errors
///
/// The first rule of the [module](self) the answer breaks. A foreign answer
/// has the text "VOD playlist response came from another origin". A body
/// that is not UTF-8 is [`ApiError::Playlist`] holding
/// [`PlaylistError::NotUtf8`]. A body that [`vod::playlist_fragments`] or
/// [`vod::absolute_playlist`] refuses is [`ApiError::Playlist`] with that
/// refusal. Fragment timing out of order or outside the window is
/// [`ApiError::Invalid`] with [`Rule::FragmentOrder`] or
/// [`Rule::NoFragmentInWindow`].
pub fn vod_playlist(
    origin: &ApiOrigin,
    answer: &Answer<'_>,
    window: Window,
) -> Result<VodPlaylist, ApiError> {
    head(origin, answer, Call::VodPlaylist)?;
    let text = str::from_utf8(body(answer, Call::VodPlaylist)?).map_err(PlaylistError::NotUtf8)?;
    let fragments = vod::playlist_fragments(text)?;
    validate_playback_fragments(&fragments, window)?;
    let body = vod::absolute_playlist(text, answer.final_url, &origin.url().origin())?;
    Ok(VodPlaylist::new(fragments, body))
}

/// Checks the answer to [`ApiOrigin::discover`]: any `2xx`, a body of at most
/// [`MAX_DEVICES_BODY_BYTES`](crate::api::MAX_DEVICES_BODY_BYTES), and
/// devices that [`validate_devices`] accepts. The example of the
/// [module](self) runs [`cameras`], whose steps this check takes too.
///
/// # Errors
///
/// The first rule of the [module](self) the answer breaks.
pub fn discover(
    origin: &ApiOrigin,
    answer: &Answer<'_>,
) -> Result<Vec<DiscoveredDevice>, ApiError> {
    head(origin, answer, Call::Discover)?;
    let devices: Vec<DiscoveredDevice> = decoded(answer, Call::Discover)?;
    validate_devices(&devices)?;
    Ok(devices)
}

/// Checks the answer to [`ApiOrigin::stream_urls`]: any `2xx`, a body of at
/// most [`MAX_STREAMS_BODY_BYTES`](crate::api::MAX_STREAMS_BODY_BYTES), and
/// profiles that [`validate_streams`] accepts. The example of the
/// [module](self) runs [`cameras`], whose steps this check takes too.
///
/// # Errors
///
/// The first rule of the [module](self) the answer breaks.
pub fn stream_urls(
    origin: &ApiOrigin,
    answer: &Answer<'_>,
) -> Result<Vec<ProfileStream>, ApiError> {
    head(origin, answer, Call::StreamUrls)?;
    let streams: Vec<ProfileStream> = decoded(answer, Call::StreamUrls)?;
    validate_streams(&streams)?;
    Ok(streams)
}

#[cfg(test)]
mod tests {
    use std::error::Error as _;
    use std::ptr;
    use std::time::Duration;

    use iris_core::event::{MAX_EVENT_DATA_LEN, MAX_EVENT_KIND_LEN};
    use iris_core::id::{CameraId, StreamKind};
    use iris_core::time::{MediaTicks, WallUs};

    use super::*;
    use crate::api::{
        BodyRead, EventId, HttpRequest, MAX_KEYFRAME_BYTES, MAX_RETAIN_BODY_BYTES, NewCamera,
        Retention, StreamUrlsRequest,
    };
    use crate::origin::PlainHttp;
    use crate::vod::{PlaybackFragment, ReferenceError};

    const ORIGIN: &str = "https://nvr.example:8443";

    fn origin() -> ApiOrigin {
        ApiOrigin::parse(ORIGIN, PlainHttp::LoopbackOnly).unwrap()
    }

    /// `path` on the API origin.
    fn on_origin(path: &str) -> Url {
        Url::parse(ORIGIN).unwrap().join(path).unwrap()
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

    /// The window the fixtures' segments, events, and fragments fall in.
    fn window() -> Window {
        Window::try_new(time(0), time(100)).unwrap()
    }

    const CAMERAS: &str = r#"[{"id":"cam1","name":"Front","provisioned":false}]"#;
    const ADDED: &str = r#"{"id":"cam8","name":"Garage","provisioned":true}"#;
    const SEGMENTS: &str = r#"[{"wall_start_us":10,"wall_end_us":20,"sealed":true}]"#;
    const EVENTS: &str = r#"[{"id":1,"kind":"motion","nvr_time":5,"retain_until_us":null}]"#;
    const RETAINED: &str = r#"{"id":7,"kind":"motion","nvr_time":5,"retain_until_us":9}"#;
    const KEYFRAME: &[u8] = b"\0\0\0\x08ftyp";
    const PLAYLIST: &str = concat!(
        "#EXTM3U\n#EXT-X-MAP:URI=\"/api/segments/1/file.mp4?token=c4p\"\n",
        "#IRIS-FRAGMENT:WALL-US=10,DURATION-TICKS=900\n#EXTINF:0.01000,\n",
        "../../../../segments/1/file.mp4?token=c4p\n#EXT-X-ENDLIST\n"
    );
    const DEVICES: &str = concat!(
        r#"[{"endpoint":"urn:uuid:1","host":"192.0.2.20","port":80,"https":false,"#,
        r#""name":"Garage","hardware":"IPC","location":null}]"#
    );
    const STREAMS: &str = r#"[{"token":"profile-1","name":"Main","url":"rtsp://192.0.2.20/1"}]"#;

    /// One answer of each call that its check accepts: the call, its status,
    /// its content type, the path it came from, and its body.
    const ACCEPTED: [(Call, u16, &str, &str, &[u8]); 11] = [
        (
            Call::Cameras,
            200,
            "application/json",
            "/api/cameras",
            CAMERAS.as_bytes(),
        ),
        (
            Call::AddCamera,
            201,
            "application/json",
            "/api/cameras",
            ADDED.as_bytes(),
        ),
        (
            Call::RemoveCamera,
            204,
            "application/json",
            "/api/cameras/cam8",
            b"",
        ),
        (
            Call::Segments,
            200,
            "application/json",
            "/api/streams/cam1/main/segments?from=0&to=100",
            SEGMENTS.as_bytes(),
        ),
        (
            Call::Events,
            200,
            "application/json",
            "/api/events?camera=cam1&from=0&to=100&limit=10",
            EVENTS.as_bytes(),
        ),
        (
            Call::Export,
            200,
            "video/mp4",
            "/api/cameras/cam1/export.mp4?from=0&to=100",
            KEYFRAME,
        ),
        (
            Call::RetainEvent,
            200,
            "application/json",
            "/api/events/7/retain",
            RETAINED.as_bytes(),
        ),
        (
            Call::Keyframe,
            200,
            "video/mp4",
            "/api/streams/cam1/main/keyframe?t=50",
            KEYFRAME,
        ),
        (
            Call::VodPlaylist,
            200,
            "application/vnd.apple.mpegurl",
            "/api/streams/cam1/main/hls/vod.m3u8?from=0&to=100",
            PLAYLIST.as_bytes(),
        ),
        (
            Call::Discover,
            200,
            "application/json",
            "/api/onvif/devices?wait_ms=3000",
            DEVICES.as_bytes(),
        ),
        (
            Call::StreamUrls,
            200,
            "application/json",
            "/api/onvif/streams",
            STREAMS.as_bytes(),
        ),
    ];

    /// Runs the check of `call` on `answer` with the fixtures' window, added
    /// camera, retained event, and page limit, and keeps only its refusal.
    fn check(call: Call, answer: &Answer<'_>) -> Result<(), ApiError> {
        let origin = origin();
        match call {
            Call::Cameras => cameras(&origin, answer).map(drop),
            Call::AddCamera => add_camera(&origin, answer, &camera("cam8")).map(drop),
            Call::RemoveCamera => remove_camera(&origin, answer),
            Call::Segments => segments(&origin, answer, window()).map(drop),
            Call::Events => events(&origin, answer, window(), 10).map(drop),
            Call::Export => export(&origin, answer),
            Call::RetainEvent => retain_event(&origin, answer, event_id(7)).map(drop),
            Call::Keyframe => keyframe(&origin, answer).map(drop),
            Call::VodPlaylist => vod_playlist(&origin, answer, window()).map(drop),
            Call::Discover => discover(&origin, answer).map(drop),
            Call::StreamUrls => stream_urls(&origin, answer).map(drop),
        }
    }

    /// Runs `assertion` on the accepted answer of every call.
    fn each_accepted(mut assertion: impl FnMut(Call, Answer<'_>)) {
        for (call, status, content_type, path, body) in ACCEPTED {
            let url = on_origin(path);
            assertion(
                call,
                Answer {
                    status,
                    content_type: Some(content_type),
                    final_url: &url,
                    body: Some(body),
                },
            );
        }
    }

    /// The calls whose check reads a body.
    fn reads_a_body(call: Call) -> bool {
        !matches!(call, Call::Export | Call::RemoveCamera)
    }

    #[test]
    fn each_check_accepts_the_answer_its_call_is_given() {
        each_accepted(|call, answer| {
            assert!(
                check(call, &answer).is_ok(),
                "{call:?}: {:?}",
                check(call, &answer)
            );
        });
    }

    #[test]
    fn each_check_returns_its_calls_value() {
        let origin = origin();
        let url = on_origin("/api/x");
        let listed = cameras(&origin, &json(&url, 200, CAMERAS.as_bytes())).unwrap();
        assert_eq!(
            listed
                .iter()
                .map(|camera| (camera.id.as_str(), camera.name.as_str()))
                .collect::<Vec<_>>(),
            [("cam1", "Front")]
        );
        let added = add_camera(&origin, &json(&url, 201, ADDED.as_bytes()), &camera("cam8"));
        let added = added.unwrap();
        assert_eq!((added.id.as_str(), added.provisioned), ("cam8", true));
        let event = retain_event(&origin, &json(&url, 200, RETAINED.as_bytes()), event_id(7));
        let event = event.unwrap();
        assert_eq!((event.id, event.retain_until), (event_id(7), Some(time(9))));
        let listed = events(&origin, &json(&url, 200, EVENTS.as_bytes()), window(), 10).unwrap();
        assert_eq!(
            listed.iter().map(|event| event.id).collect::<Vec<_>>(),
            [event_id(1)]
        );
        let devices = discover(&origin, &json(&url, 200, DEVICES.as_bytes())).unwrap();
        assert_eq!(devices[0].host, "192.0.2.20");
        let streams = stream_urls(&origin, &json(&url, 200, STREAMS.as_bytes())).unwrap();
        assert_eq!(streams[0].url, "rtsp://192.0.2.20/1");
    }

    /// An answer of `status` from `final_url` with a JSON `body`.
    fn json<'received>(
        final_url: &'received Url,
        status: u16,
        body: &'received [u8],
    ) -> Answer<'received> {
        Answer {
            status,
            content_type: Some("application/json"),
            final_url,
            body: Some(body),
        }
    }

    #[test]
    fn an_answer_from_another_origin_is_refused() {
        let others = [
            "https://other.example/api/cameras",
            "https://nvr.example/api/cameras",
            "http://nvr.example:8443/api/cameras",
        ]
        .map(|url| Url::parse(url).unwrap());
        each_accepted(|call, accepted| {
            let text = if call == Call::VodPlaylist {
                "VOD playlist response came from another origin".to_owned()
            } else {
                format!("the answer to {call} came from another origin")
            };
            for other in &others {
                let answer = Answer {
                    final_url: other,
                    ..accepted
                };
                let refused = check(call, &answer).unwrap_err();
                assert!(
                    matches!(refused, ApiError::ForeignOrigin { call: named } if named == call),
                    "{call:?} from {other}: {refused:?}"
                );
                assert_eq!(refused.to_string(), text, "{call:?} from {other}");
            }
        });
    }

    #[test]
    fn a_body_over_the_calls_limit_is_refused() {
        each_accepted(|call, accepted| {
            if !reads_a_body(call) {
                return;
            }
            let over = vec![b' '; call.max_body_bytes() + 1];
            for body in [Some(over.as_slice()), None] {
                let answer = Answer { body, ..accepted };
                let refused = check(call, &answer).unwrap_err();
                assert!(
                    matches!(refused, ApiError::Oversized { call: named } if named == call),
                    "{call:?} with {:?} bytes: {refused:?}",
                    body.map(<[u8]>::len)
                );
                assert_eq!(
                    refused.to_string(),
                    format!("the answer to {call} exceeds its byte limit")
                );
            }
        });
    }

    /// A body at the limit passes the bound and is refused for what it holds,
    /// or accepted.
    #[test]
    fn a_body_at_the_calls_limit_passes_the_bound() {
        each_accepted(|call, accepted| {
            if !reads_a_body(call) {
                return;
            }
            let at = vec![b' '; call.max_body_bytes()];
            let answer = Answer {
                body: Some(&at),
                ..accepted
            };
            assert!(
                !matches!(check(call, &answer), Err(ApiError::Oversized { .. })),
                "{call:?} at its limit"
            );
        });
    }

    /// An export's body streams to its destination after its check, and a
    /// removal's answer has none: neither check reads one.
    #[test]
    fn neither_an_export_nor_a_removal_reads_a_body() {
        each_accepted(|call, accepted| {
            if reads_a_body(call) {
                return;
            }
            let over = vec![0; 1 << 20];
            for body in [None, Some(over.as_slice())] {
                let answer = Answer { body, ..accepted };
                assert!(check(call, &answer).is_ok(), "{call:?}");
            }
        });
    }

    #[test]
    fn a_redirect_status_is_a_refusal() {
        each_accepted(|call, accepted| {
            let answer = Answer {
                status: 302,
                body: Some(b""),
                ..accepted
            };
            let refused = check(call, &answer).unwrap_err();
            assert!(
                matches!(
                    refused,
                    ApiError::Refused { call: named, status: 302, detail: None } if named == call
                ),
                "{call:?}: {refused:?}"
            );
            assert_eq!(refused.to_string(), format!("{call} failed with 302 Found"));
        });
    }

    #[test]
    fn a_refusal_carries_the_daemons_error_text() {
        each_accepted(|call, accepted| {
            let answer = Answer {
                status: 403,
                body: Some(br#"{"error":"provisioning is disabled"}"#),
                ..accepted
            };
            let refused = check(call, &answer).unwrap_err();
            assert!(
                matches!(
                    &refused,
                    ApiError::Refused { call: named, status: 403, detail: Some(detail) }
                        if *named == call && detail == "provisioning is disabled"
                ),
                "{call:?}: {refused:?}"
            );
            assert_eq!(
                refused.to_string(),
                format!("{call} failed with 403 Forbidden: provisioning is disabled")
            );
        });
    }

    /// The text of the refusal of a camera to add answered 409 with `body`,
    /// as the viewer's status bar shows it.
    fn conflict(body: Option<&[u8]>) -> String {
        let url = on_origin("/api/cameras");
        let answer = Answer {
            status: 409,
            content_type: Some("application/json"),
            final_url: &url,
            body,
        };
        add_camera(&origin(), &answer, &camera("cam8"))
            .unwrap_err()
            .to_string()
    }

    #[test]
    fn a_refusal_with_a_message_names_it_after_the_status() {
        assert_eq!(
            conflict(Some(br#"{"error":"conflict: camera cam8 already exists"}"#)),
            "adding the camera failed with 409 Conflict: conflict: camera cam8 already exists"
        );
    }

    /// A body that is not JSON, and no body, hold no message.
    #[test]
    fn a_refusal_without_a_message_names_the_status_alone() {
        for body in [Some(b"<html>not json</html>".as_slice()), None] {
            assert_eq!(
                conflict(body),
                "adding the camera failed with 409 Conflict",
                "{body:?}"
            );
        }
    }

    /// Each control character of the daemon's message shows as a space.
    #[test]
    fn a_refusal_shows_a_control_character_of_the_daemons_message_as_a_space() {
        let hostile = r#"{"error":"line\nbreak\u001b[31m red"}"#;
        assert_eq!(
            conflict(Some(hostile.as_bytes())),
            "adding the camera failed with 409 Conflict: line break [31m red"
        );
    }

    /// The daemon's message is cut to its first 200 characters.
    #[test]
    fn a_refusal_cuts_the_daemons_message_to_200_characters() {
        let long = format!(r#"{{"error":"{}"}}"#, "x".repeat(500));
        assert_eq!(
            conflict(Some(long.as_bytes())),
            format!(
                "adding the camera failed with 409 Conflict: {}",
                "x".repeat(200)
            )
        );
    }

    /// The refusal of a removal, whose answer has no body, as the viewer's
    /// status bar shows it.
    #[test]
    fn a_refused_removal_names_its_status() {
        let url = on_origin("/api/cameras/cam8");
        let removal = Answer {
            status: 403,
            content_type: None,
            final_url: &url,
            body: None,
        };
        assert_eq!(
            remove_camera(&origin(), &removal).unwrap_err().to_string(),
            "removing the camera failed with 403 Forbidden"
        );
    }

    /// A JSON body whose `error` field holds `error`, written as JSON.
    fn error_body(error: &str) -> String {
        format!(r#"{{"error":{error}}}"#)
    }

    /// The message is the string as it stands, its spaces kept. Any other
    /// value of `error`, another field, and a body that is no JSON object
    /// hold none.
    #[test]
    fn the_daemons_message_is_the_string_field_error_of_a_json_body() {
        for (body, detail) in [
            (error_body(r#""in use""#), Some("in use")),
            (error_body(r#""  in use ""#), Some("  in use ")),
            (error_body("7"), None),
            (error_body("null"), None),
            (error_body(r#"{"error":"nested"}"#), None),
            (r#"{"message":"in use"}"#.to_owned(), None),
            (r#"["in use"]"#.to_owned(), None),
            (r#""in use""#.to_owned(), None),
            ("not json".to_owned(), None),
            (String::new(), None),
        ] {
            assert_eq!(
                refusal_detail(body.as_bytes()).as_deref(),
                detail,
                "{body:?}"
            );
        }
    }

    #[test]
    fn a_control_character_of_the_daemons_message_is_a_space() {
        let body = error_body(r#""tab\there""#);
        assert_eq!(refusal_detail(body.as_bytes()).as_deref(), Some("tab here"));
    }

    /// An empty message, spaces alone, and control characters alone.
    #[test]
    fn a_blank_message_of_the_daemon_is_none() {
        for blank in [r#""""#, r#""   ""#, r#""\n\t\r""#] {
            let body = error_body(blank);
            assert_eq!(refusal_detail(body.as_bytes()), None, "{body:?}");
        }
    }

    /// The limit counts characters, not bytes.
    #[test]
    fn the_daemons_message_is_cut_to_the_character_limit() {
        let long = error_body(&format!(r#""{}""#, "é".repeat(MAX_ERROR_MESSAGE_CHARS + 1)));
        assert_eq!(
            refusal_detail(long.as_bytes()),
            Some("é".repeat(MAX_ERROR_MESSAGE_CHARS))
        );
    }

    #[test]
    fn a_message_of_the_daemon_blank_once_cut_is_none() {
        let late = error_body(&format!(r#""{}x""#, " ".repeat(MAX_ERROR_MESSAGE_CHARS)));
        assert_eq!(refusal_detail(late.as_bytes()), None);
    }

    /// The body of a refusal is read for its message only up to the error
    /// body's limit: a body one byte longer gives none.
    #[test]
    fn a_refusal_body_beyond_its_limit_gives_no_message() {
        let padded = |bytes: usize| {
            let head = r#"{"error":"in use","pad":""#;
            let tail = r#""}"#;
            let body = format!(
                "{head}{}{tail}",
                "p".repeat(bytes - head.len() - tail.len())
            );
            assert_eq!(body.len(), bytes);
            body
        };
        assert_eq!(
            refusal_detail(padded(MAX_ERROR_BODY_BYTES).as_bytes()).as_deref(),
            Some("in use")
        );
        assert_eq!(
            refusal_detail(padded(MAX_ERROR_BODY_BYTES + 1).as_bytes()),
            None
        );
    }

    /// The URL a keyframe request names.
    fn keyframe_url() -> Url {
        on_origin("/api/streams/cam1/main/keyframe?t=50")
    }

    /// The text of the refusal of a keyframe answered with `status`,
    /// `content_type`, and `body` from `final_url`. An answer the check takes
    /// fails the test at the line that asked for the text.
    #[track_caller]
    fn keyframe_refusal(
        status: u16,
        content_type: Option<&str>,
        final_url: &Url,
        body: Option<&[u8]>,
    ) -> String {
        let answer = Answer {
            status,
            content_type,
            final_url,
            body,
        };
        keyframe(&origin(), &answer).unwrap_err().to_string()
    }

    /// The refusal of a camera added with `body` for the camera `cam8`. An
    /// answer the check takes fails the test at the line that asked for it.
    #[track_caller]
    fn add_refusal(body: &str) -> ApiError {
        let url = on_origin("/api/cameras");
        add_camera(
            &origin(),
            &json(&url, 201, body.as_bytes()),
            &camera("cam8"),
        )
        .unwrap_err()
    }

    /// Another origin is refused even for a keyframe's 404.
    #[test]
    fn another_origin_is_checked_before_the_status() {
        let other = Url::parse("https://other.example/x").unwrap();
        for status in [404, 500] {
            assert_eq!(
                keyframe_refusal(status, None, &other, None),
                "the answer to requesting a keyframe came from another origin",
                "{status}"
            );
        }
    }

    #[test]
    fn the_status_is_checked_before_the_content_type() {
        assert_eq!(
            keyframe_refusal(500, Some("text/plain"), &keyframe_url(), None),
            "requesting a keyframe failed with 500 Internal Server Error"
        );
    }

    /// A refusal with a body past the bound stays a refusal.
    #[test]
    fn the_status_is_checked_before_the_byte_bound() {
        let over = vec![0; MAX_KEYFRAME_BYTES + 1];
        assert_eq!(
            keyframe_refusal(500, Some("text/plain"), &keyframe_url(), Some(&over)),
            "requesting a keyframe failed with 500 Internal Server Error"
        );
    }

    #[test]
    fn the_content_type_is_checked_before_the_byte_bound() {
        assert_eq!(
            keyframe_refusal(200, Some("text/plain"), &keyframe_url(), None),
            "response is not video/mp4"
        );
    }

    /// A keyframe whose body the transport does not hold is too large, and
    /// never empty.
    #[test]
    fn the_byte_bound_is_checked_before_the_keyframe_rule() {
        assert_eq!(
            keyframe_refusal(200, Some("video/mp4"), &keyframe_url(), None),
            "the answer to requesting a keyframe exceeds its byte limit"
        );
    }

    #[test]
    fn an_empty_keyframe_is_refused() {
        assert_eq!(
            keyframe_refusal(200, Some("video/mp4"), &keyframe_url(), Some(b"")),
            "keyframe response is empty"
        );
    }

    /// JSON cut short after a name the validator refuses, for another camera.
    #[test]
    fn decoding_is_checked_before_the_validator() {
        let refused = add_refusal(r#"{"id":"cam9","name":"\n"#);
        assert!(
            matches!(
                refused,
                ApiError::Decode {
                    call: Call::AddCamera,
                    ..
                }
            ),
            "{refused:?}"
        );
    }

    /// Another camera, whose name the validator refuses.
    #[test]
    fn the_validator_is_checked_before_the_rule_of_the_call() {
        assert_eq!(
            add_refusal(r#"{"id":"cam9","name":"\n","provisioned":true}"#).to_string(),
            "invalid camera name in response"
        );
    }

    /// A playlist with a reference to another origin and no fragment in the
    /// window it was asked for.
    #[test]
    fn a_playlist_timing_is_checked_before_its_references() {
        let foreign = PLAYLIST.replace(
            "../../../../segments/1/file.mp4?token=c4p",
            "https://other.example/file.mp4",
        );
        let later = Window::try_new(time(20_000), time(30_000)).unwrap();
        let refused = refused_playlist(foreign.as_bytes(), later);
        assert!(
            matches!(refused, ApiError::Invalid(Rule::NoFragmentInWindow)),
            "{refused:?}"
        );
    }

    /// Another event, of a kind the validator refuses.
    #[test]
    fn a_retained_event_kind_is_checked_before_its_id() {
        let url = on_origin("/api/events/7/retain");
        let blank = br#"{"id":7,"kind":"","nvr_time":5,"retain_until_us":9}"#;
        assert_eq!(
            retain_event(&origin(), &json(&url, 200, blank), event_id(8))
                .unwrap_err()
                .to_string(),
            "invalid event kind in response"
        );
    }

    #[test]
    fn an_undecodable_answer_keeps_the_serde_json_error_as_its_source() {
        each_accepted(|call, accepted| {
            if !reads_a_body(call) || matches!(call, Call::Keyframe | Call::VodPlaylist) {
                return;
            }
            let answer = Answer {
                body: Some(b"{"),
                ..accepted
            };
            let refused = check(call, &answer).unwrap_err();
            assert!(
                matches!(refused, ApiError::Decode { call: named, .. } if named == call),
                "{call:?}: {refused:?}"
            );
            assert!(refused.source().is_some(), "{call:?}");
        });
    }

    /// An added camera is answered with 201, a removal with 204, and an
    /// export with 200 alone.
    #[test]
    fn each_call_is_refused_a_status_other_than_its_own() {
        for (call, refused) in [
            (Call::AddCamera, [200, 202]),
            (Call::RemoveCamera, [200, 205]),
            (Call::Export, [201, 206]),
        ] {
            let (_, _, content_type, path, body) = ACCEPTED
                .into_iter()
                .find(|(accepted, ..)| *accepted == call)
                .unwrap();
            let url = on_origin(path);
            for status in refused {
                let answer = Answer {
                    status,
                    content_type: Some(content_type),
                    final_url: &url,
                    body: Some(body),
                };
                let named = match check(call, &answer) {
                    Err(ApiError::Refused { status, .. }) => Some(status),
                    _ => None,
                };
                assert_eq!(named, Some(status), "{call:?}");
            }
        }
    }

    #[test]
    fn the_other_calls_take_any_success_status() {
        each_accepted(|call, accepted| {
            if matches!(call, Call::AddCamera | Call::RemoveCamera | Call::Export) {
                return;
            }
            for status in [200, 299] {
                assert!(
                    check(call, &Answer { status, ..accepted }).is_ok(),
                    "{call:?} {status}"
                );
            }
            for status in [199, 300] {
                assert!(
                    matches!(
                        check(call, &Answer { status, ..accepted }),
                        Err(ApiError::Refused { .. })
                    ),
                    "{call:?} {status}"
                );
            }
        });
    }

    #[test]
    fn a_keyframe_404_says_nothing_is_recorded() {
        let url = on_origin("/api/streams/cam1/main/keyframe?t=50");
        for (content_type, body) in [
            (None, None),
            (
                Some("application/json"),
                Some(br#"{"error":"none"}"#.as_slice()),
            ),
        ] {
            let answer = Answer {
                status: 404,
                content_type,
                final_url: &url,
                body,
            };
            assert_eq!(keyframe(&origin(), &answer).unwrap(), None);
        }
    }

    #[test]
    fn an_answer_to_an_export_or_a_keyframe_must_be_video_mp4() {
        each_accepted(|call, accepted| {
            if !matches!(call, Call::Export | Call::Keyframe) {
                return;
            }
            for content_type in [None, Some("text/plain")] {
                let answer = Answer {
                    content_type,
                    ..accepted
                };
                let refused = check(call, &answer).unwrap_err();
                let named = match refused {
                    ApiError::ContentType { call, expected } => Some((call, expected)),
                    _ => None,
                };
                assert_eq!(
                    named,
                    Some((call, "video/mp4")),
                    "{call:?} {content_type:?}: {refused:?}"
                );
                assert_eq!(refused.to_string(), "response is not video/mp4");
            }
        });
    }

    #[test]
    fn a_content_type_is_its_media_type_before_any_parameter_in_any_case() {
        let url = on_origin("/api/streams/cam1/main/keyframe?t=50");
        let answer = |content_type| Answer {
            status: 200,
            content_type,
            final_url: &url,
            body: Some(KEYFRAME),
        };
        for accepted in [
            "video/mp4",
            "Video/MP4",
            " video/mp4 ; codecs=\"hvc1\"",
            "video/mp4;",
        ] {
            assert_eq!(
                keyframe(&origin(), &answer(Some(accepted))).unwrap(),
                Some(KEYFRAME),
                "{accepted:?}"
            );
        }
        for refused in [
            None,
            Some(""),
            Some("text/plain"),
            Some("video/mp4x"),
            Some("video"),
        ] {
            assert_eq!(
                keyframe(&origin(), &answer(refused))
                    .unwrap_err()
                    .to_string(),
                "response is not video/mp4",
                "{refused:?}"
            );
        }
    }

    /// A keyframe is the answer's body itself, the whole slice: the viewer
    /// keeps the body it holds instead of copying the keyframe out of it.
    #[test]
    fn a_keyframe_is_the_body_it_was_handed() {
        let url = on_origin("/api/streams/cam1/main/keyframe?t=50");
        let body = vec![7; 32 << 10];
        let answer = Answer {
            status: 200,
            content_type: Some("video/mp4"),
            final_url: &url,
            body: Some(&body),
        };
        let keyframe = keyframe(&origin(), &answer).unwrap().unwrap();
        assert!(
            ptr::eq(keyframe, body.as_slice()),
            "the keyframe is the body itself, at its address and length"
        );
    }

    /// The request of `call` its builder makes: its `body_to_read` reads by
    /// the call's rules alone.
    fn request_of(call: Call) -> HttpRequest {
        let origin = origin();
        let cam = camera("cam8");
        let lookup = StreamUrlsRequest {
            host: "192.0.2.20".into(),
            http_port: 80,
            https: false,
            tls_verify: true,
            username: "viewer".into(),
            password: "secret".into(),
        };
        match call {
            Call::Cameras => origin.cameras(),
            Call::AddCamera => origin
                .add_camera(&NewCamera {
                    id: cam,
                    name: "Garage".into(),
                    rtsp_url: "rtsp://192.0.2.10/x".into(),
                    sub_url: None,
                    username: "viewer".into(),
                    password: "secret".into(),
                    record: vec![StreamKind::Main],
                    events: Vec::new(),
                })
                .unwrap(),
            Call::RemoveCamera => origin.remove_camera(&cam),
            Call::Segments => origin.segments(&cam, StreamKind::Main, window()),
            Call::Events => origin.events(&cam, window(), 10).unwrap(),
            Call::Export => origin.export(&cam, StreamKind::Main, window()),
            Call::RetainEvent => origin.retain_event(event_id(7), Retention::Release),
            Call::Keyframe => origin.keyframe(&cam, StreamKind::Main, time(50)),
            Call::VodPlaylist => origin.vod_playlist(&cam, StreamKind::Main, window()),
            Call::Discover => origin.discover(Duration::from_secs(3)).unwrap(),
            Call::StreamUrls => origin.stream_urls(&lookup),
        }
    }

    /// `HttpRequest::body_to_read` and the checks agree, for every call over
    /// statuses and content types at the edges of their rules. A check gives
    /// the same verdict whatever the body of an answer that `body_to_read`
    /// reads no further than its head. It refuses every answer that
    /// `body_to_read` reads for a message as a refused status, and it refuses
    /// no head that `body_to_read` reads for a value.
    #[test]
    fn a_check_reads_only_the_body_that_body_to_read_reads() {
        let statuses = [
            0, 99, 100, 199, 200, 201, 202, 203, 204, 205, 206, 299, 300, 302, 404, 500, 599, 1000,
        ];
        let content_types = [
            None,
            Some(""),
            Some("video/mp4"),
            Some(" Video/MP4 ; codecs=\"hvc1\""),
            Some("video/mp4x"),
            Some("text/html"),
            Some("application/json"),
        ];
        let stray: &[u8] = &[0xff; 64];
        for (call, _, _, path, accepted) in ACCEPTED {
            let url = on_origin(path);
            let request = request_of(call);
            // No body first, then an empty one, the body the call is accepted
            // with, and bytes that are nothing the call answers with.
            let bodies = [None, Some(b"".as_slice()), Some(accepted), Some(stray)];
            for status in statuses {
                for content_type in content_types {
                    let verdicts = bodies.map(|body| {
                        let answer = Answer {
                            status,
                            content_type,
                            final_url: &url,
                            body,
                        };
                        check(call, &answer)
                    });
                    let case = format!("{call:?} {status} {content_type:?}");
                    match request.body_to_read(status, content_type) {
                        BodyRead::Nothing => {
                            let shown = verdicts.map(|verdict| verdict.map_err(|r| r.to_string()));
                            assert!(shown.iter().all(|verdict| *verdict == shown[0]), "{case}");
                        }
                        BodyRead::Message { max_bytes } => {
                            assert_eq!(max_bytes, MAX_ERROR_BODY_BYTES as u64, "{case}");
                            assert!(
                                matches!(
                                    verdicts[0],
                                    Err(ApiError::Refused { status: named, detail: None, .. })
                                        if named == status
                                ),
                                "{case}"
                            );
                        }
                        BodyRead::Value { max_bytes } => {
                            assert_eq!(max_bytes, call.max_body_bytes() as u64, "{case}");
                            assert!(
                                matches!(
                                    verdicts[0],
                                    Err(ApiError::Oversized { call: named }) if named == call
                                ),
                                "{case}"
                            );
                        }
                    }
                }
            }
        }
    }

    /// The camera `cam8` answered for `cam9`, and `cam9` answered for `cam8`.
    #[test]
    fn an_added_camera_must_be_the_one_sent() {
        let url = on_origin("/api/cameras");
        let answer = json(&url, 201, ADDED.as_bytes());
        assert_eq!(
            add_camera(&origin(), &answer, &camera("cam9"))
                .unwrap_err()
                .to_string(),
            "the daemon answered with another camera"
        );
        assert_eq!(
            add_refusal(r#"{"id":"cam9","name":"Garage","provisioned":true}"#).to_string(),
            "the daemon answered with another camera"
        );
    }

    #[test]
    fn a_retained_event_must_be_the_one_marked() {
        let url = on_origin("/api/events/7/retain");
        let answer = json(&url, 200, RETAINED.as_bytes());
        assert_eq!(
            retain_event(&origin(), &answer, event_id(8))
                .unwrap_err()
                .to_string(),
            "retain response changed the event id"
        );
    }

    /// The largest event the daemon answers a retain mark with, as its
    /// `EventDto` writes it. Its `data` is `MAX_EVENT_DATA_LEN` bytes of `"`
    /// and `\`, and its kind a vendor code of `MAX_EVENT_KIND_LEN` control
    /// characters. Every other field is as wide as it can be.
    fn largest_retained_event() -> String {
        // `serde_json` writes each backslash in a string as two.
        let data = serde_json::to_string(&"\\".repeat(MAX_EVENT_DATA_LEN / 2 - 1)).unwrap();
        assert_eq!(data.len(), MAX_EVENT_DATA_LEN);
        serde_json::json!({
            "id": i64::MAX,
            "camera": "c".repeat(32),
            "source": "onvif_pull_point",
            "kind": { "other": "\u{1}".repeat(MAX_EVENT_KIND_LEN) },
            "state": "start",
            "nvr_time": i64::MIN,
            "camera_time": i64::MIN,
            "data": data,
            "retain_until_us": i64::MIN,
        })
        .to_string()
    }

    /// The largest event passes the byte bound and decodes, and the check
    /// refuses it for its kind.
    #[test]
    fn the_largest_event_the_daemon_retains_passes_the_byte_bound() {
        let url = on_origin("/api/events/9223372036854775807/retain");
        let body = largest_retained_event();
        let answer = json(&url, 200, body.as_bytes());
        let refused = retain_event(&origin(), &answer, event_id(i64::MAX)).unwrap_err();
        assert!(
            matches!(refused, ApiError::Invalid(Rule::EventKindLabel)),
            "{} bytes: {refused:?}",
            body.len()
        );
    }

    #[test]
    fn a_retain_answer_a_byte_past_its_bound_is_refused() {
        let url = on_origin("/api/events/7/retain");
        let over = vec![b' '; MAX_RETAIN_BODY_BYTES + 1];
        let refused = retain_event(&origin(), &json(&url, 200, &over), event_id(7)).unwrap_err();
        assert!(
            matches!(
                refused,
                ApiError::Oversized {
                    call: Call::RetainEvent
                }
            ),
            "{refused:?}"
        );
        assert_eq!(
            refused.to_string(),
            "the answer to retaining the event exceeds its byte limit"
        );
    }

    /// An event's id is positive, as every `EventId` is, so a retained event
    /// or a page of events that carries another does not decode.
    #[test]
    fn an_event_whose_id_is_not_positive_does_not_decode() {
        let url = on_origin("/api/x");
        let retained = br#"{"id":0,"kind":"motion","nvr_time":5,"retain_until_us":9}"#;
        let refused = retain_event(&origin(), &json(&url, 200, retained), event_id(7)).unwrap_err();
        assert!(
            matches!(
                refused,
                ApiError::Decode {
                    call: Call::RetainEvent,
                    ..
                }
            ),
            "{refused:?}"
        );
        assert_eq!(
            refused.source().map(ToString::to_string).as_deref(),
            Some("invalid event id at line 1 column 7")
        );
        let listed = br#"[{"id":-1,"kind":"motion","nvr_time":5,"retain_until_us":null}]"#;
        let refused = events(&origin(), &json(&url, 200, listed), window(), 10).unwrap_err();
        assert!(
            matches!(
                refused,
                ApiError::Decode {
                    call: Call::Events,
                    ..
                }
            ),
            "{refused:?}"
        );
    }

    #[test]
    fn listed_answers_pass_their_validators() {
        let url = on_origin("/api/x");
        let origin = origin();
        assert_eq!(
            cameras(
                &origin,
                &json(
                    &url,
                    200,
                    br#"[{"id":"cam1","name":" ","provisioned":false}]"#
                )
            )
            .unwrap_err()
            .to_string(),
            "invalid camera name in response"
        );
        assert_eq!(
            segments(
                &origin,
                &json(
                    &url,
                    200,
                    br#"[{"wall_start_us":200,"wall_end_us":300,"sealed":true}]"#
                ),
                window()
            )
            .unwrap_err()
            .to_string(),
            "segment is outside the requested window"
        );
        assert_eq!(
            events(&origin, &json(&url, 200, EVENTS.as_bytes()), window(), 0)
                .unwrap_err()
                .to_string(),
            "invalid event page limit"
        );
        let late = br#"[{"id":1,"kind":"motion","nvr_time":100,"retain_until_us":null}]"#;
        assert_eq!(
            events(&origin, &json(&url, 200, late), window(), 10)
                .unwrap_err()
                .to_string(),
            "event is outside the requested window"
        );
        let unhosted = concat!(
            r#"[{"endpoint":"","host":"","port":80,"https":false,"#,
            r#""name":null,"hardware":null,"location":null}]"#
        );
        assert_eq!(
            discover(&origin, &json(&url, 200, unhosted.as_bytes()))
                .unwrap_err()
                .to_string(),
            "the daemon listed a device with an invalid host"
        );
        assert_eq!(
            stream_urls(&origin, &json(&url, 200, b"[]"))
                .unwrap_err()
                .to_string(),
            "the daemon listed no usable stream"
        );
    }

    /// The segments a window lists leave out the open rows that recorded
    /// nothing in it.
    #[test]
    fn segments_that_recorded_nothing_in_the_window_are_dropped() {
        let url = on_origin("/api/streams/cam1/main/segments");
        let rows = concat!(
            r#"[{"wall_start_us":10,"wall_end_us":20,"sealed":true},"#,
            r#"{"wall_start_us":50,"wall_end_us":50,"sealed":false},"#,
            r#"{"wall_start_us":60,"wall_end_us":70,"sealed":false}]"#
        );
        let listed = segments(&origin(), &json(&url, 200, rows.as_bytes()), window()).unwrap();
        assert_eq!(
            listed
                .iter()
                .map(|segment| (segment.start, segment.end))
                .collect::<Vec<_>>(),
            [(time(10), time(20)), (time(60), time(70))]
        );
    }

    /// The playlist the check of [`PLAYLIST`] gives, from the URL a playlist
    /// request names.
    fn checked_playlist() -> VodPlaylist {
        let url = on_origin("/api/streams/cam1/main/hls/vod.m3u8?from=0&to=100");
        let answer = Answer {
            status: 200,
            content_type: None,
            final_url: &url,
            body: Some(PLAYLIST.as_bytes()),
        };
        vod_playlist(&origin(), &answer, window()).unwrap()
    }

    #[test]
    fn a_checked_playlist_holds_the_timing_of_its_body() {
        assert_eq!(
            checked_playlist().fragments(),
            [PlaybackFragment {
                wall_start: time(10),
                duration: MediaTicks::new(900),
            }]
        );
    }

    #[test]
    fn a_playlist_resolves_against_the_url_it_came_from() {
        let playlist = checked_playlist();
        assert!(
            playlist.body().contains(
                "\n#EXT-X-MAP:URI=\"https://nvr.example:8443/api/segments/1/file.mp4?token=c4p\"\n"
            ),
            "{}",
            playlist.body()
        );
        assert!(
            playlist
                .body()
                .contains("\nhttps://nvr.example:8443/api/segments/1/file.mp4?token=c4p\n"),
            "{}",
            playlist.body()
        );
    }

    /// The refusal of a VOD playlist answered with `body` for `window`, from
    /// the URL a playlist request names; an answer that is no refusal fails
    /// the test at the line that asked for it.
    #[track_caller]
    fn refused_playlist(body: &[u8], window: Window) -> ApiError {
        let url = on_origin("/api/streams/cam1/main/hls/vod.m3u8?from=0&to=100");
        let answer = Answer {
            status: 200,
            content_type: None,
            final_url: &url,
            body: Some(body),
        };
        vod_playlist(&origin(), &answer, window).unwrap_err()
    }

    /// A body that is not UTF-8 is refused as a playlist that is not UTF-8,
    /// with that refusal's text.
    #[test]
    fn a_playlist_that_is_not_utf8_is_refused_with_its_text() {
        let refused = refused_playlist(b"#EXTM3U\n\xff\n", window());
        assert!(
            matches!(refused, ApiError::Playlist(PlaylistError::NotUtf8(_))),
            "{refused:?}"
        );
        assert_eq!(refused.to_string(), "VOD playlist is not UTF-8");
    }

    #[test]
    fn a_playlist_that_is_not_utf8_keeps_the_utf8_error_as_its_source() {
        let refused = refused_playlist(b"#EXTM3U\n\xff\n", window());
        assert_eq!(
            refused.source().map(ToString::to_string).as_deref(),
            Some("invalid utf-8 sequence of 1 bytes from index 8")
        );
    }

    #[test]
    fn a_playlist_without_fragment_timing_is_refused() {
        let refused = refused_playlist(b"#EXTM3U\n", window());
        assert!(
            matches!(refused, ApiError::Playlist(PlaylistError::NoFragmentTiming)),
            "{refused:?}"
        );
        assert_eq!(
            refused.to_string(),
            "VOD playlist has no fragment timing metadata"
        );
    }

    #[test]
    fn a_playlist_without_a_fragment_in_the_window_is_refused() {
        let later = Window::try_new(time(20_000), time(30_000)).unwrap();
        let refused = refused_playlist(PLAYLIST.as_bytes(), later);
        assert!(
            matches!(refused, ApiError::Invalid(Rule::NoFragmentInWindow)),
            "{refused:?}"
        );
        assert_eq!(
            refused.to_string(),
            "VOD playlist has no fragment in the requested window"
        );
    }

    #[test]
    fn a_playlist_that_references_another_origin_is_refused() {
        let foreign = PLAYLIST.replace(
            "../../../../segments/1/file.mp4?token=c4p",
            "https://other.example/file.mp4",
        );
        let refused = refused_playlist(foreign.as_bytes(), window());
        assert!(
            matches!(
                refused,
                ApiError::Playlist(PlaylistError::MediaUri(ReferenceError::OtherOrigin))
            ),
            "{refused:?}"
        );
    }

    #[test]
    fn an_answer_debug_hides_its_body() {
        let url = on_origin("/api/streams/cam1/main/hls/vod.m3u8");
        let answer = Answer {
            status: 200,
            content_type: Some("application/vnd.apple.mpegurl"),
            final_url: &url,
            body: Some(PLAYLIST.as_bytes()),
        };
        let shown = format!("{answer:?}");
        assert!(!shown.contains("token="), "{shown}");
        assert!(
            shown.contains(&format!("body_bytes: Some({})", PLAYLIST.len())),
            "{shown}"
        );
        let unread = format!(
            "{:?}",
            Answer {
                body: None,
                ..answer
            }
        );
        assert!(unread.contains("body_bytes: None"), "{unread}");
    }

    #[test]
    fn an_answer_debug_shows_its_status() {
        let url = on_origin("/api/cameras");
        let shown = format!("{:?}", json(&url, 403, b"[]"));
        assert!(shown.contains("status: 403"), "{shown}");
    }

    /// An answer's URL shows as it stands without userinfo, a query, or a
    /// fragment, and with each of them redacted, whatever URL a transport
    /// hands over.
    #[test]
    fn an_answer_debug_shows_its_url_without_userinfo_query_or_fragment() {
        let plain = on_origin("/api/cameras");
        let shown = format!("{:?}", json(&plain, 200, b"[]"));
        assert!(
            shown.contains(&format!("final_url: {:?}", plain.as_str())),
            "{shown}"
        );
        let url =
            Url::parse("https://viewer:hidden@nvr.example:8443/api/cameras?token=hidden#hidden")
                .unwrap();
        let shown = format!("{:?}", json(&url, 200, b"[]"));
        let redacted = concat!(
            r#"final_url: "https://<redacted>@nvr.example:8443"#,
            r#"/api/cameras?<redacted>#<redacted>""#
        );
        assert!(shown.contains(redacted), "{shown}");
        assert!(
            !shown.contains("hidden") && !shown.contains("viewer"),
            "{shown}"
        );
    }
}
