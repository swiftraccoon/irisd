//! `replay-bench`: the daemon's cost against replayed cameras, with nothing
//! but loopback involved.

use std::collections::BTreeMap;
use std::io::Write as _;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpStream};
use std::os::unix::fs::{DirBuilderExt as _, MetadataExt as _};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::Context;
use clap::Parser;
use serde::Serialize;
use serde_json::json;

use crate::logs::{kv, number, split, strip_ansi};
use crate::time;

/// Arguments of `replay-bench`.
#[derive(Debug, Parser)]
pub struct Args {
    /// The recording every camera replays.
    #[arg(long)]
    file: PathBuf,
    /// Number of cameras.
    #[arg(long, default_value_t = 1, value_parser = clap::value_parser!(u32).range(1..=256))]
    cameras: u32,
    /// How long the daemon runs.
    #[arg(long, default_value_t = 120, value_parser = clap::value_parser!(u64).range(25..=86_400))]
    seconds: u64,
    /// Fresh or marker-owned scratch root.
    #[arg(long)]
    root: PathBuf,
    /// The daemon binary.
    #[arg(long)]
    irisd: PathBuf,
    /// The replay binary.
    #[arg(long)]
    replay: PathBuf,
    /// The replay server's port.
    #[arg(long, default_value_t = 18554, value_parser = clap::value_parser!(u16).range(1..))]
    port: u16,
    /// The daemon's API port.
    #[arg(long, default_value_t = 18600, value_parser = clap::value_parser!(u16).range(1..))]
    api_port: u16,
    /// `ingest.read_interval_ms` for the daemon.
    #[arg(long, default_value_t = 0, value_parser = clap::value_parser!(u64).range(..=86_400))]
    read_interval_ms: u64,
    /// `ingest.recv_low_water_kib` for the daemon.
    #[arg(long, default_value_t = 0, value_parser = clap::value_parser!(u32).range(..=1_024))]
    recv_low_water_kib: u32,
    /// The daemon's statistics interval.
    #[arg(long, default_value = "30s")]
    stats_interval: String,
    /// Extra arguments for `irisd run`.
    #[arg(long = "extra-arg")]
    daemon_arg: Vec<String>,
    /// Extra arguments for `iris-replay`.
    #[arg(long = "replay-arg")]
    replay_arg: Vec<String>,
    /// A label for the result line.
    #[arg(long, default_value = "")]
    label: String,
    /// Keep the recordings under the root.
    #[arg(long)]
    keep: bool,
    /// After 20 s of warm-up, sample the daemon with `perf record` for this
    /// long into `ROOT/perf.data` (through `sudo -n`, so kernel frames are included).
    #[arg(long, default_value_t = 0, value_name = "SECONDS", value_parser = clap::value_parser!(u64).range(..=86_400))]
    perf: u64,
}

#[derive(Clone, Copy, Debug)]
struct Sample {
    t: Instant,
    wall: f64,
    utime: i64,
    stime: i64,
    rss_kib: i64,
    voluntary: i64,
}

fn proc_sample(pid: u32) -> anyhow::Result<Sample> {
    let status = std::fs::read_to_string(format!("/proc/{pid}/status"))?;
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat"))?;
    let after = &stat[stat.rfind(')').map_or(0, |i| i + 2)..];
    let fields: Vec<&str> = after.split_whitespace().collect();
    let n = |i: usize| {
        fields
            .get(i)
            .and_then(|v| v.parse::<i64>().ok())
            .unwrap_or(0)
    };
    let mut rss_kib = 0;
    let mut voluntary = 0;
    for line in status.lines() {
        if let Some((key, value)) = line.split_once(':') {
            let v = value
                .split_whitespace()
                .next()
                .and_then(|v| v.parse::<i64>().ok())
                .unwrap_or(0);
            match key {
                "VmRSS" => rss_kib = v,
                "voluntary_ctxt_switches" => voluntary = v,
                _ => {}
            }
        }
    }
    Ok(Sample {
        t: Instant::now(),
        wall: time::now(),
        utime: n(11),
        stime: n(12),
        rss_kib,
        voluntary,
    })
}

#[derive(Debug)]
struct Measurement {
    seconds: f64,
    user: f64,
    system: f64,
    wakeups: f64,
    mbps: f64,
}

