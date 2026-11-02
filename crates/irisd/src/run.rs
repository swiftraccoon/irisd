//! `irisd run`: the daemon. Records every configured stream, keeps the
//! catalog, serves the API, subscribes to camera events, enforces retention,
//! and reports to systemd.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, bail};
use clap::Args as ClapArgs;
use iris_api::{ApiState, ApiStats, LiveRegistry, SnapshotSource};
use iris_catalog::CatalogHandle;
use iris_core::config::{CameraConfig, Config};
use iris_core::event::{Event, EventSource};
use iris_core::id::{CameraId, StreamKey, StreamKind};
use iris_core::metrics::{HistogramSnapshot, Peak};
use iris_core::time::{MediaTicks, WallUs};
use iris_events::{CameraEndpoint, EventSink, EventStats, run_dahua_cgi, run_onvif_pullpoint};
use iris_ingest::{
    FragmentSink, IngestOptions, LiveRing, ReadTicker, SinkError, StreamSource, StreamStats,
    run_stream,
};
use iris_mp4::fragment::Fragment;
use iris_store::{StoragePolicy, WriterHandle, WriterStats, free_space};
use tokio::sync::broadcast;
use tokio::task::JoinHandle;
use tokio::time::MissedTickBehavior;
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

use crate::catalog_observer::{CatalogEventSink, CatalogObserver};
use crate::proc::{self, CpuMeter, CpuSplit};
use crate::sandbox::SandboxSpec;
use crate::selfcheck::{SelfCheck, SelfCheckStats};
use crate::{adopt, retention, sandbox, systemd};

/// Bytes per mebibyte, for human-readable sizes.
const MIB: f64 = 1_048_576.0;
/// Bytes per gibibyte.
const GIB: f64 = 1_073_741_824.0;
/// Media ticks per second.
const TICKS_PER_SEC: f64 = 90_000.0;
/// Cadence of the timer that measures how late the runtime thread runs.
const LOOP_PROBE_INTERVAL: Duration = Duration::from_millis(250);
/// How often retention runs.
const RETENTION_INTERVAL: Duration = Duration::from_secs(60);
/// Watchdog cadence when systemd did not configure one (used only for liveness logging).
const DEFAULT_WATCHDOG_INTERVAL: Duration = Duration::from_secs(30);
/// Frames kept per live ring.
const LIVE_RING_FRAMES: usize = 256;
/// Event fan-out channel depth.
const EVENT_CHANNEL: usize = 256;
/// How long tasks get to stop before being aborted.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(15);

/// Arguments for `run`.
#[derive(Debug, ClapArgs)]
pub struct Args {
    /// Daemon configuration (TOML).
    #[arg(long, value_name = "PATH")]
    config: PathBuf,
    /// Skip the Landlock sandbox (for debugging on kernels without Landlock).
    #[arg(long)]
    no_sandbox: bool,
    /// Log statistics this often.
    #[arg(long, value_parser = humantime::parse_duration, default_value = "60s", value_name = "DURATION")]
    stats_interval: Duration,
    /// Check every stream's newest sealed file once per this long.
    #[arg(long, value_parser = humantime::parse_duration, default_value = "24h", value_name = "DURATION")]
    selfcheck_cycle: Duration,
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
    /// Counters at the last watchdog check.
    last_progress: (u64, u64),
}

struct EventTask {
    camera: CameraId,
    source: EventSource,
    stats: Arc<EventStats>,
    handle: JoinHandle<()>,
    /// Pull-latency counters at the last statistics line.
    pull_seen: HistogramSnapshot,
}

/// Everything the monitor loop needs.
struct Daemon {
    config: Config,
    catalog: CatalogHandle,
    writer: WriterHandle,
    streams: Vec<StreamTask>,
    events: Vec<EventTask>,
    api: JoinHandle<std::io::Result<()>>,
    shutdown: CancellationToken,
    /// Largest timer lateness seen on the runtime thread since the last statistics line.
    loop_lag: Arc<Peak>,
}

