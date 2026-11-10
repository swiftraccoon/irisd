//! Per-stream rotation policy on top of [`SegmentFile`].

use std::fmt;
use std::io;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use iris_core::id::StreamKey;
use iris_mp4::fragment::Fragment;
use iris_mp4::index::{
    FLAG_DISCONTINUITY_BEFORE, FLAG_NEW_PARAMETERS, FLAG_STARTS_WITH_SYNC, IndexHeader,
    IndexRecord, MAX_PERSISTED_INDEX_RECORDS,
};
use iris_mp4::init::{SampleEntry, build_init_segment};
use tracing::{info, warn};

use crate::fs::ApprovedRoot;
use crate::layout;
use crate::observer::{SegmentOpened, StoreObserver};
use crate::policy::StoragePolicy;
use crate::segment::SegmentFile;

/// Fragments one recording may hold: the record ceiling of every sidecar
/// reader. A file at this count takes no further fragment.
const FRAGMENT_LIMIT: usize = MAX_PERSISTED_INDEX_RECORDS;
/// Fragments after which a recording rotates at its next sync fragment, like
/// one that reached its size or age limit. The margin lets a group of
/// pictures in progress end before [`FRAGMENT_LIMIT`].
const FRAGMENT_SOFT_LIMIT: usize = MAX_PERSISTED_INDEX_RECORDS - 4_096;

/// What happened to a fragment handed to [`StreamWriter::write`].
#[derive(Debug)]
pub(crate) enum Written {
    /// Appended; the record describes it.
    Record(IndexRecord),
    /// Dropped: the fragment needs a fresh file (none open, a new run, new
    /// parameter sets, or a full file) and does not start with a sync sample,
    /// so no file can take it.
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
    /// The sample entry of the open file, compared by content: its CRC-32C
    /// is only a hint, so two entries with one hash still rotate.
    sample_entry: Option<Arc<SampleEntry>>,
    pending_new_run: bool,
    /// Whether the open file reached [`FRAGMENT_LIMIT`] and the warning for
    /// the dependent fragments dropped since has been logged.
    dropping_at_limit: bool,
    fragment_limit: usize,
    fragment_soft_limit: usize,
    files_opened: u64,
    observer: Arc<dyn StoreObserver>,
    approved_root: Option<ApprovedRoot>,
}

