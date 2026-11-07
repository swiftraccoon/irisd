//! `soak-sample`: one host-side sample beside a running service, meant for a
//! systemd user timer every minute. Everything it reads is read-only: `/proc`,
//! `/sys`, the service's cgroup, `systemctl show`, `ss`, `chronyc`, the
//! journal, and the API over loopback.

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{DirBuilderExt as _, MetadataExt as _};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::Context;
use clap::Parser;
use serde_json::{Map, Value, json};

use crate::logs::terminal_safe;
use crate::time::{iso, now};

const CGROUP_ROOT: &str = "/sys/fs/cgroup/system.slice";
const HELPER_TIMEOUT: Duration = Duration::from_secs(5);
const HELPER_OUTPUT_LIMIT: u64 = 256 * 1024;
const API_CHECK_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_CAMERAS: usize = 128;
const MAX_KINDS: usize = 8;
const MAX_STREAM_REQUESTS: usize = 1_024;
const HEALTH_LIMIT: usize = 4 * 1024;
const CAMERAS_LIMIT: usize = 256 * 1024;
const SEGMENTS_LIMIT: usize = 4 * 1024 * 1024;
const PLAYLIST_LIMIT: usize = 4 * 1024 * 1024;
const METRICS_LIMIT: usize = 512 * 1024;
const SOAK_METRICS: [&str; 7] = [
    "irisd_catalog_batch_failures_total",
    "irisd_catalog_commands_rejected_total",
    "irisd_catalog_events_rejected_total",
    "irisd_catalog_progress_rejected_total",
    "irisd_catalog_queries_rejected_total",
    "irisd_catalog_lifecycle_rejected_total",
    "irisd_catalog_reconciliation_required",
];

/// Arguments of `soak-sample`.
#[derive(Debug, Parser)]
pub struct Args {
    /// Directory for `host.jsonl`, `api.jsonl`, and `journal.log`.
    #[arg(long)]
    out: PathBuf,
    /// The systemd service name without `.service`.
    #[arg(long)]
    unit: String,
    /// Block device of the recording root, for example `sda`.
    #[arg(long)]
    disk: Option<String>,
    /// Network interface facing the cameras.
    #[arg(long)]
    iface: Option<String>,
    /// Camera subnet in CIDR form, for the TCP summary.
    #[arg(long)]
    subnet: Option<String>,
    /// API base URL, for example `http://127.0.0.1:8554`.
    #[arg(long)]
    api: Option<String>,
    /// Environment variable holding the API bearer token.
    #[arg(long, requires = "api")]
    token_env: Option<String>,
    /// Minutes between API checks; 0 disables them.
    #[arg(long, default_value_t = 0, requires = "api")]
    api_check_every: u64,
    /// Stream kinds to check per camera.
    #[arg(long, default_value = "main,sub")]
    kinds: String,
    /// Skip the journal export.
    #[arg(long)]
    no_journal: bool,
    /// Also print the sample.
    #[arg(long)]
    print: bool,
}

fn read_text(path: &Path) -> anyhow::Result<String> {
    std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))
}

/// Parses `key value` lines such as cgroup stat files.
fn read_kv(path: &Path) -> anyhow::Result<BTreeMap<String, i64>> {
    Ok(read_text(path)?
        .lines()
        .filter_map(|line| {
            let mut parts = line.split_whitespace();
            let key = parts.next()?;
            let value = parts.next()?.parse::<i64>().ok()?;
            parts.next().is_none().then(|| (key.to_owned(), value))
        })
        .collect())
}

fn read_limited(mut pipe: impl Read) -> std::io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    pipe.by_ref()
        .take(HELPER_OUTPUT_LIMIT + 1)
        .read_to_end(&mut bytes)?;
    Ok(bytes)
}

