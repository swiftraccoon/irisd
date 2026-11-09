//! `soak-report`: the soak exit-criteria figures from a sampler directory.

use std::cmp::Reverse;
use std::collections::hash_map::Entry;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fmt;
use std::fs::File;
use std::io::{self, BufReader, BufWriter, ErrorKind, Write};
use std::path::{Path, PathBuf};

use anyhow::Context;
use clap::Parser;
use serde_json::Value;

use crate::logs::{self, Fields, LineReader, Series, Statistics};
use crate::sample;
use crate::summary::{self, Row};
use crate::time;

/// The longest gap between two sampler records whose counters are compared:
/// three of the timer's one-minute intervals.
const MAX_SAMPLE_GAP_SECONDS: f64 = 180.0;
/// How long after a daemon start an API check can still see that restart's
/// discontinuity: the checked playlist window plus one sampler interval.
const RESTART_HORIZON_SECONDS: f64 = sample::PLAYLIST_WINDOW_SECONDS + 60.0;
/// The shortest playlist an API check accepts outside that horizon: the
/// checked window less one sampler interval.
const MIN_PLAYLIST_SECONDS: f64 = sample::PLAYLIST_WINDOW_SECONDS - 60.0;
/// retina's warning about RTCP from an SSRC it does not know. A test camera's
/// sender reports carry an SSRC that does not match its RTP stream, and the
/// daemon's clock is authoritative (`docs/research/11-multi-camera-results.md`),
/// so the warning is counted apart from the journal problems.
const UNKNOWN_RTCP_SSRC: &str = "unknown rtcp ssrc";
/// Bytes in a MiB, the daemon's unit for payload and file counters.
const MIB: f64 = 1_048_576.0;
/// Bytes in a GiB.
const GIB: f64 = 1_073_741_824.0;

fn rfc3339(text: &str) -> Result<f64, String> {
    time::parse_seconds(text)
        .ok_or_else(|| format!("{text:?} is not an RFC 3339 time such as 2026-01-01T00:00:00Z"))
}

/// Arguments of `soak-report`.
#[derive(Debug, Parser)]
pub struct Args {
    /// The sampler's output directory: `journal.log`, `host.jsonl`, `api.jsonl`.
    dir: PathBuf,
    /// Start of the window (RFC 3339).
    #[arg(long, value_parser = rfc3339)]
    since: Option<f64>,
    /// End of the window (RFC 3339).
    #[arg(long, value_parser = rfc3339)]
    until: Option<f64>,
}

/// An event feed: its camera and source.
type Feed = (String, String);

/// One statistics series of a run, as much of it as the window needs.
#[derive(Debug, Default)]
struct Window {
    /// The last row before `--since`: where the window's counters start.
    baseline: Option<Row>,
    /// The rows inside `[since, until]`.
    rows: Vec<Row>,
}

impl Window {
    /// Keeps a row that arrives while the journal streams past: the newest
    /// one before the window as the baseline, and every one inside it.
    fn push(&mut self, row: Row, since: f64, until: f64) {
        if row.0 < since {
            self.baseline = Some(row);
        } else if row.0 <= until {
            self.rows.push(row);
        }
    }

    /// The baseline and the rows, in order.
    fn points(&self) -> impl Iterator<Item = &Row> {
        self.baseline.iter().chain(&self.rows)
    }
}

/// A journal line kept for the report: its time, level, and text.
#[derive(Clone, Debug, PartialEq)]
struct Logged {
    at: f64,
    level: String,
    text: String,
}

/// The one-off lines the report lists.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Notable {
    Gap,
    SelfCheck,
    Retention,
    Slow,
}

/// One observed service instance, including startup attempts that do not reconcile.
#[derive(Debug, Default)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "each flag is an independent fact about the run that the report prints"
)]
struct Run {
    /// The first observed line, moved up to the window start.
    start: f64,
    /// The first observed line.
    first_seen: f64,
    /// Whether that line is a startup stage or the reconciliation, so the
    /// instance started there rather than before the journal begins.
    began_with_startup: bool,
    /// The service instance began before the selected report window.
    started_before_window: bool,
    /// Last observed service line; bounds an unclean stop before the next start.
    last_seen: f64,
    stop: Option<f64>,
    /// When the next instance's startup showed this one ended without a
    /// `stopped` line.
    restarted_at: Option<f64>,
    clean: bool,
    startup: Option<Startup>,
    recovered: Option<Fields>,
    recovered_at: Option<f64>,
    /// Whether any statistics row arrived, inside the window or not.
    has_statistics: bool,
    streams: BTreeMap<String, Window>,
    events: BTreeMap<Feed, Window>,
    writer: Window,
    catalog: Window,
    process: Window,
    roots: BTreeMap<String, Window>,
    notable: BTreeMap<Notable, Vec<Logged>>,
    problems: Vec<Logged>,
    /// [`UNKNOWN_RTCP_SSRC`] warnings in the window.
    unknown_ssrc_warnings: usize,
}

/// The daemon's ordered startup stages before reconciliation, as its log
/// shows them. A stage that does not repeat, or an earlier one, begins
/// another attempt.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
enum Startup {
    /// The warning that the configuration file is readable by others.
    ExposedConfiguration,
    /// The warning that the API accepts unauthenticated requests.
    Unauthenticated,
    /// The descriptor limit, raised or not.
    Descriptors,
    /// A state file's mode repaired; one line per file.
    StateModes,
    /// A recording root unavailable or aliased; one line per root.
    Roots,
    /// The sandbox decision.
    Sandbox,
    /// The catalog opened.
    Catalog,
}

impl Startup {
    fn repeats(self) -> bool {
        matches!(self, Self::StateModes | Self::Roots)
    }
}

impl Run {
    fn starting(at: f64, began_with_startup: bool) -> Self {
        Self {
            start: at,
            first_seen: at,
            began_with_startup,
            ..Self::default()
        }
    }

    fn has_runtime(&self) -> bool {
        self.recovered.is_some() || self.has_statistics || self.clean
    }

    fn restarts_at(&self, startup: Option<Startup>, reconciled: bool) -> bool {
        if self.stop.is_some() || (startup.is_none() && !reconciled) {
            return false;
        }
        if self.has_runtime() {
            return true;
        }
        match (startup, self.startup) {
            (Some(stage), Some(previous)) => {
                stage < previous || (stage == previous && !stage.repeats())
            }
            _ => false,
        }
    }

    /// Files one line of this instance; `startup` is the line's startup stage.
    fn observe(
        &mut self,
        at: f64,
        line: &logs::Line<'_>,
        startup: Option<Startup>,
        window: (f64, f64),
    ) {
        let (since, until) = window;
        let in_window = since <= at && at <= until;
        self.last_seen = at;
        if let Some(stage) = startup {
            self.startup = Some(stage);
        }
        if line.rest.starts_with("catalog reconciled") {
            self.recovered = Some(logs::kv(line.rest));
            self.recovered_at = Some(at);
        } else if line.rest.starts_with("SIGTERM") || line.rest.starts_with("interrupted") {
            self.clean = true;
        } else if line.rest == "stopped" {
            self.stop = Some(at);
        } else if let Some(statistics) = logs::statistics(line).filter(|_| startup.is_none()) {
            match statistics {
                Statistics::Row(series, fields) => {
                    self.has_statistics = true;
                    self.series(series).push((at, fields), since, until);
                }
                Statistics::Malformed(kind) if in_window => self.problems.push(Logged {
                    at,
                    level: "ERROR".to_owned(),
                    text: format!("malformed {kind} statistics row"),
                }),
                Statistics::Malformed(_) => {}
            }
        } else if let Some(kind) = notable_kind(line.rest) {
            let lines = self.notable.entry(kind).or_default();
            if in_window {
                lines.push(Logged {
                    at,
                    level: line.level.to_owned(),
                    text: line.rest.to_owned(),
                });
            }
        } else if in_window && matches!(line.level, "WARN" | "ERROR") {
            if line.rest.contains(UNKNOWN_RTCP_SSRC) {
                self.unknown_ssrc_warnings += 1;
            } else {
                self.problems.push(Logged {
                    at,
                    level: line.level.to_owned(),
                    text: line.rest.to_owned(),
                });
            }
        }
    }

    fn series(&mut self, series: Series) -> &mut Window {
        match series {
            Series::Stream(name) => self.streams.entry(name).or_default(),
            Series::Events { camera, source } => self.events.entry((camera, source)).or_default(),
            Series::Root(root) => self.roots.entry(root).or_default(),
            Series::Writer => &mut self.writer,
            Series::Catalog => &mut self.catalog,
            Series::Process => &mut self.process,
        }
    }

    /// Limits the instance's state to the window once the journal is read.
    fn clip(&mut self, since: f64, until: f64) {
        self.started_before_window = self.start < since;
        self.start = self.start.max(since);
        self.last_seen = self.last_seen.min(until);
        if self.stop.is_some_and(|stop| stop > until) {
            self.stop = None;
            self.clean = false;
        }
        if self.restarted_at.is_some_and(|restart| restart > until) {
            self.restarted_at = None;
        }
        if self
            .recovered_at
            .is_some_and(|timestamp| timestamp < since || timestamp > until)
        {
            self.recovered = None;
            self.recovered_at = None;
        }
    }
}

fn startup_stage(rest: &str) -> Option<Startup> {
    if rest.starts_with("the configuration file holds credentials ") {
        Some(Startup::ExposedConfiguration)
    } else if rest.starts_with("api.allow_unauthenticated is set") {
        Some(Startup::Unauthenticated)
    } else if rest.starts_with("descriptor limit ")
        || rest.starts_with("raised the descriptor soft limit")
        || rest.starts_with("cannot raise the descriptor soft limit")
    {
        Some(Startup::Descriptors)
    } else if rest.starts_with("restricted state permissions") {
        Some(Startup::StateModes)
    } else if rest.starts_with("recording root unavailable at startup ")
        || rest.starts_with("recording root aliases another configured root")
    {
        Some(Startup::Roots)
    } else if rest == "sandbox disabled by --no-sandbox"
        || rest == "sandbox fully enforced"
        || rest.starts_with("sandbox partially enforced: ")
    {
        Some(Startup::Sandbox)
    } else if rest.starts_with("catalog open ") {
        Some(Startup::Catalog)
    } else {
        None
    }
}

fn notable_kind(rest: &str) -> Option<Notable> {
    if rest.starts_with("gap ") {
        Some(Notable::Gap)
    } else if rest.starts_with("selfcheck ") {
        Some(Notable::SelfCheck)
    } else if rest.starts_with("retention pass") || rest.starts_with("slow retention") {
        Some(Notable::Retention)
    } else if rest.starts_with("slow ") {
        Some(Notable::Slow)
    } else {
        None
    }
}

/// The service instances a journal shows within the window.
#[derive(Debug, Default)]
struct Journal {
    runs: Vec<Run>,
    /// Lines skipped as unreadable: not UTF-8, or in the daemon's shape with a
    /// timestamp that does not parse.
    unreadable: usize,
}

impl Journal {
    fn observe(&mut self, at: f64, line: &logs::Line<'_>, window: (f64, f64)) {
        let startup = startup_stage(line.rest);
        let reconciled = line.rest.starts_with("catalog reconciled");
        let mut restarted = false;
        if let Some(previous) = self.runs.last_mut()
            && previous.restarts_at(startup, reconciled)
        {
            previous.restarted_at = Some(at);
            restarted = true;
        }
        if restarted || self.runs.last().is_none_or(|run| run.stop.is_some()) {
            self.runs
                .push(Run::starting(at, startup.is_some() || reconciled));
        }
        self.runs
            .last_mut()
            .expect("an instance is open after the push above")
            .observe(at, line, startup, window);
    }
}

/// Streams the journal, keeping only what `[since, until]` needs.
fn parse_journal(path: &Path, since: f64, until: f64) -> anyhow::Result<Journal> {
    let file = File::open(path).with_context(|| format!("reading {}", path.display()))?;
    let mut lines = LineReader::new(BufReader::new(file));
    let mut journal = Journal::default();
    while let Some(input) = lines
        .next_line()
        .with_context(|| format!("reading {}", path.display()))?
    {
        let Ok(raw) = std::str::from_utf8(input.bytes) else {
            journal.unreadable += 1;
            continue;
        };
        let line = logs::strip_ansi(raw);
        let Some(split) = logs::split(&line) else {
            continue;
        };
        let Some(at) = time::parse_seconds(split.timestamp) else {
            journal.unreadable += 1;
            continue;
        };
        journal.observe(at, &split, (since, until));
    }
    journal
        .runs
        .retain(|run| run.start <= until && run.last_seen >= since);
    for run in &mut journal.runs {
        run.clip(since, until);
    }
    Ok(journal)
}

