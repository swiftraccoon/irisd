//! Thin wrappers over the file-system calls the store relies on.

use std::ffi::{OsStr, OsString};
use std::fmt;
use std::fs::{self, File};
use std::io::{self, IoSlice, Read, Write};
use std::mem::MaybeUninit;
use std::path::{Component, Path, PathBuf};
use std::str::FromStr;

use rustix::fs::{AtFlags, CWD, FallocateFlags, Mode, OFlags, RawDir, ResolveFlags};
use rustix::io::Errno;

/// Marker stored inside every admitted recording root.
pub const ROOT_IDENTITY_MARKER: &str = ".irisd-root-id";

/// Durable random identity of one recording root.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct RootIdentity([u8; 16]);

impl fmt::Display for RootIdentity {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        for byte in self.0 {
            write!(formatter, "{byte:02x}")?;
        }
        Ok(())
    }
}

impl FromStr for RootIdentity {
    type Err = io::Error;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        if value.len() != 32
            || !value
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "recording-root identity must be exactly 32 lowercase hexadecimal characters",
            ));
        }
        let mut identity = [0u8; 16];
        for (index, output) in identity.iter_mut().enumerate() {
            *output = u8::from_str_radix(&value[index * 2..index * 2 + 2], 16).map_err(|_| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "invalid recording-root identity",
                )
            })?;
        }
        Ok(Self(identity))
    }
}

/// A recording root admitted against its durable marker and physical inode.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ApprovedRoot {
    path: PathBuf,
    identity: RootIdentity,
    device: u64,
    inode: u64,
}

/// Catalog knowledge used while admitting a configured recording root.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RootExpectation {
    /// Catalog identity from a previous successful admission.
    Known(RootIdentity),
    /// A pre-identity catalog owns rows under this root. The root must already
    /// carry an operator-installed marker; absence is never initialized.
    LegacyOwned,
    /// No catalog state has ever been assigned to this root.
    Fresh,
}

impl ApprovedRoot {
    /// Configured spelling of this recording root.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Durable marker identity expected at this root.
    #[must_use]
    pub const fn identity(&self) -> RootIdentity {
        self.identity
    }

    /// Stable physical identity used to reject configured aliases.
    #[must_use]
    pub const fn physical_identity(&self) -> (u64, u64) {
        (self.device, self.inode)
    }
}

/// Returns the directory containing `path`; a basename is contained by the
/// current directory rather than the empty path returned by [`Path::parent`].
pub(crate) fn parent_dir(path: &Path) -> &Path {
    match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    }
}

