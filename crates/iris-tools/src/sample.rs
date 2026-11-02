//! `soak-sample`: one host-side sample beside a running service, meant for a
//! systemd user timer every minute. Everything it reads is read-only: `/proc`,
//! `/sys`, the service's cgroup, `systemctl show`, `ss`, `chronyc`, the
//! journal, and the API over loopback.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::Context;
use clap::Parser;
use serde_json::{Map, Value, json};

use crate::time::{iso, now};

const CGROUP_ROOT: &str = "/sys/fs/cgroup/system.slice";

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
    #[arg(long)]
    token_env: Option<String>,
    /// Minutes between API checks; 0 disables them.
    #[arg(long, default_value_t = 0)]
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

fn run_cmd(cmd: &[&str]) -> anyhow::Result<String> {
    let output = Command::new(cmd[0])
        .args(&cmd[1..])
        .output()
        .with_context(|| format!("running {}", cmd[0]))?;
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
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

fn unit_props(unit: &str) -> anyhow::Result<Value> {
    let props = "ActiveState,SubState,NRestarts,MainPID,StatusText,ExecMainStartTimestamp";
    let text = run_cmd(&["systemctl", "show", &format!("{unit}.service"), "-p", props])?;
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
    let start = line.find(prefix)? + prefix.len();
    let rest = &line[start..];
    let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
    Some(&rest[..end])
}

#[derive(Default)]
struct TcpClass {
    conns: i64,
    retrans_total: i64,
    lost: i64,
    rtts: Vec<f64>,
}

fn tcp(subnet: &str) -> anyhow::Result<Value> {
    let text = run_cmd(&["ss", "-tinH", "dst", subnet])?;
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

fn chrony() -> anyhow::Result<Value> {
    let mut out = Map::new();
    for line in run_cmd(&["chronyc", "tracking"])?.lines() {
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

fn export_journal(unit: &str, out_dir: &Path) -> anyhow::Result<Value> {
    let cursor = out_dir.join("journal.cursor");
    let log = out_dir.join("journal.log");
    let sink = std::fs::OpenOptions::new()
        .append(true)
        .create(true)
        .open(&log)?;
    let output = Command::new("journalctl")
        .args([
            "-u",
            &format!("{unit}.service"),
            "-o",
            "cat",
            "--no-pager",
            &format!("--cursor-file={}", cursor.display()),
        ])
        .stdout(sink)
        .output()
        .context("running journalctl")?;
    let stderr: String = String::from_utf8_lossy(&output.stderr)
        .chars()
        .take(120)
        .collect();
    Ok(
        json!({ "rc": output.status.code(), "bytes": std::fs::metadata(&log)?.len(), "stderr": stderr }),
    )
}

/// Counts media segments, duration, discontinuities, gaps, and byte-range breaks.
pub fn playlist_summary(text: &str) -> Value {
    let (mut media, mut discontinuities, mut gaps, mut breaks, mut maps) = (0, 0, 0, 0, 0);
    let mut duration = 0.0;
    let mut next_offset: BTreeMap<String, i64> = BTreeMap::new();
    let mut pending: Option<(i64, Option<i64>)> = None;
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("#EXTINF:") {
            media += 1;
            duration += rest
                .split(',')
                .next()
                .and_then(|v| v.parse::<f64>().ok())
                .unwrap_or(0.0);
        } else if let Some(rest) = line.strip_prefix("#EXT-X-BYTERANGE:") {
            let (length, offset) = rest
                .split_once('@')
                .map_or((rest, None), |(l, o)| (l, Some(o)));
            pending = Some((
                length.parse().unwrap_or(0),
                offset.and_then(|o| o.parse().ok()),
            ));
        } else if line.starts_with("#EXT-X-DISCONTINUITY") {
            discontinuities += 1;
        } else if line.starts_with("#EXT-X-GAP") {
            gaps += 1;
        } else if line.starts_with("#EXT-X-MAP") {
            maps += 1;
        } else if !line.is_empty()
            && !line.starts_with('#')
            && let Some((length, offset)) = pending.take()
        {
            let offset = match offset {
                None => next_offset.get(line).copied().unwrap_or(0),
                Some(o) => {
                    if next_offset.get(line).is_some_and(|&n| n != o) {
                        breaks += 1;
                    }
                    o
                }
            };
            next_offset.insert(line.to_owned(), offset + length);
        }
    }
    json!({
        "media_segments": media, "duration_s": (duration * 1000.0).round() / 1000.0,
        "discontinuities": discontinuities, "gaps": gaps, "range_breaks": breaks,
        "files": next_offset.len(), "maps": maps,
    })
}

async fn api_get(
    client: &reqwest::Client,
    base: &str,
    path: &str,
    token: Option<&str>,
) -> anyhow::Result<(u16, Vec<u8>)> {
    let mut request = client.get(format!("{base}{path}"));
    if let Some(token) = token {
        request = request.bearer_auth(token);
    }
    let response = request.send().await?;
    let status = response.status().as_u16();
    Ok((status, response.bytes().await?.to_vec()))
}

async fn api_check(
    base: &str,
    token: Option<&str>,
    kinds: &[&str],
    now_s: f64,
) -> anyhow::Result<Value> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .build()?;
    let mut out = Map::new();
    let (status, body) = api_get(&client, base, "/api/health", token).await?;
    out.insert(
        "health".into(),
        json!({ "status": status, "body": String::from_utf8_lossy(&body[..body.len().min(200)]) }),
    );
    let (status, body) = api_get(&client, base, "/api/cameras", token).await?;
    let ids: Vec<String> = if status == 200 {
        serde_json::from_slice::<Vec<Value>>(&body)?
            .iter()
            .filter_map(|c| c.get("id")?.as_str().map(str::to_owned))
            .collect()
    } else {
        Vec::new()
    };
    out.insert(
        "cameras".into(),
        json!({ "status": status, "count": ids.len() }),
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
            let (status, body) = api_get(&client, base, &path, token).await?;
            entry.insert("segments_status".into(), json!(status));
            if status == 200 {
                let rows: Vec<Value> = serde_json::from_slice(&body)?;
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
                entry.insert(
                    "bytes".into(),
                    json!(
                        rows.iter()
                            .filter_map(|r| r.get("bytes")?.as_i64())
                            .sum::<i64>()
                    ),
                );
                entry.insert(
                    "fragments".into(),
                    json!(
                        rows.iter()
                            .filter_map(|r| r.get("fragments")?.as_i64())
                            .sum::<i64>()
                    ),
                );
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
            let (status, body) = api_get(&client, base, &path, token).await?;
            entry.insert("playlist_status".into(), json!(status));
            if status == 200 {
                entry.insert(
                    "playlist".into(),
                    playlist_summary(&String::from_utf8_lossy(&body)),
                );
            }
            streams.insert(format!("{camera}/{kind}"), Value::Object(entry));
        }
    }
    out.insert("streams".into(), Value::Object(streams));
    Ok(Value::Object(out))
}

fn append_line(path: &Path, value: &Value) -> anyhow::Result<()> {
    let mut sink = std::fs::OpenOptions::new()
        .append(true)
        .create(true)
        .open(path)?;
    writeln!(sink, "{value}")?;
    Ok(())
}

/// Runs the tool.
pub fn run(args: &Args) -> anyhow::Result<()> {
    std::fs::create_dir_all(&args.out)?;
    let now_s = now();
    let unit = section(|| unit_props(&args.unit));
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
    sample.insert("chrony".into(), section(chrony));
    if let Some(disk) = &args.disk {
        sample.insert("disk".into(), section(|| diskstats(disk)));
    }
    if let Some(iface) = &args.iface {
        sample.insert("nic".into(), section(|| nic(iface)));
    }
    if let Some(subnet) = &args.subnet {
        sample.insert("tcp".into(), section(|| tcp(subnet)));
    }
    if !args.no_journal {
        sample.insert(
            "journal".into(),
            section(|| export_journal(&args.unit, &args.out)),
        );
    }
    let sample = Value::Object(sample);
    append_line(&args.out.join("host.jsonl"), &sample)?;
    if args.print {
        println!("{}", serde_json::to_string_pretty(&sample)?);
    }
    let due = args.api_check_every > 0 && (now_s as u64 / 60).is_multiple_of(args.api_check_every);
    if let (Some(base), true) = (&args.api, due) {
        let token = args.token_env.as_ref().and_then(|v| std::env::var(v).ok());
        let kinds: Vec<&str> = args.kinds.split(',').collect();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        let mut check = Map::new();
        check.insert("ts".into(), json!(iso(now_s)));
        let result = section(|| runtime.block_on(api_check(base, token.as_deref(), &kinds, now_s)));
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
        append_line(&args.out.join("api.jsonl"), &check)?;
        if args.print {
            println!("{}", serde_json::to_string_pretty(&check)?);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn playlist_summary_counts_ranges_and_breaks() {
        let text = "#EXTM3U\n#EXT-X-MAP:URI=\"a\",BYTERANGE=\"10@0\"\n#EXTINF:2.0,\n#EXT-X-BYTERANGE:100@10\na\n#EXTINF:2.0,\n#EXT-X-BYTERANGE:50\na\n#EXT-X-DISCONTINUITY\n#EXTINF:1.5,\n#EXT-X-BYTERANGE:20@500\na\n";
        let s = playlist_summary(text);
        assert_eq!(s["media_segments"], 3);
        assert_eq!(s["duration_s"], 5.5);
        assert_eq!(s["discontinuities"], 1);
        assert_eq!(s["range_breaks"], 1);
        assert_eq!(s["maps"], 1);
        assert_eq!(s["files"], 1);
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
}
