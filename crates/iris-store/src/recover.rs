//! Repairing a file that was open when the process died.

use std::collections::HashMap;
use std::fs::OpenOptions;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use iris_core::time::WallUs;
use iris_mp4::index::{
    FLAG_DISCONTINUITY_BEFORE, FLAG_NEW_PARAMETERS, IndexHeader, IndexRecord, IndexWriter,
    MAX_PERSISTED_INDEX_RECORDS, read_index_bounded,
};
#[cfg(test)]
use iris_mp4::index::{HEADER_LEN, RECORD_LEN};
use iris_mp4::reader::{ScanOptions, ScanReport, ScanStop, scan};
use thiserror::Error;

use crate::layout;

static RECOVERY_TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// What recovery did to one file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecoveryReport {
    /// The media file.
    pub path: PathBuf,
    /// Valid fragments found.
    pub fragments: usize,
    /// File length before recovery.
    pub original_len: u64,
    /// Length of the valid prefix, which is the file length afterwards.
    pub valid_len: u64,
    /// Initialization segment length.
    pub init_len: u32,
    /// Wall time of the first recovered fragment, or zero without `prft`.
    pub wall_start: WallUs,
    /// Wall time at the end of the last recovered fragment, or zero without `prft`.
    pub wall_end: WallUs,
    /// Decode time of the first recovered fragment.
    pub media_start: u64,
    /// Decode-time end of the last recovered fragment.
    pub media_end: u64,
    /// Whether the file was shortened.
    pub truncated: bool,
    /// Why the scan stopped before the original end, if it did.
    pub stop: Option<ScanStop>,
    /// Whether the sidecar index was rewritten (always, on success).
    pub index_rewritten: bool,
}

/// Errors from recovery.
#[derive(Debug, Error)]
pub enum RecoverError {
    /// An I/O failure on the named path.
    #[error("{path}: {source}")]
    Io {
        /// The file involved.
        path: PathBuf,
        /// The underlying error.
        #[source]
        source: io::Error,
    },
    /// The file does not start with `ftyp` + `moov`.
    #[error("{0} is not an irisd recording")]
    NotARecording(PathBuf),
}

/// Scans `mp4_path` with CRC verification, truncates it to its valid prefix,
/// and rewrites the sidecar index. Flags and camera timestamps from an
/// prior index are preserved for fragments that survive.
pub fn recover_segment(mp4_path: &Path) -> Result<RecoveryReport, RecoverError> {
    recover_segment_with_index(mp4_path, &layout::index_path(mp4_path))
}

/// Recovers a recording through an already-approved root. Every layout
/// component is opened no-follow/beneath by [`crate::ApprovedRoot`], and the
/// media plus sidecar remain addressed through pinned descriptors throughout
/// recovery and publication.
pub fn recover_segment_beneath(
    root: &crate::ApprovedRoot,
    mp4_path: &Path,
) -> Result<RecoveryReport, RecoverError> {
    use std::os::fd::AsRawFd as _;

    let (directory, media, media_name) = crate::fs::open_recording_beneath(root, mp4_path)
        .map_err(|source| RecoverError::Io {
            path: mp4_path.to_path_buf(),
            source,
        })?;
    let descriptor_dir = PathBuf::from(format!("/proc/self/fd/{}", directory.as_raw_fd()));
    let descriptor_media = PathBuf::from(format!("/proc/self/fd/{}", media.as_raw_fd()));
    let descriptor_index = descriptor_dir.join(Path::new(&media_name).with_extension("idx"));
    let mut report = recover_segment_with_index(&descriptor_media, &descriptor_index)?;
    report.path = mp4_path.to_path_buf();
    Ok(report)
}