fn interpolate(points: &[(f64, f64)], time: f64) -> Option<f64> {
    for pair in points.windows(2) {
        let [(a, av), (b, bv)] = pair else {
            unreachable!("two-element window")
        };
        if *a <= time && time <= *b && b > a {
            return Some(av + (bv - av) * ((time - a) / (b - a)));
        }
    }
    None
}

/// Intersects the stream-counter and process windows before calculating any rate.
fn measure(
    samples: &[Sample],
    text: &str,
    ticks: f64,
    warmup: Duration,
) -> anyhow::Result<Measurement> {
    let first = samples.first().context("no process samples")?;
    let last = samples.last().context("no process samples")?;
    anyhow::ensure!(
        samples.windows(2).all(|p| p[1].wall > p[0].wall),
        "process sample wall clock did not advance"
    );
    anyhow::ensure!(
        samples.windows(2).all(|pair| {
            let monotonic = pair[1].t.duration_since(pair[0].t).as_secs_f64();
            let realtime = pair[1].wall - pair[0].wall;
            (realtime - monotonic).abs() <= 1.0_f64.max(monotonic * 0.05)
        }),
        "realtime clock changed relative to monotonic time"
    );
    let eligible = samples
        .iter()
        .find(|s| s.t.duration_since(first.t) >= warmup)
        .context("no process samples after warm-up")?;
    let mut from = eligible.wall;
    let mut to = last.wall;
    let mut streams: BTreeMap<String, Vec<(f64, f64)>> = BTreeMap::new();
    for raw in text.lines() {
        let line = strip_ansi(raw);
        let Some(line) = split(&line) else { continue };
        if line.level != "INFO" || !line.rest.starts_with("stream ") {
            continue;
        }
        let fields = kv(line.rest);
        if let (Some(name), Some(mib), Some(ts)) = (
            fields.get("stream"),
            number(&fields, "mib"),
            time::parse_seconds(line.ts),
        ) {
            streams.entry(name.clone()).or_default().push((ts, mib));
        }
    }
    anyhow::ensure!(
        !streams.is_empty(),
        "no timestamped stream counters in the daemon log"
    );
    for series in streams.values() {
        anyhow::ensure!(
            series.len() >= 2,
            "need two stream statistics snapshots; increase --seconds or shorten --stats-interval"
        );
        anyhow::ensure!(
            series
                .windows(2)
                .all(|p| p[1].0 > p[0].0 && p[1].1 >= p[0].1),
            "stream counter reset or non-increasing timestamp in benchmark"
        );
        from = from.max(series[0].0);
        to = to.min(series[series.len() - 1].0);
    }
    anyhow::ensure!(
        to > from,
        "no shared measurement window after warm-up; increase --seconds or shorten --stats-interval"
    );
    let delta = |series: &[(f64, f64)]| -> anyhow::Result<f64> {
        Ok(interpolate(series, to).context("missing ending counter")?
            - interpolate(series, from).context("missing starting counter")?)
    };
    let process_delta = |pick: fn(&Sample) -> i64| -> anyhow::Result<f64> {
        delta(
            &samples
                .iter()
                .map(|s| (s.wall, pick(s) as f64))
                .collect::<Vec<_>>(),
        )
    };
    let seconds = to - from;
    let mib = streams
        .values()
        .map(|series| delta(series))
        .sum::<anyhow::Result<f64>>()?;
    Ok(Measurement {
        seconds,
        user: process_delta(|s| s.utime)? / ticks / seconds * 100.0,
        system: process_delta(|s| s.stime)? / ticks / seconds * 100.0,
        wakeups: process_delta(|s| s.voluntary)? / seconds,
        mbps: mib * 8.388_608 / seconds,
    })
}

const ROOT_MARKER: &str = "irisd-replay-bench-v1\n";

fn safe_root_location(root: &Path, cwd: &Path) -> bool {
    root != Path::new("/")
        && root != cwd
        && root.components().count() >= 4
        && root.file_name().is_some_and(|name| {
            !matches!(
                name.to_str(),
                Some("tmp" | "home" | "var" | "mnt" | "target")
            )
        })
        && !cwd.starts_with(root)
        && !root.starts_with(cwd)
}

