//! `irisd record`: ingest every configured stream and write fragments.

use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, bail};
use clap::Args as ClapArgs;
use iris_core::config::{CameraConfig, Config};
use iris_core::id::{StreamKey, StreamKind};
use iris_ingest::{FragmentSink, IngestOptions, SinkError, StreamSource, StreamStats, run_stream};
use iris_mp4::fragment::Fragment;
use iris_store::{StoragePolicy, WriterHandle, WriterStats, create_recording_root};
use tokio::task::JoinHandle;
use tokio::time::MissedTickBehavior;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use crate::proc::{self, CpuMeter};

/// Bytes per mebibyte, for human-readable sizes.
const MIB: f64 = 1_048_576.0;
/// Shared grace for all standalone ingest tasks.
const PRODUCER_GRACE: Duration = Duration::from_secs(10);
/// Tail of the producer grace reserved for observing aborted ingest tasks.
const ABORT_OBSERVATION_GRACE: Duration = Duration::from_secs(1);
/// Total standalone shutdown budget.
const SHUTDOWN_BUDGET: Duration = Duration::from_secs(25);

#[derive(Clone, Copy)]
struct StandaloneShutdownSchedule {
    producer_grace: Duration,
    abort_observation_grace: Duration,
    budget: Duration,
}

const SHUTDOWN_SCHEDULE: StandaloneShutdownSchedule = StandaloneShutdownSchedule {
    producer_grace: PRODUCER_GRACE,
    abort_observation_grace: ABORT_OBSERVATION_GRACE,
    budget: SHUTDOWN_BUDGET,
};

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
    joined: bool,
}

struct MonitorState {
    cpu: CpuMeter,
    peak_rss: u64,
    ticker: tokio::time::Interval,
    sigint: tokio::signal::unix::Signal,
    deadline: Pin<Box<tokio::time::Sleep>>,
}

impl MonitorState {
    fn new(args: &Args) -> anyhow::Result<Self> {
        validate_args(args)?;
        let now = tokio::time::Instant::now();
        let mut ticker = tokio::time::interval_at(now + args.stats_interval, args.stats_interval);
        ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
        Ok(Self {
            cpu: CpuMeter::new().context("reading /proc/self/stat")?,
            peak_rss: 0,
            ticker,
            sigint: tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())
                .context("installing the SIGINT handler")?,
            deadline: Box::pin(tokio::time::sleep(
                args.duration.unwrap_or(Duration::from_hours(876_600)),
            )),
        })
    }
}