fn recover_segment_with_index(
    mp4_path: &Path,
    index_path: &Path,
) -> Result<RecoveryReport, RecoverError> {
    recover_segment_with_index_and_publication(mp4_path, index_path, |_| Ok(()))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RecoveryPublicationStep {
    BeforeRename,
    BeforeDirectorySync,
}

fn recover_segment_with_index_and_publication(
    mp4_path: &Path,
    index_path: &Path,
    mut publication_step: impl FnMut(RecoveryPublicationStep) -> io::Result<()>,
) -> Result<RecoveryReport, RecoverError> {
    let io_err = |source: io::Error| RecoverError::Io {
        path: mp4_path.to_path_buf(),
        source,
    };
    if index_path == mp4_path {
        return Err(io_err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "the recording path is also its derived .idx sidecar path",
        )));
    }
    if std::fs::symlink_metadata(index_path).is_ok_and(|metadata| metadata.is_dir()) {
        return Err(io_err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "the derived sidecar destination is a directory",
        )));
    }
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(mp4_path)
        .map_err(io_err)?;
    if let Ok(index_metadata) = std::fs::metadata(index_path)
        && same_file(&file.metadata().map_err(io_err)?, &index_metadata)
    {
        return Err(io_err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "the derived sidecar aliases the recording inode",
        )));
    }
    let original_len = file.metadata().map_err(io_err)?.len();
    let existing = read_index_bounded(index_path, MAX_PERSISTED_INDEX_RECORDS).ok();
    let report = scan_for_recovery(&file).map_err(io_err)?;
    if report.init_len == 0 {
        return Err(RecoverError::NotARecording(mp4_path.to_path_buf()));
    }
    let truncated = report.valid_len < original_len;

    let old_records: HashMap<u64, IndexRecord> = existing
        .as_ref()
        .map(|(_, records)| records.iter().map(|r| (r.offset, *r)).collect())
        .unwrap_or_default();
    let header = IndexHeader {
        start_wall: report
            .fragments
            .first()
            .and_then(|fragment| fragment.wall)
            .unwrap_or_default(),
        init_len: report.init_len,
        ..IndexHeader::default()
    };
    let idx_err = |source: io::Error| RecoverError::Io {
        path: index_path.to_path_buf(),
        source,
    };
    let mut rebuilt = Vec::with_capacity(report.fragments.len());
    for info in &report.fragments {
        let mut record = IndexRecord::from_scan(info);
        if let Some(old) = old_records.get(&record.offset) {
            record.flags |= old.flags & (FLAG_DISCONTINUITY_BEFORE | FLAG_NEW_PARAMETERS);
            record.raw_rtp = old.raw_rtp;
        }
        rebuilt.push(record);
    }
    let final_wall = report.fragments.last().and_then(|fragment| fragment.wall);
    let (wall_start, wall_end, media_start, media_end) =
        recovered_times(&rebuilt, final_wall).map_err(io_err)?;
    publish_rebuilt_index(index_path, &header, &rebuilt, &mut publication_step).map_err(idx_err)?;

    // Publication failures above leave media untouched. Publishing the cache
    // first is crash-safe because it describes a verified media prefix; a
    // retry authoritatively scans every fragment before republishing it.
    // An equal-length truncate releases KEEP_SIZE reservations beyond EOF on
    // the supported filesystems, just as normal sealing does.
    file.set_len(report.valid_len).map_err(io_err)?;
    // Complete fragments can survive a process crash only in the page cache.
    // Recovery must make them durable before the catalog can seal the file.
    file.sync_data().map_err(io_err)?;

    Ok(RecoveryReport {
        path: mp4_path.to_path_buf(),
        fragments: report.fragments.len(),
        original_len,
        valid_len: report.valid_len,
        init_len: report.init_len,
        wall_start,
        wall_end,
        media_start,
        media_end,
        truncated,
        stop: report.stop,
        index_rewritten: true,
    })
}

