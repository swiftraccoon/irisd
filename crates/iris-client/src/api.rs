//! The irisd HTTP API as a viewer uses it, without a transport. The module
//! holds its requests, the checks their answers pass, and the answers as the
//! viewer decodes them. It also holds the bounds each is checked against and
//! the rules each passes. A request that
//! names a time range takes a [`Window`]. A method of
//! [`ApiOrigin`](crate::origin::ApiOrigin) builds each call's [`HttpRequest`].
//! The viewer's transport sends the request with the bearer. Once the
//! answer's head arrives, the transport asks [`HttpRequest::body_to_read`]
//! how much of the body to read. It then hands the [`Answer`] to the call's
//! check in [`answer`], which returns the value the viewer may use.
//! A refusal is an [`ApiError`] whose text is the message a viewer shows; a
//! [`Call`] names the request it answered, and a [`Rule`] the rule an answer
//! or a request broke.
//!
//! # Example
//!
//! ```
//! # use std::error::Error;
//! #
//! use iris_client::api::{self, Camera, Window};
//! use iris_core::time::WallUs;
//!
//! let body = r#"[{"id":"cam1","name":"Front","provisioned":false}]"#;
//! let cameras: Vec<Camera> = serde_json::from_str(body)?;
//! api::validate_cameras(&cameras)?;
//! let window = Window::try_new(WallUs::new(0), WallUs::new(60_000_000))?;
//! assert_eq!(window.to(), WallUs::new(60_000_000));
//! let Err(reversed) = Window::try_new(window.to(), window.from()) else {
//!     return Err("a reversed window is refused".into());
//! };
//! assert_eq!(reversed.to_string(), "invalid time range");
//! # Ok::<(), Box<dyn Error>>(())
//! ```

pub mod answer;
mod error;
mod limits;
mod request;
mod types;
mod validate;

pub use answer::Answer;
pub use error::{ApiError, Call, Rule};
pub use limits::{
    DISCOVERY_WAIT_MS, MAX_CAMERA_BODY_BYTES, MAX_CAMERAS, MAX_CAMERAS_BODY_BYTES,
    MAX_DEVICES_BODY_BYTES, MAX_DISCOVERED_DEVICES, MAX_DISPLAY_STRING_BYTES, MAX_ENDPOINT_CHARS,
    MAX_ERROR_BODY_BYTES, MAX_ERROR_MESSAGE_CHARS, MAX_EVENTS, MAX_HOST_BYTES, MAX_KEYFRAME_BYTES,
    MAX_LABEL_CHARS, MAX_LIST_BODY_BYTES, MAX_PROFILE_STREAMS, MAX_RANGE_US, MAX_RETAIN_BODY_BYTES,
    MAX_SEGMENTS, MAX_STREAM_URL_BYTES, MAX_STREAMS_BODY_BYTES,
};
pub use request::{BodyRead, HttpRequest, Method, READ_IDLE, STREAM_LOOKUP_READ_IDLE};
pub use types::{
    Camera, DiscoveredDevice, EventId, EventKind, EventRecord, NewCamera, ProfileStream, Retention,
    Segment, StreamUrlsRequest, VodPlaylist,
};
pub use validate::{
    StreamUrlProblem, Window, is_valid_camera_text, is_valid_device_host, is_valid_display_string,
    recorded_in_window, rtsp_url_problem, validate_cameras, validate_devices, validate_event,
    validate_events, validate_playback_fragments, validate_segments, validate_streams,
};
