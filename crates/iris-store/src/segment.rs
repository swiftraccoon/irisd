//! One open recording file and its sidecar index.

use std::ffi::{OsStr, OsString};
use std::fs::{self, File, OpenOptions};
use std::io;
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use iris_core::time::{MediaTicks, WallUs};
use iris_mp4::fragment::{Fragment, FragmentHeader, encode_fragment};
use iris_mp4::index::{IndexHeader, IndexRecord, IndexWriter};
use rustix::fs::{AtFlags, Mode};
use rustix::io::Errno;
use tracing::warn;

use crate::fs::ApprovedRoot;
use crate::layout;
use crate::observer::{SegmentProgress, SegmentSealed};

/// Recordings and sidecars are created readable and writable by their owner
/// only; the umask can only narrow that further.
const PRIVATE_FILE_MODE: u32 = 0o600;

/// An open `.mp4` with its `.idx`, written strictly sequentially.
#[derive(Debug)]
pub(crate) struct SegmentFile {
    path: PathBuf,
    file: File,
    index: IndexWriter,
    directory: Option<Arc<File>>,
    end: u64,
    allocated_end: u64,
    preallocate_bytes: u64,
    dirty: bool,
    /// A complete media fragment must not be retried if its sidecar or the
    /// following reservation fails. The worker checks this before its next write.
    post_write_error: Option<io::Error>,
    /// When the next `fdatasync` is due; advances by the interval so the
    /// stream keeps its phase against the other streams.
    next_sync: Instant,
    opened_at: Instant,
    init_len: u32,
    fragments: u32,
    wall_end: WallUs,
    media_end: u64,
}

impl SegmentFile {
    /// Creates the file (and its directories), writes the init segment, and
    /// reserves space ahead of it. When `path` exists, a numbered sibling is
    /// used so that two runs starting in the same microsecond never collide.
    pub(crate) fn create(
        path: &Path,
        init: &[u8],
        header: IndexHeader,
        preallocate_bytes: u64,
        next_sync: Instant,
    ) -> io::Result<Self> {
        Self::create_with(path, init, header, preallocate_bytes, next_sync, |_| Ok(()))
    }

    fn create_with(
        path: &Path,
        init: &[u8],
        header: IndexHeader,
        preallocate_bytes: u64,
        next_sync: Instant,
        mut before: impl FnMut(CreateStep) -> io::Result<()>,
    ) -> io::Result<Self> {
        let init_len = u32::try_from(init.len()).map_err(io::Error::other)?;
        let parent = crate::fs::parent_dir(path);
        before(CreateStep::Directories)?;
        crate::fs::create_dir_all_durable(parent)?;
        let pair = claim_unique_pair(path, &mut before)?;
        Self::initialize_claimed(
            pair,
            init,
            header,
            preallocate_bytes,
            next_sync,
            init_len,
            &mut before,
        )
    }

    pub(crate) fn create_beneath(
        approved: &ApprovedRoot,
        root_directory: File,
        path: &Path,
        init: &[u8],
        header: IndexHeader,
        preallocate_bytes: u64,
        next_sync: Instant,
    ) -> io::Result<Self> {
        Self::create_beneath_with(
            approved,
            root_directory,
            path,
            init,
            header,
            preallocate_bytes,
            next_sync,
            |_| Ok(()),
        )
    }

    fn create_beneath_with(
        approved: &ApprovedRoot,
        root_directory: File,
        path: &Path,
        init: &[u8],
        header: IndexHeader,
        preallocate_bytes: u64,
        next_sync: Instant,
        mut before: impl FnMut(CreateStep) -> io::Result<()>,
    ) -> io::Result<Self> {
        let init_len = u32::try_from(init.len()).map_err(io::Error::other)?;
        before(CreateStep::Directories)?;
        let (directory, name) =
            crate::fs::create_recording_parent_beneath(approved, root_directory, path)?;
        let pair = claim_unique_pair_beneath(path, directory, name, &mut before)?;
        Self::initialize_claimed(
            pair,
            init,
            header,
            preallocate_bytes,
            next_sync,
            init_len,
            &mut before,
        )
    }

