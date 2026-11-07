//! `irisd run`: the daemon. Records every configured stream, keeps the
//! catalog, serves the API, subscribes to camera events, enforces retention,
//! and reports to systemd.

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, bail};
use clap::Args as ClapArgs;
use iris_api::{ActiveCamera, ApiState, ApiStats, LiveRegistry, SnapshotSource};
use iris_catalog::{CatalogHandle, CatalogRootExpectation, CatalogRootRegistry};
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
use iris_store::{
    ApprovedRoot, RootExpectation, RootIdentity, StoragePolicy, WriterHandle, WriterStats,
    admit_recording_root, free_space,
};
use tokio::sync::{broadcast, oneshot};
use tokio::task::JoinHandle;
use tokio::time::MissedTickBehavior;
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

use crate::catalog_observer::{CatalogEventSink, CatalogObserver};
use crate::proc::{self, CpuMeter, CpuSplit};
use crate::sandbox::SandboxSpec;
use crate::selfcheck::SelfCheck;
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
/// Total daemon shutdown budget, kept below systemd's 30-second stop timeout.
const SHUTDOWN_BUDGET: Duration = Duration::from_secs(25);
/// Shared grace for all async producers and maintenance owners.
const PRODUCER_GRACE: Duration = Duration::from_secs(10);
/// Tail of the producer grace reserved for observing aborted owners.
const ABORT_OBSERVATION_GRACE: Duration = Duration::from_secs(1);
/// Writer sealing receives the next ten seconds of the absolute budget.
const WRITER_DEADLINE: Duration = Duration::from_secs(20);
/// The API receives two seconds after writer sealing.
const API_DEADLINE: Duration = Duration::from_secs(22);
/// A statistics or retention job is cancelled after one period of bounded work.
const MAINTENANCE_TIMEOUT: Duration = Duration::from_secs(30);
/// Cadence for extending startup while catalog ownership reconciliation runs.
const STARTUP_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(5);

#[derive(Clone, Copy)]
struct ShutdownSchedule {
    producer_grace: Duration,
    abort_observation_grace: Duration,
    writer_deadline: Duration,
    api_deadline: Duration,
    budget: Duration,
}

const SHUTDOWN_SCHEDULE: ShutdownSchedule = ShutdownSchedule {
    producer_grace: PRODUCER_GRACE,
    abort_observation_grace: ABORT_OBSERVATION_GRACE,
    writer_deadline: WRITER_DEADLINE,
    api_deadline: API_DEADLINE,
    budget: SHUTDOWN_BUDGET,
};

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
    joined: bool,
}

struct EventTask {
    camera: CameraId,
    source: EventSource,
    stats: Arc<EventStats>,
    handle: JoinHandle<()>,
    joined: bool,
}

/// Everything the monitor loop needs.
struct Daemon {
    config: Config,
    approved_roots: Vec<ApprovedRoot>,
    catalog: CatalogHandle,
    writer: WriterHandle,
    streams: Vec<StreamTask>,
    events: Vec<EventTask>,
    api: Option<JoinHandle<io::Result<()>>>,
    shutdown: CancellationToken,
    /// Largest timer lateness seen on the runtime thread since the last statistics line.
    loop_lag: Arc<Peak>,
    loop_probe: JoinHandle<()>,
}

/// Fallible control-plane state constructed before recording workers or readiness.
struct MonitorState {
    cpu: CpuMeter,
    stats_ticker: tokio::time::Interval,
    watchdog_interval: Option<Duration>,
    watchdog_ticker: tokio::time::Interval,
    retention_ticker: tokio::time::Interval,
    selfcheck_ticker: tokio::time::Interval,
    selfcheck: SelfCheck,
    sigterm: tokio::signal::unix::Signal,
    sigint: tokio::signal::unix::Signal,
    started: Instant,
}

impl MonitorState {
    fn new(config: &Config, args: &Args, shutdown: &CancellationToken) -> anyhow::Result<Self> {
        Self::new_with_cpu(config, args, shutdown, CpuMeter::new)
    }

    fn new_with_cpu<F>(
        config: &Config,
        args: &Args,
        shutdown: &CancellationToken,
        cpu_meter: F,
    ) -> anyhow::Result<Self>
    where
        F: FnOnce() -> io::Result<CpuMeter>,
    {
        Self::new_with_initializers(
            config,
            args,
            shutdown,
            cpu_meter,
            tokio::signal::unix::signal,
        )
    }

    fn new_with_initializers<F, S>(
        config: &Config,
        args: &Args,
        shutdown: &CancellationToken,
        cpu_meter: F,
        mut signal: S,
    ) -> anyhow::Result<Self>
    where
        F: FnOnce() -> io::Result<CpuMeter>,
        S: FnMut(tokio::signal::unix::SignalKind) -> io::Result<tokio::signal::unix::Signal>,
    {
        validate_period("--stats-interval", args.stats_interval)?;
        let stream_keys = sources(config)
            .into_iter()
            .map(|(camera, kind, _)| StreamKey {
                camera: camera.id.clone(),
                kind,
            })
            .collect();
        let selfcheck = SelfCheck::new(stream_keys, shutdown.clone());
        let selfcheck_interval = selfcheck
            .interval(args.selfcheck_cycle)
            .context("--selfcheck-cycle is too short for the configured stream count")?;
        let watchdog_interval = systemd::watchdog_interval();
        let watchdog_period = watchdog_interval.unwrap_or(DEFAULT_WATCHDOG_INTERVAL);
        validate_period("systemd watchdog interval", watchdog_period)?;
        let now = tokio::time::Instant::now();
        let mut stats_ticker =
            tokio::time::interval_at(now + args.stats_interval, args.stats_interval);
        stats_ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
        let mut watchdog_ticker = tokio::time::interval_at(now + watchdog_period, watchdog_period);
        watchdog_ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
        let mut retention_ticker =
            tokio::time::interval_at(now + RETENTION_INTERVAL, RETENTION_INTERVAL);
        retention_ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
        let mut selfcheck_ticker =
            tokio::time::interval_at(now + selfcheck_interval, selfcheck_interval);
        selfcheck_ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
        let cpu = cpu_meter().context("reading /proc/self/stat")?;
        let sigterm = signal(tokio::signal::unix::SignalKind::terminate())
            .context("installing the SIGTERM handler")?;
        let sigint = signal(tokio::signal::unix::SignalKind::interrupt())
            .context("installing the SIGINT handler")?;
        Ok(Self {
            cpu,
            stats_ticker,
            watchdog_interval,
            watchdog_ticker,
            retention_ticker,
            selfcheck_ticker,
            selfcheck,
            sigterm,
            sigint,
            started: Instant::now(),
        })
    }
}

/// Runs the daemon until SIGTERM or Ctrl-C.
pub async fn run(args: Args) -> anyhow::Result<()> {
    run_with_monitor_initializer(args, MonitorState::new).await
}

async fn run_with_monitor_initializer<F>(args: Args, monitor_initializer: F) -> anyhow::Result<()>
where
    F: FnOnce(&Config, &Args, &CancellationToken) -> anyhow::Result<MonitorState>,
{
    let mut config =
        Config::load(&args.config).with_context(|| format!("loading {}", args.config.display()))?;
    validate(&config)?;
    validate_period("--stats-interval", args.stats_interval)?;
    let source_count = validate_source_urls(&config)?;
    checked_queue_capacity(config.storage.max_queued_fragments, source_count)?;
    checked_fragment_ticks(config.storage.max_fragment_ms)?;
    let shutdown = CancellationToken::new();
    let mut monitor_state = monitor_initializer(&config, &args, &shutdown)?;
    std::fs::create_dir_all(&config.storage.state_dir)
        .with_context(|| format!("creating {}", config.storage.state_dir.display()))?;
    let approved_roots = admit_configured_roots(
        &config.storage.state_dir.join("irisd.db"),
        &config.storage.video_roots,
    )?;
    if approved_roots.is_empty() {
        bail!("no recording root could be admitted");
    }
    config.storage.video_roots = approved_roots
        .iter()
        .map(|root| root.path().to_path_buf())
        .collect();

    if args.no_sandbox {
        warn!("sandbox disabled by --no-sandbox");
    } else {
        // Before any thread exists: threads inherit the Landlock domain of their creator.
        sandbox::apply(&sandbox_spec(&config, &args.config))?;
    }

    let catalog = CatalogHandle::open(&config.storage.state_dir.join("irisd.db"))
        .context("opening the catalog")?;
    for root in &approved_roots {
        catalog
            .register_recording_root(root.path(), &root.identity().to_string())
            .await
            .with_context(|| format!("registering recording root {}", root.path().display()))?;
    }
    with_startup_heartbeat(
        catalog.configure_video_roots(&config.storage.video_roots),
        STARTUP_HEARTBEAT_INTERVAL,
        || {
            systemd::extend_timeout(Duration::from_secs(60));
            systemd::status("assigning catalog rows to recording roots");
        },
    )
    .await
    .context("registering recording roots in the catalog")?;
    for camera in &config.cameras {
        catalog
            .ensure_camera(&camera.id, &camera.name, camera.vendor.as_deref())
            .await
            .with_context(|| format!("registering camera {}", camera.id))?;
    }
    let finished_deletions = adopt::finish_deleting(&catalog, &approved_roots).await?;
    let recovered = adopt::recover_writing(&catalog, &approved_roots).await?;
    let adopted = adopt::adopt_unknown(&catalog, &approved_roots).await?;
    info!(
        finished_deletions,
        recovered, adopted, "catalog reconciled with the recording roots"
    );
    let api_listener = iris_api::bind(config.api.listen)
        .await
        .with_context(|| format!("binding API listener {}", config.api.listen))?;

    let loop_lag = Arc::new(Peak::new());
    let loop_probe = tokio::spawn(probe_loop_lag(Arc::clone(&loop_lag), shutdown.clone()));
    let (writer, streams, live) = start_recording(&config, &approved_roots, &catalog, &shutdown)?;
    let (events_tx, _) = broadcast::channel::<Arc<Event>>(EVENT_CHANNEL);
    let events = start_events(&config, &catalog, &events_tx, &shutdown);
    let api = start_api(&config, &catalog, live, events_tx, api_listener, &shutdown);

    let mut daemon = Daemon {
        config,
        approved_roots,
        catalog,
        writer,
        streams,
        events,
        api: Some(api),
        shutdown,
        loop_lag,
        loop_probe,
    };
    let mut maintenance = MaintenanceJobs::new(&daemon);

    systemd::ready();
    systemd::status("recording");
    info!(listen = %daemon.config.api.listen, cameras = daemon.config.cameras.len(), "irisd running");

    let outcome = monitor(&mut daemon, &mut monitor_state, &mut maintenance).await;
    let shutdown = stop(daemon, monitor_state, maintenance).await;
    combine_run_outcomes(outcome, shutdown)
}

