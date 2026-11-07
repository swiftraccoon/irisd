//! `soak-report`: the soak exit-criteria figures from a sampler directory.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::Context;
use clap::Parser;
use serde_json::Value;

use crate::logs::{
    col, col_max, fmt, growth, kv, mean, median, number, rates, root_identity, split, strip_ansi,
};
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

/// One observed service instance, including startup attempts that do not reconcile.
#[derive(Debug, Default)]
struct Run {
    start: f64,
    /// The service instance began before the selected report window.
    started_before_window: bool,
    /// Last observed service line; bounds an unclean stop before the next start.
    last_seen: f64,
    stop: Option<f64>,
    restarted: bool,
    clean: bool,
    startup: Option<Startup>,
    recovered: Option<BTreeMap<String, String>>,
    recovered_at: Option<f64>,
    streams: BTreeMap<String, Vec<Row>>,
    events: BTreeMap<(String, String), Vec<Row>>,
    writer: Vec<Row>,
    catalog: Vec<Row>,
    process: Vec<Row>,
    roots: BTreeMap<String, Vec<Row>>,
    /// Notable lines by kind: `gap`, `selfcheck`, `retention`, `slow`.
    notable: BTreeMap<&'static str, Vec<(f64, String, String)>>,
    problems: Vec<(f64, String, String)>,
}

/// Ordered, once-per-instance startup stages; root failures can repeat per root.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
enum Startup {
    Roots,
    Sandbox,
    Catalog,
}

impl Run {
    fn has_runtime(&self) -> bool {
        self.recovered.is_some()
            || !self.streams.is_empty()
            || !self.events.is_empty()
            || !self.process.is_empty()
            || !self.writer.is_empty()
            || !self.catalog.is_empty()
            || !self.roots.is_empty()
            || self.clean
    }

    fn restarts_at(&self, startup: Option<Startup>, reconciled: bool) -> bool {
        if self.stop.is_some() {
            return false;
        }
        if startup.is_some() || reconciled {
            if self.has_runtime() {
                return true;
            }
            if let (Some(stage), Some(previous)) = (startup, self.startup) {
                // A sandbox decision and catalog open occur once per attempt. Several
                // unavailable roots can precede them, so repeated root failures coalesce.
                return stage < previous || (stage == previous && stage != Startup::Roots);
            }
        }
        false
    }
}

fn startup_stage(rest: &str) -> Option<Startup> {
    if rest.starts_with("recording root unavailable at startup ") {
        Some(Startup::Roots)
    } else if matches!(
        rest,
        "sandbox disabled by --no-sandbox"
            | "sandbox fully enforced"
            | "sandbox partially enforced (kernel lacks some Landlock features)"
            | "sandbox not enforced: kernel lacks Landlock"
    ) {
        Some(Startup::Sandbox)
    } else if rest.starts_with("catalog open ") {
        Some(Startup::Catalog)
    } else {
        None
    }
}