/// Writes every byte of `bufs` at `offset`, looping over partial writes.
pub(crate) fn pwritev_all(file: &File, bufs: &[IoSlice<'_>], mut offset: u64) -> io::Result<()> {
    let mut owned: Vec<IoSlice<'_>> = bufs.to_vec();
    let mut remaining: &mut [IoSlice<'_>] = &mut owned;
    while !remaining.is_empty() {
        let written = match rustix::io::pwritev(file, remaining, offset) {
            Ok(0) => {
                return Err(io::Error::new(
                    io::ErrorKind::WriteZero,
                    "pwritev wrote zero bytes",
                ));
            }
            Ok(n) => n,
            Err(Errno::INTR) => continue,
            Err(e) => return Err(e.into()),
        };
        offset += written as u64;
        IoSlice::advance_slices(&mut remaining, written);
    }
    Ok(())
}

/// Reserves `len` bytes at `offset` without changing the file size. File
/// systems without `fallocate` support are tolerated: the write path works
/// without preallocation, only less contiguously.
pub(crate) fn preallocate(file: &File, offset: u64, len: u64) -> io::Result<()> {
    match rustix::fs::fallocate(file, FallocateFlags::KEEP_SIZE, offset, len) {
        Ok(()) | Err(Errno::OPNOTSUPP | Errno::NOSYS) => Ok(()),
        Err(e) => Err(e.into()),
    }
}

/// Flushes a directory so that a created or truncated entry is durable.
pub(crate) fn fsync_dir(path: &Path) -> io::Result<()> {
    File::open(path)?.sync_all()
}

/// Creates a directory tree and makes each newly created entry durable before
/// returning. Previously present ancestors require no directory-entry barrier.
pub(crate) fn create_dir_all_durable(path: &Path) -> io::Result<()> {
    create_dir_all_durable_with(path, &mut fsync_dir)
}

fn create_dir_all_durable_with(
    path: &Path,
    sync_dir: &mut impl FnMut(&Path) -> io::Result<()>,
) -> io::Result<()> {
    if path.as_os_str().is_empty() || path == Path::new(".") {
        return Ok(());
    }
    match fs::create_dir(path) {
        Ok(()) => {
            sync_dir(path)?;
            sync_dir(parent_dir(path))
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            create_dir_all_durable_with(parent_dir(path), sync_dir)?;
            match fs::create_dir(path) {
                Ok(()) => {
                    sync_dir(path)?;
                    sync_dir(parent_dir(path))
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists && path.is_dir() => {
                    Ok(())
                }
                Err(error) => Err(error),
            }
        }
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists && path.is_dir() => Ok(()),
        Err(error) => Err(error),
    }
}

/// Creates a configured recording root and makes every newly created
/// directory entry durable through its parent before returning.
pub fn create_recording_root(path: &Path) -> io::Result<()> {
    create_dir_all_durable(path)
}

fn open_root_nofollow(path: &Path) -> io::Result<File> {
    rustix::fs::openat2(
        CWD,
        path,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
        ResolveFlags::NO_SYMLINKS | ResolveFlags::NO_MAGICLINKS,
    )
    .map(File::from)
    .map_err(Into::into)
}

fn read_root_identity(root: &File) -> io::Result<Option<RootIdentity>> {
    let fd = match rustix::fs::openat(
        root,
        ROOT_IDENTITY_MARKER,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    ) {
        Ok(fd) => fd,
        Err(Errno::NOENT) => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let mut text = String::new();
    File::from(fd).take(33).read_to_string(&mut text)?;
    if text.len() != 32 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "recording-root marker must contain exactly 32 lowercase hexadecimal characters",
        ));
    }
    text.parse().map(Some)
}

fn create_root_identity(root: &File) -> io::Result<RootIdentity> {
    let mut random = [0u8; 16];
    getrandom::fill(&mut random).map_err(io::Error::other)?;
    let identity = RootIdentity(random);
    let fd = rustix::fs::openat(
        root,
        ROOT_IDENTITY_MARKER,
        OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::RUSR | Mode::WUSR,
    )?;
    let mut marker = File::from(fd);
    marker.write_all(identity.to_string().as_bytes())?;
    marker.sync_data()?;
    root.sync_all()?;
    Ok(identity)
}

fn root_is_empty(root: &File) -> io::Result<bool> {
    let mut buffer = [MaybeUninit::uninit(); 4096];
    let mut entries = RawDir::new(root, &mut buffer);
    while let Some(entry) = entries.next() {
        let entry = entry?;
        let name = entry.file_name().to_bytes();
        if name != b"." && name != b".." {
            return Ok(false);
        }
    }
    Ok(true)
}

fn approved_from_open(
    path: &Path,
    identity: RootIdentity,
    root: &File,
) -> io::Result<ApprovedRoot> {
    use std::os::unix::fs::MetadataExt as _;

    let metadata = root.metadata()?;
    Ok(ApprovedRoot {
        path: path.to_path_buf(),
        identity,
        device: metadata.dev(),
        inode: metadata.ino(),
    })
}

/// Admits a configured recording root against pre-thread catalog knowledge.
///
/// A known or legacy-owned root is never created or initialized. A fresh
/// absent or empty root receives a durable marker. A legacy-owned root
/// without an operator-installed marker fails closed even when its current
/// pathname is absent or empty.
pub fn admit_recording_root(path: &Path, expectation: RootExpectation) -> io::Result<ApprovedRoot> {
    let may_initialize = expectation == RootExpectation::Fresh;
    let root = match open_root_nofollow(path) {
        Ok(root) => root,
        Err(error) if error.kind() == io::ErrorKind::NotFound && may_initialize => {
            create_recording_root(path)?;
            open_root_nofollow(path)?
        }
        Err(error) => return Err(error),
    };
    let identity = match (expectation, read_root_identity(&root)?) {
        (RootExpectation::Known(expected), Some(found)) if found == expected => expected,
        (RootExpectation::Known(_), Some(_)) => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "recording-root identity differs from the catalog",
            ));
        }
        (RootExpectation::Known(_), None) => {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "known recording root has no identity marker",
            ));
        }
        (RootExpectation::LegacyOwned | RootExpectation::Fresh, Some(found)) => found,
        (RootExpectation::LegacyOwned, None) => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "legacy recording root owns catalog rows but has no identity marker; verify the intended mount and create .irisd-root-id before restart",
            ));
        }
        (RootExpectation::Fresh, None) if root_is_empty(&root)? => create_root_identity(&root)?,
        (RootExpectation::Fresh, None) => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "nonempty recording root has no identity marker; verify the intended mount and create .irisd-root-id before restart",
            ));
        }
    };
    let approved = approved_from_open(path, identity, &root)?;
    verify_approved_root(&approved)?;
    Ok(approved)
}

