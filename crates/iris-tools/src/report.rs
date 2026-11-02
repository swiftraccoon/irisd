//! `soak-report`: the soak exit-criteria figures from a sampler directory.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::Context;
use clap::Parser;
use serde_json::Value;

use crate::logs::{col, col_max, fmt, growth, kv, mean, median, number, rates, split, strip_ansi};
use crate::stats::Row;
use crate::time::{iso, parse_seconds};

const STATS: [&str; 6] = ["stream", "events", "writer", "catalog", "root", "process"];

/// Arguments of `soak-report`.
#[derive(Debug, Parser)]
pub struct Args {
    /// The sampler's output directory: `journal.log`, `host.jsonl`, `api.jsonl`.
    dir: PathBuf,
    /// Start of the window (RFC 3339).
    #[arg(long)]
    since: Option<String>,
    /// End of the window (RFC 3339).
    #[arg(long)]
    until: Option<String>,
}

/// One service run between a start and a `stopped` line.
#[derive(Debug, Default)]
struct Run {
    start: f64,
    stop: Option<f64>,
    clean: bool,
    recovered: Option<BTreeMap<String, String>>,
    streams: BTreeMap<String, Vec<Row>>,
    events: BTreeMap<String, Vec<Row>>,
    writer: Vec<Row>,
    catalog: Vec<Row>,
    process: Vec<Row>,
    roots: BTreeMap<String, Vec<Row>>,
    /// Notable lines by kind: `gap`, `selfcheck`, `retention`, `slow`.
    notable: BTreeMap<&'static str, Vec<(f64, String, String)>>,
    problems: Vec<(f64, String, String)>,
}

fn parse_journal(path: &Path, since: f64, until: f64) -> anyhow::Result<Vec<Run>> {
    let text =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let mut runs: Vec<Run> = Vec::new();
    for raw in text.lines() {
        let line = strip_ansi(raw);
        let Some(l) = split(&line) else { continue };
        let Some(t) = parse_seconds(l.ts) else {
            continue;
        };
        if t < since || t > until {
            continue;
        }
        let kind = l.rest.split(' ').next().unwrap_or("");
        if runs.last().is_none_or(|r| r.stop.is_some()) {
            runs.push(Run {
                start: t,
                ..Run::default()
            });
        }
        let run = runs.last_mut().expect("pushed above");
        if l.rest.starts_with("catalog reconciled") {
            run.recovered = Some(kv(l.rest));
        } else if l.rest.starts_with("SIGTERM") || l.rest.starts_with("interrupted") {
            run.clean = true;
        } else if l.rest == "stopped" {
            run.stop = Some(t);
        } else if l.level == "INFO" && STATS.contains(&kind) && l.rest.contains('=') {
            let d = kv(l.rest);
            match kind {
                "stream" => run
                    .streams
                    .entry(d.get("stream").cloned().unwrap_or_default())
                    .or_default()
                    .push((t, d)),
                "events" => run
                    .events
                    .entry(d.get("camera").cloned().unwrap_or_default())
                    .or_default()
                    .push((t, d)),
                "root" => run
                    .roots
                    .entry(d.get("root").cloned().unwrap_or_default())
                    .or_default()
                    .push((t, d)),
                "writer" => run.writer.push((t, d)),
                "catalog" => run.catalog.push((t, d)),
                _ => run.process.push((t, d)),
            }
        } else if let Some(k) = notable_kind(l.rest) {
            run.notable
                .entry(k)
                .or_default()
                .push((t, l.level.to_owned(), l.rest.to_owned()));
        } else if (l.level == "WARN" || l.level == "ERROR") && !l.rest.contains("unknown rtcp ssrc")
        {
            run.problems
                .push((t, l.level.to_owned(), l.rest.to_owned()));
        }
    }
    Ok(runs)
}

fn notable_kind(rest: &str) -> Option<&'static str> {
    if rest.starts_with("gap ") {
        Some("gap")
    } else if rest.starts_with("selfcheck ") {
        Some("selfcheck")
    } else if rest.starts_with("retention pass") || rest.starts_with("slow retention") {
        Some("retention")
    } else if rest.starts_with("slow ") {
        Some("slow")
    } else {
        None
    }
}