fn parse_interval(value: &str) -> anyhow::Result<Duration> {
    let (number, multiplier) = if let Some(value) = value.strip_suffix("ms") {
        (value, 0.001)
    } else if let Some(value) = value.strip_suffix('s') {
        (value, 1.0)
    } else if let Some(value) = value.strip_suffix('m') {
        (value, 60.0)
    } else {
        anyhow::bail!("--stats-interval must end in ms, s, or m");
    };
    let number = number.parse::<f64>()?;
    anyhow::ensure!(
        number.is_finite() && number > 0.0,
        "invalid --stats-interval"
    );
    let seconds = number * multiplier;
    anyhow::ensure!(
        (0.1..=3_600.0).contains(&seconds),
        "--stats-interval is out of range"
    );
    Ok(Duration::from_secs_f64(seconds))
}

fn canonical_input(path: &Path, label: &str) -> anyhow::Result<PathBuf> {
    path.canonicalize()
        .with_context(|| format!("canonicalizing {label} {}", path.display()))
}

fn validate_args(args: &Args) -> anyhow::Result<()> {
    anyhow::ensure!(
        args.port != args.api_port,
        "replay and API ports must differ"
    );
    anyhow::ensure!(args.perf <= args.seconds, "--perf cannot exceed --seconds");
    let interval = parse_interval(&args.stats_interval)?;
    anyhow::ensure!(
        interval < Duration::from_secs(args.seconds.saturating_sub(20)),
        "benchmark needs at least two post-warmup statistics intervals"
    );
    anyhow::ensure!(
        args.file.as_os_str() != args.root.as_os_str(),
        "recording and root must differ"
    );
    for (path, label) in [
        (&args.file, "recording"),
        (&args.irisd, "irisd"),
        (&args.replay, "replay"),
    ] {
        anyhow::ensure!(path.to_str().is_some(), "{label} path is not valid UTF-8");
        let metadata = std::fs::metadata(path)
            .with_context(|| format!("reading {label} {}", path.display()))?;
        anyhow::ensure!(metadata.is_file(), "{label} must be a regular file");
    }
    anyhow::ensure!(args.root.to_str().is_some(), "root path is not valid UTF-8");
    Ok(())
}

struct BenchRoot {
    root: PathBuf,
    keep: bool,
    cleanup_pending: bool,
}

impl BenchRoot {
    fn prepare(args: &Args) -> anyhow::Result<Self> {
        let root = std::path::absolute(&args.root)?;
        let cwd = std::env::current_dir()?.canonicalize()?;
        let parent = root
            .parent()
            .context("benchmark root has no parent")?
            .canonicalize()
            .context("canonicalizing benchmark root parent")?;
        let resolved_root = parent.join(root.file_name().context("benchmark root has no name")?);
        anyhow::ensure!(
            safe_root_location(&root, &cwd) && safe_root_location(&resolved_root, &cwd),
            "unsafe benchmark root"
        );
        if root.exists() {
            let metadata = std::fs::symlink_metadata(&root)?;
            anyhow::ensure!(
                metadata.is_dir()
                    && !metadata.file_type().is_symlink()
                    && metadata.uid() == rustix::process::getuid().as_raw(),
                "existing benchmark root must be a caller-owned real directory"
            );
            let marker = root.join(".irisd-replay-bench");
            let marker_metadata = std::fs::symlink_metadata(&marker)
                .with_context(|| format!("reading benchmark marker in {}", root.display()))?;
            anyhow::ensure!(
                marker_metadata.is_file()
                    && !marker_metadata.file_type().is_symlink()
                    && marker_metadata.uid() == rustix::process::getuid().as_raw()
                    && marker_metadata.nlink() == 1
                    && std::fs::read_to_string(&marker)
                        .is_ok_and(|contents| contents == ROOT_MARKER),
                "refusing pre-existing unmarked benchmark root {}",
                root.display()
            );
        } else {
            std::fs::DirBuilder::new().mode(0o700).create(&root)?;
            let mut marker = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(root.join(".irisd-replay-bench"))?;
            marker.write_all(ROOT_MARKER.as_bytes())?;
            marker.sync_all()?;
        }
        let canonical_root = root.canonicalize()?;
        anyhow::ensure!(
            safe_root_location(&canonical_root, &cwd),
            "canonical benchmark root overlaps the working tree"
        );
        for (path, label) in [
            (&args.file, "recording"),
            (&args.irisd, "irisd"),
            (&args.replay, "replay"),
        ] {
            let input = canonical_input(path, label)?;
            anyhow::ensure!(
                !input.starts_with(&canonical_root),
                "{label} is inside the benchmark root"
            );
        }
        for name in ["run", "state"] {
            let path = root.join(name);
            if path.exists() {
                let metadata = std::fs::symlink_metadata(&path)?;
                anyhow::ensure!(
                    metadata.is_dir()
                        && !metadata.file_type().is_symlink()
                        && metadata.uid() == rustix::process::getuid().as_raw(),
                    "unsafe {name} entry in benchmark root"
                );
                std::fs::remove_dir_all(&path)?;
            }
            std::fs::create_dir(&path)?;
        }
        for name in [
            "bench.toml",
            "irisd.log",
            "replay.log",
            "perf.log",
            "perf.data",
        ] {
            let path = root.join(name);
            if path.exists() {
                let metadata = std::fs::symlink_metadata(&path)?;
                anyhow::ensure!(
                    metadata.is_file()
                        && !metadata.file_type().is_symlink()
                        && metadata.uid() == rustix::process::getuid().as_raw(),
                    "unsafe {name} entry in benchmark root"
                );
                std::fs::remove_file(path)?;
            }
        }
        Ok(Self {
            root,
            keep: args.keep,
            cleanup_pending: true,
        })
    }

