//! Recording files for irisd: segment files, preallocation, sync policy, the
//! writer thread, and recovery.
//!
//! Ingest hands completed [`Fragment`]s to a [`WriterHandle`]; a dedicated
//! thread per video root appends them to per-stream recording files laid out
//! as `<root>/<camera>/<kind>/<YYYY-MM-DD>/<HHMMSS>Z.mp4` with a sidecar
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
//! - [`free_space`] and [`remove_segment`]: what retention needs.
//!
//! # Invariants
//!
//! - The writer thread is the only writer under a root; ingest tasks never
//!   block on it, and a full queue drops the fragment.
//! - Each fragment is one `pwritev`: `prft` + `moof` + `mdat` header, the
//!   samples, the CRC trailer; then one index record.
//! - Every dirty file and its index are synced every sync interval, and the
//!   observer learns the durable length only after the sync.
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

pub use fs::{FreeSpace, free_space, remove_segment};
pub use iris_mp4::fragment::Fragment;
pub use observer::{NoopObserver, SegmentOpened, SegmentProgress, SegmentSealed, StoreObserver};
pub use policy::StoragePolicy;
pub use writer::{QueueFull, WriterHandle, WriterStats};
