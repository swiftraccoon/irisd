//! Repairing a file that was open when the process died.

use std::collections::HashMap;
use std::ffi::{OsStr, OsString};
use std::fs::{File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use iris_core::time::WallUs;
#[cfg(test)]
use iris_mp4::index::RECORD_LEN;
use iris_mp4::index::{
    FLAG_DISCONTINUITY_BEFORE, FLAG_NEW_PARAMETERS, HEADER_LEN, IndexHeader, IndexRecord,
    IndexWriter, MAX_PERSISTED_INDEX_RECORDS, read_index_file_bounded,
};
use iris_mp4::reader::{ScanOptions, ScanReport, ScanStop, scan_with_limit};
use rustix::fs::{AtFlags, CWD, FileType, Mode, OFlags};
use thiserror::Error;

static RECOVERY_TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// The sidecar header fields that bind a recording to its catalog row: the
/// writer's run and the CRC-32C of the sample entry in force.
///
/// The media file records neither, so a scan cannot rebuild them. Zero means
/// unknown in either field; [`IndexBinding::UNKNOWN`] and [`Default`] leave
/// both unknown.
///
/// ```
/// use iris_store::recover::IndexBinding;
///
/// let binding = IndexBinding::new(3, 0x6142_98a5);
/// assert_eq!(binding.run_id(), 3);
/// assert_eq!(IndexBinding::default(), IndexBinding::UNKNOWN);
/// ```
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct IndexBinding {
    run_id: u64,
    sample_entry_hash: u32,
}

impl IndexBinding {
    /// A binding whose run and sample entry are both unknown.
    pub const UNKNOWN: Self = Self::new(0, 0);

    /// A binding to `run_id` (the catalog's `run_seq`) and
    /// `sample_entry_hash`; zero in either field means unknown.
    #[must_use]
    pub const fn new(run_id: u64, sample_entry_hash: u32) -> Self {
        Self {
            run_id,
            sample_entry_hash,
        }
    }

    /// The writer run, or zero when unknown.
    #[must_use]
    pub const fn run_id(self) -> u64 {
        self.run_id
    }

    /// The CRC-32C of the sample entry, or zero when unknown.
    #[must_use]
    pub const fn sample_entry_hash(self) -> u32 {
        self.sample_entry_hash
    }

    /// Each known field of `self`, with unknown fields taken from `fallback`.
    const fn known_or(self, fallback: Self) -> Self {
        Self {
            run_id: if self.run_id == 0 {
                fallback.run_id
            } else {
                self.run_id
            },
            sample_entry_hash: if self.sample_entry_hash == 0 {
                fallback.sample_entry_hash
            } else {
                self.sample_entry_hash
            },
        }
    }
}

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
    /// The run and sample-entry binding written into the rebuilt sidecar
    /// header.
    pub binding: IndexBinding,
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
    /// The verified root holds no such recording: a layout directory or the
    /// media name is absent. A missing or replaced root is an [`Io`] error
    /// instead.
    ///
    /// [`Io`]: RecoverError::Io
    #[error("{0} is absent beneath its verified recording root")]
    Absent(PathBuf),
    /// The media holds more complete fragments than one sidecar index may
    /// describe; recovery leaves the media and its sidecar unchanged.
    #[error("{path} holds more than {limit} fragments; recovery leaves it unchanged")]
    TooManyFragments {
        /// The file involved.
        path: PathBuf,
        /// The most fragments recovery publishes in one sidecar.
        limit: usize,
    },
}

/// Scans `mp4_path` with CRC verification, truncates it to its valid prefix,
/// and rewrites the sidecar index. Flags and camera timestamps from a prior
/// index are preserved for fragments that survive, and so is each known field
/// of the prior header's run and sample-entry binding.
///
/// Directory components resolve normally and a final media symlink is
/// followed, as suits an operator naming a file. The sidecar beside that name
/// is opened through the directory's descriptor without following a symlink
/// or blocking on a FIFO; anything but a regular sidecar counts as absent and
/// is replaced by the rebuilt one.
///
/// # Errors
///
/// [`RecoverError::NotARecording`] without a complete initialization,
/// [`RecoverError::TooManyFragments`] when the media holds more than
/// [`MAX_PERSISTED_INDEX_RECORDS`] fragments (both leave every byte
/// unchanged), and [`RecoverError::Io`] for file-system failures.
pub fn recover_segment(mp4_path: &Path) -> Result<RecoveryReport, RecoverError> {
    recover_path(
        mp4_path,
        IndexBinding::UNKNOWN,
        MAX_PERSISTED_INDEX_RECORDS,
        |_| Ok(()),
    )
}