fn slope_per_day(rows: &[Row], key: &str) -> Option<f64> {
    let points: Vec<(f64, f64)> = rows
        .iter()
        .filter_map(|(t, d)| number(d, key).map(|v| (*t, v)))
        .collect();
    if points.len() < 3 {
        return None;
    }
    let t0 = points[0].0;
    let xs: Vec<f64> = points.iter().map(|(t, _)| (t - t0) / 86_400.0).collect();
    let ys: Vec<f64> = points.iter().map(|(_, v)| *v).collect();
    let mx = mean(&xs)?;
    let my = mean(&ys)?;
    let den: f64 = xs.iter().map(|x| (x - mx).powi(2)).sum();
    if den == 0.0 {
        return None;
    }
    Some(
        xs.iter()
            .zip(&ys)
            .map(|(x, y)| (x - mx) * (y - my))
            .sum::<f64>()
            / den,
    )
}

fn sum_of_maxes(runs: &[Run], pick: impl Fn(&Run) -> &Vec<Row>, key: &str) -> f64 {
    runs.iter()
        .map(|r| col_max(pick(r), key).unwrap_or(0.0))
        .sum()
}

fn all<'a>(runs: &'a [Run], pick: impl Fn(&'a Run) -> &'a Vec<Row>) -> Vec<Row> {
    runs.iter().flat_map(|r| pick(r).iter().cloned()).collect()
}

fn min_of(values: &[f64]) -> Option<f64> {
    values.iter().copied().reduce(f64::min)
}

fn max_of(values: &[f64]) -> Option<f64> {
    values.iter().copied().reduce(f64::max)
}

fn report_runs(runs: &[Run], out: &mut impl Write) -> std::io::Result<()> {
    writeln!(out, "== runs")?;
    for (i, run) in runs.iter().enumerate() {
        let end = run
            .stop
            .or_else(|| run.process.last().map(|r| r.0))
            .unwrap_or(run.start);
        let hours = (end - run.start) / 3_600.0;
        let state = match (run.clean, run.stop) {
            (true, Some(_)) => "stopped cleanly",
            (_, Some(_)) => "stopped",
            _ => "running",
        };
        let rec = run.recovered.clone().unwrap_or_default();
        writeln!(
            out,
            "run {}: {} to {} ({hours:.1} h) {state}; recovered={} adopted={}; warnings/errors={}",
            i + 1,
            iso(run.start),
            iso(end),
            rec.get("recovered").map_or("-", String::as_str),
            rec.get("adopted").map_or("-", String::as_str),
            run.problems.len()
        )?;
        if i > 0 {
            let previous = &runs[i - 1];
            let gap = (run.start - previous.stop.unwrap_or(previous.start)) / 60.0;
            writeln!(out, "   downtime before this run: {gap:.1} min")?;
        }
    }
    let mut problems: Vec<(String, usize)> = Vec::new();
    for (_, level, rest) in runs.iter().flat_map(|r| r.problems.iter()) {
        let key = format!("{level} {}", mask_values(rest));
        match problems.iter_mut().find(|(k, _)| *k == key) {
            Some(p) => p.1 += 1,
            None => problems.push((key, 1)),
        }
    }
    problems.sort_by_key(|p| std::cmp::Reverse(p.1));
    for (key, n) in problems.iter().take(10) {
        writeln!(out, "   {n:5}x {}", truncate(key, 140))?;
    }
    Ok(())
}

/// Replaces every `key=value` value with an ellipsis, so lines group by shape.
fn mask_values(rest: &str) -> String {
    let mut out = String::new();
    for (i, word) in rest.split(' ').enumerate() {
        if i > 0 {
            out.push(' ');
        }
        match word.split_once('=') {
            Some((k, _)) => {
                out.push_str(k);
                out.push_str("=…");
            }
            None => out.push_str(word),
        }
    }
    out
}

fn truncate(s: &str, max: usize) -> &str {
    match s.char_indices().nth(max) {
        Some((i, _)) => &s[..i],
        None => s,
    }
}

