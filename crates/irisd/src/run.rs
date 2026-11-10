//! `irisd run`: the daemon. Records every configured stream, keeps the
//! catalog, serves the API, subscribes to camera events, enforces retention,
//! and reports to systemd.

use std::collections::{HashMap, HashSet};
use std::io;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use anyhow::{Context, bail};
use clap::Args as ClapArgs;
use iris_api::{
    ActiveCamera, ApiState, ApiStats, CameraControl, CapabilitySecret, ControlRequest,
    LiveRegistry, ProvisionError, ServeLimits, SnapshotSource,
};
use iris_catalog::{
    CatalogHandle, CatalogRootExpectation, CatalogRootRegistry, OPEN_SEGMENT_GRACE,
};
use iris_core::config::{CameraConfig, Config, StorageConfig};
use iris_core::event::{Event, EventSource};
use iris_core::id::{CameraId, StreamKey, StreamKind};
use iris_core::metrics::{HistogramSnapshot, Peak};
use iris_core::time::{MediaTicks, TIMESCALE, WallUs};
use iris_events::{CameraEndpoint, EventSink, EventStats, run_dahua_cgi, run_onvif_pullpoint};
use iris_ingest::{
    FragmentSink, IngestOptions, LiveRing, ReadTicker, SinkError, StreamSource, StreamStats,
    run_stream,
};
use iris_mp4::fragment::Fragment;
use iris_store::{
    ApprovedRoot, RootExpectation, StoragePolicy, WriterHandle, WriterStats, admit_recording_root,
};
use tokio::sync::{Semaphore, broadcast, mpsc, oneshot};
use tokio::task::{AbortHandle, JoinError, JoinHandle, JoinSet};
use tokio::time::MissedTickBehavior;
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

use crate::catalog_observer::{CatalogEventSink, CatalogObserver};
use crate::proc::{self, CpuMeter, CpuSplit};
use crate::raw_calls::RawCalls;
use crate::sandbox::{SandboxEnforcement, SandboxSpec};
use crate::selfcheck::SelfCheck;
use crate::{adopt, capability, permissions, provision, retention, sandbox, systemd};

/// Bytes per mebibyte, for human-readable sizes.
const MIB: f64 = 1_048_576.0;
/// Bytes per gibibyte.
const GIB: f64 = 1_073_741_824.0;
/// Media ticks per second, as a float for rates.
const TICKS_PER_SEC: f64 = TIMESCALE as f64;
/// Cadence of the timer that measures how late the runtime thread runs.
const LOOP_PROBE_INTERVAL: Duration = Duration::from_millis(250);
/// How often retention runs.
const RETENTION_INTERVAL: Duration = Duration::from_secs(60);
/// Catalog batching and busy-wait margin added to the open-segment grace.
const OPEN_SEGMENT_GRACE_MARGIN: Duration = Duration::from_secs(10);
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
/// A statistics or retention job still running this long after its launch is
/// dropped and its state kept for the next launch.
const MAINTENANCE_TIMEOUT: Duration = Duration::from_secs(30);
/// A raw filesystem call still in the kernel this long after its job timed out
/// stops the daemon, so systemd restarts it.
const RAW_CALL_ESCALATION: Duration = Duration::from_secs(600);
/// Launches skipped behind an unfinished raw call warn at most this often.
const SKIPPED_LAUNCH_WARNING_INTERVAL: Duration = Duration::from_secs(60);
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
    /// Run without the Landlock sandbox. Without this flag the daemon refuses to
    /// start on a kernel lacking Landlock ABI 4 (filesystem and TCP rules).
    #[arg(long)]
    no_sandbox: bool,
    /// Log statistics this often.
    #[arg(
        long,
        value_parser = humantime::parse_duration,
        default_value = "60s",
        value_name = "DURATION"
    )]
    stats_interval: Duration,
    /// Check every stream's newest sealed file once per this long.
    #[arg(
        long,
        value_parser = humantime::parse_duration,
        default_value = "24h",
        value_name = "DURATION"
    )]
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
    /// Landlock enforcement, reported in the `process` statistics line.
    sandbox: SandboxEnforcement,
    /// Per-camera retention ages, shared with the retention job and kept
    /// current as cameras are added and removed.
    age_limits: Arc<RwLock<HashMap<CameraId, u32>>>,
    /// Camera provisioning through the API, while `[provisioning]` is enabled.
    provisioning: Option<Provisioning>,
    /// One task per removed camera, joining its tasks within
    /// [`CAMERA_STOP_GRACE`] and aborting the rest, so a removal never holds
    /// the monitor loop; ordered shutdown joins them with the producers.
    retiring: JoinSet<()>,
}

/// Requests queued from the API before the monitor loop takes them.
const CONTROL_QUEUE: usize = 8;
/// How long a removed camera's tasks may take to end before they are aborted.
const CAMERA_STOP_GRACE: Duration = Duration::from_secs(5);
/// ONVIF lookups (discovery probes and stream URL resolutions) the daemon runs
/// at once beside its monitor loop.
const MAX_LOOKUPS: usize = 4;

/// Everything the monitor loop needs to add and remove cameras at run time.
struct Provisioning {
    /// The API's registries, which list and serve the cameras.
    api_state: Arc<ApiState>,
    /// Requests from `POST` and `DELETE /api/cameras`.
    requests: mpsc::Receiver<ControlRequest>,
    /// The API dropped its sender; nothing more arrives.
    closed: bool,
    streams: StreamSpawner,
    events: EventSpawner,
    /// Cameras from the configuration file, which the API never removes.
    static_ids: HashSet<CameraId>,
    /// The cameras the API added, as persisted in the store.
    cameras: Vec<CameraConfig>,
    /// One cancellation token per added camera, for its removal.
    tokens: HashMap<CameraId, CancellationToken>,
    /// The ONVIF lookups running beside the loop.
    lookups: Lookups,
}

/// The ONVIF lookups (discovery probes and stream URL resolutions) running
/// beside the monitor loop.
struct Lookups {
    /// Permits, at most [`MAX_LOOKUPS`] at a time.
    permits: Arc<Semaphore>,
    /// The lookup tasks: finished ones are observed before the next spawn,
    /// and every one at shutdown.
    tasks: JoinSet<()>,
}

impl Lookups {
    fn new() -> Self {
        Self {
            permits: Arc::new(Semaphore::new(MAX_LOOKUPS)),
            tasks: JoinSet::new(),
        }
    }

    /// Observes the lookups that have finished, logging a failed task.
    fn reap(&mut self) {
        while let Some(result) = self.tasks.try_join_next() {
            if let Err(err) = result {
                warn!(%err, "ONVIF lookup task failed");
            }
        }
    }
}

/// What the API needs besides its registries: the capability secret and,
/// with provisioning enabled, the daemon's camera control.
struct ApiAccess {
    capability_secret: CapabilitySecret,
    control: Option<CameraControl>,
    /// Cameras from the configuration file; the others were added through the API.
    static_ids: HashSet<CameraId>,
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
    /// The self-check cycle, to re-time the ticker when the streams change.
    selfcheck_cycle: Duration,
    sigterm: tokio::signal::unix::Signal,
    sigint: tokio::signal::unix::Signal,
    started: Instant,
}

impl MonitorState {
    fn try_new(config: &Config, args: &Args, shutdown: &CancellationToken) -> anyhow::Result<Self> {
        Self::try_new_with_cpu(config, args, shutdown, CpuMeter::try_new)
    }

    /// Points the self-check at `streams`, the streams recording after a
    /// camera was added or removed, and re-times its ticker so the cycle
    /// still visits each stream once.
    fn refresh_selfcheck(&mut self, streams: Vec<StreamKey>) {
        self.selfcheck.set_streams(streams);
        if let Some(interval) = self.selfcheck.interval(self.selfcheck_cycle) {
            let mut ticker =
                tokio::time::interval_at(tokio::time::Instant::now() + interval, interval);
            ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
            self.selfcheck_ticker = ticker;
        }
    }

    fn try_new_with_cpu<F>(
        config: &Config,
        args: &Args,
        shutdown: &CancellationToken,
        cpu_meter: F,
    ) -> anyhow::Result<Self>
    where
        F: FnOnce() -> io::Result<CpuMeter>,
    {
        Self::try_new_with_initializers(
            config,
            args,
            shutdown,
            cpu_meter,
            tokio::signal::unix::signal,
        )
    }

    fn try_new_with_initializers<F, S>(
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
        let stream_keys = config
            .cameras
            .iter()
            .flat_map(|camera| {
                camera_sources(camera)
                    .into_iter()
                    .map(|(kind, _)| StreamKey {
                        camera: camera.id.clone(),
                        kind,
                    })
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
            selfcheck_cycle: args.selfcheck_cycle,
            sigterm,
            sigint,
            started: Instant::now(),
        })
    }
}

/// Runs the daemon until SIGTERM, SIGINT, or a failure that stops it: the
/// API task ending, a maintenance task failing, or a raw filesystem call
/// outliving its escalation bound.
pub async fn run(args: Args) -> anyhow::Result<()> {
    run_with_monitor_initializer(args, MonitorState::try_new).await
}

#[expect(
    clippy::too_many_lines,
    reason = "startup is one ordered sequence read together, like `stop_with_schedule`"
)]
async fn run_with_monitor_initializer<F>(args: Args, monitor_initializer: F) -> anyhow::Result<()>
where
    F: FnOnce(&Config, &Args, &CancellationToken) -> anyhow::Result<MonitorState>,
{
    let mut config =
        Config::load(&args.config).with_context(|| format!("loading {}", args.config.display()))?;
    warn_if_configuration_exposed(&args.config);
    validate(&config)?;
    let (static_ids, provisioned) = load_provisioned(&mut config)?;
    validate_period("--stats-interval", args.stats_interval)?;
    let source_count = validate_source_urls(&config)?;
    let reserved_streams = reserved_streams(&config);
    checked_queue_capacity(
        config.storage.max_queued_fragments,
        source_count + reserved_streams,
    )?;
    checked_fragment_ticks(config.storage.max_fragment_ms)?;
    let shutdown = CancellationToken::new();
    let mut monitor_state = monitor_initializer(&config, &args, &shutdown)?;
    raise_descriptor_limit();
    let capability_secret = prepare_state(&config)?;
    let approved_roots = admit_configured_roots(
        &config.storage.state_dir.join(permissions::CATALOG_FILE),
        &config.storage.video_roots,
    )?;
    monitor_state.selfcheck.set_roots(approved_roots.clone());
    if approved_roots.is_empty() {
        bail!("no recording root could be admitted");
    }
    config.storage.video_roots = approved_roots
        .iter()
        .map(|root| root.path().to_path_buf())
        .collect();

    let sandbox = if args.no_sandbox {
        warn!("sandbox disabled by --no-sandbox");
        SandboxEnforcement::Disabled
    } else {
        // Before any thread exists: threads inherit the Landlock domain of their creator.
        sandbox::apply(&sandbox_spec(&config, &args.config))?
    };

    let catalog = CatalogHandle::open_with_grace(
        &config.storage.state_dir.join(permissions::CATALOG_FILE),
        open_segment_grace(&config.storage),
    )
    .context("opening the catalog")?;
    for root in &approved_roots {
        catalog
            .register_recording_root(root.path(), root.identity())
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
    let configured: Vec<CameraConfig> = config
        .cameras
        .iter()
        .filter(|camera| static_ids.contains(&camera.id))
        .cloned()
        .collect();
    let (writer, mut streams, live, stream_spawner) = start_recording(
        &config,
        &configured,
        reserved_streams,
        &approved_roots,
        &catalog,
        &shutdown,
    )?;
    let (events_tx, _) = broadcast::channel::<Arc<Event>>(EVENT_CHANNEL);
    let (mut events, event_spawner) = start_events(&configured, &catalog, &events_tx, &shutdown);
    let mut tokens = HashMap::new();
    for camera in &provisioned {
        let token = shutdown.child_token();
        streams.extend(stream_spawner.spawn(camera, parsed_sources(camera)?, &token));
        events.extend(event_spawner.spawn(camera, &token));
        tokens.insert(camera.id.clone(), token);
    }
    let (control, requests) = if config.provisioning.enabled {
        let (sender, receiver) = mpsc::channel(CONTROL_QUEUE);
        (Some(CameraControl::new(sender)), Some(receiver))
    } else {
        (None, None)
    };
    let (api, api_state) = start_api(
        &config,
        &approved_roots,
        &catalog,
        ApiAccess {
            capability_secret,
            control,
            static_ids: static_ids.clone(),
        },
        live,
        events_tx,
        api_listener,
        &shutdown,
    );
    let provisioning = requests.map(|requests| Provisioning {
        api_state,
        requests,
        closed: false,
        streams: stream_spawner,
        events: event_spawner,
        static_ids,
        cameras: provisioned,
        tokens,
        lookups: Lookups::new(),
    });

    let age_limit_map = Arc::new(RwLock::new(age_limits(&config)));
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
        sandbox,
        age_limits: age_limit_map,
        provisioning,
        retiring: JoinSet::new(),
    };
    let mut maintenance = MaintenanceJobs::new(&daemon);

    systemd::ready();
    systemd::status("recording");
    info!(
        listen = %daemon.config.api.listen,
        cameras = daemon.config.cameras.len(),
        "irisd running"
    );

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

/// Awaits `future`, calling `heartbeat` every `interval` while it runs.
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
            CatalogRootExpectation::Known(identity) => RootExpectation::Known(identity),
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
            warn!(
                root = %path.display(),
                "recording root aliases another configured root; skipping"
            );
            continue;
        }
        identities.insert(root.identity());
        physical_roots.insert(root.physical_identity());
        approved.push(root);
    }
    Ok(approved)
}