fn combine_run_outcomes(
    monitor: anyhow::Result<()>,
    shutdown: anyhow::Result<()>,
) -> anyhow::Result<()> {
    match (monitor, shutdown) {
        (Err(primary), Err(shutdown)) => {
            error!(%shutdown, "shutdown also failed after the daemon failure");
            Err(primary)
        }
        (Err(primary), Ok(())) => Err(primary),
        (Ok(()), result) => result,
    }
}

async fn with_startup_heartbeat<F, H>(future: F, interval: Duration, mut heartbeat: H) -> F::Output
where
    F: Future,
    H: FnMut(),
{
    let mut ticker = tokio::time::interval_at(tokio::time::Instant::now() + interval, interval);
    ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
    tokio::pin!(future);
    loop {
        tokio::select! {
            result = &mut future => return result,
            _ = ticker.tick() => heartbeat(),
        }
    }
}

fn admit_configured_roots(
    catalog_path: &Path,
    configured: &[PathBuf],
) -> anyhow::Result<Vec<ApprovedRoot>> {
    let mut registry =
        CatalogRootRegistry::open(catalog_path).context("opening the recording-root registry")?;
    let mut pages = 0usize;
    let expectations = registry
        .expectations_with_progress(configured, || {
            pages = pages.saturating_add(1);
            systemd::extend_timeout(Duration::from_secs(60));
            systemd::status(&format!(
                "checking recording-root ownership: {pages} catalog pages"
            ));
        })
        .context("checking recording-root ownership")?;
    drop(registry);

    let mut identities = HashSet::new();
    let mut physical_roots = HashSet::new();
    let mut approved = Vec::with_capacity(configured.len());
    for (path, expectation) in configured.iter().zip(expectations) {
        let expectation = match expectation {
            CatalogRootExpectation::Known(identity) => RootExpectation::Known(
                identity
                    .parse::<RootIdentity>()
                    .with_context(|| format!("invalid catalog identity for {}", path.display()))?,
            ),
            CatalogRootExpectation::LegacyOwned => RootExpectation::LegacyOwned,
            CatalogRootExpectation::Fresh => RootExpectation::Fresh,
        };
        let root = match admit_recording_root(path, expectation) {
            Ok(root) => root,
            Err(error) => {
                warn!(root = %path.display(), %error, "recording root unavailable at startup");
                continue;
            }
        };
        if identities.contains(&root.identity())
            || physical_roots.contains(&root.physical_identity())
        {
            warn!(root = %path.display(), "recording root aliases another configured root; skipping");
            continue;
        }
        identities.insert(root.identity());
        physical_roots.insert(root.physical_identity());
        approved.push(root);
    }
    Ok(approved)
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

fn validate_period(name: &str, period: Duration) -> anyhow::Result<()> {
    if period.is_zero() {
        bail!("{name} must be greater than zero");
    }
    Ok(())
}

fn validate_source_urls(config: &Config) -> anyhow::Result<usize> {
    let sources = sources(config);
    for (camera, kind, url) in &sources {
        url.parse::<iris_ingest::Url>()
            .with_context(|| format!("parsing camera {} {kind} URL", camera.id))?;
    }
    Ok(sources.len())
}

pub(crate) fn checked_queue_capacity(
    per_stream: usize,
    source_count: usize,
) -> anyhow::Result<usize> {
    if source_count == 0 {
        bail!("no streams to record");
    }
    per_stream
        .max(1)
        .checked_mul(source_count)
        .context("writer queue capacity exceeds this platform's limit")
}

pub(crate) fn checked_fragment_ticks(millis: u64) -> anyhow::Result<MediaTicks> {
    MediaTicks::checked_from_millis(millis)
        .filter(|ticks| ticks.get() > 0)
        .context("storage.max_fragment_ms cannot be represented as positive media ticks")
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
    approved_roots: &[ApprovedRoot],
    catalog: &CatalogHandle,
    shutdown: &CancellationToken,
) -> anyhow::Result<Recording> {
    let sources = sources(config);
    if sources.is_empty() {
        bail!("no streams to record");
    }
    let policy = StoragePolicy::from(&config.storage);
    let capacity = checked_queue_capacity(config.storage.max_queued_fragments, sources.len())?;
    let observer = Arc::new(CatalogObserver::new(catalog.clone()));
    let writer = WriterHandle::spawn_with_approved_roots(
        approved_roots.to_vec(),
        policy,
        capacity,
        config.storage.min_free_percent,
        observer,
    )
    .context("starting the writer thread")?;
    let sink: Arc<dyn FragmentSink> = Arc::new(WriterSink(writer.clone()));
    let live = LiveRegistry::new();
    let max_fragment = checked_fragment_ticks(config.storage.max_fragment_ms)?;

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
                .with_context(|| format!("parsing camera {} {kind} URL", camera.id))?,
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
            joined: false,
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
                joined: false,
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
    listener: tokio::net::TcpListener,
    shutdown: &CancellationToken,
) -> JoinHandle<io::Result<()>> {
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
    let active_cameras = config
        .cameras
        .iter()
        .map(|camera| {
            (
                camera.id.clone(),
                ActiveCamera {
                    id: camera.id.clone(),
                    name: camera.name.clone(),
                    vendor: camera.vendor.clone(),
                },
            )
        })
        .collect();
    let state = Arc::new(ApiState {
        catalog: catalog.clone(),
        live,
        events,
        tokens: config.api.tokens.clone(),
        snapshot_sources,
        active_cameras,
        stats: Arc::new(ApiStats::default()),
        started: WallUs::now(),
        started_monotonic: Instant::now(),
        shutdown: shutdown.clone(),
    });
    let router = iris_api::router(state);
    tokio::spawn(iris_api::serve_listener(listener, router, shutdown.clone()))
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

struct ReportStream {
    key: StreamKey,
    stats: Arc<StreamStats>,
}

struct ReportEvent {
    camera: CameraId,
    source: EventSource,
    stats: Arc<EventStats>,
    pull_seen: HistogramSnapshot,
}

struct ReportState {
    streams: Vec<ReportStream>,
    events: Vec<ReportEvent>,
    writer: WriterHandle,
    catalog: CatalogHandle,
    roots: Vec<PathBuf>,
    snapshots: Snapshots,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MaintenanceOutcome {
    Complete,
    TimedOut,
    Cancelled,
}

struct ReportCompletion {
    state: ReportState,
    outcome: MaintenanceOutcome,
}

struct ReportJob {
    idle: Option<ReportState>,
    active: Option<JoinHandle<ReportCompletion>>,
    shutdown: CancellationToken,
}

impl ReportJob {
    fn new(daemon: &Daemon) -> Self {
        Self {
            idle: Some(ReportState {
                streams: daemon
                    .streams
                    .iter()
                    .map(|task| ReportStream {
                        key: task.key.clone(),
                        stats: Arc::clone(&task.stats),
                    })
                    .collect(),
                events: daemon
                    .events
                    .iter()
                    .map(|task| ReportEvent {
                        camera: task.camera.clone(),
                        source: task.source,
                        stats: Arc::clone(&task.stats),
                        pull_seen: HistogramSnapshot::default(),
                    })
                    .collect(),
                writer: daemon.writer.clone(),
                catalog: daemon.catalog.clone(),
                roots: daemon.config.storage.video_roots.clone(),
                snapshots: Snapshots::default(),
            }),
            active: None,
            shutdown: daemon.shutdown.clone(),
        }
    }

    fn launch(&mut self, gauges: Gauges) {
        let Some(mut state) = self.idle.take() else {
            if self.active.is_none() {
                warn!("statistics report owner has no state");
            }
            return;
        };
        let shutdown = self.shutdown.clone();
        self.active = Some(tokio::spawn(async move {
            let outcome = {
                let work = report(&mut state, &gauges);
                tokio::pin!(work);
                tokio::select! {
                    () = shutdown.cancelled() => MaintenanceOutcome::Cancelled,
                    result = tokio::time::timeout(MAINTENANCE_TIMEOUT, &mut work) => {
                        if result.is_err() {
                            warn!("statistics report exceeded its work deadline");
                            MaintenanceOutcome::TimedOut
                        } else {
                            MaintenanceOutcome::Complete
                        }
                    }
                }
            };
            ReportCompletion { state, outcome }
        }));
    }

    fn running(&self) -> bool {
        self.active.is_some()
    }

    async fn wait_active(&mut self) -> Result<ReportCompletion, tokio::task::JoinError> {
        self.active
            .as_mut()
            .expect("report task exists while selected")
            .await
    }

    fn finish(
        &mut self,
        result: Result<ReportCompletion, tokio::task::JoinError>,
    ) -> Option<&'static str> {
        self.active.take();
        match result {
            Ok(completion) if completion.outcome == MaintenanceOutcome::Complete => {
                self.idle = Some(completion.state);
                None
            }
            Ok(completion) if completion.outcome == MaintenanceOutcome::TimedOut => {
                Some("statistics report exceeded its work deadline")
            }
            Ok(_) => Some("statistics report cancelled unexpectedly"),
            Err(err) => {
                error!(%err, "statistics report task failed");
                Some("statistics report task failed")
            }
        }
    }

    fn take_active(&mut self) -> Option<JoinHandle<ReportCompletion>> {
        self.shutdown.cancel();
        self.active.take()
    }
}

struct RetentionJobState {
    catalog: CatalogHandle,
    config: Config,
    approved_roots: Vec<ApprovedRoot>,
    age_limits: HashMap<CameraId, u32>,
    peak: Arc<Peak>,
    state: retention::RetentionState,
}

struct RetentionCompletion {
    state: RetentionJobState,
    outcome: MaintenanceOutcome,
}

struct RetentionJob {
    idle: Option<RetentionJobState>,
    active: Option<JoinHandle<RetentionCompletion>>,
    shutdown: CancellationToken,
}

impl RetentionJob {
    fn new(daemon: &Daemon, peak: Arc<Peak>) -> Self {
        Self {
            idle: Some(RetentionJobState {
                catalog: daemon.catalog.clone(),
                config: daemon.config.clone(),
                approved_roots: daemon.approved_roots.clone(),
                age_limits: age_limits(&daemon.config),
                peak,
                state: retention::RetentionState::default(),
            }),
            active: None,
            shutdown: daemon.shutdown.clone(),
        }
    }

    fn launch(&mut self) {
        let Some(mut state) = self.idle.take() else {
            if self.active.is_none() {
                warn!("retention owner has no state");
            }
            return;
        };
        let shutdown = self.shutdown.clone();
        self.active = Some(tokio::spawn(async move {
            let outcome = {
                let work = retention_tick(&mut state);
                tokio::pin!(work);
                tokio::select! {
                    () = shutdown.cancelled() => MaintenanceOutcome::Cancelled,
                    result = tokio::time::timeout(MAINTENANCE_TIMEOUT, &mut work) => {
                        if result.is_err() {
                            warn!("retention pass exceeded its work deadline");
                            MaintenanceOutcome::TimedOut
                        } else {
                            MaintenanceOutcome::Complete
                        }
                    }
                }
            };
            RetentionCompletion { state, outcome }
        }));
    }

    fn running(&self) -> bool {
        self.active.is_some()
    }

    async fn wait_active(&mut self) -> Result<RetentionCompletion, tokio::task::JoinError> {
        self.active
            .as_mut()
            .expect("retention task exists while selected")
            .await
    }

    fn finish(
        &mut self,
        result: Result<RetentionCompletion, tokio::task::JoinError>,
    ) -> Option<&'static str> {
        self.active.take();
        match result {
            Ok(completion) if completion.outcome == MaintenanceOutcome::Complete => {
                self.idle = Some(completion.state);
                None
            }
            Ok(completion) if completion.outcome == MaintenanceOutcome::TimedOut => {
                Some("retention pass exceeded its work deadline")
            }
            Ok(_) => Some("retention task cancelled unexpectedly"),
            Err(err) => {
                error!(%err, "retention task failed");
                Some("retention task failed")
            }
        }
    }

    fn take_active(&mut self) -> Option<JoinHandle<RetentionCompletion>> {
        self.shutdown.cancel();
        self.active.take()
    }
}

struct MaintenanceJobs {
    report: ReportJob,
    retention: RetentionJob,
    retention_max: Arc<Peak>,
}

impl MaintenanceJobs {
    fn new(daemon: &Daemon) -> Self {
        let retention_max = Arc::new(Peak::new());
        Self {
            report: ReportJob::new(daemon),
            retention: RetentionJob::new(daemon, Arc::clone(&retention_max)),
            retention_max,
        }
    }
}

/// Figures the monitor loop gathers between statistics lines.
struct Gauges {
    cpu: CpuSplit,
    rss: u64,
    uptime: Duration,
    loop_lag_ms: u64,
    watchdog_withheld: u64,
    retention_max_ms: u64,
    selfcheck_ok: u64,
    selfcheck_failed: u64,
    selfcheck_skipped: u64,
}

/// Logs statistics, pings the watchdog, runs retention and the self-check until a signal.
async fn monitor(
    daemon: &mut Daemon,
    state: &mut MonitorState,
    maintenance: &mut MaintenanceJobs,
) -> anyhow::Result<()> {
    let selfcheck_stats = state.selfcheck.stats();
    let mut failure = None;
    let mut watchdog_withheld = 0u64;

    loop {
        tokio::select! {
            _ = state.stats_ticker.tick() => {
                if !maintenance.report.running() {
                    let gauges = Gauges {
                        cpu: state.cpu.sample().unwrap_or_default(),
                        rss: proc::rss_bytes().unwrap_or(0),
                        uptime: state.started.elapsed(),
                        loop_lag_ms: daemon.loop_lag.take(),
                        watchdog_withheld,
                        retention_max_ms: maintenance.retention_max.take(),
                        selfcheck_ok: selfcheck_stats.ok(),
                        selfcheck_failed: selfcheck_stats.failed(),
                        selfcheck_skipped: selfcheck_stats.skipped(),
                    };
                    maintenance.report.launch(gauges);
                }
            }
            _ = state.watchdog_ticker.tick() => {
                if all_streams_progressed(&mut daemon.streams) {
                    if state.watchdog_interval.is_some() {
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
            _ = state.retention_ticker.tick() => {
                maintenance.retention.launch();
            }
            _ = state.selfcheck_ticker.tick() => {
                state.selfcheck.launch_next(&daemon.catalog).await;
            }
            result = maintenance.report.wait_active(), if maintenance.report.running() => {
                if let Some(reason) = maintenance.report.finish(result) {
                    failure = Some(reason);
                    break;
                }
            }
            result = maintenance.retention.wait_active(), if maintenance.retention.running() => {
                if let Some(reason) = maintenance.retention.finish(result) {
                    failure = Some(reason);
                    break;
                }
            }
            result = daemon.api.as_mut().expect("API task exists until it stops") => {
                match result {
                    Ok(Ok(())) => warn!("API server stopped"),
                    Ok(Err(err)) => error!(%err, "API server failed"),
                    Err(err) => error!(%err, "API task panicked"),
                }
                failure = Some("API server stopped unexpectedly");
                daemon.api.take();
                break;
            }
            _ = state.sigterm.recv() => {
                info!("SIGTERM; stopping");
                break;
            }
            _ = state.sigint.recv() => {
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
async fn retention_tick(job: &mut RetentionJobState) {
    let storage = &job.config.storage;
    match retention::pass_approved(
        &job.catalog,
        &job.approved_roots,
        &mut job.state,
        storage.min_free_percent,
        storage.quota_bytes(),
        &job.age_limits,
    )
    .await
    {
        Ok(report) => {
            job.peak
                .observe(u64::try_from(report.duration.as_millis()).unwrap_or(u64::MAX));
        }
        Err(err) => warn!(?err, "retention pass failed"),
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
async fn report(state: &mut ReportState, gauges: &Gauges) {
    let now = WallUs::now();
    log_streams(&state.streams, now);
    log_events(&mut state.events, now);
    log_writer(&state.writer.stats(), &mut state.snapshots);
    log_catalog(&state.catalog, &mut state.snapshots).await;
    log_roots(&state.roots).await;
    log_process(gauges);
    systemd::status(&format!(
        "recording {} streams, {:.2} % CPU, {:.0} MiB, up {} min",
        state.streams.len(),
        gauges.cpu.total,
        gauges.rss as f64 / MIB,
        gauges.uptime.as_secs() / 60
    ));
}

fn log_streams(streams: &[ReportStream], now: WallUs) {
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

fn log_events(events: &mut [ReportEvent], now: WallUs) {
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
        event_mib = format_args!("{:.1}", stats.as_ref().map_or(0.0, |c| c.event_bytes as f64 / MIB)),
        retained_events = stats.as_ref().map_or(0, |c| c.retained_events),
        pruned_events = stats.as_ref().map_or(0, |c| c.pruned_events),
        db_mib = format_args!("{:.1}", stats.as_ref().map_or(0.0, |c| c.db_bytes as f64 / MIB)),
        wal_mib = format_args!("{:.1}", stats.as_ref().map_or(0.0, |c| c.wal_bytes as f64 / MIB)),
        queued = metrics.queue_depth(),
        queue_mib = format_args!("{:.1}", metrics.queue_bytes() as f64 / MIB),
        queue_peak = metrics.queue_peak().take(),
        queue_mib_peak = format_args!("{:.1}", metrics.queue_bytes_peak().take() as f64 / MIB),
        coalesced = metrics.coalesced(),
        rejected = metrics.rejected(),
        rejected_events = metrics.rejected_events(),
        rejected_progress = metrics.rejected_progress(),
        rejected_queries = metrics.rejected_queries(),
        rejected_critical = metrics.rejected_critical(),
        reconcile_required = metrics.reconciliation_required(),
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

async fn log_roots(roots: &[PathBuf]) {
    log_roots_with(roots, |root| free_space(&root)).await;
}

async fn log_roots_with<F>(roots: &[PathBuf], probe: F)
where
    F: Fn(PathBuf) -> io::Result<iris_store::FreeSpace> + Send + 'static,
{
    let roots = roots.to_vec();
    let (result_tx, result_rx) = oneshot::channel();
    // `statvfs` can remain uninterruptible in the kernel. The report owner is
    // single-flight; if it times out, this one standard-library thread cannot
    // hold process exit and no replacement report is launched.
    let thread = std::thread::Builder::new()
        .name("irisd-root-probe".into())
        .spawn(move || {
            let results = roots
                .into_iter()
                .map(|root| {
                    let result = probe(root.clone());
                    (root, result)
                })
                .collect::<Vec<_>>();
            let _ = result_tx.send(results);
        });
    let thread = match thread {
        Ok(thread) => thread,
        Err(err) => {
            warn!(%err, "cannot start recording-root probe");
            return;
        }
    };
    let Ok(results) = result_rx.await else {
        let _ = thread.join();
        warn!("recording-root probe stopped without a result");
        return;
    };
    if thread.join().is_err() {
        warn!("recording-root probe thread panicked after returning a result");
        return;
    }
    for (root, result) in results {
        match result {
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

fn log_process(gauges: &Gauges) {
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
        selfcheck_ok = gauges.selfcheck_ok,
        selfcheck_failed = gauges.selfcheck_failed,
        selfcheck_skipped = gauges.selfcheck_skipped,
        "process"
    );
}

fn record_shutdown_error(first: &mut Option<anyhow::Error>, error: anyhow::Error) {
    error!(%error, "shutdown stage failed");
    if first.is_none() {
        *first = Some(error);
    }
}

pub(crate) fn run_on_shutdown_thread<T, F>(
    name: &str,
    operation: F,
) -> io::Result<oneshot::Receiver<T>>
where
    T: Send + 'static,
    F: FnOnce() -> T + Send + 'static,
{
    let (result_tx, result_rx) = oneshot::channel();
    std::thread::Builder::new()
        .name(name.to_owned())
        .spawn(move || {
            let _ = result_tx.send(operation());
        })?;
    Ok(result_rx)
}

pub(crate) async fn complete_by_deadline<F>(
    deadline: tokio::time::Instant,
    future: F,
) -> Option<F::Output>
where
    F: Future,
{
    tokio::time::timeout_at(deadline, future).await.ok()
}

/// Stops everything in order: ingest, writer, API, catalog.
async fn stop(
    daemon: Daemon,
    monitor: MonitorState,
    maintenance: MaintenanceJobs,
) -> anyhow::Result<()> {
    stop_with_schedule(daemon, monitor, maintenance, SHUTDOWN_SCHEDULE).await
}

// Keep every ordered shutdown owner in one function so the absolute schedule and
// first-error precedence remain reviewable together.
#[allow(clippy::too_many_lines)]
async fn stop_with_schedule(
    mut daemon: Daemon,
    mut monitor: MonitorState,
    mut maintenance: MaintenanceJobs,
    schedule: ShutdownSchedule,
) -> anyhow::Result<()> {
    systemd::stopping();
    let started = tokio::time::Instant::now();
    let producer_deadline = started + schedule.producer_grace;
    let cooperative_deadline = started
        + schedule
            .producer_grace
            .saturating_sub(schedule.abort_observation_grace);
    let writer_deadline = started + schedule.writer_deadline;
    let api_deadline = started + schedule.api_deadline;
    let shutdown_deadline = started + schedule.budget;
    let mut first_error = None;

    daemon.shutdown.cancel();
    let mut report_task = maintenance.report.take_active();
    let mut retention_task = maintenance.retention.take_active();
    let mut selfcheck_task = monitor.selfcheck.cancel_and_take();
    let mut report_joined = report_task.is_none();
    let mut retention_joined = retention_task.is_none();
    let mut selfcheck_joined = selfcheck_task.is_none();
    let mut loop_probe_joined = false;

    let wait_for_producers = async {
        for task in &mut daemon.streams {
            match (&mut task.handle).await {
                Ok(()) => {}
                Err(err) => record_shutdown_error(
                    &mut first_error,
                    anyhow::anyhow!("stream {} task failed: {err}", task.key),
                ),
            }
            task.joined = true;
        }
        for task in &mut daemon.events {
            match (&mut task.handle).await {
                Ok(()) => {}
                Err(err) => record_shutdown_error(
                    &mut first_error,
                    anyhow::anyhow!(
                        "event task for camera {} ({:?}) failed: {err}",
                        task.camera,
                        task.source
                    ),
                ),
            }
            task.joined = true;
        }
        if let Err(err) = (&mut daemon.loop_probe).await {
            record_shutdown_error(
                &mut first_error,
                anyhow::anyhow!("loop probe task failed: {err}"),
            );
        }
        loop_probe_joined = true;
        if let Some(task) = &mut selfcheck_task
            && let Err(err) = task.await
        {
            record_shutdown_error(
                &mut first_error,
                anyhow::anyhow!("selfcheck task failed: {err}"),
            );
        }
        selfcheck_joined = true;
        if let Some(task) = &mut report_task
            && let Err(err) = task.await
        {
            record_shutdown_error(
                &mut first_error,
                anyhow::anyhow!("statistics report task failed: {err}"),
            );
        }
        report_joined = true;
        if let Some(task) = &mut retention_task
            && let Err(err) = task.await
        {
            record_shutdown_error(
                &mut first_error,
                anyhow::anyhow!("retention task failed: {err}"),
            );
        }
        retention_joined = true;
    };
    if tokio::time::timeout_at(cooperative_deadline, wait_for_producers)
        .await
        .is_err()
    {
        record_shutdown_error(
            &mut first_error,
            anyhow::anyhow!("producer shutdown exceeded {:?}", schedule.producer_grace),
        );
    }

    for task in &daemon.streams {
        if !task.joined {
            task.handle.abort();
        }
    }
    for task in &daemon.events {
        if !task.joined {
            task.handle.abort();
        }
    }
    if !loop_probe_joined {
        daemon.loop_probe.abort();
    }
    if !selfcheck_joined && let Some(task) = &selfcheck_task {
        task.abort();
    }
    if !report_joined && let Some(task) = &report_task {
        task.abort();
    }
    if !retention_joined && let Some(task) = &retention_task {
        task.abort();
    }

    let observe_aborted = async {
        for task in &mut daemon.streams {
            if !task.joined {
                if let Err(err) = (&mut task.handle).await
                    && !err.is_cancelled()
                {
                    record_shutdown_error(
                        &mut first_error,
                        anyhow::anyhow!("stream {} abort failed: {err}", task.key),
                    );
                }
                task.joined = true;
            }
        }
        for task in &mut daemon.events {
            if !task.joined {
                if let Err(err) = (&mut task.handle).await
                    && !err.is_cancelled()
                {
                    record_shutdown_error(
                        &mut first_error,
                        anyhow::anyhow!(
                            "event task for camera {} abort failed: {err}",
                            task.camera
                        ),
                    );
                }
                task.joined = true;
            }
        }
        if !loop_probe_joined
            && let Err(err) = (&mut daemon.loop_probe).await
            && !err.is_cancelled()
        {
            record_shutdown_error(
                &mut first_error,
                anyhow::anyhow!("loop probe abort failed: {err}"),
            );
        }
        if !selfcheck_joined
            && let Some(task) = &mut selfcheck_task
            && let Err(err) = task.await
            && !err.is_cancelled()
        {
            record_shutdown_error(
                &mut first_error,
                anyhow::anyhow!("selfcheck abort failed: {err}"),
            );
        }
        if !report_joined
            && let Some(task) = &mut report_task
            && let Err(err) = task.await
            && !err.is_cancelled()
        {
            record_shutdown_error(
                &mut first_error,
                anyhow::anyhow!("statistics report abort failed: {err}"),
            );
        }
        if !retention_joined
            && let Some(task) = &mut retention_task
            && let Err(err) = task.await
            && !err.is_cancelled()
        {
            record_shutdown_error(
                &mut first_error,
                anyhow::anyhow!("retention abort failed: {err}"),
            );
        }
    };
    if complete_by_deadline(producer_deadline, observe_aborted)
        .await
        .is_none()
    {
        record_shutdown_error(
            &mut first_error,
            anyhow::anyhow!("aborted task observation exceeded the shared producer grace"),
        );
    }

    let writer = daemon.writer.clone();
    match run_on_shutdown_thread("irisd-writer-shutdown", move || writer.shutdown()) {
        Ok(result) => match tokio::time::timeout_at(writer_deadline, result).await {
            Ok(Ok(Ok(()))) => {}
            Ok(Ok(Err(err))) => record_shutdown_error(
                &mut first_error,
                anyhow::Error::new(err).context("sealing recordings"),
            ),
            Ok(Err(err)) => record_shutdown_error(
                &mut first_error,
                anyhow::anyhow!("writer shutdown thread stopped without a result: {err}"),
            ),
            Err(_) => record_shutdown_error(
                &mut first_error,
                anyhow::anyhow!("writer sealing exceeded its absolute shutdown deadline"),
            ),
        },
        Err(err) => record_shutdown_error(
            &mut first_error,
            anyhow::Error::new(err).context("starting writer shutdown thread"),
        ),
    }

    if let Some(mut api) = daemon.api.take() {
        match tokio::time::timeout_at(api_deadline, &mut api).await {
            Ok(Ok(Ok(()))) => {}
            Ok(Ok(Err(err))) => record_shutdown_error(
                &mut first_error,
                anyhow::Error::new(err).context("API server shutdown"),
            ),
            Ok(Err(err)) => record_shutdown_error(
                &mut first_error,
                anyhow::anyhow!("API task failed during shutdown: {err}"),
            ),
            Err(_) => {
                record_shutdown_error(
                    &mut first_error,
                    anyhow::anyhow!("API shutdown exceeded its absolute deadline"),
                );
                api.abort();
                match complete_by_deadline(shutdown_deadline, &mut api).await {
                    Some(Err(err)) if !err.is_cancelled() => record_shutdown_error(
                        &mut first_error,
                        anyhow::anyhow!("API abort failed: {err}"),
                    ),
                    Some(_) => {}
                    None => record_shutdown_error(
                        &mut first_error,
                        anyhow::anyhow!(
                            "API abort observation exceeded the service shutdown budget"
                        ),
                    ),
                }
            }
        }
    }

    let catalog = daemon.catalog.clone();
    // If the OS refuses the helper thread, retain one owner until process exit.
    // Dropping the final CatalogHandle joins synchronously and would otherwise
    // escape the daemon's absolute shutdown deadline.
    let catalog_drop_guard = daemon.catalog.clone();
    match run_on_shutdown_thread("irisd-catalog-shutdown", move || catalog.shutdown()) {
        Ok(result) => {
            drop(catalog_drop_guard);
            match tokio::time::timeout_at(shutdown_deadline, result).await {
                Ok(Ok(())) => {}
                Ok(Err(err)) => record_shutdown_error(
                    &mut first_error,
                    anyhow::anyhow!("catalog shutdown thread stopped without a result: {err}"),
                ),
                Err(_) => record_shutdown_error(
                    &mut first_error,
                    anyhow::anyhow!("catalog shutdown exceeded the service shutdown budget"),
                ),
            }
        }
        Err(err) => {
            std::mem::forget(catalog_drop_guard);
            record_shutdown_error(
                &mut first_error,
                anyhow::Error::new(err).context("starting catalog shutdown thread"),
            );
        }
    }
    info!("stopped");
    first_error.map_or(Ok(()), Err)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timer_and_capacity_admission_is_checked() {
        assert!(validate_period("period", Duration::from_nanos(1)).is_ok());
        assert!(validate_period("period", Duration::ZERO).is_err());
        assert!(checked_queue_capacity(0, 0).is_err());
        assert_eq!(checked_queue_capacity(0, 1).unwrap(), 1);
        assert_eq!(checked_queue_capacity(32, 14).unwrap(), 448);
        assert!(checked_queue_capacity(usize::MAX, 2).is_err());

        let largest = u64::try_from(i64::MAX / 90).unwrap();
        assert_eq!(
            checked_fragment_ticks(largest).unwrap().get(),
            (i64::MAX / 90) * 90
        );
        assert!(checked_fragment_ticks(0).is_err());
        assert!(checked_fragment_ticks(largest + 1).is_err());
        assert!(checked_fragment_ticks(u64::MAX).is_err());
    }

    #[test]
    fn daemon_malformed_source_error_does_not_echo_the_url() {
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
            let error = validate_source_urls(&config).unwrap_err();
            let text = format!("{error:#}");
            assert!(!text.contains(marker), "{text}");
            assert!(!text.contains(&raw), "{text}");
            assert!(text.contains("camera cam1 main URL"), "{text}");
            assert!(text.contains(&parser_cause), "{text}");
        }
    }

    #[tokio::test]
    async fn fallible_monitor_prerequisite_fails_before_worker_state_exists() {
        let config = Config::from_toml_str(
            "[[camera]]\nid='cam1'\nname=''\nrtsp_url='rtsp://127.0.0.1/main'\nusername=''\npassword=''\nrecord=['main']\n",
        )
        .unwrap();
        let args = Args {
            config: PathBuf::from("config.toml"),
            no_sandbox: true,
            stats_interval: Duration::from_secs(1),
            selfcheck_cycle: Duration::from_secs(1),
        };
        let result = MonitorState::new_with_cpu(&config, &args, &CancellationToken::new(), || {
            Err(io::Error::other("injected proc failure"))
        });
        let Err(error) = result else {
            panic!("injected monitor initialization must fail");
        };
        assert!(error.to_string().contains("reading /proc/self/stat"));
    }

    #[tokio::test]
    async fn signal_initialization_failure_precedes_startup_side_effects() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("video");
        let state = dir.path().join("state");
        let config_path = dir.path().join("config.toml");
        std::fs::write(
            &config_path,
            format!(
                "[storage]\nvideo_roots=['{}']\nstate_dir='{}'\n\
                 [[camera]]\nid='cam1'\nname=''\nrtsp_url='rtsp://127.0.0.1/main'\n\
                 username=''\npassword=''\nrecord=['main']\n",
                root.display(),
                state.display(),
            ),
        )
        .unwrap();
        let args = Args {
            config: config_path,
            no_sandbox: true,
            stats_interval: Duration::from_secs(1),
            selfcheck_cycle: Duration::from_secs(1),
        };

        let result = run_with_monitor_initializer(args, |config, args, shutdown| {
            MonitorState::new_with_initializers(config, args, shutdown, CpuMeter::new, |_| {
                Err(io::Error::other("injected signal failure"))
            })
        })
        .await;
        let Err(error) = result else {
            panic!("injected signal initialization must fail");
        };
        let text = format!("{error:#}");
        assert!(text.contains("installing the SIGTERM handler"), "{text}");
        assert!(text.contains("injected signal failure"), "{text}");
        assert!(!state.exists(), "state directory must not be created");
        assert!(!root.exists(), "recording root must not be created");
    }

    #[tokio::test]
    async fn delayed_root_assignment_emits_reconciliation_heartbeats() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let (release_tx, release_rx) = oneshot::channel::<()>();
        let heartbeats = Arc::new(AtomicUsize::new(0));
        let observed = Arc::clone(&heartbeats);
        let assignment = tokio::spawn(async move {
            with_startup_heartbeat(
                async move { release_rx.await.unwrap() },
                Duration::from_millis(5),
                move || {
                    observed.fetch_add(1, Ordering::Relaxed);
                },
            )
            .await;
        });
        tokio::time::sleep(Duration::from_millis(18)).await;
        assert!(heartbeats.load(Ordering::Relaxed) >= 2);
        release_tx.send(()).unwrap();
        assignment.await.unwrap();
    }

    #[tokio::test]
    async fn pending_catalog_report_and_long_retention_are_single_flight_and_control_responsive() {
        let report_shutdown = CancellationToken::new();
        let report_handle = tokio::spawn(std::future::pending::<ReportCompletion>());
        let report_id = report_handle.id();
        let mut report = ReportJob {
            idle: None,
            active: Some(report_handle),
            shutdown: report_shutdown,
        };
        for _ in 0..100 {
            report.launch(Gauges {
                cpu: CpuSplit::default(),
                rss: 0,
                uptime: Duration::ZERO,
                loop_lag_ms: 0,
                watchdog_withheld: 0,
                retention_max_ms: 0,
                selfcheck_ok: 0,
                selfcheck_failed: 0,
                selfcheck_skipped: 0,
            });
            assert_eq!(report.active.as_ref().unwrap().id(), report_id);
        }
        tokio::select! {
            _ = report.wait_active() => panic!("pending report unexpectedly completed"),
            () = tokio::time::sleep(Duration::from_millis(5)) => {}
        }
        let report_handle = report.take_active().unwrap();
        report_handle.abort();
        match report_handle.await {
            Err(error) => assert!(error.is_cancelled()),
            Ok(_) => panic!("aborted report task completed normally"),
        }
        report.launch(Gauges {
            cpu: CpuSplit::default(),
            rss: 0,
            uptime: Duration::ZERO,
            loop_lag_ms: 0,
            watchdog_withheld: 0,
            retention_max_ms: 0,
            selfcheck_ok: 0,
            selfcheck_failed: 0,
            selfcheck_skipped: 0,
        });
        assert!(
            report.active.is_none(),
            "cancelled report must not relaunch"
        );

        let retention_shutdown = CancellationToken::new();
        let retention_handle = tokio::spawn(std::future::pending::<RetentionCompletion>());
        let retention_id = retention_handle.id();
        let mut retention = RetentionJob {
            idle: None,
            active: Some(retention_handle),
            shutdown: retention_shutdown,
        };
        for _ in 0..100 {
            retention.launch();
            assert_eq!(retention.active.as_ref().unwrap().id(), retention_id);
        }
        tokio::select! {
            _ = retention.wait_active() => panic!("pending retention unexpectedly completed"),
            () = tokio::time::sleep(Duration::from_millis(5)) => {}
        }
        let retention_handle = retention.take_active().unwrap();
        retention_handle.abort();
        match retention_handle.await {
            Err(error) => assert!(error.is_cancelled()),
            Ok(_) => panic!("aborted retention task completed normally"),
        }
        retention.launch();
        assert!(
            retention.active.is_none(),
            "cancelled retention must not relaunch"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn gated_report_statvfs_keeps_control_responsive_and_has_one_bounded_raw_tail() {
        use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

        let active = Arc::new(AtomicBool::new(false));
        let calls = Arc::new(AtomicUsize::new(0));
        let worker_active = Arc::clone(&active);
        let worker_calls = Arc::clone(&calls);
        let (started_tx, started_rx) = std::sync::mpsc::sync_channel(0);
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let release_rx = Arc::new(std::sync::Mutex::new(release_rx));
        let mut task = tokio::spawn(async move {
            log_roots_with(&[PathBuf::from("synthetic-root")], move |_| {
                worker_calls.fetch_add(1, Ordering::SeqCst);
                worker_active.store(true, Ordering::SeqCst);
                let _ = started_tx.send(());
                let _ = release_rx.lock().unwrap().recv();
                worker_active.store(false, Ordering::SeqCst);
                Ok(iris_store::FreeSpace {
                    total: 100,
                    available: 50,
                })
            })
            .await;
        });
        started_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("raw report statvfs worker must reach the gate");
        tokio::select! {
            result = &mut task => panic!("gated report statvfs unexpectedly completed: {result:?}"),
            () = tokio::time::sleep(Duration::from_millis(5)) => {}
        }
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        assert!(active.load(Ordering::SeqCst));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        release_tx.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(1), async {
            while active.load(Ordering::SeqCst) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("released report statvfs tail must exit");
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn blocking_shutdown_thread_does_not_hold_the_runtime_deadline() {
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let result = run_on_shutdown_thread("bounded-shutdown-test", move || {
            let _ = release_rx.recv();
        })
        .unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(10), result)
                .await
                .is_err()
        );
        drop(release_tx);
        tokio::task::yield_now().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn post_abort_observation_stops_at_the_absolute_deadline() {
        let (started_tx, started_rx) = std::sync::mpsc::sync_channel::<()>(0);
        let (release_tx, release_rx) = std::sync::mpsc::sync_channel::<()>(0);
        let mut task = tokio::spawn(async move {
            let _ = started_tx.send(());
            let _ = release_rx.recv();
        });
        started_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("gated task must start");
        task.abort();
        let started = Instant::now();
        let deadline = tokio::time::Instant::now() + Duration::from_millis(20);
        assert!(complete_by_deadline(deadline, &mut task).await.is_none());
        assert!(started.elapsed() < Duration::from_millis(250));

        release_tx.send(()).unwrap();
        let _ = tokio::time::timeout(Duration::from_secs(1), &mut task)
            .await
            .expect("released task must become observable");
    }

    #[test]
    fn shutdown_error_aggregation_preserves_the_primary_failure() {
        let mut first = None;
        record_shutdown_error(&mut first, anyhow::anyhow!("primary"));
        record_shutdown_error(&mut first, anyhow::anyhow!("secondary"));
        assert_eq!(first.unwrap().to_string(), "primary");
    }

    #[test]
    fn daemon_failure_precedes_a_later_shutdown_failure() {
        let result = combine_run_outcomes(
            Err(anyhow::anyhow!("monitor primary")),
            Err(anyhow::anyhow!("shutdown secondary")),
        );
        assert_eq!(result.unwrap_err().to_string(), "monitor primary");
    }

    struct CountOnDrop(Arc<std::sync::atomic::AtomicUsize>);

    impl Drop for CountOnDrop {
        fn drop(&mut self) {
            self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
    }

    struct TimedCountOnDrop {
        count: Arc<std::sync::atomic::AtomicUsize>,
        clock: Arc<std::sync::Mutex<Option<Instant>>>,
        latest_ms: Arc<std::sync::atomic::AtomicU64>,
    }

    impl Drop for TimedCountOnDrop {
        fn drop(&mut self) {
            use std::sync::atomic::Ordering;

            self.count.fetch_add(1, Ordering::SeqCst);
            if let Some(started) = *self.clock.lock().unwrap() {
                self.latest_ms.fetch_max(
                    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
                    Ordering::SeqCst,
                );
            }
        }
    }

    #[derive(Default)]
    struct CountingObserver {
        sealed: std::sync::atomic::AtomicUsize,
    }

    impl iris_store::StoreObserver for CountingObserver {
        fn segment_opened(&self, _info: iris_store::SegmentOpened) {}

        fn segment_progress(&self, _info: iris_store::SegmentProgress) {}

        fn segment_sealed(&self, _info: iris_store::SegmentSealed) {
            self.sealed
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
    }

    struct PanicOnSeal {
        attempted: Arc<std::sync::atomic::AtomicUsize>,
    }

    impl iris_store::StoreObserver for PanicOnSeal {
        fn segment_opened(&self, _info: iris_store::SegmentOpened) {}

        fn segment_progress(&self, _info: iris_store::SegmentProgress) {}

        fn segment_sealed(&self, _info: iris_store::SegmentSealed) {
            self.attempted
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            panic!("injected writer observer panic");
        }
    }

    fn shutdown_schedule_for_test() -> ShutdownSchedule {
        ShutdownSchedule {
            producer_grace: Duration::from_millis(200),
            abort_observation_grace: Duration::from_millis(50),
            writer_deadline: Duration::from_millis(2_500),
            api_deadline: Duration::from_millis(2_700),
            budget: Duration::from_secs(4),
        }
    }

    fn writer_fragment() -> Fragment {
        use bytes::Bytes;
        use iris_mp4::fragment::Sample;
        use iris_mp4::init::SampleEntry;

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

    fn spawn_test_writer(
        root: &Path,
        observer: Arc<dyn iris_store::StoreObserver>,
    ) -> WriterHandle {
        WriterHandle::spawn_with_observer(
            root.to_path_buf(),
            StoragePolicy {
                sync_interval: Duration::from_secs(60),
                preallocate_bytes: 0,
                rotate_bytes: u64::MAX,
                rotate_secs: u64::MAX,
            },
            4,
            observer,
        )
        .unwrap()
    }

    async fn open_test_writer(writer: &WriterHandle) {
        let key = StreamKey {
            camera: "shutdown".parse().unwrap(),
            kind: StreamKind::Main,
        };
        writer.try_send(key, writer_fragment()).unwrap();
        tokio::time::timeout(Duration::from_secs(10), async {
            while writer.stats().files_opened() == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("writer must open the synthetic segment");
    }

    fn monitor_for_test(config: &Config, shutdown: &CancellationToken) -> MonitorState {
        MonitorState::new(
            config,
            &Args {
                config: PathBuf::from("synthetic.toml"),
                no_sandbox: true,
                stats_interval: Duration::from_secs(60),
                selfcheck_cycle: Duration::from_secs(60),
            },
            shutdown,
        )
        .unwrap()
    }

    fn daemon_for_test(
        config: Config,
        catalog: CatalogHandle,
        writer: WriterHandle,
        shutdown: CancellationToken,
        streams: Vec<StreamTask>,
        events: Vec<EventTask>,
        api: JoinHandle<io::Result<()>>,
        loop_probe: JoinHandle<()>,
    ) -> Daemon {
        Daemon {
            config,
            approved_roots: Vec::new(),
            catalog,
            writer,
            streams,
            events,
            api: Some(api),
            shutdown,
            loop_lag: Arc::new(Peak::new()),
            loop_probe,
        }
    }

    fn pending_timed(
        count: Arc<std::sync::atomic::AtomicUsize>,
        clock: Arc<std::sync::Mutex<Option<Instant>>>,
        latest_ms: Arc<std::sync::atomic::AtomicU64>,
    ) -> JoinHandle<()> {
        let guard = TimedCountOnDrop {
            count,
            clock,
            latest_ms,
        };
        tokio::spawn(async move {
            let _guard = guard;
            std::future::pending::<()>().await;
        })
    }

    // This matrix assembles every producer and maintenance owner in one fixture.
    #[allow(clippy::too_many_lines)]
    #[tokio::test]
    async fn one_shutdown_interval_aborts_and_observes_every_pending_owner_before_later_stages() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("video");
        let observer = Arc::new(CountingObserver::default());
        let writer = spawn_test_writer(&root, observer.clone());
        open_test_writer(&writer).await;
        let mut config = Config::default();
        config.storage.video_roots = vec![root];
        config.storage.state_dir = dir.path().join("state");
        let catalog = CatalogHandle::open(&dir.path().join("catalog.db")).unwrap();
        let catalog_probe = catalog.clone();
        let shutdown = CancellationToken::new();
        let monitor = monitor_for_test(&config, &shutdown);
        let producer_dropped = Arc::new(AtomicUsize::new(0));
        let producer_clock = Arc::new(std::sync::Mutex::new(None));
        let producer_latest_ms = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let api_dropped = Arc::new(AtomicUsize::new(0));
        let stream_key = StreamKey {
            camera: "stream".parse().unwrap(),
            kind: StreamKind::Main,
        };
        let streams = (0..3)
            .map(|_| StreamTask {
                key: stream_key.clone(),
                stats: Arc::new(StreamStats::default()),
                handle: pending_timed(
                    Arc::clone(&producer_dropped),
                    Arc::clone(&producer_clock),
                    Arc::clone(&producer_latest_ms),
                ),
                last_progress: (0, 0),
                joined: false,
            })
            .collect();
        let events = (0..2)
            .map(|_| EventTask {
                camera: "event".parse().unwrap(),
                source: EventSource::DahuaCgi,
                stats: Arc::new(EventStats::default()),
                handle: pending_timed(
                    Arc::clone(&producer_dropped),
                    Arc::clone(&producer_clock),
                    Arc::clone(&producer_latest_ms),
                ),
                joined: false,
            })
            .collect();
        let api_guard = CountOnDrop(Arc::clone(&api_dropped));
        let api = tokio::spawn(async move {
            let _guard = api_guard;
            std::future::pending::<io::Result<()>>().await
        });
        let loop_probe = pending_timed(
            Arc::clone(&producer_dropped),
            Arc::clone(&producer_clock),
            Arc::clone(&producer_latest_ms),
        );
        let daemon = daemon_for_test(
            config, catalog, writer, shutdown, streams, events, api, loop_probe,
        );
        let mut maintenance = MaintenanceJobs::new(&daemon);
        maintenance.report.idle.take();
        let report_guard = TimedCountOnDrop {
            count: Arc::clone(&producer_dropped),
            clock: Arc::clone(&producer_clock),
            latest_ms: Arc::clone(&producer_latest_ms),
        };
        maintenance.report.active = Some(tokio::spawn(async move {
            let _guard = report_guard;
            std::future::pending::<ReportCompletion>().await
        }));
        maintenance.retention.idle.take();
        let retention_guard = TimedCountOnDrop {
            count: Arc::clone(&producer_dropped),
            clock: Arc::clone(&producer_clock),
            latest_ms: Arc::clone(&producer_latest_ms),
        };
        maintenance.retention.active = Some(tokio::spawn(async move {
            let _guard = retention_guard;
            std::future::pending::<RetentionCompletion>().await
        }));

        let started = Instant::now();
        *producer_clock.lock().unwrap() = Some(started);
        let error = stop_with_schedule(daemon, monitor, maintenance, shutdown_schedule_for_test())
            .await
            .unwrap_err();
        let elapsed = started.elapsed();
        assert!(
            error.to_string().contains("producer shutdown exceeded"),
            "{error:#}"
        );
        assert!(elapsed >= Duration::from_millis(15), "elapsed {elapsed:?}");
        assert!(elapsed < Duration::from_secs(6), "elapsed {elapsed:?}");
        assert_eq!(producer_dropped.load(Ordering::SeqCst), 8);
        assert_eq!(api_dropped.load(Ordering::SeqCst), 1);
        assert!(
            producer_latest_ms.load(Ordering::SeqCst) < 1_000,
            "producer tasks exceeded one shared grace: {} ms",
            producer_latest_ms.load(Ordering::SeqCst)
        );
        assert_eq!(observer.sealed.load(Ordering::SeqCst), 1);
        assert!(catalog_probe.stats().await.is_err());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn unobservable_aborted_producer_cannot_consume_reserved_cleanup_windows() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        struct ReleaseOnDrop(Option<std::sync::mpsc::Sender<()>>);

        impl Drop for ReleaseOnDrop {
            fn drop(&mut self) {
                if let Some(release) = self.0.take() {
                    let _ = release.send(());
                }
            }
        }

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("video");
        let observer = Arc::new(CountingObserver::default());
        let writer = spawn_test_writer(&root, observer.clone());
        open_test_writer(&writer).await;
        let mut config = Config::default();
        config.storage.video_roots = vec![root];
        config.storage.state_dir = dir.path().join("state");
        let catalog = CatalogHandle::open(&dir.path().join("catalog.db")).unwrap();
        let catalog_probe = catalog.clone();
        let shutdown = CancellationToken::new();
        let monitor = monitor_for_test(&config, &shutdown);
        let (started_tx, started_rx) = std::sync::mpsc::sync_channel(0);
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let mut release = ReleaseOnDrop(Some(release_tx));
        let blocked = tokio::task::spawn_blocking(move || {
            let _ = started_tx.send(());
            let _ = release_rx.recv();
        });
        started_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        let api_dropped = Arc::new(AtomicUsize::new(0));
        let api_guard = CountOnDrop(Arc::clone(&api_dropped));
        let api = tokio::spawn(async move {
            let _guard = api_guard;
            std::future::pending::<io::Result<()>>().await
        });
        let daemon = daemon_for_test(
            config,
            catalog,
            writer,
            shutdown,
            vec![StreamTask {
                key: StreamKey {
                    camera: "blocked".parse().unwrap(),
                    kind: StreamKind::Main,
                },
                stats: Arc::new(StreamStats::default()),
                handle: blocked,
                last_progress: (0, 0),
                joined: false,
            }],
            Vec::new(),
            api,
            tokio::spawn(async {}),
        );
        let maintenance = MaintenanceJobs::new(&daemon);
        let started = Instant::now();
        let error = stop_with_schedule(daemon, monitor, maintenance, shutdown_schedule_for_test())
            .await
            .unwrap_err();
        assert!(error.to_string().contains("producer shutdown exceeded"));
        assert!(started.elapsed() < Duration::from_secs(6));
        assert_eq!(observer.sealed.load(Ordering::SeqCst), 1);
        assert_eq!(api_dropped.load(Ordering::SeqCst), 1);
        assert!(catalog_probe.stats().await.is_err());

        release.0.take().unwrap().send(()).unwrap();
    }

    async fn stop_after_writer_failure(
        dir: &tempfile::TempDir,
        writer: WriterHandle,
        api_runs: Arc<std::sync::atomic::AtomicUsize>,
    ) -> anyhow::Error {
        use std::sync::atomic::Ordering;

        let mut config = Config::default();
        config.storage.state_dir = dir.path().join("state");
        let catalog = CatalogHandle::open(&dir.path().join("catalog.db")).unwrap();
        let catalog_probe = catalog.clone();
        let shutdown = CancellationToken::new();
        let monitor = monitor_for_test(&config, &shutdown);
        let api = tokio::spawn(async move {
            api_runs.fetch_add(1, Ordering::SeqCst);
            Err(io::Error::other("injected API cleanup failure"))
        });
        let daemon = daemon_for_test(
            config,
            catalog,
            writer,
            shutdown,
            Vec::new(),
            Vec::new(),
            api,
            tokio::spawn(async {}),
        );
        let maintenance = MaintenanceJobs::new(&daemon);
        let error = stop_with_schedule(daemon, monitor, maintenance, shutdown_schedule_for_test())
            .await
            .unwrap_err();
        assert!(catalog_probe.stats().await.is_err());
        error
    }

    #[tokio::test]
    async fn writer_close_error_preserves_primary_and_runs_api_and_catalog_cleanup() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("video");
        let writer = spawn_test_writer(&root, Arc::new(CountingObserver::default()));
        open_test_writer(&writer).await;
        std::fs::rename(&root, dir.path().join("video-displaced")).unwrap();
        let api_runs = Arc::new(AtomicUsize::new(0));
        let error = stop_after_writer_failure(&dir, writer, Arc::clone(&api_runs)).await;
        assert!(
            format!("{error:#}").contains("sealing recordings"),
            "{error:#}"
        );
        assert_eq!(api_runs.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn writer_join_panic_preserves_primary_and_runs_api_and_catalog_cleanup() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let dir = tempfile::tempdir().unwrap();
        let attempted = Arc::new(AtomicUsize::new(0));
        let writer = spawn_test_writer(
            &dir.path().join("video"),
            Arc::new(PanicOnSeal {
                attempted: Arc::clone(&attempted),
            }),
        );
        open_test_writer(&writer).await;
        let api_runs = Arc::new(AtomicUsize::new(0));
        let error = stop_after_writer_failure(&dir, writer, Arc::clone(&api_runs)).await;
        assert!(
            format!("{error:#}").contains("writer thread panicked"),
            "{error:#}"
        );
        assert_eq!(attempted.load(Ordering::SeqCst), 1);
        assert_eq!(api_runs.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn stream_event_and_api_panics_are_observed_before_cleanup_finishes() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let dir = tempfile::tempdir().unwrap();
        let writer = spawn_test_writer(
            &dir.path().join("video"),
            Arc::new(CountingObserver::default()),
        );
        let mut config = Config::default();
        config.storage.state_dir = dir.path().join("state");
        let catalog = CatalogHandle::open(&dir.path().join("catalog.db")).unwrap();
        let catalog_probe = catalog.clone();
        let shutdown = CancellationToken::new();
        let monitor = monitor_for_test(&config, &shutdown);
        let panics = Arc::new(AtomicUsize::new(0));
        let stream_panics = Arc::clone(&panics);
        let stream = tokio::spawn(async move {
            stream_panics.fetch_add(1, Ordering::SeqCst);
            panic!("injected stream panic");
        });
        let event_panics = Arc::clone(&panics);
        let event = tokio::spawn(async move {
            event_panics.fetch_add(1, Ordering::SeqCst);
            panic!("injected event panic");
        });
        let api_panics = Arc::clone(&panics);
        let api = tokio::spawn(async move {
            api_panics.fetch_add(1, Ordering::SeqCst);
            panic!("injected API panic");
            #[allow(unreachable_code)]
            Ok(())
        });
        let daemon = daemon_for_test(
            config,
            catalog,
            writer,
            shutdown,
            vec![StreamTask {
                key: StreamKey {
                    camera: "stream".parse().unwrap(),
                    kind: StreamKind::Main,
                },
                stats: Arc::new(StreamStats::default()),
                handle: stream,
                last_progress: (0, 0),
                joined: false,
            }],
            vec![EventTask {
                camera: "event".parse().unwrap(),
                source: EventSource::DahuaCgi,
                stats: Arc::new(EventStats::default()),
                handle: event,
                joined: false,
            }],
            api,
            tokio::spawn(async {}),
        );
        let maintenance = MaintenanceJobs::new(&daemon);
        let error = stop_with_schedule(daemon, monitor, maintenance, shutdown_schedule_for_test())
            .await
            .unwrap_err();
        assert!(error.to_string().contains("stream stream/main task failed"));
        assert_eq!(panics.load(Ordering::SeqCst), 3);
        assert!(catalog_probe.stats().await.is_err());
    }

    #[test]
    fn daemon_root_admission_durably_creates_a_nonexistent_root() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("nested/recordings");
        let roots =
            admit_configured_roots(&dir.path().join("catalog.db"), std::slice::from_ref(&root))
                .unwrap();
        eprintln!("daemon recording root prepared");
        assert_eq!(roots[0].path(), root);
        assert!(root.is_dir());
        assert!(root.join(iris_store::ROOT_IDENTITY_MARKER).is_file());
    }

    #[tokio::test]
    async fn configured_physical_aliases_admit_and_register_exactly_one_root_spelling() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("recordings");
        let alias = root.join("..").join("recordings");
        let catalog_path = dir.path().join("catalog.db");
        let approved =
            admit_configured_roots(&catalog_path, &[root.clone(), alias.clone()]).unwrap();
        assert_eq!(approved.len(), 1);
        assert_eq!(approved[0].path(), root);

        let identity = approved[0].identity().to_string();
        let catalog = CatalogHandle::open(&catalog_path).unwrap();
        catalog
            .register_recording_root(approved[0].path(), &identity)
            .await
            .unwrap();
        catalog
            .configure_video_roots(std::slice::from_ref(&root))
            .await
            .unwrap();
        catalog.shutdown();

        let expectations = CatalogRootRegistry::open(&catalog_path)
            .unwrap()
            .expectations_with_progress(&[root, alias], || {})
            .unwrap();
        assert_eq!(
            expectations,
            [
                CatalogRootExpectation::Known(identity),
                CatalogRootExpectation::Fresh,
            ]
        );
    }

    #[tokio::test]
    async fn records_on_an_alternative_when_the_first_root_rejects_writes() {
        use bytes::Bytes;
        use iris_mp4::fragment::Sample;
        use iris_mp4::init::SampleEntry;
        use std::os::unix::fs::PermissionsExt as _;

        if rustix::process::geteuid().is_root() {
            return; // Root bypasses the permission fault injected by this test.
        }
        let dir = tempfile::tempdir().unwrap();
        let first = dir.path().join("first");
        let second = dir.path().join("second");
        std::fs::create_dir_all(&first).unwrap();
        std::fs::create_dir_all(&second).unwrap();
        let catalog_path = dir.path().join("catalog.db");
        let approved =
            admit_configured_roots(&catalog_path, &[first.clone(), second.clone()]).unwrap();
        std::fs::set_permissions(&first, std::fs::Permissions::from_mode(0o500)).unwrap();
        let mut config = Config::from_toml_str(
            "[[camera]]\nid=\"test\"\nname=\"test\"\nrtsp_url=\"rtsp://127.0.0.1:1/main\"\nusername=\"\"\npassword=\"\"\nrecord=[\"main\"]\n",
        ).unwrap();
        config.storage.video_roots = vec![first.clone(), second.clone()];
        config.storage.min_free_percent = 0;
        config.storage.preallocate_bytes = 0;
        let catalog = CatalogHandle::open(&catalog_path).unwrap();
        let shutdown = CancellationToken::new();
        let (writer, streams, _) =
            start_recording(&config, &approved, &catalog, &shutdown).unwrap();
        let fragment = Fragment {
            samples: vec![Sample {
                data: Bytes::from_static(&[0, 0, 0, 2, 0x26, 1]),
                duration: 3000,
                is_sync: true,
            }],
            decode_time: 0,
            wall: WallUs::new(1_700_000_000_000_000),
            raw_rtp: 0,
            new_run: true,
            sample_entry: Arc::new(SampleEntry::new(
                Bytes::from_static(b"\0\0\0\x10hvc1\0\0\0\0\0\0\0\0"),
                "hvc1".into(),
                640,
                480,
            )),
        };
        writer.try_send(streams[0].key.clone(), fragment).unwrap();
        writer.shutdown().unwrap();
        shutdown.cancel();
        for task in streams {
            task.handle.await.unwrap();
        }
        std::fs::set_permissions(&first, std::fs::Permissions::from_mode(0o700)).unwrap();
        catalog.shutdown();
        assert_eq!(adopt::recordings_under(&second).len(), 1);
    }

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