    fn initialize_claimed(
        pair: ClaimedPair,
        init: &[u8],
        header: IndexHeader,
        preallocate_bytes: u64,
        next_sync: Instant,
        init_len: u32,
        before: &mut impl FnMut(CreateStep) -> io::Result<()>,
    ) -> io::Result<Self> {
        let ClaimedPair {
            path,
            file,
            index_file,
            directory,
            mut guard,
        } = pair;
        let result = (|| {
            before(CreateStep::IndexHeader)?;
            let index = IndexWriter::from_claimed_file(index_file, &header)?;
            let mut segment = Self {
                path,
                file,
                index,
                directory,
                end: 0,
                allocated_end: 0,
                preallocate_bytes,
                dirty: true,
                post_write_error: None,
                next_sync,
                opened_at: Instant::now(),
                init_len,
                fragments: 0,
                wall_end: header.start_wall,
                media_end: 0,
            };
            before(CreateStep::InitWrite)?;
            crate::fs::pwritev_all(&segment.file, &[io::IoSlice::new(init)], 0)?;
            segment.end = init.len() as u64;
            segment.allocated_end = segment.end;
            before(CreateStep::Preallocate)?;
            segment.ensure_preallocated()?;
            before(CreateStep::MediaSync)?;
            segment.file.sync_data()?;
            before(CreateStep::IndexSync)?;
            segment.index.sync()?;
            before(CreateStep::DirectorySync)?;
            segment.sync_directory()?;
            Ok(segment)
        })();
        match result {
            Ok(segment) => {
                guard.disarm();
                Ok(segment)
            }
            Err(error) => {
                if let Err(cleanup) = guard.cleanup() {
                    warn!(%error, %cleanup, "segment initialization failed and its rollback failed");
                }
                Err(error)
            }
        }
    }

    /// Appends one fragment and its index record; returns the record once the
    /// complete media is written. Later maintenance errors surface at sync.
    pub(crate) fn write_fragment(
        &mut self,
        fragment: &Fragment,
        sequence: u32,
        flags: u8,
    ) -> io::Result<IndexRecord> {
        self.write_fragment_with(fragment, sequence, flags, Self::ensure_preallocated)
    }

    fn write_fragment_with(
        &mut self,
        fragment: &Fragment,
        sequence: u32,
        flags: u8,
        preallocate: impl FnOnce(&mut Self) -> io::Result<()>,
    ) -> io::Result<IndexRecord> {
        if let Some(error) = self.post_write_error.take() {
            return Err(error);
        }
        let header = FragmentHeader {
            sequence,
            decode_time: fragment.decode_time,
            wall: fragment.wall,
        };
        let encoded = encode_fragment(&header, &fragment.samples).map_err(io::Error::other)?;
        let record = IndexRecord {
            offset: self.end,
            decode_time: fragment.decode_time,
            wall: fragment.wall,
            len: u32::try_from(encoded.total_len()).map_err(io::Error::other)?,
            duration: u32::try_from(fragment.duration()).unwrap_or(u32::MAX),
            sample_count: u16::try_from(fragment.samples.len()).map_err(io::Error::other)?,
            flags,
            raw_rtp: fragment.raw_rtp,
        };
        // Finish fallible record construction before writing any media. After
        // pwritev_all succeeds, retrying this fragment would duplicate it.
        crate::fs::pwritev_all(&self.file, &encoded.io_slices(&fragment.samples), self.end)?;
        self.end += encoded.total_len();
        self.dirty = true;
        let duration = fragment.duration();
        self.fragments += 1;
        self.media_end = fragment.decode_time + duration;
        let duration_micros =
            MediaTicks::new(i64::try_from(duration).unwrap_or(i64::MAX)).as_micros();
        self.wall_end = fragment.wall.offset(duration_micros);
        self.post_write_error = self
            .index
            .append(&record)
            .and_then(|()| preallocate(self))
            .err();
        Ok(record)
    }

    /// What is durable right after a successful sync.
    pub(crate) fn progress(&self) -> SegmentProgress {
        SegmentProgress {
            path: self.path.clone(),
            durable_len: self.end,
            fragments: self.fragments,
            wall_end: self.wall_end,
            media_end: self.media_end,
        }
    }

    /// Length of the init segment at the start of the file.
    pub(crate) fn init_len(&self) -> u32 {
        self.init_len
    }