/// Runs the daemon until SIGTERM or Ctrl-C.
pub async fn run(args: Args) -> anyhow::Result<()> {
    let config =
        Config::load(&args.config).with_context(|| format!("loading {}", args.config.display()))?;
    validate(&config)?;
    for root in &config.storage.video_roots {
        std::fs::create_dir_all(root).with_context(|| format!("creating {}", root.display()))?;
    }
    std::fs::create_dir_all(&config.storage.state_dir)
        .with_context(|| format!("creating {}", config.storage.state_dir.display()))?;

    if args.no_sandbox {
        warn!("sandbox disabled by --no-sandbox");
    } else {
        // Before any thread exists: threads inherit the Landlock domain of their creator.
        sandbox::apply(&sandbox_spec(&config, &args.config))?;
    }

    let catalog = CatalogHandle::open(&config.storage.state_dir.join("irisd.db"))
        .context("opening the catalog")?;
    for camera in &config.cameras {
        catalog
            .ensure_camera(&camera.id, &camera.name, camera.vendor.as_deref())
            .await
            .with_context(|| format!("registering camera {}", camera.id))?;
    }
    let recovered = adopt::recover_writing(&catalog).await?;
    let adopted = adopt::adopt_unknown(&catalog, &config.storage.video_roots).await?;
    info!(
        recovered,
        adopted, "catalog reconciled with the recording roots"
    );

    let shutdown = CancellationToken::new();
    let loop_lag = Arc::new(Peak::new());
    tokio::spawn(probe_loop_lag(Arc::clone(&loop_lag), shutdown.clone()));
    let (writer, streams, live) = start_recording(&config, &catalog, &shutdown)?;
    let (events_tx, _) = broadcast::channel::<Arc<Event>>(EVENT_CHANNEL);
    let events = start_events(&config, &catalog, &events_tx, &shutdown);
    let api = start_api(&config, &catalog, live, events_tx, &shutdown);

    systemd::ready();
    systemd::status("recording");
    info!(listen = %config.api.listen, cameras = config.cameras.len(), "irisd running");

    let mut daemon = Daemon {
        config,
        catalog,
        writer,
        streams,
        events,
        api,
        shutdown,
        loop_lag,
    };
    let outcome = monitor(&mut daemon, &args).await;
    stop(daemon).await?;
    outcome
}

fn validate(config: &Config) -> anyhow::Result<()> {
    if config.cameras.is_empty() {
        bail!("no cameras configured");
    }
    if config.storage.video_roots.is_empty() {
        bail!("storage.video_roots is empty");
    }
    if config.api.tokens.is_empty() {
        warn!("api.tokens is empty: the API accepts unauthenticated requests");
    }
    Ok(())
}

fn sandbox_spec(config: &Config, config_path: &Path) -> SandboxSpec {
    let config_dir = config_path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map_or_else(|| PathBuf::from("."), Path::to_path_buf);
    let mut read_only = vec![config_dir, PathBuf::from("/proc"), PathBuf::from("/etc")];
    read_only.retain(|p| p.exists());
    let mut read_write = config.storage.video_roots.clone();
    read_write.push(config.storage.state_dir.clone());
    let mut connect_ports: Vec<u16> = config
        .cameras
        .iter()
        .flat_map(|c| {
            let mut ports = c.rtsp_ports();
            ports.push(c.http_port_or_default());
            ports
        })
        .collect();
    connect_ports.sort_unstable();
    connect_ports.dedup();
    SandboxSpec {
        read_only,
        read_write,
        connect_ports,
        bind_ports: vec![config.api.listen.port()],
    }
}

type Recording = (WriterHandle, Vec<StreamTask>, Arc<LiveRegistry>);

