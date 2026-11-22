//! The Linux viewer's transport. `iris-client` builds the requests of
//! [`iris_client::api`] and checks their answers, without input or output.
//! This crate carries them. It holds three parts: the HTTP client, which
//! sends each request to the daemon and hands its answer to the call's
//! check; the export download; and the session loop of a live feed. Only the
//! Linux viewer (`iris-gui`) depends on the crate. The planned iOS and macOS
//! viewer that `docs/research/15-apple-clients.md` §4 designs carries its
//! requests and feeds through its platform, so `iris-client` holds no
//! networking or TLS code.
//!
//! A [`Bearer`] is the root token, checked once to be one or more characters
//! of visible ASCII. [`Client`] sends every request with the bearer it was
//! given, if any, in `Authorization`, and follows no redirect. Once an
//! answer's head is in, it reads the part of the body that
//! [`HttpRequest::body_to_read`] asks for and hands the answer to the call's
//! check in [`answer`](iris_client::api::answer). A failed call is a
//! [`ClientError`], whose text is the message the viewer shows, and a failed
//! export an [`ExportError`]. [`Client::export`] streams an export into an
//! exclusive temporary beside its destination, which it renames into place
//! once the whole file is synced, and stops when the destination's free space
//! runs low. [`Feed::run`] connects a [`Feed`], pushes every message into a
//! [`LiveSink`], and connects again as the feed's [`Reconnect`] says until it
//! is cancelled.
//!
//! A feed's bearer travels in its WebSocket handshake, whose whole request
//! tungstenite writes at trace level, `Authorization` included, under the
//! log target that [`is_sensitive_log_target`] names. An application that
//! installs a log subscriber must drop every event of that target, whatever
//! its filter says, as the Linux viewer's output layer does.
//!
//! # Panics
//!
//! The calls of a [`Client`], [`Client::export`] among them, and
//! [`Feed::run`] run on a Tokio runtime with its I/O and time drivers enabled
//! (`Builder::enable_all`, or `enable_io` and `enable_time`). They panic when
//! polled outside one, or on a runtime that lacks one of the two drivers.
//! The HTTP and WebSocket connections need the I/O driver, and the read
//! deadlines and the feed's pauses need the timer. An export's file work runs
//! on the runtime's blocking pool. [`Client::new`] and [`Bearer::try_new`]
//! need no runtime.
//!
//! # Example
//!
//! ```no_run
//! # use std::error::Error;
//! use std::env;
//! use std::sync::Arc;
//!
//! use iris_client::origin::{ApiOrigin, PlainHttp};
//! use iris_transport::{Bearer, Client};
//!
//! # async fn list() -> Result<(), Box<dyn Error>> {
//! let api = "https://nvr.example:8443";
//! let origin = ApiOrigin::parse(api, PlainHttp::LoopbackOnly)?;
//! let tls = Arc::unwrap_or_clone(iris_tls::client_config(None)?);
//! // The daemon trims a presented token and treats an empty one as none.
//! let variable = env::var("IRISD_TOKEN").unwrap_or_default();
//! let token = Some(variable.trim()).filter(|token| !token.is_empty());
//! let bearer = token.map(Bearer::try_new).transpose()?;
//! let client = Client::new(origin, bearer, tls);
//! for camera in client.cameras().await? {
//!     println!("{} ({})", camera.name, camera.id);
//! }
//! # Ok(())
//! # }
//! ```
//!
//! [`HttpRequest::body_to_read`]: iris_client::api::HttpRequest::body_to_read
//! [`LiveSink`]: iris_client::live::LiveSink
//! [`Reconnect`]: iris_client::live::Reconnect

mod client;
mod export;
mod feed;
#[cfg(test)]
mod fixture;

pub use client::{Bearer, BearerError, Client, ClientError};
pub use export::ExportError;
pub use feed::{Feed, is_sensitive_log_target};

/// Compiles and runs the example of `README.md` as a doctest.
#[cfg(doctest)]
#[doc = include_str!("../README.md")]
struct ReadmeDoctests;
