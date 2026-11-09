//! `daemon-stats`: the summary of one `irisd run` log.

use std::collections::hash_map::Entry;
use std::collections::{BTreeMap, HashMap};
use std::fs::File;
use std::io::{self, BufReader, BufWriter, ErrorKind, Write};
use std::path::PathBuf;

use anyhow::Context;
use clap::Parser;

use crate::logs::{self, Fields, LineReader, Series, Statistics};
use crate::summary::{self, Row};
use crate::time;

/// The prefixes of the one-off lines listed as notable.
const NOTABLE: [&str; 4] = ["gap ", "selfcheck ", "retention pass", "slow "];
/// Notable lines listed before the rest are only counted.
const NOTABLE_SHOWN: usize = 200;
/// Characters shown of each notable line.
const NOTABLE_CHARS: usize = 220;
/// Characters shown of each other line.
const OTHER_CHARS: usize = 200;
/// Characters of a line, timestamps masked, that decide whether it repeats
/// another; this bounds what each distinct line costs.
const OTHER_KEY_CHARS: usize = 1_024;

/// Arguments of `daemon-stats`.
#[derive(Debug, Parser)]
pub struct Args {
    /// The log: an `irisd run` capture or a `journalctl -o cat` export.
    log: PathBuf,
}

/// An event feed: its camera and source.
type Feed = (String, String);

/// Everything a log holds, grouped by line kind.
#[derive(Debug, Default)]
struct Parsed {
    /// Each stream's rows, by name.
    streams: BTreeMap<String, Vec<Row>>,
    /// Each event feed's rows.
    events: BTreeMap<Feed, Vec<Row>>,
    /// The writer's rows.
    writer: Vec<Row>,
    /// The catalog's rows.
    catalog: Vec<Row>,
    /// The process rows.
    process: Vec<Row>,
    /// Each recording root's rows.
    roots: BTreeMap<String, Vec<Row>>,
    /// The first [`NOTABLE_SHOWN`] notable lines, each cut to [`NOTABLE_CHARS`].
    notable: Vec<String>,
    /// How many notable lines the log holds.
    notable_count: usize,
    /// Every other line, deduplicated.
    other: Distinct,
}

impl Parsed {
    fn push(&mut self, series: Series, row: Row) {
        match series {
            Series::Stream(name) => self.streams.entry(name).or_default().push(row),
            Series::Events { camera, source } => {
                self.events.entry((camera, source)).or_default().push(row);
            }
            Series::Root(root) => self.roots.entry(root).or_default().push(row),
            Series::Writer => self.writer.push(row),
            Series::Catalog => self.catalog.push(row),
            Series::Process => self.process.push(row),
        }
    }

    fn push_notable(&mut self, line: &str) {
        self.notable_count += 1;
        if self.notable.len() < NOTABLE_SHOWN {
            self.notable
                .push(summary::truncate(line, NOTABLE_CHARS).to_owned());
        }
    }
}

/// Distinct lines in first-seen order, each with its count and its first
/// occurrence cut to [`OTHER_CHARS`].
#[derive(Debug, Default)]
struct Distinct {
    index: HashMap<String, usize>,
    lines: Vec<(usize, String)>,
}

impl Distinct {
    fn add(&mut self, line: &str) {
        match self
            .index
            .entry(dedup_key(summary::truncate(line, OTHER_KEY_CHARS)))
        {
            Entry::Occupied(entry) => self.lines[*entry.get()].0 += 1,
            Entry::Vacant(entry) => {
                entry.insert(self.lines.len());
                self.lines
                    .push((1, summary::truncate(line, OTHER_CHARS).to_owned()));
            }
        }
    }
}