    /// `fdatasync`s data and index when the next sync is due and the file is
    /// dirty. The due time advances by `interval` whether or not a sync ran,
    /// so each stream keeps the phase it was given at creation; a sync that
    /// finishes after the next due time pushes it to now.
    pub(crate) fn maybe_sync(&mut self, now: Instant, interval: Duration) -> io::Result<bool> {
        if let Some(error) = self.post_write_error.take() {
            return Err(error);
        }
        if now < self.next_sync {
            return Ok(false);
        }
        self.next_sync = (self.next_sync + interval).max(now);
        if !self.dirty {
            return Ok(false);
        }
        self.file.sync_data()?;
        self.index.sync()?;
        self.dirty = false;
        Ok(true)
    }

    /// When the next sync is due.
    pub(crate) fn next_sync(&self) -> Instant {
        self.next_sync
    }

    /// Trims preallocated space, flushes everything, and closes the file.
    pub(crate) fn seal(self) -> io::Result<SegmentSealed> {
        if let Some(error) = self.post_write_error {
            return Err(error);
        }
        self.file.set_len(self.end)?;
        self.file.sync_all()?;
        self.index.sync()?;
        self.sync_directory()?;
        Ok(SegmentSealed {
            path: self.path,
            len: self.end,
            fragments: self.fragments,
            wall_end: self.wall_end,
            media_end: self.media_end,
        })
    }

    /// Bytes written so far, including the init segment.
    pub(crate) fn bytes_written(&self) -> u64 {
        self.end
    }

    /// Fragments appended so far; each has one sidecar record.
    pub(crate) fn fragments(&self) -> u32 {
        self.fragments
    }

    /// When the file was created.
    pub(crate) fn opened_at(&self) -> Instant {
        self.opened_at
    }

    /// The media file path.
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    fn ensure_preallocated(&mut self) -> io::Result<()> {
        if self.preallocate_bytes == 0 {
            return Ok(());
        }
        let remaining = self.allocated_end.saturating_sub(self.end);
        if remaining < self.preallocate_bytes / 4 {
            // A fragment larger than the reservation leaves the write position past
            // the reserved end; the next reservation starts at the position, not
            // behind it.
            let start = self.allocated_end.max(self.end);
            crate::fs::preallocate(&self.file, start, self.preallocate_bytes)?;
            self.allocated_end = start + self.preallocate_bytes;
        }
        Ok(())
    }

    fn sync_directory(&self) -> io::Result<()> {
        self.directory.as_ref().map_or_else(
            || crate::fs::fsync_dir(crate::fs::parent_dir(&self.path)),
            |directory| directory.sync_all(),
        )
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CreateStep {
    Directories,
    MediaClaim,
    IndexClaim,
    IndexHeader,
    InitWrite,
    Preallocate,
    MediaSync,
    IndexSync,
    DirectorySync,
}

struct ClaimedPair {
    path: PathBuf,
    file: File,
    index_file: File,
    directory: Option<Arc<File>>,
    guard: CreationGuard,
}

fn claim_unique_pair(
    path: &Path,
    before: &mut impl FnMut(CreateStep) -> io::Result<()>,
) -> io::Result<ClaimedPair> {
    let mut candidate = path.to_path_buf();
    for n in 0..=layout::MAX_NUMBERED_SIBLING {
        if n > 0 {
            candidate = layout::numbered_segment_path(path, n);
        }
        before(CreateStep::MediaClaim)?;
        let file = match OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(PRIVATE_FILE_MODE)
            .open(&candidate)
        {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        };
        let index_path = layout::index_path(&candidate);
        if let Err(error) = before(CreateStep::IndexClaim) {
            drop(file);
            if let Err(cleanup) = cleanup_paths(&candidate, None) {
                warn!(%error, %cleanup, "media claim rollback failed");
            }
            return Err(error);
        }
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(PRIVATE_FILE_MODE)
            .open(&index_path)
        {
            Ok(index_file) => {
                return Ok(ClaimedPair {
                    path: candidate.clone(),
                    file,
                    index_file,
                    directory: None,
                    guard: CreationGuard::new_paths(candidate.clone(), index_path),
                });
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                drop(file);
                cleanup_paths(&candidate, None)?;
            }
            Err(error) => {
                drop(file);
                if let Err(cleanup) = cleanup_paths(&candidate, None) {
                    warn!(%error, %cleanup, "media claim rollback failed");
                }
                return Err(error);
            }
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        format!("too many files named like {}", path.display()),
    ))
}

fn claim_unique_pair_beneath(
    path: &Path,
    directory: File,
    name: OsString,
    before: &mut impl FnMut(CreateStep) -> io::Result<()>,
) -> io::Result<ClaimedPair> {
    let directory = Arc::new(directory);
    let mut candidate = path.to_path_buf();
    let mut candidate_name = name;
    let create_mode = Mode::RUSR | Mode::WUSR;
    for n in 0..=layout::MAX_NUMBERED_SIBLING {
        if n > 0 {
            candidate = layout::numbered_segment_path(path, n);
            candidate
                .file_name()
                .expect("numbered candidate has a basename")
                .clone_into(&mut candidate_name);
        }
        before(CreateStep::MediaClaim)?;
        let file = match crate::fs::create_file_beneath(&directory, &candidate_name, create_mode) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        };
        let index_name = layout::index_path(Path::new(&candidate_name)).into_os_string();
        if let Err(error) = before(CreateStep::IndexClaim) {
            drop(file);
            if let Err(cleanup) = cleanup_beneath(&directory, &candidate_name, None) {
                warn!(%error, %cleanup, "media claim rollback failed");
            }
            return Err(error);
        }
        match crate::fs::create_file_beneath(&directory, &index_name, create_mode) {
            Ok(index_file) => {
                return Ok(ClaimedPair {
                    path: candidate.clone(),
                    file,
                    index_file,
                    directory: Some(Arc::clone(&directory)),
                    guard: CreationGuard::new_beneath(
                        Arc::clone(&directory),
                        candidate_name.clone(),
                        index_name,
                    ),
                });
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                drop(file);
                cleanup_beneath(&directory, &candidate_name, None)?;
            }
            Err(error) => {
                drop(file);
                if let Err(cleanup) = cleanup_beneath(&directory, &candidate_name, None) {
                    warn!(%error, %cleanup, "media claim rollback failed");
                }
                return Err(error);
            }
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        format!("too many files named like {}", path.display()),
    ))
}

enum CreationTarget {
    Paths {
        media: PathBuf,
        index: PathBuf,
    },
    Beneath {
        directory: Arc<File>,
        media: OsString,
        index: OsString,
    },
}

struct CreationGuard {
    target: CreationTarget,
    armed: bool,
}

impl CreationGuard {
    fn new_paths(media: PathBuf, index: PathBuf) -> Self {
        Self {
            target: CreationTarget::Paths { media, index },
            armed: true,
        }
    }

