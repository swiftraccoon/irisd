//! Maintain checked-in source snapshots and their integrity manifest.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::Path;

use anyhow::{Context, Result, bail, ensure};

use crate::manifest::{self, Manifest, Source};
use crate::{git, sources, web};

/// Select verification, pinned restoration, and overwrite behavior.
#[derive(Clone, Copy, Debug, Default)]
pub struct Options {
    pub check: bool,
    pub locked: bool,
    pub force: bool,
}

/// How a snapshot directory differs from its manifest entry.
#[derive(Debug, Default)]
struct Differences {
    /// Recorded files that are absent from the directory.
    missing: Vec<String>,
    /// Files whose content differs from the record, or that the record lacks.
    changed: Vec<String>,
}

impl Differences {
    fn is_empty(&self) -> bool {
        self.missing.is_empty() && self.changed.is_empty()
    }

    fn summary(&self) -> String {
        summarize(self.changed.iter().chain(&self.missing))
    }
}

fn summarize<'a>(paths: impl Iterator<Item = &'a String>) -> String {
    paths.take(8).cloned().collect::<Vec<_>>().join(", ")
}

fn differences(directory: &Path, expected: &BTreeMap<String, String>) -> Result<Differences> {
    let actual = if fs::symlink_metadata(directory).is_ok() {
        manifest::inventory(directory)?
    } else {
        BTreeMap::new()
    };
    let mut differences = Differences::default();
    for (path, hash) in expected {
        match actual.get(path) {
            None => differences.missing.push(path.clone()),
            Some(found) if found != hash => differences.changed.push(path.clone()),
            Some(_) => {}
        }
    }
    differences.changed.extend(
        actual
            .keys()
            .filter(|path| !expected.contains_key(*path))
            .cloned(),
    );
    differences.changed.sort();
    Ok(differences)
}

fn promote(references: &Path, staging: &Path, ids: &[String], manifest: &Manifest) -> Result<()> {
    let mut promoted = Vec::new();
    let mut backups = Vec::new();
    let result: Result<()> = (|| {
        for id in ids {
            let destination = references.join(id);
            if fs::symlink_metadata(&destination).is_ok() {
                fs::rename(&destination, staging.join(format!("{id}.backup")))?;
                backups.push(id);
            }
            fs::rename(staging.join(id), destination)?;
            promoted.push(id);
        }
        manifest::write(references, manifest)
    })();
    if let Err(error) = result {
        let mut failures = Vec::new();
        // Move fetched content back into staging so the retained directory
        // still holds everything needed for recovery.
        for id in promoted.into_iter().rev() {
            if let Err(error) = fs::rename(references.join(id), staging.join(id)) {
                failures.push(format!("withdraw {id}: {error}"));
            }
        }
        for id in backups {
            if let Err(error) =
                fs::rename(staging.join(format!("{id}.backup")), references.join(id))
            {
                failures.push(format!("restore {id}: {error}"));
            }
        }
        if !failures.is_empty() {
            bail!(
                "snapshot promotion failed: {error}; rollback errors: {}",
                failures.join("; ")
            );
        }
        return Err(error.context("snapshot promotion failed; restored previous snapshots"));
    }
    Ok(())
}