    fn cleanup(&mut self) -> anyhow::Result<()> {
        if self.cleanup_pending && !self.keep {
            let path = self.root.join("run");
            if path.exists() {
                let metadata = std::fs::symlink_metadata(&path)?;
                anyhow::ensure!(
                    metadata.is_dir()
                        && !metadata.file_type().is_symlink()
                        && metadata.uid() == rustix::process::getuid().as_raw(),
                    "refusing unsafe recording cleanup"
                );
                std::fs::remove_dir_all(path)?;
            }
        }
        self.cleanup_pending = false;
        Ok(())
    }
}

impl Drop for BenchRoot {
    fn drop(&mut self) {
        if let Err(error) = self.cleanup() {
            eprintln!("replay-bench cleanup failed: {error:#}");
        }
    }
}

#[derive(Serialize)]
struct StorageConfig<'a> {
    video_roots: [&'a str; 1],
    state_dir: &'a str,
    min_free_percent: u8,
    quota_gib: u64,
}

#[derive(Serialize)]
struct ApiConfig<'a> {
    listen: String,
    tokens: [&'a str; 1],
}

#[derive(Serialize)]
struct IngestConfig {
    read_interval_ms: u64,
    recv_low_water_kib: u32,
}

#[derive(Serialize)]
struct CameraConfig {
    id: String,
    name: String,
    rtsp_url: String,
    username: String,
    password: String,
    record: [&'static str; 1],
    events: [String; 0],
}

#[derive(Serialize)]
struct BenchConfig<'a> {
    storage: StorageConfig<'a>,
    api: ApiConfig<'a>,
    ingest: IngestConfig,
    camera: Vec<CameraConfig>,
}

fn write_config(path: &Path, root: &Path, args: &Args) -> anyhow::Result<()> {
    let run = root.join("run");
    let state = root.join("state");
    let config = BenchConfig {
        storage: StorageConfig {
            video_roots: [run.to_str().context("run path is not UTF-8")?],
            state_dir: state.to_str().context("state path is not UTF-8")?,
            min_free_percent: 1,
            quota_gib: 20,
        },
        api: ApiConfig {
            listen: format!("127.0.0.1:{}", args.api_port),
            tokens: ["bench"],
        },
        ingest: IngestConfig {
            read_interval_ms: args.read_interval_ms,
            recv_low_water_kib: args.recv_low_water_kib,
        },
        camera: (1..=args.cameras)
            .map(|i| CameraConfig {
                id: format!("cam{i:02}"),
                name: format!("bench {i}"),
                rtsp_url: format!("rtsp://127.0.0.1:{}/main", args.port),
                username: "bench".to_owned(),
                password: "bench".to_owned(),
                record: ["main"],
                events: [],
            })
            .collect(),
    };
    std::fs::write(path, toml::to_string(&config)?)?;
    Ok(())
}

