//! Startup permission checks for the files that hold the daemon's secrets and
//! state: the configuration file, the state directory, and the state files in
//! it (the catalog's database, write-ahead log, and shared-memory files, the
//! media-capability key, and the store of cameras the API added).
//!
//! SQLite gives a write-ahead log or shared-memory file it creates the
//! database file's mode, so a catalog once created readable by other users
//! stays that way across restarts even under a restrictive umask. The daemon
//! therefore checks the state directory and the state files before SQLite
//! opens them: each must belong to the service account, the files must be
//! regular files with one link, and group or other permissions are removed
//! with a warning. A wrong owner, type, or link count fails startup.

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
/// File name of the media-capability signing secret inside the state directory.
pub const CAPABILITY_KEY_FILE: &str = "capability.key";
/// File name of the store of cameras the API added, inside the state directory.
pub const PROVISIONED_CAMERAS_FILE: &str = "cameras.toml";
/// Mode of the state directory: the service account only.
const PRIVATE_DIRECTORY: u32 = 0o700;
/// Mode of a state file (catalog, capability key, camera store): read and
/// write for the service account only.
const PRIVATE_FILE: u32 = 0o600;
/// Flags of every state-file open: never through a final symbolic link, never
/// blocking on a FIFO or device, never acquiring a controlling terminal.
const STATE_FILE_FLAGS: OFlags = OFlags::RDONLY
    .union(OFlags::NOFOLLOW)
    .union(OFlags::NONBLOCK)
    .union(OFlags::NOCTTY)
    .union(OFlags::CLOEXEC);

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

/// Checks the state directory and the state files in it before the catalog
/// opens, and returns the modes it tightened.
///
/// The directory is opened without following a final symbolic link and must
/// be owned by the effective user; group or other bits are removed (mode
/// 0700). Each of `irisd.db`, `irisd.db-wal`, `irisd.db-shm`,
/// `capability.key`, and `cameras.toml` that exists is opened relative to
/// that directory without following a symbolic link or blocking on a special
/// file, must be a regular file with one link owned by the effective user,
/// and gets mode 0600 when group or other bits are set.
///
/// # Errors
///
/// Fails when the directory or a file cannot be opened or inspected, is a
/// symbolic link, a special file, or has several links, or belongs to
/// another account.
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
    for name in std::iter::once(CATALOG_FILE)
        .chain(CATALOG_COMPANIONS)
        .chain([CAPABILITY_KEY_FILE, PROVISIONED_CAMERAS_FILE])
    {
        let path = dir.join(name);
        let file = match rustix::fs::openat(&directory, name, STATE_FILE_FLAGS, Mode::empty()) {
            Ok(file) => File::from(file),
            Err(Errno::NOENT) => continue,
            Err(Errno::LOOP) => bail!(
                "{} is a symbolic link; state files must be regular files in the state directory",
                path.display()
            ),
            Err(error) => {
                return Err(io::Error::from(error)).with_context(|| {
                    format!("opening {} to check its permissions", path.display())
                });
            }
        };
        require_regular_with_one_link(&file, &path)?;
        check_owner(&file, &path, owner)?;
        tighten(&file, &path, PRIVATE_FILE, &mut repairs)?;
    }
    Ok(repairs)
}

/// Opens the state file at `path` for reading without following a symbolic
/// link or blocking on a special file; `None` when nothing exists there. The
/// file must be a regular file with one link.
///
/// # Errors
///
/// Fails when the path is a symbolic link, is not a regular file, is
/// reachable through another name, or cannot be opened or inspected.
pub fn open_private_file(path: &Path) -> anyhow::Result<Option<File>> {
    let file = match rustix::fs::open(path, STATE_FILE_FLAGS, Mode::empty()) {
        Ok(file) => File::from(file),
        Err(Errno::NOENT) => return Ok(None),
        Err(Errno::LOOP) => bail!(
            "{} is a symbolic link; state files must be regular files in the state directory",
            path.display()
        ),
        Err(error) => {
            return Err(io::Error::from(error))
                .with_context(|| format!("opening {}", path.display()));
        }
    };
    require_regular_with_one_link(&file, path)?;
    Ok(Some(file))
}

fn require_regular_with_one_link(file: &File, path: &Path) -> anyhow::Result<()> {
    let metadata = file
        .metadata()
        .with_context(|| format!("reading the status of {}", path.display()))?;
    if !metadata.file_type().is_file() {
        bail!("{} is not a regular file", path.display());
    }
    if metadata.nlink() != 1 {
        bail!(
            "{} has {} hard links; a state file must not be reachable through another name",
            path.display(),
            metadata.nlink()
        );
    }
    Ok(())
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

/// A user and group id pair.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Account {
    uid: u32,
    gid: u32,
}

/// Permission bits of the configuration file when a user other than root or
/// the daemon's account can read or write it; `None` when it is private or
/// its status cannot be read.
///
/// The file holds camera passwords and API tokens. Group access is accepted
/// only for the daemon's own group, and owner access only for root or the
/// daemon's account, which matches the packaged layout (mode 0640, owner
/// root, group irisd).
pub fn exposed_configuration(path: &Path) -> Option<u32> {
    let metadata = std::fs::metadata(path).ok()?;
    let mode = metadata.mode() & 0o7777;
    let owner = Account {
        uid: metadata.uid(),
        gid: metadata.gid(),
    };
    let daemon = Account {
        uid: rustix::process::geteuid().as_raw(),
        gid: rustix::process::getegid().as_raw(),
    };
    configuration_exposed(mode, owner, daemon).then_some(mode)
}

