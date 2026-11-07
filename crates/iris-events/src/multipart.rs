//! Incremental reader for `multipart/x-mixed-replace` streams.
//!
//! Camera feeds are sloppy: `Content-Length:` may lack its space, the blank
//! line after the headers is sometimes missing, and line endings mix `\r\n`
//! and `\n`. The reader accepts all of that and bounds every part.

use thiserror::Error;

/// Largest part body accepted, in bytes.
pub const MAX_PART_LEN: usize = 256 * 1024;

/// Maximum syntax retained around one part body.
const MAX_SYNTAX_LEN: usize = 16 * 1024;
/// Longest line ending that separates a body from its following boundary.
const MAX_BODY_SEPARATOR_LEN: usize = 2;
/// Longest accepted MIME boundary parameter.
const MAX_BOUNDARY_LEN: usize = 256;

/// Boundary used when the response does not name one.
pub const DEFAULT_BOUNDARY: &str = "myboundary";

/// Errors that end a feed session.
#[derive(Clone, Debug, PartialEq, Eq, Error)]
pub enum MultipartError {
    /// A part grew past [`MAX_PART_LEN`] without completing.
    #[error("multipart part exceeds {MAX_PART_LEN} bytes")]
    PartTooLarge,
}

/// Extracts the boundary from a `Content-Type` header value such as
/// `multipart/x-mixed-replace; boundary=myboundary`.
#[must_use]
pub fn boundary_from_content_type(content_type: &str) -> Option<String> {
    content_type.split(';').map(str::trim).find_map(|param| {
        let (key, value) = param.split_once('=')?;
        let value = value.trim().trim_matches('"');
        (key.trim().eq_ignore_ascii_case("boundary")
            && !value.is_empty()
            && value.len() <= MAX_BOUNDARY_LEN)
            .then(|| value.to_owned())
    })
}

#[derive(Debug)]
enum ParseState {
    SeekingBoundary {
        scan: usize,
    },
    BoundaryLine {
        pos: usize,
        scan: usize,
    },
    Headers {
        pos: usize,
        scan: usize,
        content_length: Option<usize>,
    },
    Body {
        start: usize,
        scan: usize,
        content_length: Option<usize>,
    },
}

impl ParseState {
    fn shift_left(&mut self, amount: usize) {
        match self {
            Self::SeekingBoundary { scan } => *scan -= amount,
            Self::BoundaryLine { pos, scan } | Self::Headers { pos, scan, .. } => {
                *pos -= amount;
                *scan -= amount;
            }
            Self::Body { start, scan, .. } => {
                *start -= amount;
                *scan -= amount;
            }
        }
    }
}

enum ParseStep {
    Continue,
    Pending,
    Part(Vec<u8>),
}

/// Splits a byte stream into part bodies.
#[derive(Debug)]
pub struct MultipartReader {
    delimiter: Vec<u8>,
    buf: Vec<u8>,
    max_part: usize,
    consumed: usize,
    state: ParseState,
    #[cfg(any(test, feature = "fuzzing"))]
    examined: usize,
}

impl MultipartReader {
    /// Creates a reader for the given boundary (without the leading `--`).
    #[must_use]
    pub fn new(boundary: &str) -> Self {
        let mut delimiter = Vec::with_capacity(boundary.len() + 2);
        delimiter.extend_from_slice(b"--");
        delimiter.extend_from_slice(boundary.as_bytes());
        Self {
            delimiter,
            buf: Vec::new(),
            max_part: MAX_PART_LEN,
            consumed: 0,
            state: ParseState::SeekingBoundary { scan: 0 },
            #[cfg(any(test, feature = "fuzzing"))]
            examined: 0,
        }
    }

    /// Feeds bytes and returns the bodies of every part completed so far.
    pub fn push(&mut self, chunk: &[u8]) -> Result<Vec<Vec<u8>>, MultipartError> {
        let mut parts = Vec::new();
        let mut offset = 0;
        loop {
            while let Some(body) = self.next_part()? {
                parts.push(body);
            }
            self.compact();
            if offset == chunk.len() {
                return Ok(parts);
            }
            let maximum = self.maximum_buffer_len();
            let available = maximum.saturating_sub(self.buf.len());
            if available == 0 {
                return Err(MultipartError::PartTooLarge);
            }
            let take = available.min(chunk.len() - offset);
            let needed = self.buf.len() + take;
            if self.buf.capacity() < needed {
                self.buf.reserve_exact(needed - self.buf.len());
            }
            self.buf.extend_from_slice(&chunk[offset..offset + take]);
            offset += take;
        }
    }

