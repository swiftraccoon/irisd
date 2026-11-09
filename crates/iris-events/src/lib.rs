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
//! - [`onvif`] — the pull-point session, topic mapping, notification
//!   normalisation, ISO 8601 parsing.
//! - [`media`] — Media service lookups: [`snapshot_url`] and [`stream_urls`],
//!   with [`MediaUrlError`] naming what went wrong without camera text.
//! - [`multipart`] — a bounded, incremental `multipart/x-mixed-replace` reader.
//! - [`digest`] — HTTP digest authentication (RFC 7616, MD5).
//! - [`discovery`] — WS-Discovery of ONVIF cameras, bounded and sanitised, for
//!   the Add camera flow.
//! - [`CameraEndpoint`] — how to reach a camera's HTTP interface; its
//!   credentials are redacted from `Debug`.
//!
//! # Invariants
//!
//! - [`EventSink::publish`] runs on the receive loop and must not block.
//! - The ONVIF pull timeout stays under the 10 s that the SOAP transport
//!   applies to every request, and subscriptions are renewed before they
//!   expire from the lifetime the camera granted; WS-Security tokens fall back
//!   to host time only for structured clock-authentication faults from cameras
//!   that misreport UTC.
//! - Camera-advertised ONVIF service URLs stay on the configured origin and
//!   stream URLs on the configured host; every advertised URL is bounded in
//!   length. Authenticated SOAP does not follow redirects or proxies and has
//!   byte and XML-structure limits.
//! - Credentials never reach logs, URLs, or errors, and camera text reaches a
//!   log line only through [`iris_core::text::LogSafe`].
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
pub mod discovery;
mod endpoint;
mod feed;
#[cfg(test)]
mod loopback;
pub mod media;
pub mod multipart;
pub mod onvif;
mod sink;
mod stats;
mod text;
mod transport;

pub use discovery::{DiscoveredCamera, discover};
pub use endpoint::CameraEndpoint;
pub use feed::run_dahua_cgi;
pub use media::{MediaUrlError, ProfileStream, snapshot_url, stream_urls};
pub use onvif::run_onvif_pullpoint;
pub use sink::EventSink;
pub use stats::EventStats;
pub use transport::MAX_ADVERTISED_URL_LEN;
