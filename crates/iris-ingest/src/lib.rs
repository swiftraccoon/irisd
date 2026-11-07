//! RTSP ingest for irisd: one `retina` session per stream, a constant-frame-rate
//! timestamp regulariser, and a fragment assembler that hands complete
//! fragments to a [`FragmentSink`].
//!
//! The record path never decodes. Frames arrive as 4-byte-length-prefixed
//! NAL units (the `hvc1` sample format), are grouped from one random access
//! point to the next (or to a configured cap), given regularised durations,
//! and leave as [`iris_mp4::fragment::Fragment`]s. Every frame is also
//! published to the stream's [`LiveRing`] before its fragment closes.
//!
//! # Modules
//!
//! - [`run_stream`] records one [`StreamSource`] under [`IngestOptions`] until a
//!   shutdown token is cancelled, reconnecting after every failure.
//! - [`regularize`] assigns frame durations: the nominal rate, slow lock to the
//!   camera's timeline, holds for dropped frames, snaps when the error exceeds
//!   the tolerance.
//! - [`assembler`] groups frames into fragments and keeps the payload checksum.
//! - [`live`] fans frames out to live viewers without copying.
//! - [`StreamStats`] exposes the per-stream counters the daemon logs.
//!
//! # Invariants
//!
//! - The NVR clock is the authority for wall time; RTP timestamps only give
//!   relative timing through the regulariser.
//! - A fragment starts at a sync frame; frames before the first sync frame of a
//!   session are dropped; a discontinuity starts a new run with the decode
//!   timeline at zero.
//! - [`FragmentSink::send`] runs on the ingest task and must not block: a
//!   rejected fragment is dropped and counted.
//! - With [`IngestOptions::read_interval`] set, sockets are drained on a tick
//!   shared by every stream instead of on every packet; with
//!   [`IngestOptions::recv_low_water`] set, the kernel reports a socket
//!   readable once per that many bytes. A camera that stops reading never
//!   blocks the runtime thread: the session fails at its next keepalive.
//! - A session error, which quotes camera-supplied text, is logged as one
//!   escaped line of at most 512 characters.
//!
//! # Example
//!
//! The regulariser can be driven on its own; the daemon drives it through
//! [`run_stream`].
//!
//! ```
//! use iris_core::config::TimestampMode;
//! use iris_core::time::MediaTicks;
//! use iris_ingest::regularize::Regularizer;
//!
//! let mut regularizer = Regularizer::new(TimestampMode::Regularize, Some(3000), MediaTicks::from_millis(150));
//! let raw = [0, 2700, 6300, 9000];
//! let assignment = regularizer.assign(&raw, 12_000);
//! assert_eq!(assignment.durations.iter().sum::<u32>(), 12_000);
//! ```

pub mod assembler;
pub mod live;
pub mod regularize;
mod session;
mod sink;
mod source;
mod stats;

pub use live::{LiveError, LiveFrame, LiveRing, LiveSubscription};
pub use session::{ReadTicker, deliver_fragment, run_stream};
pub use sink::{FragmentSink, SinkError};
pub use source::{IngestOptions, StreamSource};
pub use stats::StreamStats;
/// The URL type accepted by [`StreamSource`], re-exported so callers need not depend on `url`.
pub use url::Url;
