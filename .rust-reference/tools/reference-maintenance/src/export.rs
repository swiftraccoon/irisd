//! Copy a verified reference pack without changing the consuming project's files.

use std::collections::BTreeSet;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use anyhow::{Context, Result, bail, ensure};

use crate::sync::{self, Options};
use crate::{git, manifest};

const PACK_NAME: &str = ".rust-reference";
const REQUIRED_ENTRIES: &[&str] = &[
    "README.md",
    "AGENTS.md",
    "LICENSE",
    "docs",
    "references",
    "templates",
    "tools/reference-maintenance",
];
const OPTIONAL_ENTRIES: &[&str] = &[".gitattributes", ".gitignore", ".editorconfig"];

/// The outcome of a successful export.
#[derive(Debug)]
pub struct Export {
    /// The installed pack directory.
    pub destination: PathBuf,
    /// Pack files, relative to the project, that its Git ignore rules would
    /// exclude from commits; empty when the project is not a Git work tree.
    pub ignored_by_git: Vec<String>,
}

/// Export the complete reference pack into an existing project directory.
///
/// # Errors
/// Returns an error if the pack is incomplete or holds unverified content, if
/// the project is missing or already has a pack, or if copying fails.
pub fn run(root: &Path, project: &Path) -> Result<Export> {
    let root = root
        .canonicalize()
        .context("The source reference workspace does not exist")?;
    let project = project
        .canonicalize()
        .with_context(|| format!("Project directory does not exist: {}", project.display()))?;
    ensure!(project.is_dir(), "Project path must be a directory");
    ensure!(
        !project.starts_with(&root),
        "Choose a project outside the reference workspace"
    );
    let destination = project.join(PACK_NAME);
    ensure_absent(&destination)?;
    for entry in REQUIRED_ENTRIES {
        ensure!(
            root.join(entry).exists(),
            "Incomplete reference pack; missing: {entry}"
        );
    }
    for entry in ["Cargo.toml", "Cargo.lock", "src"] {
        ensure!(
            root.join("tools/reference-maintenance")
                .join(entry)
                .exists(),
            "Incomplete maintenance tool; missing: {entry}"
        );
    }
    ensure!(
        root.join("references/manifest.json").is_file(),
        "Missing references/manifest.json; fetch references before exporting"
    );
    let managed: BTreeSet<String> = manifest::read(&root)?.sources.into_keys().collect();

    let temporary = tempfile::Builder::new()
        .prefix(".rust-reference-export-")
        .tempdir_in(&project)
        .context("Cannot stage the reference pack in the destination project")?;
    let staged = temporary.path().join(PACK_NAME);
    fs::create_dir(&staged)?;
    for entry in REQUIRED_ENTRIES.iter().chain(OPTIONAL_ENTRIES) {
        let source = root.join(entry);
        if source.exists() {
            let destination = staged.join(entry);
            if let Some(parent) = destination.parent() {
                fs::create_dir_all(parent)?;
            }
            if *entry == "references" {
                copy_references(&source, &destination, &managed)?;
            } else {
                copy_entry(&source, &destination, false)?;
            }
        }
    }
    sync::run(
        &staged,
        &[],
        Options {
            check: true,
            locked: false,
            force: false,
        },
    )
    .context("Copied reference snapshots failed their integrity check")?;

    let mut paths = Vec::new();
    relative_files(&staged, &staged, &mut paths)?;
    paths.sort();
    ensure_absent(&destination)?;
    fs::rename(&staged, &destination).context("Cannot install the staged reference pack")?;
    // Git needs the installed .gitignore hierarchy to apply the pack's own
    // rules and negations. Failure of this advisory check must not make an
    // already installed pack look like a failed export.
    let ignored_by_git = match ignored_by_git(&project, &paths) {
        Ok(ignored) => ignored,
        Err(error) => {
            eprintln!(
                "Warning: reference pack installed, but its Git ignore rules could not be checked: \
                 {error:#}"
            );
            Vec::new()
        }
    };
    Ok(Export {
        destination,
        ignored_by_git,
    })
}