/// Reopens an approved root without following its final component and verifies
/// both its durable marker and physical inode.
pub fn verify_approved_root(approved: &ApprovedRoot) -> io::Result<()> {
    open_approved_root(approved).map(drop)
}

fn open_approved_root(approved: &ApprovedRoot) -> io::Result<File> {
    let root = open_root_nofollow(&approved.path)?;
    let reopened = approved_from_open(&approved.path, approved.identity, &root)?;
    if reopened.physical_identity() != approved.physical_identity() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "recording root was physically replaced after admission",
        ));
    }
    if read_root_identity(&root)? != Some(approved.identity) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "recording-root marker changed after admission",
        ));
    }
    Ok(root)
}

/// Opens and verifies an approved recording-root directory without following
/// any configured-path symlink. The returned descriptor pins the admitted
/// physical root for descriptor-relative discovery.
pub fn open_approved_root_directory(approved: &ApprovedRoot) -> io::Result<File> {
    open_approved_root(approved)
}

/// Total and available bytes of the file system holding a path.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FreeSpace {
    /// Size of the file system in bytes.
    pub total: u64,
    /// Bytes available to unprivileged writers.
    pub available: u64,
}

/// Reports the space of the file system holding `root` (`statvfs`).
pub fn free_space(root: &Path) -> io::Result<FreeSpace> {
    let v = rustix::fs::statvfs(root)?;
    Ok(FreeSpace {
        total: v.f_blocks.saturating_mul(v.f_frsize),
        available: v.f_bavail.saturating_mul(v.f_frsize),
    })
}

/// Reports free space through a reopened and identity-verified recording-root
/// descriptor. A replaced path is rejected before its filesystem can influence
/// an approved writer or retention decision.
pub fn free_space_approved(root: &ApprovedRoot) -> io::Result<FreeSpace> {
    let directory = open_approved_root(root)?;
    let value = rustix::fs::fstatvfs(&directory)?;
    Ok(FreeSpace {
        total: value.f_blocks.saturating_mul(value.f_frsize),
        available: value.f_bavail.saturating_mul(value.f_frsize),
    })
}

/// Deletes a recording (`.mp4` and its `.idx`), flushes the directory, and
/// returns the bytes found before deletion. Either member may already be
/// absent; success means both are absent and the containing directory is
/// durable.
pub fn remove_segment(mp4_path: &Path) -> io::Result<u64> {
    let index_path = crate::layout::index_path(mp4_path);
    let mp4_len = existing_len(mp4_path)?;
    let index_len = existing_len(&index_path)?;
    let mut first_error = None;
    for path in [mp4_path, index_path.as_path()] {
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
    fsync_dir(parent_dir(mp4_path))?;
    Ok(mp4_len + index_len)
}

fn recording_components(approved: &ApprovedRoot, path: &Path) -> io::Result<Vec<OsString>> {
    let relative = path.strip_prefix(approved.path()).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "recording path is outside its approved root",
        )
    })?;
    let components: Vec<OsString> = relative
        .components()
        .map(|component| match component {
            Component::Normal(value) => Ok(value.to_owned()),
            _ => Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "recording path contains a non-normal component",
            )),
        })
        .collect::<io::Result<_>>()?;
    if components.len() != 4 || Path::new(&components[3]).extension() != Some(OsStr::new("mp4")) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "recording path does not match the four-component .mp4 layout",
        ));
    }
    Ok(components)
}

