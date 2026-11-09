//! Where finished fragments go.

use iris_core::id::StreamKey;
use iris_mp4::fragment::Fragment;
use thiserror::Error;

/// Receives fragments from ingest.
///
/// Implementations must not block: the camera socket is read on the same
/// task, so a slow sink stalls the stream. Reject with [`SinkError`] instead
/// (the daemon's sink is a bounded queue); the fragment is then dropped and
/// counted in [`crate::StreamStats::dropped_sink`].
pub trait FragmentSink: Send + Sync {
    /// Accepts a fragment, or rejects it (for example, a full queue).
    ///
    /// # Errors
    ///
    /// [`SinkError`] when the sink cannot take the fragment without blocking.
    fn send(&self, key: &StreamKey, fragment: Fragment) -> Result<(), SinkError>;
}

/// The sink rejected a fragment.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Error)]
#[error("sink rejected fragment")]
pub struct SinkError;