fn counter(fields: &Fields, key: &str) -> Option<u64> {
    fields.get(key)?.parse::<u64>().ok()
}

/// A cumulative counter's growth over one run's window: from the baseline
/// before the window, or from zero when the run started inside it, through
/// every row, a drop (a counter reset) adding nothing. `None` when a row
/// lacks the counter.
fn counter_growth(window: &Window, from_zero: bool, key: &str) -> Option<u64> {
    let mut values = Vec::with_capacity(window.rows.len() + 1);
    if window.baseline.is_none() && from_zero {
        values.push(0);
    }
    for (_, fields) in window.points() {
        values.push(counter(fields, key)?);
    }
    values.windows(2).try_fold(0u64, |total, pair| {
        total.checked_add(pair[1].saturating_sub(pair[0]))
    })
}

/// [`counter_growth`] for a cumulative counter the daemon logs with a
/// fraction, such as `mib`, over finite nonnegative values.
fn float_growth(window: &Window, from_zero: bool, key: &str) -> Option<f64> {
    let mut values = Vec::with_capacity(window.rows.len() + 1);
    if window.baseline.is_none() && from_zero {
        values.push(0.0);
    }
    for (_, fields) in window.points() {
        values.push(logs::number(fields, key).filter(|value| *value >= 0.0)?);
    }
    Some(summary::growth(&values))
}

/// The sum over runs of a counter's [`counter_growth`].
fn window_growth(runs: &[Run], pick: impl Fn(&Run) -> &Window, key: &str) -> Option<u64> {
    runs.iter().try_fold(0u64, |total, run| {
        total.checked_add(counter_growth(pick(run), !run.started_before_window, key)?)
    })
}

/// The sum over runs of a counter's [`float_growth`].
fn float_window_growth(runs: &[Run], pick: impl Fn(&Run) -> &Window, key: &str) -> Option<f64> {
    runs.iter().try_fold(0.0, |total, run| {
        Some(total + float_growth(pick(run), !run.started_before_window, key)?)
    })
}

/// The sum over every run and series of a counter's [`counter_growth`].
fn series_growth<'a>(
    runs: &'a [Run],
    series: impl Fn(&'a Run) -> Box<dyn Iterator<Item = &'a Window> + 'a>,
    key: &str,
) -> Option<u64> {
    runs.iter().try_fold(0u64, |total, run| {
        series(run).try_fold(total, |total, window| {
            total.checked_add(counter_growth(window, !run.started_before_window, key)?)
        })
    })
}

fn stream_windows(run: &Run) -> Box<dyn Iterator<Item = &Window> + '_> {
    Box::new(run.streams.values())
}

fn feed_windows(run: &Run) -> Box<dyn Iterator<Item = &Window> + '_> {
    Box::new(run.events.values())
}

fn display_counter(value: Option<u64>) -> String {
    value.map_or_else(|| "unknown".to_owned(), |value| value.to_string())
}

fn display_float_counter(value: Option<f64>, digits: usize) -> String {
    value.map_or_else(|| "unknown".to_owned(), |value| format!("{value:.digits$}"))
}

/// How many rows hold `true` for `key`; `None` when one holds no boolean.
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

/// Every run's in-window rows of one series kind.
fn window_rows(runs: &[Run], pick: impl Fn(&Run) -> &Window) -> Vec<Row> {
    runs.iter()
        .flat_map(|run| pick(run).rows.iter().cloned())
        .collect()
}

/// The least-squares slope of `key` against time, per day.
fn slope_per_day(rows: &[Row], key: &str) -> Option<f64> {
    let points: Vec<(f64, f64)> = rows
        .iter()
        .filter_map(|(t, fields)| logs::number(fields, key).map(|value| (*t, value)))
        .collect();
    if points.len() < 3 {
        return None;
    }
    let t0 = points[0].0;
    let xs: Vec<f64> = points.iter().map(|(t, _)| (t - t0) / 86_400.0).collect();
    let ys: Vec<f64> = points.iter().map(|(_, value)| *value).collect();
    let mx = summary::mean(&xs)?;
    let my = summary::mean(&ys)?;
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

/// The rows within an hour of the first one.
fn first_hour(rows: &[Row]) -> &[Row] {
    let Some((start, _)) = rows.first() else {
        return rows;
    };
    &rows[..rows.partition_point(|(t, _)| *t < start + 3_600.0)]
}

/// The rows within an hour of the last one.
fn last_hour(rows: &[Row]) -> &[Row] {
    let Some((end, _)) = rows.last() else {
        return rows;
    };
    &rows[rows.partition_point(|(t, _)| *t <= end - 3_600.0)..]
}

/// Replaces every `key=value` value with an ellipsis, so lines group by shape.
fn mask_values(rest: &str) -> String {
    let mut out = String::new();
    for (i, word) in rest.split(' ').enumerate() {
        if i > 0 {
            out.push(' ');
        }
        match word.split_once('=') {
            Some((key, _)) => {
                out.push_str(key);
                out.push_str("=…");
            }
            None => out.push_str(word),
        }
    }
    out
}

fn report_runs(journal: &Journal, out: &mut impl Write) -> io::Result<()> {
    let runs = &journal.runs;
    writeln!(out, "== runs")?;
    for (i, run) in runs.iter().enumerate() {
        let end = run.stop.unwrap_or(run.last_seen);
        let hours = (end - run.start) / 3_600.0;
        let state = match (run.clean, run.stop, run.restarted_at) {
            (true, Some(_), _) => "stopped cleanly",
            (_, Some(_), _) => "stopped",
            (_, _, Some(_)) => "restarted without a stopped line",
            _ => "running",
        };
        let recovered = |key: &str| {
            run.recovered
                .as_ref()
                .and_then(|fields| fields.get(key))
                .map_or("-", String::as_str)
        };
        writeln!(
            out,
            "run {}: {} to {} ({hours:.1} h) {state}; recovered={} adopted={}; warnings/errors={}",
            i + 1,
            time::iso(run.start),
            time::iso(end),
            recovered("recovered"),
            recovered("adopted"),
            run.problems.len()
        )?;
        if let Some(previous) = i.checked_sub(1).and_then(|index| runs.get(index)) {
            let gap = (run.start - previous.stop.unwrap_or(previous.last_seen)) / 60.0;
            let label = if previous.stop.is_some() {
                "downtime"
            } else {
                "unobserved interval (includes possible downtime)"
            };
            writeln!(out, "   {label} before this run: {gap:.1} min")?;
        }
    }
    let mut index: HashMap<String, usize> = HashMap::new();
    let mut problems: Vec<(String, usize)> = Vec::new();
    for problem in runs.iter().flat_map(|run| &run.problems) {
        match index.entry(format!("{} {}", problem.level, mask_values(&problem.text))) {
            Entry::Occupied(entry) => problems[*entry.get()].1 += 1,
            Entry::Vacant(entry) => {
                problems.push((entry.key().clone(), 1));
                entry.insert(problems.len() - 1);
            }
        }
    }
    problems.sort_by_key(|(_, count)| Reverse(*count));
    for (key, count) in problems.iter().take(10) {
        writeln!(out, "   {count:5}x {}", summary::truncate(key, 140))?;
    }
    let unknown_ssrc = runs
        .iter()
        .map(|run| run.unknown_ssrc_warnings)
        .sum::<usize>();
    if unknown_ssrc > 0 {
        writeln!(
            out,
            "   {unknown_ssrc} `{UNKNOWN_RTCP_SSRC}` warnings are not counted as problems"
        )?;
    }
    if journal.unreadable > 0 {
        writeln!(
            out,
            "   {} unreadable journal lines skipped (not UTF-8, or an invalid timestamp)",
            journal.unreadable
        )?;
    }
    Ok(())
}

fn report_process(runs: &[Run], out: &mut impl Write) -> io::Result<()> {
    writeln!(out, "\n== process")?;
    let rows = window_rows(runs, |run| &run.process);
    let Some(last_run) = runs.last().filter(|_| !rows.is_empty()) else {
        return Ok(());
    };
    let cpu = summary::col(&rows, "cpu_pct");
    let rss = summary::col(&rows, "rss_mib");
    let last = &last_run.process.rows;
    writeln!(
        out,
        "samples={} cpu_pct mean={} max={} (user {}, sys {})",
        rows.len(),
        summary::decimals(summary::mean(&cpu), 2),
        summary::decimals(summary::maximum(&cpu), 2),
        summary::decimals(summary::mean(&summary::col(&rows, "cpu_user_pct")), 2),
        summary::decimals(summary::mean(&summary::col(&rows, "cpu_sys_pct")), 2)
    )?;
    writeln!(
        out,
        "rss_mib min={} max={}; last run: first hour mean={} last hour mean={} slope={} MiB/day",
        summary::decimals(summary::minimum(&rss), 1),
        summary::decimals(summary::maximum(&rss), 1),
        summary::decimals(summary::mean(&summary::col(first_hour(last), "rss_mib")), 1),
        summary::decimals(summary::mean(&summary::col(last_hour(last), "rss_mib")), 1),
        summary::decimals(slope_per_day(last, "rss_mib"), 2)
    )?;
    writeln!(
        out,
        "loop_lag_max_ms={} retention_max_ms={} watchdog_withheld={} teardowns max={} fds min={} \
         max={} threads max={} selfcheck ok={} failed={}",
        summary::decimals(summary::col_max(&rows, "loop_lag_max_ms"), 0),
        summary::decimals(summary::col_max(&rows, "retention_max_ms"), 0),
        display_counter(window_growth(runs, |run| &run.process, "watchdog_withheld")),
        summary::decimals(summary::col_max(&rows, "teardowns"), 0),
        summary::decimals(summary::minimum(&summary::col(&rows, "fds")), 0),
        summary::decimals(summary::col_max(&rows, "fds"), 0),
        summary::decimals(summary::col_max(&rows, "threads"), 0),
        display_counter(window_growth(runs, |run| &run.process, "selfcheck_ok")),
        display_counter(window_growth(runs, |run| &run.process, "selfcheck_failed"))
    )
}

/// The rows that carry a run's file counters. The daemon's earliest
/// statistics put the writer's `fragments`, `mib`, `syncs`, and `files` on the
/// `process` line, before the `writer` line existed; a run without `writer`
/// rows reads them there.
fn file_counters(run: &Run) -> &Window {
    if run.writer.rows.is_empty() && run.writer.baseline.is_none() {
        &run.process
    } else {
        &run.writer
    }
}

/// The payload the daemon received and the bytes it wrote to files in the
/// window, from its own counters, over the runs that log both.
#[derive(Clone, Copy, Debug, Default)]
struct DaemonBytes {
    payload_mib: f64,
    files_mib: f64,
    /// Runs that log both counters, and runs in all.
    runs: (usize, usize),
}

impl DaemonBytes {
    fn of(runs: &[Run]) -> Self {
        let mut bytes = Self {
            runs: (0, runs.len()),
            ..Self::default()
        };
        for run in runs {
            let from_zero = !run.started_before_window;
            let files = file_counters(run);
            if files.rows.is_empty() && files.baseline.is_none() {
                continue;
            }
            let payload = run
                .streams
                .values()
                .map(|window| float_growth(window, from_zero, "mib"))
                .sum::<Option<f64>>();
            if let (Some(payload), Some(files)) = (payload, float_growth(files, from_zero, "mib")) {
                bytes.payload_mib += payload;
                bytes.files_mib += files;
                bytes.runs.0 += 1;
            }
        }
        bytes
    }

    /// The bytes written beyond the payload, as a percentage of the payload.
    fn overhead_pct(self) -> Option<f64> {
        (self.payload_mib > 0.0).then(|| (self.files_mib / self.payload_mib - 1.0) * 100.0)
    }
}

fn report_writer(runs: &[Run], out: &mut impl Write) -> io::Result<()> {
    writeln!(out, "\n== writer")?;
    let rows = window_rows(runs, |run| &run.writer);
    if rows.is_empty() {
        return Ok(());
    }
    // `mib` carries one decimal in the daemon's line; the rest are integers.
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
    .map(|key| {
        let total = if *key == "mib" {
            display_float_counter(float_window_growth(runs, |run| &run.writer, key), 1)
        } else {
            display_counter(window_growth(runs, |run| &run.writer, key))
        };
        format!("{key}={total}")
    })
    .collect();
    writeln!(out, "totals: {}", totals.join(" "))?;
    let bytes = DaemonBytes::of(runs);
    writeln!(
        out,
        "payload_mib={:.1} files_mib={:.1} container_overhead_pct={} (runs {} of {})",
        bytes.payload_mib,
        bytes.files_mib,
        summary::decimals(bytes.overhead_pct(), 4),
        bytes.runs.0,
        bytes.runs.1
    )?;
    writeln!(
        out,
        "write_p99_max_ms={} write_max_ms={} sync_p99_max_ms={} sync_max_ms={} queue_peak={} \
         queue_mib_peak={}",
        summary::decimals(summary::col_max(&rows, "write_p99_ms"), 1),
        summary::decimals(summary::col_max(&rows, "write_max_ms"), 1),
        summary::decimals(summary::col_max(&rows, "sync_p99_ms"), 1),
        summary::decimals(summary::col_max(&rows, "sync_max_ms"), 1),
        summary::decimals(summary::col_max(&rows, "queue_peak"), 0),
        summary::decimals(summary::col_max(&rows, "queue_mib_peak"), 1)
    )
}

fn report_catalog(runs: &[Run], out: &mut impl Write) -> io::Result<()> {
    writeln!(out, "\n== catalog")?;
    let rows = window_rows(runs, |run| &run.catalog);
    let Some((_, last)) = rows.last() else {
        return Ok(());
    };
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
    .map(|key| format!("{key}={}", last.get(*key).map_or("-", String::as_str)))
    .collect();
    writeln!(out, "last: {}", items.join(" "))?;
    let growth = |key: &str| display_counter(window_growth(runs, |run| &run.catalog, key));
    writeln!(
        out,
        "txn_p99_max_ms={} txn_max_ms={} query_max_ms={} wait_max_ms={} \
         event_commit_p99_max_ms={} event_commit_max_ms={} batch_peak={} queue_peak={} \
         queue_mib_peak={} coalesced={} rejected={} rejected_events={} rejected_progress={} \
         rejected_queries={} rejected_lifecycle={} failures={}",
        summary::decimals(summary::col_max(&rows, "txn_p99_ms"), 1),
        summary::decimals(summary::col_max(&rows, "txn_max_ms"), 1),
        summary::decimals(summary::col_max(&rows, "query_max_ms"), 1),
        summary::decimals(summary::col_max(&rows, "wait_max_ms"), 1),
        summary::decimals(summary::col_max(&rows, "event_commit_p99_ms"), 1),
        summary::decimals(summary::col_max(&rows, "event_commit_max_ms"), 1),
        summary::decimals(summary::col_max(&rows, "batch_peak"), 0),
        summary::decimals(summary::col_max(&rows, "queue_peak"), 0),
        summary::decimals(summary::col_max(&rows, "queue_mib_peak"), 1),
        growth("coalesced"),
        growth("rejected"),
        growth("rejected_events"),
        growth("rejected_progress"),
        growth("rejected_queries"),
        growth("rejected_lifecycle"),
        growth("failures")
    )
}

fn report_streams(runs: &[Run], out: &mut impl Write) -> io::Result<()> {
    writeln!(
        out,
        "\n== streams (sums over runs; fps between consecutive samples within a run)"
    )?;
    writeln!(
        out,
        "stream           fps_med fps_min stalls reconn gaps holds hold_s snaps loss  \
         cam_ms(min/max) idle_max frag_kib_max"
    )?;
    let names: BTreeSet<&String> = runs.iter().flat_map(|run| run.streams.keys()).collect();
    for name in names {
        let mut all_rates = Vec::new();
        let mut sums: BTreeMap<&'static str, Option<u64>> = BTreeMap::new();
        let mut hold_s = 0.0;
        let (mut offsets, mut idle, mut fragments) = (Vec::new(), Vec::new(), Vec::new());
        for run in runs {
            let Some(window) = run.streams.get(name) else {
                continue;
            };
            all_rates.extend(summary::rates(&window.rows));
            for key in ["reconnects", "gaps", "holds", "snaps", "loss"] {
                add_counter_sum(
                    &mut sums,
                    key,
                    counter_growth(window, !run.started_before_window, key),
                );
            }
            hold_s += float_growth(window, !run.started_before_window, "hold_s").unwrap_or(0.0);
            offsets.extend(summary::col(&window.rows, "cam_offset_ms"));
            idle.extend(summary::col(&window.rows, "idle_s"));
            fragments.extend(summary::col(&window.rows, "frag_kib_max"));
        }
        let (median, stalls) = summary::median_and_stalls(&all_rates);
        let sum = |key: &str| display_counter(sums.get(key).copied().flatten());
        writeln!(
            out,
            "{name:16} {median:7.2} {:7.2} {stalls:6} {:6} {:4} {:5} {hold_s:6.1} {:5} {:4}  \
             {:>5}/{:<5} {:>8} {:>12}",
            summary::minimum(&all_rates).unwrap_or(0.0),
            sum("reconnects"),
            sum("gaps"),
            sum("holds"),
            sum("snaps"),
            sum("loss"),
            summary::decimals(summary::minimum(&offsets), 0),
            summary::decimals(summary::maximum(&offsets), 0),
            summary::decimals(summary::maximum(&idle), 1),
            summary::decimals(summary::maximum(&fragments), 0)
        )?;
    }
    Ok(())
}

fn report_event_feeds(runs: &[Run], out: &mut impl Write) -> io::Result<()> {
    writeln!(out, "\n== event feeds")?;
    writeln!(
        out,
        "camera       source             events messages reconn renewals errors idle_max \
         pull_p99_max pull_max"
    )?;
    let feeds: BTreeSet<&Feed> = runs.iter().flat_map(|run| run.events.keys()).collect();
    for feed in feeds {
        let mut sums: BTreeMap<&'static str, Option<u64>> = BTreeMap::new();
        let (mut idle, mut pull, mut pull_max) = (Vec::new(), Vec::new(), Vec::new());
        for run in runs {
            let Some(window) = run.events.get(feed) else {
                continue;
            };
            for key in ["events", "messages", "reconnects", "renewals", "errors"] {
                add_counter_sum(
                    &mut sums,
                    key,
                    counter_growth(window, !run.started_before_window, key),
                );
            }
            idle.extend(summary::col(&window.rows, "idle_s"));
            pull.extend(summary::col(&window.rows, "pull_p99_ms"));
            pull_max.extend(summary::col(&window.rows, "pull_max_ms"));
        }
        let sum = |key: &str| display_counter(sums.get(key).copied().flatten());
        writeln!(
            out,
            "{:12} {:18} {:6} {:8} {:6} {:8} {:6} {:>8} {:>12} {:>8}",
            feed.0,
            feed.1,
            sum("events"),
            sum("messages"),
            sum("reconnects"),
            sum("renewals"),
            sum("errors"),
            summary::decimals(summary::maximum(&idle), 1),
            summary::decimals(summary::maximum(&pull), 1),
            summary::decimals(summary::maximum(&pull_max), 1)
        )?;
    }
    Ok(())
}

fn notable(runs: &[Run], kind: Notable) -> Vec<&Logged> {
    runs.iter()
        .flat_map(|run| run.notable.get(&kind).into_iter().flatten())
        .collect()
}

/// The self-check lines by outcome: checks that ran and passed, checks that
/// failed or could not run, and checks skipped.
#[derive(Debug, Default)]
struct SelfChecks<'a> {
    passed: usize,
    failed: Vec<&'a Logged>,
    skipped: usize,
}

