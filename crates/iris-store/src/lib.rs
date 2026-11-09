//! Recording files for irisd: segment files, preallocation, sync policy, the
//! writer thread, durable root identity and admission, and recovery.
//!
//! Ingest hands completed [`Fragment`]s to a [`WriterHandle`]; a dedicated
//! thread per video root appends them to per-stream recording files laid out
//! as `<root>/<camera>/<kind>/<YYYY-MM-DD>/<HHMMSS.ffffff>Z.mp4` with a sidecar
//! `.idx`, preallocates space ahead of the write position, `fdatasync`s on a
//! timer, rotates files by size or age, and seals them on shutdown. A file
//! that was open when the process died is repaired by
//! [`recover::recover_segment`], which the daemon runs at startup.
//!
//! # Contents
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
//! - [`ApprovedRoot`], [`RootIdentity`], [`RootExpectation`],
//!   [`PhysicalIdentity`], [`ROOT_IDENTITY_MARKER`], [`admit_recording_root`],
//!   [`verify_approved_root`], and [`open_approved_root_directory`]: the
//!   durable identity marker of a root and the daemon's admission against the
//!   catalog's expectation; [`InvalidRootIdentity`] is a malformed identity.
//! - [`create_recording_root`], [`free_space`], [`FreeSpace`],
//!   [`free_space_approved`], and [`remove_segment_beneath`]: durable root
//!   preparation and the file-system operations retention needs.
//! - [`open_segment_beneath`], [`open_segment_pair_beneath`], and
//!   [`open_regular_file`]: opens that never follow a final symlink and never
//!   block on a FIFO or device, accepting only regular files.
//!
//! # Invariants
//!
//! - The writer thread is the only writer under a root; ingest tasks never
//!   block on it, and a full queue drops the fragment.
//! - [`WriterHandle::spawn_with_roots`] tries roots in order through bounded
//!   queues. An incomplete media write is retried on the fallback root, while
//!   a complete media write followed by a sidecar or preallocation error is
//!   acknowledged and left for recovery; each file starts at a sync sample,
//!   forwarding loss discards dependent fragments until a new run can start,
//!   and shutdown drains and joins the entire chain.
//! - Each fragment is one vectored write of up to 1,022 samples: `prft` +
//!   `moof` + `mdat` header, the samples, the CRC trailer (a `pwritev` takes
//!   at most 1,024 slices, so a longer fragment continues in further calls);
//!   then one index record.
//! - Every dirty file and its index are synced every sync interval, and the
//!   observer learns the durable length only after the sync.
//! - A newly opened pair and its initialization are durable before the open
//!   notification; failed initialization removes only paths claimed by it.
//! - A sealed file is truncated to its written length, so its length is its
//!   valid length and the scanner finds every fragment.
//!
//! # Example
//!
//! ```
//! use std::sync::Arc;
//!
//! use iris_core::id::{CameraId, StreamKey, StreamKind};
//! use iris_store::{Fragment, StoragePolicy, WriterHandle};
//! # use bytes::Bytes;
//! # use iris_mp4::fragment::Sample;
//! # use iris_mp4::init::SampleEntry;
//!
//! let root = tempfile::tempdir()?;
//! let policy = StoragePolicy::from(&iris_core::config::StorageConfig::default());
//! let writer = WriterHandle::spawn(root.path().to_path_buf(), policy, 64)?;
//! let key = StreamKey {
//!     camera: CameraId::try_new("cam1")?,
//!     kind: StreamKind::Main,
//! };
//! # let entry = Arc::new(SampleEntry::new(
//! #     Bytes::from_static(b"\0\0\0\x10hvc1\0\0\0\0\0\0\0\0"),
//! #     "hvc1".into(),
//! #     16,
//! #     16,
//! # ));
//! # let fragment = Fragment {
//! #     samples: vec![Sample {
//! #         data: Bytes::from_static(&[0, 0, 0, 2, 0x26, 1]),
//! #         duration: 3000,
//! #         is_sync: true,
//! #     }],
//! #     decode_time: 0,
//! #     wall: iris_core::time::WallUs::now(),
//! #     raw_rtp: 0,
//! #     new_run: true,
//! #     sample_entry: entry,
//! # };
//! // Ingest calls this on its own task; a full queue returns QueueFull instead of waiting.
//! writer.try_send(key, fragment)?;
//! writer.shutdown()?;
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```

mod fs;
pub mod layout;
mod observer;
mod policy;
pub mod recover;
mod root_worker;
mod segment;
mod stream;
mod writer;

pub use fs::{
    ApprovedRoot, FreeSpace, InvalidRootIdentity, PhysicalIdentity, ROOT_IDENTITY_MARKER,
    RootExpectation, RootIdentity, admit_recording_root, create_recording_root, free_space,
    free_space_approved, open_approved_root_directory, open_regular_file, open_segment_beneath,
    open_segment_pair_beneath, remove_segment_beneath, verify_approved_root,
};
pub use iris_mp4::fragment::Fragment;
pub use observer::{NoopObserver, SegmentOpened, SegmentProgress, SegmentSealed, StoreObserver};
pub use policy::StoragePolicy;
pub use writer::{QueueFull, WriterHandle, WriterStats};
