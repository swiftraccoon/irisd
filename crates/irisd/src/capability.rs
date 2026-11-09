//! The media-capability signing secret.
//!
//! Media capabilities ([`CapabilitySecret`]) are signed with 32 random bytes
//! the daemon creates once and keeps in its state directory as
//! `capability.key`: 64 lowercase hexadecimal characters and a newline, mode
//! 0600. Reusing the file across restarts keeps capabilities issued before a
//! restart valid, and keeping the root tokens out of the derivation means a
//! captured capability URL reveals nothing about them. An unreadable or
//! malformed file fails startup with its path: replacing it silently would
//! invalidate every outstanding capability without a trace.

use std::fmt::Write as _;
use std::fs::File;
use std::io::{Read as _, Write as _};
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::Path;

use anyhow::{Context, bail};
use iris_api::{CAPABILITY_SECRET_BYTES, CapabilitySecret};

use crate::permissions::{CAPABILITY_KEY_FILE, open_private_file};

/// Hexadecimal characters in the file before the newline.
const ENCODED_LEN: usize = CAPABILITY_SECRET_BYTES * 2;
/// Bytes read from the file at most: the encoding, its newline, and one byte
/// that shows the file holds more than that.
const MAX_FILE_BYTES: usize = ENCODED_LEN + 2;

/// Loads the secret from `capability.key` in `dir`, creating the file when it
/// is absent. The directory must exist and be private to the daemon
/// ([`crate::permissions::secure_state_directory`] runs first).
///
/// # Errors
///
/// Fails when the file is a link, a special file, or malformed, when it
/// cannot be read, or when a fresh key cannot be drawn or written.
pub fn load_or_create(dir: &Path) -> anyhow::Result<CapabilitySecret> {
    let path = dir.join(CAPABILITY_KEY_FILE);
    match open_private_file(&path)
        .with_context(|| format!("opening the capability key {}", path.display()))?
    {
        Some(file) => read_secret(file, &path),
        None => create(dir, &path),
    }
}

/// A secret for a daemon without API tokens: never persisted and never used to
/// issue a capability, present so the API state is complete.
///
/// # Errors
///
/// Fails when the random source cannot be read.
pub fn ephemeral() -> anyhow::Result<CapabilitySecret> {
    Ok(CapabilitySecret::new(random_bytes()?))
}

fn random_bytes() -> anyhow::Result<[u8; CAPABILITY_SECRET_BYTES]> {
    let mut bytes = [0; CAPABILITY_SECRET_BYTES];
    getrandom::fill(&mut bytes).context("drawing the capability secret from the random source")?;
    Ok(bytes)
}

fn read_secret(mut file: File, path: &Path) -> anyhow::Result<CapabilitySecret> {
    let mut text = Vec::with_capacity(MAX_FILE_BYTES);
    std::io::Read::by_ref(&mut file)
        .take(MAX_FILE_BYTES as u64)
        .read_to_end(&mut text)
        .with_context(|| format!("reading the capability key {}", path.display()))?;
    let encoded = text.strip_suffix(b"\n").unwrap_or(&text);
    decode(encoded).map(CapabilitySecret::new).with_context(|| {
        format!(
            "{} must hold {ENCODED_LEN} hexadecimal characters; remove the file to create a fresh \
             secret, which invalidates outstanding media capability URLs",
            path.display()
        )
    })
}

fn decode(encoded: &[u8]) -> anyhow::Result<[u8; CAPABILITY_SECRET_BYTES]> {
    anyhow::ensure!(
        encoded.len() == ENCODED_LEN,
        "the file holds {} bytes before its newline",
        encoded.len()
    );
    let mut bytes = [0; CAPABILITY_SECRET_BYTES];
    let (pairs, _) = encoded.as_chunks::<2>();
    for (byte, pair) in bytes.iter_mut().zip(pairs) {
        *byte = (nibble(pair[0])? << 4) | nibble(pair[1])?;
    }
    Ok(bytes)
}

fn nibble(character: u8) -> anyhow::Result<u8> {
    match character {
        b'0'..=b'9' => Ok(character - b'0'),
        b'a'..=b'f' => Ok(character - b'a' + 10),
        b'A'..=b'F' => Ok(character - b'A' + 10),
        other => bail!("byte 0x{other:02x} is not a hexadecimal digit"),
    }
}

fn encode(bytes: &[u8; CAPABILITY_SECRET_BYTES]) -> String {
    let mut text = String::with_capacity(ENCODED_LEN + 1);
    for byte in bytes {
        let _ = write!(text, "{byte:02x}");
    }
    text.push('\n');
    text
}

