//! gh-commit publishes the exact tree of the checked-out branch through
//! GitHub's createCommitOnBranch mutation, then atomically adopts the signed
//! commit locally.
//!
//! The source commit is pinned, the origin host and branch must be
//! unambiguous, paths are literal and full-tree, and the checked-out ref is
//! updated with a compare-and-swap only after verifying the returned tree.

use std::io::Write as _;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::Duration;

use anyhow::Context;
use base64::Engine as _;
use clap::Parser;
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

const GH_TIMEOUT: Duration = Duration::from_secs(120);
const GH_OUTPUT_LIMIT: u64 = 1024 * 1024;
const BASE_REF: &str = "refs/irisd/gh-commit/base";
const RESULT_REF: &str = "refs/irisd/gh-commit/result";

/// Arguments of gh-commit.
#[derive(Debug, Parser)]
pub struct Args {
    /// The repository as owner/name; default: from origin.
    #[arg(long)]
    repo: Option<String>,
    /// GitHub hostname. Required with --repo when origin is not a GitHub URL.
    #[arg(long)]
    hostname: Option<String>,
    /// The branch; it must be the checked-out branch.
    #[arg(long)]
    branch: Option<String>,
    /// The commit message's first line.
    #[arg(short, long)]
    message: String,
    /// The commit message's body.
    #[arg(long)]
    body: Option<String>,
    /// Show what would be committed and stop.
    #[arg(long)]
    dry_run: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Target {
    hostname: String,
    repo: String,
}

#[derive(Debug)]
struct GhOutput {
    status: std::process::ExitStatus,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

fn git(args: &[&str]) -> anyhow::Result<String> {
    String::from_utf8(git_bytes(args)?)
        .map(|text| text.trim_end().to_owned())
        .context("git output is not UTF-8")
}

fn git_bytes(args: &[&str]) -> anyhow::Result<Vec<u8>> {
    let output = Command::new("git")
        .env("GIT_TERMINAL_PROMPT", "0")
        .args(args)
        .output()
        .context("running git")?;
    anyhow::ensure!(
        output.status.success(),
        "git {} failed: {}",
        args.join(" "),
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Ok(output.stdout)
}

fn validate_repo(repo: &str) -> anyhow::Result<()> {
    let mut parts = repo.split('/');
    let owner = parts.next().unwrap_or_default();
    let name = parts.next().unwrap_or_default();
    anyhow::ensure!(
        !owner.is_empty()
            && !name.is_empty()
            && parts.next().is_none()
            && [owner, name].iter().all(|part| {
                part.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
            }),
        "repository must be exactly owner/name using GitHub-safe characters"
    );
    Ok(())
}

fn validate_hostname(hostname: &str) -> anyhow::Result<()> {
    anyhow::ensure!(
        !hostname.is_empty()
            && hostname
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-'))
            && !hostname.starts_with(['.', '-'])
            && !hostname.ends_with(['.', '-']),
        "invalid GitHub hostname"
    );
    Ok(())
}

fn valid_oid(oid: &str) -> bool {
    matches!(oid.len(), 40 | 64) && oid.bytes().all(|byte| byte.is_ascii_hexdigit())
}

/// GitHub host and owner/name from an HTTPS, ssh URL, or scp-style URL.
fn target_from_url(url: &str) -> Option<Target> {
    let raw = url.trim();
    let (hostname, path) = if let Ok(parsed) = reqwest::Url::parse(raw) {
        if !matches!(parsed.scheme(), "https" | "ssh" | "git") || parsed.password().is_some() {
            return None;
        }
        (
            parsed.host_str()?.to_ascii_lowercase(),
            parsed.path().to_owned(),
        )
    } else {
        let (left, path) = raw.split_once(':')?;
        let hostname = left.rsplit_once('@').map_or(left, |(_, host)| host);
        if hostname.contains('/') || hostname.is_empty() {
            return None;
        }
        (hostname.to_ascii_lowercase(), format!("/{path}"))
    };
    let trimmed = path.trim_matches('/');
    let path = trimmed.strip_suffix(".git").unwrap_or(trimmed);
    let mut parts = path.split('/');
    let owner = parts.next()?;
    let name = parts.next()?;
    if owner.is_empty() || name.is_empty() || parts.next().is_some() {
        return None;
    }
    let target = Target {
        hostname,
        repo: format!("{owner}/{name}"),
    };
    validate_hostname(&target.hostname).ok()?;
    validate_repo(&target.repo).ok()?;
    Some(target)
}

/// Compatibility helper used by documentation tests.
#[cfg(test)]
pub fn repo_from_url(url: &str) -> Option<String> {
    target_from_url(url).map(|target| target.repo)
}

fn resolve_target(args: &Args) -> anyhow::Result<Target> {
    let origin = git(&["remote", "get-url", "origin"])
        .ok()
        .and_then(|url| target_from_url(&url));
    let repo = args
        .repo
        .clone()
        .or_else(|| origin.as_ref().map(|target| target.repo.clone()))
        .context("origin is not an unambiguous GitHub URL; pass --repo and --hostname")?;
    let hostname = args
        .hostname
        .clone()
        .or_else(|| origin.as_ref().map(|target| target.hostname.clone()))
        .context("origin is not an unambiguous GitHub URL; pass --hostname")?
        .to_ascii_lowercase();
    validate_repo(&repo)?;
    validate_hostname(&hostname)?;
    if let Some(origin) = origin {
        anyhow::ensure!(
            origin.hostname == hostname && origin.repo == repo,
            "--repo/--hostname must match origin exactly"
        );
    }
    Ok(Target { hostname, repo })
}

fn current_branch() -> anyhow::Result<String> {
    let branch = git(&["symbolic-ref", "--quiet", "--short", "HEAD"])
        .context("HEAD must be attached to the publication branch")?;
    anyhow::ensure!(!branch.is_empty(), "HEAD must be attached");
    Ok(branch)
}

fn ensure_clean() -> anyhow::Result<()> {
    let dirty = git_bytes(&[
        "-c",
        "status.showUntrackedFiles=all",
        "status",
        "--porcelain=v1",
        "-z",
        "--untracked-files=all",
        "--ignore-submodules=none",
    ])?;
    anyhow::ensure!(
        dirty.is_empty(),
        "the working tree, index, or a submodule has uncommitted changes"
    );
    Ok(())
}

fn delete_temp_refs() {
    let _ = Command::new("git")
        .args(["update-ref", "-d", BASE_REF])
        .status();
    let _ = Command::new("git")
        .args(["update-ref", "-d", RESULT_REF])
        .status();
}

fn fetch_ref(branch: &str) -> anyhow::Result<String> {
    git_network(&[
        "fetch",
        "--no-tags",
        "origin",
        &format!("+refs/heads/{branch}:{BASE_REF}"),
    ])?;
    git(&["rev-parse", &format!("{BASE_REF}^{{commit}}")])
}

fn fetch_oid(oid: &str) -> anyhow::Result<()> {
    git_network(&[
        "fetch",
        "--no-tags",
        "origin",
        &format!("+{oid}:{RESULT_REF}"),
    ])?;
    let fetched = git(&["rev-parse", &format!("{RESULT_REF}^{{commit}}")])?;
    anyhow::ensure!(
        fetched == oid,
        "fetched {fetched}, expected published OID {oid}"
    );
    Ok(())
}

fn trees_equal(a: &str, b: &str) -> anyhow::Result<bool> {
    Ok(git(&["rev-parse", &format!("{a}^{{tree}}")])?
        == git(&["rev-parse", &format!("{b}^{{tree}}")])?)
}

fn reconcile(branch: &str, source_oid: &str, published_oid: &str) -> anyhow::Result<()> {
    fetch_oid(published_oid).with_context(|| {
        format!("remote commit {published_oid} exists; fetch/recovery is required")
    })?;
    anyhow::ensure!(
        trees_equal(source_oid, RESULT_REF)?,
        "remote commit {published_oid} exists but its tree differs from pinned source {source_oid}"
    );
    anyhow::ensure!(
        current_branch()? == branch,
        "checked-out branch changed during publication"
    );
    ensure_clean()?;
    let current = git(&["rev-parse", "HEAD"])?;
    anyhow::ensure!(
        current == source_oid,
        "remote commit {published_oid} exists, but local {branch} moved from {source_oid} to {current}; no local state was overwritten"
    );
    git(&[
        "update-ref",
        &format!("refs/heads/{branch}"),
        published_oid,
        source_oid,
    ])
    .with_context(|| {
        format!("remote commit {published_oid} exists; local compare-and-swap failed")
    })?;
    Ok(())
}

async fn read_bounded<R: tokio::io::AsyncRead + Unpin>(reader: R) -> anyhow::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    reader
        .take(GH_OUTPUT_LIMIT + 1)
        .read_to_end(&mut bytes)
        .await?;
    anyhow::ensure!(
        bytes.len() as u64 <= GH_OUTPUT_LIMIT,
        "gh output exceeded 1 MiB"
    );
    Ok(bytes)
}

async fn command_async(
    program: &str,
    args: &[&str],
    input: Option<Vec<u8>>,
) -> anyhow::Result<GhOutput> {
    // A private process group lets timeout and cancellation stop helpers too.
    let mut child = tokio::process::Command::new("setsid")
        .arg(program)
        .args(args)
        .stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .with_context(|| format!("starting {program}"))?;
    let pid = child.id().context("supervised child has no pid")?;
    let stdout = child.stdout.take().context("capturing child stdout")?;
    let stderr = child.stderr.take().context("capturing child stderr")?;
    let stdout_task = tokio::spawn(read_bounded(stdout));
    let stderr_task = tokio::spawn(read_bounded(stderr));
    let input_task = child.stdin.take().map(|mut stdin| {
        tokio::spawn(async move {
            if let Some(input) = input {
                stdin.write_all(&input).await?;
            }
            stdin.shutdown().await
        })
    });
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let outcome = tokio::select! {
        status = child.wait() => Ok(status?),
        () = tokio::time::sleep(GH_TIMEOUT) => Err(anyhow::anyhow!("{program} timed out; publication outcome may be ambiguous")),
        _ = tokio::signal::ctrl_c() => Err(anyhow::anyhow!("{program} interrupted; publication outcome may be ambiguous")),
        _ = terminate.recv() => Err(anyhow::anyhow!("{program} terminated; publication outcome may be ambiguous")),
    };
    if outcome.is_err() {
        if let Some(group) = rustix::process::Pid::from_raw(pid as i32) {
            let _ = rustix::process::kill_process_group(group, rustix::process::Signal::KILL);
        }
        let _ = child.start_kill();
        let _ = tokio::time::timeout(Duration::from_secs(5), child.wait()).await;
    }
    if let Some(task) = input_task {
        let _ = task.await;
    }
    let stdout = stdout_task.await.context("joining gh stdout reader")??;
    let stderr = stderr_task.await.context("joining gh stderr reader")??;
    Ok(GhOutput {
        status: outcome?,
        stdout,
        stderr,
    })
}

fn gh(args: &[&str], input: Option<Vec<u8>>) -> anyhow::Result<GhOutput> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?
        .block_on(command_async("gh", args, input))
}

fn git_network(args: &[&str]) -> anyhow::Result<()> {
    let output = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?
        .block_on(command_async("git", args, None))?;
    anyhow::ensure!(
        output.status.success(),
        "git {} failed: {}",
        args.join(" "),
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Ok(())
}

fn require_github_host(target: &Target) -> anyhow::Result<()> {
    if target.hostname == "github.com" {
        return Ok(());
    }
    let output = gh(&["auth", "status", "--hostname", &target.hostname], None)?;
    anyhow::ensure!(
        output.status.success(),
        "{} is not configured as an authenticated GitHub Enterprise host: {}",
        target.hostname,
        String::from_utf8_lossy(&output.stderr).trim()
    );
    let metadata = gh(
        &[
            "api",
            "--hostname",
            &target.hostname,
            "meta",
            "--jq",
            ".installed_version",
        ],
        None,
    )?;
    anyhow::ensure!(
        metadata.status.success() && !metadata.stdout.is_empty(),
        "{} did not identify itself as GitHub Enterprise: {}",
        target.hostname,
        String::from_utf8_lossy(&metadata.stderr).trim()
    );
    Ok(())
}

fn changed_paths(remote_oid: &str, source_oid: &str) -> anyhow::Result<Vec<(u8, String)>> {
    let bytes = git_bytes(&[
        "diff",
        "--no-renames",
        "--ignore-submodules=none",
        "--name-status",
        "-z",
        remote_oid,
        source_oid,
    ])?;
    let fields = bytes.split(|byte| *byte == 0).collect::<Vec<_>>();
    let mut changes = Vec::new();
    for pair in fields.chunks(2) {
        if pair.len() == 1 && pair[0].is_empty() {
            break;
        }
        anyhow::ensure!(
            pair.len() == 2 && !pair[0].is_empty(),
            "truncated git diff output"
        );
        let status = pair[0][0];
        anyhow::ensure!(
            matches!(status, b'A' | b'M' | b'D' | b'T'),
            "unsupported diff status"
        );
        let path = String::from_utf8(pair[1].to_vec()).context("Git path is not UTF-8")?;
        changes.push((status, path));
    }
    Ok(changes)
}

fn addition(source_oid: &str, path: &str) -> anyhow::Result<Value> {
    let entry = git(&[
        "--literal-pathspecs",
        "ls-tree",
        "--full-tree",
        "-z",
        source_oid,
        "--",
        path,
    ])?;
    let oid = regular_blob_oid(&entry, path)?;
    let bytes = git_bytes(&["cat-file", "blob", oid])?;
    Ok(json!({
        "path": path,
        "contents": base64::engine::general_purpose::STANDARD.encode(bytes)
    }))
}

fn regular_blob_oid<'a>(entry: &'a str, path: &str) -> anyhow::Result<&'a str> {
    let mut fields = entry.splitn(2, '\t');
    let header = fields.next().context("missing tree entry")?;
    let tree_path = fields.next().context("missing tree path")?;
    anyhow::ensure!(
        tree_path.strip_suffix('\0') == Some(path),
        "unexpected tree path for {path}"
    );
    let fields: Vec<&str> = header.split_whitespace().collect();
    anyhow::ensure!(
        fields.len() == 3 && fields[0] == "100644" && fields[1] == "blob",
        "cannot publish {path}: GitHub file additions support only ordinary non-executable files; symlinks, executable files and submodules are unsupported"
    );
    Ok(fields[2])
}