/// Recovers a recording through an already-approved root. Every layout
/// component is opened no-follow/beneath by [`crate::ApprovedRoot`], and the
/// media plus sidecar remain addressed through pinned descriptors throughout
/// recovery and publication.
///
/// `expected` is the binding the caller's catalog holds for this recording;
/// it supplies each field the prior sidecar header does not provide, so a
/// rebuilt header still matches its catalog row. The prior header's known
/// fields take precedence. A missing, symlinked, or special sidecar counts as
/// absent and is replaced; no open blocks on a FIFO or device.
///
/// # Errors
///
/// [`RecoverError::Absent`] when a layout directory or the media name is
/// missing beneath the verified root, plus the errors of
/// [`recover_segment`].
pub fn recover_segment_beneath(
    root: &crate::ApprovedRoot,
    mp4_path: &Path,
    expected: IndexBinding,
) -> Result<RecoveryReport, RecoverError> {
    let (directory, media, media_name) = crate::fs::open_recording_beneath(root, mp4_path)
        .map_err(|source| RecoverError::Io {
            path: mp4_path.to_path_buf(),
            source,
        })?
        .ok_or_else(|| RecoverError::Absent(mp4_path.to_path_buf()))?;
    recover_open(
        &directory,
        &media,
        &media_name,
        mp4_path,
        expected,
        MAX_PERSISTED_INDEX_RECORDS,
        |_| Ok(()),
    )
}

fn recover_path(
    mp4_path: &Path,
    expected: IndexBinding,
    max_fragments: usize,
    publication_step: impl FnMut(RecoveryPublicationStep) -> io::Result<()>,
) -> Result<RecoveryReport, RecoverError> {
    let io_err = |source: io::Error| RecoverError::Io {
        path: mp4_path.to_path_buf(),
        source,
    };
    let media_name = mp4_path.file_name().ok_or_else(|| {
        io_err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "the recording path has no file name",
        ))
    })?;
    let directory = rustix::fs::openat(
        CWD,
        crate::fs::parent_dir(mp4_path),
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map(File::from)
    .map_err(|error| io_err(error.into()))?;
    let media = OpenOptions::new()
        .read(true)
        .write(true)
        .open(mp4_path)
        .map_err(io_err)?;
    if !media.metadata().map_err(io_err)?.is_file() {
        return Err(io_err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "recording path is not a regular file",
        )));
    }
    recover_open(
        &directory,
        &media,
        media_name,
        mp4_path,
        expected,
        max_fragments,
        publication_step,
    )
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RecoveryPublicationStep {
    BeforeRename,
    BeforeDirectorySync,
}

/// What a prior sidecar contributes to the rebuilt one.
#[derive(Debug, Default)]
struct PriorIndex {
    binding: IndexBinding,
    records: Vec<IndexRecord>,
}

