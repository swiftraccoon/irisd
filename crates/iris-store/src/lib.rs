//! Recording files for irisd: segment files, preallocation, sync policy, the
//! writer thread, and recovery.
//!
//! Ingest hands completed [`Fragment`]s to a [`WriterHandle`]; a dedicated
//! thread per video root appends them to per-stream recording files laid out
//! as `<root>/<camera>/<kind>/<YYYY-MM-DD>/<HHMMSS.ffffff>Z.mp4` with a sidecar
//! `.idx`, preallocates space ahead of the write position, `fdatasync`s on a
//! timer, rotates files by size or age, and seals them on shutdown. A file
//! that was open when the process died is repaired by
//! [`recover::recover_segment`].
//!
//! # Modules
//!
//! - [`WriterHandle`] and [`WriterStats`]: the queue into the writer thread and
//!   its counters; [`QueueFull`] is the only way a send fails.
//! - [`StoragePolicy`]: sync interval, preallocation, rotation thresholds.
//! - [`StoreObserver`] with [`SegmentOpened`], [`SegmentProgress`], and
//!   [`SegmentSealed`]: file lifecycle notifications, delivered on the writer
//!   thread; [`NoopObserver`] ignores them.
//! - [`layout`]: path naming.
//! - [`recover`]: truncate a damaged file to its valid prefix and rewrite its
//!   index.
//! - [`create_recording_root`], [`free_space`], [`free_space_approved`], and
//!   [`remove_segment`]: durable root preparation and the file-system
//!   operations retention needs.
//!
//! # Invariants
//!
//! - The writer thread is the only writer under a root; ingest tasks never
//!   block on it, and a full queue drops the fragment.
//! - [`WriterHandle::spawn_with_roots`] tries roots in order through bounded
//!   queues. Failed writes are retried on the fallback root; each file starts
//!   at a sync sample, forwarding loss discards dependent fragments until a
//!   new run can start, and shutdown drains and joins the entire chain.
//! - Each fragment is one `pwritev`: `prft` + `moof` + `mdat` header, the
//!   samples, the CRC trailer; then one index record.
//! - Every dirty file and its index are synced every sync interval, and the
//!   observer learns the durable length only after the sync.
//! - A newly opened pair and its initialization are durable before the open
//!   notification; failed initialization removes only paths claimed by it.
//! - A sealed file is truncated to its written length, so its length is its
//!   valid length and the scanner finds every fragment.
//!
//! # Example
//!
//! ```no_run
//! use std::path::PathBuf;
//!
//! use iris_store::{StoragePolicy, WriterHandle};
//!
//! let policy = StoragePolicy::from(&iris_core::config::StorageConfig::default());
//! let writer = WriterHandle::spawn(PathBuf::from("/srv/irisd/video"), policy, 64)?;
//! // ... writer.try_send(key, fragment) from ingest ...
//! writer.shutdown()?;
//! # Ok::<(), std::io::Error>(())
//! ```

mod fs;
pub mod layout;
mod observer;
mod policy;
pub mod recover;
mod segment;
mod stream;
mod writer;

pub use fs::{
    ApprovedRoot, FreeSpace, ROOT_IDENTITY_MARKER, RootExpectation, RootIdentity,
    admit_recording_root, create_recording_root, free_space, free_space_approved,
    open_approved_root_directory, open_segment_beneath, open_segment_pair_beneath, remove_segment,
    remove_segment_beneath, verify_approved_root,
};
pub use iris_mp4::fragment::Fragment;
pub use observer::{NoopObserver, SegmentOpened, SegmentProgress, SegmentSealed, StoreObserver};
pub use policy::StoragePolicy;
pub use writer::{QueueFull, WriterHandle, WriterStats};