impl fmt::Debug for StreamWriter {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
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
            sample_entry: None,
            pending_new_run: false,
            dropping_at_limit: false,
            fragment_limit: FRAGMENT_LIMIT,
            fragment_soft_limit: FRAGMENT_SOFT_LIMIT,
            files_opened: 0,
            observer,
            approved_root,
        }
    }

    /// Appends `fragment`, opening or rotating the file first when needed. A
    /// fragment that needs a fresh file but does not start with a sync sample
    /// is returned as [`Written::Dropped`] rather than opening a file mid-GOP.
    pub(crate) fn write(&mut self, fragment: &Fragment) -> io::Result<Written> {
        let starts_sync = fragment.starts_with_sync();
        // The first file of a stream starts run 1: nothing continuous precedes it.
        let new_run = fragment.new_run || self.pending_new_run || self.run_id == 0;
        let parameters_changed = self.sample_entry.as_ref().is_none_or(|current| {
            !Arc::ptr_eq(current, &fragment.sample_entry) && **current != *fragment.sample_entry
        });
        let full = self
            .current
            .as_ref()
            .is_some_and(|segment| fragment_count(segment) >= self.fragment_limit);
        // A file can only start at a sync sample. Without one, a fragment that
        // needs a fresh file (no file open, a new run, new parameter sets, or
        // an open file holding as many fragments as a sidecar may describe)
        // cannot be stored; a fragment that merely reached the size, age, or
        // soft fragment limit goes into the open file and the rotation waits
        // for the next sync sample.
        let must_start_file = match &self.current {
            None => true,
            Some(_) => new_run || parameters_changed || full,
        };
        if must_start_file && !starts_sync {
            if full {
                // The dropped dependents leave a recording gap, so the next
                // file starts a new run.
                self.pending_new_run = true;
                if !self.dropping_at_limit {
                    self.dropping_at_limit = true;
                    warn!(
                        stream = %self.key,
                        fragments = self.fragment_limit,
                        "open file reached the sidecar record limit; dropping until a sync sample"
                    );
                }
            } else {
                // The dropped fragment leaves a recording gap, so the next file
                // starts a new run.
                self.pending_new_run = true;
                let reason = if self.current.is_none() {
                    "no open file"
                } else if new_run {
                    "new run"
                } else {
                    "new parameter sets"
                };
                warn!(
                    stream = %self.key,
                    reason,
                    "dropping fragment: it does not start with a sync sample"
                );
            }
            return Ok(Written::Dropped);
        }
        let limit_reached = self.current.as_ref().is_some_and(|segment| {
            segment.bytes_written() >= self.policy.rotate_bytes
                || segment.opened_at().elapsed() >= self.policy.rotate_age
                || fragment_count(segment) >= self.fragment_soft_limit
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
        if parameters_changed {
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
        let segment = self
            .current
            .as_mut()
            .expect("a file is open: rotate opened one or must_start_file found one");
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
            info!(
                stream = %self.key,
                path = %segment.path().display(),
                bytes = segment.bytes_written(),
                "sealing"
            );
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
        self.sample_entry = Some(Arc::clone(&fragment.sample_entry));
        self.dropping_at_limit = false;
        self.files_opened += 1;
        Ok(())
    }

    /// Syncs the open file if its interval elapsed, reporting progress. Returns
    /// `Ok(true)` only when it synced.
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
            warn!(
                stream = %self.key,
                path = %segment.path().display(),
                "abandoning file after error"
            );
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
}

fn fragment_count(segment: &SegmentFile) -> usize {
    usize::try_from(segment.fragments()).unwrap_or(usize::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use iris_core::id::{CameraId, StreamKind};
    use iris_core::time::WallUs;
    use iris_mp4::fragment::Sample;
    use iris_mp4::index::{MAX_PERSISTED_INDEX_RECORDS, read_index_bounded};
    use iris_mp4::init::SampleEntry;

    use crate::observer::NoopObserver;

    /// Whether a test fragment starts a run.
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Run {
        New,
        Continues,
    }

    /// Whether a test fragment starts with a sync sample.
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Start {
        Sync,
        Continuation,
    }

    fn sync_deadline(writer: &StreamWriter) -> Instant {
        writer
            .current
            .as_ref()
            .map_or(writer.next_sync, SegmentFile::next_sync)
    }

    fn with_fragment_limits(mut writer: StreamWriter, soft: usize, hard: usize) -> StreamWriter {
        writer.fragment_soft_limit = soft;
        writer.fragment_limit = hard;
        writer
    }

    fn key() -> StreamKey {
        StreamKey {
            camera: CameraId::try_new("cam1").unwrap(),
            kind: StreamKind::Main,
        }
    }

    fn fragment(index: u64, run: Run) -> Fragment {
        Fragment {
            samples: vec![Sample {
                data: Bytes::from_static(&[0, 0, 0, 2, 0x26, 1]),
                duration: 3_000,
                is_sync: true,
            }],
            decode_time: index * 3_000,
            wall: WallUs::new(1_700_000_000_000_000 + i64::try_from(index).unwrap()),
            raw_rtp: 0,
            new_run: run == Run::New,
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
                rotate_age: Duration::MAX,
            },
            Arc::new(NoopObserver),
            phase,
            None,
        )
    }

    fn sample_entry(bytes: Vec<u8>) -> Arc<SampleEntry> {
        Arc::new(SampleEntry::new(Bytes::from(bytes), "hvc1".into(), 16, 16))
    }

    fn fragment_with(index: u64, start: Start, entry: &Arc<SampleEntry>) -> Fragment {
        Fragment {
            samples: vec![Sample {
                data: Bytes::from_static(&[0, 0, 0, 2, 0x26, 1]),
                duration: 3_000,
                is_sync: start == Start::Sync,
            }],
            decode_time: index * 3_000,
            wall: WallUs::new(1_700_000_000_000_000 + i64::try_from(index).unwrap()),
            raw_rtp: 0,
            new_run: false,
            sample_entry: Arc::clone(entry),
        }
    }

    fn record(written: &Written) -> IndexRecord {
        match written {
            Written::Record(record) => *record,
            Written::Dropped => panic!("fragment was dropped"),
        }
    }

    fn sidecars(root: &std::path::Path) -> Vec<(IndexHeader, Vec<IndexRecord>)> {
        fn walk(directory: &std::path::Path, found: &mut Vec<PathBuf>) {
            for entry in std::fs::read_dir(directory).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    walk(&path, found);
                } else if path.extension().is_some_and(|extension| extension == "idx") {
                    found.push(path);
                }
            }
        }
        let mut found = Vec::new();
        walk(root, &mut found);
        found.sort();
        found
            .iter()
            .map(|path| read_index_bounded(path, MAX_PERSISTED_INDEX_RECORDS).unwrap())
            .collect()
    }

    /// The CRC-32C (reflected) byte table.
    fn crc32c_table() -> [u32; 256] {
        let mut table = [0u32; 256];
        for (index, slot) in table.iter_mut().enumerate() {
            let mut crc = u32::try_from(index).unwrap();
            for _ in 0..8 {
                crc = if crc & 1 == 0 {
                    crc >> 1
                } else {
                    (crc >> 1) ^ 0x82f6_3b78
                };
            }
            *slot = crc;
        }
        table
    }

    /// Appends four bytes to `prefix` so that the CRC-32C of the result is
    /// `target`, by running the checksum backwards from the target.
    fn forge_crc32c(prefix: &[u8], target: u32) -> Vec<u8> {
        let table = crc32c_table();
        let mut reverse = [0u32; 256];
        for (index, value) in table.iter().enumerate() {
            reverse[usize::try_from(value >> 24).unwrap()] =
                (value << 8) ^ u32::try_from(index).unwrap();
        }
        let mut register = 0xffff_ffff_u32;
        for &byte in prefix {
            register = (register >> 8)
                ^ table[usize::try_from((register ^ u32::from(byte)) & 0xff).unwrap()];
        }
        let mut back = target ^ 0xffff_ffff;
        for _ in 0..4 {
            back = (back << 8) ^ reverse[usize::try_from(back >> 24).unwrap()];
        }
        let mut forged = prefix.to_vec();
        forged.extend_from_slice(&(back ^ register).to_le_bytes());
        forged
    }

    #[test]
    fn a_parameter_change_dropped_mid_gop_starts_a_new_run_at_the_next_sync() {
        let dir = tempfile::tempdir().unwrap();
        let mut writer = writer(dir.path().to_path_buf(), Duration::ZERO);
        let first = sample_entry(vec![1; 16]);
        let second = sample_entry(vec![2; 16]);
        record(
            &writer
                .write(&fragment_with(0, Start::Sync, &first))
                .unwrap(),
        );
        assert!(matches!(
            writer
                .write(&fragment_with(1, Start::Continuation, &second))
                .unwrap(),
            Written::Dropped
        ));
        let resumed = record(
            &writer
                .write(&fragment_with(2, Start::Sync, &second))
                .unwrap(),
        );
        assert_ne!(
            resumed.flags & FLAG_DISCONTINUITY_BEFORE,
            0,
            "the dropped fragment left a gap, so the next file starts a run"
        );
        assert_eq!(writer.run_id, 2);
        writer.close().unwrap();
    }

    #[test]
    fn a_full_file_drops_dependents_and_starts_a_new_run_at_the_next_sync() {
        let dir = tempfile::tempdir().unwrap();
        let mut writer =
            with_fragment_limits(writer(dir.path().to_path_buf(), Duration::ZERO), 10, 3);
        let entry = fragment(0, Run::Continues).sample_entry;
        record(
            &writer
                .write(&fragment_with(0, Start::Sync, &entry))
                .unwrap(),
        );
        for index in 1..3 {
            record(
                &writer
                    .write(&fragment_with(index, Start::Continuation, &entry))
                    .unwrap(),
            );
        }
        for index in 3..5 {
            assert!(matches!(
                writer
                    .write(&fragment_with(index, Start::Continuation, &entry))
                    .unwrap(),
                Written::Dropped
            ));
        }
        let first_of_next = record(
            &writer
                .write(&fragment_with(5, Start::Sync, &entry))
                .unwrap(),
        );
        assert_ne!(first_of_next.flags & FLAG_DISCONTINUITY_BEFORE, 0);
        assert_eq!(writer.files_opened(), 2);
        writer.close().unwrap();

        let files = sidecars(dir.path());
        assert_eq!(
            files
                .iter()
                .map(|(header, records)| (header.run_id, records.len()))
                .collect::<Vec<_>>(),
            [(1, 3), (2, 1)]
        );
    }

    #[test]
    fn the_soft_fragment_limit_rotates_at_the_next_sync_fragment() {
        let dir = tempfile::tempdir().unwrap();
        let mut writer =
            with_fragment_limits(writer(dir.path().to_path_buf(), Duration::ZERO), 2, 10);
        let entry = fragment(0, Run::Continues).sample_entry;
        record(
            &writer
                .write(&fragment_with(0, Start::Sync, &entry))
                .unwrap(),
        );
        record(
            &writer
                .write(&fragment_with(1, Start::Continuation, &entry))
                .unwrap(),
        );
        record(
            &writer
                .write(&fragment_with(2, Start::Continuation, &entry))
                .unwrap(),
        );
        assert_eq!(
            writer.files_opened(),
            1,
            "rotation waits for a sync fragment"
        );
        let rotated = record(
            &writer
                .write(&fragment_with(3, Start::Sync, &entry))
                .unwrap(),
        );
        assert_eq!(rotated.flags & FLAG_DISCONTINUITY_BEFORE, 0);
        assert_eq!(writer.files_opened(), 2);
        writer.close().unwrap();
        let files = sidecars(dir.path());
        assert_eq!(
            files
                .iter()
                .map(|(header, records)| (header.run_id, records.len()))
                .collect::<Vec<_>>(),
            [(1, 3), (1, 1)]
        );
    }

    #[test]
    fn a_sample_entry_with_the_same_crc_but_other_bytes_rotates() {
        let mut first = b"\0\0\0\x10hvc1\0\0\0\0\0\0\0\0".to_vec();
        first.extend_from_slice(b"parameter-set-epoch-A");
        let mut second = b"\0\0\0\x10hvc1\0\0\0\0\0\0\0\0".to_vec();
        second.extend_from_slice(b"parameter-set-epoch-B");
        let second = forge_crc32c(&second, iris_mp4::crc::crc32c(&first));
        let (first, second) = (sample_entry(first), sample_entry(second));
        assert_eq!(first.hash, second.hash, "the forged entries collide");
        assert_ne!(first.bytes, second.bytes);

        let dir = tempfile::tempdir().unwrap();
        let mut writer = writer(dir.path().to_path_buf(), Duration::ZERO);
        record(
            &writer
                .write(&fragment_with(0, Start::Sync, &first))
                .unwrap(),
        );
        record(
            &writer
                .write(&fragment_with(1, Start::Continuation, &first))
                .unwrap(),
        );
        let changed = record(
            &writer
                .write(&fragment_with(2, Start::Sync, &second))
                .unwrap(),
        );
        assert_ne!(changed.flags & FLAG_NEW_PARAMETERS, 0);
        assert_eq!(writer.files_opened(), 2);
        writer.close().unwrap();
    }

    #[test]
    fn an_equal_sample_entry_in_another_allocation_keeps_the_file() {
        let bytes = b"\0\0\0\x10hvc1\0\0\0\0\0\0\0\0".to_vec();
        let (first, copy) = (sample_entry(bytes.clone()), sample_entry(bytes));
        assert!(!Arc::ptr_eq(&first, &copy));
        let dir = tempfile::tempdir().unwrap();
        let mut writer = writer(dir.path().to_path_buf(), Duration::ZERO);
        record(
            &writer
                .write(&fragment_with(0, Start::Sync, &first))
                .unwrap(),
        );
        let same = record(&writer.write(&fragment_with(1, Start::Sync, &copy)).unwrap());
        assert_eq!(same.flags & FLAG_NEW_PARAMETERS, 0);
        assert_eq!(writer.files_opened(), 1);
        writer.close().unwrap();
    }

    #[test]
    fn rotation_inherits_the_absolute_sync_deadline() {
        for phase in [Duration::ZERO, Duration::from_millis(1_250)] {
            let dir = tempfile::tempdir().unwrap();
            let mut writer = writer(dir.path().to_path_buf(), phase);
            let initial = sync_deadline(&writer);
            writer.write(&fragment(0, Run::Continues)).unwrap();
            assert_eq!(sync_deadline(&writer), initial);

            let interval = Duration::from_secs(5);
            assert!(writer.maybe_sync(initial, interval).unwrap());
            let advanced = initial + interval;
            assert_eq!(sync_deadline(&writer), advanced);

            writer.write(&fragment(1, Run::New)).unwrap();
            assert_eq!(sync_deadline(&writer), advanced);
            assert!(writer.maybe_sync(advanced, interval).unwrap());
            assert_eq!(sync_deadline(&writer), advanced + interval);
            assert!(!writer.maybe_sync(advanced + interval, interval).unwrap());
            assert_eq!(sync_deadline(&writer), advanced + 2 * interval);

            let late = advanced + 10 * interval;
            assert!(!writer.maybe_sync(late, interval).unwrap());
            assert_eq!(sync_deadline(&writer), late);
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
                rotate_age: Duration::MAX,
            },
            Arc::new(NoopObserver),
            Duration::ZERO,
            Some(approved),
        );
        let fragment = fragment(0, Run::Continues);
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
                rotate_age: Duration::MAX,
            },
            Arc::new(NoopObserver),
            Duration::ZERO,
            Some(approved),
        );
        let fragment = fragment(0, Run::Continues);
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
                rotate_age: Duration::MAX,
            },
            Arc::new(NoopObserver),
            Duration::ZERO,
            Some(approved),
        );
        let fragment = fragment(0, Run::Continues);
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