    fn new_beneath(directory: Arc<File>, media: OsString, index: OsString) -> Self {
        Self {
            target: CreationTarget::Beneath {
                directory,
                media,
                index,
            },
            armed: true,
        }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }

    fn cleanup(&mut self) -> io::Result<()> {
        if !self.armed {
            return Ok(());
        }
        self.cleanup_target()?;
        self.armed = false;
        Ok(())
    }

    fn cleanup_target(&self) -> io::Result<()> {
        match &self.target {
            CreationTarget::Paths { media, index } => cleanup_paths(media, Some(index)),
            CreationTarget::Beneath {
                directory,
                media,
                index,
            } => cleanup_beneath(directory, media, Some(index)),
        }
    }
}

impl Drop for CreationGuard {
    fn drop(&mut self) {
        if self.armed
            && let Err(error) = self.cleanup_target()
        {
            warn!(%error, "segment creation rollback failed");
        }
    }
}

fn cleanup_beneath(directory: &File, media: &OsStr, index: Option<&OsStr>) -> io::Result<()> {
    let mut first_error = None;
    for name in [Some(media), index].into_iter().flatten() {
        match rustix::fs::unlinkat(directory, name, AtFlags::empty()) {
            Ok(()) | Err(Errno::NOENT) => {}
            Err(error) => {
                first_error.get_or_insert(io::Error::from(error));
            }
        }
    }
    if let Err(error) = directory.sync_all() {
        first_error.get_or_insert(error);
    }
    first_error.map_or(Ok(()), Err)
}

fn cleanup_paths(media: &Path, index: Option<&Path>) -> io::Result<()> {
    let mut first_error = None;
    for path in [Some(media), index].into_iter().flatten() {
        match fs::remove_file(path) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => {
                first_error.get_or_insert(error);
            }
        }
    }
    if let Some(error) = first_error {
        return Err(error);
    }
    crate::fs::fsync_dir(crate::fs::parent_dir(media))
}

#[cfg(test)]
mod tests {
    use super::*;
    use iris_core::time::WallUs;