fn report_process(runs: &[Run], out: &mut impl Write) -> std::io::Result<()> {
    writeln!(out, "\n== process")?;
    let rows = all(runs, |r| &r.process);
    if rows.is_empty() {
        return Ok(());
    }
    let cpu = col(&rows, "cpu_pct");
    let rss = col(&rows, "rss_mib");
    let last = &runs[runs.len() - 1].process;
    let first_hour = col(&last[..last.len().min(60)], "rss_mib");
    let last_hour = col(&last[last.len().saturating_sub(60)..], "rss_mib");
    writeln!(
        out,
        "samples={} cpu_pct mean={} max={} (user {}, sys {})",
        rows.len(),
        fmt(mean(&cpu), 2),
        fmt(max_of(&cpu), 2),
        fmt(mean(&col(&rows, "cpu_user_pct")).or(Some(0.0)), 2),
        fmt(mean(&col(&rows, "cpu_sys_pct")).or(Some(0.0)), 2)
    )?;
    writeln!(
        out,
        "rss_mib min={} max={}; last run: first hour mean={} last hour mean={} slope={} MiB/day",
        fmt(min_of(&rss), 1),
        fmt(max_of(&rss), 1),
        fmt(mean(&first_hour), 1),
        fmt(mean(&last_hour), 1),
        fmt(slope_per_day(last, "rss_mib"), 2)
    )?;
    writeln!(
        out,
        "loop_lag_max_ms={} retention_max_ms={} watchdog_withheld={} fds min={} max={} threads max={} selfcheck ok={} failed={}",
        fmt(col_max(&rows, "loop_lag_max_ms"), 0),
        fmt(col_max(&rows, "retention_max_ms"), 0),
        fmt(
            Some(sum_of_maxes(runs, |r| &r.process, "watchdog_withheld")),
            0
        ),
        fmt(min_of(&col(&rows, "fds")).or(Some(0.0)), 0),
        fmt(col_max(&rows, "fds"), 0),
        fmt(col_max(&rows, "threads"), 0),
        fmt(Some(sum_of_maxes(runs, |r| &r.process, "selfcheck_ok")), 0),
        fmt(
            Some(sum_of_maxes(runs, |r| &r.process, "selfcheck_failed")),
            0
        )
    )
}

fn report_writer_catalog(runs: &[Run], out: &mut impl Write) -> std::io::Result<()> {
    writeln!(out, "\n== writer")?;
    let rows = all(runs, |r| &r.writer);
    if !rows.is_empty() {
        let totals: Vec<String> = [
            "fragments",
            "mib",
            "syncs",
            "files",
            "dropped",
            "write_errors",
            "slow_io",
        ]
        .iter()
        .map(|k| format!("{k}={}", sum_of_maxes(runs, |r| &r.writer, k) as i64))
        .collect();
        writeln!(out, "totals: {}", totals.join(" "))?;
        writeln!(
            out,
            "write_p99_max_ms={} write_max_ms={} sync_p99_max_ms={} sync_max_ms={} queue_peak={} queue_mib_peak={}",
            fmt(col_max(&rows, "write_p99_ms"), 1),
            fmt(col_max(&rows, "write_max_ms"), 1),
            fmt(col_max(&rows, "sync_p99_ms"), 1),
            fmt(col_max(&rows, "sync_max_ms"), 1),
            fmt(col_max(&rows, "queue_peak"), 0),
            fmt(col_max(&rows, "queue_mib_peak"), 1)
        )?;
    }
    writeln!(out, "\n== catalog")?;
    let rows = all(runs, |r| &r.catalog);
    if let Some((_, last)) = rows.last() {
        let items: Vec<String> = ["segments", "gib", "events", "db_mib", "wal_mib"]
            .iter()
            .map(|k| format!("{k}={}", last.get(*k).map_or("-", String::as_str)))
            .collect();
        writeln!(out, "last: {}", items.join(" "))?;
        writeln!(
            out,
            "txn_p99_max_ms={} txn_max_ms={} query_max_ms={} wait_max_ms={} event_commit_p99_max_ms={} event_commit_max_ms={} batch_peak={} failures={}",
            fmt(col_max(&rows, "txn_p99_ms"), 1),
            fmt(col_max(&rows, "txn_max_ms"), 1),
            fmt(col_max(&rows, "query_max_ms"), 1),
            fmt(col_max(&rows, "wait_max_ms"), 1),
            fmt(col_max(&rows, "event_commit_p99_ms"), 1),
            fmt(col_max(&rows, "event_commit_max_ms"), 1),
            fmt(col_max(&rows, "batch_peak"), 0),
            sum_of_maxes(runs, |r| &r.catalog, "failures") as i64
        )?;
    }
    Ok(())
}

fn int(d: &BTreeMap<String, String>, key: &str) -> i64 {
    d.get(key)
        .and_then(|v| v.parse::<f64>().ok())
        .map_or(0, |v| v as i64)
}

