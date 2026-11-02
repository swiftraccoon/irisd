//! Notifications about recording files, delivered on the writer thread.

use std::path::PathBuf;
use std::sync::Arc;

use iris_core::id::StreamKey;
use iris_core::time::WallUs;
use iris_mp4::init::SampleEntry;

/// A recording file was created.
#[derive(Clone, Debug)]
pub struct SegmentOpened {
    /// The stream the file belongs to.
    pub key: StreamKey,
    /// Path of the `.mp4`; the sidecar is at [`crate::layout::index_path`].
    pub path: PathBuf,
    /// The stream's run counter; increments at every timing discontinuity.
    pub run_seq: u64,
    /// Whether a discontinuity precedes the first fragment.
    pub discontinuity: bool,
    /// Wall time of the first sample.
    pub wall_start: WallUs,
    /// Decode time of the first sample in 90 kHz ticks.
    pub media_start: u64,
    /// Length of `ftyp` + `moov`.
    pub init_len: u32,
    /// Codec configuration of the file.
    pub sample_entry: Arc<SampleEntry>,
}

/// Bytes of a file became durable (after `fdatasync`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SegmentProgress {
    /// Path of the `.mp4`.
    pub path: PathBuf,
    /// Bytes written and synced so far.
    pub durable_len: u64,
    /// Fragments written so far.
    pub fragments: u32,
    /// Wall time at the end of the last fragment.
    pub wall_end: WallUs,
    /// Decode time at the end of the last fragment.
    pub media_end: u64,
}

/// A file was sealed and will not change again.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SegmentSealed {
    /// Path of the `.mp4`.
    pub path: PathBuf,
    /// Final length of the `.mp4`.
    pub len: u64,
    /// Fragments in the file.
    pub fragments: u32,
    /// Wall time at the end of the last fragment.
    pub wall_end: WallUs,
    /// Decode time at the end of the last fragment.
    pub media_end: u64,
}

/// Receives file lifecycle notifications from the writer thread.
///
/// Implementations must be cheap and must never block: they run on the thread
/// that writes every stream's data.
pub trait StoreObserver: Send + Sync {
    /// A file was created.
    fn segment_opened(&self, info: SegmentOpened);
    /// A file's data and index were `fdatasync`ed.
    fn segment_progress(&self, info: SegmentProgress);
    /// A file was sealed.
    fn segment_sealed(&self, info: SegmentSealed);
}

/// An observer that ignores every notification.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoopObserver;

impl StoreObserver for NoopObserver {
    fn segment_opened(&self, _info: SegmentOpened) {}

    fn segment_progress(&self, _info: SegmentProgress) {}

    fn segment_sealed(&self, _info: SegmentSealed) {}
}