fn start_recording(
    config: &Config,
    catalog: &CatalogHandle,
    shutdown: &CancellationToken,
) -> anyhow::Result<Recording> {
    let sources = sources(config);
    if sources.is_empty() {
        bail!("no streams to record");
    }
    let policy = StoragePolicy::from(&config.storage);
    let capacity = config.storage.max_queued_fragments.max(1) * sources.len();
    let observer = Arc::new(CatalogObserver::new(catalog.clone()));
    let writer = WriterHandle::spawn_with_observer(
        config.storage.video_roots[0].clone(),
        policy,
        capacity,
        observer,
    )
    .context("starting the writer thread")?;
    let sink: Arc<dyn FragmentSink> = Arc::new(WriterSink(writer.clone()));
    let live = LiveRegistry::new();
    let max_fragment = MediaTicks::from_millis(config.storage.max_fragment_ms as i64);

    let mut streams = Vec::with_capacity(sources.len());
    // One ticker for every stream: a timer per stream costs one runtime unpark per
    // stream per tick (`iris_ingest::ReadTicker`).
    let ticker = config.ingest.read_interval().map(ReadTicker::spawn);
    for (camera, kind, url) in sources {
        let key = StreamKey {
            camera: camera.id.clone(),
            kind,
        };
        let ring = LiveRing::new(LIVE_RING_FRAMES);
        live.insert(key.clone(), ring.clone());
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
            live: Some(ring),
            read_interval: config.ingest.read_interval(),
            ticker: ticker.clone(),
            recv_low_water: config.ingest.recv_low_water(),
            ..IngestOptions::default()
        };
        let stats = Arc::new(StreamStats::default());
        live.insert_stats(key.clone(), Arc::clone(&stats));
        let handle = tokio::spawn(run_stream(
            source,
            options,
            Arc::clone(&sink),
            Arc::clone(&stats),
            shutdown.clone(),
        ));
        info!(stream = %key, "recording");
        streams.push(StreamTask {
            key,
            stats,
            handle,
            last_progress: (0, 0),
        });
    }
    Ok((writer, streams, Arc::new(live)))
}

/// Resolves the (camera, kind, URL) triples to record.
fn sources(config: &Config) -> Vec<(&CameraConfig, StreamKind, String)> {
    let mut sources = Vec::new();
    for camera in &config.cameras {
        for &kind in &camera.record {
            if let Some(url) = camera.url_for(kind) {
                sources.push((camera, kind, url));
            } else {
                warn!(camera = %camera.id, %kind, "no URL for this stream; skipping");
            }
        }
    }
    sources
}

/// Maps a camera's configuration to the endpoint event sources use.
pub fn endpoint(camera: &CameraConfig) -> Option<CameraEndpoint> {
    Some(CameraEndpoint {
        camera: camera.id.clone(),
        host: camera.host()?,
        http_port: camera.http_port_or_default(),
        https: camera.https,
        tls_verify: camera.tls_verify,
        username: camera.username.clone(),
        password: camera.password.clone(),
    })
}

fn start_events(
    config: &Config,
    catalog: &CatalogHandle,
    events_tx: &broadcast::Sender<Arc<Event>>,
    shutdown: &CancellationToken,
) -> Vec<EventTask> {
    let sink: Arc<dyn EventSink> =
        Arc::new(CatalogEventSink::new(catalog.clone(), events_tx.clone()));
    let mut tasks = Vec::new();
    for camera in &config.cameras {
        for &source in &camera.events {
            let Some(endpoint) = endpoint(camera) else {
                warn!(camera = %camera.id, "cannot derive a host from rtsp_url; no events");
                break;
            };
            let stats = Arc::new(EventStats::default());
            let handle = match source {
                EventSource::DahuaCgi => tokio::spawn(run_dahua_cgi(
                    endpoint,
                    Arc::clone(&sink),
                    Arc::clone(&stats),
                    shutdown.clone(),
                )),
                EventSource::OnvifPullPoint => tokio::spawn(run_onvif_pullpoint(
                    endpoint,
                    Arc::clone(&sink),
                    Arc::clone(&stats),
                    shutdown.clone(),
                )),
                EventSource::OnvifMetadata | EventSource::Manual => {
                    warn!(camera = %camera.id, ?source, "not a subscribable event source; skipping");
                    continue;
                }
            };
            info!(camera = %camera.id, ?source, "subscribed to events");
            tasks.push(EventTask {
                camera: camera.id.clone(),
                source,
                stats,
                handle,
                pull_seen: HistogramSnapshot::default(),
            });
        }
    }
    tasks
}