fn report_streams(runs: &[Run], out: &mut impl Write) -> std::io::Result<()> {
    writeln!(
        out,
        "\n== streams (sums over runs; fps between consecutive samples within a run)"
    )?;
    writeln!(
        out,
        "stream           fps_med fps_min stalls reconn gaps holds hold_s snaps loss  cam_ms(min/max) idle_max frag_kib_max"
    )?;
    let mut names: Vec<&String> = runs.iter().flat_map(|r| r.streams.keys()).collect();
    names.sort();
    names.dedup();
    for name in names {
        let mut all_rates = Vec::new();
        let mut sums: BTreeMap<&str, i64> = BTreeMap::new();
        let mut hold_s = 0.0;
        let (mut offsets, mut idle, mut frag) = (Vec::new(), Vec::new(), Vec::new());
        for run in runs {
            let Some(rows) = run.streams.get(name) else {
                continue;
            };
            all_rates.extend(rates(rows));
            if let Some((_, f)) = rows.last() {
                for k in ["reconnects", "gaps", "holds", "snaps", "loss"] {
                    *sums.entry(k).or_default() += int(f, k);
                }
                hold_s += number(f, "hold_s").unwrap_or(0.0);
            }
            offsets.extend(col(rows, "cam_offset_ms"));
            idle.extend(col(rows, "idle_s"));
            frag.extend(col(rows, "frag_kib_max"));
        }
        let Some(med) = median(&all_rates) else {
            continue;
        };
        let stalls = all_rates.iter().filter(|&&r| r < 0.5 * med).count();
        writeln!(
            out,
            "{name:16} {med:7.2} {:7.2} {stalls:6} {:6} {:4} {:5} {hold_s:6.1} {:5} {:4}  {:>5}/{:<5} {:>8} {:>12}",
            min_of(&all_rates).unwrap_or(0.0),
            sums.get("reconnects").copied().unwrap_or(0),
            sums.get("gaps").copied().unwrap_or(0),
            sums.get("holds").copied().unwrap_or(0),
            sums.get("snaps").copied().unwrap_or(0),
            sums.get("loss").copied().unwrap_or(0),
            fmt(min_of(&offsets), 0),
            fmt(max_of(&offsets), 0),
            fmt(max_of(&idle), 1),
            fmt(max_of(&frag), 0)
        )?;
    }
    writeln!(out, "\n== event feeds")?;
    writeln!(
        out,
        "camera   events messages reconn renewals errors idle_max pull_p99_max"
    )?;
    let mut cams: Vec<&String> = runs.iter().flat_map(|r| r.events.keys()).collect();
    cams.sort();
    cams.dedup();
    for cam in cams {
        let mut sums: BTreeMap<&str, i64> = BTreeMap::new();
        let (mut idle, mut pull) = (Vec::new(), Vec::new());
        for run in runs {
            let Some(rows) = run.events.get(cam) else {
                continue;
            };
            if let Some((_, f)) = rows.last() {
                for k in ["events", "messages", "reconnects", "renewals", "errors"] {
                    *sums.entry(k).or_default() += int(f, k);
                }
            }
            idle.extend(col(rows, "idle_s"));
            pull.extend(col(rows, "pull_p99_ms"));
        }
        writeln!(
            out,
            "{cam:8} {:6} {:8} {:6} {:8} {:6} {:>8} {:>12}",
            sums.get("events").copied().unwrap_or(0),
            sums.get("messages").copied().unwrap_or(0),
            sums.get("reconnects").copied().unwrap_or(0),
            sums.get("renewals").copied().unwrap_or(0),
            sums.get("errors").copied().unwrap_or(0),
            fmt(max_of(&idle), 1),
            fmt(max_of(&pull), 1)
        )?;
    }
    Ok(())
}

fn notable<'a>(runs: &'a [Run], kind: &str) -> Vec<&'a (f64, String, String)> {
    runs.iter()
        .flat_map(|r| r.notable.get(kind).into_iter().flatten())
        .collect()
}