/// Refuses a configuration the daemon cannot run: no camera to record unless
/// provisioning may add them, or no recording root.
fn validate(config: &Config) -> anyhow::Result<()> {
    if config.cameras.is_empty() && !config.provisioning.enabled {
        bail!("no cameras configured; enable [provisioning] to add cameras through the API");
    }
    if config.storage.video_roots.is_empty() {
        bail!("storage.video_roots is empty");
    }
    if config.api.allow_unauthenticated {
        warn!(
            listen = %config.api.listen,
            "api.allow_unauthenticated is set: the loopback API accepts unauthenticated requests"
        );
    }
    Ok(())
}

/// Warns when users beyond the daemon's account and group can reach the
/// configuration file, which holds camera passwords and API tokens.
pub(crate) fn warn_if_configuration_exposed(path: &Path) {
    if let Some(mode) = permissions::exposed_configuration(path) {
        warn!(
            path = %path.display(),
            mode = format_args!("{mode:04o}"),
            "the configuration file holds credentials and is accessible to other users; \
             restrict it to mode 0600, or 0640 with the service account's group"
        );
    }
}

/// Creates the state directory owner-only when it is missing, then checks and
/// repairs the modes of the directory and the catalog files before SQLite
/// opens them, then loads or creates the media-capability secret in it (an
/// unpersisted one when no API tokens are configured).
fn prepare_state(config: &Config) -> anyhow::Result<CapabilitySecret> {
    use std::os::unix::fs::DirBuilderExt as _;

    let dir = &config.storage.state_dir;

    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
        .with_context(|| format!("creating {}", dir.display()))?;
    for repair in permissions::secure_state_directory(dir)
        .with_context(|| format!("checking the state directory {}", dir.display()))?
    {
        warn!(
            path = %repair.path.display(),
            old_mode = format_args!("{:04o}", repair.old_mode),
            new_mode = format_args!("{:04o}", repair.new_mode),
            "restricted state permissions to the service account"
        );
    }
    if config.api.tokens.is_empty() {
        capability::ephemeral()
    } else {
        capability::load_or_create(dir)
    }
}

/// Raises the soft descriptor limit to the hard limit, so recording files, camera
/// sockets, and API clients share the whole budget the service allows, and logs
/// both values. A failure keeps the inherited limit.
fn raise_descriptor_limit() {
    use rustix::process::Resource;

    let limit = rustix::process::getrlimit(Resource::Nofile);
    let Some(raised) = raised_descriptor_limit(limit) else {
        info!(soft = ?limit.current, hard = ?limit.maximum, "descriptor limit");
        return;
    };
    match rustix::process::setrlimit(Resource::Nofile, raised) {
        Ok(()) => info!(
            previous_soft = ?limit.current,
            soft = ?raised.current,
            hard = ?raised.maximum,
            "raised the descriptor soft limit to the hard limit"
        ),
        Err(error) => warn!(
            %error,
            soft = ?limit.current,
            hard = ?limit.maximum,
            "cannot raise the descriptor soft limit"
        ),
    }
}

/// The limit with its soft value raised to a finite hard value, or `None` when
/// the soft value already reaches it or the hard value is unlimited (a soft
/// limit cannot be set to unlimited for descriptors).
fn raised_descriptor_limit(limit: rustix::process::Rlimit) -> Option<rustix::process::Rlimit> {
    let hard = limit.maximum?;
    limit
        .current
        .is_some_and(|soft| soft < hard)
        .then_some(rustix::process::Rlimit {
            current: Some(hard),
            maximum: Some(hard),
        })
}

fn validate_period(name: &str, period: Duration) -> anyhow::Result<()> {
    if period.is_zero() {
        bail!("{name} must be greater than zero");
    }
    Ok(())
}

/// Parses every configured camera's stream URLs and counts the streams.
fn validate_source_urls(config: &Config) -> anyhow::Result<usize> {
    let mut count = 0;
    for camera in &config.cameras {
        count += parsed_sources(camera)?.len();
    }
    Ok(count)
}

/// The recorded streams of `camera` with their parsed URLs. The error names
/// the camera and the stream, never the URL, which may carry credentials.
pub(crate) fn parsed_sources(
    camera: &CameraConfig,
) -> anyhow::Result<Vec<(StreamKind, iris_ingest::Url)>> {
    camera_sources(camera)
        .into_iter()
        .map(|(kind, url)| {
            let url = url
                .parse()
                .with_context(|| format!("parsing camera {} {kind} URL", camera.id))?;
            Ok((kind, url))
        })
        .collect()
}

/// The writer queue capacity for `source_count` streams: a zero per-stream
/// value counts as one, and a daemon with no stream to record is refused.
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

/// The fragment cap in media ticks; zero and unrepresentable values are refused.
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
        .flat_map(CameraConfig::connect_ports)
        .collect();
    if config.provisioning.enabled {
        connect_ports.extend(config.provisioning.sorted_connect_ports());
    }
    connect_ports.sort_unstable();
    connect_ports.dedup();
    SandboxSpec {
        read_only,
        read_write,
        connect_ports,
        bind_ports: vec![config.api.listen.port()],
    }
}

type Recording = (
    WriterHandle,
    Vec<StreamTask>,
    Arc<LiveRegistry>,
    StreamSpawner,
);

/// Starts the writer and the streams of `cameras`; the returned spawner
/// starts the streams of cameras added later. `reserved_streams` counts the
/// streams provisioning may add, so the writer queue has room for them.
fn start_recording(
    config: &Config,
    cameras: &[CameraConfig],
    reserved_streams: usize,
    approved_roots: &[ApprovedRoot],
    catalog: &CatalogHandle,
    shutdown: &CancellationToken,
) -> anyhow::Result<Recording> {
    let source_count: usize = cameras
        .iter()
        .map(|camera| camera_sources(camera).len())
        .sum();
    if source_count == 0 && reserved_streams == 0 {
        bail!("no streams to record");
    }
    let policy = StoragePolicy::from(&config.storage);
    let capacity = checked_queue_capacity(
        config.storage.max_queued_fragments,
        source_count + reserved_streams,
    )?;
    let observer = Arc::new(CatalogObserver::new(catalog.clone()));
    let writer = WriterHandle::spawn_with_approved_roots(
        approved_roots.to_vec(),
        policy,
        capacity,
        config.storage.min_free_percent,
        observer,
    )
    .context("starting the writer thread")?;
    let spawner = StreamSpawner {
        sink: Arc::new(WriterSink(writer.clone())),
        live: Arc::new(LiveRegistry::new()),
        max_fragment: checked_fragment_ticks(config.storage.max_fragment_ms)?,
        read_interval: config.ingest.read_interval(),
        // One ticker for every stream: a timer per stream costs one runtime unpark per
        // stream per tick (`iris_ingest::ReadTicker`).
        ticker: config.ingest.read_interval().map(ReadTicker::spawn),
        recv_low_water: config.ingest.recv_low_water(),
    };
    let mut streams = Vec::with_capacity(source_count);
    for camera in cameras {
        streams.extend(spawner.spawn(camera, parsed_sources(camera)?, shutdown));
    }
    let live = Arc::clone(&spawner.live);
    Ok((writer, streams, live, spawner))
}

/// What a stream task needs besides its camera: the writer's sink, the live
/// registry, the shared read ticker, and the ingest policy.
struct StreamSpawner {
    sink: Arc<dyn FragmentSink>,
    live: Arc<LiveRegistry>,
    max_fragment: MediaTicks,
    read_interval: Option<Duration>,
    ticker: Option<ReadTicker>,
    recv_low_water: Option<u32>,
}

impl StreamSpawner {
    /// Starts one task per entry of `sources`, the camera's parsed stream
    /// URLs ([`parsed_sources`]); the tasks end when `shutdown` is cancelled.
    fn spawn(
        &self,
        camera: &CameraConfig,
        sources: Vec<(StreamKind, iris_ingest::Url)>,
        shutdown: &CancellationToken,
    ) -> Vec<StreamTask> {
        let mut tasks = Vec::new();
        for (kind, url) in sources {
            let key = StreamKey {
                camera: camera.id.clone(),
                kind,
            };
            let ring = LiveRing::new(LIVE_RING_FRAMES);
            self.live.insert(key.clone(), ring.clone());
            let source = StreamSource {
                key: key.clone(),
                url,
                username: camera.username.clone(),
                password: camera.password.clone(),
            };
            let options = IngestOptions {
                max_fragment: self.max_fragment,
                timestamp_mode: camera.timestamp_mode,
                live: Some(ring),
                read_interval: self.read_interval,
                ticker: self.ticker.clone(),
                recv_low_water: self.recv_low_water,
                ..IngestOptions::default()
            };
            let stats = Arc::new(StreamStats::default());
            self.live.insert_stats(key.clone(), Arc::clone(&stats));
            let handle = tokio::spawn(run_stream(
                source,
                options,
                Arc::clone(&self.sink),
                Arc::clone(&stats),
                shutdown.clone(),
            ));
            info!(stream = %key, "recording");
            tasks.push(StreamTask {
                key,
                stats,
                handle,
                last_progress: (0, 0),
            });
        }
        tasks
    }
}

/// The recorded streams of one camera with their URLs.
fn camera_sources(camera: &CameraConfig) -> Vec<(StreamKind, String)> {
    let mut sources = Vec::new();
    for &kind in &camera.record {
        if let Some(url) = camera.url_for(kind) {
            sources.push((kind, url));
        } else {
            warn!(camera = %camera.id, %kind, "no URL for this stream; skipping");
        }
    }
    sources
}

