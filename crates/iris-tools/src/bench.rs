//! `replay-bench`: the daemon's cost against replayed cameras, with nothing
//! but loopback involved.

use std::collections::BTreeMap;
use std::fs::{DirBuilder, File, Metadata, OpenOptions};
use std::io::{self, Write as _};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpStream};
use std::os::unix::fs::{DirBuilderExt as _, MetadataExt as _};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::Context;
use clap::Parser;
use rustix::fs::{Mode, OFlags};
use rustix::process::{Pid, Signal};
use serde::Serialize;
use serde_json::{Value, json};
use tokio::signal::unix::SignalKind;

use crate::logs::{self, Fields, Series, Statistics};
use crate::time;

/// How long the daemon runs before its cost is measured.
const WARM_UP: Duration = Duration::from_secs(20);
/// How often the daemon's process counters are read.
const SAMPLE_INTERVAL: Duration = Duration::from_secs(5);

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
    #[arg(
        long,
        default_value_t = 120,
        value_parser = clap::value_parser!(u64).range(25..=86_400)
    )]
    seconds: u64,
    /// Fresh or marker-owned scratch root, at least three components below
    /// `/` and outside the working directory.
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
    /// The daemon's statistics interval: a number with an `ms`, `s`, or `m`
    /// suffix, from 0.1 s to 1 h.
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
    /// long into `ROOT/perf.data` (through `sudo -n`, so kernel frames are
    /// included). The file holds perf's pipe format:
    /// `perf report -i - < ROOT/perf.data`.
    #[arg(
        long,
        default_value_t = 0,
        value_name = "SECONDS",
        value_parser = clap::value_parser!(u64).range(..=86_400)
    )]
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
    // The command name may hold spaces and parentheses; fields follow the last `)`.
    let after = stat
        .get(stat.rfind(')').map_or(0, |i| i + 2)..)
        .unwrap_or_default();
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
    points.windows(2).find_map(|pair| {
        let [(a, av), (b, bv)] = pair else {
            return None;
        };
        (*a <= time && time <= *b && b > a).then(|| av + (bv - av) * ((time - a) / (b - a)))
    })
}

