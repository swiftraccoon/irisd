//! Per-stream rotation policy on top of [`SegmentFile`].

use std::io;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use iris_core::id::StreamKey;
use iris_mp4::fragment::Fragment;
use iris_mp4::index::{
    FLAG_DISCONTINUITY_BEFORE, FLAG_NEW_PARAMETERS, FLAG_STARTS_WITH_SYNC, IndexHeader, IndexRecord,
};
use iris_mp4::init::build_init_segment;
use tracing::{info, warn};

use crate::layout;
use crate::observer::{SegmentOpened, StoreObserver};
use crate::policy::StoragePolicy;
use crate::segment::SegmentFile;

/// What happened to a fragment handed to [`StreamWriter::write`].
#[derive(Debug)]
pub(crate) enum Written {
    /// Appended; the record describes it.
    Record(IndexRecord),
    /// Dropped because no file was open and the fragment did not start with a sync sample.
    Dropped,
}

/// Owns the open file of one stream and decides when to rotate.
pub(crate) struct StreamWriter {
    key: StreamKey,
    root: PathBuf,
    policy: StoragePolicy,
    /// Offset of this stream's syncs within the interval.
    sync_phase: Duration,
    current: Option<SegmentFile>,
    run_id: u64,
    sequence: u32,
    sample_entry_hash: Option<u32>,
    pending_new_run: bool,
    files_opened: u64,
    observer: Arc<dyn StoreObserver>,
}

impl std::fmt::Debug for StreamWriter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StreamWriter")
            .field("key", &self.key)
            .field("run_id", &self.run_id)
            .field("sequence", &self.sequence)
            .field("files_opened", &self.files_opened)
            .finish_non_exhaustive()
    }
}

impl StreamWriter {
    pub(crate) fn new(
        key: StreamKey,
        root: PathBuf,
        policy: StoragePolicy,
        observer: Arc<dyn StoreObserver>,
        sync_phase: Duration,
    ) -> Self {
        Self {
            key,
            root,
            policy,
            sync_phase,
            current: None,
            run_id: 0,
            sequence: 0,
            sample_entry_hash: None,
            pending_new_run: false,
            files_opened: 0,
            observer,
        }
    }

    /// Appends `fragment`, opening or rotating the file first when needed.
    pub(crate) fn write(&mut self, fragment: &Fragment) -> io::Result<Written> {
        let starts_sync = fragment.starts_with_sync();
        // The first file of a stream starts run 1: nothing continuous precedes it.
        let new_run = fragment.new_run || self.pending_new_run || self.run_id == 0;
        let hash_changed = self.sample_entry_hash != Some(fragment.sample_entry.hash);
        // A file can only start at a sync sample. Without one, a fragment that
        // needs a fresh file (no file open, a new run, or new parameter sets)
        // cannot be stored; a fragment that merely reached the size or age
        // limit goes into the open file and the rotation waits for the next
        // sync sample.
        let must_start_file = match &self.current {
            None => true,
            Some(_) => new_run || hash_changed,
        };
        if must_start_file && !starts_sync {
            self.pending_new_run = new_run;
            warn!(stream = %self.key, "dropping fragment: no open file and not a sync sample");
            return Ok(Written::Dropped);
        }
        let limit_reached = self.current.as_ref().is_some_and(|segment| {
            segment.bytes_written() >= self.policy.rotate_bytes
                || segment.opened_at().elapsed() >= Duration::from_secs(self.policy.rotate_secs)
        });
        let rotate = must_start_file || (limit_reached && starts_sync);

        let mut flags = 0u8;
        if starts_sync {
            flags |= FLAG_STARTS_WITH_SYNC;
        }
        if new_run {
            flags |= FLAG_DISCONTINUITY_BEFORE;
            self.run_id += 1;
            self.pending_new_run = false;
        }
        if hash_changed {
            flags |= FLAG_NEW_PARAMETERS;
        }

        if rotate {
            self.rotate(fragment)?;
            let segment = self.current.as_ref().expect("rotate opens a file");
            self.observer.segment_opened(SegmentOpened {
                key: self.key.clone(),
                path: segment.path().to_path_buf(),
                run_seq: self.run_id,
                discontinuity: new_run,
                wall_start: fragment.wall,
                media_start: fragment.decode_time,
                init_len: segment.init_len(),
                sample_entry: Arc::clone(&fragment.sample_entry),
            });
        }
        self.sequence += 1;
        let segment = self.current.as_mut().expect("rotate opens a file");
        let record = segment.write_fragment(fragment, self.sequence, flags)?;
        Ok(Written::Record(record))
    }

    fn rotate(&mut self, fragment: &Fragment) -> io::Result<()> {
        if let Some(segment) = self.current.take() {
            info!(stream = %self.key, path = %segment.path().display(), bytes = segment.bytes_written(), "sealing");
            self.observer.segment_sealed(segment.seal()?);
        }
        let init = build_init_segment(&fragment.sample_entry);
        let header = IndexHeader {
            start_wall: fragment.wall,
            run_id: self.run_id,
            init_len: u32::try_from(init.len()).map_err(io::Error::other)?,
            sample_entry_hash: fragment.sample_entry.hash,
            ..IndexHeader::default()
        };
        let path = layout::segment_path(&self.root, &self.key, fragment.wall);
        let segment = SegmentFile::create(
            &path,
            &init,
            header,
            self.policy.preallocate_bytes,
            self.sync_phase,
        )?;
        info!(stream = %self.key, path = %segment.path().display(), run = self.run_id, "opened");
        self.current = Some(segment);
        self.sequence = 0;
        self.sample_entry_hash = Some(fragment.sample_entry.hash);
        self.files_opened += 1;
        Ok(())
    }

    /// Syncs the open file if its interval elapsed, reporting progress.
    pub(crate) fn maybe_sync(&mut self, now: Instant, interval: Duration) -> io::Result<bool> {
        let Some(segment) = self.current.as_mut() else {
            return Ok(false);
        };
        if !segment.maybe_sync(now, interval)? {
            return Ok(false);
        }
        self.observer.segment_progress(segment.progress());
        Ok(true)
    }

    /// Seals the open file.
    pub(crate) fn close(&mut self) -> io::Result<()> {
        if let Some(segment) = self.current.take() {
            self.observer.segment_sealed(segment.seal()?);
        }
        Ok(())
    }

    /// Forgets the open file after an I/O error; recovery repairs it later.
    pub(crate) fn abandon(&mut self) {
        if let Some(segment) = self.current.take() {
            warn!(stream = %self.key, path = %segment.path().display(), "abandoning file after error");
        }
    }

    /// Number of files this stream has opened.
    pub(crate) fn files_opened(&self) -> u64 {
        self.files_opened
    }
}