fn kill_supervised(child: &mut std::process::Child, pid: u32) -> anyhow::Result<()> {
    if let Some(group) = rustix::process::Pid::from_raw(pid as i32) {
        let _ = rustix::process::kill_process_group(group, rustix::process::Signal::KILL);
    }
    let _ = child.kill();
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        if child.try_wait()?.is_some() {
            return Ok(());
        }
        anyhow::ensure!(
            Instant::now() < deadline,
            "supervised helper could not be reaped after SIGKILL"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn run_cmd(cmd: &[&str], secret_env: Option<&str>) -> anyhow::Result<String> {
    let mut command = Command::new("setsid");
    command
        .arg(cmd[0])
        .args(&cmd[1..])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(name) = secret_env {
        command.env_remove(name);
    }
    let mut child = command
        .spawn()
        .with_context(|| format!("running {}", cmd[0]))?;
    let pid = child.id();
    let stdout = child.stdout.take().context("capturing helper stdout")?;
    let stderr = child.stderr.take().context("capturing helper stderr")?;
    let stdout_thread = std::thread::spawn(move || read_limited(stdout));
    let stderr_thread = std::thread::spawn(move || read_limited(stderr));
    let deadline = Instant::now() + HELPER_TIMEOUT;
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if Instant::now() >= deadline {
            kill_supervised(&mut child, pid)?;
            anyhow::bail!("{} timed out after 5 seconds", cmd[0]);
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    let stdout = stdout_thread
        .join()
        .map_err(|_| anyhow::anyhow!("{} stdout reader panicked", cmd[0]))??;
    let stderr = stderr_thread
        .join()
        .map_err(|_| anyhow::anyhow!("{} stderr reader panicked", cmd[0]))??;
    anyhow::ensure!(
        stdout.len() as u64 <= HELPER_OUTPUT_LIMIT && stderr.len() as u64 <= HELPER_OUTPUT_LIMIT,
        "{} output exceeded 256 KiB",
        cmd[0]
    );
    anyhow::ensure!(
        status.success(),
        "{} failed with {}: {}",
        cmd[0],
        status,
        terminal_safe(&String::from_utf8_lossy(&stderr))
            .chars()
            .take(200)
            .collect::<String>()
    );
    String::from_utf8(stdout).with_context(|| format!("{} output is not UTF-8", cmd[0]))
}

/// Runs one section, turning a failure into an `error` field.
fn section(f: impl FnOnce() -> anyhow::Result<Value>) -> Value {
    match f() {
        Ok(v) => v,
        Err(e) => {
            let text: String = format!("{e:#}").chars().take(200).collect();
            json!({ "error": text })
        }
    }
}

fn unit_props(unit: &str, secret_env: Option<&str>) -> anyhow::Result<Value> {
    let props = "ActiveState,SubState,NRestarts,MainPID,StatusText,ExecMainStartTimestamp";
    let text = run_cmd(
        &["systemctl", "show", &format!("{unit}.service"), "-p", props],
        secret_env,
    )?;
    let mut out = Map::new();
    for line in text.lines() {
        if let Some((key, value)) = line.split_once('=') {
            let v = match key {
                "NRestarts" | "MainPID" => json!(value.parse::<i64>().unwrap_or(0)),
                _ => json!(value),
            };
            out.insert(key.to_owned(), v);
        }
    }
    Ok(Value::Object(out))
}

fn block_devno(disk: &str) -> Option<String> {
    read_text(&Path::new("/sys/class/block").join(disk).join("dev"))
        .ok()
        .map(|s| s.trim().to_owned())
}

fn cgroup(unit: &str, devno: Option<&str>) -> anyhow::Result<Value> {
    let base = Path::new(CGROUP_ROOT).join(format!("{unit}.service"));
    let cpu = read_kv(&base.join("cpu.stat"))?;
    let mem = read_kv(&base.join("memory.stat"))?;
    let int_file = |name: &str| -> anyhow::Result<i64> {
        Ok(read_text(&base.join(name))?.trim().parse::<i64>()?)
    };
    let mut out = Map::new();
    out.insert("cpu_usage_us".into(), json!(cpu.get("usage_usec")));
    out.insert("cpu_user_us".into(), json!(cpu.get("user_usec")));
    out.insert("cpu_system_us".into(), json!(cpu.get("system_usec")));
    out.insert("memory_current".into(), json!(int_file("memory.current")?));
    out.insert("anon".into(), json!(mem.get("anon")));
    out.insert("file".into(), json!(mem.get("file")));
    out.insert("kernel".into(), json!(mem.get("kernel")));
    out.insert("pids".into(), json!(int_file("pids.current")?));
    if let Ok(peak) = int_file("memory.peak") {
        out.insert("memory_peak".into(), json!(peak));
    }
    if let Some(devno) = devno {
        for line in read_text(&base.join("io.stat"))?.lines() {
            let mut parts = line.split_whitespace();
            if parts.next() == Some(devno) {
                for item in parts {
                    if let Some((k, v)) = item.split_once('=') {
                        out.insert(format!("io_{k}"), json!(v.parse::<i64>().unwrap_or(0)));
                    }
                }
            }
        }
    }
    Ok(Value::Object(out))
}

fn diskstats(disk: &str) -> anyhow::Result<Value> {
    for line in read_text(Path::new("/proc/diskstats"))?.lines() {
        let f: Vec<&str> = line.split_whitespace().collect();
        if f.len() >= 14 && f[2] == disk {
            let n = |i: usize| f[i].parse::<i64>().unwrap_or(0);
            return Ok(json!({
                "reads": n(3), "sectors_read": n(5), "read_ms": n(6), "writes": n(7),
                "sectors_written": n(9), "write_ms": n(10), "in_flight": n(11), "io_ms": n(12),
                "weighted_io_ms": n(13),
            }));
        }
    }
    anyhow::bail!("{disk} not in /proc/diskstats")
}

fn psi() -> Value {
    let mut out = Map::new();
    for resource in ["cpu", "io", "memory"] {
        let path = Path::new("/proc/pressure").join(resource);
        let Ok(text) = read_text(&path) else { continue };
        for line in text.lines() {
            let mut parts = line.split_whitespace();
            let Some(kind) = parts.next() else { continue };
            let values: BTreeMap<&str, &str> = parts.filter_map(|p| p.split_once('=')).collect();
            if let Some(avg10) = values.get("avg10").and_then(|v| v.parse::<f64>().ok()) {
                out.insert(format!("{resource}_{kind}_avg10"), json!(avg10));
            }
            if let Some(total) = values.get("total").and_then(|v| v.parse::<i64>().ok()) {
                out.insert(format!("{resource}_{kind}_total_us"), json!(total));
            }
        }
    }
    Value::Object(out)
}

fn nic(iface: &str) -> anyhow::Result<Value> {
    let base = Path::new("/sys/class/net").join(iface).join("statistics");
    let keys = [
        "rx_bytes",
        "tx_bytes",
        "rx_packets",
        "tx_packets",
        "rx_errors",
        "tx_errors",
        "rx_dropped",
        "rx_missed_errors",
        "rx_fifo_errors",
        "rx_over_errors",
    ];
    let mut out = Map::new();
    for key in keys {
        if let Ok(text) = read_text(&base.join(key)) {
            out.insert(key.into(), json!(text.trim().parse::<i64>().unwrap_or(0)));
        }
    }
    anyhow::ensure!(!out.is_empty(), "no statistics for {iface}");
    Ok(Value::Object(out))
}

/// Classifies a socket line by its peer port: rtsp (554), http (80, 443), or other.
fn peer_class(line: &str) -> &'static str {
    let fields: Vec<&str> = line.split_whitespace().collect();
    if fields.len() < 5 {
        return "other";
    }
    match fields[4].rsplit(':').next() {
        Some("554") => "rtsp",
        Some("80" | "443") => "http",
        _ => "other",
    }
}

fn field_after<'a>(line: &'a str, prefix: &str) -> Option<&'a str> {
    line.split_whitespace()
        .find_map(|field| field.strip_prefix(prefix))
}

#[derive(Default)]
struct TcpClass {
    conns: i64,
    retrans_total: i64,
    lost: i64,
    rtts: Vec<f64>,
}

fn tcp(subnet: &str, secret_env: Option<&str>) -> anyhow::Result<Value> {
    let text = run_cmd(&["ss", "-tinH", "dst", subnet], secret_env)?;
    let mut classes: BTreeMap<&str, TcpClass> = ["rtsp", "http", "other"]
        .into_iter()
        .map(|k| (k, TcpClass::default()))
        .collect();
    let mut current = "other";
    let (mut outstanding, mut unacked_max) = (0i64, 0i64);
    for line in text.lines() {
        if !line.starts_with([' ', '\t']) {
            current = peer_class(line);
            if line.starts_with("ESTAB") {
                classes.get_mut(current).expect("known class").conns += 1;
            }
            continue;
        }
        let class = classes.get_mut(current).expect("known class");
        if let Some(rtt) =
            field_after(line, "rtt:").and_then(|v| v.split('/').next()?.parse::<f64>().ok())
        {
            class.rtts.push(rtt);
        }
        if let Some(r) = field_after(line, "retrans:") {
            let mut parts = r.split('/');
            outstanding += parts
                .next()
                .and_then(|v| v.parse::<i64>().ok())
                .unwrap_or(0);
            class.retrans_total += parts
                .next()
                .and_then(|v| v.parse::<i64>().ok())
                .unwrap_or(0);
        }
        if let Some(lost) = field_after(line, "lost:").and_then(|v| v.parse::<i64>().ok()) {
            class.lost += lost;
        }
        if let Some(unacked) = field_after(line, "unacked:").and_then(|v| v.parse::<i64>().ok()) {
            unacked_max = unacked_max.max(unacked);
        }
    }
    let mut out = Map::new();
    out.insert("retrans_outstanding".into(), json!(outstanding));
    out.insert("unacked_max".into(), json!(unacked_max));
    for (name, c) in classes {
        let mean = if c.rtts.is_empty() {
            0.0
        } else {
            (c.rtts.iter().sum::<f64>() / c.rtts.len() as f64 * 1000.0).round() / 1000.0
        };
        out.insert(
            name.into(),
            json!({
                "conns": c.conns, "retrans_total": c.retrans_total, "lost": c.lost,
                "rtt_max_ms": c.rtts.iter().copied().fold(0.0, f64::max), "rtt_mean_ms": mean,
            }),
        );
    }
    Ok(Value::Object(out))
}

