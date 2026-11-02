//! `irisd record`: ingest every configured stream and write fragments.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, bail};
use clap::Args as ClapArgs;
use iris_core::config::{CameraConfig, Config};
use iris_core::id::{StreamKey, StreamKind};
use iris_core::time::MediaTicks;
use iris_ingest::{FragmentSink, IngestOptions, SinkError, StreamSource, StreamStats, run_stream};
use iris_mp4::fragment::Fragment;
use iris_store::{StoragePolicy, WriterHandle, WriterStats};
use tokio::task::JoinHandle;
use tokio::time::MissedTickBehavior;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use crate::proc::{self, CpuMeter};

/// Bytes per mebibyte, for human-readable sizes.
const MIB: f64 = 1_048_576.0;

/// Arguments for `record`.
#[derive(Debug, ClapArgs)]
pub struct Args {
    /// Camera configuration (TOML).
    #[arg(long, value_name = "PATH")]
    config: PathBuf,
    /// Directory that receives recordings.
    #[arg(long, value_name = "PATH")]
    root: PathBuf,
    /// Stop after this long (for example `10m`); otherwise run until Ctrl-C.
    #[arg(long, value_parser = humantime::parse_duration, value_name = "DURATION")]
    duration: Option<Duration>,
    /// Which streams to record, comma separated.
    #[arg(long, value_delimiter = ',', default_value = "main,sub")]
    streams: Vec<StreamKind>,
    /// Restrict recording to these camera ids, comma separated.
    #[arg(long, value_delimiter = ',', value_name = "ID")]
    cameras: Option<Vec<String>>,
    /// Log statistics this often.
    #[arg(long, value_parser = humantime::parse_duration, default_value = "10s", value_name = "DURATION")]
    stats_interval: Duration,
}

/// Forwards fragments to the writer thread without blocking the ingest task.
struct WriterSink(WriterHandle);

impl FragmentSink for WriterSink {
    fn send(&self, key: &StreamKey, fragment: Fragment) -> Result<(), SinkError> {
        self.0
            .try_send(key.clone(), fragment)
            .map_err(|_| SinkError)
    }
}

struct StreamTask {
    key: StreamKey,
    stats: Arc<StreamStats>,
    handle: JoinHandle<()>,
}

/// Runs the subcommand.
pub async fn run(args: Args) -> anyhow::Result<()> {
    let config =
        Config::load(&args.config).with_context(|| format!("loading {}", args.config.display()))?;
    let sources = select_sources(&config, &args)?;
    std::fs::create_dir_all(&args.root)
        .with_context(|| format!("creating {}", args.root.display()))?;

    let policy = StoragePolicy::from(&config.storage);
    let queue_capacity = config.storage.max_queued_fragments.max(1) * sources.len();
    let writer = WriterHandle::spawn(args.root.clone(), policy, queue_capacity)
        .context("starting the writer thread")?;
    let sink: Arc<dyn FragmentSink> = Arc::new(WriterSink(writer.clone()));
    let shutdown = CancellationToken::new();

    let max_fragment = MediaTicks::from_millis(config.storage.max_fragment_ms as i64);
    let mut tasks = Vec::with_capacity(sources.len());
    for (camera, kind, url) in sources {
        let key = StreamKey {
            camera: camera.id.clone(),
            kind,
        };
        let source = StreamSource {
            key: key.clone(),
            url: url
                .parse()
                .with_context(|| format!("camera {} {kind} URL {url:?}", camera.id))?,
            username: camera.username.clone(),
            password: camera.password.clone(),
        };
        let options = IngestOptions {
            max_fragment,
            timestamp_mode: camera.timestamp_mode,
            ..IngestOptions::default()
        };
        let stats = Arc::new(StreamStats::default());
        let handle = tokio::spawn(run_stream(
            source,
            options,
            Arc::clone(&sink),
            Arc::clone(&stats),
            shutdown.clone(),
        ));
        info!(stream = %key, "recording");
        tasks.push(StreamTask { key, stats, handle });
    }

    let started = Instant::now();
    let writer_stats = writer.stats();
    let (cpu, peak_rss) = monitor(&args, &tasks, &writer_stats).await?;

    shutdown.cancel();
    for task in &mut tasks {
        match tokio::time::timeout(Duration::from_secs(15), &mut task.handle).await {
            Ok(Ok(())) => {}
            Ok(Err(err)) => warn!(stream = %task.key, %err, "stream task failed"),
            Err(_) => {
                warn!(stream = %task.key, "stream task did not stop in 15 s; aborting");
                task.handle.abort();
            }
        }
    }
    let writer_for_shutdown = writer.clone();
    tokio::task::spawn_blocking(move || writer_for_shutdown.shutdown())
        .await
        .context("writer thread panicked")?
        .context("sealing recordings")?;

    print_summary(started.elapsed(), &cpu, peak_rss, &tasks, &writer_stats);
    Ok(())
}

