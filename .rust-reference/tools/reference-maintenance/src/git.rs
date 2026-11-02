//! Export pinned Git source files without copying repository metadata.

use std::fs;
use std::io::{BufRead, Cursor, Read, Write};
use std::path::{Component, Path};
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail, ensure};

use crate::manifest::Source;

/// Recorded in a source's `transformations` when its tree held symlinks.
pub const SYMLINK_TRANSFORMATION: &str =
    "Tracked symlinks are materialized as copies of their tracked targets";

#[cfg(test)]
thread_local! {
    /// Extra `key=value` Git configuration that tests on this thread apply to
    /// every Git command the tool runs, standing in for a user's global config.
    pub(crate) static TEST_GIT_CONFIG: std::cell::RefCell<Vec<String>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

pub(crate) fn command(directory: Option<&Path>) -> Command {
    let mut command = Command::new("git");
    // These cache operations have no working tree to monitor.
    command.args(["-c", "core.fsmonitor=false"]);
    if let Some(directory) = directory {
        command.current_dir(directory);
    }
    #[cfg(test)]
    {
        command.env("GIT_CONFIG_NOSYSTEM", "1").env(
            "GIT_CONFIG_GLOBAL",
            if cfg!(windows) { "NUL" } else { "/dev/null" },
        );
        TEST_GIT_CONFIG.with_borrow(|entries| {
            for entry in entries {
                command.args(["-c", entry]);
            }
        });
    }
    command
}

fn output(directory: Option<&Path>, arguments: &[&str]) -> Result<Vec<u8>> {
    let output = command(directory)
        .args(arguments)
        .output()
        .with_context(|| format!("run git {}", arguments.join(" ")))?;
    ensure!(
        output.status.success(),
        "git {} failed: {}",
        arguments.join(" "),
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Ok(output.stdout)
}

struct Entry {
    mode: String,
    object_id: String,
    name: String,
}

/// Write the selected tree at `commit` into `destination`, returning whether any
/// tracked symlink had to be materialized as a copy of its target.
fn export(cache: &Path, commit: &str, selection: &[String], destination: &Path) -> Result<bool> {
    let mut arguments = vec!["ls-tree", "-r", "-z", commit, "--"];
    arguments.extend(selection.iter().map(String::as_str));
    let tree = output(Some(cache), &arguments)?;
    let mut entries = Vec::new();
    for record in tree
        .split(|byte| *byte == 0)
        .filter(|record| !record.is_empty())
    {
        let record = std::str::from_utf8(record).context("Git source filenames must be UTF-8")?;
        let (metadata, name) = record
            .split_once('\t')
            .context("malformed Git tree entry")?;
        let metadata: Vec<_> = metadata.split_whitespace().collect();
        ensure!(
            metadata.len() == 3
                && metadata[1] == "blob"
                && matches!(metadata[0], "100644" | "100755" | "120000")
                && !name.is_empty()
                && Path::new(name)
                    .components()
                    .all(|part| matches!(part, Component::Normal(_))),
            "unsupported Git entry: {record}"
        );
        entries.push(Entry {
            mode: metadata[0].into(),
            object_id: metadata[2].into(),
            name: name.into(),
        });
    }
    ensure!(!entries.is_empty(), "no tracked source files found");
    // `ls-tree` silently omits pathspecs that no longer exist, so a renamed
    // license file or moved directory must not shrink the snapshot unnoticed.
    for selected in selection {
        let prefix = format!("{}/", selected.trim_end_matches('/'));
        ensure!(
            entries
                .iter()
                .any(|entry| entry.name == *selected || entry.name.starts_with(&prefix)),
            "selected path {selected} is missing at commit {commit}; update the source selection"
        );
    }

    // Batch reads preserve tracked files marked export-ignore and request only
    // selected blobs from partial clones of the Rust compiler repository.
    let input: String = entries
        .iter()
        .map(|entry| format!("{}\n", entry.object_id))
        .collect();
    let mut child = command(Some(cache))
        .args(["cat-file", "--batch"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("start Git blob export")?;
    let mut stdin = child.stdin.take().context("open Git batch input")?;
    let writer = std::thread::spawn(move || stdin.write_all(input.as_bytes()));
    let output = child.wait_with_output().context("read Git blob export")?;
    writer
        .join()
        .map_err(|_| anyhow::anyhow!("Git input writer panicked"))??;
    ensure!(
        output.status.success(),
        "Git blob export failed: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    );
    let mut blobs = Cursor::new(output.stdout);
    let mut links = Vec::new();
    for entry in entries {
        let mut header = String::new();
        blobs.read_line(&mut header)?;
        let header: Vec<_> = header.split_whitespace().collect();
        ensure!(
            header.len() == 3 && header[0] == entry.object_id && header[1] == "blob",
            "missing Git blob: {}",
            entry.object_id
        );
        let size = header[2].parse().context("invalid Git blob size")?;
        let mut content = vec![0; size];
        blobs.read_exact(&mut content)?;
        let mut separator = [0];
        blobs.read_exact(&mut separator)?;
        ensure!(separator == *b"\n", "malformed Git batch response");
        let path = destination.join(&entry.name);
        if entry.mode == "120000" {
            links.push((
                path,
                String::from_utf8(content).context("non-UTF-8 symlink target")?,
            ));
        } else {
            fs::create_dir_all(path.parent().context("Git file has no parent")?)?;
            fs::write(&path, content)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(
                    &path,
                    fs::Permissions::from_mode(if entry.mode == "100755" { 0o755 } else { 0o644 }),
                )?;
            }
        }
    }
    let destination = destination.canonicalize()?;
    let materialized = !links.is_empty();
    // A symlink may point at another symlink that appears later in the tree,
    // so resolve in passes until every target exists or no pass makes progress.
    let mut pending = links;
    while !pending.is_empty() {
        let attempted = pending.len();
        let mut unresolved = Vec::new();
        for (path, target) in pending {
            let parent = path.parent().context("symlink has no parent")?;
            fs::create_dir_all(parent)?;
            let Ok(source) = parent.join(&target).canonicalize() else {
                unresolved.push((path, target));
                continue;
            };
            ensure!(
                source.starts_with(&destination) && source.is_file(),
                "symlink target outside snapshot or unavailable: {} -> {target}",
                path.display()
            );
            fs::copy(source, &path)?;
        }
        if unresolved.len() == attempted {
            let (path, target) = &unresolved[0];
            bail!("symlink target unavailable: {} -> {target}", path.display());
        }
        pending = unresolved;
    }
    Ok(materialized)
}

/// Export the selected source revision, fetching only when needed.
///
/// # Errors
/// Returns an error if Git fails, provenance differs, or a tracked entry cannot
/// be copied into a self-contained snapshot.
pub fn snapshot(
    root: &Path,
    id: &str,
    config: &Source,
    destination: &Path,
    locked: Option<&Source>,
) -> Result<Source> {
    let cache = root.join(".cache/upstreams").join(id);
    let branch = config
        .branch
        .as_deref()
        .context("Git source branch is missing")?;
    if !cache.join(".git").is_dir() {
        fs::create_dir_all(cache.parent().context("cache has no parent")?)?;
        let cache_string = cache.to_str().context("cache path must be UTF-8")?;
        // Name the remote explicitly: `clone.defaultRemoteName` would otherwise
        // leave the cache without an `origin`.
        let mut args = vec![
            "clone",
            "--depth",
            "1",
            "--no-checkout",
            "--origin",
            "origin",
        ];
        // A pinned commit is fetched by hash below, so a renamed or deleted
        // upstream branch must not block restoring it.
        if locked.is_none() {
            args.extend(["--branch", branch]);
        }
        if !config.selection.is_empty() {
            args.push("--filter=blob:none");
        }
        args.extend([config.url.as_str(), cache_string]);
        output(None, &args)?;
    }
    // `remote get-url` reports the URL after `insteadOf` rewriting from the
    // user's configuration; the stored URL is what must match the source.
    let origin = String::from_utf8(output(
        Some(&cache),
        &["config", "--get", "remote.origin.url"],
    )?)?;
    ensure!(
        origin.trim() == config.url,
        "unexpected origin in {}: {}",
        cache.display(),
        origin.trim()
    );
    let commit = if let Some(locked) = locked {
        let commit = locked
            .commit
            .as_deref()
            .context("pinned Git commit is missing")?;
        ensure!(
            (commit.len() == 40 || commit.len() == 64)
                && commit.bytes().all(|byte| byte.is_ascii_hexdigit()),
            "invalid pinned Git commit"
        );
        let exists = command(Some(&cache))
            .args(["cat-file", "-e", &format!("{commit}^{{commit}}")])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()?
            .success();
        if !exists {
            output(Some(&cache), &["fetch", "--depth", "1", "origin", commit])?;
        }
        commit.into()
    } else {
        output(Some(&cache), &["fetch", "--depth", "1", "origin", branch])?;
        String::from_utf8(output(Some(&cache), &["rev-parse", "FETCH_HEAD"])?)?
            .trim()
            .to_owned()
    };
    if commit.is_empty() {
        bail!("Git did not return a commit");
    }
    let materialized = export(&cache, &commit, &config.selection, destination)?;
    let mut source = config.clone();
    source.commit = Some(commit);
    source.transformations = if materialized {
        vec![SYMLINK_TRANSFORMATION.into()]
    } else {
        Vec::new()
    };
    Ok(source)
}
