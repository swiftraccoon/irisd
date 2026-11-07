//! Startup permission checks for the files that hold the daemon's secrets and
//! state: the configuration file, the state directory, and the catalog's
//! database, write-ahead log, and shared-memory files.
//!
//! SQLite gives a new write-ahead log and shared-memory file the database
//! file's mode, so a catalog once created readable by other users stays that
//! way across restarts even under a restrictive umask. The daemon therefore
//! checks the state directory and the catalog files before SQLite opens them:
//! each must belong to the service account, the catalog files must be regular
//! files with one link, and group or other permissions are removed with a
//! warning. A wrong owner, type, or link count fails startup.

use std::fs::{File, Permissions};
use std::io;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

use anyhow::{Context, bail};
use rustix::fs::{Mode, OFlags};
use rustix::io::Errno;

/// File name of the catalog database inside the state directory.
pub const CATALOG_FILE: &str = "irisd.db";
/// SQLite's companions of the catalog database.
const CATALOG_COMPANIONS: [&str; 2] = ["irisd.db-wal", "irisd.db-shm"];
/// Mode of the state directory: the service account only.
const PRIVATE_DIRECTORY: u32 = 0o700;
/// Mode of a catalog file: read and write for the service account only.
const PRIVATE_FILE: u32 = 0o600;

/// One permission change made by [`secure_state_directory`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModeRepair {
    /// The directory or file whose mode changed.
    pub path: PathBuf,
    /// Permission bits before the repair.
    pub old_mode: u32,
    /// Permission bits after the repair.
    pub new_mode: u32,
}

/// Checks the state directory and the catalog files in it before the catalog
/// opens, and returns the modes it tightened.
///
/// The directory is opened without following a final symbolic link and must be
/// owned by the effective user; group or other bits are removed (mode 0700).
/// Each of `irisd.db`, `irisd.db-wal`, and `irisd.db-shm` that exists is opened
/// without following a symbolic link or blocking on a special file, must be a
/// regular file with one link owned by the effective user, and gets mode 0600
/// when group or other bits are set.
pub fn secure_state_directory(dir: &Path) -> anyhow::Result<Vec<ModeRepair>> {
    secure_state_directory_for(dir, rustix::process::geteuid().as_raw())
}