fn report_notable(runs: &[Run], out: &mut impl Write) -> std::io::Result<()> {
    let gaps = notable(runs, "gap");
    writeln!(out, "\n== gaps ({})", gaps.len())?;
    let mut per: BTreeMap<String, f64> = BTreeMap::new();
    let mut causes: BTreeMap<String, usize> = BTreeMap::new();
    for (_, _, rest) in &gaps {
        let d = kv(rest);
        *per.entry(d.get("stream").cloned().unwrap_or_else(|| "?".into()))
            .or_default() += number(&d, "secs").unwrap_or(0.0);
        *causes
            .entry(d.get("cause").cloned().unwrap_or_else(|| "?".into()))
            .or_default() += 1;
    }
    for (stream, secs) in &per {
        writeln!(out, "   {stream:16} {secs:8.1} s")?;
    }
    if !causes.is_empty() {
        let items: Vec<String> = causes.iter().map(|(k, v)| format!("{k}={v}")).collect();
        writeln!(out, "   by cause: {}", items.join(" "))?;
    }
    for (t, _, rest) in gaps.iter().take(50) {
        writeln!(out, "   {} {}", iso(*t), truncate(rest, 160))?;
    }
    let passes = notable(runs, "retention");
    writeln!(out, "\n== retention passes logged ({})", passes.len())?;
    if !passes.is_empty() {
        let ds: Vec<BTreeMap<String, String>> =
            passes.iter().map(|(_, _, rest)| kv(rest)).collect();
        let deleted: i64 = ds.iter().map(|d| int(d, "deleted")).sum();
        let freed: i64 = ds.iter().map(|d| int(d, "freed_mib")).sum();
        let ms: Vec<f64> = ds.iter().map(|d| number(d, "ms").unwrap_or(0.0)).collect();
        let free: Vec<f64> = ds.iter().filter_map(|d| number(d, "free_pct")).collect();
        let total: Vec<f64> = ds.iter().filter_map(|d| number(d, "total_gib")).collect();
        writeln!(
            out,
            "deleted={deleted} freed_gib={:.1} ms max={:.0} mean={:.0} free_pct min={} total_gib min={} max={} quota_gib={} slow={}",
            freed as f64 / 1024.0,
            max_of(&ms).unwrap_or(0.0),
            mean(&ms).unwrap_or(0.0),
            fmt(min_of(&free), 1),
            fmt(min_of(&total), 2),
            fmt(max_of(&total), 2),
            ds.last()
                .and_then(|d| d.get("quota_gib").cloned())
                .unwrap_or_else(|| "-".into()),
            passes
                .iter()
                .filter(|(_, level, _)| level == "WARN")
                .count()
        )?;
    }
    let slow = notable(runs, "slow");
    writeln!(out, "\n== slow I/O warnings ({})", slow.len())?;
    for (t, _, rest) in slow.iter().take(20) {
        writeln!(out, "   {} {}", iso(*t), truncate(rest, 120))?;
    }
    let checks = notable(runs, "selfcheck");
    let failed: Vec<_> = checks
        .iter()
        .filter(|(_, _, rest)| rest.starts_with("selfcheck failed"))
        .collect();
    writeln!(
        out,
        "\n== self-checks: {} run, {} failed",
        checks.len(),
        failed.len()
    )?;
    for (t, _, rest) in failed.iter().take(20) {
        writeln!(out, "   {} {}", iso(*t), truncate(rest, 220))?;
    }
    Ok(())
}

type Sample = (f64, Value);

fn load_jsonl(path: &Path, since: f64, until: f64) -> Vec<Sample> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    text.lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter_map(|d| {
            let t = parse_seconds(d.get("ts")?.as_str()?)?;
            (since <= t && t <= until).then_some((t, d))
        })
        .collect()
}

fn payload_bytes(runs: &[Run], since: f64, until: f64) -> f64 {
    runs.iter()
        .flat_map(|r| r.streams.values())
        .map(|rows| {
            let series: Vec<f64> = rows
                .iter()
                .filter(|(t, _)| since <= *t && *t <= until)
                .filter_map(|(_, d)| number(d, "mib"))
                .collect();
            growth(&series) * 1_048_576.0
        })
        .sum()
}

fn file_bytes(runs: &[Run], since: f64, until: f64) -> f64 {
    runs.iter()
        .map(|run| {
            let rows = if run.writer.is_empty() {
                &run.process
            } else {
                &run.writer
            };
            let series: Vec<f64> = rows
                .iter()
                .filter(|(t, _)| since <= *t && *t <= until)
                .filter_map(|(_, d)| number(d, "mib"))
                .collect();
            growth(&series) * 1_048_576.0
        })
        .sum()
}

fn f(v: &Value, key: &str) -> f64 {
    v.get(key).and_then(Value::as_f64).unwrap_or(0.0)
}

fn section<'a>(v: &'a Value, key: &str) -> Option<&'a Value> {
    v.get(key)
        .filter(|s| s.is_object() && s.get("error").is_none())
}