/// Whether `mode` lets a user other than root or the daemon read or write the
/// file: other bits, group bits for a group other than the daemon's, or owner
/// bits for an owner who is neither root nor the daemon.
fn configuration_exposed(mode: u32, owner: Account, daemon: Account) -> bool {
    mode & 0o006 != 0
        || (mode & 0o060 != 0 && owner.gid != daemon.gid)
        || (mode & 0o600 != 0 && owner.uid != 0 && owner.uid != daemon.uid)
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

    fn mkfifo(path: &Path) {
        rustix::fs::mknodat(
            rustix::fs::CWD,
            path,
            rustix::fs::FileType::Fifo,
            Mode::from_raw_mode(0o600),
            0,
        )
        .unwrap();
    }

    /// A state directory holding the catalog files, the capability key, and
    /// the camera store, all readable by other users.
    fn shared_state() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let state = dir.path().join("state");
        std::fs::create_dir(&state).unwrap();
        set_mode(&state, 0o755);
        for name in std::iter::once(CATALOG_FILE)
            .chain(CATALOG_COMPANIONS)
            .chain([CAPABILITY_KEY_FILE, PROVISIONED_CAMERAS_FILE])
        {
            let path = state.join(name);
            std::fs::write(&path, b"catalog bytes").unwrap();
            set_mode(&path, 0o644);
        }
        (dir, state)
    }

    #[test]
    fn a_shared_state_directory_and_its_files_are_restricted_to_the_owner() {
        let (_dir, state) = shared_state();
        let repairs = secure_state_directory_for(&state, euid()).unwrap();
        assert_eq!(repairs.len(), 6, "{repairs:?}");
        assert_eq!(
            repairs[0],
            ModeRepair {
                path: state.clone(),
                old_mode: 0o755,
                new_mode: 0o700,
            }
        );
        assert_eq!(mode(&state), 0o700);
        for name in std::iter::once(CATALOG_FILE)
            .chain(CATALOG_COMPANIONS)
            .chain([CAPABILITY_KEY_FILE, PROVISIONED_CAMERAS_FILE])
        {
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

    /// The entry point the daemon calls checks against the effective user
    /// and repairs the same six modes.
    #[test]
    fn the_daemons_entry_point_repairs_for_the_effective_user() {
        let (_dir, state) = shared_state();
        let repairs = secure_state_directory(&state).unwrap();
        assert_eq!(repairs.len(), 6, "{repairs:?}");
        assert_eq!(mode(&state), 0o700);
        assert_eq!(mode(&state.join(CATALOG_FILE)), 0o600);
        assert_eq!(secure_state_directory(&state).unwrap(), Vec::new());
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
        mkfifo(&database);
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
    fn open_private_file_returns_none_for_an_absent_path() {
        let dir = tempfile::tempdir().unwrap();
        assert!(
            open_private_file(&dir.path().join("absent"))
                .unwrap()
                .is_none()
        );
        std::fs::write(dir.path().join("present"), b"x").unwrap();
        assert!(
            open_private_file(&dir.path().join("present"))
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn open_private_file_refuses_a_symbolic_link() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("target"), b"x").unwrap();
        std::os::unix::fs::symlink(dir.path().join("target"), dir.path().join("link")).unwrap();
        let error = open_private_file(&dir.path().join("link")).unwrap_err();
        assert!(error.to_string().contains("symbolic link"), "{error:#}");
    }

    #[test]
    fn open_private_file_refuses_a_hard_linked_file() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("file"), b"x").unwrap();
        std::fs::hard_link(dir.path().join("file"), dir.path().join("alias")).unwrap();
        let error = open_private_file(&dir.path().join("file")).unwrap_err();
        assert!(error.to_string().contains("hard links"), "{error:#}");
    }

    #[test]
    fn open_private_file_does_not_block_on_a_fifo() {
        let dir = tempfile::tempdir().unwrap();
        let fifo = dir.path().join("fifo");
        mkfifo(&fifo);
        let (result_tx, result_rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = result_tx.send(open_private_file(&fifo).map(|file| file.is_some()));
        });
        let error = result_rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("the open must not block on a FIFO")
            .unwrap_err();
        assert!(
            error.to_string().contains("not a regular file"),
            "{error:#}"
        );
    }

    #[test]
    fn configuration_access_beyond_root_and_the_daemon_account_and_group_is_reported() {
        let daemon = Account { uid: 10, gid: 10 };
        let same = Account { uid: 10, gid: 10 };
        let root_in_group = Account { uid: 0, gid: 10 };
        for mode in [0o600, 0o400, 0o640, 0o440] {
            assert!(!configuration_exposed(mode, same, daemon), "{mode:o}");
            assert!(
                !configuration_exposed(mode, root_in_group, daemon),
                "{mode:o}"
            );
        }
        for mode in [0o644, 0o604, 0o602, 0o666] {
            assert!(configuration_exposed(mode, same, daemon), "{mode:o}");
        }
        assert!(
            configuration_exposed(0o640, Account { uid: 10, gid: 11 }, daemon),
            "another group can read it"
        );
        assert!(!configuration_exposed(
            0o600,
            Account { uid: 10, gid: 11 },
            daemon
        ));
        assert!(
            configuration_exposed(0o640, Account { uid: 11, gid: 10 }, daemon),
            "another account owns it and can read it"
        );
        assert!(
            !configuration_exposed(0o040, Account { uid: 11, gid: 10 }, daemon),
            "an owner without owner bits exposes nothing"
        );

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