impl<'a> SelfChecks<'a> {
    fn of(lines: &[&'a Logged]) -> Self {
        let mut checks = Self::default();
        for line in lines {
            if line.text.starts_with("selfcheck ok") {
                checks.passed += 1;
            } else if line.text.starts_with("selfcheck failed")
                || line.text.starts_with("selfcheck task failed")
            {
                checks.failed.push(line);
            } else if line.text.starts_with("selfcheck skipped") {
                checks.skipped += 1;
            }
        }
        checks
    }
}

fn report_gaps(runs: &[Run], out: &mut impl Write) -> io::Result<()> {
    let gaps = notable(runs, Notable::Gap);
    writeln!(out, "\n== gaps ({})", gaps.len())?;
    let mut per: BTreeMap<String, f64> = BTreeMap::new();
    let mut causes: BTreeMap<String, usize> = BTreeMap::new();
    for gap in &gaps {
        let fields = logs::kv(&gap.text);
        let stream = fields.get("stream").cloned().unwrap_or_else(|| "?".into());
        *per.entry(stream).or_default() += logs::number(&fields, "secs").unwrap_or(0.0);
        let cause = fields.get("cause").cloned().unwrap_or_else(|| "?".into());
        *causes.entry(cause).or_default() += 1;
    }
    for (stream, secs) in &per {
        writeln!(out, "   {stream:16} {secs:8.1} s")?;
    }
    if !causes.is_empty() {
        let items: Vec<String> = causes
            .iter()
            .map(|(cause, count)| format!("{cause}={count}"))
            .collect();
        writeln!(out, "   by cause: {}", items.join(" "))?;
    }
    for gap in gaps.iter().take(50) {
        writeln!(
            out,
            "   {} {}",
            time::iso(gap.at),
            summary::truncate(&gap.text, 160)
        )?;
    }
    if gaps.len() > 50 {
        writeln!(out, "   ... {} more", gaps.len() - 50)?;
    }
    Ok(())
}

fn report_retention(runs: &[Run], out: &mut impl Write) -> io::Result<()> {
    let passes = notable(runs, Notable::Retention);
    let failures = passes
        .iter()
        .filter(|pass| pass.text.starts_with("retention pass failed"))
        .count();
    let successful: Vec<&Logged> = passes
        .iter()
        .filter(|pass| !pass.text.starts_with("retention pass failed"))
        .copied()
        .collect();
    writeln!(
        out,
        "\n== retention passes logged ({} successful, {failures} failed)",
        successful.len()
    )?;
    if successful.is_empty() {
        return Ok(());
    }
    let fields: Vec<Fields> = successful.iter().map(|pass| logs::kv(&pass.text)).collect();
    let sum = |key: &str| {
        fields.iter().try_fold(0u64, |total, fields| {
            total.checked_add(counter(fields, key)?)
        })
    };
    let column = |key: &str| -> Vec<f64> {
        fields
            .iter()
            .filter_map(|fields| logs::number(fields, key))
            .collect()
    };
    let ms = column("ms");
    let total = column("total_gib");
    writeln!(
        out,
        "deleted={} freed_gib={} ms max={} mean={} free_pct min={} total_gib min={} max={} \
         quota_gib={} slow={}",
        display_counter(sum("deleted")),
        summary::decimals(sum("freed_mib").map(|value| value as f64 / 1024.0), 1),
        summary::decimals(summary::maximum(&ms), 0),
        summary::decimals(summary::mean(&ms), 0),
        summary::decimals(summary::minimum(&column("free_pct")), 1),
        summary::decimals(summary::minimum(&total), 2),
        summary::decimals(summary::maximum(&total), 2),
        fields
            .last()
            .and_then(|fields| fields.get("quota_gib").cloned())
            .unwrap_or_else(|| "-".into()),
        successful
            .iter()
            .filter(|pass| pass.level == "WARN")
            .count()
    )
}

fn report_notable(runs: &[Run], out: &mut impl Write) -> io::Result<()> {
    report_gaps(runs, out)?;
    report_retention(runs, out)?;
    let slow = notable(runs, Notable::Slow);
    writeln!(out, "\n== slow I/O warnings ({})", slow.len())?;
    for line in slow.iter().take(20) {
        writeln!(
            out,
            "   {} {}",
            time::iso(line.at),
            summary::truncate(&line.text, 120)
        )?;
    }
    let checks = SelfChecks::of(&notable(runs, Notable::SelfCheck));
    writeln!(
        out,
        "\n== self-checks: {} run, {} failed, {} skipped",
        checks.passed + checks.failed.len(),
        checks.failed.len(),
        checks.skipped
    )?;
    for line in checks.failed.iter().take(20) {
        writeln!(
            out,
            "   {} {}",
            time::iso(line.at),
            summary::truncate(&line.text, 220)
        )?;
    }
    Ok(())
}

type Sample = (f64, Value);

/// Whether a sampler series could be read whole.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Completeness {
    Missing,
    Complete,
    /// The final record lacked its newline, a sample still being written.
    IncompleteFinalRecord,
    /// Records that are not one JSON object with a valid `ts` were skipped.
    Corrupt {
        lines: usize,
        first: usize,
    },
}