fn chrony(secret_env: Option<&str>) -> anyhow::Result<Value> {
    let mut out = Map::new();
    for line in run_cmd(&["chronyc", "tracking"], secret_env)?.lines() {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let (key, value) = (key.trim(), value.trim());
        match key {
            "System time" => {
                let mut parts = value.split_whitespace();
                if let (Some(n), Some("seconds"), Some(dir)) = (
                    parts.next().and_then(|v| v.parse::<f64>().ok()),
                    parts.next(),
                    parts.next(),
                ) {
                    out.insert("offset_s".into(), json!(if dir == "slow" { -n } else { n }));
                }
            }
            "RMS offset" => {
                if let Some(n) = value
                    .split_whitespace()
                    .next()
                    .and_then(|v| v.parse::<f64>().ok())
                {
                    out.insert("rms_offset_s".into(), json!(n));
                }
            }
            "Leap status" => {
                out.insert("leap".into(), json!(value));
            }
            "Stratum" => {
                if let Ok(n) = value.parse::<i64>() {
                    out.insert("stratum".into(), json!(n));
                }
            }
            _ => {}
        }
    }
    Ok(Value::Object(out))
}

fn proc(pid: i64) -> anyhow::Result<Value> {
    anyhow::ensure!(pid > 0, "no main pid");
    let mut out = Map::new();
    for line in read_text(&Path::new("/proc").join(pid.to_string()).join("status"))?.lines() {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        if matches!(
            key,
            "VmRSS"
                | "VmHWM"
                | "VmSize"
                | "Threads"
                | "voluntary_ctxt_switches"
                | "nonvoluntary_ctxt_switches"
        ) {
            out.insert(
                key.into(),
                json!(
                    value
                        .split_whitespace()
                        .next()
                        .and_then(|v| v.parse::<i64>().ok())
                        .unwrap_or(0)
                ),
            );
        }
    }
    let stat = read_text(&Path::new("/proc").join(pid.to_string()).join("stat"))?;
    let after = &stat[stat.rfind(')').map_or(0, |i| i + 2)..];
    let fields: Vec<&str> = after.split_whitespace().collect();
    let n = |i: usize| {
        fields
            .get(i)
            .and_then(|v| v.parse::<i64>().ok())
            .unwrap_or(0)
    };
    out.insert("minflt".into(), json!(n(7)));
    out.insert("majflt".into(), json!(n(9)));
    out.insert("utime_ticks".into(), json!(n(11)));
    out.insert("stime_ticks".into(), json!(n(12)));
    Ok(Value::Object(out))
}

fn host() -> anyhow::Result<Value> {
    let mut out = Map::new();
    for line in read_text(Path::new("/proc/meminfo"))?.lines() {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        if matches!(key, "MemAvailable" | "Cached" | "Dirty" | "Writeback") {
            out.insert(
                format!("{}_kb", key.to_lowercase()),
                json!(
                    value
                        .split_whitespace()
                        .next()
                        .and_then(|v| v.parse::<i64>().ok())
                        .unwrap_or(0)
                ),
            );
        }
    }
    let load = read_text(Path::new("/proc/loadavg"))?;
    out.insert(
        "load1".into(),
        json!(
            load.split_whitespace()
                .next()
                .and_then(|v| v.parse::<f64>().ok())
                .unwrap_or(0.0)
        ),
    );
    let uptime = read_text(Path::new("/proc/uptime"))?;
    out.insert(
        "uptime_s".into(),
        json!(
            uptime
                .split_whitespace()
                .next()
                .and_then(|v| v.parse::<f64>().ok())
                .unwrap_or(0.0)
        ),
    );
    Ok(Value::Object(out))
}

struct OutputDir {
    fd: std::fs::File,
    _lock: std::fs::File,
}

impl OutputDir {
    /// Opens, or creates with mode 0700, the sampler's directory. A directory
    /// already there must be owned by the caller and writable by no one else;
    /// its mode is otherwise the operator's and is never changed.
    fn open(path: &Path) -> anyhow::Result<Self> {
        if !path.exists() {
            std::fs::DirBuilder::new()
                .mode(0o700)
                .create(path)
                .with_context(|| format!("creating private output directory {}", path.display()))?;
        }
        let metadata = std::fs::symlink_metadata(path)?;
        anyhow::ensure!(
            metadata.file_type().is_dir() && !metadata.file_type().is_symlink(),
            "output path must be a real directory"
        );
        anyhow::ensure!(
            metadata.uid() == rustix::process::getuid().as_raw(),
            "output directory must be owned by the current user"
        );
        let fd = rustix::fs::open(
            path,
            rustix::fs::OFlags::RDONLY
                | rustix::fs::OFlags::DIRECTORY
                | rustix::fs::OFlags::NOFOLLOW,
            rustix::fs::Mode::empty(),
        )?;
        let fd = std::fs::File::from(fd);
        let opened = fd.metadata()?;
        anyhow::ensure!(
            opened.is_dir() && opened.uid() == rustix::process::getuid().as_raw(),
            "opened output directory is not a caller-owned directory"
        );
        anyhow::ensure!(
            opened.mode() & 0o022 == 0,
            "output directory {} is writable by its group or others (mode {:o}); remove that \
             with `chmod go-w {}` or choose a private directory",
            path.display(),
            opened.mode() & 0o7777,
            path.display()
        );
        let lock = Self::open_regular_fd(&fd, ".lock", false)?;
        rustix::fs::flock(&lock, rustix::fs::FlockOperation::NonBlockingLockExclusive)
            .context("another soak-sample process holds the output lock")?;
        Ok(Self { fd, _lock: lock })
    }