fn open_directory_beneath(directory: &File, name: &OsStr) -> io::Result<File> {
    rustix::fs::openat2(
        directory,
        name,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
        ResolveFlags::BENEATH
            | ResolveFlags::NO_SYMLINKS
            | ResolveFlags::NO_MAGICLINKS
            | ResolveFlags::NO_XDEV,
    )
    .map(File::from)
    .map_err(Into::into)
}

pub(crate) fn open_file_beneath(
    directory: &File,
    name: &OsStr,
    flags: OFlags,
    mode: Mode,
) -> io::Result<File> {
    rustix::fs::openat2(
        directory,
        name,
        flags | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        mode,
        ResolveFlags::BENEATH
            | ResolveFlags::NO_SYMLINKS
            | ResolveFlags::NO_MAGICLINKS
            | ResolveFlags::NO_XDEV,
    )
    .map(File::from)
    .map_err(Into::into)
}

fn recording_parent_beneath(approved: &ApprovedRoot, path: &Path) -> io::Result<(File, OsString)> {
    let components = recording_components(approved, path)?;
    let mut directory = open_approved_root(approved)?;
    for component in &components[..3] {
        directory = open_directory_beneath(&directory, component)?;
    }
    Ok((directory, components[3].clone()))
}

pub(crate) fn create_recording_parent_beneath(
    approved: &ApprovedRoot,
    mut directory: File,
    path: &Path,
) -> io::Result<(File, OsString)> {
    let components = recording_components(approved, path)?;
    for component in &components[..3] {
        directory = match open_directory_beneath(&directory, component) {
            Ok(child) => child,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                match rustix::fs::mkdirat(
                    &directory,
                    component,
                    Mode::RWXU | Mode::RWXG | Mode::RWXO,
                ) {
                    Ok(()) | Err(Errno::EXIST) => {}
                    Err(error) => return Err(error.into()),
                }
                let child = open_directory_beneath(&directory, component)?;
                child.sync_all()?;
                directory.sync_all()?;
                child
            }
            Err(error) => return Err(error),
        };
    }
    Ok((directory, components[3].clone()))
}

pub(crate) fn open_recording_beneath(
    approved: &ApprovedRoot,
    path: &Path,
) -> io::Result<(File, File, OsString)> {
    let (directory, name) = recording_parent_beneath(approved, path)?;
    let media = open_file_beneath(&directory, &name, OFlags::RDWR, Mode::empty())?;
    if !media.metadata()?.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "recording path is not a regular file",
        ));
    }
    Ok((directory, media, name))
}

/// Opens one recording read-only through an approved root without following
/// layout symlinks or crossing nested mounts.
pub fn open_segment_beneath(approved: &ApprovedRoot, path: &Path) -> io::Result<File> {
    let (directory, name) = recording_parent_beneath(approved, path)?;
    let media = open_file_beneath(&directory, &name, OFlags::RDONLY, Mode::empty())?;
    if !media.metadata()?.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "recording path is not a regular file",
        ));
    }
    Ok(media)
}

/// Opens a recording and its optional sidecar from one pinned no-follow parent
/// directory. A sidecar symlink or non-regular entry is rejected rather than
/// followed.
pub fn open_segment_pair_beneath(
    approved: &ApprovedRoot,
    path: &Path,
) -> io::Result<(File, Option<File>)> {
    let (directory, media_name) = recording_parent_beneath(approved, path)?;
    let media = open_file_beneath(&directory, &media_name, OFlags::RDONLY, Mode::empty())?;
    if !media.metadata()?.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "recording path is not a regular file",
        ));
    }
    let index_name = Path::new(&media_name).with_extension("idx");
    let index = match open_file_beneath(
        &directory,
        index_name.as_os_str(),
        OFlags::RDONLY,
        Mode::empty(),
    ) {
        Ok(file) => {
            if !file.metadata()?.is_file() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "recording sidecar is not a regular file",
                ));
            }
            Some(file)
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => None,
        Err(error) => return Err(error),
    };
    Ok((media, index))
}