/// Runs the subcommand.
pub async fn run(args: Args) -> anyhow::Result<()> {
    let config =
        Config::load(&args.config).with_context(|| format!("loading {}", args.config.display()))?;
    let sources = select_sources(&config, &args)?;
    let queue_capacity =
        crate::run::checked_queue_capacity(config.storage.max_queued_fragments, sources.len())?;
    let max_fragment = crate::run::checked_fragment_ticks(config.storage.max_fragment_ms)?;
    let mut monitor_state = MonitorState::new(&args)?;
    prepare_recording_root(&args.root)?;

    let policy = StoragePolicy::from(&config.storage);
    let writer = WriterHandle::spawn(args.root.clone(), policy, queue_capacity)
        .context("starting the writer thread")?;
    let sink: Arc<dyn FragmentSink> = Arc::new(WriterSink(writer.clone()));
    let shutdown = CancellationToken::new();

    let mut tasks = Vec::with_capacity(sources.len());
    for (camera, kind, url) in sources {
        let key = StreamKey {
            camera: camera.id.clone(),
            kind,
        };
        let source = StreamSource {
            key: key.clone(),
            url,
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
        tasks.push(StreamTask {
            key,
            stats,
            handle,
            joined: false,
        });
    }

    let started = Instant::now();
    let writer_stats = writer.stats();
    monitor(&mut monitor_state, &tasks, &writer_stats).await;

    let failure = shutdown_recording(&mut tasks, &writer, &shutdown, SHUTDOWN_SCHEDULE).await;

    print_summary(
        started.elapsed(),
        &monitor_state.cpu,
        monitor_state.peak_rss,
        &tasks,
        &writer_stats,
    );
    failure.map_or(Ok(()), Err)
}

async fn shutdown_recording(
    tasks: &mut [StreamTask],
    writer: &WriterHandle,
    shutdown: &CancellationToken,
    schedule: StandaloneShutdownSchedule,
) -> Option<anyhow::Error> {
    let shutdown_started = tokio::time::Instant::now();
    let producer_deadline = shutdown_started + schedule.producer_grace;
    let cooperative_deadline = shutdown_started
        + schedule
            .producer_grace
            .saturating_sub(schedule.abort_observation_grace);
    let shutdown_deadline = shutdown_started + schedule.budget;
    let mut failure = None;
    shutdown.cancel();
    let wait_for_producers = async {
        for task in tasks.iter_mut() {
            if let Err(err) = (&mut task.handle).await {
                warn!(stream = %task.key, %err, "stream task failed");
                failure.get_or_insert_with(|| {
                    anyhow::anyhow!("stream {} task failed: {err}", task.key)
                });
            }
            task.joined = true;
        }
    };
    if tokio::time::timeout_at(cooperative_deadline, wait_for_producers)
        .await
        .is_err()
    {
        failure.get_or_insert_with(|| {
            anyhow::anyhow!(
                "recording task shutdown exceeded {:?}",
                schedule.producer_grace
            )
        });
    }
    for task in tasks.iter() {
        if !task.joined {
            task.handle.abort();
        }
    }
    let observe_aborted = async {
        for task in tasks.iter_mut() {
            if !task.joined {
                if let Err(err) = (&mut task.handle).await
                    && !err.is_cancelled()
                {
                    warn!(stream = %task.key, %err, "stream task abort failed");
                    failure.get_or_insert_with(|| {
                        anyhow::anyhow!("stream {} abort failed: {err}", task.key)
                    });
                }
                task.joined = true;
            }
        }
    };
    if crate::run::complete_by_deadline(producer_deadline, observe_aborted)
        .await
        .is_none()
    {
        failure.get_or_insert_with(|| {
            anyhow::anyhow!("aborted recording-task observation exceeded the shared producer grace")
        });
    }
    let writer_for_shutdown = writer.clone();
    match crate::run::run_on_shutdown_thread("irisd-record-writer-shutdown", move || {
        writer_for_shutdown.shutdown()
    }) {
        Ok(result) => match tokio::time::timeout_at(shutdown_deadline, result).await {
            Ok(Ok(Ok(()))) => {}
            Ok(Ok(Err(err))) => {
                warn!(%err, "cannot seal standalone recordings");
                failure
                    .get_or_insert_with(|| anyhow::Error::new(err).context("sealing recordings"));
            }
            Ok(Err(err)) => {
                failure.get_or_insert_with(|| {
                    anyhow::anyhow!("writer shutdown thread stopped without a result: {err}")
                });
            }
            Err(_) => {
                failure.get_or_insert_with(|| {
                    anyhow::anyhow!("writer sealing exceeded the standalone shutdown budget")
                });
            }
        },
        Err(err) => {
            failure.get_or_insert_with(|| {
                anyhow::Error::new(err).context("starting writer shutdown thread")
            });
        }
    }

    failure
}

fn prepare_recording_root(root: &Path) -> anyhow::Result<()> {
    create_recording_root(root).with_context(|| format!("creating {}", root.display()))
}

fn validate_args(args: &Args) -> anyhow::Result<()> {
    if args.stats_interval.is_zero() {
        bail!("--stats-interval must be greater than zero");
    }
    let mut kinds = std::collections::HashSet::new();
    for &kind in &args.streams {
        if !kinds.insert(kind) {
            bail!("--streams contains {kind} more than once");
        }
    }
    Ok(())
}

/// Logs statistics until Ctrl-C or the configured duration; returns the CPU
/// meter and the peak RSS observed.
async fn monitor(state: &mut MonitorState, tasks: &[StreamTask], writer_stats: &WriterStats) {
    loop {
        tokio::select! {
            _ = state.ticker.tick() => {
                let cpu_pct = state.cpu.sample().map_or(f64::NAN, |s| s.total);
                let rss = proc::rss_bytes().unwrap_or(0);
                state.peak_rss = state.peak_rss.max(rss);
                log_stats(tasks, writer_stats, cpu_pct, rss);
            }
            _ = state.sigint.recv() => {
                info!("interrupted; stopping");
                break;
            }
            () = state.deadline.as_mut() => {
                info!("duration elapsed; stopping");
                break;
            }
        }
    }
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
    // Every sink rejection is already part of the writer aggregate. Per-stream
    // dropped_sink counters remain the attribution for the stream lines.
    let stream_dropped = tasks.iter().map(|task| task.stats.dropped_sink()).sum();
    let dropped = summary_dropped(writer_stats.dropped(), stream_dropped);
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

/// The writer aggregate already includes every public sink rejection. Stream
/// counters are attribution only and must not be added to the final total.
fn summary_dropped(writer_dropped: u64, _stream_dropped: u64) -> u64 {
    writer_dropped
}

/// Resolves the (camera, kind, URL) triples to record.
fn select_sources<'a>(
    config: &'a Config,
    args: &Args,
) -> anyhow::Result<Vec<(&'a CameraConfig, StreamKind, iris_ingest::Url)>> {
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
                let url = url
                    .parse()
                    .with_context(|| format!("parsing camera {} {kind} URL", camera.id))?;
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

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use iris_core::time::WallUs;
    use iris_ingest::deliver_fragment;
    use iris_mp4::fragment::Sample;
    use iris_mp4::init::SampleEntry;

    fn args(streams: Vec<StreamKind>) -> Args {
        Args {
            config: PathBuf::from("config.toml"),
            root: PathBuf::from("recordings"),
            duration: None,
            streams,
            cameras: None,
            stats_interval: Duration::from_secs(10),
        }
    }

    fn fragment() -> Fragment {
        Fragment {
            samples: vec![Sample {
                data: Bytes::from_static(&[0, 0, 0, 2, 0x26, 1]),
                duration: 3_000,
                is_sync: true,
            }],
            decode_time: 0,
            wall: WallUs::new(1_700_000_000_000_000),
            raw_rtp: 0,
            new_run: true,
            sample_entry: Arc::new(SampleEntry::new(
                Bytes::from_static(b"\0\0\0\x10hvc1\0\0\0\0\0\0\0\0"),
                "hvc1".into(),
                16,
                16,
            )),
        }
    }

    #[test]
    fn standalone_root_preparation_durably_creates_a_nonexistent_root() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("nested/recordings");
        prepare_recording_root(&root).unwrap();
        eprintln!("standalone recording root prepared");
        assert!(root.is_dir());
    }

    #[test]
    fn duplicate_stream_selectors_are_rejected_before_recording() {
        for streams in [
            vec![StreamKind::Main, StreamKind::Main],
            vec![StreamKind::Sub, StreamKind::Main, StreamKind::Sub],
        ] {
            assert!(validate_args(&args(streams)).is_err());
        }
        assert!(validate_args(&args(vec![StreamKind::Main, StreamKind::Sub])).is_ok());
        assert!(validate_args(&args(vec![StreamKind::Main, StreamKind::Sub])).is_ok());
    }

    #[test]
    fn zero_statistics_period_is_rejected() {
        let mut args = args(vec![StreamKind::Main]);
        args.stats_interval = Duration::ZERO;
        assert!(validate_args(&args).is_err());
    }

    #[test]
    fn malformed_source_error_does_not_echo_the_url() {
        let marker = "validation-secret";
        for raw in [
            format!("rtsp://{marker}@"),
            format!("not-a-url?password={marker}"),
            format!("not-a-url#{marker}"),
            format!("not-a-url-{marker}"),
        ] {
            let parser_cause = raw.parse::<iris_ingest::Url>().unwrap_err().to_string();
            let config = Config::from_toml_str(&format!(
                "[[camera]]\nid='cam1'\nname=''\nrtsp_url='{raw}'\nusername=''\npassword=''\nrecord=['main']\n"
            ))
            .unwrap();
            let error = select_sources(&config, &args(vec![StreamKind::Main])).unwrap_err();
            let text = format!("{error:#}");
            assert!(!text.contains(marker), "{text}");
            assert!(!text.contains(&raw), "{text}");
            assert!(text.contains("camera cam1 main URL"), "{text}");
            assert!(text.contains(&parser_cause), "{text}");
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn unobservable_aborted_producer_cannot_steal_the_writer_seal_window() {
        struct ReleaseOnDrop(Option<std::sync::mpsc::Sender<()>>);

        impl Drop for ReleaseOnDrop {
            fn drop(&mut self) {
                if let Some(release) = self.0.take() {
                    let _ = release.send(());
                }
            }
        }

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("recordings");
        let writer = WriterHandle::spawn(
            root.clone(),
            StoragePolicy {
                sync_interval: Duration::from_secs(60),
                preallocate_bytes: 0,
                rotate_bytes: u64::MAX,
                rotate_secs: u64::MAX,
            },
            4,
        )
        .unwrap();
        let key = StreamKey {
            camera: "cam1".parse().unwrap(),
            kind: StreamKind::Main,
        };
        writer.try_send(key.clone(), fragment()).unwrap();
        tokio::time::timeout(Duration::from_secs(10), async {
            while writer.stats().files_opened() == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();

        let (started_tx, started_rx) = std::sync::mpsc::sync_channel(0);
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let mut release = ReleaseOnDrop(Some(release_tx));
        let blocked = tokio::task::spawn_blocking(move || {
            let _ = started_tx.send(());
            let _ = release_rx.recv();
        });
        started_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        let mut tasks = [StreamTask {
            key,
            stats: Arc::new(StreamStats::default()),
            handle: blocked,
            joined: false,
        }];
        let shutdown = CancellationToken::new();
        let started = Instant::now();
        let error = shutdown_recording(
            &mut tasks,
            &writer,
            &shutdown,
            StandaloneShutdownSchedule {
                producer_grace: Duration::from_millis(200),
                abort_observation_grace: Duration::from_millis(50),
                budget: Duration::from_secs(3),
            },
        )
        .await
        .expect("the blocked producer must report a bounded shutdown failure");
        assert!(
            error
                .to_string()
                .contains("recording task shutdown exceeded")
        );
        assert!(started.elapsed() < Duration::from_secs(5));

        let path = crate::adopt::recordings_under(&root).remove(0);
        let report = iris_mp4::reader::scan(
            std::io::BufReader::new(std::fs::File::open(path).unwrap()),
            iris_mp4::reader::ScanOptions::default(),
        )
        .unwrap();
        assert_eq!(report.fragments.len(), 1);
        assert_eq!(
            report.stop, None,
            "writer must seal before the total deadline"
        );

        release.0.take().unwrap().send(()).unwrap();
        if !tasks[0].joined {
            let _ = tokio::time::timeout(Duration::from_secs(1), &mut tasks[0].handle)
                .await
                .unwrap();
        }
    }

    #[test]
    fn one_public_queue_rejection_is_counted_once_in_the_final_total() {
        let dir = tempfile::tempdir().unwrap();
        let policy = StoragePolicy::from(&iris_core::config::StorageConfig::default());
        let writer = WriterHandle::spawn(dir.path().to_path_buf(), policy, 1).unwrap();
        let writer_stats = writer.stats();
        writer.shutdown().unwrap();

        let key = StreamKey {
            camera: iris_core::id::CameraId::try_new("cam1").unwrap(),
            kind: StreamKind::Main,
        };
        let stream_stats = StreamStats::new();
        let accepted = deliver_fragment(&WriterSink(writer), &key, fragment(), &stream_stats);

        assert!(!accepted, "the disconnected public queue must reject");
        assert_eq!(stream_stats.fragments(), 1);
        assert_eq!(stream_stats.dropped_sink(), 1);
        assert_eq!(writer_stats.dropped(), 1);
        assert_eq!(
            summary_dropped(writer_stats.dropped(), stream_stats.dropped_sink()),
            1
        );
    }
}