    fn open_regular_fd(
        dir: &std::fs::File,
        name: &str,
        append: bool,
    ) -> anyhow::Result<std::fs::File> {
        anyhow::ensure!(
            !name.is_empty() && !name.contains('/') && name != "." && name != "..",
            "invalid sampler artifact name"
        );
        let mut flags = rustix::fs::OFlags::WRONLY
            | rustix::fs::OFlags::CREATE
            | rustix::fs::OFlags::NOFOLLOW
            | rustix::fs::OFlags::NONBLOCK;
        if append {
            flags |= rustix::fs::OFlags::APPEND;
        }
        let fd = rustix::fs::openat(
            dir,
            name,
            flags,
            rustix::fs::Mode::RUSR | rustix::fs::Mode::WUSR,
        )?;
        let file = std::fs::File::from(fd);
        let metadata = file.metadata()?;
        anyhow::ensure!(
            metadata.is_file()
                && metadata.nlink() == 1
                && metadata.uid() == rustix::process::getuid().as_raw(),
            "sampler artifact {name} must be a caller-owned single-link regular file"
        );
        Ok(file)
    }

    fn append(&self, name: &str, value: &Value) -> anyhow::Result<()> {
        let mut sink = Self::open_regular_fd(&self.fd, name, true)?;
        writeln!(sink, "{value}")?;
        sink.sync_data()?;
        Ok(())
    }
}

fn export_journal(unit: &str, out: &OutputDir, secret_env: Option<&str>) -> anyhow::Result<Value> {
    // Precreate and validate the cursor. The directory descriptor is
    // intentionally inherited so /proc/self/fd/N remains stable in journalctl.
    drop(OutputDir::open_regular_fd(
        &out.fd,
        "journal.cursor",
        false,
    )?);
    let sink = OutputDir::open_regular_fd(&out.fd, "journal.log", true)?;
    let cursor = format!("/proc/self/fd/{}/journal.cursor", out.fd.as_raw_fd());
    let mut command = Command::new("setsid");
    command
        .arg("journalctl")
        .args([
            "-u",
            &format!("{unit}.service"),
            "-o",
            "cat",
            "--no-pager",
            &format!("--cursor-file={cursor}"),
        ])
        .stdout(sink.try_clone()?)
        .stderr(Stdio::piped());
    if let Some(name) = secret_env {
        command.env_remove(name);
    }
    let mut child = command.spawn().context("running journalctl")?;
    let pid = child.id();
    let stderr = child.stderr.take().context("capturing journalctl stderr")?;
    let stderr_thread = std::thread::spawn(move || read_limited(stderr));
    let deadline = Instant::now() + HELPER_TIMEOUT;
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if Instant::now() >= deadline {
            kill_supervised(&mut child, pid)?;
            anyhow::bail!("journalctl timed out after 5 seconds");
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    let stderr = stderr_thread
        .join()
        .map_err(|_| anyhow::anyhow!("journalctl stderr reader panicked"))??;
    anyhow::ensure!(
        stderr.len() as u64 <= HELPER_OUTPUT_LIMIT,
        "journalctl stderr exceeded 256 KiB"
    );
    anyhow::ensure!(
        status.success(),
        "journalctl failed with {status}: {}",
        terminal_safe(&String::from_utf8_lossy(&stderr))
            .chars()
            .take(200)
            .collect::<String>()
    );
    sink.sync_data()?;
    Ok(json!({
        "rc": status.code(),
        "bytes": sink.metadata()?.len(),
        "stderr": String::from_utf8_lossy(&stderr).chars().take(120).collect::<String>(),
    }))
}

fn hls_attributes(text: &str) -> anyhow::Result<BTreeMap<&str, &str>> {
    let mut attributes = BTreeMap::new();
    let mut start = 0usize;
    let mut quoted = false;
    for (index, character) in text.char_indices() {
        match character {
            '"' => quoted = !quoted,
            ',' if !quoted => {
                let item = &text[start..index];
                let (key, value) = item.split_once('=').context("invalid HLS attribute")?;
                anyhow::ensure!(
                    !key.is_empty() && !value.is_empty() && attributes.insert(key, value).is_none(),
                    "invalid or duplicate HLS attribute"
                );
                start = index + character.len_utf8();
            }
            _ => {}
        }
    }
    anyhow::ensure!(!quoted, "unterminated HLS quoted string");
    let item = &text[start..];
    let (key, value) = item.split_once('=').context("invalid HLS attribute")?;
    anyhow::ensure!(
        !key.is_empty() && !value.is_empty() && attributes.insert(key, value).is_none(),
        "invalid or duplicate HLS attribute"
    );
    Ok(attributes)
}

fn validate_map(attrs: &str) -> anyhow::Result<()> {
    let attributes = hls_attributes(attrs)?;
    let quoted = |name: &str| -> anyhow::Result<&str> {
        let value = attributes
            .get(name)
            .with_context(|| format!("map requires {name}"))?;
        value
            .strip_prefix('"')
            .and_then(|value| value.strip_suffix('"'))
            .filter(|value| !value.is_empty())
            .with_context(|| format!("map {name} must be a nonempty quoted value"))
    };
    let _uri = quoted("URI")?;
    let range = quoted("BYTERANGE")?;
    let (length, offset) = range
        .split_once('@')
        .context("map byte range requires an explicit offset")?;
    let length = length
        .parse::<u64>()
        .ok()
        .filter(|length| *length > 0)
        .context("invalid map byte-range length")?;
    let offset = offset
        .parse::<u64>()
        .context("invalid map byte-range offset")?;
    offset
        .checked_add(length)
        .context("map byte range overflow")?;
    Ok(())
}

/// Validates a byte-range VOD playlist and returns its bounded summary.
pub fn playlist_summary(text: &str) -> anyhow::Result<Value> {
    anyhow::ensure!(text.len() <= PLAYLIST_LIMIT, "playlist exceeds 4 MiB");
    let mut lines = text.lines();
    anyhow::ensure!(
        lines.next() == Some("#EXTM3U"),
        "playlist is missing #EXTM3U"
    );
    let (mut media, mut discontinuities, mut gaps, mut breaks, mut maps) =
        (0u64, 0u64, 0u64, 0u64, 0u64);
    let mut duration = 0.0;
    let mut next_offset: BTreeMap<String, u64> = BTreeMap::new();
    let mut pending_duration: Option<f64> = None;
    let mut pending_range: Option<(u64, Option<u64>)> = None;
    for (line_no, line) in lines.enumerate() {
        if let Some(rest) = line.strip_prefix("#EXTINF:") {
            anyhow::ensure!(
                pending_duration.is_none() && pending_range.is_none(),
                "line {}: dangling media state before EXTINF",
                line_no + 2
            );
            let value = rest
                .split(',')
                .next()
                .and_then(|v| v.parse::<f64>().ok())
                .filter(|v| v.is_finite() && *v > 0.0)
                .with_context(|| format!("line {}: invalid EXTINF", line_no + 2))?;
            pending_duration = Some(value);
        } else if let Some(rest) = line.strip_prefix("#EXT-X-BYTERANGE:") {
            anyhow::ensure!(
                pending_duration.is_some() && pending_range.is_none(),
                "line {}: byte range requires one pending EXTINF",
                line_no + 2
            );
            let (length, offset) = rest
                .split_once('@')
                .map_or((rest, None), |(l, o)| (l, Some(o)));
            let length = length
                .parse::<u64>()
                .ok()
                .filter(|length| *length > 0)
                .with_context(|| format!("line {}: invalid byte-range length", line_no + 2))?;
            let offset = offset
                .map(str::parse::<u64>)
                .transpose()
                .with_context(|| format!("line {}: invalid byte-range offset", line_no + 2))?;
            pending_range = Some((length, offset));
        } else if line == "#EXT-X-DISCONTINUITY" {
            discontinuities = discontinuities
                .checked_add(1)
                .context("too many discontinuities")?;
        } else if line == "#EXT-X-GAP" {
            gaps = gaps.checked_add(1).context("too many gaps")?;
        } else if let Some(attrs) = line.strip_prefix("#EXT-X-MAP:") {
            anyhow::ensure!(
                pending_duration.is_none() && pending_range.is_none(),
                "line {}: map interrupts pending media",
                line_no + 2
            );
            validate_map(attrs).with_context(|| format!("line {}: invalid map", line_no + 2))?;
            maps = maps.checked_add(1).context("too many maps")?;
        } else if !line.is_empty() && !line.starts_with('#') {
            anyhow::ensure!(maps > 0, "line {}: media precedes its map", line_no + 2);
            let segment_duration = pending_duration
                .take()
                .with_context(|| format!("line {}: media URI without EXTINF", line_no + 2))?;
            let (length, offset) = pending_range
                .take()
                .with_context(|| format!("line {}: media URI without byte range", line_no + 2))?;
            let offset = match offset {
                None => next_offset.get(line).copied().with_context(|| {
                    format!("line {}: first range needs an explicit offset", line_no + 2)
                })?,
                Some(o) => {
                    if next_offset.get(line).is_some_and(|&n| n != o) {
                        breaks = breaks.checked_add(1).context("too many range breaks")?;
                    }
                    o
                }
            };
            let end = offset.checked_add(length).context("byte range overflow")?;
            next_offset.insert(line.to_owned(), end);
            duration += segment_duration;
            anyhow::ensure!(duration.is_finite(), "playlist duration overflow");
            media = media.checked_add(1).context("too many media segments")?;
        }
    }
    anyhow::ensure!(
        pending_duration.is_none(),
        "playlist ends with dangling EXTINF"
    );
    anyhow::ensure!(
        pending_range.is_none(),
        "playlist ends with dangling byte range"
    );
    anyhow::ensure!(maps > 0, "playlist has no initialization map");
    anyhow::ensure!(media > 0, "playlist has no media segments");
    Ok(json!({
        "media_segments": media, "duration_s": (duration * 1000.0).round() / 1000.0,
        "discontinuities": discontinuities, "gaps": gaps, "range_breaks": breaks,
        "files": next_offset.len(), "maps": maps,
    }))
}

async fn api_get(
    client: &reqwest::Client,
    base: &reqwest::Url,
    path: &str,
    token: Option<&str>,
    limit: usize,
) -> anyhow::Result<(u16, Vec<u8>)> {
    let url = base.join(path)?;
    anyhow::ensure!(
        url.origin() == base.origin(),
        "API request escaped the loopback origin"
    );
    let mut request = client.get(url);
    if let Some(token) = token {
        request = request.bearer_auth(token);
    }
    let mut response = request.send().await?;
    let status = response.status().as_u16();
    if let Some(length) = response.content_length() {
        anyhow::ensure!(length <= limit as u64, "API response exceeds {limit} bytes");
    }
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        anyhow::ensure!(
            body.len()
                .checked_add(chunk.len())
                .is_some_and(|n| n <= limit),
            "API response exceeds {limit} bytes"
        );
        body.extend_from_slice(&chunk);
    }
    Ok((status, body))
}

