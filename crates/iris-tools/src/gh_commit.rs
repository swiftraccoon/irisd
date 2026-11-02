//! `gh-commit`: commits the difference between the remote branch and the
//! local `HEAD` through the GitHub GraphQL API (`createCommitOnBranch`), so
//! the commit is signed by GitHub, then resets the local branch to it.
//!
//! The mutation always builds on the remote branch head, so the commit it
//! creates has that head as its parent: it cannot make a root commit, and it
//! needs `gh` logged in with write access to the repository.

use std::path::Path;
use std::process::Command;

use anyhow::Context;
use base64::Engine as _;
use clap::Parser;
use serde_json::{Value, json};

/// Arguments of `gh-commit`.
#[derive(Debug, Parser)]
pub struct Args {
    /// The repository as `owner/name`; default: from `origin`.
    #[arg(long)]
    repo: Option<String>,
    /// The branch; default: the current one.
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

fn git(args: &[&str]) -> anyhow::Result<String> {
    let output = Command::new("git")
        .args(args)
        .output()
        .context("running git")?;
    anyhow::ensure!(
        output.status.success(),
        "git {} failed: {}",
        args.join(" "),
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Ok(String::from_utf8_lossy(&output.stdout)
        .trim_end()
        .to_owned())
}

/// `owner/name` from an `origin` URL in HTTPS or SSH form.
pub fn repo_from_url(url: &str) -> Option<String> {
    let trimmed = url.trim().trim_end_matches('/').trim_end_matches(".git");
    let after_host = trimmed.rsplit_once(':').map_or(trimmed, |(_, r)| r);
    let mut parts = after_host.rsplit('/');
    let name = parts.next()?;
    let owner = parts.next()?;
    (!owner.is_empty() && !name.is_empty()).then(|| format!("{owner}/{name}"))
}

/// Runs the tool.
pub fn run(args: &Args) -> anyhow::Result<()> {
    let repo = match &args.repo {
        Some(r) => r.clone(),
        None => repo_from_url(&git(&["remote", "get-url", "origin"])?)
            .context("cannot read owner/name from the origin URL")?,
    };
    let branch = match &args.branch {
        Some(b) => b.clone(),
        None => git(&["rev-parse", "--abbrev-ref", "HEAD"])?,
    };
    anyhow::ensure!(
        git(&["status", "--porcelain"])?.is_empty(),
        "the working tree has uncommitted changes; commit locally first"
    );
    git(&["fetch", "origin", &branch])?;
    let remote = git(&["rev-parse", &format!("origin/{branch}")])?;
    let changes = git(&["diff", "--name-status", "-z", &remote, "HEAD"])?;
    let mut additions = Vec::new();
    let mut deletions = Vec::new();
    let mut parts = changes.split('\0');
    while let Some(status) = parts.next() {
        if status.is_empty() {
            break;
        }
        let path = parts.next().context("truncated diff output")?;
        match status.chars().next() {
            Some('D') => deletions.push(json!({ "path": path })),
            Some('R' | 'C') => {
                // Rename or copy: the old path goes, the new path is added.
                let new_path = parts.next().context("truncated rename")?;
                deletions.push(json!({ "path": path }));
                additions.push(addition(new_path)?);
            }
            _ => additions.push(addition(path)?),
        }
    }
    anyhow::ensure!(
        !additions.is_empty() || !deletions.is_empty(),
        "nothing differs from origin/{branch}"
    );
    println!(
        "{repo} {branch}: {} additions, {} deletions on top of {}",
        additions.len(),
        deletions.len(),
        &remote[..12]
    );
    if args.dry_run {
        return Ok(());
    }
    let input = json!({
        "query": "mutation($input: CreateCommitOnBranchInput!) { createCommitOnBranch(input: $input) { commit { oid } } }",
        "variables": { "input": {
            "branch": { "repositoryNameWithOwner": repo, "branchName": branch },
            "expectedHeadOid": remote,
            "message": { "headline": args.message, "body": args.body.clone().unwrap_or_default() },
            "fileChanges": { "additions": additions, "deletions": deletions },
        }},
    });
    let output = Command::new("gh")
        .args(["api", "graphql", "--input", "-"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .and_then(|mut child| {
            use std::io::Write as _;
            if let Some(mut stdin) = child.stdin.take() {
                stdin.write_all(input.to_string().as_bytes())?;
            }
            child.wait_with_output()
        })
        .context("running gh api graphql")?;
    anyhow::ensure!(
        output.status.success(),
        "gh failed: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    );
    let response: Value = serde_json::from_slice(&output.stdout)?;
    let oid = response
        .pointer("/data/createCommitOnBranch/commit/oid")
        .and_then(Value::as_str)
        .with_context(|| format!("no commit in the response: {response}"))?
        .to_owned();
    git(&["fetch", "origin", &branch])?;
    git(&["reset", "-q", "--hard", &format!("origin/{branch}")])?;
    let verified = Command::new("gh")
        .args([
            "api",
            &format!("repos/{repo}/git/commits/{oid}"),
            "--jq",
            ".verification.verified",
        ])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned())
        .unwrap_or_default();
    println!("committed {oid} (verified: {verified}); local {branch} reset to it");
    Ok(())
}

fn addition(path: &str) -> anyhow::Result<Value> {
    let bytes = std::fs::read(Path::new(path)).with_context(|| format!("reading {path}"))?;
    Ok(json!({ "path": path, "contents": base64::engine::general_purpose::STANDARD.encode(bytes) }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_owner_and_name_from_origin_urls() {
        assert_eq!(
            repo_from_url("https://github.com/owner/name.git").as_deref(),
            Some("owner/name")
        );
        assert_eq!(
            repo_from_url("git@github.com:owner/name.git\n").as_deref(),
            Some("owner/name")
        );
        assert_eq!(
            repo_from_url("https://github.com/owner/name").as_deref(),
            Some("owner/name")
        );
        assert_eq!(repo_from_url("name"), None);
    }
}
