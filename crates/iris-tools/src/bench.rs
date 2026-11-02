//! `replay-bench`: the daemon's cost against replayed cameras, with nothing
//! but loopback involved.

use std::fmt::Write as _;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::Context;
use clap::Parser;
use serde_json::json;

use crate::logs::{kv, number, strip_ansi};

/// Arguments of `replay-bench`.
#[derive(Debug, Parser)]
pub struct Args {
    /// The recording every camera replays.
    #[arg(long)]
    file: PathBuf,
    /// Number of cameras.
    #[arg(long, default_value_t = 1)]
    cameras: u32,
    /// How long the daemon runs.
    #[arg(long, default_value_t = 120)]
    seconds: u64,
    /// Scratch root (recreated).
    #[arg(long)]
    root: PathBuf,
    /// The daemon binary.
    #[arg(long)]
    irisd: PathBuf,
    /// The replay binary.
    #[arg(long)]
    replay: PathBuf,
    /// The replay server's port.
    #[arg(long, default_value_t = 18554)]
    port: u16,
    /// The daemon's API port.
    #[arg(long, default_value_t = 18600)]
    api_port: u16,
    /// `ingest.read_interval_ms` for the daemon.
    #[arg(long, default_value_t = 0)]
    read_interval_ms: u64,
    /// `ingest.recv_low_water_kib` for the daemon.
    #[arg(long, default_value_t = 0)]
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
    #[arg(long, default_value_t = 0, value_name = "SECONDS")]
    perf: u64,
}

#[derive(Clone, Copy, Debug)]
struct Sample {
    t: Instant,
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
        utime: n(11),
        stime: n(12),
        rss_kib,
        voluntary,
    })
}