fn start_api(
    config: &Config,
    catalog: &CatalogHandle,
    live: Arc<LiveRegistry>,
    events: broadcast::Sender<Arc<Event>>,
    shutdown: &CancellationToken,
) -> JoinHandle<std::io::Result<()>> {
    let snapshot_sources: HashMap<CameraId, SnapshotSource> = config
        .cameras
        .iter()
        .filter_map(|camera| {
            Some((
                camera.id.clone(),
                SnapshotSource {
                    host: camera.host()?,
                    port: camera.http_port_or_default(),
                    https: camera.https,
                    tls_verify: camera.tls_verify,
                    username: camera.username.clone(),
                    password: camera.password.clone(),
                },
            ))
        })
        .collect();
    let state = Arc::new(ApiState {
        catalog: catalog.clone(),
        live,
        events,
        tokens: config.api.tokens.clone(),
        snapshot_sources,
        stats: Arc::new(ApiStats::default()),
        started: WallUs::now(),
        shutdown: shutdown.clone(),
    });
    let router = iris_api::router(state);
    tokio::spawn(iris_api::serve(config.api.listen, router, shutdown.clone()))
}

/// Measures how late timers fire on the runtime thread; a blocked thread shows as lag.
async fn probe_loop_lag(peak: Arc<Peak>, shutdown: CancellationToken) {
    loop {
        let before = Instant::now();
        tokio::select! {
            () = shutdown.cancelled() => return,
            () = tokio::time::sleep(LOOP_PROBE_INTERVAL) => {}
        }
        let lag = before.elapsed().saturating_sub(LOOP_PROBE_INTERVAL);
        peak.observe(u64::try_from(lag.as_millis()).unwrap_or(u64::MAX));
    }
}

/// Histogram counters at the previous statistics line, for per-interval percentiles.
#[derive(Default)]
struct Snapshots {
    write: HistogramSnapshot,
    sync: HistogramSnapshot,
    transaction: HistogramSnapshot,
    query: HistogramSnapshot,
    wait: HistogramSnapshot,
    event_commit: HistogramSnapshot,
}

/// Figures the monitor loop gathers between statistics lines.
struct Gauges<'a> {
    cpu: CpuSplit,
    rss: u64,
    uptime: Duration,
    loop_lag_ms: u64,
    watchdog_withheld: u64,
    retention_max_ms: u64,
    selfcheck: &'a SelfCheckStats,
}

