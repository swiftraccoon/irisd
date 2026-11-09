//! Parsing of the daemon's log lines: terminal controls removed, the
//! timestamp and level split off, `key=value` pairs read, and the periodic
//! statistics rows (`docs/research/07-architecture.md §9`) classified by series.
//!
//! The `tools_logs` fuzz target includes this file by path, so it uses nothing
//! outside the standard library.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::io::{self, BufRead, ErrorKind, Read as _};

/// Longest input line the report tools accept; the daemon's lines are far shorter.
pub const MAX_LINE_BYTES: usize = 1 << 20;

/// Longest escape string [`strip_ansi`] removes; a longer one is escaped instead.
const MAX_ESCAPE_BYTES: usize = 4_096;

/// The `key=value` fields of one line.
pub type Fields = BTreeMap<String, String>;

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
#[derive(Debug)]
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
    pub fn next_line(&mut self) -> io::Result<Option<InputLine<'_>>> {
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
            return Err(io::Error::new(
                ErrorKind::InvalidData,
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
/// to print to a terminal: every other control character and every bidirectional
/// formatting character is written as a `\u{..}` escape. An unterminated control
/// string never consumes the printable text that follows it.
pub fn strip_ansi(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let bytes = line.as_bytes();
    let mut index = 0usize;
    while let Some(c) = line.get(index..).and_then(|rest| rest.chars().next()) {
        if c == '\x1b'
            && let Some(end) = escape_end(bytes, index + 1)
        {
            index = end;
            continue;
        }
        push_safe(&mut out, c);
        index += c.len_utf8();
    }
    out
}

/// The end of the escape string whose introducer follows an ESC at
/// `next - 1`, or `None` when no complete string starts there. Every end lies
/// just past an ASCII byte, so it falls on a character boundary.
fn escape_end(bytes: &[u8], next: usize) -> Option<usize> {
    match bytes.get(next).copied()? {
        b'[' => bytes
            .get(next + 1..)?
            .iter()
            .take(MAX_ESCAPE_BYTES)
            .position(|byte| (b'@'..=b'~').contains(byte))
            .map(|offset| next + 2 + offset),
        b']' | b'P' | b'X' | b'^' | b'_' => {
            let start = next + 1;
            let end = bytes.len().min(start + MAX_ESCAPE_BYTES);
            (start..end).find_map(|cursor| match bytes[cursor] {
                b'\x07' => Some(cursor + 1),
                b'\x1b' if bytes.get(cursor + 1) == Some(&b'\\') => Some(cursor + 2),
                _ => None,
            })
        }
        byte if byte.is_ascii() => Some(next + 1),
        _ => None,
    }
}

/// Escapes untrusted text without interpreting any of it as terminal control:
/// control and bidirectional formatting characters become `\u{..}` escapes.
pub fn terminal_safe(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        push_safe(&mut out, c);
    }
    out
}

fn push_safe(out: &mut String, c: char) {
    if c.is_control() || is_bidi_control(c) {
        let _ = write!(out, "\\u{{{:x}}}", u32::from(c));
    } else {
        out.push(c);
    }
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
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Line<'a> {
    /// The RFC 3339 timestamp, as written.
    pub timestamp: &'a str,
    /// The level: `INFO`, `WARN`, `ERROR`, `DEBUG`, or `TRACE`.
    pub level: &'a str,
    /// The message and its fields.
    pub rest: &'a str,
}

const LEVELS: [&str; 5] = ["INFO", "WARN", "ERROR", "DEBUG", "TRACE"];

/// Splits `<timestamp> <LEVEL> <rest>`, where the timestamp has the shape of
/// the daemon's RFC 3339 form; `None` for any other line. The caller parses
/// the timestamp itself.
pub fn split(line: &str) -> Option<Line<'_>> {
    let mut parts = line.splitn(3, char::is_whitespace);
    let timestamp = parts.next()?;
    if timestamp.len() < 20 || timestamp.as_bytes()[4] != b'-' || !timestamp.ends_with('Z') {
        return None;
    }
    let after = line[timestamp.len()..].trim_start();
    let level = after.split_whitespace().next()?;
    if !LEVELS.contains(&level) {
        return None;
    }
    let rest = after[level.len()..].trim_start();
    Some(Line {
        timestamp,
        level,
        rest,
    })
}

