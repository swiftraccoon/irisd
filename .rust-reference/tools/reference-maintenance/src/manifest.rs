//! Snapshot provenance and offline integrity checks.

use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::fs;
use std::io::Write;
use std::path::Path;

use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use time::{OffsetDateTime, UtcOffset, format_description::well_known::Rfc3339};

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
pub struct Source {
    pub kind: String,
    pub url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub selection: Vec<String>,
    pub license: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub transformations: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit: Option<String>,
    #[serde(default)]
    pub retrieved_at: String,
    #[serde(default)]
    pub files: BTreeMap<String, String>,
}

impl Source {
    /// Names of the recorded provenance fields that differ from `config`, the
    /// source definition compiled into the tool.
    #[must_use]
    pub fn provenance_drift(&self, config: &Source) -> Vec<&'static str> {
        let mut drift = Vec::new();
        if self.kind != config.kind {
            drift.push("kind");
        }
        if self.url != config.url {
            drift.push("url");
        }
        if self.branch != config.branch {
            drift.push("branch");
        }
        if self.selection != config.selection {
            drift.push("selection");
        }
        if self.license != config.license {
            drift.push("license");
        }
        drift
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct Manifest {
    pub schema_version: u32,
    pub sources: BTreeMap<String, Source>,
}

impl Default for Manifest {
    fn default() -> Self {
        Self {
            schema_version: 1,
            sources: BTreeMap::new(),
        }
    }
}

pub fn read(root: &Path) -> Result<Manifest> {
    let path = root.join("references/manifest.json");
    let raw = match fs::read(&path) {
        Ok(raw) => raw,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(Manifest::default());
        }
        Err(error) => return Err(error).with_context(|| format!("Read {}", path.display())),
    };
    let manifest: Manifest =
        serde_json::from_slice(&raw).with_context(|| format!("Parse {}", path.display()))?;
    ensure!(manifest.schema_version == 1, "Unsupported manifest version");
    Ok(manifest)
}

/// Serialize a manifest as the pretty-printed JSON document stored in
/// `references/manifest.json`, including its trailing newline.
///
/// # Errors
/// Returns an error if serialization fails.
pub fn serialize(manifest: &Manifest) -> Result<Vec<u8>> {
    let mut bytes = serde_json::to_vec_pretty(manifest)?;
    bytes.push(b'\n');
    Ok(bytes)
}