/// Logs statistics, pings the watchdog, runs retention and the self-check until a signal.
async fn monitor(daemon: &mut Daemon, args: &Args) -> anyhow::Result<()> {
    let mut cpu = CpuMeter::new().context("reading /proc/self/stat")?;
    let mut stats_ticker = tokio::time::interval(args.stats_interval);
    stats_ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let watchdog_interval = systemd::watchdog_interval();
    let mut watchdog_ticker =
        tokio::time::interval(watchdog_interval.unwrap_or(DEFAULT_WATCHDOG_INTERVAL));
    watchdog_ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut retention_ticker = tokio::time::interval(RETENTION_INTERVAL);
    retention_ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut selfcheck = SelfCheck::new(daemon.streams.iter().map(|t| t.key.clone()).collect());
    let selfcheck_stats = selfcheck.stats();
    let mut selfcheck_ticker = tokio::time::interval(selfcheck.interval(args.selfcheck_cycle));
    selfcheck_ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
    stats_ticker.tick().await;
    watchdog_ticker.tick().await;
    retention_ticker.tick().await;
    selfcheck_ticker.tick().await;
    let started = Instant::now();
    let age_limits = age_limits(&daemon.config);
    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .context("installing the SIGTERM handler")?;
    let mut failure = None;
    let mut snapshots = Snapshots::default();
    let mut watchdog_withheld = 0u64;
    let retention_max = Peak::new();

    loop {
        tokio::select! {
            _ = stats_ticker.tick() => {
                let gauges = Gauges {
                    cpu: cpu.sample().unwrap_or_default(),
                    rss: proc::rss_bytes().unwrap_or(0),
                    uptime: started.elapsed(),
                    loop_lag_ms: daemon.loop_lag.take(),
                    watchdog_withheld,
                    retention_max_ms: retention_max.take(),
                    selfcheck: &selfcheck_stats,
                };
                report(
                    &daemon.streams,
                    &mut daemon.events,
                    &daemon.writer,
                    &daemon.catalog,
                    &daemon.config.storage.video_roots,
                    &mut snapshots,
                    &gauges,
                )
                .await;
            }
            _ = watchdog_ticker.tick() => {
                if all_streams_progressed(&mut daemon.streams) {
                    if watchdog_interval.is_some() {
                        systemd::watchdog();
                    }
                } else {
                    watchdog_withheld += 1;
                    error!(
                        withheld = watchdog_withheld,
                        "an ingest task made no progress since the last check; withholding the watchdog ping"
                    );
                }
            }
            _ = retention_ticker.tick() => {
                retention_tick(&daemon.catalog, &daemon.config, &age_limits, &retention_max).await;
            }
            _ = selfcheck_ticker.tick() => {
                selfcheck.launch_next(&daemon.catalog);
            }
            result = &mut daemon.api => {
                match result {
                    Ok(Ok(())) => warn!("API server stopped"),
                    Ok(Err(err)) => error!(%err, "API server failed"),
                    Err(err) => error!(%err, "API task panicked"),
                }
                failure = Some("API server stopped unexpectedly");
                break;
            }
            _ = sigterm.recv() => {
                info!("SIGTERM; stopping");
                break;
            }
            result = tokio::signal::ctrl_c() => {
                if let Err(err) = result {
                    warn!(%err, "cannot listen for Ctrl-C; stopping");
                }
                info!("interrupted; stopping");
                break;
            }
        }
    }
    failure.map_or(Ok(()), |reason| Err(anyhow::anyhow!(reason)))
}

/// Per-camera age limits from the configuration.
fn age_limits(config: &Config) -> HashMap<CameraId, u32> {
    config
        .cameras
        .iter()
        .filter_map(|c| c.retention_days.map(|d| (c.id.clone(), d)))
        .collect()
}

/// Runs one retention pass and records how long it took.
async fn retention_tick(
    catalog: &CatalogHandle,
    config: &Config,
    age_limits: &HashMap<CameraId, u32>,
    retention_max: &Peak,
) {
    let storage = &config.storage;
    match retention::pass(
        catalog,
        &storage.video_roots,
        storage.min_free_percent,
        storage.quota_bytes(),
        age_limits,
    )
    .await
    {
        Ok(report) => {
            retention_max.observe(u64::try_from(report.duration.as_millis()).unwrap_or(u64::MAX));
        }
        Err(err) => warn!(%err, "retention pass failed"),
    }
}

/// True when every stream produced frames or reconnected since the last call.
fn all_streams_progressed(streams: &mut [StreamTask]) -> bool {
    let mut all = true;
    for task in streams {
        let now = (task.stats.frames(), task.stats.reconnects());
        if now == task.last_progress && !task.handle.is_finished() {
            all = false;
        }
        if task.handle.is_finished() {
            all = false;
        }
        task.last_progress = now;
    }
    all
}

