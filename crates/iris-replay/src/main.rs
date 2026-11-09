//! `iris-replay`: serve recordings as synthetic RTSP cameras.

use std::net::SocketAddr;
use std::num::NonZeroU64;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, bail};
use clap::Parser;
use iris_core::time::TIMESCALE;
use iris_replay::media::Media;
use iris_replay::rtsp::MountName;
use iris_replay::server::{DEFAULT_MAX_CONNECTIONS, Delivery, Mounts, Options, Server};
use iris_replay::synth::{self, Plan};
use tracing::info;
use tracing_subscriber::EnvFilter;

/// Serve recordings as synthetic RTSP cameras over TCP-interleaved RTP.
#[derive(Debug, Parser)]
#[command(version)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "each flag is one independent command-line switch"
)]
struct Args {
    /// Address to listen on; anything but loopback also needs `--allow-remote`.
    #[arg(long, default_value = "127.0.0.1:8554")]
    listen: SocketAddr,
    /// Accept a non-loopback `--listen`. The server has no authentication, so
    /// every mounted recording is then open to the network.
    #[arg(long)]
    allow_remote: bool,
    /// Connections served at once; further connections are closed on accept.
    #[arg(
        long,
        default_value_t = DEFAULT_MAX_CONNECTIONS as u32,
        value_name = "N",
        value_parser = clap::value_parser!(u32).range(1..=65_536)
    )]
    max_connections: u32,
    /// A mount: NAME=PATH of an irisd recording, served as `rtsp://<listen>/NAME`.
    #[arg(long = "mount", value_name = "NAME=PATH")]
    mounts: Vec<String>,
    /// A synthetic mount: NAME[=GOPS] of dummy 1080p video at 30 frames per
    /// second in groups of 60 frames; GOPS defaults to 30, a 60-second loop.
    #[arg(long = "synth", value_name = "NAME[=GOPS]")]
    synths: Vec<String>,
    /// Largest RTP packet, header included.
    #[arg(long, default_value_t = 1_400)]
    mtu: usize,
    /// Stop at the end of the file instead of repeating it.
    #[arg(long)]
    once: bool,
    /// Drop every n-th RTP packet; n is at least 1.
    #[arg(long, value_name = "N")]
    drop_every: Option<NonZeroU64>,
    /// Pause sending once, this long after PLAY.
    #[arg(long, value_parser = humantime::parse_duration, value_name = "DURATION")]
    stall_after: Option<Duration>,
    /// Length of the stall.
    #[arg(
        long,
        value_parser = humantime::parse_duration,
        default_value = "10s",
        value_name = "DURATION"
    )]
    stall_for: Duration,
    /// Write each access unit with one system call (cameras write per packet).
    #[arg(long, conflicts_with = "pace")]
    burst: bool,
    /// Spread each frame's packets over the frame interval.
    #[arg(long)]
    pace: bool,
}

impl Args {
    /// The delivery `--burst` and `--pace` select; clap refuses both together.
    fn delivery(&self) -> Delivery {
        if self.burst {
            Delivery::Burst
        } else if self.pace {
            Delivery::Paced
        } else {
            Delivery::PerPacket
        }
    }
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

/// The log filter: `RUST_LOG` when it parses, `info` when it is unset, and
/// `info` with a notice on stderr when it does not parse.
fn log_filter() -> EnvFilter {
    if std::env::var_os("RUST_LOG").is_none() {
        return EnvFilter::new("info");
    }
    EnvFilter::try_from_default_env().unwrap_or_else(|error| {
        eprintln!("iris-replay: RUST_LOG does not parse ({error}); logging at info");
        EnvFilter::new("info")
    })
}

fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    check_listen(args.listen, args.allow_remote)?;
    tracing_subscriber::fmt()
        .with_env_filter(log_filter())
        .init();
    // Recordings load and synthesize before the runtime starts, so no task
    // waits behind the reads.
    let mounts = load_mounts(&args)?;
    let options = Options {
        mtu: args.mtu,
        repeat: !args.once,
        drop_every: args.drop_every,
        stall_after: args.stall_after,
        stall_for: args.stall_for,
        delivery: args.delivery(),
        max_connections: usize::try_from(args.max_connections)?,
        ..Options::default()
    };
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("starting the runtime")?
        .block_on(serve(args.listen, mounts, options))
}

/// Binds and serves until the listener fails or Ctrl-C arrives.
async fn serve(listen: SocketAddr, mounts: Mounts, options: Options) -> anyhow::Result<()> {
    let server = Server::bind(listen, mounts, options).await?;
    info!(listen = %server.local_addr()?, "serving");
    tokio::select! {
        result = server.run() => result.context("serving"),
        signal = tokio::signal::ctrl_c() => {
            signal.context("listening for Ctrl-C")?;
            info!("interrupted");
            Ok(())
        }
    }
}

