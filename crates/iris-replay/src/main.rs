//! `iris-replay`: serve recordings as synthetic RTSP cameras.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, bail};
use clap::Parser;
use iris_replay::media::Media;
use iris_replay::server::{Mounts, Options, Server};
use iris_replay::synth::{self, Plan};
use tracing::info;
use tracing_subscriber::EnvFilter;

/// Serve recordings as synthetic RTSP cameras over TCP-interleaved RTP.
#[derive(Debug, Parser)]
#[command(version)]
struct Args {
    /// Address to listen on.
    #[arg(long, default_value = "127.0.0.1:8554")]
    listen: SocketAddr,
    /// A mount: NAME=PATH of an irisd recording, served as `rtsp://<listen>/NAME`.
    #[arg(long = "mount", value_name = "NAME=PATH")]
    mounts: Vec<String>,
    /// A synthetic mount: NAME[=GOPS] of thirty-frame-per-second dummy video.
    #[arg(long = "synth", value_name = "NAME[=GOPS]")]
    synths: Vec<String>,
    /// Largest RTP packet, header included.
    #[arg(long, default_value_t = 1_400)]
    mtu: usize,
    /// Stop at the end of the file instead of repeating it.
    #[arg(long)]
    once: bool,
    /// Drop every n-th RTP packet.
    #[arg(long, value_name = "N")]
    drop_every: Option<u64>,
    /// Pause sending once, this long after PLAY.
    #[arg(long, value_parser = humantime::parse_duration, value_name = "DURATION")]
    stall_after: Option<Duration>,
    /// Length of the stall.
    #[arg(long, value_parser = humantime::parse_duration, default_value = "10s", value_name = "DURATION")]
    stall_for: Duration,
    /// Write each access unit with one system call (cameras write per packet).
    #[arg(long)]
    burst: bool,
    /// Spread each frame's packets over the frame interval.
    #[arg(long)]
    pace: bool,
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt().with_env_filter(filter).init();
    let mut mounts = Mounts::new();
    for spec in &args.mounts {
        let (name, path) = spec
            .split_once('=')
            .with_context(|| format!("mount {spec:?} is not NAME=PATH"))?;
        let media = Media::load(&PathBuf::from(path))?;
        info!(
            name,
            frames = media.frames.len(),
            secs = media.total_ticks / 90_000,
            "loaded"
        );
        mounts.insert(name.to_owned(), Arc::new(media));
    }
    for spec in &args.synths {
        let (name, gops) = spec.split_once('=').unwrap_or((spec, "30"));
        let plan = Plan {
            gops: gops
                .parse()
                .with_context(|| format!("synth {spec:?}: bad GOP count"))?,
            ..Plan::default()
        };
        let media = Media::from_bytes(synth::recording(plan))?;
        info!(
            name,
            frames = media.frames.len(),
            secs = media.total_ticks / 90_000,
            "synthesized"
        );
        mounts.insert(name.to_owned(), Arc::new(media));
    }
    if mounts.is_empty() {
        bail!("no mounts; pass --mount NAME=PATH or --synth NAME");
    }
    let options = Options {
        mtu: args.mtu,
        repeat: !args.once,
        drop_every: args.drop_every,
        stall_after: args.stall_after,
        stall_for: args.stall_for,
        burst: args.burst,
        pace: args.pace,
    };
    let server = Server::bind(args.listen, mounts, options).await?;
    info!(listen = %server.local_addr()?, "serving");
    tokio::select! {
        result = server.run() => result,
        _ = tokio::signal::ctrl_c() => {
            info!("interrupted");
            Ok(())
        }
    }
}