/// Seconds between `then` and `now` with one decimal, or `-` when `then` is unknown.
fn age(now: WallUs, then: Option<WallUs>) -> String {
    then.map_or_else(
        || "-".to_owned(),
        |t| format!("{:.1}", (now.get() - t.get()).max(0) as f64 / 1_000_000.0),
    )
}

/// Milliseconds with one decimal.
fn ms(duration: Duration) -> String {
    format!("{:.1}", duration.as_secs_f64() * 1_000.0)
}

/// Per-interval p50, p99, and the interval maximum of a histogram, in milliseconds.
fn latency(
    histogram: &iris_core::metrics::Histogram,
    seen: &mut HistogramSnapshot,
) -> (String, String, String) {
    let snapshot = histogram.snapshot();
    let interval = snapshot.since(seen);
    *seen = snapshot;
    (
        ms(interval.percentile(50.0)),
        ms(interval.percentile(99.0)),
        ms(histogram.take_max()),
    )
}

/// Writes the statistics lines and the systemd status.
async fn report(
    streams: &[StreamTask],
    events: &mut [EventTask],
    writer: &WriterHandle,
    catalog: &CatalogHandle,
    roots: &[PathBuf],
    prev: &mut Snapshots,
    gauges: &Gauges<'_>,
) {
    let now = WallUs::now();
    log_streams(streams, now);
    log_events(events, now);
    log_writer(&writer.stats(), prev);
    log_catalog(catalog, prev).await;
    log_roots(roots);
    log_process(gauges);
    systemd::status(&format!(
        "recording {} streams, {:.2} % CPU, {:.0} MiB, up {} min",
        streams.len(),
        gauges.cpu.total,
        gauges.rss as f64 / MIB,
        gauges.uptime.as_secs() / 60
    ));
}

fn log_streams(streams: &[StreamTask], now: WallUs) {
    for task in streams {
        let s = &task.stats;
        let fps = s.nominal_ticks().map_or_else(
            || "-".to_owned(),
            |n| format!("{:.2}", TICKS_PER_SEC / f64::from(n)),
        );
        let cam_offset_ms = s
            .camera_offset_us()
            .map_or_else(|| "-".to_owned(), |us| (us / 1_000).to_string());
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
            holds = s.holds(),
            hold_s = format_args!("{:.1}", s.hold_ticks() as f64 / TICKS_PER_SEC),
            fps = %fps,
            err_ms = s.error_ticks() / 90,
            frag_kib_max = s.fragment_bytes_peak().take() >> 10,
            frag_s_max = format_args!("{:.1}", s.fragment_ticks_peak().take() as f64 / TICKS_PER_SEC),
            srs = s.sender_reports(),
            cam_offset_ms = %cam_offset_ms,
            gaps = s.gaps(),
            idle_s = %age(now, s.last_frame_wall()),
            codec = s.codec_description().as_deref().unwrap_or("-"),
            "stream"
        );
    }
}

fn log_events(events: &mut [EventTask], now: WallUs) {
    for task in events {
        let s = &task.stats;
        let pull = s.pull_latency().snapshot();
        let interval = pull.since(&task.pull_seen);
        task.pull_seen = pull;
        let (pull_p99_ms, pull_max_ms) = if interval.count() == 0 {
            ("-".to_owned(), "-".to_owned())
        } else {
            (
                ms(interval.percentile(99.0)),
                ms(s.pull_latency().take_max()),
            )
        };
        info!(
            camera = %task.camera,
            source = ?task.source,
            connected = s.connected(),
            events = s.events(),
            messages = s.messages(),
            reconnects = s.reconnects(),
            renewals = s.renewals(),
            errors = s.errors(),
            idle_s = %age(now, s.last_message_wall()),
            pull_p99_ms = %pull_p99_ms,
            pull_max_ms = %pull_max_ms,
            last_event = s.last_event_wall().map_or_else(|| "-".to_owned(), |w| w.to_string()),
            "events"
        );
    }
}