/// Reads every `key=value` pair; a quoted value loses its quotes, and a later
/// pair replaces an earlier one with the same key.
pub fn kv(text: &str) -> Fields {
    let mut out = BTreeMap::new();
    let mut rest = text;
    while let Some(eq) = rest.find('=') {
        let key_start = rest[..eq]
            .char_indices()
            .rfind(|(_, c)| !(c.is_ascii_alphanumeric() || *c == '_'))
            .map_or(0, |(i, c)| i + c.len_utf8());
        let key = &rest[key_start..eq];
        let after = &rest[eq + 1..];
        let (value, consumed) = if let Some(quoted) = after.strip_prefix('"') {
            let end = quoted.find('"').unwrap_or(quoted.len());
            (&quoted[..end], end + 2)
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
/// contain whitespace, so the last ` free_gib=` field delimits the value.
pub fn root_identity(text: &str) -> Option<String> {
    let value = text.strip_prefix("root root=")?;
    value
        .rsplit_once(" free_gib=")
        .map(|(root, _)| root.to_owned())
        .filter(|root| !root.is_empty())
}

/// A numeric field, if present and finite.
pub fn number(fields: &Fields, key: &str) -> Option<f64> {
    fields
        .get(key)
        .and_then(|value| value.parse::<f64>().ok())
        .filter(|value| value.is_finite())
}

/// The daemon's periodic statistics kinds, the first word of each row.
const STATISTICS_KINDS: [&str; 6] = ["stream", "events", "writer", "catalog", "root", "process"];

/// The series a statistics row belongs to.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Series {
    /// One stream's `stream` rows, by its `camera/kind` name.
    Stream(String),
    /// One event feed's `events` rows; feeds are independent per camera and source.
    Events {
        /// The camera id.
        camera: String,
        /// The event source, as the daemon writes it.
        source: String,
    },
    /// One recording root's `root` rows.
    Root(String),
    /// The writer's rows.
    Writer,
    /// The catalog's rows.
    Catalog,
    /// The process rows.
    Process,
}

/// A line that has the shape of a statistics row.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Statistics<'a> {
    /// A row of its series, with its fields; a root row's fields carry the
    /// whole `root` path.
    Row(Series, Fields),
    /// An `INFO` line of a statistics kind, with fields, that lacks what its
    /// series needs: a stream, camera, source, or root name, the writer's
    /// `fragments`, or the catalog's `segments` and `gib`.
    Malformed(&'a str),
}

/// Classifies an `INFO` line whose first word is a statistics kind and that
/// carries `key=value` fields; `None` for every other line.
pub fn statistics<'a>(line: &Line<'a>) -> Option<Statistics<'a>> {
    let kind = line.rest.split(' ').next()?;
    if line.level != "INFO" || !STATISTICS_KINDS.contains(&kind) || !line.rest.contains('=') {
        return None;
    }
    let mut fields = kv(line.rest);
    let named =
        |fields: &Fields, key: &str| fields.get(key).filter(|value| !value.is_empty()).cloned();
    let series = match kind {
        "stream" => named(&fields, "stream").map(Series::Stream),
        "events" => named(&fields, "camera")
            .zip(named(&fields, "source"))
            .map(|(camera, source)| Series::Events { camera, source }),
        "root" => root_identity(line.rest).map(|root| {
            fields.insert("root".to_owned(), root.clone());
            Series::Root(root)
        }),
        "writer" => fields.contains_key("fragments").then_some(Series::Writer),
        "catalog" => (fields.contains_key("segments") && fields.contains_key("gib"))
            .then_some(Series::Catalog),
        // The one kind left in `STATISTICS_KINDS`.
        _ => Some(Series::Process),
    };
    Some(match series {
        Some(series) => Statistics::Row(series, fields),
        None => Statistics::Malformed(kind),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read_all(text: &str) -> Vec<(usize, String, bool)> {
        let mut reader = LineReader::new(text.as_bytes());
        let mut lines = Vec::new();
        while let Some(line) = reader.next_line().unwrap() {
            lines.push((
                line.number,
                String::from_utf8(line.bytes.to_vec()).unwrap(),
                line.terminated,
            ));
        }
        lines
    }

    #[test]
    fn line_reader_splits_like_str_lines() {
        let text = "a\nb\r\n\nc\r";
        let lines = read_all(text);
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
    }

    #[test]
    fn line_reader_refuses_a_line_over_the_limit() {
        let mut exact = vec![b'x'; MAX_LINE_BYTES];
        exact.push(b'\n');
        exact.resize(exact.len() + MAX_LINE_BYTES + 1, b'y');
        let mut reader = LineReader::new(exact.as_slice());
        assert_eq!(
            reader.next_line().unwrap().unwrap().bytes.len(),
            MAX_LINE_BYTES
        );
        let error = reader.next_line().unwrap_err();
        assert_eq!(error.kind(), ErrorKind::InvalidData);
        assert!(error.to_string().contains("line 2"), "{error}");
    }

    #[test]
    fn splits_the_timestamp_and_level_off_a_coloured_line() {
        let line = strip_ansi(
            "\x1b[2m2026-11-02T01:45:23.123456Z\x1b[0m \x1b[32m INFO\x1b[0m stream \
             \x1b[3mstream\x1b[0m\x1b[2m=\x1b[0mcam/main",
        );
        let split = split(&line).unwrap();
        assert_eq!(split.timestamp, "2026-11-02T01:45:23.123456Z");
        assert_eq!(split.level, "INFO");
        assert_eq!(split.rest, "stream stream=cam/main");
    }

    #[test]
    fn a_line_without_the_daemon_prefix_does_not_split() {
        assert!(split("irisd.service: Deactivated successfully.").is_none());
    }

    #[test]
    fn reads_plain_and_quoted_pairs() {
        let fields = kv("stream stream=cam/main connected=true fps=29.97 name=\"a b\"");
        assert_eq!(fields["stream"], "cam/main");
        assert_eq!(fields["connected"], "true");
        assert_eq!(number(&fields, "fps"), Some(29.97));
        assert_eq!(fields["name"], "a b");
    }

    #[test]
    fn a_key_ends_at_a_non_ascii_character() {
        assert_eq!(kv("x 🦀key=value")["key"], "value");
    }

    #[test]
    fn complete_escape_strings_are_removed() {
        assert_eq!(
            strip_ansi("\x1b[2m2026-11-02T01:45:23Z\x1b[0m INFO x note=ok\x1b]52;c;bad\x07 tail"),
            "2026-11-02T01:45:23Z INFO x note=ok tail"
        );
    }

    #[test]
    fn an_unterminated_control_string_keeps_the_text_after_it() {
        assert_eq!(
            strip_ansi("left\x1b]unterminated right"),
            "left\\u{1b}]unterminated right"
        );
    }

    #[test]
    fn an_escape_before_a_multibyte_character_keeps_the_character() {
        assert_eq!(strip_ansi("\x1bé"), "\\u{1b}é");
        assert_eq!(strip_ansi("a\x1b\u{202e}b"), "a\\u{1b}\\u{202e}b");
    }

    #[test]
    fn terminal_safe_escapes_controls_and_bidi_formatting() {
        assert_eq!(
            terminal_safe("a\r\x1b[31m\u{202e}b"),
            "a\\u{d}\\u{1b}[31m\\u{202e}b"
        );
    }

    #[test]
    fn root_identity_ends_at_the_last_free_gib_field() {
        assert_eq!(
            root_identity("root root=/srv/camera archive free_gib=10 free_pct=20"),
            Some("/srv/camera archive".into())
        );
        assert_eq!(
            root_identity("root root=/srv/\"camera\" free_gib=old free_gib=10 free_pct=20"),
            Some("/srv/\"camera\" free_gib=old".into())
        );
    }

    #[test]
    fn number_rejects_non_finite_values() {
        assert_eq!(
            number(&BTreeMap::from([("x".into(), "NaN".into())]), "x"),
            None
        );
    }

    fn classify(line: &str) -> Option<Statistics<'_>> {
        statistics(&split(line).unwrap())
    }

    #[test]
    fn statistics_rows_are_classified_by_series() {
        let Some(Statistics::Row(series, fields)) = classify(
            "2026-11-07T20:06:14.396879Z  INFO events camera=cam source=DahuaCgi connected=true \
             events=2",
        ) else {
            panic!("an events row")
        };
        assert_eq!(
            series,
            Series::Events {
                camera: "cam".into(),
                source: "DahuaCgi".into()
            }
        );
        assert_eq!(fields["events"], "2");
    }

    #[test]
    fn a_root_row_carries_its_whole_path() {
        let Some(Statistics::Row(series, fields)) = classify(
            "2026-11-07T20:05:14.397731Z  INFO root root=/srv/camera archive free_gib=10.0 \
             free_pct=69.6",
        ) else {
            panic!("a root row")
        };
        assert_eq!(series, Series::Root("/srv/camera archive".into()));
        assert_eq!(fields["root"], "/srv/camera archive");
    }

    #[test]
    fn a_statistics_kind_without_its_identity_is_malformed() {
        assert_eq!(
            classify("2026-11-01T00:00:00Z INFO stream connected=true"),
            Some(Statistics::Malformed("stream"))
        );
        assert_eq!(
            classify("2026-11-01T00:00:00Z INFO catalog open path=/srv/state/irisd.db"),
            Some(Statistics::Malformed("catalog"))
        );
    }

    #[test]
    fn other_lines_are_not_statistics() {
        for line in [
            "2026-11-01T00:00:00Z WARN stream stream=cam/main frames=1",
            "2026-11-01T00:00:00Z INFO catalog thread exiting",
            "2026-11-01T00:00:00Z INFO gap stream=cam/main secs=1.0",
        ] {
            assert_eq!(classify(line), None, "{line}");
        }
    }
}