#[allow(clippy::too_many_lines)]
fn parse_journal(path: &Path, since: f64, until: f64) -> anyhow::Result<Vec<Run>> {
    let text =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let mut runs: Vec<Run> = Vec::new();
    for (line_no, raw) in text.lines().enumerate() {
        let line = strip_ansi(raw);
        let Some(l) = split(&line) else { continue };
        let Some(t) = parse_seconds(l.ts) else {
            anyhow::bail!(
                "{} line {} has an invalid timestamp",
                path.display(),
                line_no + 1
            );
        };
        let kind = l.rest.split(' ').next().unwrap_or("");
        let startup = startup_stage(l.rest);
        let reconciled = l.rest.starts_with("catalog reconciled");
        let restarted = runs
            .last()
            .is_some_and(|r| r.restarts_at(startup, reconciled));
        if restarted && let Some(previous) = runs.last_mut() {
            previous.restarted = true;
        }
        if restarted || runs.last().is_none_or(|r| r.stop.is_some()) {
            runs.push(Run {
                start: t,
                ..Run::default()
            });
        }
        let run = runs.last_mut().expect("pushed above");
        run.last_seen = t;
        if let Some(stage) = startup {
            run.startup = Some(stage);
        }
        if reconciled {
            run.recovered = Some(kv(l.rest));
            run.recovered_at = Some(t);
        } else if l.rest.starts_with("SIGTERM") || l.rest.starts_with("interrupted") {
            run.clean = true;
        } else if l.rest == "stopped" {
            run.stop = Some(t);
        } else if startup.is_none()
            && l.level == "INFO"
            && STATS.contains(&kind)
            && l.rest.contains('=')
        {
            let mut d = kv(l.rest);
            if kind == "root"
                && let Some(root) = root_identity(l.rest)
            {
                d.insert("root".into(), root);
            }
            match kind {
                "stream" if d.get("stream").is_some_and(|value| !value.is_empty()) => run
                    .streams
                    .entry(d["stream"].clone())
                    .or_default()
                    .push((t, d)),
                "events"
                    if d.get("camera").is_some_and(|value| !value.is_empty())
                        && d.get("source").is_some_and(|value| !value.is_empty()) =>
                {
                    run.events
                        .entry((d["camera"].clone(), d["source"].clone()))
                        .or_default()
                        .push((t, d));
                }
                "root" if d.get("root").is_some_and(|value| !value.is_empty()) => {
                    run.roots.entry(d["root"].clone()).or_default().push((t, d));
                }
                "writer" if d.contains_key("fragments") => run.writer.push((t, d)),
                "catalog" if d.contains_key("segments") && d.contains_key("gib") => {
                    run.catalog.push((t, d));
                }
                "process" => run.process.push((t, d)),
                _ => run.problems.push((
                    t,
                    "ERROR".to_owned(),
                    format!("malformed {kind} statistics row"),
                )),
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
    runs.retain(|run| run.start <= until && run.last_seen >= since);
    for run in &mut runs {
        run.started_before_window = run.start < since;
        run.start = run.start.max(since);
        run.last_seen = run.last_seen.min(until);
        if run.stop.is_some_and(|stop| stop > until) {
            run.stop = None;
            run.clean = false;
        }
        if run
            .recovered_at
            .is_some_and(|timestamp| timestamp < since || timestamp > until)
        {
            run.recovered = None;
            run.recovered_at = None;
        }
        clip_rows(&mut run.writer, since, until);
        clip_rows(&mut run.catalog, since, until);
        clip_rows(&mut run.process, since, until);
        for rows in run.streams.values_mut() {
            clip_rows(rows, since, until);
        }
        for rows in run.events.values_mut() {
            clip_rows(rows, since, until);
        }
        for rows in run.roots.values_mut() {
            clip_rows(rows, since, until);
        }
        for rows in run.notable.values_mut() {
            rows.retain(|(t, _, _)| since <= *t && *t <= until);
        }
        run.problems.retain(|(t, _, _)| since <= *t && *t <= until);
    }
    Ok(runs)
}

fn clip_rows(rows: &mut Vec<Row>, since: f64, until: f64) {
    let baseline = rows
        .iter()
        .rev()
        .find(|(t, _)| *t < since)
        .map(|(_, fields)| (since, fields.clone()));
    rows.retain(|(t, _)| since <= *t && *t <= until);
    if let Some(baseline) = baseline {
        rows.insert(0, baseline);
    }
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

fn counter(fields: &BTreeMap<String, String>, key: &str) -> Option<u64> {
    fields.get(key)?.parse::<u64>().ok()
}

fn counter_growth(rows: &[Row], include_initial: bool, key: &str) -> Option<u64> {
    let values = rows
        .iter()
        .map(|(_, fields)| counter(fields, key))
        .collect::<Option<Vec<_>>>()?;
    let mut total = if include_initial {
        values.first().copied().unwrap_or(0)
    } else {
        0
    };
    for pair in values.windows(2) {
        if pair[1] >= pair[0] {
            total = total.checked_add(pair[1] - pair[0])?;
        }
    }
    Some(total)
}

fn sum_of_maxes(runs: &[Run], pick: impl Fn(&Run) -> &Vec<Row>, key: &str) -> Option<u64> {
    runs.iter().try_fold(0u64, |total, run| {
        total.checked_add(counter_growth(pick(run), !run.started_before_window, key)?)
    })
}

fn display_counter(value: Option<u64>) -> String {
    value.map_or_else(|| "unknown".to_owned(), |value| value.to_string())
}

fn true_sample_count(rows: &[Row], key: &str) -> Option<u64> {
    rows.iter().try_fold(0u64, |total, (_, fields)| {
        let value = fields.get(key)?.parse::<bool>().ok()?;
        total.checked_add(u64::from(value))
    })
}

fn add_counter_sum(
    sums: &mut BTreeMap<&'static str, Option<u64>>,
    key: &'static str,
    value: Option<u64>,
) {
    let entry = sums.entry(key).or_insert(Some(0));
    *entry = entry.and_then(|total| value.and_then(|value| total.checked_add(value)));
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
        let end = run.stop.unwrap_or(run.last_seen);
        let hours = (end - run.start) / 3_600.0;
        let state = match (run.clean, run.stop, run.restarted) {
            (true, Some(_), _) => "stopped cleanly",
            (_, Some(_), _) => "stopped",
            (_, _, true) => "restarted without a stopped line",
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
            let gap = (run.start - previous.stop.unwrap_or(previous.last_seen)) / 60.0;
            let label = if previous.stop.is_some() {
                "downtime"
            } else {
                "unobserved interval (includes possible downtime)"
            };
            writeln!(out, "   {label} before this run: {gap:.1} min")?;
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
        fmt(mean(&col(&rows, "cpu_user_pct")), 2),
        fmt(mean(&col(&rows, "cpu_sys_pct")), 2)
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
        display_counter(sum_of_maxes(runs, |r| &r.process, "watchdog_withheld")),
        fmt(min_of(&col(&rows, "fds")), 0),
        fmt(col_max(&rows, "fds"), 0),
        fmt(col_max(&rows, "threads"), 0),
        display_counter(sum_of_maxes(runs, |r| &r.process, "selfcheck_ok")),
        display_counter(sum_of_maxes(runs, |r| &r.process, "selfcheck_failed"))
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
        .map(|k| {
            format!(
                "{k}={}",
                display_counter(sum_of_maxes(runs, |r| &r.writer, k))
            )
        })
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
        let items: Vec<String> = [
            "segments",
            "gib",
            "events",
            "event_mib",
            "retained_events",
            "pruned_events",
            "db_mib",
            "wal_mib",
            "queued",
            "queue_mib",
            "reconcile_required",
        ]
        .iter()
        .map(|k| format!("{k}={}", last.get(*k).map_or("-", String::as_str)))
        .collect();
        writeln!(out, "last: {}", items.join(" "))?;
        writeln!(
            out,
            "txn_p99_max_ms={} txn_max_ms={} query_max_ms={} wait_max_ms={} event_commit_p99_max_ms={} event_commit_max_ms={} batch_peak={} queue_peak={} queue_mib_peak={} coalesced={} rejected={} rejected_events={} rejected_progress={} rejected_queries={} rejected_critical={} failures={}",
            fmt(col_max(&rows, "txn_p99_ms"), 1),
            fmt(col_max(&rows, "txn_max_ms"), 1),
            fmt(col_max(&rows, "query_max_ms"), 1),
            fmt(col_max(&rows, "wait_max_ms"), 1),
            fmt(col_max(&rows, "event_commit_p99_ms"), 1),
            fmt(col_max(&rows, "event_commit_max_ms"), 1),
            fmt(col_max(&rows, "batch_peak"), 0),
            fmt(col_max(&rows, "queue_peak"), 0),
            fmt(col_max(&rows, "queue_mib_peak"), 1),
            display_counter(sum_of_maxes(runs, |r| &r.catalog, "coalesced")),
            display_counter(sum_of_maxes(runs, |r| &r.catalog, "rejected")),
            display_counter(sum_of_maxes(runs, |r| &r.catalog, "rejected_events")),
            display_counter(sum_of_maxes(runs, |r| &r.catalog, "rejected_progress")),
            display_counter(sum_of_maxes(runs, |r| &r.catalog, "rejected_queries")),
            display_counter(sum_of_maxes(runs, |r| &r.catalog, "rejected_critical")),
            display_counter(sum_of_maxes(runs, |r| &r.catalog, "failures"))
        )?;
    }
    Ok(())
}

#[allow(clippy::too_many_lines)]
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
        let mut sums: BTreeMap<&'static str, Option<u64>> = BTreeMap::new();
        let mut hold_s = 0.0;
        let (mut offsets, mut idle, mut frag) = (Vec::new(), Vec::new(), Vec::new());
        for run in runs {
            let Some(rows) = run.streams.get(name) else {
                continue;
            };
            all_rates.extend(rates(rows));
            for k in ["reconnects", "gaps", "holds", "snaps", "loss"] {
                add_counter_sum(
                    &mut sums,
                    k,
                    counter_growth(rows, !run.started_before_window, k),
                );
            }
            let hold_values = rows
                .iter()
                .filter_map(|(_, fields)| number(fields, "hold_s"))
                .collect::<Vec<_>>();
            if !run.started_before_window {
                hold_s += hold_values.first().copied().unwrap_or(0.0);
            }
            hold_s += growth(&hold_values);
            offsets.extend(col(rows, "cam_offset_ms"));
            idle.extend(col(rows, "idle_s"));
            frag.extend(col(rows, "frag_kib_max"));
        }
        let positive = all_rates
            .iter()
            .copied()
            .filter(|rate| *rate > 0.0)
            .collect::<Vec<_>>();
        let med = median(&positive).unwrap_or(0.0);
        let stalls = all_rates
            .iter()
            .filter(|&&rate| rate <= 0.0 || (med > 0.0 && rate < 0.5 * med))
            .count();
        writeln!(
            out,
            "{name:16} {med:7.2} {:7.2} {stalls:6} {:6} {:4} {:5} {hold_s:6.1} {:5} {:4}  {:>5}/{:<5} {:>8} {:>12}",
            min_of(&all_rates).unwrap_or(0.0),
            display_counter(sums.get("reconnects").copied().flatten()),
            display_counter(sums.get("gaps").copied().flatten()),
            display_counter(sums.get("holds").copied().flatten()),
            display_counter(sums.get("snaps").copied().flatten()),
            display_counter(sums.get("loss").copied().flatten()),
            fmt(min_of(&offsets), 0),
            fmt(max_of(&offsets), 0),
            fmt(max_of(&idle), 1),
            fmt(max_of(&frag), 0)
        )?;
    }
    writeln!(out, "\n== event feeds")?;
    writeln!(
        out,
        "camera       source             events messages reconn renewals errors idle_max pull_p99_max pull_max"
    )?;
    let mut feeds: Vec<&(String, String)> = runs.iter().flat_map(|r| r.events.keys()).collect();
    feeds.sort();
    feeds.dedup();
    for feed in feeds {
        let mut sums: BTreeMap<&'static str, Option<u64>> = BTreeMap::new();
        let (mut idle, mut pull, mut pull_max) = (Vec::new(), Vec::new(), Vec::new());
        for run in runs {
            let Some(rows) = run.events.get(feed) else {
                continue;
            };
            for k in ["events", "messages", "reconnects", "renewals", "errors"] {
                add_counter_sum(
                    &mut sums,
                    k,
                    counter_growth(rows, !run.started_before_window, k),
                );
            }
            idle.extend(col(rows, "idle_s"));
            pull.extend(col(rows, "pull_p99_ms"));
            pull_max.extend(col(rows, "pull_max_ms"));
        }
        writeln!(
            out,
            "{:12} {:18} {:6} {:8} {:6} {:8} {:6} {:>8} {:>12} {:>8}",
            feed.0,
            feed.1,
            display_counter(sums.get("events").copied().flatten()),
            display_counter(sums.get("messages").copied().flatten()),
            display_counter(sums.get("reconnects").copied().flatten()),
            display_counter(sums.get("renewals").copied().flatten()),
            display_counter(sums.get("errors").copied().flatten()),
            fmt(max_of(&idle), 1),
            fmt(max_of(&pull), 1),
            fmt(max_of(&pull_max), 1)
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
    let failures = passes
        .iter()
        .filter(|(_, _, rest)| rest.starts_with("retention pass failed"))
        .count();
    let successful = passes
        .iter()
        .filter(|(_, _, rest)| !rest.starts_with("retention pass failed"))
        .copied()
        .collect::<Vec<_>>();
    writeln!(
        out,
        "\n== retention passes logged ({} successful, {failures} failed)",
        successful.len()
    )?;
    if !successful.is_empty() {
        let ds: Vec<BTreeMap<String, String>> =
            successful.iter().map(|(_, _, rest)| kv(rest)).collect();
        let sum = |key: &str| {
            ds.iter().try_fold(0u64, |total, fields| {
                total.checked_add(counter(fields, key)?)
            })
        };
        let deleted = sum("deleted");
        let freed = sum("freed_mib");
        let ms: Vec<f64> = ds.iter().map(|d| number(d, "ms").unwrap_or(0.0)).collect();
        let free: Vec<f64> = ds.iter().filter_map(|d| number(d, "free_pct")).collect();
        let total: Vec<f64> = ds.iter().filter_map(|d| number(d, "total_gib")).collect();
        writeln!(
            out,
            "deleted={} freed_gib={} ms max={:.0} mean={:.0} free_pct min={} total_gib min={} max={} quota_gib={} slow={}",
            display_counter(deleted),
            fmt(freed.map(|value| value as f64 / 1024.0), 1),
            max_of(&ms).unwrap_or(0.0),
            mean(&ms).unwrap_or(0.0),
            fmt(min_of(&free), 1),
            fmt(min_of(&total), 2),
            fmt(max_of(&total), 2),
            ds.last()
                .and_then(|d| d.get("quota_gib").cloned())
                .unwrap_or_else(|| "-".into()),
            successful
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

struct LoadedSamples {
    rows: Vec<Sample>,
    completeness: &'static str,
}

fn load_jsonl(path: &Path, since: f64, until: f64) -> anyhow::Result<LoadedSamples> {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(LoadedSamples {
                rows: Vec::new(),
                completeness: "missing",
            });
        }
        Err(error) => return Err(error).with_context(|| format!("reading {}", path.display())),
    };
    let complete = bytes.is_empty() || bytes.ends_with(b"\n");
    let usable = if complete {
        bytes.as_slice()
    } else {
        let end = bytes
            .iter()
            .rposition(|byte| *byte == b'\n')
            .map_or(0, |i| i + 1);
        &bytes[..end]
    };
    let text = std::str::from_utf8(usable)
        .with_context(|| format!("{} contains invalid UTF-8", path.display()))?;
    let mut rows = Vec::new();
    for (line_no, line) in text.lines().enumerate() {
        anyhow::ensure!(
            !line.trim().is_empty(),
            "{} line {} is empty",
            path.display(),
            line_no + 1
        );
        let value: Value = serde_json::from_str(line)
            .with_context(|| format!("decoding {} line {}", path.display(), line_no + 1))?;
        let timestamp = value
            .get("ts")
            .and_then(Value::as_str)
            .and_then(parse_seconds)
            .with_context(|| {
                format!("{} line {} has an invalid ts", path.display(), line_no + 1)
            })?;
        if since <= timestamp && timestamp <= until {
            rows.push((timestamp, value));
        }
    }
    Ok(LoadedSamples {
        rows,
        completeness: if complete {
            "complete"
        } else {
            "incomplete final record ignored"
        },
    })
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
    v.get(key)
        .and_then(Value::as_f64)
        .filter(|value| value.is_finite())
        .unwrap_or(0.0)
}

fn sample_identity(sample: &Value) -> Option<String> {
    let unit = sample.get("unit")?;
    Some(format!(
        "{}:{}",
        unit.get("MainPID")?.as_u64()?,
        unit.get("ExecMainStartTimestamp")?.as_str()?
    ))
}

fn segmented_json_growth(points: &[(f64, String, &Value)], key: &str) -> Option<(f64, f64)> {
    let mut total = 0u64;
    let mut wall = 0.0;
    let mut edges = 0usize;
    for pair in points.windows(2) {
        let (a_t, a_id, a) = &pair[0];
        let (b_t, b_id, b) = &pair[1];
        let dt = b_t - a_t;
        let (Some(a), Some(b)) = (
            a.get(key).and_then(Value::as_u64),
            b.get(key).and_then(Value::as_u64),
        ) else {
            continue;
        };
        if a_id == b_id && dt > 0.0 && dt <= 180.0 && b >= a {
            total = total.checked_add(b - a)?;
            wall += dt;
            edges += 1;
        }
    }
    (edges > 0).then_some((total as f64, wall))
}

fn section<'a>(v: &'a Value, key: &str) -> Option<&'a Value> {
    v.get(key)
        .filter(|s| s.is_object() && s.get("error").is_none())
}

#[allow(clippy::too_many_lines)]
fn report_host(
    samples: &[Sample],
    completeness: &str,
    runs: &[Run],
    out: &mut impl Write,
) -> std::io::Result<(Option<f64>, Option<f64>)> {
    writeln!(out, "\n== host samples ({}, {completeness})", samples.len())?;
    if samples.is_empty() {
        return Ok((None, None));
    }
    let restart_values = samples
        .iter()
        .filter_map(|(_, d)| d.get("unit")?.get("NRestarts")?.as_f64())
        .filter(|value| value.is_finite() && *value >= 0.0)
        .collect::<Vec<_>>();
    let restarts = max_of(&restart_values);
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
    let cg: Vec<(f64, String, &Value)> = active
        .iter()
        .filter_map(|(t, d)| Some((*t, sample_identity(d)?, section(d, "cgroup")?)))
        .collect();
    let user = segmented_json_growth(&cg, "cpu_user_us");
    let system = segmented_json_growth(&cg, "cpu_system_us");
    let wbytes = segmented_json_growth(&cg, "io_wbytes").map(|(value, _)| value);
    let rbytes = segmented_json_growth(&cg, "io_rbytes").map(|(value, _)| value);
    let anon = cg.iter().map(|(_, _, c)| f(c, "anon")).fold(0.0, f64::max) / 1_048_576.0;
    let page_cache = cg.iter().map(|(_, _, c)| f(c, "file")).fold(0.0, f64::max) / 1_048_576.0;
    if let (Some((user, wall)), Some((system, system_wall))) = (user, system) {
        let wall = wall.min(system_wall);
        writeln!(
            out,
            "cgroup cpu: user {:.2} % + system {:.2} % of one core; anon max {anon:.1} MiB, page cache max {page_cache:.0} MiB",
            user / 1e6 / wall * 100.0,
            system / 1e6 / wall * 100.0
        )?;
    } else {
        writeln!(
            out,
            "cgroup cpu: unmeasured (need two compatible active samples)"
        )?;
    }
    let (since, until) = match (cg.first(), cg.last()) {
        (Some(a), Some(b)) => (a.0, b.0),
        _ => (0.0, f64::INFINITY),
    };
    let payload = payload_bytes(runs, since, until);
    let files = file_bytes(runs, since, until);
    let amplification = (payload > 0.0)
        .then_some(wbytes)
        .flatten()
        .map(|bytes| bytes / payload);
    let overhead = (payload > 0.0).then(|| files / payload);
    let gib = f64::from(1u32 << 30);
    writeln!(
        out,
        "bytes: payload {:.1} GiB, files {:.1} GiB, cgroup written {} GiB, cgroup read {} GiB -> container overhead {}, write amplification {}",
        payload / gib,
        files / gib,
        wbytes.map_or_else(
            || "unmeasured".to_owned(),
            |bytes| format!("{:.1}", bytes / gib)
        ),
        rbytes.map_or_else(
            || "unmeasured".to_owned(),
            |bytes| format!("{:.2}", bytes / gib)
        ),
        fmt(overhead, 4),
        fmt(amplification, 4)
    )?;
    let disk: Vec<(f64, String, &Value)> = samples
        .iter()
        .filter_map(|(t, d)| Some((*t, "disk".to_owned(), section(d, "disk")?)))
        .collect();
    if let (Some((sectors, _)), Some((weighted, _))) = (
        segmented_json_growth(&disk, "sectors_written"),
        segmented_json_growth(&disk, "weighted_io_ms"),
    ) {
        let sectors = sectors * 512.0;
        let weighted = weighted / 1000.0;
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
    let nic: Vec<(f64, String, &Value)> = samples
        .iter()
        .filter_map(|(t, d)| Some((*t, "nic".to_owned(), section(d, "nic")?)))
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
                segmented_json_growth(&nic, k)
                    .map_or_else(|| "unknown".to_owned(), |(value, _)| format!("{value:.0}"))
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
    let proc: Vec<(f64, String, &Value)> = active
        .iter()
        .filter_map(|(t, d)| Some((*t, sample_identity(d)?, section(d, "proc")?)))
        .collect();
    if !proc.is_empty() {
        writeln!(
            out,
            "proc VmHWM max {:.1} MiB, majflt growth {}, nonvoluntary switches growth {}",
            proc.iter()
                .map(|(_, _, p)| f(p, "VmHWM"))
                .fold(0.0, f64::max)
                / 1024.0,
            segmented_json_growth(&proc, "majflt")
                .map_or_else(|| "unknown".to_owned(), |(value, _)| format!("{value:.0}")),
            segmented_json_growth(&proc, "nonvoluntary_ctxt_switches")
                .map_or_else(|| "unknown".to_owned(), |(value, _)| format!("{value:.0}"))
        )?;
    }
    let load: Vec<f64> = samples
        .iter()
        .filter_map(|(_, d)| d.get("host")?.get("load1")?.as_f64())
        .collect();
    if let Some(m) = max_of(&load) {
        writeln!(out, "load1 max {m:.1}")?;
    }
    Ok((amplification, restarts))
}

fn status_of(v: &Value, key: &str) -> i64 {
    v.get(key).and_then(Value::as_i64).unwrap_or(0)
}

fn report_api(
    checks: &[Sample],
    completeness: &str,
    runs: &[Run],
    out: &mut impl Write,
) -> std::io::Result<usize> {
    writeln!(out, "\n== api checks ({}, {completeness})", checks.len())?;
    let mut bad: Vec<(f64, String, String)> = Vec::new();
    if checks.is_empty() || completeness != "complete" {
        bad.push((
            0.0,
            "series".into(),
            format!(
                "API check series is {completeness} and has {} rows",
                checks.len()
            ),
        ));
    }
    for (t, d) in checks {
        let after_restart = runs.iter().any(|r| t - 660.0 <= r.start && r.start <= *t);
        if let Some(error) = d.get("error") {
            bad.push((*t, "check".into(), error.to_string()));
            continue;
        }
        let health = d.get("health").cloned().unwrap_or(Value::Null);
        if status_of(&health, "status") != 200 {
            bad.push((*t, "health".into(), health.to_string()));
        }
        let metrics = d.get("metrics").cloned().unwrap_or(Value::Null);
        if status_of(&metrics, "status") != 200 {
            bad.push((*t, "metrics".into(), metrics.to_string()));
        } else if let Some(values) = metrics.get("values").and_then(Value::as_object) {
            if !values.contains_key("irisd_catalog_batch_failures_total") {
                bad.push((
                    *t,
                    "metrics".into(),
                    "missing catalog failure counter".into(),
                ));
            }
            for (name, value) in values {
                if value.as_f64().is_none_or(|value| value != 0.0) {
                    bad.push((*t, name.clone(), value.to_string()));
                }
            }
        } else {
            bad.push((*t, "metrics".into(), "missing metrics values".into()));
        }
        let cameras = d.get("cameras").cloned().unwrap_or(Value::Null);
        if status_of(&cameras, "status") != 200 {
            bad.push((*t, "cameras".into(), cameras.to_string()));
        }
        let expected_cameras = cameras.get("count").and_then(Value::as_u64).unwrap_or(0);
        if expected_cameras == 0 {
            bad.push((*t, "cameras".into(), "camera discovery is empty".into()));
        }
        let expected_streams = cameras
            .get("expected_streams")
            .and_then(Value::as_u64)
            .unwrap_or(expected_cameras);
        if let Some(streams) = d.get("streams").and_then(Value::as_object) {
            if u64::try_from(streams.len()).ok() != Some(expected_streams) {
                bad.push((
                    *t,
                    "streams".into(),
                    format!(
                        "{} stream checks, expected {expected_streams}",
                        streams.len(),
                    ),
                ));
            }
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
                } else if !p.is_object()
                    || f(&p, "media_segments") < 1.0
                    || f(&p, "maps") < 1.0
                    || f(&p, "range_breaks") > 0.0
                    || f(&p, "gaps") > 0.0
                    || (f(&p, "discontinuities") > 0.0 && !after_restart)
                    || (f(&p, "duration_s") < 540.0 && !after_restart)
                {
                    bad.push((*t, name.clone(), p.to_string()));
                }
            }
        } else {
            bad.push((*t, "streams".into(), "missing streams object".into()));
        }
    }
    writeln!(out, "problems: {}", bad.len())?;
    for (t, name, info) in bad.iter().take(30) {
        let timestamp = if *t == 0.0 { "-".to_owned() } else { iso(*t) };
        writeln!(out, "   {timestamp} {name} {}", truncate(info, 160))?;
    }
    Ok(bad.len())
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
    let host_samples = load_jsonl(&args.dir.join("host.jsonl"), since, until)?;
    let api_samples = load_jsonl(&args.dir.join("api.jsonl"), since, until)?;
    let stdout = std::io::stdout();
    let mut out = std::io::BufWriter::new(stdout.lock());
    let result = (|| -> std::io::Result<()> {
        report_runs(&runs, &mut out)?;
        report_process(&runs, &mut out)?;
        report_writer_catalog(&runs, &mut out)?;
        report_streams(&runs, &mut out)?;
        report_notable(&runs, &mut out)?;
        let (amplification, restarts) = report_host(
            &host_samples.rows,
            host_samples.completeness,
            &runs,
            &mut out,
        )?;
        let api_problems =
            report_api(&api_samples.rows, api_samples.completeness, &runs, &mut out)?;

        writeln!(out, "\n== criteria")?;
        let process = all(&runs, |r| &r.process);
        let writer = all(&runs, |r| &r.writer);
        let catalog = all(&runs, |r| &r.catalog);
        let gaps = notable(&runs, "gap").len();
        let reconnects = runs.iter().try_fold(0u64, |total, run| {
            run.streams.values().try_fold(total, |total, rows| {
                total.checked_add(counter_growth(
                    rows,
                    !run.started_before_window,
                    "reconnects",
                )?)
            })
        });
        let stream_drops = runs.iter().try_fold(0u64, |total, run| {
            run.streams.values().try_fold(total, |total, rows| {
                total.checked_add(counter_growth(rows, !run.started_before_window, "dropped")?)
            })
        });
        let feed_errors = runs.iter().try_fold(0u64, |total, run| {
            run.events.values().try_fold(total, |total, rows| {
                total.checked_add(counter_growth(rows, !run.started_before_window, "errors")?)
            })
        });
        let disconnected_stream_samples = runs
            .iter()
            .flat_map(|run| run.streams.values().flatten())
            .filter(|(_, fields)| fields.get("connected").is_some_and(|v| v == "false"))
            .count();
        let disconnected_feed_samples = runs
            .iter()
            .flat_map(|run| run.events.values().flatten())
            .filter(|(_, fields)| fields.get("connected").is_some_and(|v| v == "false"))
            .count();
        let retention_failures = notable(&runs, "retention")
            .iter()
            .filter(|(_, _, rest)| rest.starts_with("retention pass failed"))
            .count();
        let journal_problems = runs.iter().map(|run| run.problems.len()).sum::<usize>();
        let last = &runs[runs.len() - 1];
        let rows: Vec<(&str, String, &str)> = vec![
            (
                "runs / supervisor restarts",
                format!("{} / {}", runs.len(), fmt(restarts, 0)),
                "1 / 0",
            ),
            (
                "gaps",
                format!(
                    "{gaps} gap lines, {} reconnects",
                    display_counter(reconnects)
                ),
                "every gap explained by a logged cause",
            ),
            (
                "stream drops / disconnected samples",
                format!(
                    "{} / {disconnected_stream_samples}",
                    display_counter(stream_drops)
                ),
                "0 / 0",
            ),
            (
                "feed errors / disconnected samples",
                format!(
                    "{} / {disconnected_feed_samples}",
                    display_counter(feed_errors)
                ),
                "0 / 0",
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
            ("retention failures", retention_failures.to_string(), "0"),
            (
                "catalog failures / rejected",
                format!(
                    "{} / {}",
                    display_counter(sum_of_maxes(&runs, |r| &r.catalog, "failures")),
                    display_counter(sum_of_maxes(&runs, |r| &r.catalog, "rejected"))
                ),
                "0 / 0",
            ),
            (
                "catalog reconciliation required",
                display_counter(true_sample_count(&catalog, "reconcile_required")),
                "0",
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
                    display_counter(sum_of_maxes(&runs, |r| &r.writer, "dropped")),
                    display_counter(sum_of_maxes(&runs, |r| &r.writer, "write_errors")),
                    display_counter(sum_of_maxes(&runs, |r| &r.writer, "slow_io"))
                ),
                "0 / 0 / few",
            ),
            (
                "sync max ms",
                fmt(col_max(&writer, "sync_max_ms"), 1),
                "absorbed by the queue",
            ),
            (
                "watchdog withheld / selfcheck failed / skipped",
                format!(
                    "{} / {} / {}",
                    display_counter(sum_of_maxes(&runs, |r| &r.process, "watchdog_withheld")),
                    display_counter(sum_of_maxes(&runs, |r| &r.process, "selfcheck_failed")),
                    display_counter(sum_of_maxes(&runs, |r| &r.process, "selfcheck_skipped"))
                ),
                "0 / 0 / 0",
            ),
            ("API check problems", api_problems.to_string(), "0"),
            (
                "WARN/ERROR journal problems",
                journal_problems.to_string(),
                "0",
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

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_fixture(text: &str, since: f64, until: f64) -> Vec<Run> {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let id = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "irisd-report-fixture-{}-{id}.log",
            std::process::id()
        ));
        std::fs::write(&path, text).unwrap();
        let runs = parse_journal(&path, since, until).unwrap();
        std::fs::remove_file(path).unwrap();
        runs
    }

    #[test]
    fn event_totals_remain_separate_by_source_across_counter_resets() {
        let runs = parse_fixture(
            concat!(
                "2026-11-01T00:00:00Z INFO events camera=cam1 source=dahua_cgi events=10 messages=11 reconnects=1 renewals=0 errors=0 idle_s=2 pull_p99_ms=0\n",
                "2026-11-01T00:00:01Z INFO events camera=cam1 source=onvif_pull_point events=2 messages=4 reconnects=3 renewals=1 errors=2 idle_s=3 pull_p99_ms=20\n",
                "2026-11-01T00:02:00Z INFO SIGTERM\n",
                "2026-11-01T00:02:01Z INFO stopped\n",
                "2026-11-01T01:00:00Z INFO events camera=cam1 source=onvif_pull_point events=5 messages=7 reconnects=1 renewals=2 errors=1 idle_s=4 pull_p99_ms=30\n",
                "2026-11-01T01:00:01Z INFO events camera=cam1 source=dahua_cgi events=3 messages=4 reconnects=0 renewals=0 errors=0 idle_s=1 pull_p99_ms=0\n",
            ),
            0.0,
            f64::INFINITY,
        );
        assert_eq!(runs.len(), 2);
        assert_eq!(runs[0].events.len(), 2);
        assert_eq!(runs[1].events.len(), 2);

        let mut output = Vec::new();
        report_streams(&runs, &mut output).unwrap();
        let output = String::from_utf8(output).unwrap();
        let dahua = output
            .lines()
            .find(|line| line.contains("dahua_cgi"))
            .unwrap();
        let onvif = output
            .lines()
            .find(|line| line.contains("onvif_pull_point"))
            .unwrap();
        assert_eq!(
            dahua.split_whitespace().collect::<Vec<_>>(),
            [
                "cam1",
                "dahua_cgi",
                "13",
                "15",
                "1",
                "0",
                "0",
                "2.0",
                "0.0",
                "-"
            ]
        );
        assert_eq!(
            onvif.split_whitespace().collect::<Vec<_>>(),
            [
                "cam1",
                "onvif_pull_point",
                "7",
                "11",
                "4",
                "3",
                "3",
                "4.0",
                "30.0",
                "-"
            ]
        );
    }

    #[test]
    fn startup_warning_belongs_to_the_restarted_process() {
        let path = std::env::temp_dir().join(format!(
            "irisd-report-startup-warning-{}.log",
            std::process::id()
        ));
        std::fs::write(
            &path,
            concat!(
                "2026-11-01T00:00:00Z INFO catalog reconciled recovered=0 adopted=0\n",
                "2026-11-01T00:01:00Z INFO process cpu_pct=1 rss_mib=2\n",
                "2026-11-01T03:00:00Z WARN sandbox disabled by --no-sandbox\n",
                "2026-11-01T03:00:10Z INFO catalog reconciled recovered=1 adopted=0\n",
                "2026-11-01T03:01:00Z INFO process cpu_pct=1 rss_mib=2\n",
            ),
        )
        .unwrap();
        let runs = parse_journal(&path, 0.0, f64::INFINITY).unwrap();
        std::fs::remove_file(path).unwrap();
        assert_eq!(runs.len(), 2);
        assert_eq!(
            runs[0].last_seen,
            parse_seconds("2026-11-01T00:01:00Z").unwrap()
        );
        assert_eq!(
            runs[1].start,
            parse_seconds("2026-11-01T03:00:00Z").unwrap()
        );
        assert_eq!(runs[0].problems, []);
        assert_eq!(runs[1].problems.len(), 1);
        let mut report = Vec::new();
        report_runs(&runs, &mut report).unwrap();
        assert!(String::from_utf8(report).unwrap().contains(
            "unobserved interval (includes possible downtime) before this run: 179.0 min"
        ));
    }

    #[test]
    fn startup_prefix_stages_stay_with_their_instance() {
        for prefix in [
            concat!(
                "2026-11-01T03:00:00Z WARN recording root unavailable at startup root=a\n",
                "2026-11-01T03:00:01Z WARN recording root unavailable at startup root=b\n",
                "2026-11-01T03:00:02Z INFO sandbox fully enforced\n",
                "2026-11-01T03:00:03Z INFO catalog open path=fixture.db\n",
            ),
            concat!(
                "2026-11-01T03:00:00Z INFO sandbox partially enforced (kernel lacks some Landlock features)\n",
                "2026-11-01T03:00:03Z INFO catalog open path=fixture.db\n",
            ),
            concat!(
                "2026-11-01T03:00:00Z WARN sandbox not enforced: kernel lacks Landlock\n",
                "2026-11-01T03:00:03Z INFO catalog open path=fixture.db\n",
            ),
            "2026-11-01T03:00:00Z INFO catalog open path=fixture.db\n",
        ] {
            let text = format!(
                "2026-11-01T00:01:00Z INFO process mib=10\n{prefix}\
                 2026-11-01T03:00:10Z INFO catalog reconciled recovered=1 adopted=0\n"
            );
            let runs = parse_fixture(&text, 0.0, f64::INFINITY);
            assert_eq!(runs.len(), 2, "{prefix}");
            assert_eq!(
                runs[0].last_seen,
                parse_seconds("2026-11-01T00:01:00Z").unwrap()
            );
            assert_eq!(
                runs[1].start,
                parse_seconds("2026-11-01T03:00:00Z").unwrap()
            );
            assert!(runs[0].restarted);
            assert_eq!(runs[1].recovered.as_ref().unwrap()["recovered"], "1");
        }
    }

    #[test]
    fn runtime_warnings_before_a_crash_remain_with_the_prior_run() {
        let runs = parse_fixture(
            concat!(
                "2026-11-01T00:01:00Z INFO process mib=10\n",
                "2026-11-01T00:02:00Z WARN recording root unavailable root=fixture\n",
                "2026-11-01T00:02:01Z WARN camera disconnected camera=fixture\n",
                "2026-11-01T03:00:00Z INFO sandbox fully enforced\n",
                "2026-11-01T03:00:10Z INFO catalog reconciled recovered=1 adopted=0\n",
            ),
            0.0,
            f64::INFINITY,
        );
        assert_eq!(runs.len(), 2);
        assert_eq!(
            runs[0].last_seen,
            parse_seconds("2026-11-01T00:02:01Z").unwrap()
        );
        assert_eq!(runs[0].problems.len(), 2);
        assert_eq!(runs[1].problems, []);
    }

    #[test]
    fn a_warning_alone_does_not_prove_a_restart() {
        let runs = parse_fixture(
            concat!(
                "2026-11-01T00:01:00Z INFO process mib=10\n",
                "2026-11-01T03:00:00Z WARN camera disconnected camera=fixture\n",
                "2026-11-01T03:01:00Z INFO process mib=20\n",
            ),
            0.0,
            f64::INFINITY,
        );
        assert_eq!(runs.len(), 1);
        assert!(!runs[0].restarted);
        assert_eq!(runs[0].process.len(), 2);
        assert_eq!(runs[0].problems.len(), 1);
    }

    #[test]
    fn a_clean_stop_keeps_its_end_before_the_next_startup() {
        let runs = parse_fixture(
            concat!(
                "2026-11-01T00:01:00Z INFO process mib=10\n",
                "2026-11-01T00:02:00Z INFO SIGTERM\n",
                "2026-11-01T00:02:01Z INFO stopped\n",
                "2026-11-01T03:00:00Z WARN sandbox disabled by --no-sandbox\n",
                "2026-11-01T03:00:10Z INFO catalog reconciled recovered=0 adopted=0\n",
            ),
            0.0,
            f64::INFINITY,
        );
        assert_eq!(runs.len(), 2);
        assert!(runs[0].clean);
        assert!(!runs[0].restarted);
        assert_eq!(runs[0].stop, parse_seconds("2026-11-01T00:02:01Z"));
        assert_eq!(runs[1].problems.len(), 1);
        let mut report = Vec::new();
        report_runs(&runs, &mut report).unwrap();
        let report = String::from_utf8(report).unwrap();
        assert!(report.contains("stopped cleanly"));
        assert!(report.contains("downtime before this run: 178.0 min"));
    }

    #[test]
    fn a_partial_window_preserves_startup_boundaries_without_reconciliation() {
        let runs = parse_fixture(
            concat!(
                "2026-11-01T00:00:00Z INFO catalog reconciled recovered=0 adopted=0\n",
                "2026-11-01T00:01:00Z INFO process mib=10\n",
                "2026-11-01T03:00:00Z WARN sandbox disabled by --no-sandbox\n",
                "2026-11-01T03:00:10Z INFO catalog reconciled recovered=1 adopted=0\n",
            ),
            parse_seconds("2026-11-01T00:00:30Z").unwrap(),
            parse_seconds("2026-11-01T03:00:05Z").unwrap(),
        );
        assert_eq!(runs.len(), 2);
        assert!(runs[0].recovered.is_none());
        assert!(runs[0].restarted);
        assert_eq!(runs[1].problems.len(), 1);
        assert!(runs[1].recovered.is_none());
    }

    #[test]
    fn repeated_startup_attempts_with_equal_timestamps_stay_separate() {
        let runs = parse_fixture(
            concat!(
                "2026-11-01T00:00:00Z WARN sandbox disabled by --no-sandbox\n",
                "2026-11-01T00:00:00Z INFO catalog open path=fixture.db\n",
                "2026-11-01T00:00:00Z WARN recording root unavailable at startup root=a\n",
                "2026-11-01T00:00:00Z WARN recording root unavailable at startup root=b\n",
                "2026-11-01T00:00:00Z WARN sandbox disabled by --no-sandbox\n",
                "2026-11-01T00:00:00Z INFO catalog open path=fixture.db\n",
                "2026-11-01T00:00:00Z INFO catalog open path=fixture.db\n",
                "2026-11-01T00:00:00Z INFO catalog reconciled recovered=1 adopted=0\n",
                "2026-11-01T00:00:00Z INFO catalog reconciled recovered=2 adopted=0\n",
            ),
            0.0,
            f64::INFINITY,
        );
        assert_eq!(runs.len(), 4);
        assert_eq!(runs[0].problems.len(), 1);
        assert_eq!(runs[1].problems.len(), 3);
        assert!(runs[..3].iter().all(|run| run.restarted));
        assert_eq!(runs[2].recovered.as_ref().unwrap()["recovered"], "1");
        assert_eq!(runs[3].recovered.as_ref().unwrap()["recovered"], "2");
        let mut report = Vec::new();
        report_runs(&runs, &mut report).unwrap();
        assert!(!String::from_utf8(report).unwrap().contains("-0.0 min"));
    }

    #[test]
    fn reconciliation_after_a_crash_starts_a_separate_run() {
        let path =
            std::env::temp_dir().join(format!("irisd-report-crash-{}.log", std::process::id()));
        std::fs::write(
            &path,
            concat!(
                "2026-11-01T00:00:00Z INFO catalog reconciled recovered=0 adopted=0\n",
                "2026-11-01T00:01:00Z INFO stream stream=fixture/main mib=10 reconnects=3\n",
                "2026-11-01T00:02:00Z INFO catalog reconciled recovered=1 adopted=0\n",
                "2026-11-01T00:03:00Z INFO stream stream=fixture/main mib=10 reconnects=0\n",
            ),
        )
        .unwrap();
        let runs = parse_journal(&path, 0.0, f64::INFINITY).unwrap();
        std::fs::remove_file(path).unwrap();
        assert_eq!(runs.len(), 2);
        assert!(runs[0].restarted);
        assert_eq!(runs[0].recovered.as_ref().unwrap()["recovered"], "0");
        assert_eq!(runs[1].recovered.as_ref().unwrap()["recovered"], "1");
        let reconnects: u64 = runs
            .iter()
            .flat_map(|r| r.streams.values())
            .filter_map(|rows| rows.last())
            .map(|(_, fields)| counter(fields, "reconnects").unwrap())
            .sum();
        assert_eq!(reconnects, 3);
        let mut report = Vec::new();
        report_runs(&runs, &mut report).unwrap();
        let report = String::from_utf8(report).unwrap();
        assert!(report.contains("restarted without a stopped line"));
        assert!(report.contains("unobserved interval (includes possible downtime)"));
    }

    #[test]
    fn a_window_starting_mid_run_still_detects_the_next_crash_restart() {
        let path =
            std::env::temp_dir().join(format!("irisd-report-partial-{}.log", std::process::id()));
        std::fs::write(
            &path,
            concat!(
                "2026-11-01T00:01:00Z INFO process mib=10\n",
                "2026-11-01T00:02:00Z INFO catalog reconciled recovered=1 adopted=0\n",
            ),
        )
        .unwrap();
        let runs = parse_journal(&path, 0.0, f64::INFINITY).unwrap();
        std::fs::remove_file(path).unwrap();
        assert_eq!(runs.len(), 2);
        assert!(runs[0].restarted);
    }

    #[test]
    fn jsonl_loader_reports_truncation_and_rejects_complete_corruption() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("samples.jsonl");
        std::fs::write(
            &path,
            b"{\"ts\":\"2026-11-01T00:00:00Z\",\"ok\":true}\n{\"ts\":",
        )
        .unwrap();
        let loaded = load_jsonl(&path, 0.0, f64::INFINITY).unwrap();
        assert_eq!(loaded.rows.len(), 1);
        assert_eq!(loaded.completeness, "incomplete final record ignored");

        std::fs::write(&path, b"{not json}\n").unwrap();
        assert!(load_jsonl(&path, 0.0, f64::INFINITY).is_err());

        let missing =
            load_jsonl(&directory.path().join("missing.jsonl"), 0.0, f64::INFINITY).unwrap();
        assert_eq!(missing.rows.len(), 0);
        assert_eq!(missing.completeness, "missing");
    }

    #[test]
    fn malformed_counters_render_unknown_instead_of_zero() {
        let runs = parse_fixture(
            concat!(
                "2026-11-01T00:00:00Z INFO stream stream=cam/main frames=1 reconnects=1 gaps=0 holds=0 snaps=0 loss=0\n",
                "2026-11-01T00:01:00Z INFO stream stream=cam/main frames=2 gaps=0 holds=0 snaps=0 loss=0\n",
            ),
            0.0,
            f64::INFINITY,
        );
        let mut output = Vec::new();
        report_streams(&runs, &mut output).unwrap();
        assert!(String::from_utf8(output).unwrap().contains("unknown"));
    }

    #[test]
    fn a_success_status_cannot_hide_a_missing_playlist_summary() {
        let timestamp = parse_seconds("2026-11-01T00:00:00Z").unwrap();
        let checks = vec![(
            timestamp,
            serde_json::json!({
                "health": { "status": 200 },
                "metrics": {
                    "status": 200,
                    "values": { "irisd_catalog_batch_failures_total": 0 }
                },
                "cameras": { "status": 200, "count": 1, "expected_streams": 1 },
                "streams": {
                    "cam/main": { "segments_status": 200, "playlist_status": 200 }
                }
            }),
        )];
        let runs = vec![Run {
            start: timestamp,
            last_seen: timestamp,
            ..Run::default()
        }];
        let mut output = Vec::new();
        report_api(&checks, "complete", &runs, &mut output).unwrap();
        let output = String::from_utf8(output).unwrap();
        assert!(output.contains("problems: 1"), "{output}");
    }

    #[test]
    fn restart_discontinuities_are_tolerated_only_while_they_remain_in_the_horizon() {
        let started = parse_seconds("2026-11-01T00:00:00Z").unwrap();
        let check = |timestamp| {
            (
                timestamp,
                serde_json::json!({
                    "health": { "status": 200 },
                    "metrics": {
                        "status": 200,
                        "values": { "irisd_catalog_batch_failures_total": 0 }
                    },
                    "cameras": { "status": 200, "count": 1, "expected_streams": 1 },
                    "streams": {
                        "cam/main": {
                            "segments_status": 200,
                            "playlist_status": 200,
                            "playlist": {
                                "media_segments": 1,
                                "maps": 1,
                                "range_breaks": 0,
                                "gaps": 0,
                                "discontinuities": 1,
                                "duration_s": 600
                            }
                        }
                    }
                }),
            )
        };
        let runs = vec![Run {
            start: started,
            last_seen: started,
            ..Run::default()
        }];
        let checks = vec![check(started + 60.0), check(started + 661.0)];
        let mut output = Vec::new();
        report_api(&checks, "complete", &runs, &mut output).unwrap();
        let output = String::from_utf8(output).unwrap();
        assert!(output.contains("problems: 1"), "{output}");
    }

    #[test]
    fn reconciliation_boolean_counts_true_samples() {
        let rows = vec![
            (
                0.0,
                BTreeMap::from([("reconcile_required".to_owned(), "false".to_owned())]),
            ),
            (
                1.0,
                BTreeMap::from([("reconcile_required".to_owned(), "true".to_owned())]),
            ),
        ];
        assert_eq!(true_sample_count(&rows, "reconcile_required"), Some(1));
        assert_eq!(true_sample_count(&rows, "missing"), None);
    }
}
