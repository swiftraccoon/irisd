//! Shared identifiers, time units, event types, configuration, and metrics for irisd.
//!
//! The recorder's crates speak in these types so that a wall-clock
//! microsecond is never confused with a 90 kHz media tick, and a camera id is
//! never confused with a stream key.
//!
//! # Modules
//!
//! - [`id`]: [`id::CameraId`] (validated, path- and URL-safe), [`id::StreamKind`],
//!   and [`id::StreamKey`], the name of one recorded stream.
//! - [`time`]: [`time::WallUs`], microseconds on the NVR clock and the only wall
//!   clock the system trusts, and [`time::MediaTicks`] at [`time::TIMESCALE`]
//!   (90 kHz), the scale of every recording.
//! - [`event`]: [`event::Event`], one normalised camera event placed on the NVR
//!   clock, with its [`event::EventSource`], [`event::EventKind`], and
//!   [`event::EventState`].
//! - [`config`]: [`config::Config`], the TOML configuration and its defaults.
//! - [`metrics`]: [`metrics::Histogram`] and [`metrics::Peak`], lock-free atomic
//!   counters for the statistics lines.
//! - [`text`]: [`text::LogSafe`], peer-controlled text as one bounded, escaped
//!   log line.
//! - [`nonce`]: [`nonce::client_nonce`], a random digest client nonce.
//!
//! # Invariants
//!
//! Media time is 90 kHz everywhere, so one millisecond is exactly 90 ticks.
//! [`time::WallUs`] places, searches, and expires recordings; camera clocks are
//! recorded for diagnostics only. NTP64 conversions round trip exactly to the
//! microsecond from 1970 through 2104 (both NTP eras), which the `prft` box
//! depends on.
//!
//! ```
//! use iris_core::time::{MediaTicks, TIMESCALE, WallUs};
//!
//! let two_seconds = MediaTicks::from_secs(2);
//! assert_eq!(two_seconds.get(), 2 * i64::from(TIMESCALE));
//! assert_eq!(MediaTicks::from_millis(1).get(), 90);
//!
//! let wall = WallUs::new(1_700_000_000_123_456);
//! assert_eq!(WallUs::from_ntp64(wall.to_ntp64()), wall);
//! ```

pub mod config;
pub mod event;
pub mod id;
pub mod metrics;
pub mod nonce;
pub mod text;
pub mod time;
