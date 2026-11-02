//! Parsing of the daemon's log lines: ANSI colour stripped, the timestamp
//! and level split off, and `key=value` pairs read.

use std::collections::BTreeMap;

/// Removes ANSI SGR colour sequences.
pub fn strip_ansi(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' && chars.peek() == Some(&'[') {
            chars.next();
            for d in chars.by_ref() {
                if d == 'm' {
                    break;
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// A log line split into its timestamp, level, and the rest.
#[derive(Clone, Debug, PartialEq)]
pub struct Line<'a> {
    pub ts: &'a str,
    pub level: &'a str,
    pub rest: &'a str,
}

const LEVELS: [&str; 5] = ["INFO", "WARN", "ERROR", "DEBUG", "TRACE"];

/// Splits `<ts> <LEVEL> <rest>` where the timestamp is the daemon's RFC 3339
/// form; `None` for any other line.
pub fn split(line: &str) -> Option<Line<'_>> {
    let mut parts = line.splitn(3, |c: char| c.is_whitespace());
    let ts = parts.next()?;
    if ts.len() < 20 || ts.as_bytes()[4] != b'-' || !ts.ends_with('Z') {
        return None;
    }
    let after = line[ts.len()..].trim_start();
    let level = after.split_whitespace().next()?;
    if !LEVELS.contains(&level) {
        return None;
    }
    let rest = after[level.len()..].trim_start();
    Some(Line { ts, level, rest })
}

/// Reads every `key=value` pair; a quoted value loses its quotes.
pub fn kv(text: &str) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    let mut rest = text;
    while let Some(eq) = rest.find('=') {
        let key_start = rest[..eq]
            .rfind(|c: char| !(c.is_alphanumeric() || c == '_'))
            .map_or(0, |i| i + 1);
        let key = &rest[key_start..eq];
        let after = &rest[eq + 1..];
        let (value, consumed) = if let Some(q) = after.strip_prefix('"') {
            let end = q.find('"').unwrap_or(q.len());
            (&q[..end], end + 2)
        } else {
            let end = after.find(char::is_whitespace).unwrap_or(after.len());
            (&after[..end], end)
        };
        if !key.is_empty() {
            out.insert(key.to_owned(), value.to_owned());
        }
        rest = &after[consumed.min(after.len())..];
    }
    out
}

/// A numeric field, if present and numeric.
pub fn number(fields: &BTreeMap<String, String>, key: &str) -> Option<f64> {
    fields.get(key).and_then(|v| v.parse::<f64>().ok())
}

/// `-` for a missing value, else the value to `digits` decimals.
pub fn fmt(value: Option<f64>, digits: usize) -> String {
    value.map_or_else(|| "-".to_owned(), |v| format!("{v:.digits$}"))
}

/// The largest numeric value of `key` over the rows.
pub fn col_max(rows: &[(f64, BTreeMap<String, String>)], key: &str) -> Option<f64> {
    rows.iter()
        .filter_map(|(_, d)| number(d, key))
        .fold(None, |m, v| Some(m.map_or(v, |m: f64| m.max(v))))
}

/// The numeric values of `key` over the rows.
pub fn col(rows: &[(f64, BTreeMap<String, String>)], key: &str) -> Vec<f64> {
    rows.iter().filter_map(|(_, d)| number(d, key)).collect()
}

/// The arithmetic mean, `None` of an empty list.
pub fn mean(values: &[f64]) -> Option<f64> {
    if values.is_empty() {
        None
    } else {
        Some(values.iter().sum::<f64>() / values.len() as f64)
    }
}

/// The median (the mean of the two middle values for an even count).
pub fn median(values: &[f64]) -> Option<f64> {
    if values.is_empty() {
        return None;
    }
    let mut v = values.to_vec();
    v.sort_by(f64::total_cmp);
    let n = v.len();
    Some(if n % 2 == 1 {
        v[n / 2]
    } else {
        f64::midpoint(v[n / 2 - 1], v[n / 2])
    })
}

/// Sum of the positive steps: a counter's growth across resets.
pub fn growth(values: &[f64]) -> f64 {
    values.windows(2).map(|w| (w[1] - w[0]).max(0.0)).sum()
}

/// Frames per second between consecutive samples of one stream.
pub fn rates(rows: &[(f64, BTreeMap<String, String>)]) -> Vec<f64> {
    rows.windows(2)
        .filter_map(|w| {
            let (t0, a) = &w[0];
            let (t1, b) = &w[1];
            let fa = number(a, "frames")?;
            let fb = number(b, "frames")?;
            (t1 > t0).then(|| (fb - fa) / (t1 - t0))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_lines_and_reads_pairs() {
        let line = strip_ansi(
            "\x1b[2m2026-11-02T01:45:23.123456Z\x1b[0m  INFO stream stream=cam1/main connected=true fps=29.97 name=\"a b\"",
        );
        let l = split(&line).unwrap();
        assert_eq!(l.ts, "2026-11-02T01:45:23.123456Z");
        assert_eq!(l.level, "INFO");
        assert!(l.rest.starts_with("stream stream=cam1/main"));
        let d = kv(l.rest);
        assert_eq!(d["stream"], "cam1/main");
        assert_eq!(d["connected"], "true");
        assert_eq!(number(&d, "fps"), Some(29.97));
        assert_eq!(d["name"], "a b");
        assert!(split("irisd.service: Deactivated successfully.").is_none());
    }

    #[test]
    fn statistics_helpers() {
        assert_eq!(median(&[3.0, 1.0, 2.0]), Some(2.0));
        assert_eq!(median(&[1.0, 2.0, 3.0, 4.0]), Some(2.5));
        assert_eq!(growth(&[1.0, 3.0, 2.0, 5.0]), 5.0);
        assert_eq!(mean(&[]), None);
    }
}