impl fmt::Display for Completeness {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Missing => f.write_str("missing"),
            Self::Complete => f.write_str("complete"),
            Self::IncompleteFinalRecord => f.write_str("incomplete final record ignored"),
            Self::Corrupt { lines, first } => {
                write!(
                    f,
                    "{lines} corrupt records skipped, the first at line {first}"
                )
            }
        }
    }
}

struct LoadedSamples {
    rows: Vec<Sample>,
    completeness: Completeness,
}

/// Streams a sampler series, keeping the samples inside `[since, until]`.
/// A final record without its newline (a sample being written) is ignored,
/// and a record that is not valid is skipped; both are reported.
fn load_jsonl(path: &Path, since: f64, until: f64) -> anyhow::Result<LoadedSamples> {
    let file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == ErrorKind::NotFound => {
            return Ok(LoadedSamples {
                rows: Vec::new(),
                completeness: Completeness::Missing,
            });
        }
        Err(error) => return Err(error).with_context(|| format!("reading {}", path.display())),
    };
    let mut lines = LineReader::new(BufReader::new(file));
    let mut rows = Vec::new();
    let mut truncated = false;
    let (mut corrupt, mut first_corrupt) = (0usize, None);
    while let Some(input) = lines
        .next_line()
        .with_context(|| format!("reading {}", path.display()))?
    {
        if !input.terminated {
            truncated = true;
            break;
        }
        let sample = std::str::from_utf8(input.bytes)
            .ok()
            .and_then(|line| serde_json::from_str::<Value>(line).ok())
            .and_then(|value| {
                let timestamp = time::parse_seconds(value.get("ts")?.as_str()?)?;
                Some((timestamp, value))
            });
        if let Some((timestamp, value)) = sample {
            if since <= timestamp && timestamp <= until {
                rows.push((timestamp, value));
            }
        } else {
            corrupt += 1;
            first_corrupt.get_or_insert(input.number);
        }
    }
    let completeness = match first_corrupt {
        Some(first) => Completeness::Corrupt {
            lines: corrupt,
            first,
        },
        None if truncated => Completeness::IncompleteFinalRecord,
        None => Completeness::Complete,
    };
    Ok(LoadedSamples { rows, completeness })
}

/// A finite number in a sampler record, 0.0 when absent; for display only.
fn finite_or_zero(value: &Value, key: &str) -> f64 {
    value
        .get(key)
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

/// A sampler record's section, with the identity whose counters it belongs to.
type Section<'a> = (f64, String, &'a Value);

/// Whether two consecutive section records can be compared: one service
/// instance, a positive gap of at most [`MAX_SAMPLE_GAP_SECONDS`].
fn compatible(a: &Section<'_>, b: &Section<'_>) -> bool {
    let dt = b.0 - a.0;
    a.1 == b.1 && dt > 0.0 && dt <= MAX_SAMPLE_GAP_SECONDS
}

/// A counter's growth over the compatible consecutive records that do not
/// show a reset, with the time they cover; `None` without one.
fn segmented_json_growth(points: &[Section<'_>], key: &str) -> Option<(f64, f64)> {
    let mut total = 0u64;
    let mut wall = 0.0;
    let mut edges = 0usize;
    for pair in points.windows(2) {
        let (Some(a), Some(b)) = (
            pair[0].2.get(key).and_then(Value::as_u64),
            pair[1].2.get(key).and_then(Value::as_u64),
        ) else {
            continue;
        };
        if compatible(&pair[0], &pair[1]) && b >= a {
            total = total.checked_add(b - a)?;
            wall += pair[1].0 - pair[0].0;
            edges += 1;
        }
    }
    (edges > 0).then_some((total as f64, wall))
}

fn section<'a>(value: &'a Value, key: &str) -> Option<&'a Value> {
    value
        .get(key)
        .filter(|section| section.is_object() && section.get("error").is_none())
}

/// A series' value at `t`, interpolated between its points and held at the
/// first or last point outside them.
fn interpolate(points: &[(f64, f64)], t: f64) -> Option<f64> {
    let (first, last) = (points.first()?, points.last()?);
    if t <= first.0 {
        return Some(first.1);
    }
    if t >= last.0 {
        return Some(last.1);
    }
    let index = points.partition_point(|(time, _)| *time <= t);
    let (Some(a), Some(b)) = (
        index.checked_sub(1).and_then(|before| points.get(before)),
        points.get(index),
    ) else {
        return Some(last.1);
    };
    Some(if b.0 > a.0 {
        a.1 + (b.1 - a.1) * (t - a.0) / (b.0 - a.0)
    } else {
        a.1
    })
}

/// A cumulative series' growth from `a` to `b`, resets adding nothing.
fn growth_between(points: &[(f64, f64)], a: f64, b: f64) -> f64 {
    let (Some(start), Some(end)) = (interpolate(points, a), interpolate(points, b)) else {
        return 0.0;
    };
    let inner = points
        .iter()
        .filter(|(t, _)| a < *t && *t < b)
        .map(|(_, value)| *value);
    let values: Vec<f64> = std::iter::once(start)
        .chain(inner)
        .chain(std::iter::once(end))
        .collect();
    summary::growth(&values)
}

/// The cgroup's bytes written over the payload received in the same
/// intervals: the compatible consecutive samples whose write counter did not
/// reset, each with the stream payload interpolated between its two times.
#[derive(Clone, Copy, Debug, Default)]
struct Amplification {
    written: f64,
    payload: f64,
    /// Seconds the accepted intervals cover.
    covered: f64,
    /// Seconds from the first to the last active cgroup sample.
    span: f64,
}

impl Amplification {
    fn of(cgroup: &[Section<'_>], runs: &[Run]) -> Self {
        let series: Vec<Vec<(f64, f64)>> = runs
            .iter()
            .flat_map(|run| run.streams.values())
            .map(|window| {
                window
                    .points()
                    .filter_map(|(t, fields)| Some((*t, logs::number(fields, "mib")?)))
                    .collect()
            })
            .collect();
        let mut amplification = Self {
            span: match (cgroup.first(), cgroup.last()) {
                (Some(first), Some(last)) => last.0 - first.0,
                _ => 0.0,
            },
            ..Self::default()
        };
        for pair in cgroup.windows(2) {
            let (Some(a), Some(b)) = (
                pair[0].2.get("io_wbytes").and_then(Value::as_u64),
                pair[1].2.get("io_wbytes").and_then(Value::as_u64),
            ) else {
                continue;
            };
            if !compatible(&pair[0], &pair[1]) || b < a {
                continue;
            }
            let (from, to) = (pair[0].0, pair[1].0);
            amplification.written += (b - a) as f64;
            amplification.covered += to - from;
            amplification.payload += series
                .iter()
                .map(|points| growth_between(points, from, to))
                .sum::<f64>()
                * MIB;
        }
        amplification
    }

    fn ratio(&self) -> Option<f64> {
        (self.payload > 0.0 && self.covered > 0.0).then(|| self.written / self.payload)
    }

    fn coverage_pct(&self) -> Option<f64> {
        (self.span > 0.0).then(|| self.covered / self.span * 100.0)
    }
}

/// The host-side figures the criteria table uses.
#[derive(Clone, Copy, Debug, Default)]
struct HostFigures {
    amplification: Amplification,
    /// Supervisor restarts in the window: the growth of the unit's `NRestarts`.
    restarts: Option<f64>,
}

fn report_host(
    samples: &[Sample],
    completeness: Completeness,
    runs: &[Run],
    out: &mut impl Write,
) -> io::Result<HostFigures> {
    writeln!(out, "\n== host samples ({}, {completeness})", samples.len())?;
    let (Some((first, _)), Some((last, _))) = (samples.first(), samples.last()) else {
        return Ok(HostFigures::default());
    };
    let restart_values: Vec<f64> = samples
        .iter()
        .filter_map(|(_, sample)| sample.get("unit")?.get("NRestarts")?.as_f64())
        .filter(|value| value.is_finite() && *value >= 0.0)
        .collect();
    let restarts = (!restart_values.is_empty()).then(|| summary::growth(&restart_values));
    let active: Vec<&Sample> = samples
        .iter()
        .filter(|(_, sample)| {
            sample
                .get("unit")
                .and_then(|unit| unit.get("ActiveState"))
                .and_then(Value::as_str)
                == Some("active")
        })
        .collect();
    let mut errors: BTreeMap<String, usize> = BTreeMap::new();
    for (_, sample) in &active {
        for (name, value) in sample.as_object().into_iter().flatten() {
            if value.is_object() && value.get("error").is_some() {
                *errors.entry(name.clone()).or_default() += 1;
            }
        }
    }
    let errors_text = if errors.is_empty() {
        "none".to_owned()
    } else {
        let items: Vec<String> = errors
            .iter()
            .map(|(name, count)| format!("{name}={count}"))
            .collect();
        items.join(" ")
    };
    writeln!(
        out,
        "window {} to {}; active={}; section errors while active={errors_text}",
        time::iso(*first),
        time::iso(*last),
        active.len()
    )?;
    let amplification = report_cgroup(&active, runs, out)?;
    report_disk_and_pressure(samples, out)?;
    report_network(samples, &active, out)?;
    report_clock_and_process(samples, &active, out)?;
    Ok(HostFigures {
        amplification,
        restarts,
    })
}

fn report_cgroup(
    active: &[&Sample],
    runs: &[Run],
    out: &mut impl Write,
) -> io::Result<Amplification> {
    let cgroup: Vec<Section<'_>> = active
        .iter()
        .filter_map(|(t, sample)| Some((*t, sample_identity(sample)?, section(sample, "cgroup")?)))
        .collect();
    let anon = cgroup
        .iter()
        .map(|(_, _, values)| finite_or_zero(values, "anon"))
        .fold(0.0, f64::max)
        / MIB;
    let page_cache = cgroup
        .iter()
        .map(|(_, _, values)| finite_or_zero(values, "file"))
        .fold(0.0, f64::max)
        / MIB;
    match (
        segmented_json_growth(&cgroup, "cpu_user_us"),
        segmented_json_growth(&cgroup, "cpu_system_us"),
    ) {
        (Some((user, user_wall)), Some((system, system_wall))) => {
            let wall = user_wall.min(system_wall);
            writeln!(
                out,
                "cgroup cpu: user {:.2} % + system {:.2} % of one core; anon max {anon:.1} MiB, \
                 page cache max {page_cache:.0} MiB",
                user / 1e6 / wall * 100.0,
                system / 1e6 / wall * 100.0
            )?;
        }
        _ => writeln!(
            out,
            "cgroup cpu: unmeasured (need two compatible active samples)"
        )?,
    }
    let amplification = Amplification::of(&cgroup, runs);
    let read = segmented_json_growth(&cgroup, "io_rbytes").map(|(bytes, _)| bytes);
    writeln!(
        out,
        "cgroup bytes: written {} GiB and read {} GiB over {} % of the sampled span; payload \
         over the same intervals {} GiB -> write amplification {}",
        summary::decimals(
            (amplification.covered > 0.0).then_some(amplification.written / GIB),
            1
        ),
        summary::decimals(read.map(|bytes| bytes / GIB), 2),
        summary::decimals(amplification.coverage_pct(), 1),
        summary::decimals(
            (amplification.covered > 0.0).then_some(amplification.payload / GIB),
            1
        ),
        summary::decimals(amplification.ratio(), 4)
    )?;
    Ok(amplification)
}

fn report_disk_and_pressure(samples: &[Sample], out: &mut impl Write) -> io::Result<()> {
    let disk: Vec<Section<'_>> = samples
        .iter()
        .filter_map(|(t, sample)| Some((*t, "disk".to_owned(), section(sample, "disk")?)))
        .collect();
    if let (Some((sectors, _)), Some((weighted, _))) = (
        segmented_json_growth(&disk, "sectors_written"),
        segmented_json_growth(&disk, "weighted_io_ms"),
    ) {
        writeln!(
            out,
            "disk total writes {:.1} GiB (all users of the disk), weighted io ms growth {:.0} s",
            sectors * 512.0 / GIB,
            weighted / 1000.0
        )?;
    }
    let psi: Vec<&Value> = samples
        .iter()
        .filter_map(|(_, sample)| section(sample, "psi"))
        .collect();
    if !psi.is_empty() {
        let max = |key: &str| {
            psi.iter()
                .map(|values| finite_or_zero(values, key))
                .fold(0.0, f64::max)
        };
        writeln!(
            out,
            "psi io full avg10 max={:.1} memory some avg10 max={:.1} cpu some avg10 max={:.1}",
            max("io_full_avg10"),
            max("memory_some_avg10"),
            max("cpu_some_avg10")
        )?;
    }
    Ok(())
}

fn report_network(samples: &[Sample], active: &[&Sample], out: &mut impl Write) -> io::Result<()> {
    let nic: Vec<Section<'_>> = samples
        .iter()
        .filter_map(|(t, sample)| Some((*t, "nic".to_owned(), section(sample, "nic")?)))
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
        .map(|key| {
            let growth = segmented_json_growth(&nic, key)
                .map_or_else(|| "unknown".to_owned(), |(value, _)| format!("{value:.0}"));
            format!("{key}={growth}")
        })
        .collect();
        writeln!(out, "nic growth: {}", items.join(" "))?;
    }
    let tcp: Vec<&Value> = active
        .iter()
        .filter_map(|(_, sample)| section(sample, "tcp"))
        .filter(|values| values.get("rtsp").is_some())
        .collect();
    if tcp.is_empty() {
        return Ok(());
    }
    for class in ["rtsp", "http"] {
        let field = |values: &Value, key: &str| {
            finite_or_zero(values.get(class).unwrap_or(&Value::Null), key)
        };
        let conns: Vec<f64> = tcp.iter().map(|values| field(values, "conns")).collect();
        let retransmits = summary::growth(
            &tcp.iter()
                .map(|values| field(values, "retrans_total"))
                .collect::<Vec<_>>(),
        );
        let rtt: Vec<f64> = tcp
            .iter()
            .filter(|values| field(values, "conns") > 0.0)
            .map(|values| field(values, "rtt_mean_ms"))
            .collect();
        let rtt_max = tcp
            .iter()
            .map(|values| field(values, "rtt_max_ms"))
            .fold(0.0, f64::max);
        writeln!(
            out,
            "tcp {class}: conns {}..{} retrans growth {} rtt mean {} ms max {rtt_max:.1} ms",
            summary::minimum(&conns).unwrap_or(0.0) as i64,
            summary::maximum(&conns).unwrap_or(0.0) as i64,
            retransmits as i64,
            summary::decimals(summary::mean(&rtt), 1)
        )?;
    }
    Ok(())
}