    fn header() -> IndexHeader {
        IndexHeader {
            start_wall: WallUs::new(1_700_000_000_000_000),
            ..IndexHeader::default()
        }
    }

    fn approved_root(directory: &tempfile::TempDir) -> (ApprovedRoot, PathBuf) {
        let root = directory.path().join("root");
        let approved =
            crate::fs::admit_recording_root(&root, crate::fs::RootExpectation::Fresh).unwrap();
        (approved, root)
    }

    #[test]
    fn initialization_failures_remove_only_the_claimed_pair() {
        let stages = [
            CreateStep::Directories,
            CreateStep::MediaClaim,
            CreateStep::IndexClaim,
            CreateStep::IndexHeader,
            CreateStep::InitWrite,
            CreateStep::Preallocate,
            CreateStep::MediaSync,
            CreateStep::IndexSync,
            CreateStep::DirectorySync,
        ];
        for (number, failed_stage) in stages.into_iter().enumerate() {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join(format!("tree-{number}/recording.mp4"));
            let result = SegmentFile::create_with(
                &path,
                b"init",
                header(),
                1 << 16,
                Instant::now() + Duration::from_secs(5),
                |stage| {
                    if stage == failed_stage {
                        Err(io::Error::other("injected initialization failure"))
                    } else {
                        Ok(())
                    }
                },
            );
            assert!(
                result.is_err(),
                "stage {failed_stage:?} unexpectedly succeeded"
            );
            assert!(!path.exists(), "stage {failed_stage:?} left media");
            assert!(
                !layout::index_path(&path).exists(),
                "stage {failed_stage:?} left an index"
            );

            let segment = SegmentFile::create(
                &path,
                b"init",
                header(),
                0,
                Instant::now() + Duration::from_secs(5),
            )
            .unwrap();
            assert_eq!(segment.path(), path);
            segment.seal().unwrap();
        }
    }

    #[test]
    fn approved_initialization_failures_remove_only_the_descriptor_claimed_pair() {
        let stages = [
            CreateStep::Directories,
            CreateStep::MediaClaim,
            CreateStep::IndexClaim,
            CreateStep::IndexHeader,
            CreateStep::InitWrite,
            CreateStep::Preallocate,
            CreateStep::MediaSync,
            CreateStep::IndexSync,
            CreateStep::DirectorySync,
        ];
        for (number, failed_stage) in stages.into_iter().enumerate() {
            let dir = tempfile::tempdir().unwrap();
            let (approved, root) = approved_root(&dir);
            let path = root.join(format!("cam{number}/main/2026-01-01/recording.mp4"));
            let result = SegmentFile::create_beneath_with(
                &approved,
                crate::fs::open_approved_root_directory(&approved).unwrap(),
                &path,
                b"init",
                header(),
                1 << 16,
                Instant::now() + Duration::from_secs(5),
                |stage| {
                    if stage == failed_stage {
                        Err(io::Error::other("injected initialization failure"))
                    } else {
                        Ok(())
                    }
                },
            );
            assert!(
                result.is_err(),
                "stage {failed_stage:?} unexpectedly succeeded"
            );
            assert!(!path.exists(), "stage {failed_stage:?} left media");
            assert!(
                !layout::index_path(&path).exists(),
                "stage {failed_stage:?} left an index"
            );

            let segment = SegmentFile::create_beneath(
                &approved,
                crate::fs::open_approved_root_directory(&approved).unwrap(),
                &path,
                b"init",
                header(),
                0,
                Instant::now() + Duration::from_secs(5),
            )
            .unwrap();
            assert_eq!(segment.path(), path);
            segment.seal().unwrap();
        }
    }

    #[cfg(unix)]
    #[test]
    fn approved_pair_claim_preserves_a_preexisting_sidecar_symlink_victim() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let (approved, root) = approved_root(&dir);
        let path = root.join("cam1/main/2026-01-01/recording.mp4");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let index = layout::index_path(&path);
        let victim = dir.path().join("victim.idx");
        fs::write(&victim, b"preserve victim bytes").unwrap();
        symlink(&victim, &index).unwrap();