fn validate_api_base(text: &str) -> anyhow::Result<reqwest::Url> {
    let mut url = reqwest::Url::parse(text).context("parsing --api")?;
    anyhow::ensure!(
        matches!(url.scheme(), "http" | "https"),
        "API scheme must be http or https"
    );
    anyhow::ensure!(
        url.username().is_empty() && url.password().is_none(),
        "API URL cannot contain userinfo"
    );
    anyhow::ensure!(
        url.query().is_none() && url.fragment().is_none(),
        "API URL cannot contain a query or fragment"
    );
    anyhow::ensure!(
        matches!(url.path(), "" | "/"),
        "API URL cannot contain a base path"
    );
    let loopback = url
        .host_str()
        .and_then(|host| {
            host.trim_matches(['[', ']'])
                .parse::<std::net::IpAddr>()
                .ok()
        })
        .is_some_and(|ip| ip.is_loopback());
    anyhow::ensure!(loopback, "API host must be a literal loopback address");
    url.set_path("/");
    Ok(url)
}

fn metrics_summary(text: &str) -> anyhow::Result<Value> {
    let mut values = Map::new();
    for line in text
        .lines()
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
    {
        let mut fields = line.split_whitespace();
        let Some(name) = fields.next() else { continue };
        let Some(raw) = fields.next() else { continue };
        anyhow::ensure!(fields.next().is_none(), "invalid Prometheus sample");
        if !SOAK_METRICS.contains(&name) {
            continue;
        }
        let value = raw.parse::<f64>().context("invalid Prometheus value")?;
        anyhow::ensure!(
            value.is_finite() && value >= 0.0,
            "invalid Prometheus value"
        );
        anyhow::ensure!(
            values.insert(name.to_owned(), json!(value)).is_none(),
            "duplicate Prometheus sample {name}"
        );
    }
    for name in SOAK_METRICS {
        anyhow::ensure!(
            values.contains_key(name),
            "metrics response is missing {name}"
        );
    }
    Ok(Value::Object(values))
}

fn parse_kinds(text: &str) -> anyhow::Result<Vec<String>> {
    let kinds = text
        .split(',')
        .map(str::trim)
        .filter(|kind| !kind.is_empty())
        .map(str::to_owned)
        .collect::<std::collections::BTreeSet<_>>();
    anyhow::ensure!(
        !kinds.is_empty() && kinds.len() <= MAX_KINDS,
        "--kinds must contain 1..={MAX_KINDS} unique values"
    );
    anyhow::ensure!(
        kinds.iter().all(|kind| {
            kind.len() <= 32
                && kind
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'))
        }),
        "invalid stream kind"
    );
    Ok(kinds.into_iter().collect())
}

