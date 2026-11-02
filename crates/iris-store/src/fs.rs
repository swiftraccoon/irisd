//! Thin wrappers over the file-system calls the store relies on.

use std::fs::{self, File};
use std::io::{self, IoSlice};
use std::path::Path;

use rustix::fs::FallocateFlags;
use rustix::io::Errno;

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

/// Deletes a recording (`.mp4` and its `.idx`), flushes the directory, and
/// returns the bytes freed. A missing `.mp4` is an error; a missing `.idx` is not.
pub fn remove_segment(mp4_path: &Path) -> io::Result<u64> {
    let mp4_len = fs::metadata(mp4_path)?.len();
    let index_path = crate::layout::index_path(mp4_path);
    let index_len = fs::metadata(&index_path).map_or(0, |m| m.len());
    fs::remove_file(mp4_path)?;
    match fs::remove_file(&index_path) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    if let Some(parent) = mp4_path.parent() {
        fsync_dir(parent)?;
    }
    Ok(mp4_len + index_len)
}