struct ManagedChild {
    name: &'static str,
    child: std::process::Child,
    reaped: bool,
}

impl ManagedChild {
    fn spawn(name: &'static str, command: &mut Command) -> anyhow::Result<Self> {
        let child = command
            .spawn()
            .with_context(|| format!("starting {name}"))?;
        Ok(Self {
            name,
            child,
            reaped: false,
        })
    }

    fn id(&self) -> u32 {
        self.child.id()
    }

    fn try_wait(&mut self) -> anyhow::Result<Option<std::process::ExitStatus>> {
        let status = self.child.try_wait()?;
        if status.is_some() {
            self.reaped = true;
        }
        Ok(status)
    }

    fn terminate(&mut self, signal: rustix::process::Signal, wait: Duration) -> anyhow::Result<()> {
        if self.reaped || self.try_wait()?.is_some() {
            return Ok(());
        }
        let pid = rustix::process::Pid::from_raw(self.child.id() as i32)
            .context("child pid is out of range")?;
        if rustix::process::kill_process_group(pid, signal).is_err() {
            let _ = rustix::process::kill_process(pid, signal);
        }
        let deadline = Instant::now() + wait;
        while Instant::now() < deadline {
            if self.try_wait()?.is_some() {
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        if rustix::process::kill_process_group(pid, rustix::process::Signal::KILL).is_err() {
            let _ = self.child.kill();
        }
        let kill_deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < kill_deadline {
            if self.try_wait()?.is_some() {
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        anyhow::bail!("{} did not exit after SIGKILL", self.name)
    }

    fn wait_success(&mut self, deadline: Duration) -> anyhow::Result<()> {
        let until = Instant::now() + deadline;
        while Instant::now() < until {
            if let Some(status) = self.try_wait()? {
                anyhow::ensure!(status.success(), "{} failed with {status}", self.name);
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        self.terminate(rustix::process::Signal::TERM, Duration::from_secs(2))?;
        anyhow::bail!("{} timed out", self.name)
    }
}

impl Drop for ManagedChild {
    fn drop(&mut self) {
        let _ = self.terminate(rustix::process::Signal::TERM, Duration::from_secs(2));
    }
}

fn cancellation_flag() -> Arc<AtomicBool> {
    let cancelled = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&cancelled);
    std::thread::spawn(move || {
        let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        else {
            return;
        };
        runtime.block_on(async move {
            let Ok(mut terminate) =
                tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            else {
                return;
            };
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {},
                _ = terminate.recv() => {},
            }
            flag.store(true, Ordering::SeqCst);
        });
    });
    cancelled
}

fn wait_replay(replay: &mut ManagedChild, port: u16, cancelled: &AtomicBool) -> anyhow::Result<()> {
    let address = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port);
    let deadline = Instant::now() + Duration::from_secs(30);
    while Instant::now() < deadline {
        anyhow::ensure!(!cancelled.load(Ordering::SeqCst), "benchmark cancelled");
        if let Some(status) = replay.try_wait()? {
            anyhow::bail!("iris-replay exited before readiness with {status}");
        }
        if TcpStream::connect_timeout(&address, Duration::from_millis(200)).is_ok() {
            anyhow::ensure!(
                replay.try_wait()?.is_none(),
                "iris-replay exited during readiness"
            );
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    anyhow::bail!("iris-replay did not start listening")
}

/// Runs the tool.
#[allow(clippy::too_many_lines)]
pub fn run(args: &Args) -> anyhow::Result<()> {
    validate_args(args)?;
    let cancelled = cancellation_flag();
    let mut owned_root = BenchRoot::prepare(args)?;
    let root = owned_root.root.clone();
    let operation = (|| -> anyhow::Result<()> {
        let config = root.join("bench.toml");
        write_config(&config, &root, args)?;
        let log = root.join("irisd.log");

        let mut replay_command = Command::new("setsid");
        replay_command
            .arg(&args.replay)
            .args([
                "--listen",
                &format!("127.0.0.1:{}", args.port),
                "--mount",
                &format!("main={}", args.file.display()),
            ])
            .args(&args.replay_arg)
            .stdout(std::fs::File::create(root.join("replay.log"))?)
            .stderr(Stdio::inherit());
        let mut replay = ManagedChild::spawn("iris-replay", &mut replay_command)?;
        wait_replay(&mut replay, args.port, &cancelled)?;

        let mut daemon_command = Command::new("setsid");
        daemon_command
            .arg(&args.irisd)
            .args([
                "run",
                "--config",
                config.to_str().context("config path is not UTF-8")?,
                "--stats-interval",
                &args.stats_interval,
            ])
            .args(&args.daemon_arg)
            .stdout(std::fs::File::create(&log)?)
            .stderr(std::fs::File::options().append(true).open(&log)?);
        let mut daemon = ManagedChild::spawn("irisd", &mut daemon_command)?;
        let daemon_pid = daemon.id();
        let mut samples = vec![proc_sample(daemon_pid)?];
        let started = Instant::now();
        let mut next_sample = started + Duration::from_secs(5);
        let mut perf: Option<ManagedChild> = None;
        let loop_result = (|| -> anyhow::Result<()> {
            while started.elapsed() < Duration::from_secs(args.seconds) {
                anyhow::ensure!(!cancelled.load(Ordering::SeqCst), "benchmark cancelled");
                if let Some(status) = replay.try_wait()? {
                    anyhow::bail!("iris-replay exited early with {status}");
                }
                if let Some(status) = daemon.try_wait()? {
                    anyhow::bail!("irisd exited early with {status}; see {}", log.display());
                }
                if Instant::now() >= next_sample {
                    samples.push(proc_sample(daemon_pid)?);
                    next_sample += Duration::from_secs(5);
                }
                if args.perf > 0 && perf.is_none() && started.elapsed() >= Duration::from_secs(20) {
                    let mut perf_command = Command::new("setsid");
                    perf_command
                        .args([
                            "sudo",
                            "-n",
                            "perf",
                            "record",
                            "-F",
                            "997",
                            "-g",
                            "-p",
                            &daemon_pid.to_string(),
                            "-o",
                            root.join("perf.data")
                                .to_str()
                                .context("perf path is not UTF-8")?,
                            "--",
                            "sleep",
                            &args.perf.to_string(),
                        ])
                        .stdout(std::fs::File::create(root.join("perf.log"))?)
                        .stderr(Stdio::inherit());
                    perf = Some(ManagedChild::spawn("perf", &mut perf_command)?);
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            if let Some(mut profiler) = perf.take() {
                profiler.wait_success(Duration::from_secs(args.perf.saturating_add(10)))?;
                let uid = rustix::process::getuid().as_raw();
                let gid = rustix::process::getgid().as_raw();
                let mut chown_command = Command::new("setsid");
                chown_command
                    .args([
                        "sudo",
                        "-n",
                        "chown",
                        &format!("{uid}:{gid}"),
                        root.join("perf.data")
                            .to_str()
                            .context("perf path is not UTF-8")?,
                    ])
                    .stdout(Stdio::null())
                    .stderr(Stdio::inherit());
                let mut chown = ManagedChild::spawn("perf chown", &mut chown_command)?;
                chown.wait_success(Duration::from_secs(10))?;
                let metadata = std::fs::symlink_metadata(root.join("perf.data"))
                    .context("requested perf artifact is missing")?;
                anyhow::ensure!(
                    metadata.is_file() && !metadata.file_type().is_symlink() && metadata.len() > 0,
                    "requested perf artifact is not a non-empty regular file"
                );
                std::fs::File::open(root.join("perf.data"))
                    .context("requested perf artifact is unreadable")?;
            }
            Ok(())
        })();
        let daemon_stop = daemon.terminate(rustix::process::Signal::INT, Duration::from_secs(30));
        let replay_stop = replay.terminate(rustix::process::Signal::TERM, Duration::from_secs(10));
        loop_result?;
        daemon_stop?;
        replay_stop?;
        anyhow::ensure!(!samples.is_empty(), "no samples taken");

        let ticks = rustix::param::clock_ticks_per_second() as f64;
        let rss = samples
            .iter()
            .map(|sample| sample.rss_kib)
            .max()
            .unwrap_or(0) as f64
            / 1024.0;
        let text = std::fs::read_to_string(&log)?;
        let Measurement {
            seconds: secs,
            user,
            system,
            wakeups,
            mbps,
        } = measure(&samples, &text, ticks, Duration::from_secs(20))?;
        anyhow::ensure!(
            mbps.is_finite() && mbps > 0.0,
            "benchmark observed no payload traffic"
        );
        let mut streams = BTreeMap::new();
        for raw in text.lines() {
            let line = strip_ansi(raw);
            if line.contains(" INFO stream ") {
                let fields = kv(&line);
                if let Some(name) = fields.get("stream").cloned() {
                    streams.insert(name, fields);
                }
            }
        }
        anyhow::ensure!(
            streams.len() == args.cameras as usize,
            "expected {} reporting streams, observed {}",
            args.cameras,
            streams.len()
        );
        let checked_sum = |key: &str| -> anyhow::Result<u64> {
            streams.values().try_fold(0u64, |total, fields| {
                let value = fields
                    .get(key)
                    .context("missing benchmark counter")?
                    .parse::<u64>()
                    .with_context(|| format!("invalid benchmark counter {key}"))?;
                total
                    .checked_add(value)
                    .context("benchmark counter overflow")
            })
        };
        let frames = checked_sum("frames")?;
        anyhow::ensure!(frames > 0, "benchmark observed zero frames");
        let result = json!({
            "label": args.label,
            "cameras": args.cameras,
            "read_interval_ms": args.read_interval_ms,
            "recv_low_water_kib": args.recv_low_water_kib,
            "window_s": (secs * 10.0).round() / 10.0,
            "payload_mbps": (mbps * 10.0).round() / 10.0,
            "cpu_user_pct": (user * 100.0).round() / 100.0,
            "cpu_sys_pct": (system * 100.0).round() / 100.0,
            "cpu_pct": ((user + system) * 100.0).round() / 100.0,
            "pct_per_mbps": ((user + system) / mbps * 10_000.0).round() / 10_000.0,
            "wakeups_per_s": wakeups.round() as i64,
            "rss_mib": (rss * 10.0).round() / 10.0,
            "frames": frames,
            "reconnects": checked_sum("reconnects")?,
            "dropped": checked_sum("dropped")?,
            "loss": checked_sum("loss")?,
            "streams_reporting": streams.len(),
        });
        println!("{result}");
        if args.perf > 0 {
            writeln!(
                std::io::stderr(),
                "perf data: {}",
                root.join("perf.data").display()
            )?;
        }
        Ok(())
    })();
    let cleanup = owned_root.cleanup();
    match (operation, cleanup) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(operation), Ok(())) => Err(operation),
        (Ok(()), Err(cleanup)) => Err(cleanup).context("cleaning benchmark recordings"),
        (Err(operation), Err(cleanup)) => {
            Err(operation).context(format!("benchmark cleanup also failed: {cleanup:#}"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(root: PathBuf, executable: PathBuf) -> Args {
        Args {
            file: executable.clone(),
            cameras: 2,
            seconds: 90,
            root,
            irisd: executable.clone(),
            replay: executable,
            port: 18_554,
            api_port: 18_600,
            read_interval_ms: 20,
            recv_low_water_kib: 32,
            stats_interval: "30s".into(),
            daemon_arg: Vec::new(),
            replay_arg: Vec::new(),
            label: String::new(),
            keep: false,
            perf: 0,
        }
    }

    fn samples() -> Vec<Sample> {
        let start = Instant::now();
        let wall = time::parse_seconds("2026-11-01T00:00:00Z").unwrap();
        (0..=20)
            .map(|i| {
                let seconds = i * 5;
                Sample {
                    t: start + Duration::from_secs(seconds),
                    wall: wall + seconds as f64,
                    utime: 1000 + seconds as i64 * 10,
                    stime: 500 + seconds as i64 * 5,
                    voluntary: 100 + seconds as i64 * 20,
                    rss_kib: 1024,
                }
            })
            .collect()
    }

    #[test]
    fn payload_and_cpu_use_deltas_over_the_same_post_warmup_window() {
        let text = concat!(
            "2026-11-01T00:00:00Z INFO stream stream=a/main mib=100\n",
            "2026-11-01T00:00:30Z INFO stream stream=a/main mib=400\n",
            "2026-11-01T00:01:00Z INFO stream stream=a/main mib=700\n",
            "2026-11-01T00:01:30Z INFO stream stream=a/main mib=1000\n",
            "2026-11-01T00:00:35Z INFO stream stream=b/main mib=900\n",
            "2026-11-01T00:01:05Z INFO stream stream=b/main mib=1200\n",
            "2026-11-01T00:01:35Z INFO stream stream=b/main mib=1500\n",
        );
        let measured = measure(&samples(), text, 100.0, Duration::from_secs(20)).unwrap();
        assert_eq!(measured.seconds, 55.0);
        assert!((measured.mbps - 167.77216).abs() < 1e-6);
        assert_eq!(measured.user, 10.0);
        assert_eq!(measured.system, 5.0);
        assert_eq!(measured.wakeups, 20.0);
    }

    #[test]
    fn a_single_statistics_snapshot_cannot_produce_a_payload_rate() {
        let text = "2026-11-01T00:00:30Z INFO stream stream=a/main mib=100\n";
        assert!(measure(&samples(), text, 100.0, Duration::from_secs(20)).is_err());
    }

    #[test]
    fn counter_resets_are_rejected_instead_of_reporting_a_negative_rate() {
        let text = concat!(
            "2026-11-01T00:00:30Z INFO stream stream=a/main mib=100\n",
            "2026-11-01T00:01:00Z INFO stream stream=a/main mib=10\n",
        );
        assert!(measure(&samples(), text, 100.0, Duration::from_secs(20)).is_err());
    }

    #[test]
    fn realtime_steps_are_rejected_even_when_time_stays_monotonic() {
        let mut stepped = samples();
        for sample in &mut stepped[10..] {
            sample.wall += 10.0;
        }
        let text = concat!(
            "2026-11-01T00:00:30Z INFO stream stream=a/main mib=100\n",
            "2026-11-01T00:01:00Z INFO stream stream=a/main mib=400\n",
            "2026-11-01T00:01:30Z INFO stream stream=a/main mib=700\n",
        );
        assert!(measure(&stepped, text, 100.0, Duration::from_secs(20)).is_err());
    }

    #[test]
    fn typed_config_round_trips_path_metacharacters() {
        let parent = tempfile::tempdir().unwrap();
        let root = parent.path().join("quote\" backslash\\ newline\nroot");
        std::fs::create_dir(&root).unwrap();
        let config_path = parent.path().join("bench.toml");
        let executable = std::env::current_exe().unwrap();
        write_config(&config_path, &root, &args(root.clone(), executable)).unwrap();

        let text = std::fs::read_to_string(config_path).unwrap();
        let parsed: toml::Value = toml::from_str(&text).unwrap();
        assert_eq!(
            parsed["storage"]["video_roots"][0].as_str(),
            root.join("run").to_str()
        );
        assert_eq!(
            parsed["storage"]["state_dir"].as_str(),
            root.join("state").to_str()
        );
    }

    #[test]
    fn an_unmarked_existing_root_is_never_modified() {
        let root = tempfile::tempdir().unwrap();
        let sentinel = root.path().join("sentinel");
        std::fs::write(&sentinel, b"preserve me").unwrap();
        let executable = std::env::current_exe().unwrap();
        let result = BenchRoot::prepare(&args(root.path().to_owned(), executable));
        assert!(result.is_err());
        assert_eq!(std::fs::read(&sentinel).unwrap(), b"preserve me");
    }

    #[test]
    fn scratch_roots_cannot_contain_or_enter_the_worktree() {
        let cwd = std::env::current_dir().unwrap().canonicalize().unwrap();
        assert!(!safe_root_location(&cwd.join("scratch-root"), &cwd));
        assert!(!safe_root_location(cwd.parent().unwrap(), &cwd));
    }

    #[cfg(unix)]
    #[test]
    fn non_utf8_root_is_rejected_before_creation() {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt as _;

        let parent = tempfile::tempdir().unwrap();
        let root = parent
            .path()
            .join(OsString::from_vec(b"non-utf8-\xff".to_vec()));
        let executable = std::env::current_exe().unwrap();
        let result = validate_args(&args(root.clone(), executable));
        assert!(result.is_err());
        assert!(!root.exists());
    }
}