#[allow(clippy::too_many_lines)]
async fn api_check(
    base: &reqwest::Url,
    token: Option<&str>,
    kinds: &[String],
    now_s: f64,
) -> anyhow::Result<Value> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .redirect(reqwest::redirect::Policy::none())
        .build()?;
    let mut out = Map::new();
    let (status, body) = api_get(&client, base, "/api/health", token, HEALTH_LIMIT).await?;
    let health: Value = serde_json::from_slice(&body).context("health response is not JSON")?;
    anyhow::ensure!(
        health.get("status").and_then(Value::as_str) == Some("ok")
            && health
                .get("uptime_s")
                .and_then(Value::as_i64)
                .is_some_and(|uptime| uptime >= 0)
            && health
                .get("version")
                .and_then(Value::as_str)
                .is_some_and(|version| !version.is_empty()),
        "health response has an invalid schema"
    );
    out.insert("health".into(), json!({ "status": status, "body": health }));
    anyhow::ensure!(status == 200, "health endpoint returned HTTP {status}");
    // `/metrics` requires the root bearer unless the daemon exposes it publicly.
    let (status, body) = api_get(&client, base, "/metrics", token, METRICS_LIMIT).await?;
    anyhow::ensure!(status == 200, "metrics endpoint returned HTTP {status}");
    let metrics = metrics_summary(std::str::from_utf8(&body).context("metrics are not UTF-8")?)?;
    out.insert(
        "metrics".into(),
        json!({ "status": status, "values": metrics }),
    );
    let (status, body) = api_get(&client, base, "/api/cameras", token, CAMERAS_LIMIT).await?;
    anyhow::ensure!(status == 200, "camera discovery returned HTTP {status}");
    let cameras =
        serde_json::from_slice::<Vec<Value>>(&body).context("camera discovery is not an array")?;
    anyhow::ensure!(
        cameras.len() <= MAX_CAMERAS,
        "camera discovery exceeds {MAX_CAMERAS} rows"
    );
    let mut ids = std::collections::BTreeSet::new();
    for camera in &cameras {
        let id = camera
            .get("id")
            .and_then(Value::as_str)
            .context("camera row is missing a string id")?;
        anyhow::ensure!(
            !id.is_empty()
                && id.len() <= 64
                && id
                    .bytes()
                    .all(|b| { b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.') }),
            "invalid camera id"
        );
        anyhow::ensure!(ids.insert(id.to_owned()), "duplicate camera id {id}");
    }
    anyhow::ensure!(
        ids.len()
            .checked_mul(kinds.len())
            .is_some_and(|n| n <= MAX_STREAM_REQUESTS),
        "camera/kind product exceeds {MAX_STREAM_REQUESTS}"
    );
    let expected_streams = ids
        .len()
        .checked_mul(kinds.len())
        .context("camera/kind product overflow")?;
    out.insert(
        "cameras".into(),
        json!({ "status": status, "count": ids.len(), "expected_streams": expected_streams }),
    );
    let segments_from = iso(now_s - 15.0 * 60.0);
    let playlist_from = iso(now_s - 10.0 * 60.0);
    let to = iso(now_s);
    let mut streams = Map::new();
    for camera in &ids {
        for kind in kinds {
            let mut entry = Map::new();
            let path =
                format!("/api/streams/{camera}/{kind}/segments?from={segments_from}&to={to}");
            let (status, body) = api_get(&client, base, &path, token, SEGMENTS_LIMIT).await?;
            entry.insert("segments_status".into(), json!(status));
            if status == 200 {
                let rows: Vec<Value> = serde_json::from_slice(&body)?;
                anyhow::ensure!(rows.len() <= 100_000, "segment response has too many rows");
                let runs: std::collections::BTreeSet<i64> = rows
                    .iter()
                    .filter_map(|r| r.get("run_seq")?.as_i64())
                    .collect();
                entry.insert("segments".into(), json!(rows.len()));
                entry.insert(
                    "sealed".into(),
                    json!(
                        rows.iter()
                            .filter(|r| r.get("sealed").and_then(Value::as_bool) == Some(true))
                            .count()
                    ),
                );
                let checked_sum = |key: &str| -> anyhow::Result<u64> {
                    rows.iter().try_fold(0u64, |total, row| {
                        let value = row
                            .get(key)
                            .and_then(Value::as_u64)
                            .with_context(|| format!("segment row has invalid {key}"))?;
                        total.checked_add(value).context("segment total overflow")
                    })
                };
                entry.insert("bytes".into(), json!(checked_sum("bytes")?));
                entry.insert("fragments".into(), json!(checked_sum("fragments")?));
                entry.insert("runs".into(), json!(runs.len()));
                entry.insert(
                    "discontinuities".into(),
                    json!(
                        rows.iter()
                            .filter(
                                |r| r.get("discontinuity").and_then(Value::as_bool) == Some(true)
                            )
                            .count()
                    ),
                );
            }
            let path =
                format!("/api/streams/{camera}/{kind}/hls/vod.m3u8?from={playlist_from}&to={to}");
            let (status, body) = api_get(&client, base, &path, token, PLAYLIST_LIMIT).await?;
            entry.insert("playlist_status".into(), json!(status));
            if status == 200 {
                entry.insert(
                    "playlist".into(),
                    playlist_summary(std::str::from_utf8(&body).context("playlist is not UTF-8")?)?,
                );
            }
            streams.insert(format!("{camera}/{kind}"), Value::Object(entry));
        }
    }
    out.insert("streams".into(), Value::Object(streams));
    Ok(Value::Object(out))
}

fn api_check_blocking(
    base: &reqwest::Url,
    token: Option<&str>,
    kinds: &[String],
    now_s: f64,
) -> anyhow::Result<Value> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    runtime
        .block_on(async {
            tokio::time::timeout(API_CHECK_TIMEOUT, api_check(base, token, kinds, now_s)).await
        })
        .context("API check exceeded its 30-second budget")?
}