/// Replace `references/manifest.json` atomically with a world-readable file.
///
/// # Errors
/// Returns an error if the temporary file cannot be written or moved into
/// place; no temporary file remains in that case.
pub fn write(references: &Path, manifest: &Manifest) -> Result<()> {
    let path = references.join("manifest.json");
    let mut temporary = tempfile::NamedTempFile::new_in(references)
        .with_context(|| format!("Create a temporary manifest in {}", references.display()))?;
    temporary.write_all(&serialize(manifest)?)?;
    temporary.flush()?;
    // Keep only the I/O error: dropping the failed temporary file removes it
    // even when the caller exits without unwinding.
    temporary
        .persist(&path)
        .map_err(|error| error.error)
        .with_context(|| format!("Replace {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644))
            .with_context(|| format!("Set permissions on {}", path.display()))?;
    }
    Ok(())
}

/// Return the current time as a canonical retrieval timestamp.
///
/// # Errors
/// Returns an error if the time cannot be formatted.
pub fn timestamp_now() -> Result<String> {
    format_timestamp(OffsetDateTime::now_utc())
}

/// Normalize an RFC 3339 timestamp to UTC with whole seconds and a `Z` suffix.
///
/// # Errors
/// Returns an error if `value` is not an RFC 3339 timestamp.
pub fn canonical_timestamp(value: &str) -> Result<String> {
    let parsed = OffsetDateTime::parse(value, &Rfc3339)
        .with_context(|| format!("Invalid RFC 3339 timestamp: {value}"))?;
    format_timestamp(parsed)
}

fn format_timestamp(value: OffsetDateTime) -> Result<String> {
    Ok(value
        .to_offset(UtcOffset::UTC)
        .replace_nanosecond(0)?
        .format(&Rfc3339)?)
}

pub fn digest(data: &[u8]) -> String {
    Sha256::digest(data)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// Report whether a file name belongs to a desktop file manager rather than to
/// the snapshot; such files appear wherever a folder is browsed and are never
/// reference content.
#[must_use]
pub fn is_os_metadata(name: &OsStr) -> bool {
    matches!(
        name.to_str(),
        Some(".DS_Store" | "Thumbs.db" | "desktop.ini")
    )
}

pub fn inventory(directory: &Path) -> Result<BTreeMap<String, String>> {
    let mut files = BTreeMap::new();
    visit(directory, directory, &mut files)?;
    Ok(files)
}

fn visit(root: &Path, directory: &Path, files: &mut BTreeMap<String, String>) -> Result<()> {
    let metadata = fs::symlink_metadata(directory)
        .with_context(|| format!("Read snapshot directory {}", directory.display()))?;
    ensure!(
        !metadata.is_symlink(),
        "Snapshot contains a symlink: {}",
        directory.display()
    );
    ensure!(
        metadata.is_dir(),
        "Expected directory: {}",
        directory.display()
    );
    let entries = fs::read_dir(directory)
        .with_context(|| format!("Read snapshot directory {}", directory.display()))?;
    for entry in entries {
        let entry =
            entry.with_context(|| format!("Read snapshot directory {}", directory.display()))?;
        let path = entry.path();
        let kind = entry
            .file_type()
            .with_context(|| format!("Inspect {}", path.display()))?;
        if kind.is_symlink() {
            bail!("Snapshot contains a symlink: {}", path.display());
        }
        if is_os_metadata(&entry.file_name()) {
            continue;
        }
        if kind.is_dir() {
            visit(root, &path, files)?;
        } else if kind.is_file() {
            let relative = path.strip_prefix(root)?;
            let name = relative
                .to_str()
                .context("Snapshot path is not UTF-8")?
                .replace(std::path::MAIN_SEPARATOR, "/");
            let content = fs::read(&path).with_context(|| format!("Read {}", path.display()))?;
            files.insert(name, digest(&content));
        } else {
            bail!("Snapshot contains a non-regular file: {}", path.display());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hashes_known_bytes() {
        assert_eq!(
            digest(b"hello world"),
            "b94d27b9934d3e08a52e52d7da7dabfac484efe37a5380ee9088f7ace2efcde9"
        );
    }

    #[test]
    fn inventories_hidden_files_and_detects_edits() -> Result<()> {
        let directory = tempfile::tempdir()?;
        fs::create_dir(directory.path().join(".github"))?;
        let path = directory.path().join(".github/guide.md");
        fs::write(&path, "first")?;
        let original = inventory(directory.path())?;
        assert_eq!(original.len(), 1);
        assert!(original.contains_key(".github/guide.md"));
        fs::write(&path, "second")?;
        assert_ne!(original, inventory(directory.path())?);
        Ok(())
    }

    #[test]
    fn inventory_ignores_os_metadata() -> Result<()> {
        let directory = tempfile::tempdir()?;
        fs::write(directory.path().join("guide.md"), "content")?;
        fs::write(directory.path().join(".DS_Store"), "finder")?;
        fs::create_dir(directory.path().join("nested"))?;
        fs::write(directory.path().join("nested/Thumbs.db"), "explorer")?;
        fs::write(directory.path().join("nested/desktop.ini"), "explorer")?;
        let files = inventory(directory.path())?;
        assert_eq!(files.keys().collect::<Vec<_>>(), ["guide.md"]);
        Ok(())
    }

    #[test]
    fn committed_manifest_round_trips_through_the_writer() -> Result<()> {
        let root = crate::reference_root();
        let committed = fs::read(root.join("references/manifest.json"))?;
        // A manifest the tool cannot reproduce byte for byte would make the
        // first refresh rewrite every entry and bury the real diff.
        assert_eq!(serialize(&read(root)?)?, committed);
        Ok(())
    }

    #[test]
    fn reads_existing_snapshot_manifest() -> Result<()> {
        let root = crate::reference_root();
        let manifest = read(root)?;
        assert_eq!(manifest.sources.len(), 6);
        for (id, source) in manifest.sources {
            assert_eq!(inventory(&root.join("references").join(id))?, source.files);
        }
        Ok(())
    }

    fn fixture_manifest() -> Manifest {
        let mut manifest = Manifest::default();
        manifest.sources.insert(
            "fixture".into(),
            Source {
                kind: "git".into(),
                url: "https://example.invalid/fixture.git".into(),
                branch: Some("main".into()),
                license: "MIT".into(),
                commit: Some("0".repeat(40)),
                retrieved_at: "2026-11-01T00:31:11Z".into(),
                files: BTreeMap::from([("guide.md".to_owned(), digest(b"guide"))]),
                ..Source::default()
            },
        );
        manifest
    }

    #[test]
    fn canonical_timestamp_normalizes_offset_and_subseconds() -> Result<()> {
        assert_eq!(
            canonical_timestamp("2026-11-01T00:31:11.5+00:00")?,
            "2026-11-01T00:31:11Z"
        );
        assert_eq!(
            canonical_timestamp("2026-11-01T02:31:11+02:00")?,
            "2026-11-01T00:31:11Z"
        );
        assert!(canonical_timestamp("yesterday").is_err());
        let now = timestamp_now()?;
        assert!(now.ends_with('Z') && !now.contains('.'), "{now}");
        Ok(())
    }

    #[test]
    fn write_sets_readable_mode_and_round_trips() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let references = directory.path().join("references");
        fs::create_dir(&references)?;
        let manifest = fixture_manifest();

        write(&references, &manifest)?;

        let written = fs::read(references.join("manifest.json"))?;
        assert_eq!(written, serialize(&manifest)?);
        assert!(written.ends_with(b"}\n"));
        assert_eq!(read(directory.path())?, manifest);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(references.join("manifest.json"))?
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o644, "mode {mode:o}");
        }
        Ok(())
    }

    #[test]
    fn failed_write_leaves_no_temporary_file() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let references = directory.path().join("references");
        // A directory at the manifest path makes the atomic replacement fail.
        fs::create_dir_all(references.join("manifest.json"))?;

        let error = write(&references, &fixture_manifest()).unwrap_err();

        let leftovers: Vec<_> = fs::read_dir(&references)?
            .filter_map(Result::ok)
            .map(|entry| entry.file_name())
            .filter(|name| name != "manifest.json")
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?} while {error} is alive");
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn inventory_errors_name_the_unreadable_path() -> Result<()> {
        use std::os::unix::fs::PermissionsExt;
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("secret.md");
        fs::write(&path, "hidden")?;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o000))?;
        let result = inventory(directory.path());
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644))?;
        // A superuser can read the file regardless; there is nothing to assert.
        let Err(error) = result else { return Ok(()) };
        let message = format!("{error:#}");
        assert!(message.contains("secret.md"), "{message}");
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn rejects_snapshot_symlinks() -> Result<()> {
        let directory = tempfile::tempdir()?;
        std::os::unix::fs::symlink("outside", directory.path().join("link"))?;
        assert!(inventory(directory.path()).is_err());
        Ok(())
    }
}
