//! File-system calls the store relies on, and the recording-root identity
//! model: durable markers, admission against the catalog's expectation, and
//! descriptor-relative opens and removals that never follow a link or cross a
//! mount.

use std::ffi::{OsStr, OsString};
use std::fs::{self, File};
use std::io::{self, IoSlice, Read, Write};
use std::mem::MaybeUninit;
use std::os::fd::OwnedFd;
use std::path::{Component, Path, PathBuf};

use rustix::fs::{AtFlags, CWD, FallocateFlags, FileType, Mode, OFlags, RawDir, ResolveFlags};
use rustix::io::Errno;

/// The identity types the recorder's crates share, re-exported for callers of
/// this crate's root admission.
pub use iris_core::id::{InvalidRootIdentity, RootIdentity};

/// Marker stored inside every admitted recording root.
pub const ROOT_IDENTITY_MARKER: &str = ".irisd-root-id";

/// Bytes read from a marker: the identity, one optional newline, and one more
/// byte to detect anything after it.
const ROOT_MARKER_READ_LIMIT: u64 = 34;

/// Device and inode of an admitted root directory. The identity holds for one
/// daemon run only: it rejects configured aliases and detects a physical
/// replacement, and is never persisted.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct PhysicalIdentity {
    device: u64,
    inode: u64,
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

    /// The device and inode admitted for this daemon run, compared to reject
    /// configured aliases and later physical replacement.
    #[must_use]
    pub const fn physical_identity(&self) -> PhysicalIdentity {
        PhysicalIdentity {
            device: self.device,
            inode: self.inode,
        }
    }
}

/// Whether a file-system call failed because the process lacked a resource
/// rather than because the file or root is unavailable: descriptors (`EMFILE`,
/// `ENFILE`), memory (`ENOMEM`), or a retryable call (`EAGAIN`, which is also
/// `EWOULDBLOCK`, and `EINTR`). Every other error, including `ENOENT`, `EIO`,
/// `ESTALE`, `ENOTDIR`, and an identity mismatch, means the target itself
/// failed.
pub(crate) fn is_transient_resource_error(error: &io::Error) -> bool {
    matches!(
        Errno::from_io_error(error),
        Some(Errno::MFILE | Errno::NFILE | Errno::NOMEM | Errno::AGAIN | Errno::INTR)
    ) || matches!(
        error.kind(),
        io::ErrorKind::OutOfMemory | io::ErrorKind::Interrupted | io::ErrorKind::WouldBlock
    )
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

/// Flushes a directory so that a created or truncated entry is durable. The
/// open requires a directory, so a FIFO or device planted at the path fails at
/// once instead of blocking the writer thread.
pub(crate) fn fsync_dir(path: &Path) -> io::Result<()> {
    let directory = rustix::fs::open(
        path,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
        Mode::empty(),
    )?;
    File::from(directory).sync_all()
}

/// Opens the regular file `name` beneath `directory`.
///
/// The open adds no-follow, close-on-exec, no-controlling-terminal, and
/// nonblocking flags to `access` and resolves beneath `directory` without
/// symlinks, magic links, or mount crossings, so a FIFO or device returns at
/// once instead of blocking the caller. Anything but a regular file is then
/// rejected with [`io::ErrorKind::InvalidData`] (`"<what> is not a regular
/// file"`), and the returned descriptor performs ordinary blocking I/O.
pub(crate) fn open_regular_beneath(
    directory: &File,
    name: &OsStr,
    access: OFlags,
    what: &str,
) -> io::Result<File> {
    let descriptor = rustix::fs::openat2(
        directory,
        name,
        access | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NOCTTY | OFlags::NONBLOCK,
        Mode::empty(),
        ResolveFlags::BENEATH
            | ResolveFlags::NO_SYMLINKS
            | ResolveFlags::NO_MAGICLINKS
            | ResolveFlags::NO_XDEV,
    )?;
    regular_file(descriptor, what)
}

/// Opens a regular file for reading without blocking on a FIFO or
/// device and without following a symlink in the final path component.
///
/// Directory components resolve normally, so this suits catalog paths and
/// offline tools; daemon opens beneath an [`ApprovedRoot`] use the rooted
/// helpers such as [`open_segment_beneath`].
///
/// # Errors
///
/// Returns the open error (`ELOOP` for a final symlink) or
/// [`io::ErrorKind::InvalidData`] when the entry is not a regular file.
pub fn open_regular_file(path: &Path) -> io::Result<File> {
    let descriptor = rustix::fs::openat(
        CWD,
        path,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NOCTTY | OFlags::NONBLOCK,
        Mode::empty(),
    )?;
    regular_file(descriptor, "file")
}

/// Accepts a nonblocking descriptor only when it names a regular file, then
/// restores blocking I/O on it.
fn regular_file(descriptor: OwnedFd, what: &str) -> io::Result<File> {
    let stat = rustix::fs::fstat(&descriptor)?;
    if FileType::from_raw_mode(stat.st_mode) != FileType::RegularFile {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{what} is not a regular file"),
        ));
    }
    let flags = rustix::fs::fcntl_getfl(&descriptor)?;
    rustix::fs::fcntl_setfl(&descriptor, flags.difference(OFlags::NONBLOCK))?;
    Ok(File::from(descriptor))
}