/// Sorts a log's lines into the groups.
fn parse(lines: impl Iterator<Item = String>) -> Parsed {
    let mut parsed = Parsed::default();
    for raw in lines {
        let line = logs::strip_ansi(&raw);
        let Some(split) = logs::split(&line) else {
            parsed.other.add(&line);
            continue;
        };
        let normalized = format!("{} {} {}", split.timestamp, split.level, split.rest);
        let Some(t) = time::parse_seconds(split.timestamp) else {
            parsed.other.add(&normalized);
            continue;
        };
        match logs::statistics(&split) {
            Some(Statistics::Row(series, fields)) => parsed.push(series, (t, fields)),
            None if NOTABLE.iter().any(|prefix| split.rest.starts_with(prefix)) => {
                parsed.push_notable(&normalized);
            }
            Some(Statistics::Malformed(_)) | None => parsed.other.add(&normalized),
        }
    }
    parsed
}

fn get<'a>(fields: &'a Fields, key: &str) -> &'a str {
    fields.get(key).map_or("-", String::as_str)
}

fn pairs(fields: &Fields, keys: &[&str]) -> String {
    let items: Vec<String> = keys
        .iter()
        .map(|key| format!("{key}={}", get(fields, key)))
        .collect();
    items.join(" ")
}

/// Writes the summary: the process, writer, catalog, and root figures, a
/// table per stream and per event feed, the notable lines, and the other
/// lines deduplicated.
fn render(parsed: &Parsed, out: &mut impl Write) -> io::Result<()> {
    render_process(&parsed.process, out)?;
    render_writer(&parsed.writer, out)?;
    render_catalog(&parsed.catalog, out)?;
    render_roots(&parsed.roots, out)?;
    render_streams(&parsed.streams, out)?;
    render_events(&parsed.events, out)?;
    render_notable(parsed, out)?;
    render_other(&parsed.other, out)
}

fn render_process(rows: &[Row], out: &mut impl Write) -> io::Result<()> {
    let (Some((first, _)), Some((last, fields))) = (rows.first(), rows.last()) else {
        return Ok(());
    };
    let cpu = summary::col(rows, "cpu_pct");
    let rss = summary::col(rows, "rss_mib");
    writeln!(
        out,
        "process samples={} span={:.1} min",
        rows.len(),
        (last - first) / 60.0
    )?;
    writeln!(
        out,
        "cpu_pct min={} mean={} max={}  rss_mib min={} max={}  loop_lag_max_ms={}  \
         retention_max_ms={}  fds_max={}",
        summary::decimals(summary::minimum(&cpu), 2),
        summary::decimals(summary::mean(&cpu), 2),
        summary::decimals(summary::maximum(&cpu), 2),
        summary::decimals(summary::minimum(&rss), 1),
        summary::decimals(summary::maximum(&rss), 1),
        summary::decimals(summary::col_max(rows, "loop_lag_max_ms"), 0),
        summary::decimals(summary::col_max(rows, "retention_max_ms"), 0),
        summary::decimals(summary::col_max(rows, "fds"), 0),
    )?;
    writeln!(
        out,
        "final process: {}",
        pairs(
            fields,
            &[
                "threads",
                "fds",
                "watchdog_withheld",
                "teardowns",
                "selfcheck_ok",
                "selfcheck_failed",
                "selfcheck_skipped"
            ]
        )
    )
}