#[allow(clippy::too_many_lines)]
fn report_host(
    samples: &[Sample],
    runs: &[Run],
    out: &mut impl Write,
) -> std::io::Result<(Option<f64>, Option<f64>)> {
    writeln!(out, "\n== host samples ({})", samples.len())?;
    if samples.is_empty() {
        return Ok((None, None));
    }
    let restarts = samples
        .iter()
        .map(|(_, d)| f(d.get("unit").unwrap_or(&Value::Null), "NRestarts"))
        .fold(0.0, f64::max);
    let active: Vec<&Sample> = samples
        .iter()
        .filter(|(_, d)| {
            d.get("unit")
                .and_then(|u| u.get("ActiveState"))
                .and_then(Value::as_str)
                == Some("active")
        })
        .collect();
    let mut errors: BTreeMap<String, usize> = BTreeMap::new();
    for (_, d) in &active {
        if let Some(obj) = d.as_object() {
            for (k, v) in obj {
                if v.is_object() && v.get("error").is_some() {
                    *errors.entry(k.clone()).or_default() += 1;
                }
            }
        }
    }
    let errors_text = if errors.is_empty() {
        "none".to_owned()
    } else {
        errors
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect::<Vec<_>>()
            .join(" ")
    };
    writeln!(
        out,
        "window {} to {}; active={}; section errors while active={errors_text}",
        iso(samples[0].0),
        iso(samples[samples.len() - 1].0),
        active.len()
    )?;
    let cg: Vec<(f64, &Value)> = active
        .iter()
        .filter_map(|(t, d)| section(d, "cgroup").map(|c| (*t, c)))
        .collect();
    let wall = if cg.len() > 1 {
        cg[cg.len() - 1].0 - cg[0].0
    } else {
        0.0
    };
    let user = growth(
        &cg.iter()
            .map(|(_, c)| f(c, "cpu_user_us"))
            .collect::<Vec<_>>(),
    ) / 1e6;
    let system = growth(
        &cg.iter()
            .map(|(_, c)| f(c, "cpu_system_us"))
            .collect::<Vec<_>>(),
    ) / 1e6;
    let wbytes = growth(
        &cg.iter()
            .map(|(_, c)| f(c, "io_wbytes"))
            .collect::<Vec<_>>(),
    );
    let rbytes = growth(
        &cg.iter()
            .map(|(_, c)| f(c, "io_rbytes"))
            .collect::<Vec<_>>(),
    );
    let anon = cg.iter().map(|(_, c)| f(c, "anon")).fold(0.0, f64::max) / 1_048_576.0;
    let page_cache = cg.iter().map(|(_, c)| f(c, "file")).fold(0.0, f64::max) / 1_048_576.0;
    if wall > 0.0 {
        writeln!(
            out,
            "cgroup cpu: user {:.2} % + system {:.2} % of one core; anon max {anon:.1} MiB, page cache max {page_cache:.0} MiB",
            user / wall * 100.0,
            system / wall * 100.0
        )?;
    }
    let (since, until) = match (cg.first(), cg.last()) {
        (Some(a), Some(b)) => (a.0, b.0),
        _ => (0.0, f64::INFINITY),
    };
    let payload = payload_bytes(runs, since, until);
    let files = file_bytes(runs, since, until);
    let amplification = (payload > 0.0).then(|| wbytes / payload);
    let overhead = (payload > 0.0).then(|| files / payload);
    let gib = f64::from(1u32 << 30);
    writeln!(
        out,
        "bytes: payload {:.1} GiB, files {:.1} GiB, cgroup written {:.1} GiB, cgroup read {:.2} GiB -> container overhead {}, write amplification {}",
        payload / gib,
        files / gib,
        wbytes / gib,
        rbytes / gib,
        fmt(overhead, 4),
        fmt(amplification, 4)
    )?;
    let disk: Vec<&Value> = samples
        .iter()
        .filter_map(|(_, d)| section(d, "disk"))
        .collect();
    if !disk.is_empty() {
        let sectors = growth(
            &disk
                .iter()
                .map(|x| f(x, "sectors_written"))
                .collect::<Vec<_>>(),
        ) * 512.0;
        let weighted = growth(
            &disk
                .iter()
                .map(|x| f(x, "weighted_io_ms"))
                .collect::<Vec<_>>(),
        ) / 1000.0;
        writeln!(
            out,
            "disk total writes {:.1} GiB (all users of the disk), weighted io ms growth {weighted:.0} s",
            sectors / gib
        )?;
    }
    let psi: Vec<&Value> = samples
        .iter()
        .filter_map(|(_, d)| section(d, "psi"))
        .collect();
    if !psi.is_empty() {
        let m = |k: &str| psi.iter().map(|p| f(p, k)).fold(0.0, f64::max);
        writeln!(
            out,
            "psi io full avg10 max={:.1} memory some avg10 max={:.1} cpu some avg10 max={:.1}",
            m("io_full_avg10"),
            m("memory_some_avg10"),
            m("cpu_some_avg10")
        )?;
    }
    let nic: Vec<&Value> = samples
        .iter()
        .filter_map(|(_, d)| section(d, "nic"))
        .collect();
    if !nic.is_empty() {
        let items: Vec<String> = [
            "rx_errors",
            "rx_dropped",
            "rx_missed_errors",
            "rx_fifo_errors",
            "tx_errors",
        ]
        .iter()
        .map(|k| {
            format!(
                "{k}={}",
                growth(&nic.iter().map(|n| f(n, k)).collect::<Vec<_>>()) as i64
            )
        })
        .collect();
        writeln!(out, "nic growth: {}", items.join(" "))?;
    }
    let tcp: Vec<&Value> = active
        .iter()
        .filter_map(|(_, d)| section(d, "tcp"))
        .filter(|t| t.get("rtsp").is_some())
        .collect();
    if !tcp.is_empty() {
        for cls in ["rtsp", "http"] {
            let conns: Vec<f64> = tcp
                .iter()
                .map(|x| f(x.get(cls).unwrap_or(&Value::Null), "conns"))
                .collect();
            let retrans = growth(
                &tcp.iter()
                    .map(|x| f(x.get(cls).unwrap_or(&Value::Null), "retrans_total"))
                    .collect::<Vec<_>>(),
            );
            let rtt: Vec<f64> = tcp
                .iter()
                .filter_map(|x| x.get(cls))
                .filter(|c| f(c, "conns") > 0.0)
                .map(|c| f(c, "rtt_mean_ms"))
                .collect();
            let rtt_max = tcp
                .iter()
                .map(|x| f(x.get(cls).unwrap_or(&Value::Null), "rtt_max_ms"))
                .fold(0.0, f64::max);
            writeln!(
                out,
                "tcp {cls}: conns {}..{} retrans growth {} rtt mean {} ms max {rtt_max:.1} ms",
                min_of(&conns).unwrap_or(0.0) as i64,
                max_of(&conns).unwrap_or(0.0) as i64,
                retrans as i64,
                fmt(mean(&rtt), 1)
            )?;
        }
    }
    let chrony: Vec<f64> = samples
        .iter()
        .filter_map(|(_, d)| d.get("chrony")?.get("offset_s")?.as_f64())
        .map(f64::abs)
        .collect();
    if let Some(m) = max_of(&chrony) {
        writeln!(out, "ntp |offset| max {:.3} ms", m * 1000.0)?;
    }
    let proc: Vec<&Value> = active
        .iter()
        .filter_map(|(_, d)| section(d, "proc"))
        .collect();
    if !proc.is_empty() {
        writeln!(
            out,
            "proc VmHWM max {:.1} MiB, majflt growth {}, nonvoluntary switches growth {}",
            proc.iter().map(|p| f(p, "VmHWM")).fold(0.0, f64::max) / 1024.0,
            growth(&proc.iter().map(|p| f(p, "majflt")).collect::<Vec<_>>()) as i64,
            growth(
                &proc
                    .iter()
                    .map(|p| f(p, "nonvoluntary_ctxt_switches"))
                    .collect::<Vec<_>>()
            ) as i64
        )?;
    }
    let load: Vec<f64> = samples
        .iter()
        .filter_map(|(_, d)| d.get("host")?.get("load1")?.as_f64())
        .collect();
    if let Some(m) = max_of(&load) {
        writeln!(out, "load1 max {m:.1}")?;
    }
    Ok((amplification, Some(restarts)))
}