    /// Parses one complete part from the buffer, if present.
    fn next_part(&mut self) -> Result<Option<Vec<u8>>, MultipartError> {
        loop {
            let state = std::mem::replace(&mut self.state, ParseState::SeekingBoundary { scan: 0 });
            let step = match state {
                ParseState::SeekingBoundary { scan } => self.step_seeking_boundary(scan),
                ParseState::BoundaryLine { pos, scan } => self.step_boundary_line(pos, scan),
                ParseState::Headers {
                    pos,
                    scan,
                    content_length,
                } => self.step_headers(pos, scan, content_length)?,
                ParseState::Body {
                    start,
                    scan,
                    content_length,
                } => self.step_body(start, scan, content_length)?,
            };
            match step {
                ParseStep::Continue => {}
                ParseStep::Pending => return Ok(None),
                ParseStep::Part(body) => return Ok(Some(body)),
            }
        }
    }

    fn step_seeking_boundary(&mut self, scan: usize) -> ParseStep {
        let (found, next, work) = search(&self.buf, &self.delimiter, scan);
        self.record_work(work);
        let Some(start) = found else {
            self.consumed = self.consumed.max(next);
            self.state = ParseState::SeekingBoundary { scan: next };
            return ParseStep::Pending;
        };
        let pos = start + self.delimiter.len();
        self.consumed = self.consumed.max(start);
        self.state = ParseState::BoundaryLine { pos, scan: pos };
        ParseStep::Continue
    }

    fn step_boundary_line(&mut self, pos: usize, scan: usize) -> ParseStep {
        let (found, next, work) = search(&self.buf, b"\n", scan);
        self.record_work(work);
        let Some(line_end) = found else {
            self.state = ParseState::BoundaryLine { pos, scan: next };
            return ParseStep::Pending;
        };
        let pos = line_end + 1;
        self.state = ParseState::Headers {
            pos,
            scan: pos,
            content_length: None,
        };
        ParseStep::Continue
    }

    fn step_headers(
        &mut self,
        pos: usize,
        scan: usize,
        mut content_length: Option<usize>,
    ) -> Result<ParseStep, MultipartError> {
        let (found, next, work) = search(&self.buf, b"\n", scan);
        self.record_work(work);
        let Some(line_end) = found else {
            self.state = ParseState::Headers {
                pos,
                scan: next,
                content_length,
            };
            return Ok(ParseStep::Pending);
        };
        let line = trim_cr(&self.buf[pos..line_end]);
        if line.is_empty() {
            let start = line_end + 1;
            self.state = ParseState::Body {
                start,
                scan: start,
                content_length,
            };
        } else if let Some((name, value)) = header(line) {
            if name.eq_ignore_ascii_case(b"content-length") {
                content_length = std::str::from_utf8(value)
                    .ok()
                    .and_then(|value| value.trim().parse().ok());
                if content_length.is_some_and(|length| length > self.max_part) {
                    return Err(MultipartError::PartTooLarge);
                }
            }
            let pos = line_end + 1;
            self.state = ParseState::Headers {
                pos,
                scan: pos,
                content_length,
            };
        } else {
            self.state = ParseState::Body {
                start: pos,
                scan: pos,
                content_length,
            };
        }
        Ok(ParseStep::Continue)
    }

