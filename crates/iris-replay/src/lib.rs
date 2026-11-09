//! A minimal RTSP/1.0 server that replays irisd recordings as synthetic H.265
//! cameras over TCP-interleaved RTP, for benchmarks and fault injection without
//! hardware.
//!
//! Each mount serves one recording: its samples are re-packetized per RFC 7798
//! (single NAL unit packets and fragmentation units), parameter sets from the
//! file's `hvcC` are sent in-band before every sync sample as cameras do, RTCP
//! sender reports go out every five seconds, and frames are paced by their
//! decode times. Every connection gets its own session, so one process stands
//! in for as many cameras as there are connections.
//!
//! # Modules
//!
//! - [`media`] — [`Media`](media::Media) loads a recording and indexes its frames.
//! - [`rtp`] — fallible streaming [`Packetizer`](rtp::Packetizer), sender
//!   reports, and interleaving.
//! - [`rtsp`] — request parsing and responses.
//! - [`sdp`] — the session description.
//! - [`server`] — [`Server`](server::Server) and the delivery [`Options`](server::Options):
//!   per-packet writes, pacing, or bursts ([`Delivery`](server::Delivery)), dropped packets,
//!   stalls.
//! - [`synth`] — fallible [`recording`](synth::recording) construction from a
//!   bounded [`Plan`](synth::Plan).
//!
//! # Example
//!
//! ```
//! use iris_replay::media::Media;
//! use iris_replay::synth::{self, Plan};
//!
//! let bytes = synth::recording(Plan { gops: 2, frames_per_gop: 3, ..Plan::default() })?;
//! let media = Media::from_bytes(bytes)?;
//! assert_eq!(media.frames().len(), 6);
//! assert!(media.frames()[0].is_sync && !media.frames()[1].is_sync);
//! assert_eq!(media.parameter_sets().len(), 3);
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```

pub mod media;
pub mod rtp;
pub mod rtsp;
pub mod sdp;
pub mod server;
pub mod synth;