fn log_writer(writer: &WriterStats, prev: &mut Snapshots) {
    let (write_p50_ms, write_p99_ms, write_max_ms) =
        latency(writer.write_latency(), &mut prev.write);
    let (sync_p50_ms, sync_p99_ms, sync_max_ms) = latency(writer.sync_latency(), &mut prev.sync);
    info!(
        fragments = writer.fragments(),
        mib = format_args!("{:.1}", writer.bytes() as f64 / MIB),
        syncs = writer.syncs(),
        dropped = writer.dropped(),
        write_errors = writer.write_errors(),
        files = writer.files_opened(),
        queued = writer.queued(),
        queue_peak = writer.queue_peak().take(),
        queue_mib_peak = format_args!("{:.1}", writer.queue_bytes_peak().take() as f64 / MIB),
        write_p50_ms = %write_p50_ms,
        write_p99_ms = %write_p99_ms,
        write_max_ms = %write_max_ms,
        sync_p50_ms = %sync_p50_ms,
        sync_p99_ms = %sync_p99_ms,
        sync_max_ms = %sync_max_ms,
        slow_io = writer.slow_writes(),
        "writer"
    );
}

async fn log_catalog(catalog: &CatalogHandle, prev: &mut Snapshots) {
    let metrics = catalog.metrics();
    let stats = catalog.stats().await.ok();
    let (_, txn_p99_ms, txn_max_ms) = latency(metrics.transaction(), &mut prev.transaction);
    let (_, query_p99_ms, query_max_ms) = latency(metrics.query(), &mut prev.query);
    let (_, wait_p99_ms, wait_max_ms) = latency(metrics.wait(), &mut prev.wait);
    let (_, event_commit_p99_ms, event_commit_max_ms) =
        latency(metrics.event_commit(), &mut prev.event_commit);
    info!(
        segments = stats.as_ref().map_or(0, |c| c.segments),
        gib = format_args!("{:.2}", stats.as_ref().map_or(0.0, |c| c.sealed_bytes as f64 / GIB)),
        events = stats.as_ref().map_or(0, |c| c.events),
        db_mib = format_args!("{:.1}", stats.as_ref().map_or(0.0, |c| c.db_bytes as f64 / MIB)),
        wal_mib = format_args!("{:.1}", stats.as_ref().map_or(0.0, |c| c.wal_bytes as f64 / MIB)),
        batches = metrics.batches(),
        changes = metrics.changes(),
        batch_peak = metrics.batch_peak().take(),
        failures = metrics.failures(),
        txn_p99_ms = %txn_p99_ms,
        txn_max_ms = %txn_max_ms,
        query_p99_ms = %query_p99_ms,
        query_max_ms = %query_max_ms,
        wait_p99_ms = %wait_p99_ms,
        wait_max_ms = %wait_max_ms,
        event_commit_p99_ms = %event_commit_p99_ms,
        event_commit_max_ms = %event_commit_max_ms,
        "catalog"
    );
}

fn log_roots(roots: &[PathBuf]) {
    for root in roots {
        match free_space(root) {
            Ok(space) => info!(
                root = %root.display(),
                free_gib = format_args!("{:.1}", space.available as f64 / GIB),
                free_pct = format_args!("{:.1}", retention::free_percent(&space)),
                "root"
            ),
            Err(err) => warn!(root = %root.display(), %err, "cannot read free space"),
        }
    }
}

fn log_process(gauges: &Gauges<'_>) {
    info!(
        cpu_pct = format_args!("{:.2}", gauges.cpu.total),
        cpu_user_pct = format_args!("{:.2}", gauges.cpu.user),
        cpu_sys_pct = format_args!("{:.2}", gauges.cpu.system),
        rss_mib = format_args!("{:.1}", gauges.rss as f64 / MIB),
        threads = proc::threads().unwrap_or(0),
        fds = proc::fd_count().unwrap_or(0),
        uptime_min = gauges.uptime.as_secs() / 60,
        loop_lag_max_ms = gauges.loop_lag_ms,
        watchdog_withheld = gauges.watchdog_withheld,
        retention_max_ms = gauges.retention_max_ms,
        selfcheck_ok = gauges.selfcheck.ok(),
        selfcheck_failed = gauges.selfcheck.failed(),
        selfcheck_skipped = gauges.selfcheck.skipped(),
        "process"
    );
}