/// Maps a camera's configuration to the endpoint event sources use; `None`
/// when its RTSP URL names no host.
fn endpoint(camera: &CameraConfig) -> Option<CameraEndpoint> {
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

/// Starts the event feeds of `cameras`; the returned spawner starts the feeds
/// of cameras added later.
fn start_events(
    cameras: &[CameraConfig],
    catalog: &CatalogHandle,
    events_tx: &broadcast::Sender<Arc<Event>>,
    shutdown: &CancellationToken,
) -> (Vec<EventTask>, EventSpawner) {
    let spawner = EventSpawner {
        sink: Arc::new(CatalogEventSink::new(catalog.clone(), events_tx.clone())),
    };
    let mut tasks = Vec::new();
    for camera in cameras {
        tasks.extend(spawner.spawn(camera, shutdown));
    }
    (tasks, spawner)
}

/// Where event feeds deliver: the catalog and the API's event broadcast.
struct EventSpawner {
    sink: Arc<dyn EventSink>,
}

impl EventSpawner {
    /// Starts one task per subscribable event source of `camera`; the tasks
    /// end when `shutdown` is cancelled.
    fn spawn(&self, camera: &CameraConfig, shutdown: &CancellationToken) -> Vec<EventTask> {
        let mut tasks = Vec::new();
        for &source in &camera.events {
            let Some(endpoint) = endpoint(camera) else {
                warn!(camera = %camera.id, "cannot derive a host from rtsp_url; no events");
                break;
            };
            let stats = Arc::new(EventStats::default());
            let handle = match source {
                EventSource::DahuaCgi => tokio::spawn(run_dahua_cgi(
                    endpoint,
                    Arc::clone(&self.sink),
                    Arc::clone(&stats),
                    shutdown.clone(),
                )),
                EventSource::OnvifPullPoint => tokio::spawn(run_onvif_pullpoint(
                    endpoint,
                    Arc::clone(&self.sink),
                    Arc::clone(&stats),
                    shutdown.clone(),
                )),
                EventSource::OnvifMetadata | EventSource::Manual => {
                    warn!(
                        camera = %camera.id,
                        ?source,
                        "not a subscribable event source; skipping"
                    );
                    continue;
                }
            };
            info!(camera = %camera.id, ?source, "subscribed to events");
            tasks.push(EventTask {
                camera: camera.id.clone(),
                source,
                stats,
                handle,
            });
        }
        tasks
    }
}

fn start_api(
    config: &Config,
    approved_roots: &[ApprovedRoot],
    catalog: &CatalogHandle,
    access: ApiAccess,
    live: Arc<LiveRegistry>,
    events: broadcast::Sender<Arc<Event>>,
    listener: tokio::net::TcpListener,
    shutdown: &CancellationToken,
) -> (JoinHandle<io::Result<()>>, Arc<ApiState>) {
    let snapshot_sources: HashMap<CameraId, SnapshotSource> = config
        .cameras
        .iter()
        .filter_map(|camera| Some((camera.id.clone(), snapshot_source(camera)?)))
        .collect();
    let active_cameras = config
        .cameras
        .iter()
        .map(|camera| {
            let provisioned = !access.static_ids.contains(&camera.id);
            (camera.id.clone(), active_camera(camera, provisioned))
        })
        .collect();
    let ApiAccess {
        capability_secret,
        control,
        static_ids: _,
    } = access;
    let state = Arc::new(ApiState {
        catalog: catalog.clone(),
        live,
        events,
        tokens: config.api.tokens.clone(),
        capability_secret,
        recording_roots: approved_roots.to_vec(),
        public_metrics: config.api.public_metrics,
        snapshot_sources: RwLock::new(snapshot_sources),
        active_cameras: RwLock::new(active_cameras),
        control,
        stats: Arc::new(ApiStats::new_with_media_limits(
            config.api.max_streaming_media,
            config.api.max_bounded_media,
        )),
        started: WallUs::now(),
        started_monotonic: Instant::now(),
        shutdown: shutdown.clone(),
    });
    let router = iris_api::router(Arc::clone(&state));
    let limits = ServeLimits {
        max_connections: config.api.max_connections,
        ..ServeLimits::default()
    };
    let api = tokio::spawn(iris_api::serve_listener(
        listener,
        router,
        limits,
        shutdown.clone(),
    ));
    (api, state)
}

/// The camera's snapshot endpoint, when it has a snapshot method and its RTSP
/// URL names a host.
fn snapshot_source(camera: &CameraConfig) -> Option<SnapshotSource> {
    Some(SnapshotSource::new(
        camera.host()?,
        camera.http_port_or_default(),
        camera.https,
        camera.tls_verify,
        camera.username.clone(),
        camera.password.clone(),
        camera.snapshot_method()?,
    ))
}

/// The camera as the API lists it; `provisioned` marks one the API added.
fn active_camera(camera: &CameraConfig, provisioned: bool) -> ActiveCamera {
    ActiveCamera {
        id: camera.id.clone(),
        name: camera.name.clone(),
        vendor: camera.vendor.clone(),
        provisioned,
    }
}

/// The streams provisioning may add at run time, two per camera up to its
/// limit, so the writer queue and the startup check leave room for them; a
/// daemon with provisioning enabled may therefore start without any camera.
fn reserved_streams(config: &Config) -> usize {
    if config.provisioning.enabled {
        2 * config.provisioning.max_cameras
    } else {
        0
    }
}

/// Appends the cameras the API added (`cameras.toml` in the state directory)
/// to the configured ones; returns the configured ids and the added cameras.
fn load_provisioned(config: &mut Config) -> anyhow::Result<(HashSet<CameraId>, Vec<CameraConfig>)> {
    let static_ids: HashSet<CameraId> = config
        .cameras
        .iter()
        .map(|camera| camera.id.clone())
        .collect();
    let provisioned = if config.provisioning.enabled {
        let configured: Vec<CameraId> = static_ids.iter().cloned().collect();
        provision::load(&config.storage.state_dir, &configured)
            .context("loading the provisioned cameras")?
    } else {
        Vec::new()
    };
    config.cameras.extend(provisioned.iter().cloned());
    Ok((static_ids, provisioned))
}

/// The next provisioning request, or never when provisioning is off or the
/// API dropped its sender.
async fn control_request(provisioning: Option<&mut Provisioning>) -> Option<ControlRequest> {
    match provisioning {
        Some(provisioning) if !provisioning.closed => {
            let request = provisioning.requests.recv().await;
            if request.is_none() {
                provisioning.closed = true;
            }
            request
        }
        _ => std::future::pending().await,
    }
}

/// Acts on one provisioning request and answers it; a client that went away
/// just drops the answer. A change re-points the self-check at the streams
/// that record afterwards.
async fn handle_control(
    daemon: &mut Daemon,
    state: &mut MonitorState,
    request: ControlRequest,
) -> bool {
    let changed = match request {
        ControlRequest::Add { camera, reply } => {
            let result = add_camera(daemon, *camera).await;
            let changed = result.is_ok();
            let _ = reply.send(result);
            changed
        }
        ControlRequest::Remove { id, reply } => {
            let result = remove_camera(daemon, &id).await;
            let changed = result.is_ok();
            let _ = reply.send(result);
            changed
        }
        ControlRequest::Discover { wait, reply } => {
            let lookups = daemon.provisioning.as_mut().map(|p| &mut p.lookups);
            spawn_lookup(lookups, &daemon.shutdown, reply, async move {
                iris_events::discover(wait)
                    .await
                    .map_err(|error| ProvisionError::Discovery(error.to_string()))
            });
            false
        }
        ControlRequest::StreamUrls { endpoint, reply } => {
            let port = endpoint.http_port;
            if daemon.config.provisioning.connect_ports.contains(&port) {
                let lookups = daemon.provisioning.as_mut().map(|p| &mut p.lookups);
                spawn_lookup(lookups, &daemon.shutdown, reply, async move {
                    iris_events::stream_urls(&endpoint)
                        .await
                        .map_err(|error| ProvisionError::Camera(error.kind().to_owned()))
                });
            } else {
                let _ = reply.send(Err(ProvisionError::Port(port)));
            }
            false
        }
    };
    if changed {
        state.refresh_selfcheck(daemon.streams.iter().map(|task| task.key.clone()).collect());
    }
    changed
}

/// Runs an ONVIF lookup beside the monitor loop, at most `MAX_LOOKUPS` at a
/// time, and answers through `reply`. A lookup over the limit is refused at
/// once, and so is any lookup while provisioning is disabled, so the loop
/// never waits on the network. A lookup still running when `shutdown` is
/// cancelled answers with a lifecycle error.
fn spawn_lookup<T>(
    lookups: Option<&mut Lookups>,
    shutdown: &CancellationToken,
    reply: oneshot::Sender<Result<T, ProvisionError>>,
    lookup: impl Future<Output = Result<T, ProvisionError>> + Send + 'static,
) where
    T: Send + 'static,
{
    let Some(lookups) = lookups else {
        let _ = reply.send(Err(ProvisionError::Lifecycle(
            "provisioning is disabled".into(),
        )));
        return;
    };
    let Ok(permit) = Arc::clone(&lookups.permits).try_acquire_owned() else {
        let _ = reply.send(Err(ProvisionError::Busy));
        return;
    };
    lookups.reap();
    let shutdown = shutdown.clone();
    lookups.tasks.spawn(async move {
        let result = tokio::select! {
            () = shutdown.cancelled() => {
                Err(ProvisionError::Lifecycle("the daemon is stopping".into()))
            }
            result = lookup => result,
        };
        drop(permit);
        let _ = reply.send(result);
    });
}

/// Rewrites the camera store off the runtime thread: the write syncs the
/// file and its directory, which must not stall the sessions, the API, and
/// the watchdog that share the thread.
async fn save_store(dir: &Path, cameras: Vec<CameraConfig>) -> Result<(), ProvisionError> {
    let dir = dir.to_path_buf();
    tokio::task::spawn_blocking(move || provision::save(&dir, &cameras))
        .await
        .map_err(|error| ProvisionError::Store(format!("store task failed: {error}")))?
        .map_err(|error| ProvisionError::Store(format!("{error:#}")))
}

/// Adds a camera at run time: checks it against the configuration and the
/// limits (including that its stream URLs parse and its ports are allowed),
/// registers it in the catalog, persists it, starts its streams and event
/// feeds under a token of its own, and lists it in the API.
async fn add_camera(
    daemon: &mut Daemon,
    camera: CameraConfig,
) -> Result<ActiveCamera, ProvisionError> {
    let Some(provisioning) = daemon.provisioning.as_mut() else {
        return Err(ProvisionError::Lifecycle("provisioning is disabled".into()));
    };
    if daemon
        .config
        .cameras
        .iter()
        .any(|known| known.id == camera.id)
    {
        return Err(if provisioning.static_ids.contains(&camera.id) {
            ProvisionError::Configured(camera.id)
        } else {
            ProvisionError::Duplicate(camera.id)
        });
    }
    if provisioning.cameras.len() >= daemon.config.provisioning.max_cameras {
        return Err(ProvisionError::Limit(
            daemon.config.provisioning.max_cameras,
        ));
    }
    camera
        .validate()
        .map_err(|error| ProvisionError::Invalid(error.to_string()))?;
    let sources =
        parsed_sources(&camera).map_err(|error| ProvisionError::Invalid(format!("{error:#}")))?;
    if let Some(port) = daemon.config.provisioning.refused_port(&camera) {
        return Err(ProvisionError::Port(port));
    }
    daemon
        .catalog
        .ensure_camera(&camera.id, &camera.name, camera.vendor.as_deref())
        .await
        .map_err(|error| ProvisionError::Lifecycle(format!("registering the camera: {error}")))?;
    let mut cameras = provisioning.cameras.clone();
    cameras.push(camera.clone());
    save_store(&daemon.config.storage.state_dir, cameras.clone()).await?;
    let token = daemon.shutdown.child_token();
    daemon
        .streams
        .extend(provisioning.streams.spawn(&camera, sources, &token));
    daemon
        .events
        .extend(provisioning.events.spawn(&camera, &token));
    let active = active_camera(&camera, /* provisioned */ true);
    if let Some(source) = snapshot_source(&camera) {
        provisioning
            .api_state
            .snapshot_sources
            .write()
            .expect("snapshot registry")
            .insert(camera.id.clone(), source);
    }
    provisioning
        .api_state
        .active_cameras
        .write()
        .expect("camera registry")
        .insert(camera.id.clone(), active.clone());
    if let Some(days) = camera.retention_days {
        daemon
            .age_limits
            .write()
            .expect("age limits")
            .insert(camera.id.clone(), days);
    }
    provisioning.tokens.insert(camera.id.clone(), token);
    provisioning.cameras = cameras;
    info!(camera = %camera.id, streams = camera.record.len(), "camera added through the API");
    daemon.config.cameras.push(camera);
    Ok(active)
}

/// Removes a camera the API added: rewrites the store without it first, so a
/// failed write changes nothing, then cancels its tasks, hands them to a
/// retirement task that joins them within [`CAMERA_STOP_GRACE`] and aborts
/// the rest, and forgets the camera in the API. Its recordings and catalog
/// rows stay.
async fn remove_camera(daemon: &mut Daemon, id: &CameraId) -> Result<(), ProvisionError> {
    let Some(provisioning) = daemon.provisioning.as_mut() else {
        return Err(ProvisionError::Lifecycle("provisioning is disabled".into()));
    };
    if provisioning.static_ids.contains(id) {
        return Err(ProvisionError::Configured(id.clone()));
    }
    if !provisioning.tokens.contains_key(id) {
        return Err(ProvisionError::Unknown(id.clone()));
    }
    let remaining: Vec<CameraConfig> = provisioning
        .cameras
        .iter()
        .filter(|camera| camera.id != *id)
        .cloned()
        .collect();
    save_store(&daemon.config.storage.state_dir, remaining.clone()).await?;
    if let Some(token) = provisioning.tokens.remove(id) {
        token.cancel();
    }
    let (streams, kept): (Vec<StreamTask>, Vec<StreamTask>) = std::mem::take(&mut daemon.streams)
        .into_iter()
        .partition(|task| task.key.camera == *id);
    daemon.streams = kept;
    let (events, kept): (Vec<EventTask>, Vec<EventTask>) = std::mem::take(&mut daemon.events)
        .into_iter()
        .partition(|task| task.camera == *id);
    daemon.events = kept;
    while let Some(result) = daemon.retiring.try_join_next() {
        if let Err(err) = result {
            warn!(%err, "camera retirement task failed");
        }
    }
    let handles: Vec<JoinHandle<()>> = streams
        .into_iter()
        .map(|task| task.handle)
        .chain(events.into_iter().map(|task| task.handle))
        .collect();
    daemon.retiring.spawn(retire_camera(id.clone(), handles));
    provisioning.api_state.live.remove_camera(id);
    provisioning
        .api_state
        .active_cameras
        .write()
        .expect("camera registry")
        .remove(id);
    provisioning
        .api_state
        .snapshot_sources
        .write()
        .expect("snapshot registry")
        .remove(id);
    daemon.age_limits.write().expect("age limits").remove(id);
    provisioning.cameras = remaining;
    daemon.config.cameras.retain(|camera| camera.id != *id);
    info!(camera = %id, "camera removed through the API");
    Ok(())
}

/// Waits for a removed camera's tasks to end within [`CAMERA_STOP_GRACE`],
/// then aborts and observes the rest. Runs beside the monitor loop, so a
/// removal never delays signals or watchdog ticks.
async fn retire_camera(id: CameraId, handles: Vec<JoinHandle<()>>) {
    let deadline = tokio::time::Instant::now() + CAMERA_STOP_GRACE;
    let mut late = Vec::new();
    for mut handle in handles {
        match tokio::time::timeout_at(deadline, &mut handle).await {
            Ok(Ok(())) => {}
            Ok(Err(err)) => warn!(camera = %id, %err, "a removed camera's task failed"),
            Err(_) => {
                handle.abort();
                late.push(handle);
            }
        }
    }
    if late.is_empty() {
        info!(camera = %id, "a removed camera's tasks ended");
        return;
    }
    warn!(
        camera = %id,
        late = late.len(),
        "a removed camera's tasks did not end in time; aborted them"
    );
    for handle in late {
        if let Err(err) = handle.await
            && !err.is_cancelled()
        {
            warn!(camera = %id, %err, "a removed camera's aborted task failed");
        }
    }
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
    raw: RawCalls,
}

impl ReportState {
    /// Replaces the stream and feed lists, keeping each surviving feed's last
    /// pull-latency snapshot so its next interval stays correct.
    fn adopt(&mut self, mut lists: ReportLists) {
        for event in &mut lists.events {
            if let Some(previous) = self
                .events
                .iter()
                .find(|known| known.camera == event.camera && known.source == event.source)
            {
                event.pull_seen = previous.pull_seen;
            }
        }
        self.streams = lists.streams;
        self.events = lists.events;
    }
}

/// The streams and event feeds one report covers.
struct ReportLists {
    streams: Vec<ReportStream>,
    events: Vec<ReportEvent>,
}

impl ReportLists {
    fn from_daemon(daemon: &Daemon) -> Self {
        Self {
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
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MaintenanceOutcome {
    Complete,
    TimedOut,
    Cancelled,
}

/// Timeout and raw-call bookkeeping of one single-flight maintenance owner.
///
/// A job that exceeds `timeout` is dropped and its state kept. Awaiting the
/// catalog is cancellable, but a raw filesystem call on a standard-library thread
/// is not: while one is still in the kernel no further job starts, and if it is
/// still there `escalation` after the timeout the daemon stops so systemd
/// restarts it.
struct MaintenanceGuard {
    job: &'static str,
    escalation_reason: &'static str,
    raw: RawCalls,
    timeout: Duration,
    escalation: Duration,
    timeouts: u64,
    stuck_since: Option<Instant>,
    last_skip_warning: Option<Instant>,
}

impl MaintenanceGuard {
    fn new(job: &'static str, escalation_reason: &'static str) -> Self {
        Self {
            job,
            escalation_reason,
            raw: RawCalls::default(),
            timeout: MAINTENANCE_TIMEOUT,
            escalation: RAW_CALL_ESCALATION,
            timeouts: 0,
            stuck_since: None,
            last_skip_warning: None,
        }
    }

    /// Whether a job may start: deferred while a raw call of an earlier job is
    /// still in the kernel, and escalated once it has been there longer than
    /// the bound.
    fn admit(&mut self) -> Admission {
        let in_flight = self.raw.in_flight();
        if in_flight == 0 {
            if let Some(since) = self.stuck_since.take() {
                info!(
                    job = self.job,
                    stuck_s = since.elapsed().as_secs(),
                    "raw filesystem call returned; resuming"
                );
            }
            return Admission::Start;
        }
        let since = *self.stuck_since.get_or_insert_with(Instant::now);
        if since.elapsed() >= self.escalation {
            error!(
                job = self.job,
                in_flight,
                stuck_s = since.elapsed().as_secs(),
                "a raw filesystem call has not returned; stopping so the service restarts"
            );
            return Admission::Escalate(self.escalation_reason);
        }
        if self
            .last_skip_warning
            .is_none_or(|last| last.elapsed() >= SKIPPED_LAUNCH_WARNING_INTERVAL)
        {
            warn!(
                job = self.job,
                in_flight,
                stuck_s = since.elapsed().as_secs(),
                "skipping: a raw filesystem call of the timed-out job has not returned"
            );
            self.last_skip_warning = Some(Instant::now());
        }
        Admission::Defer
    }

    /// Records a timeout; the caller keeps the job's state for the next launch.
    fn timed_out(&mut self) {
        self.timeouts = self.timeouts.saturating_add(1);
        let in_flight = self.raw.in_flight();
        warn!(
            job = self.job,
            timeouts = self.timeouts,
            raw_in_flight = in_flight,
            "maintenance job exceeded its work deadline; keeping its state"
        );
        if in_flight > 0 {
            self.stuck_since.get_or_insert_with(Instant::now);
        }
    }
}

/// What [`MaintenanceGuard::admit`] decided about launching a job.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Admission {
    /// Nothing is in flight; the job may start.
    Start,
    /// A raw call of an earlier job is still in the kernel; try at the next tick.
    Defer,
    /// That call outlived the escalation bound; the daemon stops for this reason.
    Escalate(&'static str),
}

struct ReportCompletion {
    state: ReportState,
    outcome: MaintenanceOutcome,
}

struct ReportJob {
    idle: Option<ReportState>,
    active: Option<JoinHandle<ReportCompletion>>,
    /// The streams and feeds recording after a camera was added or removed,
    /// adopted by the next launch so a report in flight keeps its lists.
    pending: Option<ReportLists>,
    shutdown: CancellationToken,
    guard: MaintenanceGuard,
}

impl ReportJob {
    fn new(daemon: &Daemon) -> Self {
        let guard = MaintenanceGuard::new(
            "statistics report",
            "a statistics report filesystem call did not return",
        );
        let ReportLists { streams, events } = ReportLists::from_daemon(daemon);
        Self {
            idle: Some(ReportState {
                streams,
                events,
                writer: daemon.writer.clone(),
                catalog: daemon.catalog.clone(),
                roots: daemon.config.storage.video_roots.clone(),
                snapshots: Snapshots::default(),
                raw: guard.raw.clone(),
            }),
            active: None,
            pending: None,
            shutdown: daemon.shutdown.clone(),
            guard,
        }
    }

    /// Points the next report at the streams and event feeds recording after
    /// a camera was added or removed. A report in flight finishes with the
    /// lists it started with.
    fn refresh(&mut self, daemon: &Daemon) {
        self.pending = Some(ReportLists::from_daemon(daemon));
    }

    /// Starts a report unless one is running or a raw call of a timed-out one is
    /// still in the kernel. Fails with the reason to stop the daemon once such
    /// a call has outlived the escalation bound.
    fn launch(&mut self, gauges: Gauges) -> Result<(), &'static str> {
        let Some(mut state) = self.idle.take() else {
            if self.active.is_none() {
                warn!("statistics report owner has no state");
            }
            return Ok(());
        };
        match self.guard.admit() {
            Admission::Start => {}
            Admission::Defer => {
                self.idle = Some(state);
                return Ok(());
            }
            Admission::Escalate(reason) => {
                self.idle = Some(state);
                return Err(reason);
            }
        }
        if let Some(lists) = self.pending.take() {
            state.adopt(lists);
        }
        let shutdown = self.shutdown.clone();
        let timeout = self.guard.timeout;
        self.active = Some(tokio::spawn(async move {
            let outcome = {
                let work = report(&mut state, &gauges);
                tokio::pin!(work);
                tokio::select! {
                    () = shutdown.cancelled() => MaintenanceOutcome::Cancelled,
                    result = tokio::time::timeout(timeout, &mut work) => {
                        if result.is_err() {
                            MaintenanceOutcome::TimedOut
                        } else {
                            MaintenanceOutcome::Complete
                        }
                    }
                }
            };
            ReportCompletion { state, outcome }
        }));
        Ok(())
    }

    fn running(&self) -> bool {
        self.active.is_some()
    }

    async fn wait_active(&mut self) -> Result<ReportCompletion, JoinError> {
        self.active
            .as_mut()
            .expect("report task exists while selected")
            .await
    }

    /// Takes the finished report's outcome back; fails with the reason to stop
    /// the daemon when the task was cancelled or failed.
    fn finish(&mut self, result: Result<ReportCompletion, JoinError>) -> Result<(), &'static str> {
        self.active.take();
        match result {
            Ok(completion) if completion.outcome == MaintenanceOutcome::Complete => {
                self.idle = Some(completion.state);
                Ok(())
            }
            Ok(completion) if completion.outcome == MaintenanceOutcome::TimedOut => {
                self.idle = Some(completion.state);
                self.guard.timed_out();
                Ok(())
            }
            Ok(_) => Err("statistics report cancelled unexpectedly"),
            Err(err) => {
                error!(%err, "statistics report task failed");
                Err("statistics report task failed")
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
    age_limits: Arc<RwLock<HashMap<CameraId, u32>>>,
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
    guard: MaintenanceGuard,
}

impl RetentionJob {
    fn new(daemon: &Daemon, peak: Arc<Peak>) -> Self {
        let guard = MaintenanceGuard::new(
            "retention pass",
            "a retention filesystem call did not return",
        );
        Self {
            idle: Some(RetentionJobState {
                catalog: daemon.catalog.clone(),
                config: daemon.config.clone(),
                approved_roots: daemon.approved_roots.clone(),
                age_limits: Arc::clone(&daemon.age_limits),
                peak,
                state: retention::RetentionState::new(guard.raw.clone()),
            }),
            active: None,
            shutdown: daemon.shutdown.clone(),
            guard,
        }
    }

    /// Starts a pass unless one is running or a raw call of a timed-out one is
    /// still in the kernel. Fails with the reason to stop the daemon once such
    /// a call has outlived the escalation bound.
    fn launch(&mut self) -> Result<(), &'static str> {
        let Some(mut state) = self.idle.take() else {
            if self.active.is_none() {
                warn!("retention owner has no state");
            }
            return Ok(());
        };
        match self.guard.admit() {
            Admission::Start => {}
            Admission::Defer => {
                self.idle = Some(state);
                return Ok(());
            }
            Admission::Escalate(reason) => {
                self.idle = Some(state);
                return Err(reason);
            }
        }
        let shutdown = self.shutdown.clone();
        let timeout = self.guard.timeout;
        self.active = Some(tokio::spawn(async move {
            let outcome = {
                let work = retention_tick(&mut state);
                tokio::pin!(work);
                tokio::select! {
                    () = shutdown.cancelled() => MaintenanceOutcome::Cancelled,
                    result = tokio::time::timeout(timeout, &mut work) => {
                        if result.is_err() {
                            MaintenanceOutcome::TimedOut
                        } else {
                            MaintenanceOutcome::Complete
                        }
                    }
                }
            };
            RetentionCompletion { state, outcome }
        }));
        Ok(())
    }

    fn running(&self) -> bool {
        self.active.is_some()
    }

    async fn wait_active(&mut self) -> Result<RetentionCompletion, JoinError> {
        self.active
            .as_mut()
            .expect("retention task exists while selected")
            .await
    }

    /// Takes the finished pass's outcome back; fails with the reason to stop
    /// the daemon when the task was cancelled or failed.
    fn finish(
        &mut self,
        result: Result<RetentionCompletion, JoinError>,
    ) -> Result<(), &'static str> {
        self.active.take();
        match result {
            Ok(completion) if completion.outcome == MaintenanceOutcome::Complete => {
                self.idle = Some(completion.state);
                Ok(())
            }
            Ok(completion) if completion.outcome == MaintenanceOutcome::TimedOut => {
                self.idle = Some(completion.state);
                self.guard.timed_out();
                Ok(())
            }
            Ok(_) => Err("retention task cancelled unexpectedly"),
            Err(err) => {
                error!(%err, "retention task failed");
                Err("retention task failed")
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
    background_teardowns: usize,
    retention_max_ms: u64,
    selfcheck_ok: u64,
    selfcheck_failed: u64,
    selfcheck_skipped: u64,
    report_timeouts: u64,
    retention_timeouts: u64,
    sandbox_enforced: u8,
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
                        background_teardowns: iris_ingest::background_teardowns(),
                        retention_max_ms: maintenance.retention_max.take(),
                        selfcheck_ok: selfcheck_stats.ok(),
                        selfcheck_failed: selfcheck_stats.failed(),
                        selfcheck_skipped: selfcheck_stats.skipped(),
                        report_timeouts: maintenance.report.guard.timeouts,
                        retention_timeouts: maintenance.retention.guard.timeouts,
                        sandbox_enforced: daemon.sandbox.level(),
                    };
                    if let Err(reason) = maintenance.report.launch(gauges) {
                        failure = Some(reason);
                        break;
                    }
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
                        "an ingest task made no progress since the last check or has ended; \
                         withholding the watchdog ping"
                    );
                }
            }
            _ = state.retention_ticker.tick() => {
                if let Err(reason) = maintenance.retention.launch() {
                    failure = Some(reason);
                    break;
                }
            }
            _ = state.selfcheck_ticker.tick() => {
                state.selfcheck.launch_next(&daemon.catalog).await;
            }
            Some(request) = control_request(daemon.provisioning.as_mut()) => {
                if handle_control(daemon, state, request).await {
                    maintenance.report.refresh(daemon);
                }
            }
            result = maintenance.report.wait_active(), if maintenance.report.running() => {
                if let Err(reason) = maintenance.report.finish(result) {
                    failure = Some(reason);
                    break;
                }
            }
            result = maintenance.retention.wait_active(), if maintenance.retention.running() => {
                if let Err(reason) = maintenance.retention.finish(result) {
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
    let age_limits = job.age_limits.read().expect("age limits").clone();
    match retention::pass_approved(
        &job.catalog,
        &job.approved_roots,
        &mut job.state,
        storage.min_free_percent,
        storage.quota_bytes(),
        &age_limits,
    )
    .await
    {
        Ok(report) => {
            job.peak
                .observe(u64::try_from(report.duration.as_millis()).unwrap_or(u64::MAX));
        }
        Err(err) => warn!(error = format_args!("{err:#}"), "retention pass failed"),
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

/// Writes the statistics lines and the systemd status. The `process` line
/// goes out before the awaited catalog and root steps, so a stalled query
/// cannot suppress the line that carries the report timeouts.
async fn report(state: &mut ReportState, gauges: &Gauges) {
    let now = WallUs::now();
    log_streams(&state.streams, now);
    log_events(&mut state.events, now);
    log_writer(&state.writer.stats(), &mut state.snapshots);
    log_process(gauges);
    log_catalog(&state.catalog, &mut state.snapshots).await;
    log_roots(&state.roots, &state.raw).await;
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
            err_ms = s.error_ticks() / i64::from(TIMESCALE / 1_000),
            frag_kib_max = s.fragment_bytes_peak().take() >> 10,
            frag_s_max = format_args!(
                "{:.1}",
                s.fragment_ticks_peak().take() as f64 / TICKS_PER_SEC
            ),
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
            source = %task.source,
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

/// A count from the catalog statistics, or `-` when the query failed.
fn stat_count(value: Option<u64>) -> String {
    value.map_or_else(|| "-".to_owned(), |value| value.to_string())
}

/// Bytes from the catalog statistics scaled by `unit` with `decimals`
/// places, or `-` when the query failed.
fn stat_scaled(value: Option<u64>, unit: f64, decimals: usize) -> String {
    value.map_or_else(
        || "-".to_owned(),
        |value| format!("{:.*}", decimals, value as f64 / unit),
    )
}

/// Writes the `catalog` line. The fields that come from the statistics
/// query are `-` when it fails, which is logged; the metrics fields never
/// depend on a query.
async fn log_catalog(catalog: &CatalogHandle, prev: &mut Snapshots) {
    let metrics = catalog.metrics();
    let stats = match catalog.stats().await {
        Ok(stats) => Some(stats),
        Err(error) => {
            warn!(%error, "cannot read catalog statistics");
            None
        }
    };
    let field = |value: fn(&iris_catalog::CatalogStats) -> u64| stats.as_ref().map(value);
    let (_, txn_p99_ms, txn_max_ms) = latency(metrics.transaction(), &mut prev.transaction);
    let (_, query_p99_ms, query_max_ms) = latency(metrics.query(), &mut prev.query);
    let (_, wait_p99_ms, wait_max_ms) = latency(metrics.wait(), &mut prev.wait);
    let (_, event_commit_p99_ms, event_commit_max_ms) =
        latency(metrics.event_commit(), &mut prev.event_commit);
    info!(
        segments = %stat_count(field(|c| c.segments)),
        gib = %stat_scaled(field(|c| c.sealed_bytes), GIB, 2),
        deleting_segments = %stat_count(field(|c| c.deleting_segments)),
        deleting_mib = %stat_scaled(field(|c| c.deleting_bytes), MIB, 1),
        events = %stat_count(field(|c| c.events)),
        event_mib = %stat_scaled(field(|c| c.event_bytes), MIB, 1),
        retained_events = %stat_count(field(|c| c.retained_events)),
        pruned_events = %stat_count(field(|c| c.pruned_events)),
        db_mib = %stat_scaled(field(|c| c.db_bytes), MIB, 1),
        wal_mib = %stat_scaled(field(|c| c.wal_bytes), MIB, 1),
        queued = metrics.queue_depth(),
        queue_mib = format_args!("{:.1}", metrics.queue_bytes() as f64 / MIB),
        queue_peak = metrics.queue_peak().take(),
        queue_mib_peak = format_args!("{:.1}", metrics.queue_bytes_peak().take() as f64 / MIB),
        coalesced = metrics.coalesced(),
        rejected = metrics.rejected(),
        rejected_events = metrics.rejected_events(),
        rejected_progress = metrics.rejected_progress(),
        rejected_queries = metrics.rejected_queries(),
        rejected_lifecycle = metrics.rejected_lifecycle(),
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

async fn log_roots(roots: &[PathBuf], raw: &RawCalls) {
    log_roots_with(roots, raw, |root| iris_store::free_space(&root)).await;
}

async fn log_roots_with<F>(roots: &[PathBuf], raw: &RawCalls, probe: F)
where
    F: Fn(PathBuf) -> io::Result<iris_store::FreeSpace> + Send + 'static,
{
    let roots = roots.to_vec();
    let (result_tx, result_rx) = oneshot::channel();
    // `statvfs` can remain uninterruptible in the kernel. If the report times
    // out, this counted standard-library thread outlives it, the report owner
    // starts no other report until it returns, and it cannot hold process exit.
    // Its last action is the send, so nothing waits for its exit.
    if let Err(err) = raw.spawn("irisd-root-probe", move || {
        let results = roots
            .into_iter()
            .map(|root| {
                let result = probe(root.clone());
                (root, result)
            })
            .collect::<Vec<_>>();
        let _ = result_tx.send(results);
    }) {
        warn!(%err, "cannot start recording-root probe");
        return;
    }
    let Ok(results) = result_rx.await else {
        warn!("recording-root probe stopped without a result");
        return;
    };
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
        teardowns = gauges.background_teardowns,
        retention_max_ms = gauges.retention_max_ms,
        selfcheck_ok = gauges.selfcheck_ok,
        selfcheck_failed = gauges.selfcheck_failed,
        selfcheck_skipped = gauges.selfcheck_skipped,
        report_timeouts = gauges.report_timeouts,
        retention_timeouts = gauges.retention_timeouts,
        sandbox_enforced = gauges.sandbox_enforced,
        "process"
    );
}

fn record_shutdown_error(first: &mut Option<anyhow::Error>, error: anyhow::Error) {
    error!(%error, "shutdown stage failed");
    if first.is_none() {
        *first = Some(error);
    }
}

/// Runs `operation` on an uncounted standard-library thread named `name` and
/// returns the channel its result arrives on, so a blocking shutdown step
/// never holds the runtime past its deadline.
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

/// Awaits `future` until `deadline`; `None` when the deadline passes first.
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

/// A task ordered shutdown waits for: joined within the producer grace, else
/// aborted and then observed. Every owner follows the same three steps, so
/// an owner is added in one place.
struct ShutdownOwner {
    label: String,
    abort: AbortHandle,
    done: Pin<Box<dyn Future<Output = Result<(), JoinError>> + Send>>,
    joined: bool,
}

impl ShutdownOwner {
    fn new<T: Send + 'static>(label: String, handle: JoinHandle<T>) -> Self {
        let abort = handle.abort_handle();
        Self {
            label,
            abort,
            done: Box::pin(async move { handle.await.map(|_| ()) }),
            joined: false,
        }
    }

    /// Joins the task, recording a failure under its label.
    async fn join(&mut self, first_error: &mut Option<anyhow::Error>) {
        if let Err(err) = (&mut self.done).await {
            record_shutdown_error(first_error, anyhow::anyhow!("{} failed: {err}", self.label));
        }
        self.joined = true;
    }

    fn abort_if_unjoined(&self) {
        if !self.joined {
            self.abort.abort();
        }
    }

    /// Observes an aborted task; cancellation is the expected outcome.
    async fn observe(&mut self, first_error: &mut Option<anyhow::Error>) {
        if self.joined {
            return;
        }
        if let Err(err) = (&mut self.done).await
            && !err.is_cancelled()
        {
            record_shutdown_error(
                first_error,
                anyhow::anyhow!("{} abort failed: {err}", self.label),
            );
        }
        self.joined = true;
    }
}

/// A set of like tasks (camera retirements, ONVIF lookups) that ordered
/// shutdown joins, aborts, and observes as one owner.
struct ShutdownGroup {
    label: &'static str,
    tasks: JoinSet<()>,
}

impl ShutdownGroup {
    async fn join(&mut self, first_error: &mut Option<anyhow::Error>) {
        while let Some(result) = self.tasks.join_next().await {
            if let Err(err) = result {
                record_shutdown_error(
                    first_error,
                    anyhow::anyhow!("{} task failed: {err}", self.label),
                );
            }
        }
    }

    async fn observe(&mut self, first_error: &mut Option<anyhow::Error>) {
        while let Some(result) = self.tasks.join_next().await {
            if let Err(err) = result
                && !err.is_cancelled()
            {
                record_shutdown_error(
                    first_error,
                    anyhow::anyhow!("{} abort failed: {err}", self.label),
                );
            }
        }
    }
}

/// Stops everything in order against one absolute schedule: every producer
/// and maintenance owner shares the producer grace (joined cooperatively,
/// then aborted and observed), then the writer seals, the API drains, and
/// the catalog closes, each against its own deadline, keeping the first
/// failure.
async fn stop_with_schedule(
    mut daemon: Daemon,
    mut monitor: MonitorState,
    mut maintenance: MaintenanceJobs,
    schedule: ShutdownSchedule,
) -> anyhow::Result<()> {
    systemd::stopping();
    let started = tokio::time::Instant::now();
    let cooperative_wait = schedule
        .producer_grace
        .saturating_sub(schedule.abort_observation_grace);
    let cooperative_deadline = started + cooperative_wait;
    let producer_deadline = started + schedule.producer_grace;
    let writer_deadline = started + schedule.writer_deadline;
    let api_deadline = started + schedule.api_deadline;
    let shutdown_deadline = started + schedule.budget;
    let mut first_error = None;

    daemon.shutdown.cancel();
    let mut groups = shutdown_groups(
        std::mem::take(&mut daemon.retiring),
        daemon
            .provisioning
            .as_mut()
            .map(|provisioning| std::mem::take(&mut provisioning.lookups.tasks)),
    );
    let mut owners = shutdown_owners(
        std::mem::take(&mut daemon.streams),
        std::mem::take(&mut daemon.events),
        daemon.loop_probe,
        &mut monitor,
        &mut maintenance,
    );

    let wait_for_producers = async {
        for owner in &mut owners {
            owner.join(&mut first_error).await;
        }
        for group in &mut groups {
            group.join(&mut first_error).await;
        }
    };
    if tokio::time::timeout_at(cooperative_deadline, wait_for_producers)
        .await
        .is_err()
    {
        record_shutdown_error(
            &mut first_error,
            anyhow::anyhow!(
                "producers did not stop within the cooperative wait of {cooperative_wait:?}"
            ),
        );
    }
    for owner in &owners {
        owner.abort_if_unjoined();
    }
    for group in &mut groups {
        group.tasks.abort_all();
    }
    let observe_aborted = async {
        for owner in &mut owners {
            owner.observe(&mut first_error).await;
        }
        for group in &mut groups {
            group.observe(&mut first_error).await;
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

    seal_writer(&daemon.writer, writer_deadline, &mut first_error).await;
    stop_api(
        daemon.api.take(),
        api_deadline,
        shutdown_deadline,
        &mut first_error,
    )
    .await;
    close_catalog(&daemon.catalog, shutdown_deadline, &mut first_error).await;
    info!("stopped");
    first_error.map_or(Ok(()), Err)
}

/// Every single task ordered shutdown owns: the producers, the loop probe,
/// and the maintenance and self-check tasks still running.
fn shutdown_owners(
    streams: Vec<StreamTask>,
    events: Vec<EventTask>,
    loop_probe: JoinHandle<()>,
    monitor: &mut MonitorState,
    maintenance: &mut MaintenanceJobs,
) -> Vec<ShutdownOwner> {
    let mut owners: Vec<ShutdownOwner> = Vec::new();
    for task in streams {
        owners.push(ShutdownOwner::new(
            format!("stream {} task", task.key),
            task.handle,
        ));
    }
    for task in events {
        owners.push(ShutdownOwner::new(
            format!("event task for camera {} ({:?})", task.camera, task.source),
            task.handle,
        ));
    }
    owners.push(ShutdownOwner::new("loop probe task".into(), loop_probe));
    if let Some(task) = monitor.selfcheck.cancel_and_take() {
        owners.push(ShutdownOwner::new("selfcheck task".into(), task));
    }
    if let Some(task) = maintenance.report.take_active() {
        owners.push(ShutdownOwner::new("statistics report task".into(), task));
    }
    if let Some(task) = maintenance.retention.take_active() {
        owners.push(ShutdownOwner::new("retention task".into(), task));
    }
    owners
}

/// The task sets ordered shutdown owns: camera retirements and, with
/// provisioning enabled, the ONVIF lookups.
fn shutdown_groups(retiring: JoinSet<()>, lookups: Option<JoinSet<()>>) -> Vec<ShutdownGroup> {
    let mut groups = vec![ShutdownGroup {
        label: "camera retirement",
        tasks: retiring,
    }];
    if let Some(tasks) = lookups {
        groups.push(ShutdownGroup {
            label: "ONVIF lookup",
            tasks,
        });
    }
    groups
}

/// Seals the recordings on a shutdown thread, within `deadline`.
async fn seal_writer(
    writer: &WriterHandle,
    deadline: tokio::time::Instant,
    first_error: &mut Option<anyhow::Error>,
) {
    let writer = writer.clone();
    match run_on_shutdown_thread("irisd-writer-shutdown", move || writer.shutdown()) {
        Ok(result) => match tokio::time::timeout_at(deadline, result).await {
            Ok(Ok(Ok(()))) => {}
            Ok(Ok(Err(err))) => record_shutdown_error(
                first_error,
                anyhow::Error::new(err).context("sealing recordings"),
            ),
            Ok(Err(err)) => record_shutdown_error(
                first_error,
                anyhow::anyhow!("writer shutdown thread stopped without a result: {err}"),
            ),
            Err(_) => record_shutdown_error(
                first_error,
                anyhow::anyhow!("writer sealing exceeded its absolute shutdown deadline"),
            ),
        },
        Err(err) => record_shutdown_error(
            first_error,
            anyhow::Error::new(err).context("starting writer shutdown thread"),
        ),
    }
}

/// Lets the API drain until `deadline`, then aborts it and observes the
/// abort until `budget_deadline`. An API task the monitor consumed is `None`.
async fn stop_api(
    api: Option<JoinHandle<io::Result<()>>>,
    deadline: tokio::time::Instant,
    budget_deadline: tokio::time::Instant,
    first_error: &mut Option<anyhow::Error>,
) {
    let Some(mut api) = api else {
        return;
    };
    match tokio::time::timeout_at(deadline, &mut api).await {
        Ok(Ok(Ok(()))) => {}
        Ok(Ok(Err(err))) => record_shutdown_error(
            first_error,
            anyhow::Error::new(err).context("API server shutdown"),
        ),
        Ok(Err(err)) => record_shutdown_error(
            first_error,
            anyhow::anyhow!("API task failed during shutdown: {err}"),
        ),
        Err(_) => {
            record_shutdown_error(
                first_error,
                anyhow::anyhow!("API shutdown exceeded its absolute deadline"),
            );
            api.abort();
            match complete_by_deadline(budget_deadline, &mut api).await {
                Some(Err(err)) if !err.is_cancelled() => {
                    record_shutdown_error(first_error, anyhow::anyhow!("API abort failed: {err}"));
                }
                Some(_) => {}
                None => record_shutdown_error(
                    first_error,
                    anyhow::anyhow!("API abort observation exceeded the service shutdown budget"),
                ),
            }
        }
    }
}

/// Closes the catalog on a shutdown thread, within `deadline`.
async fn close_catalog(
    catalog: &CatalogHandle,
    deadline: tokio::time::Instant,
    first_error: &mut Option<anyhow::Error>,
) {
    let closing = catalog.clone();
    // If the OS refuses the helper thread, retain one owner until process exit.
    // Dropping the final CatalogHandle joins synchronously and would otherwise
    // escape the daemon's absolute shutdown deadline.
    let drop_guard = catalog.clone();
    match run_on_shutdown_thread("irisd-catalog-shutdown", move || closing.shutdown()) {
        Ok(result) => {
            drop(drop_guard);
            match tokio::time::timeout_at(deadline, result).await {
                Ok(Ok(())) => {}
                Ok(Err(err)) => record_shutdown_error(
                    first_error,
                    anyhow::anyhow!("catalog shutdown thread stopped without a result: {err}"),
                ),
                Err(_) => record_shutdown_error(
                    first_error,
                    anyhow::anyhow!("catalog shutdown exceeded the service shutdown budget"),
                ),
            }
        }
        Err(err) => {
            std::mem::forget(drop_guard);
            record_shutdown_error(
                first_error,
                anyhow::Error::new(err).context("starting catalog shutdown thread"),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_period_rejects_zero() {
        assert!(validate_period("period", Duration::from_nanos(1)).is_ok());
        assert!(validate_period("period", Duration::ZERO).is_err());
    }

    /// The watchdog is fed only while every ingest task is alive and has
    /// moved since the last check: frames or reconnects. One task that
    /// stands still, or one that has ended, withholds it.
    #[tokio::test]
    async fn the_watchdog_needs_every_stream_alive_and_moving() {
        let task = |last_progress: (u64, u64), running: bool| {
            let handle = if running {
                tokio::spawn(std::future::pending::<()>())
            } else {
                tokio::spawn(async {})
            };
            StreamTask {
                key: StreamKey {
                    camera: "cam1".parse().unwrap(),
                    kind: StreamKind::Main,
                },
                stats: Arc::new(StreamStats::new()),
                handle,
                last_progress,
            }
        };
        // The counters stand at (0, 0): a remembered (9, 0) means progress.
        let moved = (9, 0);
        let mut streams = vec![task(moved, true), task((0, 7), true)];
        assert!(all_streams_progressed(&mut streams));
        // The check remembered the counters, and nothing moved since.
        assert_eq!(streams[0].last_progress, (0, 0));
        assert!(!all_streams_progressed(&mut streams));

        let mut one_still = vec![task(moved, true), task((0, 0), true)];
        assert!(!all_streams_progressed(&mut one_still));

        let mut one_ended = vec![task(moved, true), task(moved, false)];
        while !one_ended[1].handle.is_finished() {
            tokio::task::yield_now().await;
        }
        assert!(!all_streams_progressed(&mut one_ended));

        assert!(all_streams_progressed(&mut []));
        for task in streams.iter().chain(&one_still).chain(&one_ended) {
            task.handle.abort();
        }
    }

    #[test]
    fn age_limits_come_from_the_cameras_that_set_one() {
        let camera = |id: &str, extra: &str| {
            format!(
                "[[camera]]\nid=\"{id}\"\nname=\"n\"\nrtsp_url=\"rtsp://192.0.2.7/{id}\"\n\
                 username=\"u\"\npassword=\"p\"\n{extra}"
            )
        };
        let config = Config::from_toml_str(&format!(
            "[api]\nallow_unauthenticated=true\n{}{}{}",
            camera("cam1", "retention_days=7\n"),
            camera("cam2", ""),
            camera("cam3", "retention_days=30\n"),
        ))
        .unwrap();
        let limits = age_limits(&config);
        assert_eq!(limits.len(), 2);
        assert_eq!(limits[&"cam1".parse::<CameraId>().unwrap()], 7);
        assert_eq!(limits[&"cam3".parse::<CameraId>().unwrap()], 30);
    }

    #[test]
    fn queue_capacity_rejects_no_streams_and_overflow() {
        assert!(checked_queue_capacity(0, 0).is_err());
        assert_eq!(checked_queue_capacity(0, 1).unwrap(), 1);
        assert_eq!(checked_queue_capacity(32, 14).unwrap(), 448);
        assert!(checked_queue_capacity(usize::MAX, 2).is_err());
    }

    #[test]
    fn fragment_ticks_reject_zero_and_unrepresentable_millis() {
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
        let toml = |raw: &str| {
            format!(
                "[api]\nallow_unauthenticated=true\n[[camera]]\nid='cam1'\nname=''\n\
                 rtsp_url='{raw}'\nusername=''\npassword=''\nrecord=['main']\n"
            )
        };
        // Shapes the configuration refuses name the field and the rule, never the value.
        let host_rule = "rtsp_url must be an rtsp:// URL with a host";
        let credentials_rule = "rtsp_url must not embed credentials";
        for (raw, rule) in [
            (format!("rtsp://{marker}@"), credentials_rule),
            (format!("rtsp://{marker}@192.0.2.7/x"), credentials_rule),
            (format!("not-a-url?password={marker}"), host_rule),
            (format!("not-a-url#{marker}"), host_rule),
            (format!("not-a-url-{marker}"), host_rule),
        ] {
            let text = Config::from_toml_str(&toml(&raw)).unwrap_err().to_string();
            assert!(!text.contains(marker), "{text}");
            assert!(!text.contains(&raw), "{text}");
            assert!(text.contains(rule), "{raw}: {text}");
        }
        // A URL of the accepted shape that the parser still refuses is reported
        // without its text.
        let raw = format!("rtsp://192.0.2.7:99999/x?password={marker}");
        let parser_cause = raw.parse::<iris_ingest::Url>().unwrap_err().to_string();
        let config = Config::from_toml_str(&toml(&raw)).unwrap();
        let error = validate_source_urls(&config).unwrap_err();
        let text = format!("{error:#}");
        assert!(!text.contains(marker), "{text}");
        assert!(!text.contains(&raw), "{text}");
        assert!(text.contains("camera cam1 main URL"), "{text}");
        assert!(text.contains(&parser_cause), "{text}");
    }

    #[tokio::test]
    async fn monitor_state_reports_a_cpu_sampler_failure() {
        let config = Config::from_toml_str(
            "[api]\nallow_unauthenticated=true\n[[camera]]\nid='cam1'\nname=''\n\
             rtsp_url='rtsp://127.0.0.1/main'\nusername=''\npassword=''\nrecord=['main']\n",
        )
        .unwrap();
        let args = Args {
            config: PathBuf::from("config.toml"),
            no_sandbox: true,
            stats_interval: Duration::from_secs(1),
            selfcheck_cycle: Duration::from_secs(1),
        };
        let result =
            MonitorState::try_new_with_cpu(&config, &args, &CancellationToken::new(), || {
                Err(io::Error::other("injected proc failure"))
            });
        let Err(error) = result else {
            panic!("injected monitor initialization must fail");
        };
        assert!(error.to_string().contains("reading /proc/self/stat"));
    }

    /// Runs the daemon's start-up over `config_body` (the `[storage]` table
    /// is prepended) up to the monitor initializer, which fails with an
    /// injected error, and returns the failure. A failure naming the signal
    /// handler proves that every validation step before it passed.
    async fn start_until_monitor_init(dir: &Path, config_body: &str) -> anyhow::Error {
        let root = dir.join("video");
        let state = dir.join("state");
        let config_path = dir.join("config.toml");
        std::fs::write(
            &config_path,
            format!(
                "[storage]\nvideo_roots=['{}']\nstate_dir='{}'\n{config_body}",
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
            MonitorState::try_new_with_initializers(
                config,
                args,
                shutdown,
                CpuMeter::try_new,
                |_| Err(io::Error::other("injected signal failure")),
            )
        })
        .await;
        let Err(error) = result else {
            panic!("injected signal initialization must fail");
        };
        assert!(!state.exists(), "state directory must not be created");
        assert!(!root.exists(), "recording root must not be created");
        error
    }

    #[tokio::test]
    async fn signal_initialization_failure_precedes_startup_side_effects() {
        let dir = tempfile::tempdir().unwrap();
        let error = start_until_monitor_init(
            dir.path(),
            "[api]\nallow_unauthenticated=true\n[[camera]]\nid='cam1'\nname=''\n\
             rtsp_url='rtsp://127.0.0.1/main'\nusername=''\npassword=''\nrecord=['main']\n",
        )
        .await;
        let text = format!("{error:#}");
        assert!(text.contains("installing the SIGTERM handler"), "{text}");
        assert!(text.contains("injected signal failure"), "{text}");
    }

    #[tokio::test]
    async fn provisioning_lets_a_daemon_start_without_configured_cameras() {
        let dir = tempfile::tempdir().unwrap();
        let error = start_until_monitor_init(
            dir.path(),
            "[api]\nallow_unauthenticated=true\n[provisioning]\nenabled=true\nmax_cameras=2\n",
        )
        .await;
        let text = format!("{error:#}");
        assert!(
            text.contains("installing the SIGTERM handler"),
            "validation must pass without cameras: {text}"
        );
    }

    #[tokio::test]
    async fn a_daemon_without_cameras_or_provisioning_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let error =
            start_until_monitor_init(dir.path(), "[api]\nallow_unauthenticated=true\n").await;
        let text = format!("{error:#}");
        assert!(text.contains("no cameras configured"), "{text}");
    }

    #[tokio::test(start_paused = true)]
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
        assert_eq!(
            heartbeats.load(Ordering::Relaxed),
            3,
            "ticks at 5, 10, and 15 ms"
        );
        release_tx.send(()).unwrap();
        assignment.await.unwrap();
    }

    #[tokio::test]
    async fn lookups_are_refused_without_provisioning() {
        let (reply, answer) = oneshot::channel::<Result<(), ProvisionError>>();
        spawn_lookup(None, &CancellationToken::new(), reply, async { Ok(()) });
        assert!(matches!(
            answer.await.unwrap(),
            Err(ProvisionError::Lifecycle(_))
        ));
    }

    #[tokio::test]
    async fn lookups_are_bounded_by_their_permits() {
        let mut lookups = Lookups {
            permits: Arc::new(Semaphore::new(1)),
            tasks: JoinSet::new(),
        };
        let shutdown = CancellationToken::new();
        let (release, released) = oneshot::channel::<()>();
        let (first_reply, first) = oneshot::channel::<Result<(), ProvisionError>>();
        spawn_lookup(Some(&mut lookups), &shutdown, first_reply, async move {
            let _ = released.await;
            Ok(())
        });
        let (second_reply, second) = oneshot::channel::<Result<(), ProvisionError>>();
        spawn_lookup(Some(&mut lookups), &shutdown, second_reply, async {
            Ok(())
        });
        assert!(matches!(second.await.unwrap(), Err(ProvisionError::Busy)));
        let _ = release.send(());
        assert!(first.await.unwrap().is_ok());
        let (third_reply, third) = oneshot::channel::<Result<(), ProvisionError>>();
        spawn_lookup(Some(&mut lookups), &shutdown, third_reply, async { Ok(()) });
        assert!(
            third.await.unwrap().is_ok(),
            "the permit returns when the lookup ends"
        );
        lookups.reap();
        assert!(lookups.tasks.is_empty(), "finished lookups are observed");
    }

    #[tokio::test]
    async fn a_lookup_in_flight_at_shutdown_answers_with_a_lifecycle_error() {
        let mut lookups = Lookups::new();
        let shutdown = CancellationToken::new();
        let (reply, answer) = oneshot::channel::<Result<(), ProvisionError>>();
        spawn_lookup(Some(&mut lookups), &shutdown, reply, std::future::pending());
        shutdown.cancel();
        assert!(matches!(
            answer.await.unwrap(),
            Err(ProvisionError::Lifecycle(_))
        ));
        assert!(lookups.tasks.join_next().await.unwrap().is_ok());
    }

    #[tokio::test(start_paused = true)]
    async fn a_removed_camera_s_late_tasks_are_aborted_after_the_grace() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let dropped = Arc::new(AtomicUsize::new(0));
        let prompt = tokio::spawn(async {});
        let late = tokio::spawn({
            let guard = CountOnDrop(Arc::clone(&dropped));
            async move {
                let _guard = guard;
                std::future::pending::<()>().await;
            }
        });
        let started = tokio::time::Instant::now();
        retire_camera("gone".parse().unwrap(), vec![prompt, late]).await;
        assert_eq!(started.elapsed(), CAMERA_STOP_GRACE);
        assert_eq!(
            dropped.load(Ordering::SeqCst),
            1,
            "the late task was aborted"
        );
    }

    #[test]
    fn provisioning_reserves_queue_room_for_the_cameras_it_may_add() {
        let empty = Config::from_toml_str("[api]\nallow_unauthenticated=true\n").unwrap();
        assert_eq!(reserved_streams(&empty), 0);
        assert!(checked_queue_capacity(32, validate_source_urls(&empty).unwrap()).is_err());
        let provisioning = Config::from_toml_str(
            "[api]\nallow_unauthenticated=true\n[provisioning]\nenabled=true\nmax_cameras=4\n",
        )
        .unwrap();
        assert_eq!(reserved_streams(&provisioning), 8);
        let sources = validate_source_urls(&provisioning).unwrap();
        assert_eq!(
            checked_queue_capacity(32, sources + reserved_streams(&provisioning)).unwrap(),
            32 * 8
        );
    }

    fn test_gauges() -> Gauges {
        Gauges {
            cpu: CpuSplit::default(),
            rss: 0,
            uptime: Duration::ZERO,
            loop_lag_ms: 0,
            watchdog_withheld: 0,
            background_teardowns: 0,
            retention_max_ms: 0,
            selfcheck_ok: 0,
            selfcheck_failed: 0,
            selfcheck_skipped: 0,
            report_timeouts: 0,
            retention_timeouts: 0,
            sandbox_enforced: 0,
        }
    }

    fn test_guard() -> MaintenanceGuard {
        MaintenanceGuard::new("test job", "a test filesystem call did not return")
    }

    #[tokio::test]
    async fn report_lists_follow_camera_changes_at_the_next_launch() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("video");
        let writer = spawn_test_writer(&root, Arc::new(CountingObserver::default()));
        open_test_writer(&writer).await;
        let mut config = Config::default();
        config.storage.video_roots = vec![root];
        config.storage.state_dir = dir.path().join("state");
        let catalog = CatalogHandle::open(&dir.path().join("catalog.db")).unwrap();
        let stream = |camera: &str| StreamTask {
            key: StreamKey {
                camera: camera.parse().unwrap(),
                kind: StreamKind::Main,
            },
            stats: Arc::new(StreamStats::default()),
            handle: tokio::spawn(std::future::pending::<()>()),
            last_progress: (0, 0),
        };
        let feed = |camera: &str| EventTask {
            camera: camera.parse().unwrap(),
            source: EventSource::DahuaCgi,
            stats: Arc::new(EventStats::default()),
            handle: tokio::spawn(std::future::pending::<()>()),
        };
        let mut daemon = daemon_for_test(
            config,
            catalog,
            writer,
            CancellationToken::new(),
            vec![stream("cam1")],
            vec![feed("cam1")],
            tokio::spawn(async { Ok(()) }),
            tokio::spawn(async {}),
        );
        let mut job = ReportJob::new(&daemon);

        // A surviving feed keeps the snapshot its last report left behind.
        daemon.events[0]
            .stats
            .pull_latency()
            .record(Duration::from_millis(7));
        let seen = daemon.events[0].stats.pull_latency().snapshot();
        let mut state = job.idle.take().unwrap();
        state.events[0].pull_seen = seen;
        daemon.streams.push(stream("cam2"));
        daemon.events.push(feed("cam2"));
        state.adopt(ReportLists::from_daemon(&daemon));
        assert_eq!(state.streams.len(), 2);
        assert_eq!(state.events[0].pull_seen, seen);
        assert_eq!(state.events[1].pull_seen, HistogramSnapshot::default());
        job.idle = Some(state);

        // A refresh waits for the next launch, which adopts it.
        daemon.streams.push(stream("cam3"));
        job.refresh(&daemon);
        assert_eq!(job.idle.as_ref().unwrap().streams.len(), 2);
        assert_eq!(job.launch(test_gauges()), Ok(()));
        let result = tokio::time::timeout(Duration::from_secs(5), job.wait_active())
            .await
            .unwrap();
        assert_eq!(job.finish(result), Ok(()));
        let state = job.idle.as_ref().unwrap();
        assert_eq!(state.streams.len(), 3);
        assert_eq!(state.events.len(), 2);
        assert!(job.pending.is_none());
    }

    #[tokio::test]
    async fn a_running_report_is_not_relaunched() {
        let report_shutdown = CancellationToken::new();
        let report_handle = tokio::spawn(std::future::pending::<ReportCompletion>());
        let report_id = report_handle.id();
        let mut report = ReportJob {
            idle: None,
            active: Some(report_handle),
            pending: None,
            shutdown: report_shutdown,
            guard: test_guard(),
        };
        for _ in 0..100 {
            assert_eq!(report.launch(test_gauges()), Ok(()));
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
        assert_eq!(report.launch(test_gauges()), Ok(()));
        assert!(
            report.active.is_none(),
            "cancelled report must not relaunch"
        );
    }

    #[tokio::test]
    async fn a_running_retention_pass_is_not_relaunched() {
        let retention_shutdown = CancellationToken::new();
        let retention_handle = tokio::spawn(std::future::pending::<RetentionCompletion>());
        let retention_id = retention_handle.id();
        let mut retention = RetentionJob {
            idle: None,
            active: Some(retention_handle),
            shutdown: retention_shutdown,
            guard: test_guard(),
        };
        for _ in 0..100 {
            assert_eq!(retention.launch(), Ok(()));
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
        assert_eq!(retention.launch(), Ok(()));
        assert!(
            retention.active.is_none(),
            "cancelled retention must not relaunch"
        );
    }

    fn retention_job_for_test(catalog: &CatalogHandle, timeout: Duration) -> RetentionJob {
        let mut guard = test_guard();
        guard.timeout = timeout;
        RetentionJob {
            idle: Some(RetentionJobState {
                catalog: catalog.clone(),
                config: Config::default(),
                approved_roots: Vec::new(),
                age_limits: Arc::new(RwLock::new(HashMap::new())),
                peak: Arc::new(Peak::new()),
                state: retention::RetentionState::new(guard.raw.clone()),
            }),
            active: None,
            shutdown: CancellationToken::new(),
            guard,
        }
    }

    async fn finish_retention_within(
        job: &mut RetentionJob,
        limit: Duration,
    ) -> Result<(), &'static str> {
        let result = tokio::time::timeout(limit, job.wait_active())
            .await
            .expect("the retention job must finish within its test deadline");
        job.finish(result)
    }

    async fn wait_for_raw_calls(raw: &RawCalls, count: usize) {
        tokio::time::timeout(Duration::from_secs(5), async {
            while raw.in_flight() != count {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .expect("raw call count must settle within its test deadline");
    }

    #[tokio::test]
    async fn a_timed_out_retention_pass_keeps_its_state_and_relaunches() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("catalog.db");
        let catalog = CatalogHandle::open(&path).unwrap();
        let mut retention = retention_job_for_test(&catalog, Duration::from_millis(50));
        // An operator shell holding the write lock stalls the pass in a
        // cancellable catalog wait, with no raw filesystem call in flight.
        let lock = rusqlite::Connection::open(&path).unwrap();
        lock.execute_batch("BEGIN IMMEDIATE").unwrap();
        assert_eq!(retention.launch(), Ok(()));
        assert!(retention.running());
        assert_eq!(
            finish_retention_within(&mut retention, Duration::from_secs(5)).await,
            Ok(()),
            "a timeout without a raw call must not stop the daemon"
        );
        assert_eq!(retention.guard.timeouts, 1);
        assert!(
            retention.idle.is_some(),
            "the timed-out pass returns its state"
        );
        assert_eq!(retention.guard.raw.in_flight(), 0);
        lock.execute_batch("COMMIT").unwrap();

        retention.guard.timeout = Duration::from_secs(10);
        assert_eq!(retention.launch(), Ok(()));
        assert!(retention.running(), "the next tick relaunches the pass");
        assert_eq!(
            finish_retention_within(&mut retention, Duration::from_secs(10)).await,
            Ok(())
        );
        assert!(retention.idle.is_some());
        assert_eq!(retention.guard.timeouts, 1);
        catalog.shutdown();
    }

    #[tokio::test]
    async fn a_raw_call_in_flight_defers_the_next_job_until_it_returns() {
        let dir = tempfile::tempdir().unwrap();
        let catalog = CatalogHandle::open(&dir.path().join("catalog.db")).unwrap();
        let mut retention = retention_job_for_test(&catalog, MAINTENANCE_TIMEOUT);
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let raw_call = retention
            .guard
            .raw
            .spawn("irisd-test-raw", move || {
                let _ = release_rx.recv();
            })
            .unwrap();
        // The pass that started the call timed out and returned its state.
        retention.guard.timed_out();
        for _ in 0..10 {
            assert_eq!(retention.launch(), Ok(()));
            assert!(
                !retention.running(),
                "no pass may start while the raw call is in the kernel"
            );
            assert_eq!(
                retention.guard.raw.in_flight(),
                1,
                "exactly one raw call remains"
            );
        }
        release_tx.send(()).unwrap();
        wait_for_raw_calls(&retention.guard.raw, 0).await;
        raw_call.join().unwrap();
        assert_eq!(retention.launch(), Ok(()));
        assert!(
            retention.running(),
            "the job relaunches once the call returns"
        );
        assert_eq!(
            finish_retention_within(&mut retention, Duration::from_secs(10)).await,
            Ok(())
        );
        assert_eq!(retention.guard.stuck_since, None);
        catalog.shutdown();
    }

    #[tokio::test]
    async fn a_raw_call_stuck_beyond_the_escalation_bound_stops_the_daemon() {
        let dir = tempfile::tempdir().unwrap();
        let catalog = CatalogHandle::open(&dir.path().join("catalog.db")).unwrap();
        let mut retention = retention_job_for_test(&catalog, MAINTENANCE_TIMEOUT);
        retention.guard.escalation = Duration::from_millis(500);
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let _raw_call = retention
            .guard
            .raw
            .spawn("irisd-test-raw", move || {
                let _ = release_rx.recv();
            })
            .unwrap();
        retention.guard.timed_out();
        assert_eq!(retention.launch(), Ok(()), "the call is within the bound");
        tokio::time::sleep(Duration::from_millis(600)).await;
        assert_eq!(
            retention.launch(),
            Err("a test filesystem call did not return")
        );
        assert!(!retention.running());
        release_tx.send(()).unwrap();
        wait_for_raw_calls(&retention.guard.raw, 0).await;
        catalog.shutdown();
    }

    #[tokio::test]
    async fn shutdown_completes_within_its_budget_while_a_raw_call_is_stuck() {
        let dir = tempfile::tempdir().unwrap();
        let writer = spawn_test_writer(
            &dir.path().join("video"),
            Arc::new(CountingObserver::default()),
        );
        let mut config = Config::default();
        config.storage.state_dir = dir.path().join("state");
        let catalog = CatalogHandle::open(&dir.path().join("catalog.db")).unwrap();
        let shutdown = CancellationToken::new();
        let monitor = monitor_for_test(&config, &shutdown);
        let api = tokio::spawn(async { Ok::<(), io::Error>(()) });
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
        let mut maintenance = MaintenanceJobs::new(&daemon);
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let raw = maintenance.retention.guard.raw.clone();
        let _stuck = raw
            .spawn("irisd-test-stuck", move || {
                let _ = release_rx.recv();
            })
            .unwrap();
        maintenance.retention.guard.timed_out();
        let schedule = shutdown_schedule_for_test();
        let started = Instant::now();
        let result = stop_with_schedule(daemon, monitor, maintenance, schedule).await;
        let elapsed = started.elapsed();
        assert!(
            elapsed < schedule.budget,
            "shutdown took {elapsed:?} with a raw call stuck"
        );
        assert!(result.is_ok(), "{result:?}");
        assert_eq!(raw.in_flight(), 1, "the stuck call cannot hold shutdown");
        release_tx.send(()).unwrap();
        wait_for_raw_calls(&raw, 0).await;
    }

    #[test]
    fn the_descriptor_soft_limit_rises_to_a_finite_hard_limit() {
        use rustix::process::{Resource, Rlimit};

        let limit = |current, maximum| Rlimit { current, maximum };
        assert_eq!(
            raised_descriptor_limit(limit(Some(1_024), Some(524_288))),
            Some(limit(Some(524_288), Some(524_288)))
        );
        assert_eq!(
            raised_descriptor_limit(limit(Some(65_536), Some(65_536))),
            None
        );
        assert_eq!(raised_descriptor_limit(limit(Some(1_024), None)), None);
        assert_eq!(raised_descriptor_limit(limit(None, None)), None);

        // Any process may raise its own soft limit up to the hard limit.
        raise_descriptor_limit();
        let after = rustix::process::getrlimit(Resource::Nofile);
        if let Some(hard) = after.maximum {
            assert_eq!(after.current, Some(hard));
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_aborted_root_probe_stays_counted_until_statvfs_returns() {
        use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

        let active = Arc::new(AtomicBool::new(false));
        let calls = Arc::new(AtomicUsize::new(0));
        let worker_active = Arc::clone(&active);
        let worker_calls = Arc::clone(&calls);
        let (started_tx, started_rx) = std::sync::mpsc::sync_channel(0);
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let release_rx = Arc::new(std::sync::Mutex::new(release_rx));
        let raw = RawCalls::default();
        let task_raw = raw.clone();
        let mut task = tokio::spawn(async move {
            log_roots_with(&[PathBuf::from("synthetic-root")], &task_raw, move |_| {
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
        assert_eq!(raw.in_flight(), 1, "the detached statvfs stays counted");
        release_tx.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(1), async {
            while active.load(Ordering::SeqCst) || raw.in_flight() != 0 {
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
                rotate_age: Duration::MAX,
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
        MonitorState::try_new(
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
            sandbox: SandboxEnforcement::Disabled,
            age_limits: Arc::new(RwLock::new(HashMap::new())),
            provisioning: None,
            retiring: JoinSet::new(),
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

    #[expect(
        clippy::too_many_lines,
        reason = "the matrix assembles every producer and maintenance owner in one fixture"
    )]
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
            error.to_string().contains("producers did not stop"),
            "{error:#}"
        );
        assert!(
            elapsed >= Duration::from_millis(150),
            "the cooperative wait runs its full 150 ms: elapsed {elapsed:?}"
        );
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
        assert!(error.to_string().contains("producers did not stop"));
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
            #[expect(unreachable_code, reason = "the value fixes the task's output type")]
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
            }],
            vec![EventTask {
                camera: "event".parse().unwrap(),
                source: EventSource::DahuaCgi,
                stats: Arc::new(EventStats::default()),
                handle: event,
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
    fn daemon_root_admission_creates_a_nonexistent_root_with_its_marker() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("nested/recordings");
        let roots =
            admit_configured_roots(&dir.path().join("catalog.db"), std::slice::from_ref(&root))
                .unwrap();
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

        let identity = approved[0].identity();
        let catalog = CatalogHandle::open(&catalog_path).unwrap();
        catalog
            .register_recording_root(approved[0].path(), identity)
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
            "[api]\nallow_unauthenticated=true\n[[camera]]\nid=\"test\"\nname=\"test\"\n\
             rtsp_url=\"rtsp://127.0.0.1:1/main\"\nusername=\"\"\npassword=\"\"\n\
             record=[\"main\"]\n",
        )
        .unwrap();
        config.storage.video_roots = vec![first.clone(), second.clone()];
        config.storage.min_free_percent = 0;
        config.storage.preallocate_bytes = 0;
        let catalog = CatalogHandle::open(&catalog_path).unwrap();
        let shutdown = CancellationToken::new();
        let (writer, streams, _, _) =
            start_recording(&config, &config.cameras, 0, &approved, &catalog, &shutdown).unwrap();
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
            "[api]\nallow_unauthenticated=true\n[[camera]]\nid=\"cam1\"\nname=\"n\"\n\
             rtsp_url=\"{url}\"\nusername=\"u\"\npassword=\"p\"\nhttp_port={port}\n"
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
    }

    #[test]
    fn a_camera_without_a_host_is_refused_when_the_configuration_loads() {
        let hostless = "[api]\nallow_unauthenticated=true\n[[camera]]\nid=\"cam1\"\nname=\"n\"\n\
                        rtsp_url=\"not a url\"\nusername=\"u\"\npassword=\"p\"\n";
        assert!(Config::from_toml_str(hostless).is_err());
    }

    #[test]
    fn sandbox_spec_covers_roots_state_and_ports() {
        let config = Config::from_toml_str(
            "[storage]\nvideo_roots=[\"/tmp/irisd-test-root\"]\n\
             state_dir=\"/tmp/irisd-test-state\"\n\
             [api]\nlisten=\"127.0.0.1:9000\"\nallow_unauthenticated=true\n\
             [[camera]]\nid=\"a\"\nname=\"\"\nrtsp_url=\"rtsp://192.0.2.1/x\"\nusername=\"\"\n\
             password=\"\"\nhttp_port=8080\n\
             [[camera]]\nid=\"b\"\nname=\"\"\nrtsp_url=\"rtsp://192.0.2.2:18554/x\"\n\
             sub_url=\"rtsp://192.0.2.2/y\"\nusername=\"\"\npassword=\"\"\n",
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

/// How far an unsealed catalog row extends past its last durable progress in
/// range listings: two sync intervals (the writer reports progress per sync),
/// one fragment at its cap plus a quarter, and [`OPEN_SEGMENT_GRACE_MARGIN`],
/// never less than the catalog's default.
fn open_segment_grace(storage: &StorageConfig) -> Duration {
    let sync = Duration::from_millis(storage.sync_interval_ms);
    let fragment = Duration::from_millis(storage.max_fragment_ms.saturating_mul(5) / 4);
    (sync * 2 + fragment + OPEN_SEGMENT_GRACE_MARGIN).max(OPEN_SEGMENT_GRACE)
}

#[cfg(test)]
mod grace_tests {
    use super::*;

    #[test]
    fn the_default_storage_settings_keep_the_catalog_default_grace() {
        assert_eq!(
            open_segment_grace(&StorageConfig::default()),
            OPEN_SEGMENT_GRACE
        );
    }

    #[test]
    fn long_sync_intervals_and_fragments_extend_the_grace() {
        let storage = StorageConfig {
            sync_interval_ms: 20_000,
            max_fragment_ms: 60_000,
            ..StorageConfig::default()
        };
        assert_eq!(open_segment_grace(&storage), Duration::from_secs(125));
    }
}