fn report_clock_and_process(
    samples: &[Sample],
    active: &[&Sample],
    out: &mut impl Write,
) -> io::Result<()> {
    let chrony: Vec<f64> = samples
        .iter()
        .filter_map(|(_, sample)| sample.get("chrony")?.get("offset_s")?.as_f64())
        .map(f64::abs)
        .collect();
    if let Some(max) = summary::maximum(&chrony) {
        writeln!(out, "ntp |offset| max {:.3} ms", max * 1000.0)?;
    }
    let process: Vec<Section<'_>> = active
        .iter()
        .filter_map(|(t, sample)| Some((*t, sample_identity(sample)?, section(sample, "proc")?)))
        .collect();
    if !process.is_empty() {
        let growth = |key: &str| {
            segmented_json_growth(&process, key)
                .map_or_else(|| "unknown".to_owned(), |(value, _)| format!("{value:.0}"))
        };
        writeln!(
            out,
            "proc VmHWM max {:.1} MiB, majflt growth {}, nonvoluntary switches growth {}",
            process
                .iter()
                .map(|(_, _, values)| finite_or_zero(values, "VmHWM"))
                .fold(0.0, f64::max)
                / 1024.0,
            growth("majflt"),
            growth("nonvoluntary_ctxt_switches")
        )?;
    }
    let load: Vec<f64> = samples
        .iter()
        .filter_map(|(_, sample)| sample.get("host")?.get("load1")?.as_f64())
        .collect();
    if let Some(max) = summary::maximum(&load) {
        writeln!(out, "load1 max {max:.1}")?;
    }
    Ok(())
}

fn status_of(value: &Value, key: &str) -> i64 {
    value.get(key).and_then(Value::as_i64).unwrap_or(0)
}

/// An API check problem: when it was seen (none for the whole series), what
/// it concerns, and the detail.
type ApiProblem = (Option<f64>, String, String);

/// The playlist summary fields an API check needs; a missing one is a problem.
const PLAYLIST_FIELDS: [&str; 6] = [
    "media_segments",
    "maps",
    "range_breaks",
    "gaps",
    "discontinuities",
    "duration_s",
];

/// The problems one stream's check shows. A discontinuity or a short playlist
/// is expected while the playlist still spans a daemon start.
fn stream_problems(name: &str, stream: &Value, after_restart: bool) -> Option<String> {
    if status_of(stream, "segments_status") != 200 || status_of(stream, "playlist_status") != 200 {
        return Some(format!(
            "segments_status={} playlist_status={}",
            finite_or_zero(stream, "segments_status"),
            finite_or_zero(stream, "playlist_status")
        ));
    }
    let Some(playlist) = stream
        .get("playlist")
        .filter(|playlist| playlist.is_object())
    else {
        return Some(format!("{name} has no playlist summary"));
    };
    let mut field = BTreeMap::new();
    for key in PLAYLIST_FIELDS {
        let Some(value) = playlist.get(key).and_then(Value::as_f64) else {
            return Some(format!("playlist summary lacks {key}"));
        };
        field.insert(key, value);
    }
    let bad = field["media_segments"] < 1.0
        || field["maps"] < 1.0
        || field["range_breaks"] > 0.0
        || field["gaps"] > 0.0
        || (field["discontinuities"] > 0.0 && !after_restart)
        || (field["duration_s"] < MIN_PLAYLIST_SECONDS && !after_restart);
    bad.then(|| playlist.to_string())
}

/// The problems of one API check at `t`.
fn check_problems(t: f64, check: &Value, runs: &[Run], problems: &mut Vec<ApiProblem>) {
    let mut push = |name: &str, detail: String| problems.push((Some(t), name.to_owned(), detail));
    if let Some(error) = check.get("error") {
        push("check", error.to_string());
        return;
    }
    let after_restart = runs.iter().any(|run| {
        run.began_with_startup
            && t - RESTART_HORIZON_SECONDS <= run.first_seen
            && run.first_seen <= t
    });
    let health = check.get("health").unwrap_or(&Value::Null);
    if status_of(health, "status") != 200 {
        push("health", health.to_string());
    }
    let metrics = check.get("metrics").unwrap_or(&Value::Null);
    if status_of(metrics, "status") != 200 {
        push("metrics", metrics.to_string());
    } else if let Some(values) = metrics.get("values").and_then(Value::as_object) {
        if !values.contains_key("irisd_catalog_batch_failures_total") {
            push("metrics", "missing catalog failure counter".into());
        }
        for (name, value) in values {
            if value.as_f64().is_none_or(|value| value != 0.0) {
                push(name, value.to_string());
            }
        }
    } else {
        push("metrics", "missing metrics values".into());
    }
    let cameras = check.get("cameras").unwrap_or(&Value::Null);
    if status_of(cameras, "status") != 200 {
        push("cameras", cameras.to_string());
    }
    let expected_cameras = cameras.get("count").and_then(Value::as_u64).unwrap_or(0);
    if expected_cameras == 0 {
        push("cameras", "camera discovery is empty".into());
    }
    let expected_streams = cameras
        .get("expected_streams")
        .and_then(Value::as_u64)
        .unwrap_or(expected_cameras);
    let Some(streams) = check.get("streams").and_then(Value::as_object) else {
        push("streams", "missing streams object".into());
        return;
    };
    if u64::try_from(streams.len()).ok() != Some(expected_streams) {
        push(
            "streams",
            format!(
                "{} stream checks, expected {expected_streams}",
                streams.len()
            ),
        );
    }
    for (name, stream) in streams {
        if let Some(detail) = stream_problems(name, stream, after_restart) {
            push(name, detail);
        }
    }
}

fn report_api(
    checks: &[Sample],
    completeness: Completeness,
    runs: &[Run],
    out: &mut impl Write,
) -> io::Result<usize> {
    writeln!(out, "\n== api checks ({}, {completeness})", checks.len())?;
    let mut problems: Vec<ApiProblem> = Vec::new();
    if checks.is_empty() || completeness != Completeness::Complete {
        problems.push((
            None,
            "series".into(),
            format!(
                "API check series is {completeness} and has {} rows",
                checks.len()
            ),
        ));
    }
    for (t, check) in checks {
        check_problems(*t, check, runs, &mut problems);
    }
    writeln!(out, "problems: {}", problems.len())?;
    for (t, name, detail) in problems.iter().take(30) {
        let timestamp = t.map_or_else(|| "-".to_owned(), time::iso);
        writeln!(
            out,
            "   {timestamp} {name} {}",
            summary::truncate(detail, 160)
        )?;
    }
    Ok(problems.len())
}

/// One row of the criteria table: its name, the measured value, the target.
type Criterion = (&'static str, String, &'static str);

/// The criteria about runs, streams, and feeds. The targets come from
/// `docs/research/08-roadmap.md` (phase 2: a 7-day soak with 6 cameras × 2
/// streams) and the budgets in `docs/research/07-architecture.md §7`.
fn availability_criteria(runs: &[Run], host: &HostFigures, api_problems: usize) -> Vec<Criterion> {
    let gaps = notable(runs, Notable::Gap).len();
    let disconnected = |windows: fn(&Run) -> Box<dyn Iterator<Item = &Window> + '_>| {
        runs.iter()
            .flat_map(|run| windows(run).flat_map(|window| &window.rows))
            .filter(|(_, fields)| {
                fields
                    .get("connected")
                    .is_some_and(|value| value == "false")
            })
            .count()
    };
    let hours = match (runs.first(), runs.last()) {
        (Some(first), Some(last)) => (last.stop.unwrap_or(last.last_seen) - first.start) / 3_600.0,
        _ => 0.0,
    };
    let streams: BTreeSet<&String> = runs.iter().flat_map(|run| run.streams.keys()).collect();
    let free = runs
        .iter()
        .flat_map(|run| run.roots.values())
        .flat_map(|window| summary::col(&window.rows, "free_pct"))
        .collect::<Vec<_>>();
    vec![
        (
            "window hours / streams",
            format!("{hours:.1} / {}", streams.len()),
            ">= 168 / 12",
        ),
        (
            "runs / supervisor restarts in the window",
            format!("{} / {}", runs.len(), summary::decimals(host.restarts, 0)),
            "1 / 0",
        ),
        (
            "gaps",
            format!(
                "{gaps} gap lines, {} reconnects",
                display_counter(series_growth(runs, stream_windows, "reconnects"))
            ),
            "every gap explained by a logged cause",
        ),
        (
            "stream drops / disconnected samples",
            format!(
                "{} / {}",
                display_counter(series_growth(runs, stream_windows, "dropped")),
                disconnected(stream_windows)
            ),
            "0 / 0",
        ),
        (
            "feed errors / disconnected samples",
            format!(
                "{} / {}",
                display_counter(series_growth(runs, feed_windows, "errors")),
                disconnected(feed_windows)
            ),
            "0 / 0",
        ),
        (
            "root free_pct min",
            summary::decimals(summary::minimum(&free), 1),
            "above storage.min_free_percent",
        ),
        ("API check problems", api_problems.to_string(), "0"),
    ]
}