/// Recovers `media`, named `media_name` in `directory`; `display` names it in
/// reports and errors.
fn recover_open(
    directory: &File,
    media: &File,
    media_name: &OsStr,
    display: &Path,
    expected: IndexBinding,
    max_fragments: usize,
    mut publication_step: impl FnMut(RecoveryPublicationStep) -> io::Result<()>,
) -> Result<RecoveryReport, RecoverError> {
    let io_err = |source: io::Error| RecoverError::Io {
        path: display.to_path_buf(),
        source,
    };
    let index_name = Path::new(media_name).with_extension("idx").into_os_string();
    if index_name == media_name {
        return Err(io_err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "the recording path is also its derived .idx sidecar path",
        )));
    }
    if is_directory_at(directory, &index_name) {
        return Err(io_err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "the derived sidecar destination is a directory",
        )));
    }
    let media_metadata = media.metadata().map_err(io_err)?;
    let prior = prior_index(directory, &index_name, &media_metadata).map_err(io_err)?;
    let original_len = media_metadata.len();
    let report = scan_for_recovery(media, max_fragments).map_err(io_err)?;
    if report.init_len == 0 {
        return Err(RecoverError::NotARecording(display.to_path_buf()));
    }
    if let Some(ScanStop::FragmentLimit { limit, .. }) = report.stop {
        return Err(RecoverError::TooManyFragments {
            path: display.to_path_buf(),
            limit,
        });
    }
    let truncated = report.valid_len < original_len;

    let old_records: HashMap<u64, IndexRecord> = prior
        .records
        .iter()
        .map(|record| (record.offset, *record))
        .collect();
    let binding = prior.binding.known_or(expected);
    let header = IndexHeader {
        start_wall: report
            .fragments
            .first()
            .and_then(|fragment| fragment.wall)
            .unwrap_or_default(),
        run_id: binding.run_id(),
        init_len: report.init_len,
        sample_entry_hash: binding.sample_entry_hash(),
        ..IndexHeader::default()
    };
    let idx_err = |source: io::Error| RecoverError::Io {
        path: display.with_extension("idx"),
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
    publish_rebuilt_index(
        directory,
        &index_name,
        &header,
        &rebuilt,
        &mut publication_step,
    )
    .map_err(idx_err)?;

    // Publication failures above leave media untouched. Publishing the cache
    // first is crash-safe because it describes a verified media prefix; a
    // retry authoritatively scans every fragment before republishing it.
    // An equal-length truncate releases KEEP_SIZE reservations beyond EOF on
    // the supported filesystems, just as normal sealing does.
    media.set_len(report.valid_len).map_err(io_err)?;
    // Complete fragments can survive a process crash only in the page cache.
    // Recovery must make them durable before the catalog can seal the file.
    media.sync_data().map_err(io_err)?;

    Ok(RecoveryReport {
        path: display.to_path_buf(),
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
        binding,
    })
}

/// Opens the prior sidecar `index_name` beside the media. A missing,
/// symlinked, unreadable, or special sidecar is absent: publication replaces
/// its name with a regular file and never opens it. A sidecar that is the
/// media inode itself is rejected.
fn prior_index(
    directory: &File,
    index_name: &OsStr,
    media: &std::fs::Metadata,
) -> io::Result<PriorIndex> {
    let Ok(index) =
        crate::fs::open_regular_beneath(directory, index_name, OFlags::RDONLY, "recording sidecar")
    else {
        return Ok(PriorIndex::default());
    };
    if same_file(media, &index.metadata()?) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "the derived sidecar aliases the recording inode",
        ));
    }
    Ok(read_prior_index(index))
}

/// Reads what a prior sidecar can contribute: the header's binding whenever
/// the header decodes, even if the records do not, and the records when the
/// whole file is within the persisted-record ceiling.
fn read_prior_index(index: File) -> PriorIndex {
    use std::os::unix::fs::FileExt as _;

    let mut header = [0u8; HEADER_LEN];
    let binding = index
        .read_exact_at(&mut header, 0)
        .ok()
        .and_then(|()| IndexHeader::decode(&header).ok())
        .map_or(IndexBinding::UNKNOWN, |header| {
            IndexBinding::new(header.run_id, header.sample_entry_hash)
        });
    let records = read_index_file_bounded(index, MAX_PERSISTED_INDEX_RECORDS)
        .map(|(_, records)| records)
        .unwrap_or_default();
    PriorIndex { binding, records }
}

fn is_directory_at(directory: &File, name: &OsStr) -> bool {
    rustix::fs::statat(directory, name, AtFlags::SYMLINK_NOFOLLOW)
        .is_ok_and(|stat| FileType::from_raw_mode(stat.st_mode) == FileType::Directory)
}