    fn step_body(
        &mut self,
        start: usize,
        scan: usize,
        content_length: Option<usize>,
    ) -> Result<ParseStep, MultipartError> {
        let body_end = if let Some(length) = content_length {
            let Some(body_end) = start.checked_add(length) else {
                return Err(MultipartError::PartTooLarge);
            };
            if self.buf.len() < body_end {
                self.state = ParseState::Body {
                    start,
                    scan,
                    content_length,
                };
                return Ok(ParseStep::Pending);
            }
            body_end
        } else {
            let (found, next, work) = search(&self.buf, &self.delimiter, scan);
            self.record_work(work);
            let Some(raw_body_end) = found else {
                let retained = self.buf.len().saturating_sub(start);
                let incomplete_allowance = self
                    .delimiter
                    .len()
                    .saturating_sub(1)
                    .saturating_add(MAX_BODY_SEPARATOR_LEN);
                if retained > self.max_part.saturating_add(incomplete_allowance) {
                    return Err(MultipartError::PartTooLarge);
                }
                self.state = ParseState::Body {
                    start,
                    scan: next,
                    content_length,
                };
                return Ok(ParseStep::Pending);
            };
            let body_end = strip_boundary_line_ending(&self.buf, start, raw_body_end);
            if body_end.saturating_sub(start) > self.max_part {
                return Err(MultipartError::PartTooLarge);
            }
            body_end
        };
        let body = trim_newlines(&self.buf[start..body_end]);
        if body.len() > self.max_part {
            return Err(MultipartError::PartTooLarge);
        }
        let body = body.to_vec();
        self.consumed = body_end;
        self.state = ParseState::SeekingBoundary { scan: body_end };
        Ok(ParseStep::Part(body))
    }

    fn maximum_buffer_len(&self) -> usize {
        self.max_part + MAX_SYNTAX_LEN + self.delimiter.len() * 2
    }

    fn compact(&mut self) {
        if self.consumed == 0 {
            return;
        }
        self.buf.drain(..self.consumed);
        self.state.shift_left(self.consumed);
        self.consumed = 0;
    }

    fn record_work(&mut self, work: usize) {
        #[cfg(any(test, feature = "fuzzing"))]
        {
            self.examined += work;
        }
        #[cfg(not(any(test, feature = "fuzzing")))]
        let _ = (self, work);
    }

    /// Returns parser comparisons for the deterministic fuzz work oracle.
    #[cfg(feature = "fuzzing")]
    #[doc(hidden)]
    #[must_use]
    pub fn examined_for_fuzzing(&self) -> usize {
        self.examined
    }
}

fn header(line: &[u8]) -> Option<(&[u8], &[u8])> {
    let colon = line.iter().position(|&b| b == b':')?;
    let name = &line[..colon];
    let valid = !name.is_empty() && name.iter().all(|b| b.is_ascii_alphanumeric() || *b == b'-');
    valid.then(|| (name, &line[colon + 1..]))
}

/// Returns the first match, the next safe scan offset, and comparisons made.
fn search(haystack: &[u8], needle: &[u8], from: usize) -> (Option<usize>, usize, usize) {
    if needle.is_empty() {
        return (None, from, 0);
    }
    let Some(last_start) = haystack.len().checked_sub(needle.len()) else {
        return (None, from.min(haystack.len()), 0);
    };
    let mut work = 0;
    if from <= last_start {
        for at in from..=last_start {
            work += 1;
            if haystack[at..].starts_with(needle) {
                return (Some(at), at, work);
            }
        }
    }
    let overlap = needle.len().saturating_sub(1);
    (None, from.max(haystack.len().saturating_sub(overlap)), work)
}

fn trim_cr(line: &[u8]) -> &[u8] {
    line.strip_suffix(b"\r").unwrap_or(line)
}

fn trim_newlines(mut body: &[u8]) -> &[u8] {
    while let Some(rest) = body
        .strip_suffix(b"\n")
        .or_else(|| body.strip_suffix(b"\r"))
    {
        body = rest;
    }
    while let Some(rest) = body
        .strip_prefix(b"\n")
        .or_else(|| body.strip_prefix(b"\r"))
    {
        body = rest;
    }
    body
}