/// Creates a directory tree and makes each newly created entry durable before
/// returning. Previously present ancestors require no directory-entry barrier.
/// Created directories are mode 0700 whatever the process umask.
pub(crate) fn create_dir_all_durable(path: &Path) -> io::Result<()> {
    create_dir_all_durable_with(path, &mut fsync_dir)
}

fn create_private_dir(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::DirBuilderExt as _;

    fs::DirBuilder::new().mode(0o700).create(path)
}

fn create_dir_all_durable_with(
    path: &Path,
    sync_dir: &mut impl FnMut(&Path) -> io::Result<()>,
) -> io::Result<()> {
    if path.as_os_str().is_empty() || path == Path::new(".") {
        return Ok(());
    }
    match create_private_dir(path) {
        Ok(()) => {
            sync_dir(path)?;
            sync_dir(parent_dir(path))
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            create_dir_all_durable_with(parent_dir(path), sync_dir)?;
            match create_private_dir(path) {
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
///
/// The deepest ancestor that exists is opened without following any symlink, and
/// each missing component is created (mode 0700) and reopened relative to its
/// parent's descriptor without following links or crossing mounts. A symlink
/// anywhere in `path`, including one swapped in while creation runs, makes
/// creation fail instead of creating directories elsewhere.
///
/// # Errors
///
/// Returns the first open, creation, or sync error; directories created
/// before it remain.
pub fn create_recording_root(path: &Path) -> io::Result<()> {
    create_root_directory(path).map(drop)
}

/// Creates `path` as [`create_recording_root`] does and returns the
/// descriptor of the final directory, so admission never reopens it by name.
fn create_root_directory(path: &Path) -> io::Result<File> {
    create_root_directory_with(path, &mut |directory| directory.sync_all())
}

/// [`create_root_directory`] with the directory barrier injected, so tests
/// observe the child-then-parent order and a failed barrier.
fn create_root_directory_with(
    path: &Path,
    sync: &mut dyn FnMut(&File) -> io::Result<()>,
) -> io::Result<File> {
    let mut missing = Vec::new();
    let mut ancestor = path;
    let mut directory = loop {
        match open_root_nofollow(ancestor) {
            Ok(directory) => break directory,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                let name = ancestor.file_name().ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "recording root path has no creatable final component",
                    )
                })?;
                missing.push(name.to_owned());
                ancestor = parent_dir(ancestor);
            }
            Err(error) => return Err(error),
        }
    };
    for name in missing.iter().rev() {
        match rustix::fs::mkdirat(&directory, name, Mode::RWXU) {
            Ok(()) | Err(Errno::EXIST) => {}
            Err(error) => return Err(error.into()),
        }
        let child = open_directory_beneath(&directory, name)?;
        sync(&child)?;
        sync(&directory)?;
        directory = child;
    }
    Ok(directory)
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

/// Reads the identity marker of an open root. The marker must be a regular
/// file holding exactly 32 lowercase hexadecimal characters, optionally
/// followed by one line feed; the open never blocks on a FIFO or device.
fn read_root_identity(root: &File) -> io::Result<Option<RootIdentity>> {
    let marker = match open_regular_beneath(
        root,
        OsStr::new(ROOT_IDENTITY_MARKER),
        OFlags::RDONLY,
        "recording-root marker",
    ) {
        Ok(marker) => marker,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    let mut bytes = Vec::new();
    (&marker)
        .take(ROOT_MARKER_READ_LIMIT)
        .read_to_end(&mut bytes)?;
    let hex = bytes.strip_suffix(b"\n").unwrap_or(&bytes);
    let framing_error = || {
        let found = marker
            .metadata()
            .map_or(bytes.len() as u64, |metadata| metadata.len());
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "recording-root marker must contain exactly 32 lowercase hexadecimal characters, optionally followed by one newline; found {found} bytes"
            ),
        )
    };
    if hex.len() != RootIdentity::HEX_LEN {
        return Err(framing_error());
    }
    std::str::from_utf8(hex)
        .ok()
        .and_then(|text| text.parse().ok())
        .map(Some)
        .ok_or_else(framing_error)
}