fn wait_port(port: u16, seconds: u64) -> bool {
    let deadline = Instant::now() + Duration::from_secs(seconds);
    while Instant::now() < deadline {
        if let Ok(output) = Command::new("ss").args(["-ltnH"]).output()
            && String::from_utf8_lossy(&output.stdout).contains(&format!(":{port} "))
        {
            return true;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    false
}

fn write_config(path: &Path, root: &Path, args: &Args) -> anyhow::Result<()> {
    let mut text = format!(
        "[storage]\nvideo_roots = [\"{}\"]\nstate_dir = \"{}\"\nmin_free_percent = 1\nquota_gib = 20\n\n[api]\nlisten = \"127.0.0.1:{}\"\ntokens = [\"bench\"]\n\n[ingest]\nread_interval_ms = {}\nrecv_low_water_kib = {}\n",
        root.join("run").display(),
        root.join("state").display(),
        args.api_port,
        args.read_interval_ms,
        args.recv_low_water_kib
    );
    for i in 1..=args.cameras {
        let _ = write!(
            text,
            "\n[[camera]]\nid = \"cam{i:02}\"\nname = \"bench {i}\"\nrtsp_url = \"rtsp://127.0.0.1:{}/main\"\nusername = \"bench\"\npassword = \"bench\"\nrecord = [\"main\"]\nevents = []\n",
            args.port
        );
    }
    std::fs::write(path, text)?;
    Ok(())
}

fn terminate(child: &mut Child, signal: &str, wait: Duration) {
    let _ = Command::new("kill")
        .args([signal, &child.id().to_string()])
        .status();
    let deadline = Instant::now() + wait;
    while Instant::now() < deadline {
        if matches!(child.try_wait(), Ok(Some(_))) {
            return;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let _ = child.kill();
    let _ = child.wait();
}

/// Runs the tool.
#[allow(clippy::too_many_lines)]
pub fn run(args: &Args) -> anyhow::Result<()> {
    let root = std::path::absolute(&args.root)?;
    if root.exists() {
        std::fs::remove_dir_all(&root)?;
    }
    std::fs::create_dir_all(root.join("run"))?;
    std::fs::create_dir_all(root.join("state"))?;
    let config = root.join("bench.toml");
    write_config(&config, &root, args)?;
    let log = root.join("irisd.log");

    let mut replay = Command::new(&args.replay)
        .args([
            "--listen",
            &format!("127.0.0.1:{}", args.port),
            "--mount",
            &format!("main={}", args.file.display()),
        ])
        .args(&args.replay_arg)
        .stdout(std::fs::File::create(root.join("replay.log"))?)
        .stderr(Stdio::inherit())
        .spawn()
        .context("starting iris-replay")?;
    let result = (|| -> anyhow::Result<(Vec<Sample>, Option<Instant>)> {
        anyhow::ensure!(
            wait_port(args.port, 30),
            "iris-replay did not start listening"
        );
        let mut daemon = Command::new(&args.irisd)
            .args([
                "run",
                "--config",
                &config.display().to_string(),
                "--stats-interval",
                &args.stats_interval,
            ])
            .args(&args.daemon_arg)
            .stdout(std::fs::File::create(&log)?)
            .stderr(std::fs::File::options().append(true).open(&log)?)
            .spawn()
            .context("starting irisd")?;
        let mut samples = Vec::new();
        let started = Instant::now();
        let mut perf: Option<Child> = None;
        let outcome = (|| -> anyhow::Result<()> {
            while started.elapsed() < Duration::from_secs(args.seconds) {
                std::thread::sleep(Duration::from_secs(5));
                if let Some(status) = daemon.try_wait()? {
                    anyhow::bail!("irisd exited early with {status}; see {}", log.display());
                }
                samples.push(proc_sample(daemon.id())?);
                if args.perf > 0 && perf.is_none() && started.elapsed() >= Duration::from_secs(20) {
                    perf = Some(
                        Command::new("sudo")
                            .args([
                                "-n",
                                "perf",
                                "record",
                                "-F",
                                "997",
                                "-g",
                                "-p",
                                &daemon.id().to_string(),
                                "-o",
                                &root.join("perf.data").display().to_string(),
                                "--",
                                "sleep",
                                &args.perf.to_string(),
                            ])
                            .stdout(std::fs::File::create(root.join("perf.log"))?)
                            .stderr(Stdio::null())
                            .spawn()
                            .context("starting perf")?,
                    );
                }
            }
            if let Some(mut p) = perf.take() {
                let _ = p.wait();
                let uid = rustix::process::getuid().as_raw();
                let gid = rustix::process::getgid().as_raw();
                let _ = Command::new("sudo")
                    .args([
                        "-n",
                        "chown",
                        &format!("{uid}:{gid}"),
                        &root.join("perf.data").display().to_string(),
                    ])
                    .status();
            }
            Ok(())
        })();
        terminate(&mut daemon, "-INT", Duration::from_secs(30));
        outcome?;
        Ok((samples, Some(started)))
    })();
    terminate(&mut replay, "-TERM", Duration::from_secs(10));
    let (samples, _) = result?;
    anyhow::ensure!(!samples.is_empty(), "no samples taken");

    let ticks = rustix::param::clock_ticks_per_second() as f64;
    let first = samples[0];
    let window: Vec<Sample> = samples
        .iter()
        .copied()
        .filter(|s| s.t.duration_since(first.t) >= Duration::from_secs(20))
        .collect();
    let window = if window.is_empty() {
        samples.clone()
    } else {
        window
    };
    let (a, b) = (window[0], window[window.len() - 1]);
    let secs = b.t.duration_since(a.t).as_secs_f64().max(1e-9);
    let user = (b.utime - a.utime) as f64 / ticks / secs * 100.0;
    let system = (b.stime - a.stime) as f64 / ticks / secs * 100.0;
    let wakeups = (b.voluntary - a.voluntary) as f64 / secs;
    let rss = samples.iter().map(|s| s.rss_kib).max().unwrap_or(0) as f64 / 1024.0;

    let text = std::fs::read_to_string(&log)?;
    let mut streams = std::collections::BTreeMap::new();
    for raw in text.lines() {
        let line = strip_ansi(raw);
        if line.contains(" INFO stream ") {
            let d = kv(&line);
            if let Some(name) = d.get("stream").cloned() {
                streams.insert(name, d);
            }
        }
    }
    let sum = |key: &str| streams.values().filter_map(|d| number(d, key)).sum::<f64>();
    let total_secs = samples[samples.len() - 1]
        .t
        .duration_since(first.t)
        .as_secs_f64()
        .max(1e-9);
    let mbps = sum("mib") * 8.388_608 / total_secs;
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
        "pct_per_mbps": if mbps > 0.0 { json!(((user + system) / mbps * 10_000.0).round() / 10_000.0) } else { json!(null) },
        "wakeups_per_s": wakeups.round() as i64,
        "rss_mib": (rss * 10.0).round() / 10.0,
        "frames": sum("frames") as i64,
        "reconnects": sum("reconnects") as i64,
        "dropped": sum("dropped") as i64,
        "loss": sum("loss") as i64,
        "streams_reporting": streams.len(),
    });
    println!("{result}");
    if args.perf > 0 {
        let _ = writeln!(
            std::io::stderr(),
            "perf data: {}",
            root.join("perf.data").display()
        );
    }
    if !args.keep {
        let _ = std::fs::remove_dir_all(root.join("run"));
    }
    Ok(())
}
