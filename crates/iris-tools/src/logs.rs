//! Parsing of the daemon's log lines: terminal controls removed, the
//! timestamp and level split off, and `key=value` pairs read.

use std::collections::BTreeMap;
use std::io::{BufRead, Read as _};

/// Longest input line the report tools accept; the daemon's lines are far shorter.
pub const MAX_LINE_BYTES: usize = 1 << 20;

/// One line from a [`LineReader`].
#[derive(Debug, PartialEq, Eq)]
pub struct InputLine<'a> {
    /// The 1-based line number.
    pub number: usize,
    /// The line without its `\n` or `\r\n` terminator.
    pub bytes: &'a [u8],
    /// Whether a newline ended the line; only a final line can lack one.
    pub terminated: bool,
}

/// Reads input one line at a time into one reused buffer, so a report holds
/// only what it keeps from each line rather than the whole file.
pub struct LineReader<R> {
    reader: R,
    buffer: Vec<u8>,
    number: usize,
}

impl<R: BufRead> LineReader<R> {
    /// Reads lines from `reader`.
    pub fn new(reader: R) -> Self {
        Self {
            reader,
            buffer: Vec::new(),
            number: 0,
        }
    }

    /// The next line, `None` at the end of the input. A line longer than
    /// [`MAX_LINE_BYTES`] is an `InvalidData` error.
    pub fn next_line(&mut self) -> std::io::Result<Option<InputLine<'_>>> {
        self.buffer.clear();
        let limit = MAX_LINE_BYTES as u64 + 1;
        let read = (&mut self.reader)
            .take(limit)
            .read_until(b'\n', &mut self.buffer)?;
        if read == 0 {
            return Ok(None);
        }
        self.number += 1;
        let terminated = self.buffer.last() == Some(&b'\n');
        if terminated {
            self.buffer.pop();
            if self.buffer.last() == Some(&b'\r') {
                self.buffer.pop();
            }
        } else if read > MAX_LINE_BYTES {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("line {} exceeds {MAX_LINE_BYTES} bytes", self.number),
            ));
        }
        Ok(Some(InputLine {
            number: self.number,
            bytes: &self.buffer,
            terminated,
        }))
    }
}

/// Removes complete ECMA-48 escape strings and makes the remaining text safe
/// to print to a terminal.  In particular, an unterminated control string can
/// never consume printable text that follows it.
pub fn strip_ansi(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let bytes = line.as_bytes();
    let mut index = 0usize;
    while index < bytes.len() {
        let c = line[index..]
            .chars()
            .next()
            .expect("index is on a character boundary");
        if c == '\x1b' {
            let next = index + 1;
            let complete = match bytes.get(next).copied() {
                Some(b'[') => bytes[next + 1..]
                    .iter()
                    .take(4_096)
                    .position(|byte| (b'@'..=b'~').contains(byte))
                    .map(|offset| next + 2 + offset),
                Some(b']' | b'P' | b'X' | b'^' | b'_') => {
                    let end = (next + 1)..bytes.len().min(next + 1 + 4_096);
                    let mut cursor = end.start;
                    let mut complete = None;
                    while cursor < end.end {
                        if bytes[cursor] == b'\x07' {
                            complete = Some(cursor + 1);
                            break;
                        }
                        if bytes[cursor] == b'\x1b' && bytes.get(cursor + 1) == Some(&b'\\') {
                            complete = Some(cursor + 2);
                            break;
                        }
                        cursor += 1;
                    }
                    complete
                }
                Some(_) => Some(next + 1),
                None => None,
            };
            if let Some(end) = complete {
                index = end;
                continue;
            }
        }
        if c.is_control() || is_bidi_control(c) {
            use std::fmt::Write as _;
            let _ = write!(out, "\\u{{{:x}}}", c as u32);
        } else {
            out.push(c);
        }
        index += c.len_utf8();
    }
    out
}