fn publish_rebuilt_index(
    directory: &File,
    index_name: &OsStr,
    header: &IndexHeader,
    records: &[IndexRecord],
    mut publication_step: impl FnMut(RecoveryPublicationStep) -> io::Result<()>,
) -> io::Result<()> {
    let (temporary, temporary_file) = create_index_temporary(directory, index_name)?;
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
        if !claimed_temporary_matches(directory, &temporary, &temporary_claimed) {
            return Err(io::Error::other(
                "recovery sidecar temporary ownership changed",
            ));
        }
        rustix::fs::renameat(directory, temporary.as_os_str(), directory, index_name)?;
        temporary_owned = false;
        publication_step(RecoveryPublicationStep::BeforeDirectorySync)?;
        directory.sync_all()
    })();
    if result.is_err() && temporary_owned {
        remove_claimed_temporary(directory, &temporary, &temporary_claimed);
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

fn claimed_temporary_matches(directory: &File, name: &OsStr, claimed: &File) -> bool {
    let (Ok(identity), Ok(found)) = (
        rustix::fs::fstat(claimed),
        rustix::fs::statat(directory, name, AtFlags::SYMLINK_NOFOLLOW),
    ) else {
        return false;
    };
    identity.st_dev == found.st_dev && identity.st_ino == found.st_ino
}

fn remove_claimed_temporary(directory: &File, name: &OsStr, claimed: &File) {
    if claimed_temporary_matches(directory, name, claimed) {
        let _ = rustix::fs::unlinkat(directory, name, AtFlags::empty());
    }
}

/// Exclusively creates a private (mode 0600) sibling temporary for the
/// rebuilt sidecar in `directory`, without following any link.
fn create_index_temporary(directory: &File, index_name: &OsStr) -> io::Result<(OsString, File)> {
    let name = index_name.to_str().unwrap_or("index.idx");
    for _ in 0..128 {
        let sequence = RECOVERY_TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let temporary = OsString::from(format!(
            ".{name}.irisd-recover-{}-{sequence}.tmp",
            std::process::id()
        ));
        match crate::fs::open_file_beneath(
            directory,
            &temporary,
            OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL,
            Mode::RUSR | Mode::WUSR,
        ) {
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

fn scan_for_recovery<R: io::Read + io::Seek>(
    reader: R,
    max_fragments: usize,
) -> io::Result<ScanReport> {
    let options = ScanOptions {
        verify_crc: true,
        collect_samples: false,
    };
    scan_with_limit(reader, options, max_fragments)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Cursor, Read, Seek, SeekFrom};
    use std::sync::Arc;
    use std::sync::mpsc;
    use std::time::Duration;

    use bytes::Bytes;
    use iris_core::time::WallUs;
    use iris_mp4::fragment::{FragmentHeader, Sample, encode_fragment};
    use iris_mp4::index::{FLAG_STARTS_WITH_SYNC, read_index_bounded};
    use iris_mp4::init::{SampleEntry, build_init_segment};
    use iris_mp4::reader::scan;

    fn recover_with_publication(
        media: &Path,
        publication_step: impl FnMut(RecoveryPublicationStep) -> io::Result<()>,
    ) -> Result<RecoveryReport, RecoverError> {
        recover_path(
            media,
            IndexBinding::UNKNOWN,
            MAX_PERSISTED_INDEX_RECORDS,
            publication_step,
        )
    }

    /// Runs `call` on a thread and fails unless it returns within two
    /// seconds; a call blocked in an open of `fifo` is released first.
    fn within_deadline<T: Send + 'static>(
        fifo: &Path,
        call: impl FnOnce() -> T + Send + 'static,
    ) -> T {
        let (sender, receiver) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            let _ = sender.send(call());
        });
        if let Ok(value) = receiver.recv_timeout(Duration::from_secs(2)) {
            worker.join().unwrap();
            return value;
        }
        for _ in 0..20 {
            let _ = rustix::fs::openat(
                CWD,
                fifo,
                OFlags::WRONLY | OFlags::NONBLOCK | OFlags::CLOEXEC,
                Mode::empty(),
            );
            if receiver.recv_timeout(Duration::from_millis(100)).is_ok() {
                break;
            }
        }
        panic!(
            "call on {} blocked for more than two seconds",
            fifo.display()
        );
    }

    fn mkfifo(path: &Path) {
        rustix::fs::mkfifoat(CWD, path, Mode::RUSR | Mode::WUSR).unwrap();
    }

    /// An approved root holding `recording(fragments)` and its sidecar.
    fn rooted_recording(
        fragments: usize,
    ) -> (
        tempfile::TempDir,
        crate::ApprovedRoot,
        PathBuf,
        Vec<u8>,
        IndexHeader,
        Vec<IndexRecord>,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("root");
        let approved =
            crate::fs::admit_recording_root(&root, crate::fs::RootExpectation::Fresh).unwrap();
        let media = root.join("cam1/main/2026-01-01/recording.mp4");
        std::fs::create_dir_all(media.parent().unwrap()).unwrap();
        let (bytes, header, records) = recording(fragments);
        std::fs::write(&media, &bytes).unwrap();
        write_sidecar(&media.with_extension("idx"), &header, &records);
        (dir, approved, media, bytes, header, records)
    }

    fn write_sidecar(index: &Path, header: &IndexHeader, records: &[IndexRecord]) {
        let mut index_bytes = header.encode().to_vec();
        for record in records {
            index_bytes.extend(record.encode());
        }
        std::fs::write(index, index_bytes).unwrap();
    }

    fn rebuilt_header(index: &Path) -> IndexHeader {
        read_index_bounded(index, MAX_PERSISTED_INDEX_RECORDS)
            .unwrap()
            .0
    }

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
        let report = scan_for_recovery(&mut reader, MAX_PERSISTED_INDEX_RECORDS).unwrap();
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
        let report = scan_for_recovery(&mut reader, MAX_PERSISTED_INDEX_RECORDS).unwrap();
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
        let error = scan_for_recovery(
            FailOneRead {
                inner: Cursor::new(&bytes),
                fail_next: true,
            },
            MAX_PERSISTED_INDEX_RECORDS,
        )
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::Other);
    }

    #[test]
    fn authoritative_recovery_scan_returns_all_media_facts() {
        let (bytes, _, records) = recording(4);
        let report = scan_for_recovery(Cursor::new(&bytes), MAX_PERSISTED_INDEX_RECORDS).unwrap();
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
    fn recovery_rebuilds_media_facts_and_keeps_a_readable_prior_binding() {
        let dir = tempfile::tempdir().unwrap();
        let media = dir.path().join("recording.mp4");
        let index = media.with_extension("idx");
        let (bytes, mut header, records) = recording(2);
        header.start_wall = WallUs::new(9_000_000);
        header.run_id = u64::MAX;
        header.sample_entry_hash = u32::MAX;
        std::fs::write(&media, &bytes).unwrap();
        write_sidecar(&index, &header, &records);

        let report = recover_segment(&media).unwrap();
        let rebuilt = rebuilt_header(&index);
        assert_eq!(rebuilt.start_wall, WallUs::new(0));
        assert_eq!(rebuilt.run_id, u64::MAX);
        assert_eq!(rebuilt.sample_entry_hash, u32::MAX);
        assert_eq!(report.binding, IndexBinding::new(u64::MAX, u32::MAX));
    }

    #[test]
    fn a_rebuilt_header_without_any_binding_source_keeps_zeros() {
        let dir = tempfile::tempdir().unwrap();
        let media = dir.path().join("recording.mp4");
        let (bytes, _, _) = recording(2);
        std::fs::write(&media, &bytes).unwrap();

        let report = recover_segment(&media).unwrap();
        let rebuilt = rebuilt_header(&media.with_extension("idx"));
        assert_eq!((rebuilt.run_id, rebuilt.sample_entry_hash), (0, 0));
        assert_eq!(report.binding, IndexBinding::UNKNOWN);
    }

    #[test]
    fn a_caller_binding_fills_each_field_the_prior_header_lacks() {
        let expected = IndexBinding::new(7, 0x0abc_def0);
        // (prior sidecar header binding, binding the rebuilt header carries)
        for (prior, rebuilt) in [
            (None, expected),
            (Some(IndexBinding::UNKNOWN), expected),
            (
                Some(IndexBinding::new(3, 0)),
                IndexBinding::new(3, 0x0abc_def0),
            ),
            (Some(IndexBinding::new(0, 0x55)), IndexBinding::new(7, 0x55)),
            (Some(IndexBinding::new(3, 0x55)), IndexBinding::new(3, 0x55)),
        ] {
            let (_dir, approved, media, _, mut header, records) = rooted_recording(2);
            let index = media.with_extension("idx");
            match prior {
                Some(binding) => {
                    header.run_id = binding.run_id();
                    header.sample_entry_hash = binding.sample_entry_hash();
                    write_sidecar(&index, &header, &records);
                }
                None => std::fs::remove_file(&index).unwrap(),
            }
            let report = recover_segment_beneath(&approved, &media, expected).unwrap();
            let header = rebuilt_header(&index);
            assert_eq!(
                IndexBinding::new(header.run_id, header.sample_entry_hash),
                rebuilt,
                "prior {prior:?}"
            );
            assert_eq!(report.binding, rebuilt);
        }
    }

    #[test]
    fn an_unreadable_prior_header_yields_to_the_caller_binding() {
        let (_dir, approved, media, _, _, _) = rooted_recording(2);
        let index = media.with_extension("idx");
        std::fs::write(&index, b"not an index header").unwrap();
        let expected = IndexBinding::new(2, 9);
        recover_segment_beneath(&approved, &media, expected).unwrap();
        let header = rebuilt_header(&index);
        assert_eq!(
            IndexBinding::new(header.run_id, header.sample_entry_hash),
            expected
        );
    }

    #[test]
    fn a_fifo_sidecar_is_replaced_without_blocking() {
        let (_dir, approved, media, bytes, _, records) = rooted_recording(3);
        let index = media.with_extension("idx");
        std::fs::remove_file(&index).unwrap();
        mkfifo(&index);
        let (root, path) = (approved.clone(), media.clone());
        let report = within_deadline(&index, move || {
            recover_segment_beneath(&root, &path, IndexBinding::new(1, 2))
        })
        .unwrap();
        assert_eq!(report.fragments, records.len());
        assert!(std::fs::symlink_metadata(&index).unwrap().is_file());
        assert_eq!(
            read_index_bounded(&index, MAX_PERSISTED_INDEX_RECORDS)
                .unwrap()
                .1,
            records
        );
        assert_eq!(rebuilt_header(&index).run_id, 1);
        assert_eq!(std::fs::read(&media).unwrap(), bytes);

        let path = media.clone();
        std::fs::remove_file(&index).unwrap();
        mkfifo(&index);
        let report = within_deadline(&index, move || recover_segment(&path)).unwrap();
        assert_eq!(report.fragments, records.len());
        assert!(std::fs::symlink_metadata(&index).unwrap().is_file());
    }

    #[test]
    fn a_symlinked_sidecar_counts_as_absent_and_its_target_is_untouched() {
        let (dir, approved, media, bytes, mut header, records) = rooted_recording(2);
        let index = media.with_extension("idx");
        let outside = dir.path().join("outside.idx");
        header.run_id = 99;
        write_sidecar(&outside, &header, &records);
        let outside_bytes = std::fs::read(&outside).unwrap();
        std::fs::remove_file(&index).unwrap();
        std::os::unix::fs::symlink(&outside, &index).unwrap();

        recover_segment_beneath(&approved, &media, IndexBinding::new(4, 5)).unwrap();
        assert!(std::fs::symlink_metadata(&index).unwrap().is_file());
        assert_eq!(
            rebuilt_header(&index).run_id,
            4,
            "the link target is not read"
        );
        assert_eq!(std::fs::read(&outside).unwrap(), outside_bytes);
        assert_eq!(std::fs::read(&media).unwrap(), bytes);
    }

    #[test]
    fn a_fifo_media_beneath_a_root_is_rejected_promptly() {
        let (_dir, approved, media, _, _, _) = rooted_recording(1);
        std::fs::remove_file(&media).unwrap();
        mkfifo(&media);
        let (root, path) = (approved.clone(), media.clone());
        let error = within_deadline(&media, move || {
            recover_segment_beneath(&root, &path, IndexBinding::UNKNOWN)
        })
        .unwrap_err();
        assert!(
            matches!(&error, RecoverError::Io { source, .. }
                if source.kind() == io::ErrorKind::InvalidData),
            "{error}"
        );
    }

    #[test]
    fn missing_media_or_layout_beneath_a_verified_root_is_absent() {
        let (_dir, approved, media, _, _, _) = rooted_recording(1);
        std::fs::remove_file(&media).unwrap();
        assert!(matches!(
            recover_segment_beneath(&approved, &media, IndexBinding::UNKNOWN),
            Err(RecoverError::Absent(_))
        ));
        std::fs::remove_dir_all(media.parent().unwrap()).unwrap();
        assert!(matches!(
            recover_segment_beneath(&approved, &media, IndexBinding::UNKNOWN),
            Err(RecoverError::Absent(_))
        ));
        let other_camera = approved.path().join("cam9/main/2026-01-01/recording.mp4");
        assert!(matches!(
            recover_segment_beneath(&approved, &other_camera, IndexBinding::UNKNOWN),
            Err(RecoverError::Absent(_))
        ));
    }

    #[test]
    fn a_missing_root_is_an_io_error_not_an_absence() {
        let (dir, approved, media, _, _, _) = rooted_recording(1);
        std::fs::rename(approved.path(), dir.path().join("unmounted")).unwrap();
        assert!(matches!(
            recover_segment_beneath(&approved, &media, IndexBinding::UNKNOWN),
            Err(RecoverError::Io { source, .. }) if source.kind() == io::ErrorKind::NotFound
        ));
    }

    #[test]
    fn recovery_beyond_the_fragment_limit_leaves_media_and_sidecar_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        let media = dir.path().join("recording.mp4");
        let index = media.with_extension("idx");
        let (mut bytes, header, records) = recording(3);
        bytes.extend_from_slice(&[0; 100]);
        std::fs::write(&media, &bytes).unwrap();
        write_sidecar(&index, &header, &records[..1]);
        let index_bytes = std::fs::read(&index).unwrap();

        let error = recover_path(&media, IndexBinding::UNKNOWN, 2, |_| Ok(())).unwrap_err();
        assert!(
            matches!(error, RecoverError::TooManyFragments { limit: 2, .. }),
            "{error}"
        );
        assert_eq!(std::fs::read(&media).unwrap(), bytes);
        assert_eq!(std::fs::read(&index).unwrap(), index_bytes);
        assert!(std::fs::read_dir(dir.path()).unwrap().all(|entry| {
            !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .contains(".irisd-recover-")
        }));

        let report = recover_path(&media, IndexBinding::UNKNOWN, 3, |_| Ok(())).unwrap();
        assert_eq!(report.fragments, 3);
        assert!(report.truncated);
    }

    #[test]
    fn recovery_publishes_a_private_sidecar() {
        use std::os::unix::fs::PermissionsExt as _;

        let dir = tempfile::tempdir().unwrap();
        let media = dir.path().join("recording.mp4");
        let (bytes, _, _) = recording(1);
        std::fs::write(&media, &bytes).unwrap();
        recover_segment(&media).unwrap();
        let mode = std::fs::metadata(media.with_extension("idx"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o077, 0, "sidecar mode {mode:o}");
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

            let error = recover_with_publication(&media, |step| {
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
        let (bytes, _, _) = recording(2);
        std::fs::write(&media, &bytes).unwrap();
        let temporary = RefCell::new(None::<PathBuf>);
        let error = recover_with_publication(&media, |step| {
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
        let (bytes, _, _) = recording(2);
        std::fs::write(&media, &bytes).unwrap();
        let recreated = RefCell::new(None::<PathBuf>);
        let error = recover_with_publication(&media, |step| {
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
    fn a_hard_linked_sidecar_alias_of_media_is_rejected_without_changing_media() {
        let dir = tempfile::tempdir().unwrap();
        let media = dir.path().join("recording.mp4");
        let index = media.with_extension("idx");
        let (bytes, _, _) = recording(2);
        std::fs::write(&media, &bytes).unwrap();
        std::fs::hard_link(&media, &index).unwrap();

        assert!(recover_segment(&media).is_err());
        assert_eq!(std::fs::read(&media).unwrap(), bytes);
    }

    #[test]
    fn a_symlinked_sidecar_alias_of_media_is_replaced_without_changing_media() {
        let dir = tempfile::tempdir().unwrap();
        let media = dir.path().join("recording.mp4");
        let index = media.with_extension("idx");
        let (bytes, _, records) = recording(2);
        std::fs::write(&media, &bytes).unwrap();
        std::os::unix::fs::symlink(&media, &index).unwrap();

        recover_segment(&media).unwrap();
        assert_eq!(std::fs::read(&media).unwrap(), bytes);
        assert!(std::fs::symlink_metadata(&index).unwrap().is_file());
        assert_eq!(
            read_index_bounded(&index, MAX_PERSISTED_INDEX_RECORDS)
                .unwrap()
                .1
                .len(),
            records.len()
        );
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

        assert!(recover_segment_beneath(&approved, &media, IndexBinding::UNKNOWN).is_err());
        assert_eq!(std::fs::read(&victim).unwrap(), bytes);
    }

    #[test]
    fn oversized_sidecar_falls_back_to_media_and_is_rebuilt_within_the_limit() {
        let dir = tempfile::tempdir().unwrap();
        let media = dir.path().join("recording.mp4");
        let index = media.with_extension("idx");
        let (bytes, _, records) = recording(2);
        std::fs::write(&media, &bytes).unwrap();
        let sidecar = File::create(&index).unwrap();
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