fn existing_regular_len_at(directory: &File, name: &OsStr) -> io::Result<u64> {
    let file = match open_file_beneath(directory, name, OFlags::RDONLY, Mode::empty()) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(0),
        Err(error) => return Err(error),
    };
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "recording pair member is not a regular file",
        ));
    }
    Ok(metadata.len())
}

/// Deletes a recording pair through an approved root descriptor without
/// following any layout symlink. Validation and both unlinks use the same
/// pinned parent-directory handle.
pub fn remove_segment_beneath(approved: &ApprovedRoot, mp4_path: &Path) -> io::Result<u64> {
    remove_segment_beneath_with(
        approved,
        mp4_path,
        |directory, name| {
            rustix::fs::unlinkat(directory, name, AtFlags::empty()).map_err(io::Error::from)
        },
        File::sync_all,
    )
}

fn remove_segment_beneath_with(
    approved: &ApprovedRoot,
    mp4_path: &Path,
    mut unlink: impl FnMut(&File, &OsStr) -> io::Result<()>,
    sync_directory: impl FnOnce(&File) -> io::Result<()>,
) -> io::Result<u64> {
    let (directory, media_name) = recording_parent_beneath(approved, mp4_path)?;
    let index_name = Path::new(&media_name)
        .with_extension("idx")
        .into_os_string();
    let media_len = existing_regular_len_at(&directory, &media_name)?;
    let index_len = existing_regular_len_at(&directory, &index_name)?;
    let mut first_error = None;
    for name in [&media_name, &index_name] {
        match unlink(&directory, name) {
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
    sync_directory(&directory)?;
    Ok(media_len + index_len)
}

fn existing_len(path: &Path) -> io::Result<u64> {
    match fs::metadata(path) {
        Ok(metadata) => Ok(metadata.len()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(0),
        Err(error) => Err(error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cmp::Ordering;

    fn assert_cross_device(error: &io::Error) {
        assert_eq!(
            error.raw_os_error(),
            Some(Errno::XDEV.raw_os_error()),
            "expected openat2 NO_XDEV rejection, got {error}"
        );
    }

    #[test]
    fn a_basename_is_contained_by_the_current_directory() {
        assert_eq!(parent_dir(Path::new("recording.mp4")), Path::new("."));
        assert_eq!(parent_dir(Path::new(".")), Path::new("."));
        assert_eq!(
            parent_dir(Path::new("camera/recording.mp4")),
            Path::new("camera")
        );
    }

    #[test]
    fn pair_removal_is_idempotent_when_either_member_is_already_missing() {
        for missing_media in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let media = dir.path().join("recording.mp4");
            let index = media.with_extension("idx");
            if !missing_media {
                fs::write(&media, b"media").unwrap();
            }
            if missing_media {
                fs::write(&index, b"index").unwrap();
            }
            assert_eq!(remove_segment(&media).unwrap(), 5);
            assert!(!media.exists());
            assert!(!index.exists());
            assert_eq!(remove_segment(&media).unwrap(), 0);
        }
    }

    #[test]
    fn a_new_recording_root_is_synced_through_each_parent_in_order() {
        let dir = tempfile::tempdir().unwrap();
        let parent = dir.path().join("parent");
        let root = parent.join("root");
        let mut synced = Vec::new();
        create_dir_all_durable_with(&root, &mut |path| {
            synced.push(path.to_path_buf());
            Ok(())
        })
        .unwrap();
        assert_eq!(
            synced,
            [parent.clone(), dir.path().to_path_buf(), root, parent]
        );
    }

    #[test]
    fn a_recording_root_parent_barrier_failure_is_reported() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("root");
        let mut synced = Vec::new();
        let error = create_dir_all_durable_with(&root, &mut |path| {
            synced.push(path.to_path_buf());
            if path == dir.path() {
                Err(io::Error::other("injected parent sync failure"))
            } else {
                Ok(())
            }
        })
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::Other);
        assert_eq!(synced, [root.clone(), dir.path().to_path_buf()]);
        assert!(root.is_dir(), "creation preceded the failed parent barrier");
    }

    #[test]
    fn an_index_unlink_failure_can_be_retried_after_media_is_gone() {
        let dir = tempfile::tempdir().unwrap();
        let media = dir.path().join("recording.mp4");
        let index = media.with_extension("idx");
        fs::write(&media, b"media").unwrap();
        fs::create_dir(&index).unwrap();
        remove_segment(&media).unwrap_err();
        assert!(!media.exists());
        assert!(index.is_dir());

        fs::remove_dir(&index).unwrap();
        fs::write(&index, b"index").unwrap();
        assert_eq!(remove_segment(&media).unwrap(), 5);
        assert!(!index.exists());
    }

    #[test]
    fn a_known_root_rejects_an_empty_replacement_and_accepts_the_original_return() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("root");
        let displaced = dir.path().join("displaced");
        let approved = admit_recording_root(&root, RootExpectation::Fresh).unwrap();
        fs::rename(&root, &displaced).unwrap();
        fs::create_dir(&root).unwrap();

        assert!(admit_recording_root(&root, RootExpectation::Known(approved.identity())).is_err());
        assert!(root_is_empty(&File::open(&root).unwrap()).unwrap());

        fs::remove_dir(&root).unwrap();
        fs::rename(&displaced, &root).unwrap();
        let returned =
            admit_recording_root(&root, RootExpectation::Known(approved.identity())).unwrap();
        assert_eq!(returned.identity(), approved.identity());
    }

    #[test]
    fn a_legacy_owned_root_is_never_created_or_initialized() {
        let dir = tempfile::tempdir().unwrap();
        let absent = dir.path().join("absent");
        assert!(admit_recording_root(&absent, RootExpectation::LegacyOwned).is_err());
        assert!(!absent.exists());

        let empty = dir.path().join("empty");
        fs::create_dir(&empty).unwrap();
        assert!(admit_recording_root(&empty, RootExpectation::LegacyOwned).is_err());
        assert!(root_is_empty(&File::open(&empty).unwrap()).unwrap());
    }

    #[test]
    fn root_admission_rejects_symlinks_in_any_configured_path_component() {
        let dir = tempfile::tempdir().unwrap();
        let real_parent = dir.path().join("real-parent");
        fs::create_dir(&real_parent).unwrap();
        let real_root = real_parent.join("root");
        fs::create_dir(&real_root).unwrap();
        let linked_parent = dir.path().join("linked-parent");
        std::os::unix::fs::symlink(&real_parent, &linked_parent).unwrap();
        assert!(admit_recording_root(&linked_parent.join("root"), RootExpectation::Fresh).is_err());

        let linked_root = dir.path().join("linked-root");
        std::os::unix::fs::symlink(&real_root, &linked_root).unwrap();
        assert!(admit_recording_root(&linked_root, RootExpectation::Fresh).is_err());
    }

    #[test]
    fn rooted_removal_never_follows_a_symlinked_layout_component() {
        for symlink_depth in 0..3 {
            let dir = tempfile::tempdir().unwrap();
            let root = dir.path().join("root");
            let approved = admit_recording_root(&root, RootExpectation::Fresh).unwrap();
            let victim_root = dir.path().join("victim");
            let components = ["cam1", "main", "2026-01-01"];
            let mut beneath = root.clone();
            let mut victim = victim_root.clone();
            for (depth, component) in components.iter().enumerate() {
                match depth.cmp(&symlink_depth) {
                    Ordering::Less => {
                        beneath.push(component);
                        fs::create_dir(&beneath).unwrap();
                    }
                    Ordering::Equal => {
                        victim.push(component);
                        fs::create_dir_all(&victim).unwrap();
                        std::os::unix::fs::symlink(&victim, beneath.join(component)).unwrap();
                    }
                    Ordering::Greater => {
                        victim.push(component);
                        fs::create_dir(&victim).unwrap();
                    }
                }
            }
            let victim_file = victim.join("recording.mp4");
            fs::write(&victim_file, b"must survive").unwrap();
            let requested = root.join("cam1/main/2026-01-01/recording.mp4");
            assert!(remove_segment_beneath(&approved, &requested).is_err());
            assert_eq!(fs::read(&victim_file).unwrap(), b"must survive");
        }

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("root");
        let approved = admit_recording_root(&root, RootExpectation::Fresh).unwrap();
        let requested = root.join("cam1/main/2026-01-01/recording.mp4");
        fs::create_dir_all(requested.parent().unwrap()).unwrap();
        let victim = dir.path().join("victim.mp4");
        fs::write(&victim, b"final-component victim").unwrap();
        std::os::unix::fs::symlink(&victim, &requested).unwrap();
        assert!(open_segment_pair_beneath(&approved, &requested).is_err());
        assert!(remove_segment_beneath(&approved, &requested).is_err());
        assert_eq!(fs::read(&victim).unwrap(), b"final-component victim");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn rooted_final_opens_reject_a_bind_mount() {
        use std::process::Command;

        const TEST_NAME: &str = "fs::tests::rooted_final_opens_reject_a_bind_mount";
        const CHILD: &str = "IRIS_STORE_BIND_MOUNT_TEST_CHILD";

        if std::env::var_os(CHILD).is_none() {
            let probe = Command::new("unshare").args(["-Urm", "true"]).status();
            if !probe.is_ok_and(|status| status.success()) {
                eprintln!("skipping bind-mount contract test: user mount namespaces unavailable");
                return;
            }
            let status = Command::new("unshare")
                .arg("-Urm")
                .arg(std::env::current_exe().unwrap())
                .args(["--exact", TEST_NAME, "--nocapture"])
                .env(CHILD, "1")
                .status()
                .unwrap();
            assert!(status.success(), "bind-mount child test failed");
            return;
        }

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("root");
        let approved = admit_recording_root(&root, RootExpectation::Fresh).unwrap();
        let requested = root.join("cam1/main/2026-01-01/recording.mp4");
        fs::create_dir_all(requested.parent().unwrap()).unwrap();
        fs::write(&requested, b"mount target").unwrap();
        let victim = dir.path().join("victim.mp4");
        fs::write(&victim, b"bind-mounted victim").unwrap();
        let status = Command::new("mount")
            .arg("--bind")
            .arg(&victim)
            .arg(&requested)
            .status()
            .unwrap();
        assert!(status.success(), "synthetic file bind mount failed");

        let (directory, name) = recording_parent_beneath(&approved, &requested).unwrap();
        for flags in [OFlags::RDONLY, OFlags::RDWR] {
            assert_cross_device(
                &open_file_beneath(&directory, &name, flags, Mode::empty()).unwrap_err(),
            );
        }
        assert_cross_device(
            &open_file_beneath(
                &directory,
                &name,
                OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL,
                Mode::RUSR | Mode::WUSR,
            )
            .unwrap_err(),
        );
        assert_cross_device(&existing_regular_len_at(&directory, &name).unwrap_err());
        assert_cross_device(&open_segment_beneath(&approved, &requested).unwrap_err());
        assert_cross_device(&open_segment_pair_beneath(&approved, &requested).unwrap_err());
        assert_cross_device(&remove_segment_beneath(&approved, &requested).unwrap_err());
        assert_eq!(fs::read(&victim).unwrap(), b"bind-mounted victim");

        let status = Command::new("umount").arg(&requested).status().unwrap();
        assert!(status.success(), "synthetic file bind unmount failed");
    }

    #[test]
    fn separately_spelled_roots_expose_the_same_physical_identity() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("root");
        let first = admit_recording_root(&root, RootExpectation::Fresh).unwrap();
        let alias = root.join("..").join("root");
        let second =
            admit_recording_root(&alias, RootExpectation::Known(first.identity())).unwrap();
        assert_ne!(first.path(), second.path());
        assert_eq!(first.physical_identity(), second.physical_identity());
    }

    #[test]
    fn rooted_removal_rejects_a_physical_root_swap_even_with_a_copied_marker() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("root");
        let displaced = dir.path().join("displaced");
        let approved = admit_recording_root(&root, RootExpectation::Fresh).unwrap();
        fs::rename(&root, &displaced).unwrap();
        let victim = root.join("cam1/main/2026-01-01/recording.mp4");
        fs::create_dir_all(victim.parent().unwrap()).unwrap();
        fs::copy(
            displaced.join(ROOT_IDENTITY_MARKER),
            root.join(ROOT_IDENTITY_MARKER),
        )
        .unwrap();
        fs::write(&victim, b"replacement bytes").unwrap();

        assert!(remove_segment_beneath(&approved, &victim).is_err());
        assert_eq!(fs::read(&victim).unwrap(), b"replacement bytes");
    }

    #[test]
    fn rooted_pair_removal_retries_first_second_and_directory_barrier_failures() {
        enum Fault {
            FirstUnlink,
            SecondUnlink,
            DirectorySync,
        }

        for fault in [
            Fault::FirstUnlink,
            Fault::SecondUnlink,
            Fault::DirectorySync,
        ] {
            let dir = tempfile::tempdir().unwrap();
            let root = dir.path().join("root");
            let approved = admit_recording_root(&root, RootExpectation::Fresh).unwrap();
            let media = root.join("cam1/main/2026-01-01/recording.mp4");
            let index = media.with_extension("idx");
            fs::create_dir_all(media.parent().unwrap()).unwrap();
            fs::write(&media, b"media").unwrap();
            fs::write(&index, b"index").unwrap();
            let mut unlink_call = 0usize;
            let inject_first = matches!(fault, Fault::FirstUnlink);
            let inject_second = matches!(fault, Fault::SecondUnlink);
            let inject_sync = matches!(fault, Fault::DirectorySync);
            let result = remove_segment_beneath_with(
                &approved,
                &media,
                |directory, name| {
                    let call = unlink_call;
                    unlink_call += 1;
                    if (call == 0 && inject_first) || (call == 1 && inject_second) {
                        Err(io::Error::other("injected unlink failure"))
                    } else {
                        rustix::fs::unlinkat(directory, name, AtFlags::empty())
                            .map_err(io::Error::from)
                    }
                },
                |directory| {
                    if inject_sync {
                        Err(io::Error::other("injected directory sync failure"))
                    } else {
                        directory.sync_all()
                    }
                },
            );
            assert!(result.is_err());

            let remaining = u64::from(media.exists()) * 5 + u64::from(index.exists()) * 5;
            assert_eq!(
                remove_segment_beneath(&approved, &media).unwrap(),
                remaining
            );
            assert!(!media.exists());
            assert!(!index.exists());
            assert_eq!(remove_segment_beneath(&approved, &media).unwrap(), 0);
        }
    }

    #[test]
    fn rooted_removal_stays_on_the_pinned_parent_after_its_entry_is_swapped() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("root");
        let approved = admit_recording_root(&root, RootExpectation::Fresh).unwrap();
        let date = root.join("cam1/main/2026-01-01");
        let displaced = root.join("cam1/main/pinned-parent");
        let media = date.join("recording.mp4");
        let index = media.with_extension("idx");
        fs::create_dir_all(&date).unwrap();
        fs::write(&media, b"authorized media").unwrap();
        fs::write(&index, b"authorized index").unwrap();

        let mut swapped = false;
        assert_eq!(
            remove_segment_beneath_with(
                &approved,
                &media,
                |directory, name| {
                    if !swapped {
                        fs::rename(&date, &displaced)?;
                        fs::create_dir(&date)?;
                        fs::write(date.join("recording.mp4"), b"replacement media")?;
                        fs::write(date.join("recording.idx"), b"replacement index")?;
                        swapped = true;
                    }
                    rustix::fs::unlinkat(directory, name, AtFlags::empty()).map_err(io::Error::from)
                },
                File::sync_all,
            )
            .unwrap(),
            32
        );
        assert!(!displaced.join("recording.mp4").exists());
        assert!(!displaced.join("recording.idx").exists());
        assert_eq!(fs::read(&media).unwrap(), b"replacement media");
        assert_eq!(fs::read(&index).unwrap(), b"replacement index");
    }
}