fn status_of(v: &Value, key: &str) -> i64 {
    v.get(key).and_then(Value::as_i64).unwrap_or(0)
}

fn report_api(checks: &[Sample], runs: &[Run], out: &mut impl Write) -> std::io::Result<()> {
    writeln!(out, "\n== api checks ({})", checks.len())?;
    let mut bad: Vec<(f64, String, String)> = Vec::new();
    for (t, d) in checks {
        let after_restart = runs.iter().any(|r| t - 660.0 <= r.start && r.start <= *t);
        let health = d.get("health").cloned().unwrap_or(Value::Null);
        if status_of(&health, "status") != 200 {
            bad.push((*t, "health".into(), health.to_string()));
        }
        if let Some(streams) = d.get("streams").and_then(Value::as_object) {
            for (name, s) in streams {
                let p = s.get("playlist").cloned().unwrap_or(Value::Null);
                if status_of(s, "segments_status") != 200 || status_of(s, "playlist_status") != 200
                {
                    bad.push((
                        *t,
                        name.clone(),
                        format!(
                            "segments_status={} playlist_status={}",
                            f(s, "segments_status"),
                            f(s, "playlist_status")
                        ),
                    ));
                } else if f(&p, "range_breaks") > 0.0
                    || f(&p, "gaps") > 0.0
                    || f(&p, "discontinuities") > 0.0
                    || (f(&p, "duration_s") < 540.0 && !after_restart)
                {
                    bad.push((*t, name.clone(), p.to_string()));
                }
            }
        }
    }
    writeln!(out, "problems: {}", bad.len())?;
    for (t, name, info) in bad.iter().take(30) {
        writeln!(out, "   {} {name} {}", iso(*t), truncate(info, 160))?;
    }
    Ok(())
}