fn strip_boundary_line_ending(buf: &[u8], start: usize, end: usize) -> usize {
    let body = &buf[start..end];
    if body.ends_with(b"\r\n") {
        end - 2
    } else if body.ends_with(b"\r") || body.ends_with(b"\n") {
        end - 1
    } else {
        end
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FEED: &[u8] = b"\r\n--myboundary\r\nContent-Type: text/plain\r\nContent-Length:9\r\n\r\nHeartbeat\r\n--myboundary\nContent-Type: text/plain\nContent-Length: 40\n\nCode=VideoMotion;action=Start;index=0\r\n\n--myboundary\r\nContent-Type: text/plain\r\nContent-Length:9\r\nHeartbeat\r\n--myboundary\r\nContent-Type: text/plain\r\n\r\nCode=VideoLoss;action=Stop;index=0\r\n--myboundary\r\n";

    #[test]
    fn extracts_boundary_from_content_type() {
        assert_eq!(
            boundary_from_content_type("multipart/x-mixed-replace; boundary=myboundary").as_deref(),
            Some("myboundary")
        );
        assert_eq!(
            boundary_from_content_type("multipart/x-mixed-replace;boundary=\"abc\"").as_deref(),
            Some("abc")
        );
        assert_eq!(boundary_from_content_type("text/plain"), None);
    }

    #[test]
    fn parses_whole_feed_in_one_push() {
        let mut r = MultipartReader::new("myboundary");
        let parts = r.push(FEED).unwrap();
        let texts: Vec<String> = parts
            .iter()
            .map(|p| String::from_utf8_lossy(p).into_owned())
            .collect();
        assert_eq!(
            texts,
            vec![
                "Heartbeat",
                "Code=VideoMotion;action=Start;index=0",
                "Heartbeat",
                "Code=VideoLoss;action=Stop;index=0"
            ]
        );
    }

    #[test]
    fn parses_the_same_feed_byte_by_byte() {
        let mut r = MultipartReader::new("myboundary");
        let mut texts = Vec::new();
        for byte in FEED {
            for part in r.push(std::slice::from_ref(byte)).unwrap() {
                texts.push(String::from_utf8_lossy(&part).into_owned());
            }
        }
        assert_eq!(texts.len(), 4);
        assert_eq!(texts[1], "Code=VideoMotion;action=Start;index=0");
        assert_eq!(texts[3], "Code=VideoLoss;action=Stop;index=0");
    }

    #[test]
    fn bounds_runaway_parts() {
        let mut r = MultipartReader::new("myboundary");
        let mut err = None;
        let filler = vec![b'x'; 4096];
        r.push(b"--myboundary\r\nContent-Type: text/plain\r\n\r\n")
            .unwrap();
        for _ in 0..80 {
            if let Err(e) = r.push(&filler) {
                err = Some(e);
                break;
            }
        }
        assert_eq!(err, Some(MultipartError::PartTooLarge));
        let mut r = MultipartReader::new("myboundary");
        assert_eq!(
            r.push(b"--myboundary\r\nContent-Length: 999999999\r\n\r\n"),
            Err(MultipartError::PartTooLarge)
        );
    }

    fn no_length_feed(body_len: usize) -> Vec<u8> {
        let mut feed = b"--b\r\nContent-Type:text/plain\r\n\r\n".to_vec();
        feed.extend(std::iter::repeat_n(b'x', body_len));
        feed.extend_from_slice(b"--b\r\n");
        feed
    }

    #[test]
    fn no_length_body_enforces_exact_limit_for_every_boundary_split() {
        let feed = no_length_feed(MAX_PART_LEN);
        for chunk_size in [1, 17, 4096, feed.len()] {
            let mut reader = MultipartReader::new("b");
            let mut parts = Vec::new();
            for chunk in feed.chunks(chunk_size) {
                parts.extend(reader.push(chunk).unwrap());
            }
            assert_eq!(parts.len(), 1, "chunk size {chunk_size}");
            assert_eq!(parts[0].len(), MAX_PART_LEN);
            assert!(reader.buf.capacity() <= reader.maximum_buffer_len());
        }

        let oversized = no_length_feed(MAX_PART_LEN + 1);
        for chunk_size in [1, 17, 4096, oversized.len()] {
            let mut reader = MultipartReader::new("b");
            let error = oversized
                .chunks(chunk_size)
                .find_map(|chunk| reader.push(chunk).err());
            assert_eq!(error, Some(MultipartError::PartTooLarge));
            assert!(reader.buf.capacity() <= reader.maximum_buffer_len());
        }
    }

    #[test]
    fn no_length_body_checks_raw_size_before_compatibility_trimming() {
        let mut feed = b"--b\r\nContent-Type:text/plain\r\n\r\n".to_vec();
        feed.extend(std::iter::repeat_n(b'x', MAX_PART_LEN));
        feed.extend_from_slice(b"\n\n--b\r\n");
        let mut reader = MultipartReader::new("b");
        assert_eq!(reader.push(&feed), Err(MultipartError::PartTooLarge));

        let mut exact = b"--b\r\nContent-Type:text/plain\r\n\r\n".to_vec();
        exact.extend(std::iter::repeat_n(b'x', MAX_PART_LEN));
        exact.extend_from_slice(b"\r\n--b\r\n");
        let mut reader = MultipartReader::new("b");
        let parts = reader.push(&exact).unwrap();
        assert_eq!(parts.len(), 1);
        assert_eq!(parts[0].len(), MAX_PART_LEN);
    }

    #[test]
    fn repeated_compaction_never_grows_capacity_past_the_declared_allowance() {
        let body = vec![b'x'; MAX_PART_LEN];
        let mut feed = Vec::new();
        for _ in 0..3 {
            feed.extend_from_slice(b"--b\r\nContent-Type:text/plain\r\n\r\n");
            feed.extend_from_slice(&body);
            feed.extend_from_slice(b"\r\n");
        }
        feed.extend_from_slice(b"--b\r\n");

        for chunk_size in [1, 17, 4096, MAX_PART_LEN + 79] {
            let mut reader = MultipartReader::new("b");
            let mut parts = Vec::new();
            for chunk in feed.chunks(chunk_size) {
                parts.extend(reader.push(chunk).unwrap());
                assert!(
                    reader.buf.capacity() <= reader.maximum_buffer_len(),
                    "chunk size {chunk_size}: capacity {} > {}",
                    reader.buf.capacity(),
                    reader.maximum_buffer_len()
                );
            }
            assert_eq!(parts.len(), 3, "chunk size {chunk_size}");
            assert!(parts.iter().all(|part| part.len() == MAX_PART_LEN));
        }
    }

    fn incomplete_work(chunk_size: usize) -> usize {
        let mut reader = MultipartReader::new("myboundary");
        let mut input = b"--myboundary\r\nContent-Type:text/plain\r\n\r\n".to_vec();
        input.extend(std::iter::repeat_n(b'x', 64 * 1024));
        for chunk in input.chunks(chunk_size) {
            assert_eq!(reader.push(chunk).unwrap(), Vec::<Vec<u8>>::new());
        }
        assert!(
            reader.examined <= input.len() * 3,
            "{} comparisons",
            reader.examined
        );
        reader.examined
    }

    #[test]
    fn scan_work_is_linear_across_fragmentation_patterns() {
        let one = incomplete_work(1);
        let seventeen = incomplete_work(17);
        let blocks = incomplete_work(4096);
        let least = one.min(seventeen).min(blocks);
        let greatest = one.max(seventeen).max(blocks);
        assert!(
            greatest <= least + 128,
            "work varied: {one}, {seventeen}, {blocks}"
        );
    }

    fn completed_work(chunk_size: usize) -> usize {
        let mut reader = MultipartReader::new("myboundary");
        let mut input = b"--myboundary\r\nContent-Type:text/plain\r\n\r\n".to_vec();
        input.extend(std::iter::repeat_n(b'x', 64 * 1024));
        input.extend_from_slice(b"\r\n--myboundary\r\n");
        let mut parts = Vec::new();
        for chunk in input.chunks(chunk_size) {
            parts.extend(reader.push(chunk).unwrap());
        }
        assert_eq!(parts.len(), 1);
        assert_eq!(parts[0].len(), 64 * 1024);
        assert!(reader.examined <= input.len() * 3);
        reader.examined
    }

    #[test]
    fn completed_scan_work_is_linear_across_fragmentation_patterns() {
        let one = completed_work(1);
        let seventeen = completed_work(17);
        let blocks = completed_work(4096);
        let least = one.min(seventeen).min(blocks);
        let greatest = one.max(seventeen).max(blocks);
        assert!(
            greatest <= least + 128,
            "work varied: {one}, {seventeen}, {blocks}"
        );
    }

    #[test]
    fn rejects_overlong_boundary_parameters() {
        let header = format!(
            "multipart/x-mixed-replace; boundary={}",
            "x".repeat(MAX_BOUNDARY_LEN + 1)
        );
        assert_eq!(boundary_from_content_type(&header), None);
    }
}