/// Stops everything in order: ingest, writer, API, catalog.
async fn stop(mut daemon: Daemon) -> anyhow::Result<()> {
    systemd::stopping();
    daemon.shutdown.cancel();
    for task in &mut daemon.streams {
        match tokio::time::timeout(SHUTDOWN_GRACE, &mut task.handle).await {
            Ok(Ok(())) => {}
            Ok(Err(err)) => warn!(stream = %task.key, %err, "stream task failed"),
            Err(_) => {
                warn!(stream = %task.key, "stream task did not stop in time; aborting");
                task.handle.abort();
            }
        }
    }
    for task in &mut daemon.events {
        if tokio::time::timeout(SHUTDOWN_GRACE, &mut task.handle)
            .await
            .is_err()
        {
            warn!(camera = %task.camera, "event task did not stop in time; aborting");
            task.handle.abort();
        }
    }
    let writer = daemon.writer.clone();
    tokio::task::spawn_blocking(move || writer.shutdown())
        .await
        .context("writer thread panicked")?
        .context("sealing recordings")?;
    if tokio::time::timeout(SHUTDOWN_GRACE, &mut daemon.api)
        .await
        .is_err()
    {
        daemon.api.abort();
    }
    let catalog = daemon.catalog.clone();
    tokio::task::spawn_blocking(move || catalog.shutdown())
        .await
        .context("catalog thread panicked")?;
    info!("stopped");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn camera(url: &str, port: u16) -> CameraConfig {
        let text = format!(
            "[[camera]]\nid=\"cam1\"\nname=\"n\"\nrtsp_url=\"{url}\"\nusername=\"u\"\npassword=\"p\"\nhttp_port={port}\n"
        );
        Config::from_toml_str(&text).unwrap().cameras.remove(0)
    }

    #[test]
    fn endpoint_comes_from_the_rtsp_host_and_http_port() {
        let e = endpoint(&camera(
            "rtsp://192.0.2.10:554/cam/realmonitor?channel=1&subtype=0",
            8080,
        ))
        .unwrap();
        assert_eq!(
            (e.host.as_str(), e.http_port, e.username.as_str()),
            ("192.0.2.10", 8080, "u")
        );
        assert!(endpoint(&camera("not a url", 80)).is_none());
    }

    #[test]
    fn sandbox_spec_covers_roots_state_and_ports() {
        let config = Config::from_toml_str(
            "[storage]\nvideo_roots=[\"/tmp/irisd-test-root\"]\nstate_dir=\"/tmp/irisd-test-state\"\n[api]\nlisten=\"127.0.0.1:9000\"\n[[camera]]\nid=\"a\"\nname=\"\"\nrtsp_url=\"rtsp://192.0.2.1/x\"\nusername=\"\"\npassword=\"\"\nhttp_port=8080\n[[camera]]\nid=\"b\"\nname=\"\"\nrtsp_url=\"rtsp://192.0.2.2:18554/x\"\nsub_url=\"rtsp://192.0.2.2/y\"\nusername=\"\"\npassword=\"\"\n",
        )
        .unwrap();
        let spec = sandbox_spec(&config, Path::new("/etc/irisd/irisd.toml"));
        assert_eq!(
            spec.read_write,
            vec![
                PathBuf::from("/tmp/irisd-test-root"),
                PathBuf::from("/tmp/irisd-test-state")
            ]
        );
        assert_eq!(spec.connect_ports, vec![80, 554, 8080, 18554]);
        assert_eq!(spec.bind_ports, vec![9000]);
        assert!(spec.read_only.iter().all(|p| p.exists()));
    }
}