/// Logs statistics until Ctrl-C or the configured duration; returns the CPU
/// meter and the peak RSS observed.
async fn monitor(
    args: &Args,
    tasks: &[StreamTask],
    writer_stats: &WriterStats,
) -> anyhow::Result<(CpuMeter, u64)> {
    let mut cpu = CpuMeter::new().context("reading /proc/self/stat")?;
    let mut peak_rss = 0u64;
    let mut ticker = tokio::time::interval(args.stats_interval);
    ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
    ticker.tick().await; // the first tick completes immediately
    // Far enough that `Instant` arithmetic cannot overflow; tokio caps sleeps anyway.
    let deadline = tokio::time::sleep(args.duration.unwrap_or(Duration::from_hours(876_600)));
    tokio::pin!(deadline);

    loop {
        tokio::select! {
            _ = ticker.tick() => {
                let cpu_pct = cpu.sample().map_or(f64::NAN, |s| s.total);
                let rss = proc::rss_bytes().unwrap_or(0);
                peak_rss = peak_rss.max(rss);
                log_stats(tasks, writer_stats, cpu_pct, rss);
            }
            result = tokio::signal::ctrl_c() => {
                if let Err(err) = result {
                    warn!(%err, "cannot listen for Ctrl-C; stopping");
                }
                info!("interrupted; stopping");
                break;
            }
            () = &mut deadline => {
                info!("duration elapsed; stopping");
                break;
            }
        }
    }

    Ok((cpu, peak_rss))
}

fn print_summary(
    elapsed: Duration,
    cpu: &CpuMeter,
    peak_rss: u64,
    tasks: &[StreamTask],
    writer_stats: &WriterStats,
) {
    let peak_rss = peak_rss.max(proc::rss_bytes().unwrap_or(0));
    let mean_cpu = cpu.mean().unwrap_or(f64::NAN);
    let dropped: u64 =
        writer_stats.dropped() + tasks.iter().map(|t| t.stats.dropped_sink()).sum::<u64>();
    let reconnects: u64 = tasks.iter().map(|t| t.stats.reconnects()).sum();
    println!("elapsed:          {:.1} s", elapsed.as_secs_f64());
    println!("mean cpu:         {mean_cpu:.3} % of one core");
    println!("peak rss:         {:.1} MiB", peak_rss as f64 / MIB);
    println!(
        "bytes recorded:   {} ({:.1} MiB)",
        writer_stats.bytes(),
        writer_stats.bytes() as f64 / MIB
    );
    println!("fragments:        {}", writer_stats.fragments());
    println!("files opened:     {}", writer_stats.files_opened());
    println!("syncs:            {}", writer_stats.syncs());
    println!("write errors:     {}", writer_stats.write_errors());
    println!("dropped:          {dropped}");
    println!("reconnects:       {reconnects}");
    for task in tasks {
        let s = &task.stats;
        println!(
            "stream {}: frames {} fragments {} bytes {} loss events {} snaps {} codec {}",
            task.key,
            s.frames(),
            s.fragments(),
            s.bytes(),
            s.loss_events(),
            s.snaps(),
            s.codec_description().as_deref().unwrap_or("-")
        );
    }
}

/// Resolves the (camera, kind, URL) triples to record.
fn select_sources<'a>(
    config: &'a Config,
    args: &Args,
) -> anyhow::Result<Vec<(&'a CameraConfig, StreamKind, String)>> {
    let selected: Vec<&CameraConfig> = config
        .cameras
        .iter()
        .filter(|camera| {
            args.cameras
                .as_ref()
                .is_none_or(|ids| ids.iter().any(|id| id == camera.id.as_str()))
        })
        .collect();
    if selected.is_empty() {
        bail!("no cameras selected in {}", args.config.display());
    }
    let mut sources = Vec::new();
    for camera in selected {
        for &kind in &args.streams {
            if let Some(url) = camera.url_for(kind) {
                sources.push((camera, kind, url));
            } else {
                warn!(camera = %camera.id, %kind, "no URL for this stream; skipping");
            }
        }
    }
    if sources.is_empty() {
        bail!("no streams to record");
    }
    Ok(sources)
}

fn log_stats(tasks: &[StreamTask], writer: &WriterStats, cpu_pct: f64, rss: u64) {
    for task in tasks {
        let s = &task.stats;
        info!(
            stream = %task.key,
            connected = s.connected(),
            frames = s.frames(),
            fragments = s.fragments(),
            mib = format_args!("{:.1}", s.bytes() as f64 / MIB),
            reconnects = s.reconnects(),
            snaps = s.snaps(),
            loss = s.loss_events(),
            dropped = s.dropped_sink(),
            codec = s.codec_description().as_deref().unwrap_or("-"),
            last_frame = s.last_frame_wall().map_or_else(|| "-".to_owned(), |w| w.to_string()),
            "stream"
        );
    }
    info!(
        cpu_pct = format_args!("{cpu_pct:.2}"),
        rss_mib = format_args!("{:.1}", rss as f64 / MIB),
        fragments = writer.fragments(),
        mib = format_args!("{:.1}", writer.bytes() as f64 / MIB),
        syncs = writer.syncs(),
        dropped = writer.dropped(),
        write_errors = writer.write_errors(),
        files = writer.files_opened(),
        "process"
    );
}