/// The criteria about the host and process resources.
fn resource_criteria(runs: &[Run], host: &HostFigures) -> Vec<Criterion> {
    let process = window_rows(runs, |run| &run.process);
    let amplification = &host.amplification;
    vec![
        (
            "write amplification (coverage %)",
            format!(
                "{} ({})",
                summary::decimals(amplification.ratio(), 4),
                summary::decimals(amplification.coverage_pct(), 1)
            ),
            "<= 1.01",
        ),
        (
            "container overhead %",
            summary::decimals(DaemonBytes::of(runs).overhead_pct(), 4),
            "<= 0.1",
        ),
        (
            "cpu mean % of one core",
            summary::decimals(summary::mean(&summary::col(&process, "cpu_pct")), 2),
            "<= 2 at 56 Mbps (budget), measured at the soak's bitrate",
        ),
        (
            "rss max MiB",
            summary::decimals(summary::col_max(&process, "rss_mib"), 1),
            "<= 61.0 (64 MB) and flat",
        ),
        (
            "rss slope MiB/day (last run)",
            summary::decimals(
                runs.last()
                    .and_then(|run| slope_per_day(&run.process.rows, "rss_mib")),
                2,
            ),
            "~0",
        ),
        (
            "loop lag max ms",
            summary::decimals(summary::col_max(&process, "loop_lag_max_ms"), 0),
            "well under the 60 s watchdog",
        ),
        (
            "retention pass max ms",
            summary::decimals(summary::col_max(&process, "retention_max_ms"), 0),
            "bounded",
        ),
    ]
}