/// Runs the tool.
#[allow(clippy::too_many_lines)]
pub fn run(args: &Args) -> anyhow::Result<()> {
    let since = match &args.since {
        Some(s) => parse_seconds(s).with_context(|| format!("bad --since {s:?}"))?,
        None => 0.0,
    };
    let until = match &args.until {
        Some(s) => parse_seconds(s).with_context(|| format!("bad --until {s:?}"))?,
        None => f64::INFINITY,
    };
    let runs = parse_journal(&args.dir.join("journal.log"), since, until)?;
    anyhow::ensure!(!runs.is_empty(), "no service lines in the window");
    let stdout = std::io::stdout();
    let mut out = std::io::BufWriter::new(stdout.lock());
    let result = (|| -> std::io::Result<()> {
        report_runs(&runs, &mut out)?;
        report_process(&runs, &mut out)?;
        report_writer_catalog(&runs, &mut out)?;
        report_streams(&runs, &mut out)?;
        report_notable(&runs, &mut out)?;
        let samples = load_jsonl(&args.dir.join("host.jsonl"), since, until);
        let (amplification, restarts) = report_host(&samples, &runs, &mut out)?;
        report_api(
            &load_jsonl(&args.dir.join("api.jsonl"), since, until),
            &runs,
            &mut out,
        )?;

        writeln!(out, "\n== criteria")?;
        let process = all(&runs, |r| &r.process);
        let writer = all(&runs, |r| &r.writer);
        let catalog = all(&runs, |r| &r.catalog);
        let gaps = notable(&runs, "gap").len();
        let reconnects: i64 = runs
            .iter()
            .flat_map(|r| r.streams.values())
            .filter_map(|rows| rows.last())
            .map(|(_, d)| int(d, "reconnects"))
            .sum();
        let last = &runs[runs.len() - 1];
        let rows: Vec<(&str, String, &str)> = vec![
            (
                "runs / supervisor restarts",
                format!("{} / {}", runs.len(), fmt(restarts, 0)),
                "1 / 0",
            ),
            (
                "gaps",
                format!("{gaps} gap lines, {reconnects} reconnects"),
                "every gap explained by a logged cause",
            ),
            ("write amplification", fmt(amplification, 4), "<= 1.01"),
            (
                "cpu mean % of one core",
                fmt(mean(&col(&process, "cpu_pct")), 2),
                "<= 2 at 56 Mbps (budget), measured at the soak's bitrate",
            ),
            (
                "rss max MiB",
                fmt(col_max(&process, "rss_mib"), 1),
                "<= 64 and flat",
            ),
            (
                "rss slope MiB/day (last run)",
                fmt(slope_per_day(&last.process, "rss_mib"), 2),
                "~0",
            ),
            (
                "loop lag max ms",
                fmt(col_max(&process, "loop_lag_max_ms"), 0),
                "well under the 60 s watchdog",
            ),
            (
                "retention pass max ms",
                fmt(col_max(&process, "retention_max_ms"), 0),
                "bounded",
            ),
            (
                "catalog txn p99 max ms",
                fmt(col_max(&catalog, "txn_p99_ms"), 1),
                "< 10",
            ),
            (
                "event commit p99 max ms",
                fmt(col_max(&catalog, "event_commit_p99_ms"), 1),
                "< 1000",
            ),
            (
                "writer dropped / errors / slow",
                format!(
                    "{} / {} / {}",
                    sum_of_maxes(&runs, |r| &r.writer, "dropped") as i64,
                    sum_of_maxes(&runs, |r| &r.writer, "write_errors") as i64,
                    sum_of_maxes(&runs, |r| &r.writer, "slow_io") as i64
                ),
                "0 / 0 / few",
            ),
            (
                "sync max ms",
                fmt(col_max(&writer, "sync_max_ms"), 1),
                "absorbed by the queue",
            ),
        ];
        for (name, value, target) in rows {
            writeln!(out, "   {name:34} {value:>40}   target: {target}")?;
        }
        Ok(())
    })();
    match result {
        Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => Ok(()),
        other => other.context("writing the report"),
    }
}
