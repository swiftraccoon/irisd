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

use crate::fs::ApprovedRoot;
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
    /// Absolute deadline retained across file rotations.
    next_sync: Instant,
    current: Option<SegmentFile>,
    run_id: u64,
    sequence: u32,
    sample_entry_hash: Option<u32>,
    pending_new_run: bool,
    files_opened: u64,
    observer: Arc<dyn StoreObserver>,
    approved_root: Option<ApprovedRoot>,
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
        approved_root: Option<ApprovedRoot>,
    ) -> Self {
        Self {
            key,
            root,
            policy,
            next_sync: Instant::now() + sync_phase,
            current: None,
            run_id: 0,
            sequence: 0,
            sample_entry_hash: None,
            pending_new_run: false,
            files_opened: 0,
            observer,
            approved_root,
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
        self.rotate_with(fragment, || Ok(()))
    }

    fn rotate_with(
        &mut self,
        fragment: &Fragment,
        after_root_verification: impl FnOnce() -> io::Result<()>,
    ) -> io::Result<()> {
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
        let root_directory = self
            .approved_root
            .as_ref()
            .map(crate::fs::open_approved_root_directory)
            .transpose()?;
        after_root_verification()?;
        let path = layout::segment_path(&self.root, &self.key, fragment.wall);
        let segment = match (&self.approved_root, root_directory) {
            (Some(root), Some(directory)) => SegmentFile::create_beneath(
                root,
                directory,
                &path,
                &init,
                header,
                self.policy.preallocate_bytes,
                self.next_sync,
            ),
            (None, None) => SegmentFile::create(
                &path,
                &init,
                header,
                self.policy.preallocate_bytes,
                self.next_sync,
            ),
            _ => unreachable!("an approved root and its descriptor are paired"),
        }?;
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
        let synced = segment.maybe_sync(now, interval)?;
        self.next_sync = segment.next_sync();
        if !synced {
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
        self.mark_discontinuity();
        if let Some(segment) = self.current.take() {
            warn!(stream = %self.key, path = %segment.path().display(), "abandoning file after error");
        }
    }

    /// Marks a recording gap so reopening after pressure or an error starts a run.
    pub(crate) fn mark_discontinuity(&mut self) {
        self.pending_new_run = true;
    }

    /// Number of files this stream has opened.
    pub(crate) fn files_opened(&self) -> u64 {
        self.files_opened
    }

    #[cfg(test)]
    fn sync_deadline(&self) -> Instant {
        self.current
            .as_ref()
            .map_or(self.next_sync, SegmentFile::next_sync)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use iris_core::id::{CameraId, StreamKind};
    use iris_core::time::WallUs;
    use iris_mp4::fragment::Sample;
    use iris_mp4::init::SampleEntry;

    use crate::observer::NoopObserver;

    fn key() -> StreamKey {
        StreamKey {
            camera: CameraId::try_new("cam1").unwrap(),
            kind: StreamKind::Main,
        }
    }

    fn fragment(index: u64, new_run: bool) -> Fragment {
        Fragment {
            samples: vec![Sample {
                data: Bytes::from_static(&[0, 0, 0, 2, 0x26, 1]),
                duration: 3_000,
                is_sync: true,
            }],
            decode_time: index * 3_000,
            wall: WallUs::new(1_700_000_000_000_000 + i64::try_from(index).unwrap()),
            raw_rtp: 0,
            new_run,
            sample_entry: Arc::new(SampleEntry::new(
                Bytes::from_static(b"\0\0\0\x10hvc1\0\0\0\0\0\0\0\0"),
                "hvc1".into(),
                16,
                16,
            )),
        }
    }

    fn writer(root: PathBuf, phase: Duration) -> StreamWriter {
        StreamWriter::new(
            key(),
            root,
            StoragePolicy {
                sync_interval: Duration::from_secs(5),
                preallocate_bytes: 0,
                rotate_bytes: u64::MAX,
                rotate_secs: u64::MAX,
            },
            Arc::new(NoopObserver),
            phase,
            None,
        )
    }

    #[test]
    fn rotation_inherits_the_absolute_sync_deadline() {
        for phase in [Duration::ZERO, Duration::from_millis(1_250)] {
            let dir = tempfile::tempdir().unwrap();
            let mut writer = writer(dir.path().to_path_buf(), phase);
            let initial = writer.sync_deadline();
            writer.write(&fragment(0, false)).unwrap();
            assert_eq!(writer.sync_deadline(), initial);

            let interval = Duration::from_secs(5);
            assert!(writer.maybe_sync(initial, interval).unwrap());
            let advanced = initial + interval;
            assert_eq!(writer.sync_deadline(), advanced);

            writer.write(&fragment(1, true)).unwrap();
            assert_eq!(writer.sync_deadline(), advanced);
            assert!(writer.maybe_sync(advanced, interval).unwrap());
            assert_eq!(writer.sync_deadline(), advanced + interval);
            assert!(!writer.maybe_sync(advanced + interval, interval).unwrap());
            assert_eq!(writer.sync_deadline(), advanced + 2 * interval);

            let late = advanced + 10 * interval;
            assert!(!writer.maybe_sync(late, interval).unwrap());
            assert_eq!(writer.sync_deadline(), late);
            writer.close().unwrap();
        }
    }

    #[test]
    fn approved_rotation_stays_on_the_verified_root_after_its_path_is_replaced() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("root");
        let displaced = dir.path().join("displaced");
        let approved =
            crate::fs::admit_recording_root(&root, crate::fs::RootExpectation::Fresh).unwrap();
        let mut writer = StreamWriter::new(
            key(),
            root.clone(),
            StoragePolicy {
                sync_interval: Duration::from_secs(5),
                preallocate_bytes: 0,
                rotate_bytes: u64::MAX,
                rotate_secs: u64::MAX,
            },
            Arc::new(NoopObserver),
            Duration::ZERO,
            Some(approved),
        );
        let fragment = fragment(0, false);
        let replacement_path = layout::segment_path(&root, &key(), fragment.wall);
        let original_path = displaced.join(replacement_path.strip_prefix(&root).unwrap());

        writer
            .rotate_with(&fragment, || {
                std::fs::rename(&root, &displaced)?;
                std::fs::create_dir(&root)?;
                std::fs::copy(
                    displaced.join(crate::fs::ROOT_IDENTITY_MARKER),
                    root.join(crate::fs::ROOT_IDENTITY_MARKER),
                )?;
                std::fs::write(root.join("sentinel"), b"replacement bytes")?;
                Ok(())
            })
            .unwrap();
        writer.close().unwrap();

        assert!(
            original_path.exists(),
            "the pinned original root receives the pair"
        );
        assert!(
            !replacement_path.exists(),
            "the replacement root must remain untouched"
        );
        assert_eq!(
            std::fs::read(root.join("sentinel")).unwrap(),
            b"replacement bytes"
        );
    }

    #[cfg(unix)]
    #[test]
    fn approved_rotation_rejects_a_layout_symlink_installed_after_root_verification() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("root");
        let victim = dir.path().join("victim-camera");
        let approved =
            crate::fs::admit_recording_root(&root, crate::fs::RootExpectation::Fresh).unwrap();
        let mut writer = StreamWriter::new(
            key(),
            root.clone(),
            StoragePolicy {
                sync_interval: Duration::from_secs(5),
                preallocate_bytes: 0,
                rotate_bytes: u64::MAX,
                rotate_secs: u64::MAX,
            },
            Arc::new(NoopObserver),
            Duration::ZERO,
            Some(approved),
        );
        let fragment = fragment(0, false);
        let requested = layout::segment_path(&root, &key(), fragment.wall);
        std::fs::create_dir(&victim).unwrap();
        std::fs::write(victim.join("sentinel"), b"victim bytes").unwrap();

        let result = writer.rotate_with(&fragment, || {
            symlink(&victim, root.join("cam1"))?;
            Ok(())
        });
        assert!(result.is_err());
        assert_eq!(
            std::fs::read(victim.join("sentinel")).unwrap(),
            b"victim bytes"
        );
        assert!(
            !victim
                .join(requested.strip_prefix(root.join("cam1")).unwrap())
                .exists(),
            "no pair may be created through the injected layout symlink"
        );
    }

    #[test]
    fn an_approved_writer_creates_and_seals_a_normal_pair() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("root");
        let approved =
            crate::fs::admit_recording_root(&root, crate::fs::RootExpectation::Fresh).unwrap();
        let mut writer = StreamWriter::new(
            key(),
            root.clone(),
            StoragePolicy {
                sync_interval: Duration::from_secs(5),
                preallocate_bytes: 0,
                rotate_bytes: u64::MAX,
                rotate_secs: u64::MAX,
            },
            Arc::new(NoopObserver),
            Duration::ZERO,
            Some(approved),
        );
        let fragment = fragment(0, false);
        let path = layout::segment_path(&root, &key(), fragment.wall);

        assert!(matches!(
            writer.write(&fragment).unwrap(),
            Written::Record(_)
        ));
        writer.close().unwrap();
        assert!(path.is_file());
        assert!(layout::index_path(&path).is_file());
    }
}