/// Escapes untrusted text without interpreting any of it as terminal control.
pub fn terminal_safe(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        if c.is_control() || is_bidi_control(c) {
            use std::fmt::Write as _;
            let _ = write!(out, "\\u{{{:x}}}", c as u32);
        } else {
            out.push(c);
        }
    }
    out
}

fn is_bidi_control(c: char) -> bool {
    matches!(
        c,
        '\u{061c}'
            | '\u{200e}'
            | '\u{200f}'
            | '\u{202a}'..='\u{202e}'
            | '\u{2066}'..='\u{2069}'
    )
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
            .char_indices()
            .rfind(|(_, c)| !(c.is_ascii_alphanumeric() || *c == '_'))
            .map_or(0, |(i, c)| i + c.len_utf8());
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

/// Reads the root identity from the daemon's root statistics row. Paths may
/// contain whitespace, so the following fixed field delimits the value.
pub fn root_identity(text: &str) -> Option<String> {
    let value = text.strip_prefix("root root=")?;
    value
        .rsplit_once(" free_gib=")
        .map(|(root, _)| root.to_owned())
        .filter(|root| !root.is_empty())
}

/// A numeric field, if present and numeric.
pub fn number(fields: &BTreeMap<String, String>, key: &str) -> Option<f64> {
    fields
        .get(key)
        .and_then(|v| v.parse::<f64>().ok())
        .filter(|v| v.is_finite())
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
            (t1 > t0 && fb >= fa).then(|| (fb - fa) / (t1 - t0))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn line_reader_matches_str_lines_and_bounds_each_line() {
        let text = "a\nb\r\n\nc\r";
        let mut reader = LineReader::new(text.as_bytes());
        let mut lines = Vec::new();
        while let Some(line) = reader.next_line().unwrap() {
            lines.push((
                line.number,
                String::from_utf8(line.bytes.to_vec()).unwrap(),
                line.terminated,
            ));
        }
        assert_eq!(
            lines,
            [
                (1, "a".to_owned(), true),
                (2, "b".to_owned(), true),
                (3, String::new(), true),
                (4, "c\r".to_owned(), false)
            ]
        );
        assert_eq!(
            lines
                .iter()
                .map(|(_, text, _)| text.as_str())
                .collect::<Vec<_>>(),
            text.lines().collect::<Vec<_>>()
        );

        let mut exact = vec![b'x'; MAX_LINE_BYTES];
        exact.push(b'\n');
        exact.resize(exact.len() + MAX_LINE_BYTES + 1, b'y');
        let mut reader = LineReader::new(exact.as_slice());
        assert_eq!(
            reader.next_line().unwrap().unwrap().bytes.len(),
            MAX_LINE_BYTES
        );
        let error = reader.next_line().unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("line 2"), "{error}");
    }

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

    #[test]
    fn terminal_controls_and_unicode_delimiters_are_safe() {
        let cleaned =
            strip_ansi("\x1b[2m2026-11-02T01:45:23Z\x1b[0m INFO x note=ok\x1b]52;c;bad\x07 tail");
        assert_eq!(cleaned, "2026-11-02T01:45:23Z INFO x note=ok tail");
        assert_eq!(
            strip_ansi("left\x1b]unterminated right"),
            "left\\u{1b}]unterminated right"
        );
        assert_eq!(
            terminal_safe("a\r\x1b[31m\u{202e}b"),
            "a\\u{d}\\u{1b}[31m\\u{202e}b"
        );
        assert_eq!(kv("x 🦀key=value")["key"], "value");
        assert_eq!(
            root_identity("root root=/srv/camera archive free_gib=10 free_pct=20"),
            Some("/srv/camera archive".into())
        );
        assert_eq!(
            root_identity("root root=/srv/\"camera\" free_gib=old free_gib=10 free_pct=20"),
            Some("/srv/\"camera\" free_gib=old".into())
        );
        assert_eq!(
            number(&BTreeMap::from([("x".into(), "NaN".into())]), "x"),
            None
        );
    }
}