/// The stream statistics rows of a daemon log: each row's stream, time, and fields.
fn stream_rows(text: &str) -> impl Iterator<Item = (String, f64, Fields)> + '_ {
    text.lines().filter_map(|raw| {
        let line = logs::strip_ansi(raw);
        let split = logs::split(&line)?;
        let t = time::parse_seconds(split.timestamp)?;
        match logs::statistics(&split)? {
            Statistics::Row(Series::Stream(name), fields) => Some((name, t, fields)),
            _ => None,
        }
    })
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
    for (name, t, fields) in stream_rows(text) {
        if let Some(mib) = logs::number(&fields, "mib") {
            streams.entry(name).or_default().push((t, mib));
        }
    }
    anyhow::ensure!(
        !streams.is_empty(),
        "no timestamped stream counters in the daemon log"
    );
    for series in streams.values() {
        let (Some(first), Some(last)) = (series.first(), series.last()) else {
            continue;
        };
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
        from = from.max(first.0);
        to = to.min(last.0);
    }
    anyhow::ensure!(
        to > from,
        "no shared measurement window after warm-up; increase --seconds or shorten \
         --stats-interval"
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

/// The files a run writes at the top of the root, reset before each run.
const ROOT_FILES: [&str; 4] = ["bench.toml", "irisd.log", "replay.log", "perf.data"];

/// Whether the directory holding the benchmark root keeps other users from
/// renaming or replacing it: owned by the caller or root, and writable by no
/// one else unless the sticky bit restricts renames to an entry's owner.
fn parent_is_private(metadata: &Metadata, uid: u32) -> bool {
    let mode = metadata.mode();
    let owner_trusted = metadata.uid() == uid || metadata.uid() == 0;
    let shared_write = mode & 0o022 != 0;
    let sticky = mode & 0o1000 != 0;
    metadata.is_dir() && owner_trusted && (!shared_write || sticky)
}

/// The privileged profiler's command line. `-o -` makes perf write its pipe
/// format to standard output, which is a file the tool created itself, so the
/// privileged process never resolves a path inside the caller-writable root.
fn perf_command_args(pid: u32, seconds: u64) -> Vec<String> {
    let pid = pid.to_string();
    let seconds = seconds.to_string();
    Vec::from(
        [
            "sudo", "-n", "perf", "record", "-F", "997", "-g", "-p", &pid, "-o", "-", "--",
            "sleep", &seconds,
        ]
        .map(str::to_owned),
    )
}

/// Creates the profile file as the caller: exclusively, as a private (0600)
/// regular file, never through a symlink.
fn create_perf_output(path: &Path) -> anyhow::Result<File> {
    let fd = rustix::fs::open(
        path,
        OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::RUSR | Mode::WUSR,
    )
    .with_context(|| format!("creating {}", path.display()))?;
    Ok(File::from(fd))
}

/// Checks, through the tool's own descriptor, that perf wrote a nonempty
/// regular file and that `path` still names that file.
fn verify_perf_output(file: &File, path: &Path) -> anyhow::Result<()> {
    let written = file
        .metadata()
        .context("reading the perf output descriptor")?;
    anyhow::ensure!(
        written.is_file() && written.len() > 0,
        "requested perf artifact is empty"
    );
    let named = std::fs::symlink_metadata(path).context("requested perf artifact is missing")?;
    anyhow::ensure!(
        named.file_type().is_file() && named.dev() == written.dev() && named.ino() == written.ino(),
        "{} no longer names the perf output",
        path.display()
    );
    Ok(())
}

/// Whether `root` is a place the benchmark may own: at least three
/// components below `/`, not named `tmp`, `home`, `var`, `mnt`, or `target`,
/// and neither inside nor containing the working directory.
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
        interval < Duration::from_secs(args.seconds).saturating_sub(WARM_UP),
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

/// Whether `path` is a real entry of the kind wanted, owned by `uid`.
fn owned_entry(path: &Path, directory: bool, uid: u32) -> anyhow::Result<bool> {
    let metadata = std::fs::symlink_metadata(path)?;
    let kind = if directory {
        metadata.is_dir()
    } else {
        metadata.is_file()
    };
    Ok(kind && !metadata.file_type().is_symlink() && metadata.uid() == uid)
}

/// Admits a root that is already present only when it is the tool's own: a
/// caller-owned real directory carrying the marker, with mode 0700.
fn check_existing_root(root: &Path, uid: u32) -> anyhow::Result<()> {
    anyhow::ensure!(
        owned_entry(root, true, uid)?,
        "existing benchmark root must be a caller-owned real directory"
    );
    let marker = root.join(".irisd-replay-bench");
    let marker_metadata = std::fs::symlink_metadata(&marker)
        .with_context(|| format!("reading benchmark marker in {}", root.display()))?;
    anyhow::ensure!(
        marker_metadata.is_file()
            && !marker_metadata.file_type().is_symlink()
            && marker_metadata.uid() == uid
            && marker_metadata.nlink() == 1
            && std::fs::read_to_string(&marker).is_ok_and(|contents| contents == ROOT_MARKER),
        "refusing pre-existing unmarked benchmark root {}",
        root.display()
    );
    let mode = std::fs::symlink_metadata(root)?.mode();
    anyhow::ensure!(
        mode & 0o777 == 0o700,
        "existing benchmark root {} has mode {:o}; it must be private (chmod 700)",
        root.display(),
        mode & 0o7777
    );
    Ok(())
}

struct BenchRoot {
    root: PathBuf,
    keep: bool,
    cleanup_pending: bool,
}

impl BenchRoot {
    /// Creates the root, or admits one the tool owns, and resets the previous
    /// run's `run/` and `state/` directories and [`ROOT_FILES`] in it.
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
            "benchmark root {} must be at least three components below /, not named tmp, home, \
             var, mnt, or target, and neither inside nor containing the working directory",
            root.display()
        );
        let uid = rustix::process::geteuid().as_raw();
        anyhow::ensure!(
            parent_is_private(&std::fs::metadata(&parent)?, uid),
            "the directory holding the benchmark root, {}, must be owned by the caller or root \
             and not writable by others without the sticky bit",
            parent.display()
        );
        if root.exists() {
            check_existing_root(&root, uid)?;
        } else {
            DirBuilder::new().mode(0o700).create(&root)?;
            let mut marker = OpenOptions::new()
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
                anyhow::ensure!(
                    owned_entry(&path, true, uid)?,
                    "unsafe {name} entry in benchmark root"
                );
                std::fs::remove_dir_all(&path)?;
            }
            std::fs::create_dir(&path)?;
        }
        for name in ROOT_FILES {
            let path = root.join(name);
            if path.exists() {
                anyhow::ensure!(
                    owned_entry(&path, false, uid)?,
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

    /// Deletes `run/`, the recordings, unless `--keep` was given.
    fn cleanup(&mut self) -> anyhow::Result<()> {
        if self.cleanup_pending && !self.keep {
            let path = self.root.join("run");
            if path.exists() {
                anyhow::ensure!(
                    owned_entry(&path, true, rustix::process::geteuid().as_raw())?,
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
struct ApiConfig {
    listen: String,
    tokens: [String; 1],
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
    api: ApiConfig,
    ingest: IngestConfig,
    camera: Vec<CameraConfig>,
}

/// A fresh API bearer token for the daemon under test: 32 hexadecimal
/// characters from the kernel, the shortest the daemon accepts. The benchmark
/// never calls the API, so the token only satisfies the configuration check.
fn bench_api_token() -> anyhow::Result<String> {
    let mut bytes = [0_u8; 16];
    getrandom::fill(&mut bytes).context("drawing the benchmark API token")?;
    Ok(hex::encode(bytes))
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
            tokens: [bench_api_token()?],
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
    child: Child,
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

    fn try_wait(&mut self) -> anyhow::Result<Option<ExitStatus>> {
        let status = self.child.try_wait()?;
        if status.is_some() {
            self.reaped = true;
        }
        Ok(status)
    }

    /// Waits for the child to exit within `deadline`, polling every `period`.
    fn wait_until(&mut self, deadline: Instant, period: Duration) -> anyhow::Result<bool> {
        while Instant::now() < deadline {
            if self.try_wait()?.is_some() {
                return Ok(true);
            }
            std::thread::sleep(period);
        }
        Ok(false)
    }

    fn terminate(&mut self, signal: Signal, wait: Duration) -> anyhow::Result<()> {
        if self.reaped || self.try_wait()?.is_some() {
            return Ok(());
        }
        let pid = Pid::from_raw(self.child.id() as i32).context("child pid is out of range")?;
        if rustix::process::kill_process_group(pid, signal).is_err() {
            let _ = rustix::process::kill_process(pid, signal);
        }
        if self.wait_until(Instant::now() + wait, Duration::from_millis(50))? {
            return Ok(());
        }
        if rustix::process::kill_process_group(pid, Signal::KILL).is_err() {
            let _ = self.child.kill();
        }
        if self.wait_until(
            Instant::now() + Duration::from_secs(5),
            Duration::from_millis(20),
        )? {
            return Ok(());
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
        self.terminate(Signal::TERM, Duration::from_secs(2))?;
        anyhow::bail!("{} timed out", self.name)
    }
}

impl Drop for ManagedChild {
    fn drop(&mut self) {
        let _ = self.terminate(Signal::TERM, Duration::from_secs(2));
    }
}

/// Set once SIGINT or SIGTERM arrives; the run then stops and cleans up.
static CANCELLED: AtomicBool = AtomicBool::new(false);

fn cancelled() -> bool {
    CANCELLED.load(Ordering::SeqCst)
}

/// Registers SIGINT and SIGTERM on the calling thread, so a failure is an
/// error rather than leaving the signals' default action, then waits for
/// either on a thread of its own.
fn watch_for_cancellation() -> anyhow::Result<()> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("building the signal runtime")?;
    let (mut interrupt, mut terminate) = {
        let _context = runtime.enter();
        (
            tokio::signal::unix::signal(SignalKind::interrupt()).context("registering SIGINT")?,
            tokio::signal::unix::signal(SignalKind::terminate()).context("registering SIGTERM")?,
        )
    };
    std::thread::spawn(move || {
        runtime.block_on(async {
            tokio::select! {
                _ = interrupt.recv() => {},
                _ = terminate.recv() => {},
            }
        });
        CANCELLED.store(true, Ordering::SeqCst);
    });
    Ok(())
}

fn wait_replay(replay: &mut ManagedChild, port: u16) -> anyhow::Result<()> {
    let address = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port);
    let deadline = Instant::now() + Duration::from_secs(30);
    while Instant::now() < deadline {
        anyhow::ensure!(!cancelled(), "benchmark cancelled");
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

fn start_replay(args: &Args, root: &Path) -> anyhow::Result<ManagedChild> {
    let mut command = Command::new("setsid");
    command
        .arg(&args.replay)
        .args([
            "--listen",
            &format!("127.0.0.1:{}", args.port),
            "--mount",
            &format!("main={}", args.file.display()),
        ])
        .args(&args.replay_arg)
        .stdout(File::create(root.join("replay.log"))?)
        .stderr(Stdio::inherit());
    let mut replay = ManagedChild::spawn("iris-replay", &mut command)?;
    wait_replay(&mut replay, args.port)?;
    Ok(replay)
}

/// Starts the daemon with both of its output streams on one descriptor of
/// `log`, so neither overwrites the other.
fn start_daemon(args: &Args, config: &Path, log: &Path) -> anyhow::Result<ManagedChild> {
    let log_file = File::create(log)?;
    let mut command = Command::new("setsid");
    command
        .arg(&args.irisd)
        .args([
            "run",
            "--config",
            config.to_str().context("config path is not UTF-8")?,
            "--stats-interval",
            &args.stats_interval,
        ])
        .args(&args.daemon_arg)
        .stdout(log_file.try_clone()?)
        .stderr(log_file);
    ManagedChild::spawn("irisd", &mut command)
}

/// Samples the daemon's process counters until the run ends, starting the
/// profiler after the warm-up when `--perf` asks for it.
fn sample_until(
    args: &Args,
    replay: &mut ManagedChild,
    daemon: &mut ManagedChild,
    log: &Path,
    perf_path: &Path,
) -> anyhow::Result<Vec<Sample>> {
    let daemon_pid = daemon.id();
    let mut samples = vec![proc_sample(daemon_pid)?];
    let started = Instant::now();
    let mut next_sample = started + SAMPLE_INTERVAL;
    let mut perf: Option<(ManagedChild, File)> = None;
    while started.elapsed() < Duration::from_secs(args.seconds) {
        anyhow::ensure!(!cancelled(), "benchmark cancelled");
        if let Some(status) = replay.try_wait()? {
            anyhow::bail!("iris-replay exited early with {status}");
        }
        if let Some(status) = daemon.try_wait()? {
            anyhow::bail!("irisd exited early with {status}; see {}", log.display());
        }
        if Instant::now() >= next_sample {
            samples.push(proc_sample(daemon_pid)?);
            next_sample += SAMPLE_INTERVAL;
        }
        if args.perf > 0 && perf.is_none() && started.elapsed() >= WARM_UP {
            let output = create_perf_output(perf_path)?;
            let mut command = Command::new("setsid");
            command
                .args(perf_command_args(daemon_pid, args.perf))
                .stdout(output.try_clone()?)
                .stderr(Stdio::inherit());
            perf = Some((ManagedChild::spawn("perf", &mut command)?, output));
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    if let Some((mut profiler, output)) = perf.take() {
        profiler.wait_success(Duration::from_secs(args.perf.saturating_add(10)))?;
        verify_perf_output(&output, perf_path)?;
    }
    Ok(samples)
}

/// The result line: cost and traffic over the shared window, and the
/// counters summed over the last statistics row of every stream.
fn result_line(args: &Args, samples: &[Sample], text: &str) -> anyhow::Result<Value> {
    let ticks = rustix::param::clock_ticks_per_second() as f64;
    let rss = samples
        .iter()
        .map(|sample| sample.rss_kib)
        .max()
        .unwrap_or(0) as f64
        / 1024.0;
    let Measurement {
        seconds,
        user,
        system,
        wakeups,
        mbps,
    } = measure(samples, text, ticks, WARM_UP)?;
    anyhow::ensure!(
        mbps.is_finite() && mbps > 0.0,
        "benchmark observed no payload traffic"
    );
    let streams: BTreeMap<String, Fields> = stream_rows(text)
        .map(|(name, _, fields)| (name, fields))
        .collect();
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
    Ok(json!({
        "label": args.label,
        "cameras": args.cameras,
        "read_interval_ms": args.read_interval_ms,
        "recv_low_water_kib": args.recv_low_water_kib,
        "window_s": (seconds * 10.0).round() / 10.0,
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
    }))
}

/// Starts the replay server and the daemon in the root, samples the run, and
/// prints the result line. The children stop in order (the daemon first) on
/// every path.
fn benchmark(args: &Args, root: &Path) -> anyhow::Result<()> {
    let config = root.join("bench.toml");
    write_config(&config, root, args)?;
    let log = root.join("irisd.log");
    let perf_path = root.join("perf.data");
    let mut replay = start_replay(args, root)?;
    let mut daemon = start_daemon(args, &config, &log)?;
    let collected = sample_until(args, &mut replay, &mut daemon, &log, &perf_path);
    let daemon_stop = daemon.terminate(Signal::INT, Duration::from_secs(30));
    let replay_stop = replay.terminate(Signal::TERM, Duration::from_secs(10));
    let samples = collected?;
    daemon_stop?;
    replay_stop?;
    let text = std::fs::read_to_string(&log)?;
    println!("{}", result_line(args, &samples, &text)?);
    if args.perf > 0 {
        writeln!(
            io::stderr(),
            "perf data (pipe format; perf report -i - < FILE): {}",
            perf_path.display()
        )?;
    }
    Ok(())
}

/// Runs the benchmark in a fresh or marker-owned root and deletes its
/// recordings afterwards unless `--keep` is given; SIGINT and SIGTERM stop the
/// run and still clean up.
pub fn run(args: &Args) -> anyhow::Result<()> {
    validate_args(args)?;
    watch_for_cancellation()?;
    let mut owned_root = BenchRoot::prepare(args)?;
    let operation = benchmark(args, &owned_root.root);
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
    use std::fs::Permissions;
    use std::os::unix::fs::PermissionsExt as _;

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
        let measured = measure(&samples(), text, 100.0, WARM_UP).unwrap();
        assert_eq!(measured.seconds, 55.0);
        assert!((measured.mbps - 167.77216).abs() < 1e-6);
        assert_eq!(measured.user, 10.0);
        assert_eq!(measured.system, 5.0);
        assert_eq!(measured.wakeups, 20.0);
    }

    #[test]
    fn a_single_statistics_snapshot_cannot_produce_a_payload_rate() {
        let text = "2026-11-01T00:00:30Z INFO stream stream=a/main mib=100\n";
        assert!(measure(&samples(), text, 100.0, WARM_UP).is_err());
    }

    #[test]
    fn counter_resets_are_rejected_instead_of_reporting_a_negative_rate() {
        let text = concat!(
            "2026-11-01T00:00:30Z INFO stream stream=a/main mib=100\n",
            "2026-11-01T00:01:00Z INFO stream stream=a/main mib=10\n",
        );
        assert!(measure(&samples(), text, 100.0, WARM_UP).is_err());
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
        assert!(measure(&stepped, text, 100.0, WARM_UP).is_err());
    }

    #[test]
    fn the_benchmark_api_token_is_a_fresh_thirty_two_character_hex_string() {
        let first = bench_api_token().unwrap();
        let second = bench_api_token().unwrap();
        for token in [&first, &second] {
            assert_eq!(token.len(), 32);
            assert!(token.bytes().all(|byte| byte.is_ascii_hexdigit()));
        }
        assert_ne!(first, second);
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
        let token = parsed["api"]["tokens"][0].as_str().unwrap();
        assert_eq!(
            token.len(),
            32,
            "the daemon requires 32 visible ASCII characters"
        );
    }

    #[test]
    fn perf_writes_its_pipe_format_to_a_descriptor_without_a_privileged_chown() {
        let args = perf_command_args(4242, 30);
        assert_eq!(
            args,
            [
                "sudo", "-n", "perf", "record", "-F", "997", "-g", "-p", "4242", "-o", "-", "--",
                "sleep", "30"
            ]
        );
        assert!(!args.iter().any(|arg| arg.contains("chown")));
        assert!(!args.iter().any(|arg| arg.contains("perf.data")));
    }

    #[test]
    fn the_perf_output_is_a_new_private_caller_file() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("perf.data");
        create_perf_output(&path).unwrap();
        let metadata = std::fs::symlink_metadata(&path).unwrap();
        assert_eq!(metadata.mode() & 0o7777, 0o600);
        assert_eq!(metadata.uid(), rustix::process::geteuid().as_raw());
        assert!(create_perf_output(&path).is_err(), "never reuses a file");
    }

    #[test]
    fn a_perf_output_must_be_nonempty_and_still_named() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("perf.data");
        let mut output = create_perf_output(&path).unwrap();
        assert!(verify_perf_output(&output, &path).is_err(), "empty profile");
        output.write_all(b"PERFILE2").unwrap();
        verify_perf_output(&output, &path).unwrap();
        let replacement = directory.path().join("replacement");
        std::fs::write(&replacement, b"other").unwrap();
        std::fs::rename(&replacement, &path).unwrap();
        assert!(
            verify_perf_output(&output, &path).is_err(),
            "a renamed-over path no longer names the profile"
        );
    }

    #[test]
    fn the_perf_output_is_never_created_through_a_symlink() {
        let directory = tempfile::tempdir().unwrap();
        let victim = directory.path().join("victim");
        std::fs::write(&victim, b"unchanged").unwrap();
        for (name, target) in [
            ("linked.data", victim.clone()),
            ("dangling.data", directory.path().join("absent")),
        ] {
            let link = directory.path().join(name);
            std::os::unix::fs::symlink(&target, &link).unwrap();
            assert!(create_perf_output(&link).is_err(), "{name}");
        }
        assert_eq!(std::fs::read(&victim).unwrap(), b"unchanged");
        assert!(!directory.path().join("absent").exists());
    }

    #[test]
    fn existing_roots_must_be_private() {
        let parent = tempfile::tempdir().unwrap();
        let root = parent.path().join("bench-root");
        let executable = std::env::current_exe().unwrap();
        drop(BenchRoot::prepare(&args(root.clone(), executable.clone())).unwrap());
        assert_eq!(std::fs::metadata(&root).unwrap().mode() & 0o7777, 0o700);
        drop(BenchRoot::prepare(&args(root.clone(), executable.clone())).unwrap());
        for mode in [0o770, 0o707, 0o777, 0o750] {
            std::fs::set_permissions(&root, Permissions::from_mode(mode)).unwrap();
            let error = BenchRoot::prepare(&args(root.clone(), executable.clone()))
                .err()
                .unwrap_or_else(|| panic!("mode {mode:o} must be rejected"));
            assert!(error.to_string().contains("chmod 700"), "{error:#}");
        }
    }

    #[test]
    fn a_shared_parent_needs_the_sticky_bit() {
        let parent = tempfile::tempdir().unwrap();
        let root = parent.path().join("bench-root");
        let executable = std::env::current_exe().unwrap();
        std::fs::set_permissions(parent.path(), Permissions::from_mode(0o770)).unwrap();
        assert!(BenchRoot::prepare(&args(root.clone(), executable.clone())).is_err());
        assert!(!root.exists(), "nothing is created under a shared parent");
        std::fs::set_permissions(parent.path(), Permissions::from_mode(0o1770)).unwrap();
        drop(BenchRoot::prepare(&args(root.clone(), executable)).unwrap());
        assert!(root.is_dir());
        std::fs::set_permissions(parent.path(), Permissions::from_mode(0o700)).unwrap();
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
    fn a_reused_root_is_reset_and_its_recordings_deleted_afterwards() {
        let parent = tempfile::tempdir().unwrap();
        let root = parent.path().join("bench-root");
        let executable = std::env::current_exe().unwrap();
        let mut kept = BenchRoot::prepare(&args(root.clone(), executable.clone())).unwrap();
        std::fs::write(root.join("run").join("kept.mp4"), b"x").unwrap();
        std::fs::write(root.join("irisd.log"), b"old").unwrap();
        // As after a run with `--keep`.
        kept.keep = true;
        drop(kept);
        let mut prepared = BenchRoot::prepare(&args(root.clone(), executable)).unwrap();
        assert!(!root.join("run").join("kept.mp4").exists());
        assert!(!root.join("irisd.log").exists());
        prepared.cleanup().unwrap();
        assert!(!root.join("run").exists());
        assert!(root.join("state").is_dir());
    }

    #[test]
    fn scratch_roots_cannot_contain_or_enter_the_worktree() {
        let cwd = std::env::current_dir().unwrap().canonicalize().unwrap();
        assert!(!safe_root_location(&cwd.join("scratch-root"), &cwd));
        assert!(!safe_root_location(cwd.parent().unwrap(), &cwd));
    }

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