/// Runs the tool.
pub fn run(args: &Args) -> anyhow::Result<()> {
    anyhow::ensure!(
        args.api.is_some() || (args.api_check_every == 0 && args.token_env.is_none()),
        "--api-check-every and --token-env require --api"
    );
    let api_base = args.api.as_deref().map(validate_api_base).transpose()?;
    let kinds = parse_kinds(&args.kinds)?;
    let out = OutputDir::open(&args.out)?;
    let now_s = now();
    let secret_env = args.token_env.as_deref();
    let unit = section(|| unit_props(&args.unit, secret_env));
    let pid = unit.get("MainPID").and_then(Value::as_i64).unwrap_or(0);
    let devno = args.disk.as_deref().and_then(block_devno);
    let mut sample = Map::new();
    sample.insert("ts".into(), json!(iso(now_s)));
    sample.insert("unit".into(), unit);
    sample.insert(
        "cgroup".into(),
        section(|| cgroup(&args.unit, devno.as_deref())),
    );
    sample.insert("proc".into(), section(|| proc(pid)));
    sample.insert("host".into(), section(host));
    sample.insert("psi".into(), psi());
    sample.insert("chrony".into(), section(|| chrony(secret_env)));
    if let Some(disk) = &args.disk {
        sample.insert("disk".into(), section(|| diskstats(disk)));
    }
    if let Some(iface) = &args.iface {
        sample.insert("nic".into(), section(|| nic(iface)));
    }
    if let Some(subnet) = &args.subnet {
        sample.insert("tcp".into(), section(|| tcp(subnet, secret_env)));
    }
    if !args.no_journal {
        sample.insert(
            "journal".into(),
            section(|| export_journal(&args.unit, &out, secret_env)),
        );
    }
    let sample = Value::Object(sample);
    out.append("host.jsonl", &sample)?;
    if args.print {
        println!("{}", serde_json::to_string_pretty(&sample)?);
    }
    let due = args.api_check_every > 0 && (now_s as u64 / 60).is_multiple_of(args.api_check_every);
    if let (Some(base), true) = (&api_base, due) {
        let mut check = Map::new();
        check.insert("ts".into(), json!(iso(now_s)));
        let result = section(|| {
            let token = args
                .token_env
                .as_ref()
                .map(|name| {
                    std::env::var(name).with_context(|| {
                        format!("API token environment variable {name} is unavailable")
                    })
                })
                .transpose()?;
            api_check_blocking(base, token.as_deref(), &kinds, now_s)
        });
        match result {
            Value::Object(fields) => check.extend(fields),
            other => {
                check.insert(
                    "error".into(),
                    other.get("error").cloned().unwrap_or(Value::Null),
                );
            }
        }
        let check = Value::Object(check);
        out.append("api.jsonl", &check)?;
        if args.print {
            println!("{}", serde_json::to_string_pretty(&check)?);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fmt::Write as _;
    use std::os::unix::fs::PermissionsExt as _;
    use std::os::unix::fs::symlink;

    #[test]
    fn tcp_fields_match_token_boundaries() {
        let line = "bytes_retrans:12610331 retrans:0/7 rtt:1.2/0.4";
        assert_eq!(field_after(line, "retrans:"), Some("0/7"));
        assert_eq!(field_after(line, "rtt:"), Some("1.2/0.4"));
        assert_eq!(field_after(line, "lost:"), None);
    }

    #[test]
    fn metrics_summary_selects_required_bounded_counters() {
        let mut text = String::from("# HELP ignored ignored\nirisd_requests_total 9\n");
        for (index, name) in SOAK_METRICS.into_iter().enumerate() {
            writeln!(&mut text, "{name} {index}").unwrap();
        }
        let summary = metrics_summary(&text).unwrap();
        assert_eq!(summary.as_object().unwrap().len(), SOAK_METRICS.len());
        assert_eq!(summary["irisd_catalog_batch_failures_total"], 0.0);

        assert!(metrics_summary("irisd_catalog_batch_failures_total 0\n").is_err());
    }

    #[test]
    fn playlist_summary_counts_ranges_and_breaks() {
        let text = "#EXTM3U\n#EXT-X-MAP:URI=\"a\",BYTERANGE=\"10@0\"\n#EXTINF:2.0,\n#EXT-X-BYTERANGE:100@10\na\n#EXTINF:2.0,\n#EXT-X-BYTERANGE:50\na\n#EXT-X-DISCONTINUITY\n#EXTINF:1.5,\n#EXT-X-BYTERANGE:20@500\na\n";
        let s = playlist_summary(text).unwrap();
        assert_eq!(s["media_segments"], 3);
        assert_eq!(s["duration_s"], 5.5);
        assert_eq!(s["discontinuities"], 1);
        assert_eq!(s["range_breaks"], 1);
        assert_eq!(s["maps"], 1);
        assert_eq!(s["files"], 1);
    }

    #[test]
    fn playlist_summary_rejects_dangling_invalid_and_overflowing_ranges() {
        for text in [
            "#EXTINF:1,\n#EXT-X-BYTERANGE:1@0\na\n",
            "#EXTM3U\n#EXT-X-MAP:URI=\"i\",BYTERANGE=\"1@0\"\n#EXTINF:1,\n",
            "#EXTM3U\n#EXT-X-MAP:URI=\"i\",BYTERANGE=\"1@0\"\n#EXTINF:1,\na\n",
            "#EXTM3U\n#EXT-X-MAP:URI=\"i\",BYTERANGE=\"1@0\"\n#EXTINF:1,\n#EXT-X-BYTERANGE:0@0\na\n",
            "#EXTM3U\n#EXT-X-MAP:URI=\"i\",BYTERANGE=\"1@0\"\n#EXTINF:1,\n#EXT-X-BYTERANGE:2@18446744073709551615\na\n",
            "#EXTM3U\n#EXT-X-MAP:URI=\"i\",BYTERANGE=\"bad\"\n#EXTINF:1,\n#EXT-X-BYTERANGE:1@0\na\n",
            "#EXTM3U\n#EXT-X-MAP:URI=\"i\",BYTERANGE=\"2@18446744073709551615\"\n#EXTINF:1,\n#EXT-X-BYTERANGE:1@0\na\n",
            "#EXTM3U\n#EXT-X-BYTERANGE:1@0\n#EXTINF:1,\na\n",
            "#EXTM3U\n#EXTINF:1,\n#EXT-X-BYTERANGE:1@0\na\n#EXT-X-MAP:URI=\"i\",BYTERANGE=\"1@0\"\n",
        ] {
            assert!(playlist_summary(text).is_err(), "{text:?}");
        }
    }

    #[test]
    fn api_base_is_literal_loopback_and_structurally_empty() {
        for valid in ["http://127.0.0.1:8554", "https://[::1]:8554/"] {
            assert!(validate_api_base(valid).is_ok(), "{valid}");
        }
        for invalid in [
            "http://localhost:8554",
            "http://192.0.2.1:8554",
            "http://user@127.0.0.1:8554",
            "http://127.0.0.1:8554/api",
            "http://127.0.0.1:8554/?x=1",
            "http://127.0.0.1:8554/#x",
        ] {
            assert!(validate_api_base(invalid).is_err(), "{invalid}");
        }
    }

    #[test]
    fn api_options_require_an_api_only_when_explicitly_requested() {
        assert!(
            Args::try_parse_from([
                "soak-sample",
                "--out",
                "/tmp/unused",
                "--unit",
                "irisd",
                "--no-journal",
            ])
            .is_ok()
        );
        assert!(
            Args::try_parse_from([
                "soak-sample",
                "--out",
                "/tmp/unused",
                "--unit",
                "irisd",
                "--api-check-every",
                "1",
            ])
            .is_err()
        );
        assert!(
            Args::try_parse_from([
                "soak-sample",
                "--out",
                "/tmp/unused",
                "--unit",
                "irisd",
                "--token-env",
                "TOKEN",
            ])
            .is_err()
        );
    }

    #[test]
    fn output_artifacts_do_not_follow_symlinks() {
        let parent = tempfile::tempdir().unwrap();
        let out_path = parent.path().join("out");
        std::fs::create_dir(&out_path).unwrap();
        let sentinel = parent.path().join("sentinel");
        std::fs::write(&sentinel, b"unchanged").unwrap();
        symlink(&sentinel, out_path.join("host.jsonl")).unwrap();

        let out = OutputDir::open(&out_path).unwrap();
        assert!(out.append("host.jsonl", &json!({ "ts": "now" })).is_err());
        assert_eq!(std::fs::read(&sentinel).unwrap(), b"unchanged");
    }

    #[test]
    fn newly_created_output_directory_is_private() {
        let parent = tempfile::tempdir().unwrap();
        let out_path = parent.path().join("out");
        let _out = OutputDir::open(&out_path).unwrap();
        assert_eq!(std::fs::metadata(&out_path).unwrap().mode() & 0o777, 0o700);
    }

    #[test]
    fn an_existing_output_directory_keeps_its_mode_unless_others_can_write() {
        for mode in [0o755, 0o750, 0o700] {
            let parent = tempfile::tempdir().unwrap();
            let out_path = parent.path().join("out");
            std::fs::create_dir(&out_path).unwrap();
            std::fs::set_permissions(&out_path, std::fs::Permissions::from_mode(mode)).unwrap();
            drop(OutputDir::open(&out_path).unwrap());
            assert_eq!(
                std::fs::metadata(&out_path).unwrap().mode() & 0o777,
                mode,
                "mode {mode:o} is left alone"
            );
        }
        for mode in [0o775, 0o777, 0o757] {
            let parent = tempfile::tempdir().unwrap();
            let out_path = parent.path().join("out");
            std::fs::create_dir(&out_path).unwrap();
            std::fs::set_permissions(&out_path, std::fs::Permissions::from_mode(mode)).unwrap();
            let error = OutputDir::open(&out_path)
                .err()
                .unwrap_or_else(|| panic!("mode {mode:o} must be rejected"));
            assert!(error.to_string().contains("chmod go-w"), "{error:#}");
            assert_eq!(std::fs::metadata(&out_path).unwrap().mode() & 0o777, mode);
            assert!(!out_path.join(".lock").exists(), "nothing is written");
        }
    }

    #[test]
    fn fifo_artifacts_are_rejected_without_blocking() {
        let parent = tempfile::tempdir().unwrap();
        let out_path = parent.path().join("out");
        std::fs::create_dir(&out_path).unwrap();
        let out = OutputDir::open(&out_path).unwrap();
        rustix::fs::mkfifoat(
            &out.fd,
            "api.jsonl",
            rustix::fs::Mode::RUSR | rustix::fs::Mode::WUSR,
        )
        .unwrap();

        let started = Instant::now();
        assert!(out.append("api.jsonl", &json!({ "ts": "now" })).is_err());
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn peer_classes_follow_the_port() {
        assert_eq!(peer_class("ESTAB 0 0 192.0.2.1:5000 192.0.2.9:554"), "rtsp");
        assert_eq!(peer_class("ESTAB 0 0 192.0.2.1:5000 192.0.2.9:80"), "http");
        assert_eq!(
            peer_class("ESTAB 0 0 192.0.2.1:5000 192.0.2.9:8554"),
            "other"
        );
    }

    /// A loopback HTTP fixture for the API check: answers `/api/health`, a
    /// `/metrics` that requires `Bearer t`, and an empty camera list, one
    /// connection per request, and records each request's Authorization value.
    fn api_fixture() -> (
        String,
        std::sync::Arc<std::sync::Mutex<Vec<(String, String)>>>,
    ) {
        use std::io::BufRead as _;

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let record = std::sync::Arc::clone(&seen);
        std::thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(10);
            while Instant::now() < deadline {
                let Ok((socket, _)) = listener.accept() else {
                    std::thread::sleep(Duration::from_millis(5));
                    continue;
                };
                socket.set_nonblocking(false).unwrap();
                socket
                    .set_read_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                let mut reader = std::io::BufReader::new(&socket);
                let mut request_line = String::new();
                if reader.read_line(&mut request_line).is_err() {
                    continue;
                }
                let path = request_line
                    .split_whitespace()
                    .nth(1)
                    .unwrap_or_default()
                    .to_owned();
                let mut authorization = String::new();
                loop {
                    let mut header = String::new();
                    if reader.read_line(&mut header).unwrap_or(0) == 0 || header == "\r\n" {
                        break;
                    }
                    if let Some((name, value)) = header.split_once(':')
                        && name.eq_ignore_ascii_case("authorization")
                    {
                        value.trim().clone_into(&mut authorization);
                    }
                }
                record
                    .lock()
                    .unwrap()
                    .push((path.clone(), authorization.clone()));
                let (status, body) = match path.as_str() {
                    "/api/health" => (
                        "200 OK",
                        r#"{"status":"ok","uptime_s":1,"version":"0.1.0"}"#.to_owned(),
                    ),
                    "/metrics" if authorization == "Bearer t" => {
                        let mut metrics = String::new();
                        for name in SOAK_METRICS {
                            writeln!(metrics, "{name} 0").unwrap();
                        }
                        ("200 OK", metrics)
                    }
                    "/metrics" => ("401 Unauthorized", String::new()),
                    "/api/cameras" => ("200 OK", "[]".to_owned()),
                    _ => ("404 Not Found", String::new()),
                };
                let mut socket = &socket;
                let _ = write!(
                    socket,
                    "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
            }
        });
        (base, seen)
    }

    #[test]
    fn the_metrics_scrape_sends_the_configured_bearer() {
        let (base, seen) = api_fixture();
        let base = validate_api_base(&base).unwrap();
        let check = api_check_blocking(&base, Some("t"), &["main".to_owned()], now()).unwrap();
        assert_eq!(check["metrics"]["status"], 200, "{check}");
        let seen = seen.lock().unwrap().clone();
        assert!(
            seen.contains(&("/metrics".to_owned(), "Bearer t".to_owned())),
            "{seen:?}"
        );

        let (unauthenticated, _) = api_fixture();
        let unauthenticated = validate_api_base(&unauthenticated).unwrap();
        let error = api_check_blocking(&unauthenticated, None, &["main".to_owned()], now())
            .expect_err("a protected scrape without a token fails explicitly");
        assert!(format!("{error:#}").contains("HTTP 401"), "{error:#}");
    }

    #[test]
    fn api_check_constructs_its_timer_inside_the_runtime() {
        let base = validate_api_base("http://127.0.0.1:0").unwrap();
        let result = api_check_blocking(&base, None, &["main".to_owned()], now());
        assert!(result.is_err());
    }
}