        let segment = SegmentFile::create_beneath(
            &approved,
            crate::fs::open_approved_root_directory(&approved).unwrap(),
            &path,
            b"init",
            header(),
            0,
            Instant::now() + Duration::from_secs(5),
        )
        .unwrap();
        assert!(segment.path().ends_with("recording-1.mp4"));
        assert_eq!(fs::read_link(&index).unwrap(), victim);
        assert_eq!(fs::read(&victim).unwrap(), b"preserve victim bytes");
        segment.seal().unwrap();
    }

    #[test]
    fn initialization_barriers_precede_a_successful_return() {
        let dir = tempfile::tempdir().unwrap();
        let existing = dir.path().join("existing/recording.mp4");
        fs::create_dir_all(existing.parent().unwrap()).unwrap();
        let new_hierarchy = dir.path().join("new/camera/main/date/recording.mp4");
        for path in [existing, new_hierarchy] {
            let mut observed = Vec::new();
            let segment = SegmentFile::create_with(
                &path,
                b"init",
                header(),
                1 << 16,
                Instant::now() + Duration::from_secs(5),
                |stage| {
                    observed.push(stage);
                    Ok(())
                },
            )
            .unwrap();
            assert_eq!(
                observed,
                [
                    CreateStep::Directories,
                    CreateStep::MediaClaim,
                    CreateStep::IndexClaim,
                    CreateStep::IndexHeader,
                    CreateStep::InitWrite,
                    CreateStep::Preallocate,
                    CreateStep::MediaSync,
                    CreateStep::IndexSync,
                    CreateStep::DirectorySync,
                ]
            );
            assert!(segment.path().exists());
            assert!(layout::index_path(segment.path()).exists());
            segment.seal().unwrap();
        }
    }

    #[test]
    fn a_preexisting_index_is_preserved_and_the_pair_uses_a_suffix() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("recording.mp4");
        let index = layout::index_path(&path);
        fs::write(&index, b"preexisting sidecar").unwrap();
        let segment = SegmentFile::create(
            &path,
            b"init",
            header(),
            0,
            Instant::now() + Duration::from_secs(5),
        )
        .unwrap();
        assert_eq!(fs::read(&index).unwrap(), b"preexisting sidecar");
        assert!(segment.path().ends_with("recording-1.mp4"));
        segment.seal().unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn a_preexisting_index_symlink_is_never_followed_or_removed() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("recording.mp4");
        let index = layout::index_path(&path);
        let target = dir.path().join("target");
        fs::write(&target, b"preserve target").unwrap();
        symlink(&target, &index).unwrap();

        let segment = SegmentFile::create(
            &path,
            b"init",
            header(),
            0,
            Instant::now() + Duration::from_secs(5),
        )
        .unwrap();
        assert_eq!(fs::read_link(&index).unwrap(), target);
        assert_eq!(fs::read(&target).unwrap(), b"preserve target");
        assert!(segment.path().ends_with("recording-1.mp4"));
        segment.seal().unwrap();
    }

    #[cfg(target_family = "unix")]
    #[test]
    fn equal_length_recovery_releases_keep_size_reservation() {
        use std::os::unix::fs::MetadataExt as _;

        use bytes::Bytes;
        use iris_mp4::init::{SampleEntry, build_init_segment};
        use std::sync::Arc;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("reserved.mp4");
        let entry = Arc::new(SampleEntry::new(
            Bytes::from_static(b"\0\0\0\x10hvc1\0\0\0\0\0\0\0\0"),
            "hvc1".into(),
            16,
            16,
        ));
        let init = build_init_segment(&entry);
        let segment = SegmentFile::create(
            &path,
            &init,
            header(),
            8 << 20,
            Instant::now() + Duration::from_secs(5),
        )
        .unwrap();
        drop(segment); // simulate a process exit before seal

        let before = fs::metadata(&path).unwrap();
        let logical_blocks = before.len().div_ceil(512);
        if before.blocks() <= logical_blocks {
            eprintln!("skipping reservation release test: no KEEP_SIZE allocation accounting here");
            return;
        }
        let report = crate::recover::recover_segment(&path).unwrap();
        assert!(!report.truncated, "the logical prefix was already complete");
        let after = fs::metadata(&path).unwrap();
        assert_eq!(after.len(), before.len());
        assert!(
            after.blocks() < before.blocks(),
            "equal-length set_len retained the reservation: {} -> {} blocks",
            before.blocks(),
            after.blocks()
        );
    }

    #[test]
    fn completed_media_is_acknowledged_before_a_preallocation_failure() {
        use bytes::Bytes;
        use iris_mp4::fragment::Sample;
        use iris_mp4::init::{SampleEntry, build_init_segment};
        use iris_mp4::reader::{ScanOptions, scan};
        use std::sync::Arc;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("recording.mp4");
        let entry = Arc::new(SampleEntry::new(
            Bytes::from_static(b"\0\0\0\x10hvc1\0\0\0\0\0\0\0\0"),
            "hvc1".into(),
            16,
            16,
        ));
        let init = build_init_segment(&entry);
        let mut segment = SegmentFile::create(
            &path,
            &init,
            header(),
            0,
            Instant::now() + Duration::from_secs(30),
        )
        .unwrap();
        let fragment = Fragment {
            samples: vec![Sample {
                data: Bytes::from_static(&[0, 0, 0, 2, 0x26, 1]),
                duration: 3000,
                is_sync: true,
            }],
            decode_time: 0,
            wall: header().start_wall,
            raw_rtp: 0,
            new_run: true,
            sample_entry: entry,
        };
        let written = segment.write_fragment_with(&fragment, 1, 0, |_| {
            Err(io::Error::from_raw_os_error(Errno::NOSPC.raw_os_error()))
        });
        assert!(
            written.is_ok(),
            "complete media must not be retried: {written:?}"
        );
        let scan = scan(
            File::open(&path).unwrap(),
            ScanOptions {
                verify_crc: true,
                collect_samples: false,
            },
        )
        .unwrap();
        assert_eq!(scan.fragments.len(), 1);
        assert_eq!(scan.stop, None);
        assert!(
            segment
                .maybe_sync(Instant::now(), Duration::from_secs(1))
                .is_err(),
            "the maintenance error still retires the root before another write"
        );
    }

    #[test]
    fn preallocation_stays_ahead_of_fragments_larger_than_the_reservation() {
        use bytes::Bytes;
        use iris_mp4::fragment::Sample;
        use iris_mp4::init::{SampleEntry, build_init_segment};

        let dir = tempfile::tempdir().unwrap();
        let entry = Arc::new(SampleEntry::new(
            Bytes::from_static(b"\0\0\0\x10hvc1\0\0\0\0\0\0\0\0"),
            "hvc1".into(),
            16,
            16,
        ));
        let init = build_init_segment(&entry);
        let mut segment = SegmentFile::create(
            &dir.path().join("large.mp4"),
            &init,
            header(),
            4_096,
            Instant::now() + Duration::from_secs(30),
        )
        .unwrap();
        let sample = Bytes::from(vec![0u8; 16_384]);
        for sequence in 1..=3 {
            let fragment = Fragment {
                samples: vec![Sample {
                    data: sample.clone(),
                    duration: 3000,
                    is_sync: true,
                }],
                decode_time: u64::from(sequence) * 3000,
                wall: header().start_wall,
                raw_rtp: 0,
                new_run: sequence == 1,
                sample_entry: Arc::clone(&entry),
            };
            segment.write_fragment(&fragment, sequence, 0).unwrap();
            assert!(
                segment.allocated_end > segment.end,
                "fragment {sequence}: reservation ends at {} behind the write position {}",
                segment.allocated_end,
                segment.end
            );
        }
        segment.seal().unwrap();
    }

    #[test]
    fn syncs_keep_their_phase_and_skip_clean_files() {
        let dir = tempfile::tempdir().unwrap();
        let interval = Duration::from_secs(5);
        let phase = Duration::from_millis(1_250);
        let t0 = Instant::now();
        let mut segment = SegmentFile::create(
            &dir.path().join("a.mp4"),
            b"init",
            header(),
            1 << 16,
            t0 + phase,
        )
        .unwrap();
        // Not due before the phase, due at it; the file is dirty from creation.
        assert!(
            !segment
                .maybe_sync(
                    (t0 + phase).checked_sub(Duration::from_millis(1)).unwrap(),
                    interval
                )
                .unwrap()
        );
        assert!(segment.maybe_sync(t0 + phase, interval).unwrap());
        assert_eq!(segment.next_sync(), t0 + phase + interval);
        // Clean at the next due time: no sync, but the schedule still advances.
        assert!(!segment.maybe_sync(t0 + phase + interval, interval).unwrap());
        assert_eq!(segment.next_sync(), t0 + phase + 2 * interval);
        // Far behind (a slow disk): the next due time is now, not in the past.
        let late = t0 + phase + 10 * interval;
        assert!(!segment.maybe_sync(late, interval).unwrap());
        assert_eq!(segment.next_sync(), late);
    }
}