fn run_with_sources(
    root: &Path,
    source_ids: &[String],
    options: Options,
    configs: &BTreeMap<String, Source>,
) -> Result<()> {
    ensure!(
        !(options.check && options.locked),
        "--check and --locked cannot be combined"
    );
    let ids: Vec<_> = if source_ids.is_empty() {
        configs.keys().cloned().collect()
    } else {
        let mut seen = BTreeSet::new();
        source_ids
            .iter()
            .filter(|id| seen.insert((*id).clone()))
            .cloned()
            .collect()
    };
    for id in &ids {
        ensure!(
            configs.contains_key(id),
            "unknown source {id}; choose: {}",
            configs.keys().cloned().collect::<Vec<_>>().join(", ")
        );
    }
    let references = root.join("references");
    if let Ok(metadata) = fs::symlink_metadata(&references) {
        ensure!(
            metadata.is_dir(),
            "references/ must be a directory, not a symlink or file"
        );
    }
    let mut manifest = manifest::read(root)?;
    for id in manifest.sources.keys() {
        ensure!(
            configs.contains_key(id),
            "unsupported source in manifest: {id}; remove it or add a source definition before syncing"
        );
    }
    let mut work = Vec::new();
    for id in &ids {
        let destination = references.join(id);
        let config = &configs[id];
        let old = manifest.sources.get(id);
        if options.check {
            let old = old.with_context(|| format!("{id}: missing manifest entry"))?;
            let drift = old.provenance_drift(config);
            ensure!(
                drift.is_empty(),
                "{id}: manifest provenance differs from the source configuration ({}); \
                 run sync {id} to refresh",
                drift.join(", ")
            );
            let differences = differences(&destination, &old.files)?;
            ensure!(
                differences.is_empty(),
                "{id}: snapshot differs: {}",
                differences.summary()
            );
            crate::report(format_args!("OK {id}: {} files", old.files.len()))?;
            continue;
        }
        if options.locked {
            let old =
                old.with_context(|| format!("{id}: --locked requires an existing manifest entry"))?;
            if !options.force {
                let differences = differences(&destination, &old.files)?;
                if differences.is_empty() && old.provenance_drift(config).is_empty() {
                    crate::report(format_args!("Unchanged {id}: {} files", old.files.len()))?;
                    continue;
                }
                // Restoring recreates missing files; edits and additions are local
                // work that only --force may discard.
                ensure!(
                    differences.changed.is_empty(),
                    "{id}: local changes detected ({}); preserve them or use --force to replace",
                    summarize(differences.changed.iter())
                );
            }
        } else if !options.force {
            let local = if let Some(old) = old {
                let differences = differences(&destination, &old.files)?;
                (!differences.is_empty()).then(|| differences.summary())
            } else if fs::symlink_metadata(&destination).is_ok() {
                Some("unmanaged snapshot directory".to_owned())
            } else {
                None
            };
            if let Some(local) = local {
                bail!(
                    "{id}: local changes detected ({local}); preserve them or use --force to replace"
                );
            }
        }
        work.push(id.clone());
    }
    if options.check || work.is_empty() {
        return Ok(());
    }
    fs::create_dir_all(&references)?;
    let staging = tempfile::Builder::new()
        .prefix(".reference-sync-")
        .tempdir_in(&references)?;
    let mut pending = BTreeMap::new();
    for id in &work {
        let destination = staging.path().join(id);
        fs::create_dir(&destination)?;
        let config = &configs[id];
        let old = if options.locked {
            manifest.sources.get(id)
        } else {
            None
        };
        // A locked restore reproduces the recorded content; descriptive metadata
        // such as the license text comes from the current configuration.
        let pinned = old.map(|old| Source {
            license: config.license.clone(),
            ..old.clone()
        });
        let retrieval = pinned.as_ref().unwrap_or(config);
        let mut source = match retrieval.kind.as_str() {
            "git" => git::snapshot(root, id, retrieval, &destination, old)?,
            "web" => web::snapshot(root, retrieval, &destination, old)?,
            kind => bail!("{id}: unsupported source kind {kind}"),
        };
        source.retrieved_at = match old {
            Some(old) => manifest::canonical_timestamp(&old.retrieved_at)?,
            None => manifest::timestamp_now()?,
        };
        source.files = manifest::inventory(&destination)?;
        if let Some(old) = old {
            // Content and provenance must reproduce the record; descriptive and
            // derived fields are refreshed from the current tool.
            let expected = Source {
                license: source.license.clone(),
                transformations: source.transformations.clone(),
                retrieved_at: source.retrieved_at.clone(),
                ..old.clone()
            };
            ensure!(
                source == expected,
                "{id}: pinned snapshot does not match manifest"
            );
        }
        pending.insert(id.clone(), source);
    }
    for (id, source) in &pending {
        manifest.sources.insert(id.clone(), source.clone());
    }
    if let Err(error) = promote(&references, staging.path(), &work, &manifest) {
        let recovery = staging.keep();
        return Err(error.context(format!(
            "staging retained for recovery at {}",
            recovery.display()
        )));
    }
    let verb = if options.locked {
        "Restored"
    } else {
        "Updated"
    };
    for (id, source) in pending {
        let revision = source.commit.as_deref().unwrap_or("web");
        crate::report(format_args!(
            "{verb} {id}: {}, {} files",
            &revision[..revision.len().min(12)],
            source.files.len()
        ))?;
    }
    Ok(())
}

