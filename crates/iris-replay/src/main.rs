//! `iris-replay`: serve recordings as synthetic RTSP cameras.

use std::collections::HashSet;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, bail};
use clap::Parser;
use iris_replay::media::Media;
use iris_replay::rtsp::valid_mount_name;
use iris_replay::server::{DEFAULT_MAX_CONNECTIONS, Mounts, Options, Server};
use iris_replay::synth::{self, Plan};
use tracing::info;
use tracing_subscriber::EnvFilter;

/// Serve recordings as synthetic RTSP cameras over TCP-interleaved RTP.
#[derive(Debug, Parser)]
#[command(version)]
struct Args {
    /// Address to listen on; anything but loopback also needs `--allow-remote`.
    #[arg(long, default_value = "127.0.0.1:8554")]
    listen: SocketAddr,
    /// Accept a non-loopback `--listen`. The server has no authentication, so
    /// every mounted recording is then open to the network.
    #[arg(long)]
    allow_remote: bool,
    /// Connections served at once; further connections are closed on accept.
    #[arg(long, default_value_t = DEFAULT_MAX_CONNECTIONS as u32, value_name = "N",
          value_parser = clap::value_parser!(u32).range(1..=65_536))]
    max_connections: u32,
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

/// Refuses a listener outside loopback unless `allow_remote` opts in.
fn check_listen(listen: SocketAddr, allow_remote: bool) -> anyhow::Result<()> {
    if !listen.ip().is_loopback() && !allow_remote {
        bail!(
            "refusing to listen on {listen}: replay has no authentication and serves every \
             mount to whoever connects; pass --allow-remote to accept that"
        );
    }
    Ok(())
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    check_listen(args.listen, args.allow_remote)?;
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt().with_env_filter(filter).init();
    let mut mounts = Mounts::new();
    let mut claimed_names = HashSet::new();
    for spec in &args.mounts {
        let (name, path) = spec
            .split_once('=')
            .with_context(|| format!("mount {spec:?} is not NAME=PATH"))?;
        claim_mount_name(&mut claimed_names, name)?;
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
        claim_mount_name(&mut claimed_names, name)?;
        let plan = Plan {
            gops: gops
                .parse()
                .with_context(|| format!("synth {spec:?}: bad GOP count"))?,
            ..Plan::default()
        };
        let media = Media::from_bytes(synth::recording(plan)?)?;
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
        max_connections: usize::try_from(args.max_connections)?,
        ..Options::default()
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

fn claim_mount_name(claimed: &mut HashSet<String>, name: &str) -> anyhow::Result<()> {
    if !valid_mount_name(name) {
        bail!("invalid mount name {name:?}");
    }
    if !claimed.insert(name.to_owned()) {
        bail!("duplicate mount name {name:?}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_remote_listener_needs_an_explicit_opt_in() {
        for local in ["127.0.0.1:8554", "127.0.0.2:1", "[::1]:8554"] {
            assert!(
                check_listen(local.parse().unwrap(), false).is_ok(),
                "{local}"
            );
        }
        for remote in ["0.0.0.0:8554", "192.0.2.10:8554", "[::]:8554"] {
            let error = check_listen(remote.parse().unwrap(), false).unwrap_err();
            assert!(error.to_string().contains("--allow-remote"), "{remote}");
            assert!(
                check_listen(remote.parse().unwrap(), true).is_ok(),
                "{remote}"
            );
        }
        let args =
            Args::try_parse_from(["iris-replay", "--listen", "0.0.0.0:8554", "--synth", "a"])
                .unwrap();
        assert!(!args.allow_remote);
        assert_eq!(args.max_connections, 256);
        let args = Args::try_parse_from([
            "iris-replay",
            "--allow-remote",
            "--max-connections",
            "8",
            "--synth",
            "a",
        ])
        .unwrap();
        assert!(args.allow_remote);
        assert_eq!(args.max_connections, 8);
        assert!(Args::try_parse_from(["iris-replay", "--max-connections", "0"]).is_err());
    }

    #[test]
    fn mount_names_are_reachable_and_unique_across_sources() {
        let mut claimed = HashSet::new();
        claim_mount_name(&mut claimed, "cam").unwrap();
        let duplicate = claim_mount_name(&mut claimed, "cam").unwrap_err();
        assert!(duplicate.to_string().contains("duplicate mount name"));

        for invalid in ["", "trackID=0", "a/b", "a?b", "a#b"] {
            let error = claim_mount_name(&mut claimed, invalid).unwrap_err();
            assert!(
                error.to_string().contains("invalid mount name"),
                "{invalid:?}"
            );
        }
    }
}
