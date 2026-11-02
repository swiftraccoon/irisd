//! Exercise the published CLI and a copied pack's Cargo workspace boundary.

use std::fs;
use std::path::Path;
use std::process::{Command, Output};

use anyhow::{Context, Result, ensure};

fn successful(command: &mut Command) -> Result<Output> {
    let output = command.output().context("Run reference CLI")?;
    ensure!(
        output.status.success(),
        "Command failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(output)
}

#[test]
fn checks_default_pack_from_another_directory() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let output = successful(
        Command::new(env!("CARGO_BIN_EXE_rust-project-reference"))
            .args(["sync", "--check"])
            .current_dir(directory.path()),
    )?;
    ensure!(String::from_utf8_lossy(&output.stdout).contains("rust-style-guide"));
    Ok(())
}

#[test]
fn exported_pack_contains_a_separate_maintenance_workspace() -> Result<()> {
    let project = tempfile::tempdir()?;
    let host_manifest = "[workspace]\nmembers = []\nresolver = \"3\"\n";
    fs::write(project.path().join("Cargo.toml"), host_manifest)?;
    fs::write(project.path().join("AGENTS.md"), "Project-specific rules\n")?;
    successful(
        Command::new(env!("CARGO_BIN_EXE_rust-project-reference"))
            .arg("export")
            .arg(project.path()),
    )?;
    let pack = project.path().join(".rust-reference");
    successful(
        Command::new(env!("CARGO_BIN_EXE_rust-project-reference"))
            .arg("--root")
            .arg(&pack)
            .args(["sync", "--check"])
            .current_dir(project.path()),
    )?;
    let maintenance = pack.join("tools/reference-maintenance");
    let output = successful(
        Command::new(env!("CARGO"))
            .args([
                "metadata",
                "--offline",
                "--no-deps",
                "--format-version",
                "1",
            ])
            .arg("--manifest-path")
            .arg(maintenance.join("Cargo.toml")),
    )?;
    let metadata: serde_json::Value = serde_json::from_slice(&output.stdout)?;
    let workspace_root = metadata["workspace_root"]
        .as_str()
        .context("Cargo metadata has no workspace root")?;
    ensure!(Path::new(workspace_root).canonicalize()? == maintenance.canonicalize()?);
    ensure!(fs::read_to_string(project.path().join("AGENTS.md"))? == "Project-specific rules\n");
    ensure!(fs::read_to_string(project.path().join("Cargo.toml"))? == host_manifest);
    for excluded in [
        "Cargo.toml",
        "Cargo.lock",
        "src",
        "tests",
        "target",
        ".git",
        ".cache",
        "scripts",
        "tools/reference-maintenance/target",
    ] {
        ensure!(!pack.join(excluded).exists(), "Export contains {excluded}");
    }
    ensure!(maintenance.join("Cargo.toml").is_file());
    ensure!(maintenance.join("Cargo.lock").is_file());
    ensure!(maintenance.join("src/main.rs").is_file());

    let output = successful(
        Command::new(env!("CARGO"))
            .args([
                "metadata",
                "--offline",
                "--no-deps",
                "--format-version",
                "1",
            ])
            .arg("--manifest-path")
            .arg(project.path().join("Cargo.toml")),
    )?;
    let metadata: serde_json::Value = serde_json::from_slice(&output.stdout)?;
    let workspace_root = metadata["workspace_root"]
        .as_str()
        .context("Host Cargo metadata has no workspace root")?;
    ensure!(Path::new(workspace_root).canonicalize()? == project.path().canonicalize()?);
    ensure!(
        metadata["workspace_members"]
            .as_array()
            .is_some_and(Vec::is_empty)
    );
    Ok(())
}

#[test]
fn closed_stdout_pipe_exits_cleanly() -> Result<()> {
    let mut child = Command::new(env!("CARGO_BIN_EXE_rust-project-reference"))
        .args(["sync", "--check"])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .context("Start reference CLI")?;
    // Close the read end before the tool reports, as `tool | head -1` does.
    drop(child.stdout.take());
    let output = child.wait_with_output()?;
    ensure!(
        output.status.success(),
        "exit {:?}: {}",
        output.status.code(),
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(())
}

/// Run Git in `directory` the way a Windows checkout would: converting line
/// endings on checkout, with no user or system configuration interfering.
fn autocrlf_git(directory: &Path, args: &[&str]) -> Result<Output> {
    successful(
        Command::new("git")
            .args([
                "-c",
                "core.autocrlf=true",
                "-c",
                "commit.gpgsign=false",
                "-c",
                "core.hooksPath=/dev/null",
                "-c",
                "user.email=test@example.com",
                "-c",
                "user.name=Reference tests",
            ])
            .args(args)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env(
                "GIT_CONFIG_GLOBAL",
                if cfg!(windows) { "NUL" } else { "/dev/null" },
            )
            .current_dir(directory),
    )
}

#[test]
fn exported_pack_survives_autocrlf_checkout() -> Result<()> {
    let project = tempfile::tempdir()?;
    successful(
        Command::new(env!("CARGO_BIN_EXE_rust-project-reference"))
            .arg("export")
            .arg(project.path()),
    )?;
    let pack = project.path().join(".rust-reference");
    autocrlf_git(project.path(), &["init", "-q"])?;
    autocrlf_git(project.path(), &["add", "-A"])?;
    autocrlf_git(
        project.path(),
        &["commit", "-q", "-m", "Add reference pack"],
    )?;
    fs::remove_dir_all(&pack)?;
    autocrlf_git(project.path(), &["checkout", "--", "."])?;

    successful(
        Command::new(env!("CARGO_BIN_EXE_rust-project-reference"))
            .arg("--root")
            .arg(&pack)
            .args(["sync", "--check"])
            .current_dir(project.path()),
    )?;
    Ok(())
}