fn recovery_path() -> anyhow::Result<PathBuf> {
    Ok(PathBuf::from(git(&[
        "rev-parse",
        "--path-format=absolute",
        "--git-path",
        "irisd-gh-commit-recovery.json",
    ])?))
}

fn write_recovery(
    target: &Target,
    branch: &str,
    source: &str,
    oid: &str,
) -> anyhow::Result<PathBuf> {
    let path = recovery_path()?;
    let tmp = path.with_extension(format!("tmp-{}", std::process::id()));
    let bytes = serde_json::to_vec(&json!({
        "hostname": target.hostname,
        "repo": target.repo,
        "branch": branch,
        "source": source,
        "published": oid,
    }))?;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&tmp)?;
    file.write_all(&bytes)?;
    file.sync_all()?;
    std::fs::rename(&tmp, &path)?;
    if let Some(parent) = path.parent() {
        std::fs::File::open(parent)?.sync_all()?;
    }
    Ok(path)
}

fn recover_pending(target: &Target, branch: &str, source: &str) -> anyhow::Result<bool> {
    let path = recovery_path()?;
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error).context("reading gh-commit recovery state"),
    };
    let state: Value =
        serde_json::from_slice(&bytes).context("decoding gh-commit recovery state")?;
    let field = |name: &str| {
        state
            .get(name)
            .and_then(Value::as_str)
            .with_context(|| format!("recovery state is missing {name}"))
    };
    anyhow::ensure!(
        field("hostname")? == target.hostname
            && field("repo")? == target.repo
            && field("branch")? == branch,
        "pending gh-commit recovery state belongs to a different target; reconcile {} manually",
        path.display()
    );
    let prior_source = field("source")?.to_owned();
    let published = field("published")?.to_owned();
    anyhow::ensure!(valid_oid(&published), "recovery state has an invalid OID");
    if prior_source == source {
        reconcile(branch, source, &published).with_context(|| {
            format!(
                "recovering published commit {published} from {}",
                path.display()
            )
        })?;
    } else {
        anyhow::ensure!(
            published == source,
            "pending gh-commit recovery state belongs to source {prior_source}, but local HEAD is {source}; reconcile {} manually",
            path.display()
        );
        fetch_oid(&published)?;
        anyhow::ensure!(
            trees_equal(source, RESULT_REF)?,
            "adopted recovery commit no longer verifies"
        );
    }
    std::fs::remove_file(&path)?;
    if let Some(parent) = path.parent() {
        std::fs::File::open(parent)?.sync_all()?;
    }
    delete_temp_refs();
    println!("recovered and adopted published commit {published}");
    Ok(true)
}