/// The mounts `--mount` and `--synth` name, loaded and indexed.
fn load_mounts(args: &Args) -> anyhow::Result<Mounts> {
    let mut mounts = Mounts::new();
    for spec in &args.mounts {
        let (name, path) = spec
            .split_once('=')
            .with_context(|| format!("mount {spec:?} is not NAME=PATH"))?;
        let name = claim(&mounts, name)?;
        let media = Media::load(&PathBuf::from(path)).with_context(|| format!("mount {name}"))?;
        announce(&name, &media, "loaded");
        mounts.insert(name, Arc::new(media));
    }
    for spec in &args.synths {
        let (name, gops) = match spec.split_once('=') {
            Some((name, gops)) => (
                name,
                gops.parse()
                    .with_context(|| format!("synth {spec:?}: bad GOP count"))?,
            ),
            None => (spec.as_str(), Plan::default().gops),
        };
        let name = claim(&mounts, name)?;
        let plan = Plan {
            gops,
            ..Plan::default()
        };
        let media = Media::from_bytes(synth::recording(plan)?)?;
        announce(&name, &media, "synthesized");
        mounts.insert(name, Arc::new(media));
    }
    if mounts.is_empty() {
        bail!("no mounts; pass --mount NAME=PATH or --synth NAME");
    }
    Ok(mounts)
}

/// `name` as a mount name that `mounts` does not hold yet.
fn claim(mounts: &Mounts, name: &str) -> anyhow::Result<MountName> {
    let name = MountName::try_new(name)?;
    if mounts.contains_key(&name) {
        bail!("duplicate mount name {:?}", name.as_str());
    }
    Ok(name)
}

fn announce(name: &MountName, media: &Media, what: &str) {
    info!(
        name = %name,
        frames = media.frames().len(),
        secs = media.total_ticks().get() / u64::from(TIMESCALE),
        "{what}"
    );
}

#[cfg(test)]
mod tests {
    use clap::error::ErrorKind;

    use super::*;

    #[test]
    fn a_remote_listener_needs_an_explicit_opt_in() {
        for local in ["127.0.0.1:8554", "127.0.0.2:1", "[::1]:8554"] {
            assert!(
                check_listen(local.parse().unwrap(), /* allow_remote */ false).is_ok(),
                "{local}"
            );
        }
        for remote in ["0.0.0.0:8554", "192.0.2.10:8554", "[::]:8554"] {
            let error =
                check_listen(remote.parse().unwrap(), /* allow_remote */ false).unwrap_err();
            assert!(error.to_string().contains("--allow-remote"), "{remote}");
            assert!(
                check_listen(remote.parse().unwrap(), /* allow_remote */ true).is_ok(),
                "{remote}"
            );
        }
    }

    #[test]
    fn allow_remote_is_off_unless_given() {
        let args =
            Args::try_parse_from(["iris-replay", "--listen", "0.0.0.0:8554", "--synth", "a"])
                .unwrap();
        assert!(!args.allow_remote);
        let args = Args::try_parse_from(["iris-replay", "--allow-remote", "--synth", "a"]).unwrap();
        assert!(args.allow_remote);
    }

    #[test]
    fn max_connections_defaults_to_256_and_rejects_zero() {
        let args = Args::try_parse_from(["iris-replay", "--synth", "a"]).unwrap();
        assert_eq!(args.max_connections, 256);
        let args = Args::try_parse_from(["iris-replay", "--max-connections", "8", "--synth", "a"])
            .unwrap();
        assert_eq!(args.max_connections, 8);
        assert!(Args::try_parse_from(["iris-replay", "--max-connections", "0"]).is_err());
    }

    #[test]
    fn drop_every_rejects_zero() {
        let args = Args::try_parse_from(["iris-replay", "--drop-every", "3"]).unwrap();
        assert_eq!(args.drop_every, NonZeroU64::new(3));
        let error = Args::try_parse_from(["iris-replay", "--drop-every", "0"])
            .expect_err("dropping every zeroth packet means nothing");
        assert_eq!(error.kind(), ErrorKind::ValueValidation);
    }

    #[test]
    fn burst_and_pace_are_exclusive() {
        let error = Args::try_parse_from(["iris-replay", "--burst", "--pace"])
            .expect_err("a burst cannot also be paced");
        assert_eq!(error.kind(), ErrorKind::ArgumentConflict);
        let delivery = |flag: &str| {
            Args::try_parse_from(["iris-replay", flag])
                .unwrap()
                .delivery()
        };
        assert_eq!(delivery("--burst"), Delivery::Burst);
        assert_eq!(delivery("--pace"), Delivery::Paced);
        assert_eq!(delivery("--once"), Delivery::PerPacket);
    }

    #[test]
    fn mount_names_are_reachable_and_unique_across_sources() {
        let mut mounts = Mounts::new();
        let media = Media::from_bytes(
            synth::recording(Plan {
                gops: 1,
                frames_per_gop: 1,
                ..Plan::default()
            })
            .unwrap(),
        )
        .unwrap();
        mounts.insert(claim(&mounts, "cam").unwrap(), Arc::new(media));
        let duplicate = claim(&mounts, "cam").unwrap_err();
        assert!(duplicate.to_string().contains("duplicate mount name"));

        for invalid in ["", "trackID=0", "a/b", "a?b", "a#b"] {
            let error = claim(&mounts, invalid).unwrap_err();
            assert!(
                error.to_string().contains("invalid mount name"),
                "{invalid:?}"
            );
        }
    }
}
