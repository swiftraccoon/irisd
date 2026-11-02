//! `daemon-stats`: the summary of one `irisd run` log.

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;

use anyhow::Context;
use clap::Parser;

use crate::logs::{col_max, fmt, kv, mean, median, number, rates, split, strip_ansi};
use crate::time::parse_seconds;

/// Row type: a timestamp in seconds and the line's fields.
pub type Row = (f64, BTreeMap<String, String>);

const STATS: [&str; 6] = ["stream", "events", "writer", "catalog", "root", "process"];
const NOTABLE: [&str; 4] = ["gap ", "selfcheck ", "retention pass", "slow "];

/// Arguments of `daemon-stats`.
#[derive(Debug, Parser)]
pub struct Args {
    /// The log: an `irisd run` capture or a `journalctl -o cat` export.
    log: PathBuf,
}

/// Everything a log holds, grouped by line kind.
#[derive(Debug, Default)]
pub struct Parsed {
    pub streams: BTreeMap<String, Vec<Row>>,
    pub events: BTreeMap<String, Vec<Row>>,
    pub writer: Vec<Row>,
    pub catalog: Vec<Row>,
    pub process: Vec<Row>,
    pub roots: BTreeMap<String, Vec<Row>>,
    pub notable: Vec<String>,
    pub other: Vec<String>,
}

/// Sorts a log's lines into the groups.
pub fn parse(lines: impl Iterator<Item = String>) -> Parsed {
    let mut p = Parsed::default();
    for raw in lines {
        let line = strip_ansi(raw.trim_end_matches('\n'));
        let Some(l) = split(&line) else {
            p.other.push(line.clone());
            continue;
        };
        let kind = l.rest.split(' ').next().unwrap_or("");
        let Some(t) = parse_seconds(l.ts) else {
            p.other.push(line.clone());
            continue;
        };
        if l.level == "INFO" && STATS.contains(&kind) && l.rest.contains('=') {
            let d = kv(l.rest);
            match kind {
                "stream" => p
                    .streams
                    .entry(d.get("stream").cloned().unwrap_or_default())
                    .or_default()
                    .push((t, d)),
                "events" => p
                    .events
                    .entry(d.get("camera").cloned().unwrap_or_default())
                    .or_default()
                    .push((t, d)),
                "root" => p
                    .roots
                    .entry(d.get("root").cloned().unwrap_or_default())
                    .or_default()
                    .push((t, d)),
                "writer" => p.writer.push((t, d)),
                "catalog" => p.catalog.push((t, d)),
                _ => p.process.push((t, d)),
            }
        } else if NOTABLE.iter().any(|prefix| l.rest.starts_with(prefix)) {
            p.notable.push(format!("{} {} {}", l.ts, l.level, l.rest));
        } else {
            p.other.push(format!("{} {} {}", l.ts, l.level, l.rest));
        }
    }
    p
}

fn get<'a>(d: &'a BTreeMap<String, String>, key: &str) -> &'a str {
    d.get(key).map_or("-", String::as_str)
}

fn pairs(d: &BTreeMap<String, String>, keys: &[&str]) -> String {
    let items: Vec<String> = keys.iter().map(|k| format!("{k}={}", get(d, k))).collect();
    items.join(" ")
}