fn render_writer(rows: &[Row], out: &mut impl Write) -> io::Result<()> {
    let Some((_, fields)) = rows.last() else {
        return Ok(());
    };
    writeln!(
        out,
        "writer write_p99_max_ms={} write_max_ms={} sync_p99_max_ms={} sync_max_ms={} \
         queue_peak={} queue_mib_peak={}",
        summary::decimals(summary::col_max(rows, "write_p99_ms"), 1),
        summary::decimals(summary::col_max(rows, "write_max_ms"), 1),
        summary::decimals(summary::col_max(rows, "sync_p99_ms"), 1),
        summary::decimals(summary::col_max(rows, "sync_max_ms"), 1),
        summary::decimals(summary::col_max(rows, "queue_peak"), 0),
        summary::decimals(summary::col_max(rows, "queue_mib_peak"), 1),
    )?;
    writeln!(
        out,
        "final writer: {}",
        pairs(
            fields,
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
    )
}

fn render_catalog(rows: &[Row], out: &mut impl Write) -> io::Result<()> {
    let Some((_, fields)) = rows.last() else {
        return Ok(());
    };
    writeln!(
        out,
        "catalog txn_p99_max_ms={} txn_max_ms={} query_max_ms={} wait_max_ms={} \
         event_commit_p99_max_ms={} event_commit_max_ms={} batch_peak={} queue_peak={} \
         queue_mib_peak={}",
        summary::decimals(summary::col_max(rows, "txn_p99_ms"), 1),
        summary::decimals(summary::col_max(rows, "txn_max_ms"), 1),
        summary::decimals(summary::col_max(rows, "query_max_ms"), 1),
        summary::decimals(summary::col_max(rows, "wait_max_ms"), 1),
        summary::decimals(summary::col_max(rows, "event_commit_p99_ms"), 1),
        summary::decimals(summary::col_max(rows, "event_commit_max_ms"), 1),
        summary::decimals(summary::col_max(rows, "batch_peak"), 0),
        summary::decimals(summary::col_max(rows, "queue_peak"), 0),
        summary::decimals(summary::col_max(rows, "queue_mib_peak"), 1),
    )?;
    writeln!(
        out,
        "final catalog: {}",
        pairs(
            fields,
            &[
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
                "coalesced",
                "rejected",
                "rejected_events",
                "rejected_progress",
                "rejected_queries",
                "rejected_lifecycle",
                "reconcile_required",
                "batches",
                "failures"
            ]
        )
    )
}

fn render_roots(roots: &BTreeMap<String, Vec<Row>>, out: &mut impl Write) -> io::Result<()> {
    for (root, rows) in roots {
        let Some((_, fields)) = rows.last() else {
            continue;
        };
        let free = summary::col(rows, "free_pct");
        writeln!(
            out,
            "root {root} free_pct min={} max={} last_free_gib={}",
            summary::decimals(summary::minimum(&free), 1),
            summary::decimals(summary::maximum(&free), 1),
            get(fields, "free_gib")
        )?;
    }
    Ok(())
}

fn render_streams(streams: &BTreeMap<String, Vec<Row>>, out: &mut impl Write) -> io::Result<()> {
    writeln!(
        out,
        "\nstream           fps_med fps_min fps_max stalls  frames fragments reconn loss \
         drop holds gaps cam_ms idle_max connected"
    )?;
    for (name, rows) in streams {
        let Some((_, fields)) = rows.last() else {
            continue;
        };
        let rates = summary::rates(rows);
        let (median, stalls) = summary::median_and_stalls(&rates);
        writeln!(
            out,
            "{name:16} {median:7.2} {:>7} {:>7} {stalls:6} {:>7} {:>9} {:>6} {:>4} {:>4} {:>5} \
             {:>4} {:>6} {:>8} {}",
            summary::decimals(summary::minimum(&rates), 2),
            summary::decimals(summary::maximum(&rates), 2),
            get(fields, "frames"),
            get(fields, "fragments"),
            get(fields, "reconnects"),
            get(fields, "loss"),
            get(fields, "dropped"),
            get(fields, "holds"),
            get(fields, "gaps"),
            get(fields, "cam_offset_ms"),
            summary::decimals(summary::col_max(rows, "idle_s"), 1),
            get(fields, "connected"),
        )?;
    }
    Ok(())
}

fn render_events(events: &BTreeMap<Feed, Vec<Row>>, out: &mut impl Write) -> io::Result<()> {
    if events.is_empty() {
        return Ok(());
    }
    writeln!(
        out,
        "\nevents camera       source             events messages reconnects renewals errors \
         idle_max pull_p99_max pull_max connected"
    )?;
    for ((camera, source), rows) in events {
        let Some((_, fields)) = rows.last() else {
            continue;
        };
        writeln!(
            out,
            "{camera:12} {source:18} {:>7} {:>8} {:>10} {:>8} {:>6} {:>8} {:>12} {:>8} {}",
            get(fields, "events"),
            get(fields, "messages"),
            get(fields, "reconnects"),
            get(fields, "renewals"),
            get(fields, "errors"),
            summary::decimals(summary::col_max(rows, "idle_s"), 1),
            summary::decimals(summary::col_max(rows, "pull_p99_ms"), 1),
            summary::decimals(summary::col_max(rows, "pull_max_ms"), 1),
            get(fields, "connected"),
        )?;
    }
    Ok(())
}

fn render_notable(parsed: &Parsed, out: &mut impl Write) -> io::Result<()> {
    if parsed.notable_count == 0 {
        return Ok(());
    }
    writeln!(
        out,
        "\n--- gaps, self-checks, retention passes, slow I/O ({}) ---",
        parsed.notable_count
    )?;
    for line in &parsed.notable {
        writeln!(out, "{line}")?;
    }
    if parsed.notable_count > parsed.notable.len() {
        writeln!(
            out,
            "... {} more",
            parsed.notable_count - parsed.notable.len()
        )?;
    }
    Ok(())
}

fn render_other(other: &Distinct, out: &mut impl Write) -> io::Result<()> {
    writeln!(
        out,
        "\n--- other lines (deduplicated, first occurrence shown) ---"
    )?;
    for (count, first) in &other.lines {
        writeln!(out, "{count:4}x  {first}")?;
    }
    Ok(())
}

/// The line without its first word, normally the timestamp, and with
/// embedded RFC 3339 timestamps masked.
fn dedup_key(line: &str) -> String {
    let body = line.split_once(' ').map_or(line, |(_, rest)| rest);
    let mut out = String::with_capacity(body.len());
    let mut rest = body;
    while let Some(position) = find_timestamp(rest) {
        out.push_str(&rest[..position]);
        out.push_str("<ts>");
        let after = &rest[position..];
        let end = after.find('Z').map_or(after.len(), |index| index + 1);
        rest = &after[end..];
    }
    out.push_str(rest);
    out
}

/// The start of the first `YYYY-MM-DDTHH:` that has at least 19 bytes after it.
fn find_timestamp(text: &str) -> Option<usize> {
    let b = text.as_bytes();
    (0..b.len().saturating_sub(19)).find(|&i| {
        b[i..i + 4].iter().all(u8::is_ascii_digit)
            && b[i + 4] == b'-'
            && b[i + 7] == b'-'
            && b[i + 10] == b'T'
            && b[i + 13] == b':'
    })
}

/// Reads LOG line by line and writes the summary to standard output. A read
/// failure, or a line that is not UTF-8 or is longer than 1 MiB, ends the run
/// with an error; a closed pipe ends the output without one.
pub fn run(args: &Args) -> anyhow::Result<()> {
    let file = File::open(&args.log).with_context(|| format!("opening {}", args.log.display()))?;
    // Parse while reading so the log is never held whole; the first read
    // failure ends the input and fails the run.
    let mut reader = LineReader::new(BufReader::new(file));
    let mut failure: Option<anyhow::Error> = None;
    let lines = std::iter::from_fn(|| match reader.next_line() {
        Ok(Some(line)) => match std::str::from_utf8(line.bytes) {
            Ok(text) => Some(text.to_owned()),
            Err(error) => {
                failure = Some(anyhow::Error::new(error).context(format!(
                    "reading {} line {}",
                    args.log.display(),
                    line.number
                )));
                None
            }
        },
        Ok(None) => None,
        Err(error) => {
            failure =
                Some(anyhow::Error::new(error).context(format!("reading {}", args.log.display())));
            None
        }
    });
    let parsed = parse(lines);
    if let Some(error) = failure {
        return Err(error);
    }
    let stdout = io::stdout();
    let mut out = BufWriter::new(stdout.lock());
    match render(&parsed, &mut out).and_then(|()| out.flush()) {
        Err(error) if error.kind() == ErrorKind::BrokenPipe => Ok(()),
        other => other.context("writing the summary"),
    }
}

#[cfg(test)]
mod tests {
    use std::fmt::Write as _;

    use super::*;

    fn parse_text(text: &str) -> Parsed {
        parse(text.lines().map(str::to_owned))
    }

    fn rendered(parsed: &Parsed) -> String {
        let mut output = Vec::new();
        render(parsed, &mut output).unwrap();
        String::from_utf8(output).unwrap()
    }

    const EVENTS: &str = concat!(
        "2026-11-01T00:00:00Z  INFO events camera=cam source=DahuaCgi connected=true events=10 ",
        "messages=11 reconnects=1 renewals=0 errors=0 idle_s=3.7 pull_p99_ms=- pull_max_ms=-\n",
        "2026-11-01T00:00:01Z  INFO events camera=cam source=OnvifPullPoint connected=false ",
        "events=2 messages=4 reconnects=3 renewals=1 errors=2 idle_s=1.7 pull_p99_ms=5000.0 ",
        "pull_max_ms=4104.6\n",
        "2026-11-01T00:00:02Z  INFO events camera=cam source=DahuaCgi connected=true events=12 ",
        "messages=13 reconnects=1 renewals=0 errors=0 idle_s=0.5 pull_p99_ms=- pull_max_ms=-\n",
        "2026-11-01T00:00:03Z  INFO events camera=cam source=OnvifPullPoint connected=true ",
        "events=1 messages=2 reconnects=0 renewals=0 errors=0 idle_s=0.1 pull_p99_ms=10.0 ",
        "pull_max_ms=9.0\n",
    );

    #[test]
    fn event_rows_remain_separate_by_camera_and_source() {
        let parsed = parse_text(EVENTS);
        assert_eq!(parsed.events.len(), 2);
        for source in ["DahuaCgi", "OnvifPullPoint"] {
            assert_eq!(
                parsed.events[&("cam".to_owned(), source.to_owned())].len(),
                2,
                "{source}"
            );
        }
    }

    #[test]
    fn an_event_feed_shows_its_last_counters_and_its_peaks() {
        let output = rendered(&parse_text(EVENTS));
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
                "cam", "DahuaCgi", "12", "13", "1", "0", "0", "3.7", "-", "-", "true"
            ]
        );
        assert_eq!(
            row("OnvifPullPoint"),
            [
                "cam",
                "OnvifPullPoint",
                "1",
                "2",
                "0",
                "0",
                "0",
                "1.7",
                "5000.0",
                "4104.6",
                "true"
            ]
        );
    }

    #[test]
    fn missing_numeric_series_never_render_as_infinity() {
        let output = rendered(&parse_text(concat!(
            "2026-11-01T00:00:00Z INFO process threads=1\n",
            "2026-11-01T00:00:00Z INFO stream stream=cam/main frames=1\n",
        )));
        assert!(!output.contains("inf"));
        assert!(output.contains("cpu_pct min=- mean=- max=-"));
    }

    #[test]
    fn other_lines_repeat_when_only_their_timestamps_differ() {
        let parsed = parse_text(concat!(
            "2026-11-01T00:00:00Z WARN probe failed at=2026-11-01T00:00:00Z\n",
            "2026-11-01T00:00:09Z WARN probe failed at=2026-11-01T00:00:09Z\n",
            "2026-11-01T00:00:10Z WARN another line\n",
        ));
        assert_eq!(parsed.other.lines.len(), 2);
        assert_eq!(parsed.other.lines[0].0, 2);
        assert!(parsed.other.lines[0].1.ends_with("at=2026-11-01T00:00:00Z"));
    }

    #[test]
    fn notable_lines_beyond_the_cap_are_counted() {
        let mut text = String::new();
        for i in 0..NOTABLE_SHOWN + 3 {
            writeln!(
                text,
                "2026-11-01T00:00:00Z INFO selfcheck ok stream=cam/main ms={i}"
            )
            .unwrap();
        }
        let parsed = parse_text(&text);
        assert_eq!(parsed.notable.len(), NOTABLE_SHOWN);
        assert!(rendered(&parsed).contains("... 3 more"));
    }

    #[test]
    fn a_malformed_statistics_row_is_listed_with_the_other_lines() {
        let parsed =
            parse_text("2026-11-01T00:00:00Z INFO catalog open path=/srv/state/irisd.db\n");
        assert_eq!(parsed.catalog, []);
        assert_eq!(parsed.other.lines.len(), 1);
    }
}
