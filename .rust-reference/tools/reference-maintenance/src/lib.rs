//! Maintain local, pinned reference material and export it into Rust projects.

#![forbid(unsafe_code)]

pub mod export;
pub mod git;
pub mod manifest;
pub mod sources;
pub mod sync;
pub mod web;

/// Print a status line to standard output, treating a closed pipe as success so
/// that `rust-project-reference ... | head -1` cannot abort work in progress.
///
/// # Errors
/// Returns any other error from writing to standard output.
pub fn report(message: impl std::fmt::Display) -> std::io::Result<()> {
    use std::io::Write;
    let mut stdout = std::io::stdout().lock();
    match writeln!(stdout, "{message}").and_then(|()| stdout.flush()) {
        Err(error) if error.kind() == std::io::ErrorKind::BrokenPipe => Ok(()),
        result => result,
    }
}

/// Locate the reference library containing this maintenance tool.
pub fn reference_root() -> &'static std::path::Path {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(std::path::Path::parent)
        .expect("The maintenance crate lives in tools/reference-maintenance")
}
