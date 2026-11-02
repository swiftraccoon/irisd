//! Where finished fragments go.

use std::fmt;

use iris_core::id::StreamKey;
use iris_mp4::fragment::Fragment;

/// Receives fragments from ingest.
///
/// Implementations must not block: the camera socket is read on the same
/// task, so a slow sink stalls the stream. Reject with [`SinkError`] instead
/// (the daemon's sink is a bounded queue); the fragment is then dropped and
/// counted in [`crate::StreamStats::dropped_sink`].
pub trait FragmentSink: Send + Sync {
    /// Accepts a fragment, or rejects it (for example, a full queue).
    fn send(&self, key: &StreamKey, fragment: Fragment) -> Result<(), SinkError>;
}

/// The sink rejected a fragment.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SinkError;

impl fmt::Display for SinkError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("sink rejected fragment")
    }
}

impl std::error::Error for SinkError {}