/// Creates the key file: the secret is written to a private temporary file in
/// the same directory, synced, and renamed into place, so a crash leaves
/// either no key or a complete one, never a partial one.
fn create(dir: &Path, path: &Path) -> anyhow::Result<CapabilitySecret> {
    let bytes = random_bytes()?;
    let temporary = dir.join(format!("{CAPABILITY_KEY_FILE}.tmp"));
    let mut file = match exclusive_private(&temporary) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            // Left by an interrupted creation; it was never renamed into place.
            std::fs::remove_file(&temporary)
                .with_context(|| format!("removing the stale {}", temporary.display()))?;
            exclusive_private(&temporary)
                .with_context(|| format!("creating {}", temporary.display()))?
        }
        Err(error) => {
            return Err(error).with_context(|| format!("creating {}", temporary.display()));
        }
    };
    file.write_all(encode(&bytes).as_bytes())
        .and_then(|()| file.sync_all())
        .with_context(|| format!("writing {}", temporary.display()))?;
    drop(file);
    std::fs::rename(&temporary, path).with_context(|| format!("publishing {}", path.display()))?;
    File::open(dir)
        .and_then(|directory| directory.sync_all())
        .with_context(|| format!("syncing the state directory {}", dir.display()))?;
    Ok(CapabilitySecret::new(bytes))
}

fn exclusive_private(path: &Path) -> std::io::Result<File> {
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt as _;

    use super::*;

    fn key_path(dir: &tempfile::TempDir) -> std::path::PathBuf {
        dir.path().join(CAPABILITY_KEY_FILE)
    }

    #[test]
    fn a_missing_key_is_created_private_and_reloaded_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        let first = load_or_create(dir.path()).unwrap();
        let path = key_path(&dir);
        let metadata = std::fs::metadata(&path).unwrap();
        assert_eq!(metadata.permissions().mode() & 0o777, 0o600);
        let text = std::fs::read(&path).unwrap();
        assert_eq!(text.len(), ENCODED_LEN + 1);
        assert_eq!(text.last(), Some(&b'\n'));
        assert!(
            text[..ENCODED_LEN]
                .iter()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()),
            "{text:?}"
        );
        assert!(!dir.path().join("capability.key.tmp").exists());
        let second = load_or_create(dir.path()).unwrap();
        assert_eq!(first.as_bytes(), second.as_bytes());
        assert_eq!(decode(&text[..ENCODED_LEN]).unwrap(), *first.as_bytes());
    }

    #[test]
    fn every_secret_is_fresh() {
        let one = tempfile::tempdir().unwrap();
        let two = tempfile::tempdir().unwrap();
        assert_ne!(
            load_or_create(one.path()).unwrap().as_bytes(),
            load_or_create(two.path()).unwrap().as_bytes()
        );
        assert_ne!(
            ephemeral().unwrap().as_bytes(),
            ephemeral().unwrap().as_bytes()
        );
    }

    #[test]
    fn a_malformed_key_fails_naming_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = key_path(&dir);
        for content in [
            String::new(),
            "abc\n".to_owned(),
            "0".repeat(ENCODED_LEN - 1),
            format!("{}\n", "g".repeat(ENCODED_LEN)),
            format!("{}\n\n", "0".repeat(ENCODED_LEN)),
            format!("{}0\n", "0".repeat(ENCODED_LEN)),
        ] {
            std::fs::write(&path, &content).unwrap();
            let error = load_or_create(dir.path()).unwrap_err();
            let text = format!("{error:#}");
            assert!(text.contains(CAPABILITY_KEY_FILE), "{content:?}: {text}");
            assert!(text.contains("hexadecimal"), "{content:?}: {text}");
        }
    }

    #[test]
    fn uppercase_hexadecimal_is_accepted() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            key_path(&dir),
            format!("{}\n", "0A".repeat(CAPABILITY_SECRET_BYTES)),
        )
        .unwrap();
        assert_eq!(
            load_or_create(dir.path()).unwrap().as_bytes(),
            &[0x0a; CAPABILITY_SECRET_BYTES]
        );
    }

    #[test]
    fn the_trailing_newline_is_optional() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(key_path(&dir), "ff".repeat(CAPABILITY_SECRET_BYTES)).unwrap();
        assert_eq!(
            load_or_create(dir.path()).unwrap().as_bytes(),
            &[0xff; CAPABILITY_SECRET_BYTES]
        );
    }

    #[test]
    fn a_symbolically_linked_key_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let elsewhere = dir.path().join("elsewhere");
        std::fs::write(&elsewhere, encode(&[1; CAPABILITY_SECRET_BYTES])).unwrap();
        std::os::unix::fs::symlink(&elsewhere, key_path(&dir)).unwrap();
        let error = load_or_create(dir.path()).unwrap_err();
        assert!(format!("{error:#}").contains("symbolic link"), "{error:#}");
    }

    #[test]
    fn a_hard_linked_key_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let elsewhere = dir.path().join("elsewhere");
        std::fs::write(&elsewhere, encode(&[2; CAPABILITY_SECRET_BYTES])).unwrap();
        std::fs::hard_link(&elsewhere, key_path(&dir)).unwrap();
        let error = load_or_create(dir.path()).unwrap_err();
        assert!(format!("{error:#}").contains("hard links"), "{error:#}");
    }

    #[test]
    fn a_stale_temporary_key_is_replaced() {
        let dir = tempfile::tempdir().unwrap();
        let temporary = dir.path().join("capability.key.tmp");
        std::fs::write(&temporary, "partial").unwrap();
        let secret = load_or_create(dir.path()).unwrap();
        assert!(!temporary.exists());
        assert_eq!(
            load_or_create(dir.path()).unwrap().as_bytes(),
            secret.as_bytes()
        );
    }
}