/// Refresh sources, restore pinned revisions, or check their local integrity.
/// An empty source list selects every supported source.
///
/// # Errors
/// Returns an error for unknown sources, local modifications, missing pinned
/// content, failed upstream retrieval, or an invalid manifest.
pub fn run(root: &Path, source_ids: &[String], options: Options) -> Result<()> {
    run_with_sources(root, source_ids, options, &sources::all())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;
    use tempfile::TempDir;

    fn git(directory: &Path, args: &[&str]) -> Result<()> {
        let output = Command::new("git")
            .args([
                "-c",
                "commit.gpgsign=false",
                "-c",
                "core.fsmonitor=false",
                "-c",
                "core.hooksPath=/dev/null",
            ])
            .args(args)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env(
                "GIT_CONFIG_GLOBAL",
                if cfg!(windows) { "NUL" } else { "/dev/null" },
            )
            .current_dir(directory)
            .output()?;
        ensure!(
            output.status.success(),
            "fixture Git failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        Ok(())
    }

    struct Fixture {
        temporary: TempDir,
        origin: std::path::PathBuf,
        configs: BTreeMap<String, Source>,
    }

    impl Fixture {
        fn new() -> Result<Self> {
            let temporary = tempfile::tempdir()?;
            let origin = temporary.path().join("origin");
            fs::create_dir_all(origin.join("guide"))?;
            git(&origin, &["init", "-b", "main"])?;
            git(&origin, &["config", "user.email", "test@example.com"])?;
            git(&origin, &["config", "user.name", "Reference tests"])?;
            fs::write(origin.join("guide/readme.md"), "first version\n")?;
            fs::write(origin.join("LICENSE"), "fixture license\n")?;
            fs::write(origin.join("excluded.txt"), "not selected\n")?;
            fs::write(
                origin.join(".gitattributes"),
                "guide/readme.md export-ignore\n",
            )?;
            git(&origin, &["add", "."])?;
            git(&origin, &["commit", "-m", "fixture"])?;
            let mut source = sources::all()
                .remove("rust-style-guide")
                .context("Rust config missing")?;
            source.url = origin
                .to_str()
                .context("fixture path must be UTF-8")?
                .into();
            source.branch = Some("main".into());
            source.selection = vec!["guide".into(), "LICENSE".into()];
            let configs = BTreeMap::from([("fixture".into(), source)]);
            Ok(Self {
                temporary,
                origin,
                configs,
            })
        }

        fn root(&self) -> &Path {
            self.temporary.path()
        }

        fn destination(&self) -> std::path::PathBuf {
            self.root().join("references/fixture")
        }

        fn run(&self, options: Options) -> Result<()> {
            run_with_sources(self.root(), &[], options, &self.configs)
        }

        fn commit(&self) -> Result<()> {
            git(&self.origin, &["add", "."])?;
            git(&self.origin, &["commit", "-m", "update"])
        }
    }

    #[test]
    fn refresh_selection_and_locked_restore() -> Result<()> {
        let fixture = Fixture::new()?;
        fixture.run(Options::default())?;
        let old = fs::read(fixture.root().join("references/manifest.json"))?;
        assert_eq!(
            fs::read_to_string(fixture.destination().join("guide/readme.md"))?,
            "first version\n"
        );
        assert!(!fixture.destination().join("excluded.txt").exists());
        assert!(!fixture.destination().join(".git").exists());
        fs::write(fixture.origin.join("guide/readme.md"), "second version\n")?;
        fixture.commit()?;
        fixture.run(Options::default())?;
        assert_eq!(
            fs::read_to_string(fixture.destination().join("guide/readme.md"))?,
            "second version\n"
        );
        fs::write(fixture.root().join("references/manifest.json"), old)?;
        fixture.run(Options {
            locked: true,
            force: true,
            ..Options::default()
        })?;
        assert_eq!(
            fs::read_to_string(fixture.destination().join("guide/readme.md"))?,
            "first version\n"
        );
        fixture.run(Options {
            check: true,
            ..Options::default()
        })?;
        Ok(())
    }

    /// Apply extra Git configuration to every Git command the tool runs on this
    /// thread for the duration of the returned guard.
    struct GitConfigGuard;

    impl GitConfigGuard {
        fn new(entries: &[String]) -> Self {
            git::TEST_GIT_CONFIG.with_borrow_mut(|config| config.extend_from_slice(entries));
            Self
        }
    }

    impl Drop for GitConfigGuard {
        fn drop(&mut self) {
            git::TEST_GIT_CONFIG.with_borrow_mut(Vec::clear);
        }
    }

    #[test]
    fn refresh_survives_insteadof_rewrite_and_default_remote_name() -> Result<()> {
        let mut fixture = Fixture::new()?;
        let real_base =
            format!("{}/", fixture.root().display()).replace(std::path::MAIN_SEPARATOR, "/");
        fixture
            .configs
            .get_mut("fixture")
            .context("fixture config")?
            .url = "https://alias.invalid/origin".into();
        let _guard = GitConfigGuard::new(&[
            format!("url.{real_base}.insteadOf=https://alias.invalid/"),
            "clone.defaultRemoteName=upstream".into(),
        ]);

        fixture.run(Options::default())?;

        assert_eq!(
            fs::read_to_string(fixture.destination().join("guide/readme.md"))?,
            "first version\n"
        );
        let cache_config =
            fs::read_to_string(fixture.root().join(".cache/upstreams/fixture/.git/config"))?;
        assert!(
            cache_config.contains("[remote \"origin\"]"),
            "{cache_config}"
        );
        assert!(
            cache_config.contains("https://alias.invalid/origin"),
            "{cache_config}"
        );
        Ok(())
    }

    #[test]
    fn locked_restore_without_cache_survives_renamed_branch() -> Result<()> {
        let mut fixture = Fixture::new()?;
        // A file URL gives a real shallow clone, so the pinned commit must be
        // fetched by hash rather than found in a full local copy.
        let origin = fixture
            .origin
            .to_str()
            .context("Fixture path must be UTF-8")?
            .replace(std::path::MAIN_SEPARATOR, "/");
        let prefix = if origin.starts_with('/') {
            "file://"
        } else {
            "file:///"
        };
        fixture
            .configs
            .get_mut("fixture")
            .context("fixture config")?
            .url = format!("{prefix}{origin}");
        fixture.run(Options::default())?;
        let pinned = fs::read(fixture.root().join("references/manifest.json"))?;
        fs::write(fixture.origin.join("guide/readme.md"), "second version\n")?;
        fixture.commit()?;
        fixture.run(Options::default())?;
        fs::write(fixture.root().join("references/manifest.json"), pinned)?;
        fs::remove_dir_all(fixture.root().join(".cache"))?;
        git(&fixture.origin, &["branch", "-m", "main", "trunk"])?;

        fixture.run(Options {
            locked: true,
            force: true,
            ..Options::default()
        })?;

        assert_eq!(
            fs::read_to_string(fixture.destination().join("guide/readme.md"))?,
            "first version\n"
        );
        Ok(())
    }

    #[test]
    fn detects_added_changed_and_missing_files() -> Result<()> {
        let fixture = Fixture::new()?;
        fixture.run(Options::default())?;
        fs::write(fixture.destination().join("extra.txt"), "local work")?;
        assert!(
            fixture
                .run(Options {
                    check: true,
                    ..Options::default()
                })
                .is_err()
        );
        assert!(fixture.run(Options::default()).is_err());
        fixture.run(Options {
            locked: true,
            force: true,
            ..Options::default()
        })?;
        assert!(!fixture.destination().join("extra.txt").exists());
        fs::write(fixture.destination().join("guide/readme.md"), "changed")?;
        assert!(
            fixture
                .run(Options {
                    check: true,
                    ..Options::default()
                })
                .is_err()
        );
        fs::remove_file(fixture.destination().join("LICENSE"))?;
        assert!(fixture.run(Options::default()).is_err());
        Ok(())
    }

    #[test]
    fn failed_later_source_leaves_earlier_snapshot_untouched() -> Result<()> {
        let mut fixture = Fixture::new()?;
        fixture.run(Options::default())?;
        fs::write(fixture.origin.join("guide/readme.md"), "update\n")?;
        fixture.commit()?;
        let mut broken = fixture.configs["fixture"].clone();
        broken.url = fixture
            .root()
            .join("missing")
            .to_str()
            .context("fixture path")?
            .into();
        fixture.configs.insert("later-broken".into(), broken);
        assert!(fixture.run(Options::default()).is_err());
        assert_eq!(
            fs::read_to_string(fixture.destination().join("guide/readme.md"))?,
            "first version\n"
        );
        assert!(!fixture.root().join("references/later-broken").exists());
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn materializes_symlinks_and_rejects_escape() -> Result<()> {
        let fixture = Fixture::new()?;
        std::os::unix::fs::symlink("readme.md", fixture.origin.join("guide/alias.md"))?;
        fixture.commit()?;
        fixture.run(Options::default())?;
        assert!(!fs::symlink_metadata(fixture.destination().join("guide/alias.md"))?.is_symlink());
        assert_eq!(
            fs::read_to_string(fixture.destination().join("guide/alias.md"))?,
            "first version\n"
        );
        fs::write(fixture.root().join("outside"), "must not copy")?;
        let target = fixture.root().join("outside");
        std::os::unix::fs::symlink(target, fixture.origin.join("guide/escape.md"))?;
        fixture.commit()?;
        assert!(fixture.run(Options::default()).is_err());
        assert!(!fixture.destination().join("guide/escape.md").exists());
        Ok(())
    }

    #[test]
    fn locked_restores_missing_files_without_force() -> Result<()> {
        let fixture = Fixture::new()?;
        fixture.run(Options::default())?;
        let locked = Options {
            locked: true,
            ..Options::default()
        };

        fs::remove_file(fixture.destination().join("LICENSE"))?;
        fixture.run(locked)?;
        assert_eq!(
            fs::read_to_string(fixture.destination().join("LICENSE"))?,
            "fixture license\n"
        );

        fs::remove_dir_all(fixture.destination())?;
        fixture.run(locked)?;
        assert_eq!(
            fs::read_to_string(fixture.destination().join("guide/readme.md"))?,
            "first version\n"
        );

        fs::write(fixture.destination().join("guide/readme.md"), "edited")?;
        let error = format!("{:#}", fixture.run(locked).unwrap_err());
        assert!(error.contains("--force"), "{error}");
        assert_eq!(
            fs::read_to_string(fixture.destination().join("guide/readme.md"))?,
            "edited"
        );
        fs::write(
            fixture.destination().join("guide/readme.md"),
            "first version\n",
        )?;

        fs::write(fixture.destination().join("extra.txt"), "added")?;
        let error = format!("{:#}", fixture.run(locked).unwrap_err());
        assert!(error.contains("extra.txt"), "{error}");
        Ok(())
    }

    #[test]
    fn locked_skips_verified_sources_without_touching_upstream() -> Result<()> {
        let fixture = Fixture::new()?;
        fixture.run(Options::default())?;
        let manifest_path = fixture.root().join("references/manifest.json");
        let before = fs::read(&manifest_path)?;
        fs::remove_dir_all(fixture.root().join(".cache"))?;
        let moved = fixture.root().join("origin-moved");
        fs::rename(&fixture.origin, &moved)?;

        let result = fixture.run(Options {
            locked: true,
            ..Options::default()
        });

        fs::rename(&moved, &fixture.origin)?;
        result?;
        assert_eq!(fs::read(&manifest_path)?, before);
        assert!(!fixture.root().join(".cache").exists());
        Ok(())
    }

    #[test]
    fn check_rejects_manifest_provenance_drift() -> Result<()> {
        let fixture = Fixture::new()?;
        fixture.run(Options::default())?;
        let mut recorded = manifest::read(fixture.root())?;
        recorded
            .sources
            .get_mut("fixture")
            .context("fixture entry")?
            .license = "TAMPERED".into();
        manifest::write(&fixture.root().join("references"), &recorded)?;

        let error = format!(
            "{:#}",
            fixture
                .run(Options {
                    check: true,
                    ..Options::default()
                })
                .unwrap_err()
        );

        assert!(error.contains("provenance"), "{error}");
        assert!(error.contains("license"), "{error}");
        Ok(())
    }

    #[test]
    fn every_mode_rejects_unsupported_manifest_sources_before_changes() -> Result<()> {
        let fixture = Fixture::new()?;
        fixture.run(Options::default())?;
        let mut recorded = manifest::read(fixture.root())?;
        recorded
            .sources
            .insert("unsupported".into(), recorded.sources["fixture"].clone());
        manifest::write(&fixture.root().join("references"), &recorded)?;
        let manifest_before = fs::read(fixture.root().join("references/manifest.json"))?;
        let snapshot_before = manifest::inventory(&fixture.destination())?;

        for options in [
            Options::default(),
            Options {
                check: true,
                ..Options::default()
            },
            Options {
                locked: true,
                ..Options::default()
            },
            Options {
                locked: true,
                force: true,
                ..Options::default()
            },
        ] {
            let error = run_with_sources(
                fixture.root(),
                &["fixture".into()],
                options,
                &fixture.configs,
            )
            .unwrap_err();
            assert!(error.to_string().contains("unsupported source in manifest"));
            assert_eq!(
                manifest::inventory(&fixture.destination())?,
                snapshot_before
            );
            assert_eq!(
                fs::read(fixture.root().join("references/manifest.json"))?,
                manifest_before
            );
        }
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn forced_locked_restore_replaces_links_without_changing_their_targets() -> Result<()> {
        let fixture = Fixture::new()?;
        fixture.run(Options::default())?;
        let outside = fixture.root().join("outside");
        fs::create_dir(&outside)?;
        let outside_file = outside.join("keep.md");
        fs::write(&outside_file, "external content\n")?;
        let outside_before = manifest::inventory(&outside)?;
        let file = fixture.destination().join("guide/readme.md");
        fs::remove_file(&file)?;
        std::os::unix::fs::symlink(&outside_file, &file)?;
        let forced = Options {
            locked: true,
            force: true,
            ..Options::default()
        };

        assert!(
            fixture
                .run(Options {
                    force: false,
                    ..forced
                })
                .is_err()
        );
        fixture.run(forced)?;
        assert_eq!(fs::read_to_string(&file)?, "first version\n");
        assert!(!fs::symlink_metadata(&file)?.is_symlink());
        assert_eq!(manifest::inventory(&outside)?, outside_before);

        fs::remove_dir_all(fixture.destination())?;
        std::os::unix::fs::symlink(&outside, fixture.destination())?;
        fixture.run(forced)?;
        assert!(fs::symlink_metadata(fixture.destination())?.is_dir());
        assert_eq!(fs::read_to_string(&file)?, "first version\n");
        assert_eq!(manifest::inventory(&outside)?, outside_before);
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn rejects_a_linked_references_directory_before_promotion() -> Result<()> {
        let fixture = Fixture::new()?;
        fixture.run(Options::default())?;
        let references = fixture.root().join("references");
        let outside = fixture.root().join("outside");
        fs::rename(&references, &outside)?;
        std::os::unix::fs::symlink(&outside, &references)?;
        let before = manifest::inventory(&outside)?;

        let error = fixture
            .run(Options {
                locked: true,
                force: true,
                ..Options::default()
            })
            .unwrap_err();

        assert!(
            error
                .to_string()
                .contains("references/ must be a directory")
        );
        assert!(fs::symlink_metadata(&references)?.is_symlink());
        assert_eq!(manifest::inventory(&outside)?, before);
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn forced_locked_web_restore_uses_only_intact_local_html() -> Result<()> {
        let root = tempfile::tempdir()?;
        let id = "google-timeless-documentation";
        let config = sources::all().remove(id).context("web config")?;
        let mut pinned = manifest::read(crate::reference_root())?
            .sources
            .remove(id)
            .context("web record")?;
        pinned.license.clone_from(&config.license);
        let configs = BTreeMap::from([(id.into(), config)]);
        let references = root.path().join("references");
        let destination = references.join(id);
        fs::create_dir_all(&destination)?;
        for name in pinned.files.keys() {
            fs::copy(
                crate::reference_root()
                    .join("references")
                    .join(id)
                    .join(name),
                destination.join(name),
            )?;
        }
        let raw = fs::read(destination.join("source.html"))?;
        manifest::write(
            &references,
            &Manifest {
                sources: BTreeMap::from([(id.into(), pinned.clone())]),
                ..Manifest::default()
            },
        )?;
        let outside = root.path().join("outside");
        fs::create_dir(&outside)?;
        fs::write(outside.join("source.html"), &raw)?;
        fs::write(outside.join("keep.md"), "external content\n")?;
        let outside_before = manifest::inventory(&outside)?;
        let generated = destination.join("timeless-documentation.md");
        fs::remove_file(&generated)?;
        std::os::unix::fs::symlink(outside.join("keep.md"), &generated)?;
        let forced = Options {
            locked: true,
            force: true,
            ..Options::default()
        };

        run_with_sources(root.path(), &[], forced, &configs)?;
        assert_eq!(manifest::inventory(&destination)?, pinned.files);
        assert_eq!(manifest::inventory(&outside)?, outside_before);

        fs::remove_file(destination.join("source.html"))?;
        std::os::unix::fs::symlink(outside.join("source.html"), destination.join("source.html"))?;
        let error = run_with_sources(root.path(), &[], forced, &configs).unwrap_err();
        assert!(error.to_string().contains("unavailable locally"));
        assert_eq!(manifest::inventory(&outside)?, outside_before);

        fs::remove_dir_all(&destination)?;
        std::os::unix::fs::symlink(&outside, &destination)?;
        let error = run_with_sources(root.path(), &[], forced, &configs).unwrap_err();
        assert!(error.to_string().contains("unavailable locally"));
        assert!(fs::symlink_metadata(&destination)?.is_symlink());
        assert_eq!(manifest::inventory(&outside)?, outside_before);

        let cache = root.path().join(".cache/web");
        fs::create_dir_all(&cache)?;
        fs::write(cache.join(format!("{}.html", manifest::digest(&raw))), &raw)?;
        run_with_sources(root.path(), &[], forced, &configs)?;
        assert_eq!(manifest::inventory(&destination)?, pinned.files);
        assert_eq!(manifest::inventory(&outside)?, outside_before);
        Ok(())
    }

    #[test]
    fn locked_refreshes_license_from_configuration() -> Result<()> {
        let mut fixture = Fixture::new()?;
        fixture.run(Options::default())?;
        fixture
            .configs
            .get_mut("fixture")
            .context("fixture config")?
            .license = "Updated license text".into();

        fixture.run(Options {
            locked: true,
            ..Options::default()
        })?;

        assert_eq!(
            manifest::read(fixture.root())?.sources["fixture"].license,
            "Updated license text"
        );
        fixture.run(Options {
            check: true,
            ..Options::default()
        })?;
        Ok(())
    }

    #[test]
    fn refresh_rejects_selection_paths_missing_upstream() -> Result<()> {
        let fixture = Fixture::new()?;
        fixture.run(Options::default())?;
        git(&fixture.origin, &["mv", "LICENSE", "LICENSE.txt"])?;
        fixture.commit()?;

        let error = format!("{:#}", fixture.run(Options::default()).unwrap_err());

        assert!(error.contains("LICENSE"), "{error}");
        assert_eq!(
            fs::read_to_string(fixture.destination().join("LICENSE"))?,
            "fixture license\n"
        );
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn materializes_symlinks_in_empty_directories_and_chains() -> Result<()> {
        let fixture = Fixture::new()?;
        fs::create_dir(fixture.origin.join("guide/links"))?;
        std::os::unix::fs::symlink("../readme.md", fixture.origin.join("guide/links/alias.md"))?;
        std::os::unix::fs::symlink("b.md", fixture.origin.join("guide/a.md"))?;
        std::os::unix::fs::symlink("readme.md", fixture.origin.join("guide/b.md"))?;
        fixture.commit()?;

        fixture.run(Options::default())?;

        for path in ["guide/links/alias.md", "guide/a.md", "guide/b.md"] {
            assert_eq!(
                fs::read_to_string(fixture.destination().join(path))?,
                "first version\n",
                "{path}"
            );
        }
        let recorded = manifest::read(fixture.root())?;
        assert_eq!(
            recorded.sources["fixture"].transformations,
            [git::SYMLINK_TRANSFORMATION]
        );
        Ok(())
    }

    #[test]
    fn rejects_unknown_ids_and_preserves_unmanaged_directories() -> Result<()> {
        let fixture = Fixture::new()?;
        assert!(
            run_with_sources(
                fixture.root(),
                &["unknown".into()],
                Options::default(),
                &fixture.configs
            )
            .is_err()
        );
        fs::create_dir_all(fixture.destination())?;
        fs::write(fixture.destination().join("keep.md"), "local")?;
        assert!(fixture.run(Options::default()).is_err());
        assert_eq!(
            fs::read_to_string(fixture.destination().join("keep.md"))?,
            "local"
        );
        Ok(())
    }

    #[test]
    fn failed_manifest_promotion_restores_replaced_snapshot() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let references = directory.path().join("references");
        let staging = directory.path().join("staging");
        fs::create_dir_all(references.join("fixture"))?;
        fs::write(references.join("fixture/guide.md"), "original")?;
        fs::create_dir_all(staging.join("fixture"))?;
        fs::write(staging.join("fixture/guide.md"), "replacement")?;
        // A directory at the manifest path makes the atomic file replacement fail.
        fs::create_dir(references.join("manifest.json"))?;
        assert!(
            promote(
                &references,
                &staging,
                &["fixture".into()],
                &Manifest::default()
            )
            .is_err()
        );
        assert_eq!(
            fs::read_to_string(references.join("fixture/guide.md"))?,
            "original"
        );
        // The fetched content moves back into staging instead of being deleted,
        // so the retained staging directory really is usable for recovery.
        assert_eq!(
            fs::read_to_string(staging.join("fixture/guide.md"))?,
            "replacement"
        );
        Ok(())
    }
}