fn create_root_identity(root: &File) -> io::Result<RootIdentity> {
    let mut random = [0u8; 16];
    getrandom::fill(&mut random).map_err(io::Error::other)?;
    let identity = RootIdentity::from_bytes(random);
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
/// pathname is absent or empty. `path` must hold no symbolic link in any
/// component: resolution refuses links everywhere, not only at the end.
///
/// # Errors
///
/// `NotFound` for a missing known or legacy-owned root, or a known root
/// without a marker; `InvalidData` for a marker that differs from the
/// catalog, a marker that is not a regular file of the documented framing, a
/// nonempty fresh root without a marker, or a legacy-owned root without a
/// marker; `ELOOP` for a symbolic link in any component of `path`; otherwise
/// the first open, creation, or sync error.
pub fn admit_recording_root(path: &Path, expectation: RootExpectation) -> io::Result<ApprovedRoot> {
    let may_initialize = expectation == RootExpectation::Fresh;
    let root = match open_root_nofollow(path) {
        Ok(root) => root,
        Err(error) if error.kind() == io::ErrorKind::NotFound && may_initialize => {
            create_root_directory(path)?
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
                "recording root predates root identities and owns catalog rows but has no identity marker; verify the intended mount and create .irisd-root-id before restart",
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

/// Reopens an approved root without following a symbolic link in any
/// component and verifies both its durable marker and physical inode.
///
/// # Errors
///
/// `NotFound` when the root is gone; `InvalidData` when its marker changed or
/// the directory was physically replaced since admission; `ELOOP` for a
/// symbolic link in any component of its path.
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
///
/// # Errors
///
/// As [`verify_approved_root`].
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
///
/// # Errors
///
/// The `statvfs` error, for example `NotFound` for a missing root.
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
///
/// # Errors
///
/// The errors of [`verify_approved_root`], then the `fstatvfs` error.
pub fn free_space_approved(root: &ApprovedRoot) -> io::Result<FreeSpace> {
    let directory = open_approved_root(root)?;
    let value = rustix::fs::fstatvfs(&directory)?;
    Ok(FreeSpace {
        total: value.f_blocks.saturating_mul(value.f_frsize),
        available: value.f_bavail.saturating_mul(value.f_frsize),
    })
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

/// Creates `name` exclusively beneath `directory` as a regular file open for
/// writing: nothing already at the path is opened, followed, or truncated.
pub(crate) fn create_file_beneath(directory: &File, name: &OsStr, mode: Mode) -> io::Result<File> {
    rustix::fs::openat2(
        directory,
        name,
        OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
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

/// Where a recording's layout walk beneath a freshly verified root ended.
enum LayoutParent {
    /// The parent directory of the pair and the media basename.
    Found(File, OsString),
    /// A layout directory is absent; this is the deepest ancestor that exists.
    Missing(File),
}

/// Like `recording_parent_beneath`, but reports a missing layout directory
/// beneath the verified root as [`LayoutParent::Missing`]. A missing or
/// replaced root remains an error.
fn recording_parent_or_missing(approved: &ApprovedRoot, path: &Path) -> io::Result<LayoutParent> {
    let mut components = recording_components(approved, path)?;
    let mut directory = open_approved_root(approved)?;
    for component in &components[..3] {
        directory = match open_directory_beneath(&directory, component) {
            Ok(child) => child,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Ok(LayoutParent::Missing(directory));
            }
            Err(error) => return Err(error),
        };
    }
    let name = components.swap_remove(3);
    Ok(LayoutParent::Found(directory, name))
}

pub(crate) fn create_recording_parent_beneath(
    approved: &ApprovedRoot,
    directory: File,
    path: &Path,
) -> io::Result<(File, OsString)> {
    create_recording_parent_beneath_with(approved, directory, path, &mut |directory| {
        directory.sync_all()
    })
}

/// [`create_recording_parent_beneath`] with the directory barrier injected,
/// so tests observe the child-then-parent order and a failed barrier.
fn create_recording_parent_beneath_with(
    approved: &ApprovedRoot,
    mut directory: File,
    path: &Path,
    sync: &mut dyn FnMut(&File) -> io::Result<()>,
) -> io::Result<(File, OsString)> {
    let components = recording_components(approved, path)?;
    for component in &components[..3] {
        directory = match open_directory_beneath(&directory, component) {
            Ok(child) => child,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                match rustix::fs::mkdirat(&directory, component, Mode::RWXU) {
                    Ok(()) | Err(Errno::EXIST) => {}
                    Err(error) => return Err(error.into()),
                }
                let child = open_directory_beneath(&directory, component)?;
                sync(&child)?;
                sync(&directory)?;
                child
            }
            Err(error) => return Err(error),
        };
    }
    Ok((directory, components[3].clone()))
}

/// Opens a recording read-write for recovery: its parent directory, the
/// media, and the media basename. `Ok(None)` means the verified root holds no
/// such recording because a layout directory or the media name is absent; a
/// missing or replaced root is an error.
pub(crate) fn open_recording_beneath(
    approved: &ApprovedRoot,
    path: &Path,
) -> io::Result<Option<(File, File, OsString)>> {
    let LayoutParent::Found(directory, name) = recording_parent_or_missing(approved, path)? else {
        return Ok(None);
    };
    match open_regular_beneath(&directory, &name, OFlags::RDWR, "recording path") {
        Ok(media) => Ok(Some((directory, media, name))),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

/// Opens one recording read-only through an approved root without following
/// layout symlinks or crossing nested mounts.
///
/// The open never blocks: a FIFO, socket, or device at the recording's name
/// is rejected with [`io::ErrorKind::InvalidData`].
///
/// # Errors
///
/// The errors of [`verify_approved_root`]; `InvalidInput` when `path` is not
/// beneath the root in the four-component layout; `NotFound` when a layout
/// directory or the recording is absent; `InvalidData` when the name holds
/// anything but a regular file; `ELOOP` or `EXDEV` when a symbolic link or a
/// mount lies on the way.
pub fn open_segment_beneath(approved: &ApprovedRoot, path: &Path) -> io::Result<File> {
    let (directory, name) = recording_parent_beneath(approved, path)?;
    open_regular_beneath(&directory, &name, OFlags::RDONLY, "recording path")
}

/// Opens a recording and its optional sidecar from one pinned no-follow parent
/// directory. A sidecar symlink or non-regular entry is rejected rather than
/// followed; neither open blocks on a FIFO or device.
///
/// # Errors
///
/// As [`open_segment_beneath`]; an absent sidecar is not an error.
pub fn open_segment_pair_beneath(
    approved: &ApprovedRoot,
    path: &Path,
) -> io::Result<(File, Option<File>)> {
    let (directory, media_name) = recording_parent_beneath(approved, path)?;
    let media = open_regular_beneath(&directory, &media_name, OFlags::RDONLY, "recording path")?;
    let index_name = crate::layout::index_path(Path::new(&media_name));
    let index = match open_regular_beneath(
        &directory,
        index_name.as_os_str(),
        OFlags::RDONLY,
        "recording sidecar",
    ) {
        Ok(file) => Some(file),
        Err(error) if error.kind() == io::ErrorKind::NotFound => None,
        Err(error) => return Err(error),
    };
    Ok((media, index))
}

/// Bytes held by one pair member, or zero when it is absent. A FIFO, socket,
/// device node, or symlink holds no recording bytes and is never opened or
/// followed: `unlinkat` removes the name itself and never follows a symlink,
/// so a planted special file or link cannot block or prevent removal, and a
/// link's target is left untouched. Directories are rejected.
fn existing_regular_len_at(directory: &File, name: &OsStr) -> io::Result<u64> {
    let kind = match rustix::fs::statat(directory, name, AtFlags::SYMLINK_NOFOLLOW) {
        Ok(stat) => FileType::from_raw_mode(stat.st_mode),
        Err(Errno::NOENT) => return Ok(0),
        Err(error) => return Err(error.into()),
    };
    if matches!(
        kind,
        FileType::Fifo
            | FileType::Socket
            | FileType::CharacterDevice
            | FileType::BlockDevice
            | FileType::Symlink
    ) {
        return Ok(0);
    }
    match open_regular_beneath(directory, name, OFlags::RDONLY, "recording pair member") {
        Ok(file) => Ok(file.metadata()?.len()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(0),
        Err(error) => Err(error),
    }
}

/// Deletes a recording pair through an approved root descriptor without
/// following any layout symlink. Validation and both unlinks use the same
/// pinned parent-directory handle.
///
/// Once the root has been reopened and verified, a missing layout directory
/// means both members are absent: the deepest ancestor that exists is synced and
/// the removal succeeds with zero bytes. A missing or replaced root is an
/// error. A special file or symlink in place of either member counts zero
/// bytes and is unlinked without being opened or followed, so a link's
/// target outside the pair is never touched. A directory in place of a
/// member is an error.
///
/// # Errors
///
/// The errors of [`verify_approved_root`]; `InvalidInput` when `mp4_path` is
/// not beneath the root in the four-component layout; a directory in place of
/// a member; the first unlink or directory sync error.
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
    let (directory, media_name) = match recording_parent_or_missing(approved, mp4_path)? {
        LayoutParent::Found(directory, media_name) => (directory, media_name),
        LayoutParent::Missing(ancestor) => {
            sync_directory(&ancestor)?;
            return Ok(0);
        }
    };
    let index_name = crate::layout::index_path(Path::new(&media_name)).into_os_string();
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::cmp::Ordering;
    use std::os::unix::fs::PermissionsExt as _;
    use std::sync::mpsc;
    use std::time::Duration;

    const IDENTITY: &str = "0123456789abcdef0123456789abcdef";

    fn mkfifo(path: &Path) {
        rustix::fs::mkfifoat(CWD, path, Mode::RUSR | Mode::WUSR).unwrap();
    }

    /// Runs `call` on a thread and fails unless it returns within two
    /// seconds. A call blocked in an open of `fifo` is released by opening the
    /// FIFO for writing, so a failing test never hangs.
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

    fn write_marker(root: &Path, contents: &[u8]) {
        let marker = root.join(ROOT_IDENTITY_MARKER);
        let _ = fs::remove_file(&marker);
        fs::write(marker, contents).unwrap();
    }

    fn assert_private(path: &Path) {
        let mode = fs::symlink_metadata(path).unwrap().permissions().mode();
        assert_eq!(mode & 0o077, 0, "{} has mode {mode:o}", path.display());
    }

    #[test]
    fn a_marker_with_one_trailing_newline_names_the_same_identity() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("root");
        fs::create_dir(&root).unwrap();
        write_marker(&root, IDENTITY.as_bytes());
        let bare = admit_recording_root(&root, RootExpectation::LegacyOwned).unwrap();
        write_marker(&root, format!("{IDENTITY}\n").as_bytes());
        let terminated = admit_recording_root(&root, RootExpectation::LegacyOwned).unwrap();
        assert_eq!(bare.identity(), terminated.identity());
        assert_eq!(bare.identity().to_string(), IDENTITY);

        let known = admit_recording_root(&root, RootExpectation::Known(bare.identity())).unwrap();
        verify_approved_root(&known).unwrap();
        free_space_approved(&known).unwrap();
    }

    #[test]
    fn markers_with_any_other_framing_are_rejected_with_their_length() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("root");
        fs::create_dir(&root).unwrap();
        for (contents, length) in [
            (format!("{IDENTITY}\r\n"), 34),
            (format!("{IDENTITY}\n\n"), 34),
            (format!("{IDENTITY}\nX"), 34),
            (format!("{IDENTITY}\n0123"), 37),
            (format!(" {IDENTITY}"), 33),
            (IDENTITY[1..].to_owned(), 31),
        ] {
            write_marker(&root, contents.as_bytes());
            let error = admit_recording_root(&root, RootExpectation::LegacyOwned).unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::InvalidData, "{contents:?}");
            assert!(
                error.to_string().contains(&format!("found {length} bytes")),
                "{contents:?}: {error}"
            );
        }
        write_marker(&root, IDENTITY.to_uppercase().as_bytes());
        assert_eq!(
            admit_recording_root(&root, RootExpectation::LegacyOwned)
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidData
        );
    }

    #[test]
    fn a_fifo_marker_fails_admission_and_probes_promptly() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("root");
        let approved = admit_recording_root(&root, RootExpectation::Fresh).unwrap();
        let marker = root.join(ROOT_IDENTITY_MARKER);
        fs::remove_file(&marker).unwrap();
        mkfifo(&marker);

        let probe = approved.clone();
        let error = within_deadline(&marker, move || verify_approved_root(&probe)).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("not a regular file"), "{error}");
        let probe = approved.clone();
        let error = within_deadline(&marker, move || free_space_approved(&probe)).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        let (path, identity) = (root.clone(), approved.identity());
        let error = within_deadline(&marker, move || {
            admit_recording_root(&path, RootExpectation::Known(identity))
        })
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn rooted_opens_reject_fifo_members_promptly() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("root");
        let approved = admit_recording_root(&root, RootExpectation::Fresh).unwrap();
        let media = root.join("cam1/main/2026-01-01/recording.mp4");
        let index = media.with_extension("idx");
        fs::create_dir_all(media.parent().unwrap()).unwrap();
        fs::write(&media, b"media").unwrap();
        mkfifo(&index);

        let (probe, path) = (approved.clone(), media.clone());
        let error = within_deadline(&index, move || {
            open_segment_pair_beneath(&probe, &path).map(drop)
        })
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(
            error
                .to_string()
                .contains("recording sidecar is not a regular file"),
            "{error}"
        );
        let (probe, path) = (approved.clone(), media.clone());
        within_deadline(&index, move || open_segment_beneath(&probe, &path)).unwrap();

        fs::remove_file(&media).unwrap();
        mkfifo(&media);
        for read_write in [false, true] {
            let (probe, path) = (approved.clone(), media.clone());
            let error = within_deadline(&media, move || {
                if read_write {
                    open_recording_beneath(&probe, &path).map(drop)
                } else {
                    open_segment_beneath(&probe, &path).map(drop)
                }
            })
            .unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::InvalidData);
            assert!(
                error
                    .to_string()
                    .contains("recording path is not a regular file"),
                "{error}"
            );
        }
    }

    #[test]
    fn the_regular_file_open_never_blocks_or_follows_a_final_symlink() {
        let dir = tempfile::tempdir().unwrap();
        let regular = dir.path().join("regular");
        fs::write(&regular, b"bytes").unwrap();
        let mut opened = open_regular_file(&regular).unwrap();
        let flags = rustix::fs::fcntl_getfl(&opened).unwrap();
        assert!(
            !flags.contains(OFlags::NONBLOCK),
            "blocking I/O is restored"
        );
        let mut contents = Vec::new();
        opened.read_to_end(&mut contents).unwrap();
        assert_eq!(contents, b"bytes");

        let fifo = dir.path().join("fifo");
        mkfifo(&fifo);
        let probe = fifo.clone();
        let error = within_deadline(&fifo, move || open_regular_file(&probe)).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);

        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&regular, &link).unwrap();
        assert_eq!(
            open_regular_file(&link).unwrap_err().raw_os_error(),
            Some(Errno::LOOP.raw_os_error())
        );
    }

    #[test]
    fn pair_removal_unlinks_special_members_without_opening_them() {
        for fifo_member in ["idx", "mp4"] {
            let dir = tempfile::tempdir().unwrap();
            let root = dir.path().join("root");
            let approved = admit_recording_root(&root, RootExpectation::Fresh).unwrap();
            let media = root.join("cam1/main/2026-01-01/recording.mp4");
            let index = media.with_extension("idx");
            fs::create_dir_all(media.parent().unwrap()).unwrap();
            fs::write(&media, b"media").unwrap();
            fs::write(&index, b"index!").unwrap();
            let fifo = media.with_extension(fifo_member);
            fs::remove_file(&fifo).unwrap();
            mkfifo(&fifo);

            let (probe, path) = (approved.clone(), media.clone());
            let removed =
                within_deadline(&fifo, move || remove_segment_beneath(&probe, &path)).unwrap();
            assert_eq!(removed, if fifo_member == "idx" { 5 } else { 6 });
            assert!(fs::symlink_metadata(&media).is_err());
            assert!(fs::symlink_metadata(&index).is_err());
        }
    }

    #[test]
    fn pair_removal_unlinks_symlinked_members_without_following_them() {
        for linked_member in ["idx", "mp4"] {
            let dir = tempfile::tempdir().unwrap();
            let root = dir.path().join("root");
            let approved = admit_recording_root(&root, RootExpectation::Fresh).unwrap();
            let media = root.join("cam1/main/2026-01-01/recording.mp4");
            let index = media.with_extension("idx");
            fs::create_dir_all(media.parent().unwrap()).unwrap();
            fs::write(&media, b"media").unwrap();
            fs::write(&index, b"index!").unwrap();
            let outside = dir.path().join(format!("outside.{linked_member}"));
            fs::write(&outside, b"outside target").unwrap();
            let link = media.with_extension(linked_member);
            fs::remove_file(&link).unwrap();
            std::os::unix::fs::symlink(&outside, &link).unwrap();

            let removed = remove_segment_beneath(&approved, &media).unwrap();
            assert_eq!(removed, if linked_member == "idx" { 5 } else { 6 });
            assert!(fs::symlink_metadata(&media).is_err());
            assert!(fs::symlink_metadata(&index).is_err());
            assert_eq!(fs::read(&outside).unwrap(), b"outside target");
        }

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("root");
        let approved = admit_recording_root(&root, RootExpectation::Fresh).unwrap();
        let media = root.join("cam1/main/2026-01-01/recording.mp4");
        fs::create_dir_all(media.with_extension("idx")).unwrap();
        fs::write(&media, b"media").unwrap();
        assert!(
            remove_segment_beneath(&approved, &media).is_err(),
            "a directory member is still rejected"
        );
        assert_eq!(fs::read(&media).unwrap(), b"media");
    }

    #[test]
    fn pair_removal_treats_a_missing_layout_directory_as_absent() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("root");
        let approved = admit_recording_root(&root, RootExpectation::Fresh).unwrap();
        fs::create_dir_all(root.join("cam1/main")).unwrap();
        for missing in [
            "cam1/main/2026-01-01/recording.mp4",
            "cam1/sub/2026-01-01/recording.mp4",
            "cam9/main/2026-01-01/recording.mp4",
        ] {
            assert_eq!(
                remove_segment_beneath(&approved, &root.join(missing)).unwrap(),
                0
            );
        }
    }

    #[test]
    fn pair_removal_fails_for_a_missing_root() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("root");
        let approved = admit_recording_root(&root, RootExpectation::Fresh).unwrap();
        fs::rename(&root, dir.path().join("unmounted")).unwrap();
        let error = remove_segment_beneath(&approved, &root.join("cam1/main/2026-01-01/a.mp4"))
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::NotFound);
    }

    #[test]
    fn created_roots_and_layout_directories_are_private() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("parent/root");
        create_recording_root(&root).unwrap();
        assert_private(&root);
        assert_private(&dir.path().join("parent"));

        let tree = dir.path().join("tree/camera/main");
        create_dir_all_durable(&tree).unwrap();
        for created in ["tree", "tree/camera", "tree/camera/main"] {
            assert_private(&dir.path().join(created));
        }

        let approved_path = dir.path().join("approved");
        let approved = admit_recording_root(&approved_path, RootExpectation::Fresh).unwrap();
        assert_private(&approved_path);
        assert_private(&approved_path.join(ROOT_IDENTITY_MARKER));
        let media = approved_path.join("cam1/main/2026-01-01/recording.mp4");
        create_recording_parent_beneath(
            &approved,
            open_approved_root_directory(&approved).unwrap(),
            &media,
        )
        .unwrap();
        for created in ["cam1", "cam1/main", "cam1/main/2026-01-01"] {
            assert_private(&approved_path.join(created));
        }
    }

    #[test]
    fn root_creation_through_a_symlinked_ancestor_fails_and_creates_nothing_outside() {
        let dir = tempfile::tempdir().unwrap();
        let outside = dir.path().join("outside");
        fs::create_dir(&outside).unwrap();
        let linked = dir.path().join("linked");
        std::os::unix::fs::symlink(&outside, &linked).unwrap();
        let root = linked.join("nested/root");

        assert!(create_recording_root(&root).is_err());
        assert!(admit_recording_root(&root, RootExpectation::Fresh).is_err());
        assert_eq!(fs::read_dir(&outside).unwrap().count(), 0);
    }

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

    /// The directory a descriptor refers to, for asserting barrier order.
    fn directory_path(file: &File) -> PathBuf {
        use std::os::fd::AsRawFd as _;
        fs::read_link(format!("/proc/self/fd/{}", file.as_raw_fd())).unwrap()
    }

    #[test]
    fn a_created_root_is_synced_through_each_parent_in_order() {
        let dir = tempfile::tempdir().unwrap();
        let base = fs::canonicalize(dir.path()).unwrap();
        let parent = base.join("parent");
        let root = parent.join("root");
        let mut synced = Vec::new();
        create_root_directory_with(&root, &mut |directory| {
            synced.push(directory_path(directory));
            Ok(())
        })
        .unwrap();
        assert_eq!(synced, [parent.clone(), base, root, parent]);
    }

    #[test]
    fn a_created_root_parent_barrier_failure_is_reported() {
        let dir = tempfile::tempdir().unwrap();
        let base = fs::canonicalize(dir.path()).unwrap();
        let root = base.join("root");
        let mut synced = Vec::new();
        let error = create_root_directory_with(&root, &mut |directory| {
            let path = directory_path(directory);
            synced.push(path.clone());
            if path == base {
                Err(io::Error::other("injected parent sync failure"))
            } else {
                Ok(())
            }
        })
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::Other);
        assert_eq!(synced, [root.clone(), base]);
        assert!(root.is_dir(), "creation preceded the failed parent barrier");
    }

    #[test]
    fn layout_directories_beneath_a_root_are_synced_through_each_parent_in_order() {
        let dir = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(dir.path()).unwrap();
        let approved = admit_recording_root(&root, RootExpectation::Fresh).unwrap();
        let directory = open_approved_root_directory(&approved).unwrap();
        let camera = root.join("cam1");
        let kind = camera.join("main");
        let day = kind.join("2026-01-01");
        let mut synced = Vec::new();
        let (parent, name) = create_recording_parent_beneath_with(
            &approved,
            directory,
            &day.join("recording.mp4"),
            &mut |directory| {
                synced.push(directory_path(directory));
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(directory_path(&parent), day);
        assert_eq!(name, "recording.mp4");
        assert_eq!(
            synced,
            [camera.clone(), root, kind.clone(), camera, day, kind]
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
        assert_eq!(remove_segment_beneath(&approved, &requested).unwrap(), 0);
        assert!(
            fs::symlink_metadata(&requested).is_err(),
            "the link itself is removed"
        );
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
                &open_regular_beneath(&directory, &name, flags, "recording path").unwrap_err(),
            );
        }
        assert_cross_device(
            &create_file_beneath(&directory, &name, Mode::RUSR | Mode::WUSR).unwrap_err(),
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
