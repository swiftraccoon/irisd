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
//! - `run` — the daemon: start-up, task spawning, run-time camera
//!   provisioning, the monitor loop that logs statistics, pings the watchdog,
//!   runs retention and the self-check, and the ordered shutdown.
//! - `record`, `inspect`, `recover`, `hls`, `serve` — the offline subcommands.
//! - `adopt` — start-up reconciliation of the catalog with the files on disk.
//! - `catalog_observer` — store notifications and camera events into the catalog.
//! - `retention` — the retention pass: free-space floor, size quota, age limits,
//!   and a bounded retry of earlier deletion intents.
//! - `raw_calls` — the count of uncancellable filesystem calls a maintenance
//!   owner has in flight.
//! - `selfcheck` — a rotating CRC check of each stream's newest sealed file.
//! - `sandbox` — the Landlock ruleset, applied before any thread exists.
//! - `permissions` — startup checks of the configuration file, the state
//!   directory, and the state files in it.
//! - `capability` — the media-capability secret in `capability.key`.
//! - `provision` — the store of cameras the API added (`cameras.toml`).
//! - `systemd` — readiness, watchdog, and stopping notifications.
//! - `proc` — CPU, RSS, thread, and descriptor sampling from `/proc/self`.
//!
//! # Invariants
//!
//! The watchdog ping is withheld whenever an ingest task made no progress in
//! the interval. The statistics line field names are parsed by `iris-tools
//! daemon-stats` and `iris-tools soak-report` and listed in
//! `docs/research/07-architecture.md` §9. The long-lived threads are the tokio
//! current-thread runtime, one writer thread per video root, and one catalog
//! thread; beside them run the counted maintenance threads (root probes,
//! retention removals), the self-check's scan thread, tokio's blocking pool
//! during start-up reconciliation, and the writer and catalog shutdown
//! helpers, one call each (07 §3).

mod adopt;
mod capability;
mod catalog_observer;
mod hls;
mod inspect;
mod permissions;
mod proc;
mod provision;
mod raw_calls;
mod record;
mod recover;
mod retention;
mod run;
mod sandbox;
mod selfcheck;
mod serve;
mod systemd;

use std::ffi::OsStr;
use std::io::IsTerminal;

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

/// The log filter: `RUST_LOG` when it is set and parses, else `info`. An
/// unparsable value is reported on standard error, the stream the log itself
/// uses, instead of silently selecting the default.
fn log_filter() -> EnvFilter {
    if std::env::var_os(EnvFilter::DEFAULT_ENV).is_none() {
        return EnvFilter::new("info");
    }
    EnvFilter::try_from_default_env().unwrap_or_else(|error| {
        eprintln!("irisd: ignoring {}: {error}", EnvFilter::DEFAULT_ENV);
        EnvFilter::new("info")
    })
}

/// Whether log lines carry ANSI colours: only when standard error is a
/// `terminal`, and not when `NO_COLOR` is set to a non-empty value. The
/// journal and files get plain text.
fn log_in_colour(terminal: bool, no_color: Option<&OsStr>) -> bool {
    terminal && no_color.is_none_or(OsStr::is_empty)
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(log_filter())
        .with_writer(std::io::stderr)
        .with_ansi(log_in_colour(
            std::io::stderr().is_terminal(),
            std::env::var_os("NO_COLOR").as_deref(),
        ))
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn colours_only_a_terminal_without_no_color() {
        assert!(log_in_colour(true, None));
        assert!(log_in_colour(true, Some(OsStr::new(""))));
        assert!(!log_in_colour(true, Some(OsStr::new("1"))));
        assert!(!log_in_colour(false, None));
        assert!(!log_in_colour(false, Some(OsStr::new(""))));
    }
}