/// Runs the tool.
#[allow(clippy::too_many_lines)]
pub fn run(args: &Args) -> anyhow::Result<()> {
    let target = resolve_target(args)?;
    require_github_host(&target)?;
    let branch = current_branch()?;
    if let Some(requested) = &args.branch {
        anyhow::ensure!(
            requested == &branch,
            "--branch must match checked-out branch {branch}"
        );
    }
    ensure_clean()?;
    let source_oid = git(&["rev-parse", "HEAD^{commit}"])?;
    if recover_pending(&target, &branch, &source_oid)? {
        return Ok(());
    }
    let remote_oid = fetch_ref(&branch)?;
    let changes = changed_paths(&remote_oid, &source_oid)?;
    if changes.is_empty() {
        if remote_oid != source_oid && trees_equal(&remote_oid, &source_oid)? {
            reconcile(&branch, &source_oid, &remote_oid)?;
            delete_temp_refs();
            println!("adopted existing signed commit {remote_oid}");
            return Ok(());
        }
        anyhow::bail!("nothing differs from origin/{branch}");
    }
    let mut additions = Vec::new();
    let mut deletions = Vec::new();
    for (status, path) in changes {
        if status == b'D' {
            deletions.push(json!({ "path": path }));
        } else {
            additions.push(addition(&source_oid, &path)?);
        }
    }
    println!(
        "{} {} {}: {} additions, {} deletions on top of {}",
        target.hostname,
        target.repo,
        branch,
        additions.len(),
        deletions.len(),
        &remote_oid[..12]
    );
    if args.dry_run {
        delete_temp_refs();
        return Ok(());
    }
    anyhow::ensure!(
        current_branch()? == branch,
        "checked-out branch changed while planning"
    );
    ensure_clean()?;
    anyhow::ensure!(
        git(&["rev-parse", "HEAD"])? == source_oid,
        "HEAD changed while planning"
    );
    let input = json!({
        "query": "mutation($input: CreateCommitOnBranchInput!) { createCommitOnBranch(input: $input) { commit { oid } } }",
        "variables": { "input": {
            "branch": { "repositoryNameWithOwner": target.repo, "branchName": branch },
            "expectedHeadOid": remote_oid,
            "message": { "headline": args.message, "body": args.body.clone().unwrap_or_default() },
            "fileChanges": { "additions": additions, "deletions": deletions },
        }},
    });
    let output = gh(
        &[
            "api",
            "--hostname",
            &target.hostname,
            "graphql",
            "--input",
            "-",
        ],
        Some(serde_json::to_vec(&input)?),
    )?;
    anyhow::ensure!(
        output.status.success(),
        "gh failed; publication outcome may be ambiguous: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    );
    let response: Value = serde_json::from_slice(&output.stdout)
        .context("gh returned an invalid response; publication outcome may be ambiguous")?;
    let oid = response
        .pointer("/data/createCommitOnBranch/commit/oid")
        .and_then(Value::as_str)
        .with_context(|| format!("publication response is ambiguous: {response}"))?
        .to_owned();
    anyhow::ensure!(valid_oid(&oid), "invalid published commit OID");
    let recovery = write_recovery(&target, &branch, &source_oid, &oid)?;
    reconcile(&branch, &source_oid, &oid)
        .with_context(|| format!("published {oid}; recovery state is {}", recovery.display()))?;
    std::fs::remove_file(&recovery)?;
    delete_temp_refs();
    let verified = gh(
        &[
            "api",
            "--hostname",
            &target.hostname,
            &format!("repos/{}/git/commits/{oid}", target.repo),
            "--jq",
            ".verification.verified",
        ],
        None,
    )?;
    let verified = if verified.status.success() {
        String::from_utf8_lossy(&verified.stdout).trim().to_owned()
    } else {
        "unknown".to_owned()
    };
    println!("committed {oid} (verified: {verified}); local {branch} adopted it by CAS");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_tree_modes_the_commit_api_cannot_preserve() {
        for (mode, kind) in [("120000", "blob"), ("100755", "blob"), ("160000", "commit")] {
            let entry = format!("{mode} {kind} fixtureoid\tlinked\0");
            assert!(regular_blob_oid(&entry, "linked").is_err(), "mode {mode}");
        }
    }

    #[test]
    fn reads_the_regular_file_blob_without_following_the_worktree() {
        assert_eq!(
            regular_blob_oid("100644 blob fixtureoid\tfile\0", "file").unwrap(),
            "fixtureoid"
        );
        assert!(regular_blob_oid("100644 blob fixtureoid\tother\0", "file").is_err());
    }

    #[test]
    fn reads_host_owner_and_name_from_github_urls() {
        assert_eq!(
            target_from_url("https://github.com/owner/name.git"),
            Some(Target {
                hostname: "github.com".into(),
                repo: "owner/name".into()
            })
        );
        assert_eq!(
            target_from_url("git@ghe.example:owner/name.git\n"),
            Some(Target {
                hostname: "ghe.example".into(),
                repo: "owner/name".into()
            })
        );
        assert_eq!(
            target_from_url("ssh://git@ghe.example/owner/name.git"),
            Some(Target {
                hostname: "ghe.example".into(),
                repo: "owner/name".into()
            })
        );
        assert_eq!(target_from_url("/tmp/name"), None);
        assert_eq!(
            target_from_url("https://gitlab.example/owner/name/extra"),
            None
        );
        assert_eq!(repo_from_url("name"), None);
    }

    #[test]
    fn validates_repository_and_hostname_grammar() {
        assert!(validate_repo("owner/repo").is_ok());
        assert!(validate_repo("owner/repo/extra").is_err());
        assert!(validate_repo("owner/re po").is_err());
        assert!(validate_hostname("ghe.example").is_ok());
        assert!(validate_hostname("https://ghe.example").is_err());
        assert!(valid_oid(&"a".repeat(40)));
        assert!(valid_oid(&"B".repeat(64)));
        assert!(!valid_oid(&"g".repeat(40)));
        assert!(!valid_oid(&"a".repeat(39)));
    }
}
