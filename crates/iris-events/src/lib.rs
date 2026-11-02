//! Camera event sources for irisd.
//!
//! Events come from the camera, never from decoding video on the recorder.
//! Two sources are implemented: the Dahua-style HTTP CGI push feed
//! ([`run_dahua_cgi`]) and ONVIF pull-point subscriptions
//! ([`run_onvif_pullpoint`]). Both normalise what they receive into an
//! [`iris_core::event::Event`], place it on the recorder's clock (a camera
//! timestamp is carried alongside, never used as the time base), and hand it
//! to an [`EventSink`]. Each source keeps [`EventStats`] for the daemon's
//! statistics line and reconnects with jittered exponential backoff.
//!
//! # Modules
//!
//! - [`dahua`] — parsing and normalisation of CGI event parts.
//! - [`onvif`] — topic mapping, notification normalisation, ISO 8601 parsing.
//! - [`multipart`] — a bounded, incremental `multipart/x-mixed-replace` reader.
//! - [`digest`] — HTTP digest authentication (RFC 7616, MD5).
//! - [`CameraEndpoint`] — how to reach a camera's HTTP interface; the password
//!   is redacted from `Debug`.
//!
//! # Invariants
//!
//! - [`EventSink::publish`] runs on the receive loop and must not block.
//! - The ONVIF pull timeout stays under the 10 s that the HTTP transport
//!   applies to every request, and subscriptions are renewed before they
//!   expire; WS-Security tokens fall back to host time for cameras that
//!   misreport UTC.
//! - Credentials never reach logs, URLs, or errors.
//!
//! # Example
//!
//! ```
//! use iris_core::id::CameraId;
//! use iris_core::time::WallUs;
//! use iris_events::dahua;
//!
//! let raw = dahua::parse_part("Code=VideoMotion;action=Start;index=0").expect("an event");
//! let camera = CameraId::try_new("cam1")?;
//! let event = dahua::normalize(&camera, &raw, WallUs::now()).expect("mapped");
//! assert_eq!(event.kind, iris_core::event::EventKind::Motion);
//! # Ok::<(), iris_core::id::InvalidCameraId>(())
//! ```

mod backoff;
pub mod dahua;
pub mod digest;
mod endpoint;
mod feed;
pub mod multipart;
pub mod onvif;
mod stats;

pub use endpoint::{CameraEndpoint, EventSink};
pub use feed::run_dahua_cgi;
pub use onvif::run_onvif_pullpoint;
pub use stats::EventStats;