fn secure_state_directory_for(dir: &Path, owner: u32) -> anyhow::Result<Vec<ModeRepair>> {
    let directory = rustix::fs::open(
        dir,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map(File::from)
    .map_err(io::Error::from)
    .with_context(|| {
        format!(
            "opening the state directory {} without following a symbolic link",
            dir.display()
        )
    })?;
    let mut repairs = Vec::new();
    check_owner(&directory, dir, owner)?;
    tighten(&directory, dir, PRIVATE_DIRECTORY, &mut repairs)?;
    for name in std::iter::once(CATALOG_FILE).chain(CATALOG_COMPANIONS) {
        let path = dir.join(name);
        let file = match rustix::fs::openat(
            &directory,
            name,
            OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::NOCTTY | OFlags::CLOEXEC,
            Mode::empty(),
        ) {
            Ok(file) => File::from(file),
            Err(Errno::NOENT) => continue,
            Err(Errno::LOOP) => bail!(
                "{} is a symbolic link; the catalog must be a regular file in the state directory",
                path.display()
            ),
            Err(error) => {
                return Err(io::Error::from(error)).with_context(|| {
                    format!("opening {} to check its permissions", path.display())
                });
            }
        };
        let metadata = file
            .metadata()
            .with_context(|| format!("reading the status of {}", path.display()))?;
        if !metadata.file_type().is_file() {
            bail!("{} is not a regular file", path.display());
        }
        if metadata.nlink() != 1 {
            bail!(
                "{} has {} hard links; the catalog must not be reachable through another name",
                path.display(),
                metadata.nlink()
            );
        }
        check_owner(&file, &path, owner)?;
        tighten(&file, &path, PRIVATE_FILE, &mut repairs)?;
    }
    Ok(repairs)
}

fn check_owner(file: &File, path: &Path, owner: u32) -> anyhow::Result<()> {
    let uid = file
        .metadata()
        .with_context(|| format!("reading the status of {}", path.display()))?
        .uid();
    if uid != owner {
        bail!(
            "{} is owned by uid {uid}, not by the daemon's uid {owner}",
            path.display()
        );
    }
    Ok(())
}

/// Sets `private` when group or other permission bits are present.
fn tighten(
    file: &File,
    path: &Path,
    private: u32,
    repairs: &mut Vec<ModeRepair>,
) -> anyhow::Result<()> {
    let old_mode = file
        .metadata()
        .with_context(|| format!("reading the status of {}", path.display()))?
        .mode()
        & 0o7777;
    let group_and_other = old_mode & 0o077;
    if group_and_other == 0 {
        return Ok(());
    }
    file.set_permissions(Permissions::from_mode(private))
        .with_context(|| format!("restricting {} to its owner", path.display()))?;
    repairs.push(ModeRepair {
        path: path.to_path_buf(),
        old_mode,
        new_mode: private,
    });
    Ok(())
}

/// Permission bits of the configuration file when users other than the
/// daemon's account or group can read or write it; `None` when it is private
/// or its status cannot be read.
///
/// The file holds camera passwords and API tokens. Group access is accepted
/// only for the daemon's own group, which matches the packaged layout
/// (mode 0640, owner root, group irisd).
pub fn exposed_configuration(path: &Path) -> Option<u32> {
    let metadata = std::fs::metadata(path).ok()?;
    let mode = metadata.mode() & 0o7777;
    configuration_exposed(mode, metadata.gid(), rustix::process::getegid().as_raw()).then_some(mode)
}

fn configuration_exposed(mode: u32, file_group: u32, daemon_group: u32) -> bool {
    mode & 0o006 != 0 || (mode & 0o060 != 0 && file_group != daemon_group)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mode(path: &Path) -> u32 {
        std::fs::symlink_metadata(path).unwrap().mode() & 0o7777
    }

    fn set_mode(path: &Path, mode: u32) {
        std::fs::set_permissions(path, Permissions::from_mode(mode)).unwrap();
    }

    fn euid() -> u32 {
        rustix::process::geteuid().as_raw()
    }

    /// A state directory holding the three catalog files, all shared.
    fn shared_state() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let state = dir.path().join("state");
        std::fs::create_dir(&state).unwrap();
        set_mode(&state, 0o755);
        for name in std::iter::once(CATALOG_FILE).chain(CATALOG_COMPANIONS) {
            let path = state.join(name);
            std::fs::write(&path, b"catalog bytes").unwrap();
            set_mode(&path, 0o644);
        }
        (dir, state)
    }

    #[test]
    fn a_shared_state_directory_and_catalog_are_restricted_to_the_owner() {
        let (_dir, state) = shared_state();
        let repairs = secure_state_directory_for(&state, euid()).unwrap();
        assert_eq!(repairs.len(), 4, "{repairs:?}");
        assert_eq!(
            repairs[0],
            ModeRepair {
                path: state.clone(),
                old_mode: 0o755,
                new_mode: 0o700,
            }
        );
        assert_eq!(mode(&state), 0o700);
        for name in std::iter::once(CATALOG_FILE).chain(CATALOG_COMPANIONS) {
            let path = state.join(name);
            assert_eq!(mode(&path), 0o600, "{}", path.display());
            assert_eq!(std::fs::read(&path).unwrap(), b"catalog bytes");
            assert!(
                repairs
                    .iter()
                    .any(|repair| repair.path == path && repair.old_mode == 0o644)
            );
        }
        assert_eq!(
            secure_state_directory_for(&state, euid()).unwrap(),
            Vec::new(),
            "a repaired directory needs no further change"
        );
    }

    #[test]
    fn private_state_and_absent_companions_need_no_repair() {
        let dir = tempfile::tempdir().unwrap();
        let state = dir.path().join("state");
        std::fs::create_dir(&state).unwrap();
        set_mode(&state, 0o700);
        std::fs::write(state.join(CATALOG_FILE), b"").unwrap();
        set_mode(&state.join(CATALOG_FILE), 0o600);
        assert_eq!(
            secure_state_directory_for(&state, euid()).unwrap(),
            Vec::new()
        );
        let empty = dir.path().join("empty");
        std::fs::create_dir(&empty).unwrap();
        set_mode(&empty, 0o700);
        assert_eq!(
            secure_state_directory_for(&empty, euid()).unwrap(),
            Vec::new()
        );
    }

    #[test]
    fn a_symbolically_linked_catalog_is_refused() {
        let (dir, state) = shared_state();
        let elsewhere = dir.path().join("elsewhere.db");
        std::fs::write(&elsewhere, b"other").unwrap();
        set_mode(&elsewhere, 0o644);
        std::fs::remove_file(state.join(CATALOG_FILE)).unwrap();
        std::os::unix::fs::symlink(&elsewhere, state.join(CATALOG_FILE)).unwrap();
        let error = secure_state_directory_for(&state, euid()).unwrap_err();
        assert!(error.to_string().contains("symbolic link"), "{error:#}");
        assert_eq!(mode(&elsewhere), 0o644, "the link target is untouched");
    }

    #[test]
    fn a_hard_linked_catalog_is_refused() {
        let (dir, state) = shared_state();
        std::fs::hard_link(state.join(CATALOG_FILE), dir.path().join("alias.db")).unwrap();
        let error = secure_state_directory_for(&state, euid()).unwrap_err();
        assert!(error.to_string().contains("hard links"), "{error:#}");
        assert_eq!(mode(&state.join(CATALOG_FILE)), 0o644);
    }

    #[test]
    fn a_symbolically_linked_state_directory_is_refused() {
        let (dir, state) = shared_state();
        let link = dir.path().join("state-link");
        std::os::unix::fs::symlink(&state, &link).unwrap();
        let error = secure_state_directory_for(&link, euid()).unwrap_err();
        assert!(
            format!("{error:#}").contains("without following a symbolic link"),
            "{error:#}"
        );
        assert_eq!(mode(&state), 0o755);
    }

    #[test]
    fn state_owned_by_another_account_is_refused() {
        let (_dir, state) = shared_state();
        let error = secure_state_directory_for(&state, euid().wrapping_add(1)).unwrap_err();
        assert!(error.to_string().contains("is owned by uid"), "{error:#}");
        assert_eq!(
            mode(&state),
            0o755,
            "nothing changes before ownership holds"
        );
    }

    #[test]
    fn special_files_in_place_of_the_catalog_are_refused_without_blocking() {
        let (_dir, state) = shared_state();
        let database = state.join(CATALOG_FILE);
        std::fs::remove_file(&database).unwrap();
        rustix::fs::mknodat(
            rustix::fs::CWD,
            &database,
            rustix::fs::FileType::Fifo,
            Mode::from_raw_mode(0o600),
            0,
        )
        .unwrap();
        let checked = state.clone();
        let (result_tx, result_rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = result_tx.send(secure_state_directory_for(&checked, euid()));
        });
        let error = result_rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("the check must not block on a FIFO")
            .unwrap_err();
        assert!(
            error.to_string().contains("not a regular file"),
            "{error:#}"
        );

        std::fs::remove_file(&database).unwrap();
        std::fs::create_dir(&database).unwrap();
        let error = secure_state_directory_for(&state, euid()).unwrap_err();
        assert!(
            error.to_string().contains("not a regular file"),
            "{error:#}"
        );
    }

    #[test]
    fn configuration_access_beyond_the_daemon_account_and_group_is_reported() {
        for mode in [0o600, 0o400, 0o640, 0o440] {
            assert!(!configuration_exposed(mode, 10, 10), "{mode:o}");
        }
        for mode in [0o644, 0o604, 0o602, 0o666] {
            assert!(configuration_exposed(mode, 10, 10), "{mode:o}");
        }
        assert!(
            configuration_exposed(0o640, 10, 11),
            "another group can read it"
        );
        assert!(!configuration_exposed(0o600, 10, 11));

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("irisd.toml");
        std::fs::write(&path, b"").unwrap();
        set_mode(&path, 0o600);
        assert_eq!(exposed_configuration(&path), None);
        set_mode(&path, 0o644);
        assert_eq!(exposed_configuration(&path), Some(0o644));
        assert_eq!(
            exposed_configuration(&dir.path().join("missing.toml")),
            None
        );
    }
}