/// Writes the summary.
#[allow(clippy::too_many_lines)]
pub fn render(p: &Parsed, out: &mut impl Write) -> std::io::Result<()> {
    if let (Some(first), Some(last)) = (p.process.first(), p.process.last()) {
        let cpu: Vec<f64> = p
            .process
            .iter()
            .filter_map(|(_, d)| number(d, "cpu_pct"))
            .collect();
        let rss: Vec<f64> = p
            .process
            .iter()
            .filter_map(|(_, d)| number(d, "rss_mib"))
            .collect();
        let span = (last.0 - first.0) / 60.0;
        writeln!(
            out,
            "process samples={} span={span:.1} min",
            p.process.len()
        )?;
        writeln!(
            out,
            "cpu_pct min={:.2} mean={:.2} max={:.2}  rss_mib min={:.1} max={:.1}  loop_lag_max_ms={}  retention_max_ms={}  fds_max={}",
            cpu.iter().copied().fold(f64::INFINITY, f64::min),
            mean(&cpu).unwrap_or(0.0),
            cpu.iter().copied().fold(f64::NEG_INFINITY, f64::max),
            rss.iter().copied().fold(f64::INFINITY, f64::min),
            rss.iter().copied().fold(f64::NEG_INFINITY, f64::max),
            fmt(col_max(&p.process, "loop_lag_max_ms"), 0),
            fmt(col_max(&p.process, "retention_max_ms"), 0),
            fmt(col_max(&p.process, "fds"), 0),
        )?;
        writeln!(
            out,
            "final process: {}",
            pairs(
                &last.1,
                &[
                    "threads",
                    "fds",
                    "watchdog_withheld",
                    "selfcheck_ok",
                    "selfcheck_failed",
                    "selfcheck_skipped"
                ]
            )
        )?;
    }
    if let Some(last) = p.writer.last() {
        writeln!(
            out,
            "writer write_p99_max_ms={} write_max_ms={} sync_p99_max_ms={} sync_max_ms={} queue_peak={} queue_mib_peak={}",
            fmt(col_max(&p.writer, "write_p99_ms"), 1),
            fmt(col_max(&p.writer, "write_max_ms"), 1),
            fmt(col_max(&p.writer, "sync_p99_ms"), 1),
            fmt(col_max(&p.writer, "sync_max_ms"), 1),
            fmt(col_max(&p.writer, "queue_peak"), 0),
            fmt(col_max(&p.writer, "queue_mib_peak"), 1),
        )?;
        writeln!(
            out,
            "final writer: {}",
            pairs(
                &last.1,
                &[
                    "fragments",
                    "mib",
                    "syncs",
                    "dropped",
                    "write_errors",
                    "files",
                    "slow_io"
                ]
            )
        )?;
    }
    if let Some(last) = p.catalog.last() {
        writeln!(
            out,
            "catalog txn_p99_max_ms={} txn_max_ms={} query_max_ms={} wait_max_ms={} event_commit_p99_max_ms={} batch_peak={}",
            fmt(col_max(&p.catalog, "txn_p99_ms"), 1),
            fmt(col_max(&p.catalog, "txn_max_ms"), 1),
            fmt(col_max(&p.catalog, "query_max_ms"), 1),
            fmt(col_max(&p.catalog, "wait_max_ms"), 1),
            fmt(col_max(&p.catalog, "event_commit_p99_ms"), 1),
            fmt(col_max(&p.catalog, "batch_peak"), 0),
        )?;
        writeln!(
            out,
            "final catalog: {}",
            pairs(
                &last.1,
                &[
                    "segments", "gib", "events", "db_mib", "wal_mib", "batches", "failures"
                ]
            )
        )?;
    }
    for (root, rows) in &p.roots {
        let free: Vec<f64> = rows
            .iter()
            .filter_map(|(_, d)| number(d, "free_pct"))
            .collect();
        if let Some(last) = rows.last() {
            writeln!(
                out,
                "root {root} free_pct min={:.1} max={:.1} last_free_gib={}",
                free.iter().copied().fold(f64::INFINITY, f64::min),
                free.iter().copied().fold(f64::NEG_INFINITY, f64::max),
                get(&last.1, "free_gib")
            )?;
        }
    }

    writeln!(
        out,
        "\nstream           fps_med fps_min fps_max stalls  frames fragments reconn loss drop holds gaps cam_ms idle_max connected"
    )?;
    for (name, rows) in &p.streams {
        let r = rates(rows);
        let Some(med) = median(&r) else { continue };
        let stalls = r.iter().filter(|&&x| x < 0.5 * med).count();
        let f = &rows.last().expect("rates imply rows").1;
        writeln!(
            out,
            "{name:16} {med:7.2} {:7.2} {:7.2} {stalls:6} {:>7} {:>9} {:>6} {:>4} {:>4} {:>5} {:>4} {:>6} {:>8} {}",
            r.iter().copied().fold(f64::INFINITY, f64::min),
            r.iter().copied().fold(f64::NEG_INFINITY, f64::max),
            get(f, "frames"),
            get(f, "fragments"),
            get(f, "reconnects"),
            get(f, "loss"),
            get(f, "dropped"),
            get(f, "holds"),
            get(f, "gaps"),
            get(f, "cam_offset_ms"),
            fmt(col_max(rows, "idle_s"), 1),
            get(f, "connected"),
        )?;
    }

    if !p.events.is_empty() {
        writeln!(
            out,
            "\nevents camera  events messages reconnects renewals errors idle_max pull_p99_max connected"
        )?;
        for (cam, rows) in &p.events {
            let f = &rows.last().expect("non-empty").1;
            writeln!(
                out,
                "{cam:13} {:>7} {:>8} {:>10} {:>8} {:>6} {:>8} {:>12} {}",
                get(f, "events"),
                get(f, "messages"),
                get(f, "reconnects"),
                get(f, "renewals"),
                get(f, "errors"),
                fmt(col_max(rows, "idle_s"), 1),
                fmt(col_max(rows, "pull_p99_ms"), 1),
                get(f, "connected"),
            )?;
        }
    }

    if !p.notable.is_empty() {
        writeln!(
            out,
            "\n--- gaps, self-checks, retention passes, slow I/O ({}) ---",
            p.notable.len()
        )?;
        for line in p.notable.iter().take(200) {
            writeln!(out, "{}", truncate(line, 220))?;
        }
        if p.notable.len() > 200 {
            writeln!(out, "... {} more", p.notable.len() - 200)?;
        }
    }

    writeln!(
        out,
        "\n--- other lines (deduplicated, first occurrence shown) ---"
    )?;
    let mut seen: Vec<(String, usize, String)> = Vec::new();
    for line in &p.other {
        let key = dedup_key(line);
        match seen.iter_mut().find(|(k, _, _)| *k == key) {
            Some(entry) => entry.1 += 1,
            None => seen.push((key, 1, line.clone())),
        }
    }
    for (_, count, first) in seen {
        writeln!(out, "{count:4}x  {}", truncate(&first, 200))?;
    }
    Ok(())
}

