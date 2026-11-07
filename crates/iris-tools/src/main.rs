//! `iris-tools`: the operations and research tools beside the daemon.
//!
//! - `daemon-stats LOG` summarises an `irisd run` log.
//! - `soak-sample` takes one host-side sample beside a running service.
//! - `soak-report DIR` computes the soak figures from a sampler directory.
//! - `replay-bench` measures the daemon against replayed cameras.
//! - `rtsp-probe` and `onvif-call` characterise a camera.
//! - `gh-commit` commits the difference from the remote branch through the
//!   GitHub API, which signs the commit.
//!
//! Every tool is read-only towards cameras and the recording service except
//! where its name says otherwise.

mod bench;
mod gh_commit;
mod logs;
mod onvif_call;
mod report;
mod rtsp_probe;
mod sample;
mod secret;
mod stats;
mod time;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(version, about)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Summarise an `irisd run --stats-interval` log: rates and stalls per
    /// stream, writer and catalog peaks, the process trend, notable lines.
    DaemonStats(stats::Args),
    /// Append one host-side sample beside a running service (for a timer).
    SoakSample(sample::Args),
    /// Compute the soak exit-criteria figures from a sampler directory.
    SoakReport(report::Args),
    /// Run the daemon against replayed cameras and report its cost.
    ReplayBench(bench::Args),
    /// Speak RTSP to a camera and report its packetisation and timing.
    RtspProbe(rtsp_probe::Args),
    /// Issue one authenticated ONVIF SOAP call.
    OnvifCall(onvif_call::Args),
    /// Commit the difference from the remote branch through the GitHub API, which signs it.
    GhCommit(gh_commit::Args),
}

fn main() -> anyhow::Result<()> {
    match Cli::parse().command {
        Command::DaemonStats(args) => stats::run(&args),
        Command::SoakSample(args) => sample::run(&args),
        Command::SoakReport(args) => report::run(&args),
        Command::ReplayBench(args) => bench::run(&args),
        Command::RtspProbe(args) => rtsp_probe::run(&args),
        Command::OnvifCall(args) => onvif_call::run(&args),
        Command::GhCommit(args) => gh_commit::run(&args),
    }
}
