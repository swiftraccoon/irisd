//! Incremental reader for `multipart/x-mixed-replace` streams.
//!
//! Camera feeds are sloppy: `Content-Length:` may lack its space, the blank
//! line after the headers is sometimes missing, and line endings mix `\r\n`
//! and `\n`. The reader accepts all of that and bounds every part.

use thiserror::Error;

/// Largest part body accepted, in bytes.
pub const MAX_PART_LEN: usize = 256 * 1024;

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
        key.trim()
            .eq_ignore_ascii_case("boundary")
            .then(|| value.trim().trim_matches('"').to_owned())
    })
}

/// Splits a byte stream into part bodies.
#[derive(Debug)]
pub struct MultipartReader {
    delimiter: Vec<u8>,
    buf: Vec<u8>,
    max_part: usize,
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
        }
    }

    /// Feeds bytes and returns the bodies of every part completed so far.
    pub fn push(&mut self, chunk: &[u8]) -> Result<Vec<Vec<u8>>, MultipartError> {
        self.buf.extend_from_slice(chunk);
        let mut parts = Vec::new();
        while let Some(body) = self.next_part()? {
            parts.push(body);
        }
        if self.buf.len() > self.max_part + self.delimiter.len() + 1024 {
            return Err(MultipartError::PartTooLarge);
        }
        Ok(parts)
    }

    /// Parses one complete part from the buffer, if present.
    fn next_part(&mut self) -> Result<Option<Vec<u8>>, MultipartError> {
        let Some(start) = find(&self.buf, &self.delimiter) else {
            return Ok(None);
        };
        // Everything before the delimiter is preamble or the tail of the previous part.
        let mut pos = start + self.delimiter.len();
        // Closing delimiter "--" or the rest of the boundary line.
        let Some(line_end) = find(&self.buf[pos..], b"\n") else {
            return Ok(None);
        };
        pos += line_end + 1;

        // Headers: lines of the form `Name: value` (or `Name:value`); an empty
        // line ends them; a non-header line starts the body.
        let mut content_length: Option<usize> = None;
        loop {
            let Some(nl) = find(&self.buf[pos..], b"\n") else {
                return Ok(None);
            };
            let line = trim_cr(&self.buf[pos..pos + nl]);
            if line.is_empty() {
                pos += nl + 1;
                break;
            }
            match header(line) {
                Some((name, value)) => {
                    if name.eq_ignore_ascii_case(b"content-length") {
                        content_length = std::str::from_utf8(value)
                            .ok()
                            .and_then(|v| v.trim().parse().ok());
                    }
                    pos += nl + 1;
                }
                None => break,
            }
        }

        let body_end = if let Some(len) = content_length {
            if len > self.max_part {
                return Err(MultipartError::PartTooLarge);
            }
            if self.buf.len() < pos + len {
                return Ok(None);
            }
            pos + len
        } else {
            let Some(next) = find(&self.buf[pos..], &self.delimiter) else {
                return Ok(None);
            };
            pos + next
        };
        let body = trim_newlines(&self.buf[pos..body_end]).to_vec();
        self.buf.drain(..body_end);
        Ok(Some(body))
    }
}

fn header(line: &[u8]) -> Option<(&[u8], &[u8])> {
    let colon = line.iter().position(|&b| b == b':')?;
    let name = &line[..colon];
    let valid = !name.is_empty() && name.iter().all(|b| b.is_ascii_alphanumeric() || *b == b'-');
    valid.then(|| (name, &line[colon + 1..]))
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack.windows(needle.len()).position(|w| w == needle)
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
}