/// Ask the project's Git which pack paths its ignore rules would exclude from a
/// commit. The check is advisory: without Git, or outside a work tree, it
/// reports nothing rather than failing the export.
fn ignored_by_git(project: &Path, paths: &[String]) -> Result<Vec<String>> {
    let input: Vec<u8> = paths
        .iter()
        .flat_map(|path| format!("{PACK_NAME}/{path}\0").into_bytes())
        .collect();
    let Ok(mut child) = git::command(Some(project))
        .args(["check-ignore", "--no-index", "-z", "--stdin"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
    else {
        return Ok(Vec::new());
    };
    let mut stdin = child.stdin.take().context("open check-ignore input")?;
    let writer = std::thread::spawn(move || stdin.write_all(&input));
    let output = child.wait_with_output().context("run git check-ignore")?;
    let written = writer
        .join()
        .map_err(|_| anyhow::anyhow!("check-ignore writer panicked"))?;
    // Exit status 0 lists ignored paths; 1 means none, 128 means no repository.
    if written.is_err() || !output.status.success() {
        return Ok(Vec::new());
    }
    Ok(output
        .stdout
        .split(|byte| *byte == 0)
        .filter(|path| !path.is_empty())
        .map(|path| String::from_utf8_lossy(path).into_owned())
        .collect())
}

fn relative_files(root: &Path, directory: &Path, paths: &mut Vec<String>) -> Result<()> {
    let entries =
        fs::read_dir(directory).with_context(|| format!("Read {}", directory.display()))?;
    for entry in entries {
        let entry = entry.with_context(|| format!("Read {}", directory.display()))?;
        let path = entry.path();
        if entry.file_type()?.is_dir() {
            relative_files(root, &path, paths)?;
        } else {
            let relative = path.strip_prefix(root)?.to_string_lossy();
            paths.push(relative.replace(std::path::MAIN_SEPARATOR, "/"));
        }
    }
    Ok(())
}

fn ensure_absent(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(_) => bail!(
            "Reference pack already exists; refusing to overwrite: {}",
            path.display()
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| format!("Cannot inspect {}", path.display())),
    }
}

/// Copy `references/` under the pack's policy: snapshot directories recorded in
/// the manifest are copied verbatim, operating-system metadata is skipped, and
/// anything that could ship unverified content is an error.
fn copy_references(source: &Path, destination: &Path, managed: &BTreeSet<String>) -> Result<()> {
    fs::create_dir(destination).with_context(|| format!("Create {}", destination.display()))?;
    let entries = fs::read_dir(source).with_context(|| format!("Read {}", source.display()))?;
    for entry in entries {
        let entry = entry.with_context(|| format!("Read {}", source.display()))?;
        let name = entry.file_name();
        if manifest::is_os_metadata(&name) {
            continue;
        }
        let text = name.to_string_lossy();
        ensure!(
            !text.starts_with(".reference-sync-") && !text.starts_with(".tmp"),
            "references/ contains a leftover from an interrupted sync: {text}; \
             remove it before exporting"
        );
        let kind = entry
            .file_type()
            .with_context(|| format!("Inspect {}", entry.path().display()))?;
        if kind.is_dir() {
            ensure!(
                managed.contains(text.as_ref()),
                "references/ contains an unmanaged directory: {text}; \
                 remove it or record it in the manifest before exporting"
            );
        }
        copy_entry(&entry.path(), &destination.join(&name), true)?;
    }
    Ok(())
}

fn copy_entry(source: &Path, destination: &Path, upstream: bool) -> Result<()> {
    let name = source.file_name().unwrap_or_default();
    if manifest::is_os_metadata(name) || (!upstream && excluded(source)) {
        return Ok(());
    }
    let metadata =
        fs::symlink_metadata(source).with_context(|| format!("Inspect {}", source.display()))?;
    ensure!(
        !metadata.file_type().is_symlink(),
        "Reference pack contains a nonportable symlink: {}",
        source.display()
    );
    if metadata.is_dir() {
        fs::create_dir(destination).with_context(|| format!("Create {}", destination.display()))?;
        let entries = fs::read_dir(source).with_context(|| format!("Read {}", source.display()))?;
        for entry in entries {
            let entry = entry.with_context(|| format!("Read {}", source.display()))?;
            copy_entry(
                &entry.path(),
                &destination.join(entry.file_name()),
                upstream,
            )?;
        }
    } else {
        ensure!(
            metadata.is_file(),
            "Reference pack contains an unsupported file: {}",
            source.display()
        );
        fs::copy(source, destination).with_context(|| format!("Copy {}", source.display()))?;
    }
    Ok(())
}

fn excluded(path: &Path) -> bool {
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    matches!(name.as_ref(), ".git" | ".cache" | "target" | "__pycache__")
        || matches!(
            path.extension().and_then(|extension| extension.to_str()),
            Some("py" | "pyc" | "pyo")
        )
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;

    fn fixture(parent: &Path) -> Result<PathBuf> {
        let source = parent.join("source");
        fs::create_dir(&source)?;
        for directory in [
            "docs",
            "references",
            "templates",
            "tools/reference-maintenance/src",
            "tools/reference-maintenance/tests",
        ] {
            fs::create_dir_all(source.join(directory))?;
        }
        for filename in [
            "README.md",
            "AGENTS.md",
            "LICENSE",
            "tools/reference-maintenance/Cargo.toml",
            "tools/reference-maintenance/Cargo.lock",
        ] {
            fs::write(source.join(filename), "reference fixture\n")?;
        }
        fs::write(source.join("references/manifest.json"), "{}")?;
        Ok(source)
    }

    #[test]
    fn export_preserves_project_policy_and_complete_upstream_content() -> Result<()> {
        let temporary = tempfile::tempdir()?;
        let source = fixture(temporary.path())?;
        fs::remove_dir_all(source.join("references"))?;
        copy_entry(
            &crate::reference_root().join("references"),
            &source.join("references"),
            true,
        )?;
        // Upstream tracked content is copied verbatim, even when its filename
        // would be omitted from this project's own maintenance tooling.
        let upstream = source.join("references/rust-api-guidelines");
        fs::write(upstream.join("tracked-source.py"), "upstream reference\n")?;
        let manifest_path = source.join("references/manifest.json");
        let mut manifest: serde_json::Value = serde_json::from_slice(&fs::read(&manifest_path)?)?;
        manifest["sources"]["rust-api-guidelines"]["files"] =
            serde_json::to_value(crate::manifest::inventory(&upstream)?)?;
        fs::write(&manifest_path, serde_json::to_vec(&manifest)?)?;

        let maintenance = source.join("tools/reference-maintenance");
        for directory in [".cache", ".git", "target", "__pycache__"] {
            fs::create_dir(maintenance.join(directory))?;
            fs::write(maintenance.join(directory).join("private"), "cache")?;
        }
        fs::write(maintenance.join("src/legacy.py"), "legacy")?;
        fs::write(maintenance.join("tests/legacy.pyc"), "legacy")?;
        fs::write(maintenance.join("src/main.rs"), "fn main() {}")?;
        fs::write(source.join("rust-toolchain.toml"), "root toolchain\n")?;
        fs::write(
            maintenance.join("rust-toolchain.toml"),
            "maintenance toolchain\n",
        )?;
        let project = temporary.path().join("project");
        fs::create_dir(&project)?;
        fs::write(project.join("AGENTS.md"), "Project instructions\n")?;
        fs::write(project.join("rust-toolchain.toml"), "project toolchain\n")?;

        let destination = run(&source, &project)?.destination;

        assert_eq!(destination, project.canonicalize()?.join(PACK_NAME));
        assert_eq!(
            fs::read_to_string(project.join("AGENTS.md"))?,
            "Project instructions\n"
        );
        assert_eq!(
            fs::read_to_string(project.join("rust-toolchain.toml"))?,
            "project toolchain\n"
        );
        assert_eq!(
            fs::read_to_string(
                destination.join("tools/reference-maintenance/rust-toolchain.toml")
            )?,
            "maintenance toolchain\n"
        );
        assert_eq!(
            fs::read(destination.join("LICENSE"))?,
            fs::read(source.join("LICENSE"))?
        );
        assert!(
            destination
                .join("tools/reference-maintenance/src/main.rs")
                .is_file()
        );
        assert!(
            destination
                .join("references/google-styleguide/.github")
                .is_dir()
        );
        assert!(
            destination
                .join("references/rust-api-guidelines/.gitignore")
                .is_file()
        );
        assert!(
            destination
                .join("references/rust-api-guidelines/tracked-source.py")
                .is_file()
        );
        for omitted in [
            "tools/reference-maintenance/.cache",
            "tools/reference-maintenance/.git",
            "tools/reference-maintenance/target",
            "tools/reference-maintenance/__pycache__",
            "tools/reference-maintenance/src/legacy.py",
            "tools/reference-maintenance/tests/legacy.pyc",
            "Cargo.toml",
            "Cargo.lock",
            "src",
            "tests",
            "scripts",
            "rust-toolchain.toml",
        ] {
            assert!(!destination.join(omitted).exists(), "Copied {omitted}");
        }
        assert_eq!(fs::read_dir(&project)?.count(), 3);
        Ok(())
    }

    #[test]
    fn existing_pack_is_not_overwritten() -> Result<()> {
        let temporary = tempfile::tempdir()?;
        let source = fixture(temporary.path())?;
        let project = temporary.path().join("project");
        fs::create_dir(&project)?;
        let destination = project.join(PACK_NAME);
        fs::create_dir(&destination)?;
        fs::write(destination.join("keep"), "keep")?;

        assert!(
            run(&source, &project)
                .unwrap_err()
                .to_string()
                .contains("already exists")
        );
        assert_eq!(fs::read_to_string(destination.join("keep"))?, "keep");
        Ok(())
    }

    #[test]
    fn project_must_exist_outside_source() -> Result<()> {
        let temporary = tempfile::tempdir()?;
        let source = fixture(temporary.path())?;
        let child = source.join("child");
        fs::create_dir(&child)?;
        for project in [&source, &child] {
            assert!(
                run(&source, project)
                    .unwrap_err()
                    .to_string()
                    .contains("outside")
            );
        }
        assert!(run(&source, &temporary.path().join("absent")).is_err());
        Ok(())
    }

    #[test]
    fn failed_integrity_check_leaves_no_partial_pack() -> Result<()> {
        let temporary = tempfile::tempdir()?;
        let source = fixture_with_references(temporary.path())?;
        // A corrupted snapshot file must fail the verification that runs before
        // the staged pack is installed.
        fs::write(
            source.join("references/semver/spec/v2.0.0.md"),
            "tampered\n",
        )?;
        let project = temporary.path().join("project");
        fs::create_dir(&project)?;

        let error = format!("{:#}", run(&source, &project).unwrap_err());

        assert!(error.contains("integrity check"), "{error}");
        assert!(error.contains("v2.0.0.md"), "{error}");
        assert_eq!(fs::read_dir(&project)?.count(), 0);
        Ok(())
    }

    #[test]
    fn unsupported_manifest_source_is_not_exported() -> Result<()> {
        let temporary = tempfile::tempdir()?;
        let source = fixture_with_references(temporary.path())?;
        let directory = source.join("references/extra-source");
        fs::create_dir(&directory)?;
        fs::write(directory.join("guide.md"), "unverified content\n")?;
        let mut manifest = manifest::read(&source)?;
        let mut extra = manifest.sources["semver"].clone();
        extra.files = BTreeMap::from([("guide.md".into(), "incorrect hash".into())]);
        manifest.sources.insert("extra-source".into(), extra);
        manifest::write(&source.join("references"), &manifest)?;
        let project = temporary.path().join("project");
        fs::create_dir(&project)?;

        let error = format!("{:#}", run(&source, &project).unwrap_err());

        assert!(error.contains("extra-source"), "{error}");
        assert_eq!(fs::read_dir(&project)?.count(), 0);
        Ok(())
    }

    #[test]
    fn missing_first_party_license_prevents_export() -> Result<()> {
        let temporary = tempfile::tempdir()?;
        let source = fixture_with_references(temporary.path())?;
        fs::remove_file(source.join("LICENSE"))?;
        let project = temporary.path().join("project");
        fs::create_dir(&project)?;

        let error = format!("{:#}", run(&source, &project).unwrap_err());

        assert!(error.contains("missing: LICENSE"), "{error}");
        assert_eq!(fs::read_dir(&project)?.count(), 0);
        Ok(())
    }

    /// A fixture whose `references/` is a verbatim copy of this repository's
    /// snapshots, so the exporter's integrity check passes.
    fn fixture_with_references(parent: &Path) -> Result<PathBuf> {
        let source = fixture(parent)?;
        fs::remove_dir_all(source.join("references"))?;
        copy_entry(
            &crate::reference_root().join("references"),
            &source.join("references"),
            true,
        )?;
        Ok(source)
    }

    #[test]
    fn export_skips_os_metadata_and_rejects_sync_leftovers() -> Result<()> {
        let temporary = tempfile::tempdir()?;
        let source = fixture_with_references(temporary.path())?;
        for metadata in [
            "references/.DS_Store",
            "references/semver/.DS_Store",
            "docs/.DS_Store",
        ] {
            fs::write(source.join(metadata), "finder")?;
        }
        let project = temporary.path().join("project");
        fs::create_dir(&project)?;

        let destination = run(&source, &project)?.destination;

        for metadata in [
            "references/.DS_Store",
            "references/semver/.DS_Store",
            "docs/.DS_Store",
        ] {
            assert!(!destination.join(metadata).exists(), "Copied {metadata}");
        }
        assert!(
            destination
                .join("references/semver/spec/v2.0.0.md")
                .is_file()
        );

        let other = temporary.path().join("other");
        fs::create_dir(&other)?;
        for leftover in ["references/.reference-sync-abc123", "references/.tmpXyZ123"] {
            let path = source.join(leftover);
            fs::create_dir(&path)?;
            let error = format!("{:#}", run(&source, &other).unwrap_err());
            assert!(error.contains("interrupted sync"), "{error}");
            fs::remove_dir(&path)?;
        }
        fs::create_dir(source.join("references/stale-source"))?;
        fs::write(
            source.join("references/stale-source/evil.md"),
            "no provenance",
        )?;
        let error = format!("{:#}", run(&source, &other).unwrap_err());
        assert!(error.contains("unmanaged"), "{error}");
        assert_eq!(fs::read_dir(&other)?.count(), 0);
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn export_errors_name_the_unreadable_file() -> Result<()> {
        use std::os::unix::fs::PermissionsExt;
        let temporary = tempfile::tempdir()?;
        let source = fixture_with_references(temporary.path())?;
        let unreadable = source.join("docs/policy.md");
        fs::write(&unreadable, "policy")?;
        fs::set_permissions(&unreadable, fs::Permissions::from_mode(0o000))?;
        let project = temporary.path().join("project");
        fs::create_dir(&project)?;

        let result = run(&source, &project);

        fs::set_permissions(&unreadable, fs::Permissions::from_mode(0o644))?;
        // A superuser can read the file regardless; there is nothing to assert.
        let Err(error) = result else { return Ok(()) };
        let message = format!("{error:#}");
        assert!(message.contains("policy.md"), "{message}");
        Ok(())
    }

    #[test]
    fn export_reports_pack_paths_the_host_ignores() -> Result<()> {
        let temporary = tempfile::tempdir()?;
        let source = fixture_with_references(temporary.path())?;
        fs::write(source.join("docs/policy.md"), "policy")?;
        let project = temporary.path().join("project");
        fs::create_dir(&project)?;
        let status = crate::git::command(Some(&project))
            .args(["init", "-q"])
            .status()?;
        assert!(status.success());
        fs::write(project.join(".gitignore"), "docs/\n")?;

        let export = run(&source, &project)?;

        assert_eq!(
            export.ignored_by_git,
            [".rust-reference/docs/policy.md"],
            "{:?}",
            export.ignored_by_git
        );
        assert!(export.destination.join("docs/policy.md").is_file());

        // Outside a Git work tree there is nothing to report.
        let plain = temporary.path().join("plain");
        fs::create_dir(&plain)?;
        assert!(run(&source, &plain)?.ignored_by_git.is_empty());
        Ok(())
    }

    #[test]
    fn export_checks_copied_ignore_rules_and_negations() -> Result<()> {
        let temporary = tempfile::tempdir()?;
        let source = fixture_with_references(temporary.path())?;
        for name in ["hidden.md", "visible.md", "host-visible.md"] {
            fs::write(source.join("docs").join(name), "policy\n")?;
        }
        fs::write(
            source.join(".gitignore"),
            "/docs/*.md\n!/docs/visible.md\n!/docs/host-visible.md\n",
        )?;
        let upstream = source.join("references/rust-api-guidelines");
        for name in ["export-hidden.md", "export-visible.md"] {
            fs::write(
                upstream.join("src").join(name),
                "tracked upstream content\n",
            )?;
        }
        let mut rules = fs::read_to_string(upstream.join(".gitignore"))?;
        rules.push_str("\n/src/export-*.md\n!/src/export-visible.md\n");
        fs::write(upstream.join(".gitignore"), rules)?;
        let mut manifest = manifest::read(&source)?;
        manifest
            .sources
            .get_mut("rust-api-guidelines")
            .unwrap()
            .files = manifest::inventory(&upstream)?;
        manifest::write(&source.join("references"), &manifest)?;
        let project = temporary.path().join("project");
        fs::create_dir(&project)?;
        let status = git::command(Some(&project)).args(["init", "-q"]).status()?;
        assert!(status.success());
        fs::write(
            project.join(".gitignore"),
            ".rust-reference/docs/host-visible.md\n",
        )?;

        let export = run(&source, &project)?;

        assert_eq!(
            export.ignored_by_git,
            [
                ".rust-reference/docs/hidden.md",
                ".rust-reference/references/rust-api-guidelines/src/export-hidden.md",
            ]
        );
        for name in ["hidden.md", "visible.md", "host-visible.md"] {
            assert!(export.destination.join("docs").join(name).is_file());
        }
        Ok(())
    }

    #[test]
    fn export_carries_root_gitattributes() -> Result<()> {
        let temporary = tempfile::tempdir()?;
        let source = fixture_with_references(temporary.path())?;
        let attributes = "references/** -text eol=lf\n";
        fs::write(source.join(".gitattributes"), attributes)?;
        let project = temporary.path().join("project");
        fs::create_dir(&project)?;

        let destination = run(&source, &project)?.destination;

        assert_eq!(
            fs::read_to_string(destination.join(".gitattributes"))?,
            attributes
        );
        Ok(())
    }
}