fn publish_rebuilt_index(
    index_path: &Path,
    header: &IndexHeader,
    records: &[IndexRecord],
    mut publication_step: impl FnMut(RecoveryPublicationStep) -> io::Result<()>,
) -> io::Result<()> {
    let (temporary, temporary_file) = create_index_temporary(index_path)?;
    let temporary_claimed = temporary_file.try_clone()?;
    let mut temporary_owned = true;
    let result = (|| {
        let mut writer = IndexWriter::from_claimed_file(temporary_file, header)?;
        for record in records {
            writer.append(record)?;
        }
        writer.sync()?;
        drop(writer);
        publication_step(RecoveryPublicationStep::BeforeRename)?;
        if !claimed_temporary_matches(&temporary, &temporary_claimed) {
            return Err(io::Error::other(
                "recovery sidecar temporary ownership changed",
            ));
        }
        std::fs::rename(&temporary, index_path)?;
        temporary_owned = false;
        publication_step(RecoveryPublicationStep::BeforeDirectorySync)?;
        crate::fs::fsync_dir(crate::fs::parent_dir(index_path))
    })();
    if result.is_err() && temporary_owned {
        remove_claimed_temporary(&temporary, &temporary_claimed);
    }
    result
}

fn recovered_times(
    records: &[IndexRecord],
    final_wall: Option<WallUs>,
) -> io::Result<(WallUs, WallUs, u64, u64)> {
    let (Some(first), Some(last)) = (records.first(), records.last()) else {
        return Ok((WallUs::default(), WallUs::default(), 0, 0));
    };
    if records
        .windows(2)
        .any(|pair| pair[1].decode_time < pair[0].decode_time)
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "recovered media timeline is nonmonotonic",
        ));
    }
    let media_end = last
        .decode_time
        .checked_add(u64::from(last.duration))
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "recovered media timeline overflows u64",
            )
        })?;
    let wall_micros = i64::from(last.duration)
        .checked_mul(100)
        .expect("u32 duration times 100 fits i64")
        .div_euclid(9);
    let wall_end = match final_wall {
        Some(wall) => wall
            .get()
            .checked_add(wall_micros)
            .map(WallUs::new)
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "recovered wall timeline overflows i64",
                )
            })?,
        None => WallUs::default(),
    };
    Ok((first.wall, wall_end, first.decode_time, media_end))
}

fn same_file(left: &std::fs::Metadata, right: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt as _;
    left.dev() == right.dev() && left.ino() == right.ino()
}

fn claimed_temporary_matches(path: &Path, claimed: &std::fs::File) -> bool {
    claimed.metadata().is_ok_and(|identity| {
        std::fs::symlink_metadata(path).is_ok_and(|found| same_file(&identity, &found))
    })
}

fn remove_claimed_temporary(path: &Path, claimed: &std::fs::File) {
    if claimed_temporary_matches(path, claimed) {
        let _ = std::fs::remove_file(path);
    }
}

fn create_index_temporary(index_path: &Path) -> io::Result<(PathBuf, std::fs::File)> {
    let parent = crate::fs::parent_dir(index_path);
    let name = index_path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("index.idx");
    for _ in 0..128 {
        let sequence = RECOVERY_TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let temporary = parent.join(format!(
            ".{name}.irisd-recover-{}-{sequence}.tmp",
            std::process::id()
        ));
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)
        {
            Ok(file) => return Ok((temporary, file)),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "could not claim a unique recovery sidecar temporary",
    ))
}

