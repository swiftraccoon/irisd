//! `irisd recover`: truncate a recording at its last valid fragment.

use std::path::PathBuf;

use anyhow::Context;
use clap::Args as ClapArgs;
use iris_store::recover::recover_segment;

/// Arguments for `recover`.
#[derive(Debug, ClapArgs)]
pub struct Args {
    /// The recording (`.mp4`) to recover; its sidecar index is rewritten.
    file: PathBuf,
}

/// Runs the subcommand.
pub fn run(args: &Args) -> anyhow::Result<()> {
    let report = recover_segment(&args.file)
        .with_context(|| format!("recovering {}", args.file.display()))?;
    println!("file:             {}", report.path.display());
    println!("fragments:        {}", report.fragments);
    println!("original length:  {} bytes", report.original_len);
    println!("valid length:     {} bytes", report.valid_len);
    println!("truncated:        {}", report.truncated);
    println!("index rewritten:  {}", report.index_rewritten);
    match &report.stop {
        Some(stop) => println!("scan stopped at:  {stop:?}"),
        None => println!("scan stopped at:  clean end of file"),
    }
    Ok(())
}