/// The line without its leading timestamp and with embedded timestamps masked.
fn dedup_key(line: &str) -> String {
    let body = line.split_once(' ').map_or(line, |(_, rest)| rest);
    let mut out = String::with_capacity(body.len());
    let mut rest = body;
    while let Some(pos) = find_timestamp(rest) {
        out.push_str(&rest[..pos]);
        out.push_str("<ts>");
        let after = &rest[pos..];
        let end = after.find('Z').map_or(after.len(), |i| i + 1);
        rest = &after[end..];
    }
    out.push_str(rest);
    out
}

fn find_timestamp(s: &str) -> Option<usize> {
    let b = s.as_bytes();
    (0..b.len().saturating_sub(19)).find(|&i| {
        b[i..i + 4].iter().all(u8::is_ascii_digit)
            && b[i + 4] == b'-'
            && b[i + 7] == b'-'
            && b[i + 10] == b'T'
            && b[i + 13] == b':'
    })
}

fn truncate(s: &str, max: usize) -> &str {
    match s.char_indices().nth(max) {
        Some((i, _)) => &s[..i],
        None => s,
    }
}

/// Runs the tool.
pub fn run(args: &Args) -> anyhow::Result<()> {
    let file = std::fs::File::open(&args.log)
        .with_context(|| format!("opening {}", args.log.display()))?;
    let lines = BufReader::new(file).lines().map_while(Result::ok);
    let parsed = parse(lines);
    let stdout = std::io::stdout();
    let mut out = std::io::BufWriter::new(stdout.lock());
    match render(&parsed, &mut out) {
        Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => Ok(()),
        other => other.context("writing the summary"),
    }
}