/// The criteria about retention, the catalog, the writer, and the journal.
fn storage_criteria(runs: &[Run]) -> Vec<Criterion> {
    let writer = window_rows(runs, |run| &run.writer);
    let catalog = window_rows(runs, |run| &run.catalog);
    let retention_failures = notable(runs, Notable::Retention)
        .iter()
        .filter(|pass| pass.text.starts_with("retention pass failed"))
        .count();
    let growth =
        |pick: fn(&Run) -> &Window, key: &str| display_counter(window_growth(runs, pick, key));
    vec![
        ("retention failures", retention_failures.to_string(), "0"),
        (
            "catalog failures / rejected",
            format!(
                "{} / {}",
                growth(|run| &run.catalog, "failures"),
                growth(|run| &run.catalog, "rejected")
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
            summary::decimals(summary::col_max(&catalog, "txn_p99_ms"), 1),
            "< 10",
        ),
        (
            "event commit p99 max ms",
            summary::decimals(summary::col_max(&catalog, "event_commit_p99_ms"), 1),
            "< 1000",
        ),
        (
            "writer dropped / errors / slow",
            format!(
                "{} / {} / {}",
                growth(|run| &run.writer, "dropped"),
                growth(|run| &run.writer, "write_errors"),
                growth(|run| &run.writer, "slow_io")
            ),
            "0 / 0 / few",
        ),
        (
            "sync max ms",
            summary::decimals(summary::col_max(&writer, "sync_max_ms"), 1),
            "absorbed by the queue",
        ),
        (
            "watchdog withheld / selfcheck failed / skipped",
            format!(
                "{} / {} / {}",
                growth(|run| &run.process, "watchdog_withheld"),
                growth(|run| &run.process, "selfcheck_failed"),
                growth(|run| &run.process, "selfcheck_skipped")
            ),
            "0 / 0 / 0",
        ),
        (
            "WARN/ERROR journal problems",
            runs.iter()
                .map(|run| run.problems.len())
                .sum::<usize>()
                .to_string(),
            "0",
        ),
    ]
}

fn write_report(
    journal: &Journal,
    host: &LoadedSamples,
    api: &LoadedSamples,
    out: &mut impl Write,
) -> io::Result<()> {
    let runs = &journal.runs;
    report_runs(journal, out)?;
    report_process(runs, out)?;
    report_writer(runs, out)?;
    report_catalog(runs, out)?;
    report_streams(runs, out)?;
    report_event_feeds(runs, out)?;
    report_notable(runs, out)?;
    let figures = report_host(&host.rows, host.completeness, runs, out)?;
    let api_problems = report_api(&api.rows, api.completeness, runs, out)?;
    writeln!(out, "\n== criteria")?;
    for (name, value, target) in availability_criteria(runs, &figures, api_problems)
        .into_iter()
        .chain(resource_criteria(runs, &figures))
        .chain(storage_criteria(runs))
    {
        writeln!(out, "   {name:46} {value:>28}   target: {target}")?;
    }
    out.flush()
}

/// Reads `journal.log`, `host.jsonl`, and `api.jsonl` from the sampler
/// directory and writes the report for the window to standard output. A
/// missing journal, or one without service lines in the window, is an error;
/// a missing, truncated, or corrupt sampler series is reported in the output.
pub fn run(args: &Args) -> anyhow::Result<()> {
    let since = args.since.unwrap_or(f64::NEG_INFINITY);
    let until = args.until.unwrap_or(f64::INFINITY);
    anyhow::ensure!(since <= until, "--since is after --until");
    let journal = parse_journal(&args.dir.join("journal.log"), since, until)?;
    anyhow::ensure!(!journal.runs.is_empty(), "no service lines in the window");
    let host = load_jsonl(&args.dir.join("host.jsonl"), since, until)?;
    let api = load_jsonl(&args.dir.join("api.jsonl"), since, until)?;
    let stdout = io::stdout();
    let mut out = BufWriter::new(stdout.lock());
    match write_report(&journal, &host, &api, &mut out) {
        Err(error) if error.kind() == ErrorKind::BrokenPipe => Ok(()),
        other => other.context("writing the report"),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn parse_fixture(text: &str, since: f64, until: f64) -> Vec<Run> {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("journal.log");
        std::fs::write(&path, text).unwrap();
        parse_journal(&path, since, until).unwrap().runs
    }

    fn whole(text: &str) -> Vec<Run> {
        parse_fixture(text, f64::NEG_INFINITY, f64::INFINITY)
    }

    fn at(text: &str) -> f64 {
        time::parse_seconds(text).unwrap()
    }

    fn rendered(write: impl FnOnce(&mut Vec<u8>) -> io::Result<()>) -> String {
        let mut output = Vec::new();
        write(&mut output).unwrap();
        String::from_utf8(output).unwrap()
    }

    fn runs_report(runs: Vec<Run>) -> String {
        rendered(|out| {
            report_runs(
                &Journal {
                    runs,
                    unreadable: 0,
                },
                out,
            )
        })
    }

    // Lines as the daemon writes them, with identities replaced.
    const RECONCILED: &str = "catalog reconciled with the recording roots finished_deletions=0";
    const PROCESS: &str = "process cpu_pct=2.50 cpu_user_pct=1.17 cpu_sys_pct=1.33 rss_mib=60.0 \
                           threads=3 fds=80 uptime_min=1 loop_lag_max_ms=3 watchdog_withheld=0 \
                           teardowns=0 retention_max_ms=226 selfcheck_ok=0 selfcheck_failed=0 \
                           selfcheck_skipped=0 report_timeouts=0 retention_timeouts=0 \
                           sandbox_enforced=2";

    fn events(t: &str, source: &str, counts: [u32; 5], idle: u32, pull_p99: u32) -> String {
        let [events, messages, reconnects, renewals, errors] = counts;
        format!(
            "{t}  INFO events camera=cam source={source} connected=true events={events} \
             messages={messages} reconnects={reconnects} renewals={renewals} errors={errors} \
             idle_s={idle} pull_p99_ms={pull_p99} pull_max_ms=-\n"
        )
    }

    #[test]
    fn event_totals_remain_separate_by_source_across_counter_resets() {
        let text = [
            events("2026-11-01T00:00:00Z", "DahuaCgi", [10, 11, 1, 0, 0], 2, 0),
            events(
                "2026-11-01T00:00:01Z",
                "OnvifPullPoint",
                [2, 4, 3, 1, 2],
                3,
                20,
            ),
            "2026-11-01T00:02:00Z  INFO SIGTERM; stopping\n".to_owned(),
            "2026-11-01T00:02:01Z  INFO stopped\n".to_owned(),
            events(
                "2026-11-01T01:00:00Z",
                "OnvifPullPoint",
                [5, 7, 1, 2, 1],
                4,
                30,
            ),
            events("2026-11-01T01:00:01Z", "DahuaCgi", [3, 4, 0, 0, 0], 1, 0),
        ]
        .concat();
        let runs = whole(&text);
        assert_eq!(runs.len(), 2);
        assert!(runs.iter().all(|run| run.events.len() == 2));
        let output = rendered(|out| report_event_feeds(&runs, out));
        let row = |source: &str| -> Vec<String> {
            output
                .lines()
                .find(|line| line.contains(source))
                .unwrap()
                .split_whitespace()
                .map(str::to_owned)
                .collect()
        };
        assert_eq!(
            row("DahuaCgi"),
            [
                "cam", "DahuaCgi", "13", "15", "1", "0", "0", "2.0", "0.0", "-"
            ]
        );
        assert_eq!(
            row("OnvifPullPoint"),
            [
                "cam",
                "OnvifPullPoint",
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
        let runs = whole(&format!(
            "2026-11-01T00:00:00Z  INFO {RECONCILED} recovered=0 adopted=0\n\
             2026-11-01T00:01:00Z  INFO {PROCESS}\n\
             2026-11-01T03:00:00Z  WARN sandbox disabled by --no-sandbox\n\
             2026-11-01T03:00:10Z  INFO {RECONCILED} recovered=1 adopted=0\n\
             2026-11-01T03:01:00Z  INFO {PROCESS}\n"
        ));
        assert_eq!(runs.len(), 2);
        assert_eq!(runs[0].last_seen, at("2026-11-01T00:01:00Z"));
        assert_eq!(runs[1].start, at("2026-11-01T03:00:00Z"));
        assert_eq!(runs[0].problems, []);
        assert_eq!(runs[1].problems.len(), 1);
        assert!(runs_report(runs).contains(
            "unobserved interval (includes possible downtime) before this run: 179.0 min"
        ));
    }

    #[test]
    fn startup_prefix_stages_stay_with_their_instance() {
        for prefix in [
            "2026-11-01T03:00:00Z  INFO descriptor limit soft=Some(65536) hard=Some(65536)\n\
             2026-11-01T03:00:00Z  WARN recording root unavailable at startup root=a error=x\n\
             2026-11-01T03:00:01Z  WARN recording root unavailable at startup root=b error=x\n\
             2026-11-01T03:00:02Z  INFO sandbox fully enforced\n\
             2026-11-01T03:00:03Z  INFO catalog open path=fixture.db\n",
            "2026-11-01T03:00:00Z  WARN the configuration file holds credentials and is \
             accessible to other users; restrict it to mode 0600 path=x mode=0644\n\
             2026-11-01T03:00:00Z  INFO raised the descriptor soft limit to the hard limit\n\
             2026-11-01T03:00:00Z  WARN restricted state permissions to the service account\n\
             2026-11-01T03:00:00Z  WARN restricted state permissions to the service account\n\
             2026-11-01T03:00:02Z  WARN sandbox partially enforced: the kernel lacks later \
             Landlock rights kernel_abi=V5 missing=x\n\
             2026-11-01T03:00:03Z  INFO catalog open path=fixture.db\n",
            "2026-11-01T03:00:00Z  INFO catalog open path=fixture.db\n",
        ] {
            let runs = whole(&format!(
                "2026-11-01T00:01:00Z  INFO {PROCESS}\n{prefix}\
                 2026-11-01T03:00:10Z  INFO {RECONCILED} recovered=1 adopted=0\n"
            ));
            assert_eq!(runs.len(), 2, "{prefix}");
            assert_eq!(runs[0].last_seen, at("2026-11-01T00:01:00Z"), "{prefix}");
            assert_eq!(runs[1].start, at("2026-11-01T03:00:00Z"), "{prefix}");
            assert!(runs[0].restarted_at.is_some(), "{prefix}");
            assert!(runs[1].began_with_startup, "{prefix}");
            assert_eq!(runs[1].recovered.as_ref().unwrap()["recovered"], "1");
        }
    }

    #[test]
    fn runtime_warnings_before_a_crash_remain_with_the_prior_run() {
        let runs = whole(&format!(
            "2026-11-01T00:01:00Z  INFO {PROCESS}\n\
             2026-11-01T00:02:00Z  WARN recording root unavailable root=fixture\n\
             2026-11-01T00:02:01Z  WARN camera disconnected camera=fixture\n\
             2026-11-01T03:00:00Z  INFO sandbox fully enforced\n\
             2026-11-01T03:00:10Z  INFO {RECONCILED} recovered=1 adopted=0\n"
        ));
        assert_eq!(runs.len(), 2);
        assert_eq!(runs[0].last_seen, at("2026-11-01T00:02:01Z"));
        assert_eq!(runs[0].problems.len(), 2);
        assert_eq!(runs[1].problems, []);
    }

    #[test]
    fn a_warning_alone_does_not_prove_a_restart() {
        let runs = whole(&format!(
            "2026-11-01T00:01:00Z  INFO {PROCESS}\n\
             2026-11-01T03:00:00Z  WARN camera disconnected camera=fixture\n\
             2026-11-01T03:01:00Z  INFO {PROCESS}\n"
        ));
        assert_eq!(runs.len(), 1);
        assert!(runs[0].restarted_at.is_none());
        assert_eq!(runs[0].process.rows.len(), 2);
        assert_eq!(runs[0].problems.len(), 1);
    }

    #[test]
    fn a_clean_stop_keeps_its_end_before_the_next_startup() {
        let runs = whole(&format!(
            "2026-11-01T00:01:00Z  INFO {PROCESS}\n\
             2026-11-01T00:02:00Z  INFO SIGTERM; stopping\n\
             2026-11-01T00:02:01Z  INFO stopped\n\
             2026-11-01T03:00:00Z  WARN sandbox disabled by --no-sandbox\n\
             2026-11-01T03:00:10Z  INFO {RECONCILED} recovered=0 adopted=0\n"
        ));
        assert_eq!(runs.len(), 2);
        assert!(runs[0].clean);
        assert!(runs[0].restarted_at.is_none());
        assert_eq!(runs[0].stop, Some(at("2026-11-01T00:02:01Z")));
        assert_eq!(runs[1].problems.len(), 1);
        let report = runs_report(runs);
        assert!(report.contains("stopped cleanly"));
        assert!(report.contains("downtime before this run: 178.0 min"));
    }

    #[test]
    fn a_partial_window_preserves_startup_boundaries_without_reconciliation() {
        let runs = parse_fixture(
            &format!(
                "2026-11-01T00:00:00Z  INFO {RECONCILED} recovered=0 adopted=0\n\
                 2026-11-01T00:01:00Z  INFO {PROCESS}\n\
                 2026-11-01T03:00:00Z  WARN sandbox disabled by --no-sandbox\n\
                 2026-11-01T03:00:10Z  INFO {RECONCILED} recovered=1 adopted=0\n"
            ),
            at("2026-11-01T00:00:30Z"),
            at("2026-11-01T03:00:05Z"),
        );
        assert_eq!(runs.len(), 2);
        assert!(runs[0].recovered.is_none());
        assert!(runs[0].restarted_at.is_some());
        assert_eq!(runs[1].problems.len(), 1);
        assert!(runs[1].recovered.is_none());
    }

    #[test]
    fn a_restart_after_the_window_does_not_mark_the_run() {
        let runs = parse_fixture(
            &format!(
                "2026-11-01T00:01:00Z  INFO {PROCESS}\n\
                 2026-11-01T03:00:00Z  INFO sandbox fully enforced\n"
            ),
            f64::NEG_INFINITY,
            at("2026-11-01T01:00:00Z"),
        );
        assert_eq!(runs.len(), 1);
        assert!(runs[0].restarted_at.is_none());
        assert!(runs_report(runs).contains(" running;"));
    }

    #[test]
    fn repeated_startup_attempts_with_equal_timestamps_stay_separate() {
        let runs = whole(concat!(
            "2026-11-01T00:00:00Z  WARN sandbox disabled by --no-sandbox\n",
            "2026-11-01T00:00:00Z  INFO catalog open path=fixture.db\n",
            "2026-11-01T00:00:00Z  WARN recording root unavailable at startup root=a error=x\n",
            "2026-11-01T00:00:00Z  WARN recording root unavailable at startup root=b error=x\n",
            "2026-11-01T00:00:00Z  WARN sandbox disabled by --no-sandbox\n",
            "2026-11-01T00:00:00Z  INFO catalog open path=fixture.db\n",
            "2026-11-01T00:00:00Z  INFO catalog open path=fixture.db\n",
            "2026-11-01T00:00:00Z  INFO catalog reconciled with the recording roots recovered=1\n",
            "2026-11-01T00:00:00Z  INFO catalog reconciled with the recording roots recovered=2\n",
        ));
        assert_eq!(runs.len(), 4);
        assert_eq!(runs[0].problems.len(), 1);
        assert_eq!(runs[1].problems.len(), 3);
        assert!(runs[..3].iter().all(|run| run.restarted_at.is_some()));
        assert_eq!(runs[2].recovered.as_ref().unwrap()["recovered"], "1");
        assert_eq!(runs[3].recovered.as_ref().unwrap()["recovered"], "2");
        assert!(!runs_report(runs).contains("-0.0 min"));
    }

    #[test]
    fn reconciliation_after_a_crash_starts_a_separate_run() {
        let runs = whole(&format!(
            "2026-11-01T00:00:00Z  INFO {RECONCILED} recovered=0 adopted=0\n\
             2026-11-01T00:01:00Z  INFO stream stream=cam/main mib=10 reconnects=3\n\
             2026-11-01T00:02:00Z  INFO {RECONCILED} recovered=1 adopted=0\n\
             2026-11-01T00:03:00Z  INFO stream stream=cam/main mib=10 reconnects=0\n"
        ));
        assert_eq!(runs.len(), 2);
        assert!(runs[0].restarted_at.is_some());
        assert_eq!(runs[0].recovered.as_ref().unwrap()["recovered"], "0");
        assert_eq!(runs[1].recovered.as_ref().unwrap()["recovered"], "1");
        assert_eq!(series_growth(&runs, stream_windows, "reconnects"), Some(3));
        let report = runs_report(runs);
        assert!(report.contains("restarted without a stopped line"));
        assert!(report.contains("unobserved interval (includes possible downtime)"));
    }

    #[test]
    fn a_capture_starting_mid_run_still_detects_the_next_crash_restart() {
        let runs = whole(&format!(
            "2026-11-01T00:01:00Z  INFO {PROCESS}\n\
             2026-11-01T00:02:00Z  INFO {RECONCILED} recovered=1 adopted=0\n"
        ));
        assert_eq!(runs.len(), 2);
        assert!(runs[0].restarted_at.is_some());
        assert!(!runs[0].began_with_startup);
    }

    #[test]
    fn unknown_ssrc_warnings_are_counted_apart_from_problems() {
        let runs = whole(&format!(
            "2026-11-01T00:01:00Z  INFO {PROCESS}\n\
             2026-11-01T00:01:01Z  WARN saw unknown rtcp ssrc; ignoring ssrc=1\n"
        ));
        assert_eq!(runs[0].problems, []);
        assert_eq!(runs[0].unknown_ssrc_warnings, 1);
        assert!(runs_report(runs).contains("1 `unknown rtcp ssrc` warnings are not counted"));
    }

    #[test]
    fn an_unreadable_journal_line_is_skipped_and_counted() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("journal.log");
        let mut text = format!("2026-11-01T00:01:00Z  INFO {PROCESS}\n").into_bytes();
        text.extend_from_slice(b"2026-13-45T00:00:00Z  INFO bad month\n\xff\xfe\n");
        std::fs::write(&path, text).unwrap();
        let journal = parse_journal(&path, f64::NEG_INFINITY, f64::INFINITY).unwrap();
        assert_eq!(journal.runs.len(), 1);
        assert_eq!(journal.unreadable, 2);
    }

    #[test]
    fn a_truncated_final_record_is_ignored_and_reported() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("samples.jsonl");
        std::fs::write(
            &path,
            b"{\"ts\":\"2026-11-01T00:00:00Z\",\"ok\":true}\n{\"ts\":",
        )
        .unwrap();
        let loaded = load_jsonl(&path, f64::NEG_INFINITY, f64::INFINITY).unwrap();
        assert_eq!(loaded.rows.len(), 1);
        assert_eq!(loaded.completeness, Completeness::IncompleteFinalRecord);
    }

    #[test]
    fn corrupt_records_are_skipped_and_reported() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("samples.jsonl");
        std::fs::write(
            &path,
            b"{\"ts\":\"2026-11-01T00:00:00Z\"}\n{not json}\n\n{\"ts\":\"never\"}\n",
        )
        .unwrap();
        let loaded = load_jsonl(&path, f64::NEG_INFINITY, f64::INFINITY).unwrap();
        assert_eq!(loaded.rows.len(), 1);
        assert_eq!(
            loaded.completeness,
            Completeness::Corrupt { lines: 3, first: 2 }
        );
    }

    #[test]
    fn a_missing_series_is_reported_as_missing() {
        let directory = tempfile::tempdir().unwrap();
        let missing = load_jsonl(
            &directory.path().join("missing.jsonl"),
            f64::NEG_INFINITY,
            f64::INFINITY,
        )
        .unwrap();
        assert_eq!(missing.rows, []);
        assert_eq!(missing.completeness, Completeness::Missing);
    }

    fn row(t: f64, frames: u32) -> Row {
        (t, Fields::from([("frames".to_owned(), frames.to_string())]))
    }

    #[test]
    fn the_window_keeps_its_rows_and_the_newest_earlier_row() {
        let (since, until) = (100.0, 200.0);
        let mut window = Window::default();
        // Rows before the window, a clock step back across `since`, rows after `until`.
        for (t, frames) in [
            (10.0, 1),
            (90.0, 3),
            (120.0, 4),
            (95.0, 5),
            (150.0, 6),
            (210.0, 7),
            (190.0, 9),
        ] {
            window.push(row(t, frames), since, until);
        }
        assert_eq!(window.baseline, Some(row(95.0, 5)));
        assert_eq!(window.rows, [row(120.0, 4), row(150.0, 6), row(190.0, 9)]);
    }

    fn writer(t: &str, fragments: u64, mib: &str) -> String {
        format!(
            "{t}  INFO writer fragments={fragments} mib={mib} syncs=1 dropped=0 write_errors=0 \
             files=1 queued=0 queue_peak=1 queue_mib_peak=0.1 write_p50_ms=0.1 write_p99_ms=0.2 \
             write_max_ms=0.3 sync_p50_ms=1.0 sync_p99_ms=2.0 sync_max_ms=3.0 slow_io=0\n"
        )
    }

    #[test]
    fn windowed_totals_start_from_the_baseline() {
        let text = [
            writer("2026-11-01T00:00:00Z", 10, "1.0"),
            writer("2026-11-01T00:01:00Z", 20, "2.0"),
            writer("2026-11-01T00:02:00Z", 35, "3.5"),
            writer("2026-11-01T00:03:00Z", 50, "5.0"),
        ]
        .concat();
        let runs = parse_fixture(&text, at("2026-11-01T00:01:30Z"), f64::INFINITY);
        assert_eq!(runs[0].writer.rows.len(), 2);
        let output = rendered(|out| report_writer(&runs, out));
        assert!(
            output.contains("totals: fragments=30 mib=3.0 syncs=0 files=0 "),
            "{output}"
        );
    }

    #[test]
    fn a_baseline_row_changes_no_windowed_gauge() {
        let text = format!(
            "2026-11-01T00:00:00Z  INFO stream stream=cam/main connected=false frames=0 \
             idle_s=900.0\n\
             2026-11-01T00:00:00Z  INFO catalog segments=1 gib=1.0 txn_p99_ms=50.0 \
             reconcile_required=true\n\
             2026-11-01T00:00:00Z  INFO process cpu_pct=90 rss_mib=500 loop_lag_max_ms=900\n\
             2026-11-01T00:02:00Z  INFO stream stream=cam/main connected=true frames=60 \
             idle_s=0.0\n\
             2026-11-01T00:02:00Z  INFO catalog segments=1 gib=1.0 txn_p99_ms=2.0 \
             reconcile_required=false\n\
             2026-11-01T00:02:00Z  INFO {PROCESS}\n"
        );
        let runs = parse_fixture(&text, at("2026-11-01T00:01:00Z"), f64::INFINITY);
        let host = HostFigures::default();
        let criteria: BTreeMap<&str, String> = availability_criteria(&runs, &host, 0)
            .into_iter()
            .chain(resource_criteria(&runs, &host))
            .chain(storage_criteria(&runs))
            .map(|(name, value, _)| (name, value))
            .collect();
        assert_eq!(
            criteria["stream drops / disconnected samples"],
            "unknown / 0"
        );
        assert_eq!(criteria["catalog txn p99 max ms"], "2.0");
        assert_eq!(criteria["catalog reconciliation required"], "0");
        assert_eq!(criteria["loop lag max ms"], "3");
        assert_eq!(criteria["rss max MiB"], "60.0");
    }

    #[test]
    fn writer_mib_totals_keep_their_decimal_across_runs_and_resets() {
        let text = [
            writer("2026-11-01T00:00:00Z", 100, "35994.8"),
            writer("2026-11-01T00:01:00Z", 110, "36000.3"),
            "2026-11-01T00:02:00Z  INFO SIGTERM; stopping\n".to_owned(),
            "2026-11-01T00:02:01Z  INFO stopped\n".to_owned(),
            writer("2026-11-01T01:00:00Z", 5, "10.5"),
            // A counter reset inside a run adds nothing for its drop.
            writer("2026-11-01T01:01:00Z", 7, "2.0"),
            writer("2026-11-01T01:02:00Z", 9, "4.5"),
        ]
        .concat();
        let runs = whole(&text);
        assert_eq!(runs.len(), 2);
        let output = rendered(|out| report_writer(&runs, out));
        // 36000.3 counted from its start, plus 10.5 and the 2.5 after the reset.
        assert!(output.contains(" mib=36013.3 "), "{output}");
        assert!(output.contains("fragments=119 "), "{output}");
    }

    #[test]
    fn a_run_already_going_at_the_window_start_counts_only_its_growth() {
        let text = [
            writer("2026-11-01T00:00:00Z", 100, "35994.8"),
            writer("2026-11-01T00:01:00Z", 110, "36000.3"),
            "2026-11-01T00:02:00Z  INFO SIGTERM; stopping\n".to_owned(),
            "2026-11-01T00:02:01Z  INFO stopped\n".to_owned(),
            writer("2026-11-01T01:00:00Z", 5, "10.5"),
            writer("2026-11-01T01:01:00Z", 7, "2.0"),
            writer("2026-11-01T01:02:00Z", 9, "4.5"),
        ]
        .concat();
        let runs = parse_fixture(&text, at("2026-11-01T00:00:30Z"), f64::INFINITY);
        // 5.5 of growth in the first run; the second starts inside the window (13.0).
        assert!(rendered(|out| report_writer(&runs, out)).contains(" mib=18.5 "));
    }

    #[test]
    fn a_negative_mib_counter_renders_unknown() {
        let runs = whole(&writer("2026-11-01T00:00:00Z", 1, "-1.0"));
        assert!(rendered(|out| report_writer(&runs, out)).contains(" mib=unknown "));
    }

    #[test]
    fn container_overhead_is_the_file_bytes_beyond_the_payload_in_percent() {
        let text = format!(
            "2026-11-01T00:00:00Z  INFO stream stream=cam/main frames=0 mib=0.0\n\
             {}2026-11-01T00:01:00Z  INFO stream stream=cam/main frames=60 mib=1000.0\n{}",
            writer("2026-11-01T00:00:00Z", 0, "0.0"),
            writer("2026-11-01T00:01:00Z", 1, "1001.0")
        );
        let overhead = DaemonBytes::of(&whole(&text)).overhead_pct().unwrap();
        assert!((overhead - 0.1).abs() < 1e-9, "{overhead}");
    }

    #[test]
    fn the_earliest_daemons_process_line_carries_the_file_counters() {
        let text = "2026-11-01T00:00:00Z  INFO stream stream=cam/main frames=0 mib=0.0\n\
                    2026-11-01T00:00:00Z  INFO process cpu_pct=1 fragments=0 mib=0.0\n\
                    2026-11-01T00:01:00Z  INFO stream stream=cam/main frames=60 mib=1000.0\n\
                    2026-11-01T00:01:00Z  INFO process cpu_pct=1 fragments=9 mib=1002.0\n";
        let overhead = DaemonBytes::of(&whole(text)).overhead_pct().unwrap();
        assert!((overhead - 0.2).abs() < 1e-9, "{overhead}");
    }

    #[test]
    fn a_run_without_file_counters_is_left_out_of_the_overhead() {
        let text = format!(
            "2026-11-01T00:00:00Z  INFO stream stream=cam/main frames=0 mib=0.0\n\
             2026-11-01T00:00:00Z  INFO {PROCESS}\n\
             2026-11-01T00:01:00Z  INFO stream stream=cam/main frames=60 mib=500.0\n\
             2026-11-01T00:02:00Z  INFO SIGTERM; stopping\n\
             2026-11-01T00:02:01Z  INFO stopped\n\
             2026-11-01T01:00:00Z  INFO stream stream=cam/main frames=0 mib=0.0\n\
             {}2026-11-01T01:01:00Z  INFO stream stream=cam/main frames=60 mib=1000.0\n{}",
            writer("2026-11-01T01:00:00Z", 0, "0.0"),
            writer("2026-11-01T01:01:00Z", 1, "1001.0")
        );
        let bytes = DaemonBytes::of(&whole(&text));
        assert_eq!(bytes.runs, (1, 2));
        assert!((bytes.overhead_pct().unwrap() - 0.1).abs() < 1e-9);
    }

    #[test]
    fn malformed_counters_render_unknown_instead_of_zero() {
        let runs = whole(concat!(
            "2026-11-01T00:00:00Z  INFO stream stream=cam/main frames=1 reconnects=1 gaps=0 \
             holds=0 snaps=0 loss=0\n",
            "2026-11-01T00:01:00Z  INFO stream stream=cam/main frames=2 gaps=0 holds=0 snaps=0 \
             loss=0\n",
        ));
        assert!(rendered(|out| report_streams(&runs, out)).contains("unknown"));
    }

    #[test]
    fn the_first_and_last_hours_follow_the_timestamps() {
        let rows: Vec<Row> = (0..=8).map(|i| row(f64::from(i) * 1_200.0, i)).collect();
        assert_eq!(first_hour(&rows).len(), 3);
        assert_eq!(last_hour(&rows).len(), 3);
    }

    #[test]
    fn self_checks_count_runs_failures_and_skips() {
        let logged = |text: &str| Logged {
            at: 0.0,
            level: "WARN".into(),
            text: text.into(),
        };
        let lines = [
            logged("selfcheck ok stream=cam/main"),
            logged("selfcheck failed stream=cam/main"),
            logged("selfcheck task failed err=x"),
            logged("selfcheck skipped: catalog query failed stream=cam/main"),
        ];
        let checks = SelfChecks::of(&lines.iter().collect::<Vec<_>>());
        assert_eq!(
            (checks.passed, checks.failed.len(), checks.skipped),
            (1, 2, 1)
        );
    }

    fn api_check(playlist: &Value) -> Value {
        json!({
            "health": { "status": 200 },
            "metrics": { "status": 200, "values": { "irisd_catalog_batch_failures_total": 0 } },
            "cameras": { "status": 200, "count": 1, "expected_streams": 1 },
            "streams": {
                "cam/main": {
                    "segments_status": 200, "playlist_status": 200, "playlist": playlist.clone()
                }
            }
        })
    }

    fn playlist(discontinuities: u32) -> Value {
        json!({
            "media_segments": 1, "maps": 1, "range_breaks": 0, "gaps": 0,
            "discontinuities": discontinuities, "duration_s": 600
        })
    }

    fn api_problems(checks: &[Sample], runs: &[Run]) -> String {
        rendered(|out| report_api(checks, Completeness::Complete, runs, out).map(|_| ()))
    }

    fn started_run(first_seen: f64, began_with_startup: bool) -> Run {
        Run {
            last_seen: first_seen,
            ..Run::starting(first_seen, began_with_startup)
        }
    }

    #[test]
    fn a_success_status_cannot_hide_a_missing_playlist_summary() {
        let t = at("2026-11-01T00:00:00Z");
        let mut check = api_check(&Value::Null);
        check["streams"]["cam/main"]
            .as_object_mut()
            .unwrap()
            .remove("playlist");
        let output = api_problems(&[(t, check)], &[started_run(t, true)]);
        assert!(output.contains("problems: 1"), "{output}");
    }

    #[test]
    fn a_playlist_summary_missing_a_field_is_a_problem() {
        let t = at("2026-11-01T00:00:00Z");
        let mut summary = playlist(0);
        summary.as_object_mut().unwrap().remove("gaps");
        let output = api_problems(&[(t, api_check(&summary))], &[started_run(t, true)]);
        assert!(output.contains("playlist summary lacks gaps"), "{output}");
    }

    #[test]
    fn restart_discontinuities_are_tolerated_only_while_they_remain_in_the_horizon() {
        let started = at("2026-11-01T00:00:00Z");
        let checks = [
            (started + 60.0, api_check(&playlist(1))),
            (
                started + RESTART_HORIZON_SECONDS + 1.0,
                api_check(&playlist(1)),
            ),
        ];
        let output = api_problems(&checks, &[started_run(started, true)]);
        assert!(output.contains("problems: 1"), "{output}");
    }

    #[test]
    fn a_window_start_inside_a_run_does_not_excuse_discontinuities() {
        let started = at("2026-11-01T00:00:00Z");
        let since = started + 3_600.0;
        let mut run = started_run(started, true);
        run.last_seen = since + 600.0;
        run.clip(since, f64::INFINITY);
        let output = api_problems(&[(since + 60.0, api_check(&playlist(1)))], &[run]);
        assert!(output.contains("problems: 1"), "{output}");
    }

    #[test]
    fn a_capture_start_does_not_excuse_discontinuities() {
        let started = at("2026-11-01T00:00:00Z");
        let output = api_problems(
            &[(started + 60.0, api_check(&playlist(1)))],
            &[started_run(started, false)],
        );
        assert!(output.contains("problems: 1"), "{output}");
    }

    #[test]
    fn reconciliation_required_counts_true_samples() {
        let rows = vec![
            (
                0.0,
                Fields::from([("reconcile_required".to_owned(), "false".to_owned())]),
            ),
            (
                1.0,
                Fields::from([("reconcile_required".to_owned(), "true".to_owned())]),
            ),
        ];
        assert_eq!(true_sample_count(&rows, "reconcile_required"), Some(1));
    }

    #[test]
    fn a_missing_boolean_makes_the_count_unknown() {
        assert_eq!(
            true_sample_count(&[row(0.0, 1)], "reconcile_required"),
            None
        );
    }

    #[test]
    fn write_amplification_compares_bytes_and_payload_over_the_same_intervals() {
        let text = "2026-11-01T00:00:00Z  INFO stream stream=cam/main frames=0 mib=0.0\n\
                    2026-11-01T00:10:00Z  INFO stream stream=cam/main frames=1 mib=600.0\n";
        let runs = whole(text);
        let sample = |minute: u32, written_mib: u64, pid: u64| -> (f64, String, Value) {
            (
                at("2026-11-01T00:00:00Z") + f64::from(minute) * 60.0,
                format!("{pid}"),
                json!({ "io_wbytes": written_mib * 1_048_576 }),
            )
        };
        // Two minutes, a five-minute sampler gap that is skipped, then two more.
        let samples = [
            sample(0, 0, 1),
            sample(1, 61, 1),
            sample(2, 122, 1),
            sample(7, 427, 1),
            sample(8, 488, 1),
        ];
        let sections: Vec<Section<'_>> = samples
            .iter()
            .map(|(t, id, value)| (*t, id.clone(), value))
            .collect();
        let amplification = Amplification::of(&sections, &runs);
        assert!((amplification.ratio().unwrap() - 61.0 / 60.0).abs() < 1e-9);
        assert!((amplification.coverage_pct().unwrap() - 37.5).abs() < 1e-9);
    }

    #[test]
    fn an_inverted_window_is_refused() {
        let directory = tempfile::tempdir().unwrap();
        let args = Args {
            dir: directory.path().to_owned(),
            since: Some(10.0),
            until: Some(5.0),
        };
        assert!(
            run(&args)
                .unwrap_err()
                .to_string()
                .contains("--since is after --until")
        );
    }

    #[test]
    fn supervisor_restarts_count_growth_inside_the_window() {
        let sample = |t: f64, restarts: u64| (t, json!({ "unit": { "NRestarts": restarts } }));
        let samples = [sample(0.0, 1), sample(60.0, 1), sample(120.0, 2)];
        let figures =
            report_host(&samples, Completeness::Complete, &[], &mut Vec::<u8>::new()).unwrap();
        assert_eq!(figures.restarts, Some(1.0));
    }
}
