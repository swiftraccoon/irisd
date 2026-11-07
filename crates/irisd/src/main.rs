//! `irisd`: an H.265 network video recorder whose record path never decodes.
//!
//! The binary has six subcommands. `run` is the daemon: it records every
//! configured camera stream into fragmented MP4 files, keeps the SQLite
//! catalog, serves the HTTP API, subscribes to camera events, enforces
//! retention, checks its own recordings, and reports to systemd. The others
//! work offline: `record` ingests to disk without the catalog or API,
//! `inspect` scans a recording, `recover` truncates one at its last valid
//! fragment, `hls` writes static byte-range playlists, and `serve` is a
//! range-capable file server for playback tests.
//!
//! # Modules
//!
//! - `run` — the daemon: start-up, task spawning, the monitor loop that logs
//!   statistics, pings the watchdog, runs retention and the self-check, and
//!   the ordered shutdown.
//! - `record`, `inspect`, `recover`, `hls`, `serve` — the offline subcommands.
//! - `adopt` — start-up reconciliation of the catalog with the files on disk.
//! - `catalog_observer` — store notifications and camera events into the catalog.
//! - `retention` — the retention pass: free-space floor, size quota, age limits,
//!   and a bounded retry of earlier deletion intents.
//! - `selfcheck` — a rotating CRC check of each stream's newest sealed file.
//! - `sandbox` — the Landlock ruleset, applied before any thread exists.
//! - `permissions` — startup checks of the configuration file, the state
//!   directory, and the catalog files.
//! - `systemd` — readiness, watchdog, and stopping notifications.
//! - `proc` — CPU, RSS, thread, and descriptor sampling from `/proc/self`.
//!
//! # Invariants
//!
//! The watchdog ping is withheld whenever an ingest task made no progress in
//! the interval. The statistics line field names are parsed by the tools under
//! `tools/probe` and listed in `docs/research/07-architecture.md` §9. Threads
//! are the tokio current-thread runtime, one writer thread per video root, and
//! one catalog thread.

mod adopt;
mod catalog_observer;
mod hls;
mod inspect;
mod permissions;
mod proc;
mod record;
mod recover;
mod retention;
mod run;
mod sandbox;
mod selfcheck;
mod serve;
mod systemd;

use clap::{Parser, Subcommand};
use tracing_subscriber::EnvFilter;

/// An H.265 network video recorder whose record path never decodes.
#[derive(Debug, Parser)]
#[command(name = "irisd", version, about, long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Run the daemon: record, catalog, serve the API, subscribe to events, enforce retention.
    Run(run::Args),
    /// Record the configured cameras to fragmented MP4 files.
    Record(record::Args),
    /// Scan a recording and report its structure and timing.
    Inspect(inspect::Args),
    /// Truncate a recording at its last valid fragment and rebuild its index.
    Recover(recover::Args),
    /// Write static HLS playlists (byte ranges) for one recording.
    Hls(hls::Args),
    /// Serve a directory over HTTP with range support, for playback tests.
    Serve(serve::Args),
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .init();
    match Cli::parse().command {
        Command::Run(args) => run::run(args).await,
        Command::Record(args) => record::run(args).await,
        Command::Inspect(args) => inspect::run(&args),
        Command::Recover(args) => recover::run(&args),
        Command::Hls(args) => hls::run(&args),
        Command::Serve(args) => serve::run(args).await,
    }
}