fn scan_for_recovery<R: io::Read + io::Seek>(reader: R) -> io::Result<ScanReport> {
    let options = ScanOptions {
        verify_crc: true,
        collect_samples: false,
    };
    scan(reader, options)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Cursor, Read, Seek, SeekFrom};
    use std::sync::Arc;

    use bytes::Bytes;
    use iris_core::time::WallUs;
    use iris_mp4::fragment::{FragmentHeader, Sample, encode_fragment};
    use iris_mp4::index::FLAG_STARTS_WITH_SYNC;
    use iris_mp4::init::{SampleEntry, build_init_segment};

    struct Counting<R> {
        inner: R,
        bytes_read: u64,
    }

    impl<R> Counting<R> {
        fn new(inner: R) -> Self {
            Self {
                inner,
                bytes_read: 0,
            }
        }
    }

    impl<R: Read> Read for Counting<R> {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            let read = self.inner.read(buf)?;
            self.bytes_read += read as u64;
            Ok(read)
        }
    }

    impl<R: Seek> Seek for Counting<R> {
        fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
            self.inner.seek(position)
        }
    }

    struct FailOneRead<R> {
        inner: R,
        fail_next: bool,
    }

    impl<R: Read> Read for FailOneRead<R> {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            if self.fail_next {
                self.fail_next = false;
                Err(io::Error::other("injected reader failure"))
            } else {
                self.inner.read(buf)
            }
        }
    }

    impl<R: Seek> Seek for FailOneRead<R> {
        fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
            self.inner.seek(position)
        }
    }

    fn recording(fragments: usize) -> (Vec<u8>, IndexHeader, Vec<IndexRecord>) {
        let entry = Arc::new(SampleEntry::new(
            Bytes::from_static(b"\0\0\0\x10hvc1\0\0\0\0\0\0\0\0"),
            "hvc1".into(),
            16,
            16,
        ));
        let init = build_init_segment(&entry);
        let mut bytes = init.to_vec();
        for i in 0..fragments {
            let sample = Sample {
                data: Bytes::from(vec![0x55; 16 * 1024]),
                duration: 3_000,
                is_sync: true,
            };
            let encoded = encode_fragment(
                &FragmentHeader {
                    sequence: u32::try_from(i + 1).unwrap(),
                    decode_time: u64::try_from(i).unwrap() * 3_000,
                    wall: WallUs::new(i64::try_from(i).unwrap() * 33_333),
                },
                std::slice::from_ref(&sample),
            )
            .unwrap();
            for slice in encoded.io_slices(std::slice::from_ref(&sample)) {
                bytes.extend_from_slice(&slice);
            }
        }
        let report = scan(
            Cursor::new(&bytes),
            ScanOptions {
                verify_crc: true,
                collect_samples: false,
            },
        )
        .unwrap();
        let header = IndexHeader {
            init_len: report.init_len,
            start_wall: WallUs::new(0),
            ..IndexHeader::default()
        };
        let records = report
            .fragments
            .iter()
            .map(|info| IndexRecord {
                flags: FLAG_STARTS_WITH_SYNC,
                ..IndexRecord::from_scan(info)
            })
            .collect();
        (bytes, header, records)
    }

    #[test]
    fn recovery_authoritatively_scans_every_fragment() {
        let (bytes, _, records) = recording(32);
        let mut reader = Counting::new(Cursor::new(&bytes));
        let report = scan_for_recovery(&mut reader).unwrap();
        assert_eq!(report.fragments.len(), records.len());
        let rebuilt: Vec<IndexRecord> = report
            .fragments
            .iter()
            .map(IndexRecord::from_scan)
            .collect();
        assert_eq!(rebuilt, records);
        assert_eq!(report.valid_len, bytes.len() as u64);

        assert!(reader.bytes_read >= bytes.len() as u64);
    }

    #[test]
    fn a_structurally_valid_boundary_before_the_actual_init_falls_back() {
        let (bytes, mut header, mut records) = recording(1);
        header.init_len = 1;
        records[0].offset = 1;
        assert!(records[0].end() <= bytes.len() as u64);
        let dir = tempfile::tempdir().unwrap();
        let media = dir.path().join("recording.mp4");
        let index = media.with_extension("idx");
        std::fs::write(&media, &bytes).unwrap();
        let mut index_bytes = header.encode().to_vec();
        index_bytes.extend(records[0].encode());
        std::fs::write(&index, index_bytes).unwrap();

        let report = recover_segment(&media).unwrap();
        let full = scan(Cursor::new(&bytes), ScanOptions::default()).unwrap();
        assert_eq!(report.fragments, full.fragments.len());
        assert_eq!(report.valid_len, full.valid_len);
        assert_eq!(
            read_index_bounded(&index, MAX_PERSISTED_INDEX_RECORDS)
                .unwrap()
                .1,
            full.fragments
                .iter()
                .map(IndexRecord::from_scan)
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn a_torn_tail_matches_a_full_authoritative_scan() {
        let (mut bytes, _, records) = recording(33);
        let torn_len = records[32].offset + u64::from(records[32].len / 2);
        bytes.truncate(usize::try_from(torn_len).unwrap());

        let mut reader = Counting::new(Cursor::new(&bytes));
        let report = scan_for_recovery(&mut reader).unwrap();
        let rebuilt: Vec<IndexRecord> = report
            .fragments
            .iter()
            .map(IndexRecord::from_scan)
            .collect();

        let mut full_reader = Counting::new(Cursor::new(&bytes));
        let full = scan(
            &mut full_reader,
            ScanOptions {
                verify_crc: true,
                collect_samples: false,
            },
        )
        .unwrap();
        let full_records: Vec<IndexRecord> =
            full.fragments.iter().map(IndexRecord::from_scan).collect();
        assert_eq!(rebuilt, full_records);
        assert_eq!(report.valid_len, full.valid_len);
        assert_eq!(report.stop, full.stop);
        assert_eq!(reader.bytes_read, full_reader.bytes_read);
    }

    #[test]
    fn a_genuine_cached_scan_reader_failure_is_propagated() {
        let (bytes, _, _) = recording(2);
        let error = scan_for_recovery(FailOneRead {
            inner: Cursor::new(&bytes),
            fail_next: true,
        })
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::Other);
    }

    #[test]
    fn authoritative_recovery_scan_returns_all_media_facts() {
        let (bytes, _, records) = recording(4);
        let report = scan_for_recovery(Cursor::new(&bytes)).unwrap();
        assert_eq!(report.fragments.len(), records.len());
        assert_eq!(report.valid_len, bytes.len() as u64);
        assert_eq!(report.stop, None);
    }

    #[test]
    fn recovery_rebuilds_forged_prefix_facts_from_authoritative_media() {
        for changed in [0usize, 1, 2] {
            let dir = tempfile::tempdir().unwrap();
            let media = dir.path().join("recording.mp4");
            let index = media.with_extension("idx");
            let (bytes, header, records) = recording(4);
            std::fs::write(&media, &bytes).unwrap();
            let mut forged = records.clone();
            forged[changed].decode_time = forged[changed].decode_time.saturating_add(1);
            let mut index_bytes = header.encode().to_vec();
            for record in forged {
                index_bytes.extend(record.encode());
            }
            std::fs::write(&index, index_bytes).unwrap();

            recover_segment(&media).unwrap();
            assert_eq!(
                read_index_bounded(&index, MAX_PERSISTED_INDEX_RECORDS)
                    .unwrap()
                    .1,
                records,
                "forged record {changed} must be rebuilt from media"
            );
        }
    }

    #[test]
    fn recovery_rebuilds_header_authority_from_media() {
        let dir = tempfile::tempdir().unwrap();
        let media = dir.path().join("recording.mp4");
        let index = media.with_extension("idx");
        let (bytes, mut header, records) = recording(2);
        header.start_wall = WallUs::new(9_000_000);
        header.run_id = u64::MAX;
        header.sample_entry_hash = u32::MAX;
        std::fs::write(&media, &bytes).unwrap();
        let mut index_bytes = header.encode().to_vec();
        for record in records {
            index_bytes.extend(record.encode());
        }
        std::fs::write(&index, index_bytes).unwrap();

        recover_segment(&media).unwrap();
        let (rebuilt, _) = read_index_bounded(&index, MAX_PERSISTED_INDEX_RECORDS).unwrap();
        assert_eq!(rebuilt.start_wall, WallUs::new(0));
        assert_eq!(rebuilt.run_id, 0);
        assert_eq!(rebuilt.sample_entry_hash, 0);
    }

    #[test]
    fn a_nonreplaceable_sidecar_failure_leaves_media_bytes_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        let media = dir.path().join("recording.mp4");
        let index = media.with_extension("idx");
        let (bytes, _, _) = recording(2);
        std::fs::write(&media, &bytes).unwrap();
        std::fs::create_dir(&index).unwrap();

        assert!(recover_segment(&media).is_err());
        assert_eq!(std::fs::read(&media).unwrap(), bytes);
        assert!(index.is_dir());
    }

    #[test]
    fn an_idx_media_basename_is_rejected_without_changing_its_inode_or_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let media = dir.path().join("self.idx");
        let (bytes, _, _) = recording(2);
        std::fs::write(&media, &bytes).unwrap();
        let before = std::fs::metadata(&media).unwrap();

        assert!(recover_segment(&media).is_err());
        assert_eq!(std::fs::read(&media).unwrap(), bytes);
        assert!(same_file(&before, &std::fs::metadata(&media).unwrap()));
        assert!(std::fs::read_dir(dir.path()).unwrap().all(|entry| {
            !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .contains(".irisd-recover-")
        }));
    }

    #[test]
    fn publication_failures_leave_media_untouched_and_no_owned_temporary() {
        for failure in [
            RecoveryPublicationStep::BeforeRename,
            RecoveryPublicationStep::BeforeDirectorySync,
        ] {
            let dir = tempfile::tempdir().unwrap();
            let media = dir.path().join("recording.mp4");
            let index = media.with_extension("idx");
            let (bytes, _, records) = recording(2);
            std::fs::write(&media, &bytes).unwrap();
            std::fs::write(&index, b"prior complete entry").unwrap();

            let error = recover_segment_with_index_and_publication(&media, &index, |step| {
                if step == failure {
                    Err(io::Error::other("injected publication failure"))
                } else {
                    Ok(())
                }
            })
            .unwrap_err();
            assert!(error.to_string().contains("injected publication failure"));
            assert_eq!(std::fs::read(&media).unwrap(), bytes);
            if failure == RecoveryPublicationStep::BeforeRename {
                assert_eq!(std::fs::read(&index).unwrap(), b"prior complete entry");
            } else {
                assert_eq!(
                    read_index_bounded(&index, MAX_PERSISTED_INDEX_RECORDS)
                        .unwrap()
                        .1
                        .len(),
                    records.len()
                );
            }
            assert!(std::fs::read_dir(dir.path()).unwrap().all(|entry| {
                !entry
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .contains(".irisd-recover-")
            }));
        }
    }

    #[test]
    fn failed_directory_sync_never_removes_a_recreated_former_temporary_name() {
        use std::cell::RefCell;

        let dir = tempfile::tempdir().unwrap();
        let media = dir.path().join("recording.mp4");
        let index = media.with_extension("idx");
        let (bytes, _, _) = recording(2);
        std::fs::write(&media, &bytes).unwrap();
        let temporary = RefCell::new(None::<PathBuf>);
        let error = recover_segment_with_index_and_publication(&media, &index, |step| {
            match step {
                RecoveryPublicationStep::BeforeRename => {
                    let path = std::fs::read_dir(dir.path())?
                        .map(|entry| entry.map(|entry| entry.path()))
                        .collect::<Result<Vec<_>, _>>()?
                        .into_iter()
                        .find(|path| {
                            path.file_name().is_some_and(|name| {
                                name.to_string_lossy().contains(".irisd-recover-")
                            })
                        })
                        .expect("owned recovery temporary exists before rename");
                    *temporary.borrow_mut() = Some(path);
                }
                RecoveryPublicationStep::BeforeDirectorySync => {
                    std::fs::write(temporary.borrow().as_ref().unwrap(), b"concurrent sentinel")?;
                    return Err(io::Error::other("injected directory sync failure"));
                }
            }
            Ok(())
        })
        .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("injected directory sync failure")
        );
        assert_eq!(std::fs::read(&media).unwrap(), bytes);
        assert_eq!(
            std::fs::read(temporary.into_inner().unwrap()).unwrap(),
            b"concurrent sentinel"
        );
    }

    #[test]
    fn pre_rename_failure_never_removes_a_recreated_temporary_name() {
        use std::cell::RefCell;

        let dir = tempfile::tempdir().unwrap();
        let media = dir.path().join("recording.mp4");
        let index = media.with_extension("idx");
        let (bytes, _, _) = recording(2);
        std::fs::write(&media, &bytes).unwrap();
        let recreated = RefCell::new(None::<PathBuf>);
        let error = recover_segment_with_index_and_publication(&media, &index, |step| {
            if step != RecoveryPublicationStep::BeforeRename {
                return Ok(());
            }
            let temporary = std::fs::read_dir(dir.path())?
                .map(|entry| entry.map(|entry| entry.path()))
                .collect::<Result<Vec<_>, _>>()?
                .into_iter()
                .find(|path| {
                    path.file_name()
                        .is_some_and(|name| name.to_string_lossy().contains(".irisd-recover-"))
                })
                .expect("recovery temporary");
            std::fs::remove_file(&temporary)?;
            std::fs::write(&temporary, b"concurrent sentinel")?;
            recreated.replace(Some(temporary));
            Ok(())
        })
        .unwrap_err();
        assert!(error.to_string().contains("temporary ownership changed"));
        let recreated = recreated.into_inner().unwrap();
        assert_eq!(std::fs::read(recreated).unwrap(), b"concurrent sentinel");
    }

    #[test]
    fn a_sidecar_alias_of_media_is_rejected_without_changing_media() {
        for hard_link in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let media = dir.path().join("recording.mp4");
            let index = media.with_extension("idx");
            let (bytes, _, _) = recording(2);
            std::fs::write(&media, &bytes).unwrap();
            if hard_link {
                std::fs::hard_link(&media, &index).unwrap();
            } else {
                std::os::unix::fs::symlink(&media, &index).unwrap();
            }

            assert!(recover_segment(&media).is_err());
            assert_eq!(std::fs::read(&media).unwrap(), bytes);
        }
    }

    #[test]
    fn replacing_an_arbitrary_sidecar_alias_preserves_its_victim() {
        for hard_link in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let media = dir.path().join("recording.mp4");
            let index = media.with_extension("idx");
            let victim = dir.path().join("victim");
            let (bytes, _, _) = recording(2);
            std::fs::write(&media, &bytes).unwrap();
            std::fs::write(&victim, b"sentinel").unwrap();
            if hard_link {
                std::fs::hard_link(&victim, &index).unwrap();
            } else {
                std::os::unix::fs::symlink(&victim, &index).unwrap();
            }

            recover_segment(&media).unwrap();
            assert_eq!(std::fs::read(&victim).unwrap(), b"sentinel");
            assert!(read_index_bounded(&index, MAX_PERSISTED_INDEX_RECORDS).is_ok());
        }
    }

    #[test]
    fn rooted_recovery_never_follows_a_final_media_symlink() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("root");
        let approved =
            crate::fs::admit_recording_root(&root, crate::fs::RootExpectation::Fresh).unwrap();
        let media = root.join("cam1/main/2026-01-01/recording.mp4");
        std::fs::create_dir_all(media.parent().unwrap()).unwrap();
        let victim = dir.path().join("victim.mp4");
        let (bytes, _, _) = recording(2);
        std::fs::write(&victim, &bytes).unwrap();
        std::os::unix::fs::symlink(&victim, &media).unwrap();

        assert!(recover_segment_beneath(&approved, &media).is_err());
        assert_eq!(std::fs::read(&victim).unwrap(), bytes);
    }

    #[test]
    fn oversized_sidecar_falls_back_to_media_and_is_rebuilt_within_the_limit() {
        let dir = tempfile::tempdir().unwrap();
        let media = dir.path().join("recording.mp4");
        let index = media.with_extension("idx");
        let (bytes, _, records) = recording(2);
        std::fs::write(&media, &bytes).unwrap();
        let sidecar = std::fs::File::create(&index).unwrap();
        let oversized = HEADER_LEN + MAX_PERSISTED_INDEX_RECORDS * RECORD_LEN + RECORD_LEN;
        sidecar.set_len(u64::try_from(oversized).unwrap()).unwrap();

        let report = recover_segment(&media).unwrap();
        assert_eq!(report.fragments, records.len());
        assert_eq!(std::fs::read(&media).unwrap(), bytes);
        assert_eq!(
            read_index_bounded(&index, MAX_PERSISTED_INDEX_RECORDS)
                .unwrap()
                .1
                .len(),
            records.len()
        );
    }
}
