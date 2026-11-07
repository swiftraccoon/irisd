//! Just enough RTSP/1.0 (RFC 2326) to serve a TCP-interleaved PLAY session.

use std::fmt::Write;

/// Largest request body accepted; RTSP clients send small SDP or parameter bodies.
const MAX_BODY: usize = 64 * 1024;

/// A parsed request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Request {
    /// The method, upper case.
    pub method: String,
    /// The request URL as sent.
    pub url: String,
    /// The `CSeq` header, zero when absent.
    pub cseq: u32,
    /// Headers in order, names lower-cased.
    pub headers: Vec<(String, String)>,
    /// The body, if `Content-Length` announced one.
    pub body: Vec<u8>,
}

impl Request {
    /// The first header named `name` (case-insensitive).
    #[must_use]
    pub fn header(&self, name: &str) -> Option<&str> {
        let name = name.to_ascii_lowercase();
        self.headers
            .iter()
            .find(|(k, _)| *k == name)
            .map(|(_, v)| v.as_str())
    }
}

/// What the front of a receive buffer holds.
#[derive(Debug, PartialEq, Eq)]
pub enum Parsed {
    /// A complete request and the bytes it consumed.
    Request(Request, usize),
    /// An interleaved data frame (client RTCP) of this many bytes, to skip.
    Interleaved(usize),
    /// More bytes are needed.
    Incomplete,
    /// The bytes are not RTSP.
    Invalid,
}

/// Parses the front of `buf`.
#[must_use]
pub fn parse(buf: &[u8]) -> Parsed {
    if buf.is_empty() {
        return Parsed::Incomplete;
    }
    if buf[0] == b'$' {
        if buf.len() < 4 {
            return Parsed::Incomplete;
        }
        let len = usize::from(u16::from_be_bytes([buf[2], buf[3]]));
        return if buf.len() >= 4 + len {
            Parsed::Interleaved(4 + len)
        } else {
            Parsed::Incomplete
        };
    }
    let Some(head_len) = buf.windows(4).position(|w| w == b"\r\n\r\n") else {
        return if buf.len() > 64 * 1024 {
            Parsed::Invalid
        } else {
            Parsed::Incomplete
        };
    };
    let Ok(head) = std::str::from_utf8(&buf[..head_len]) else {
        return Parsed::Invalid;
    };
    let mut lines = head.split("\r\n");
    let Some(request_line) = lines.next() else {
        return Parsed::Invalid;
    };
    let mut parts = request_line.split_ascii_whitespace();
    let (Some(method), Some(url), Some(version)) = (parts.next(), parts.next(), parts.next())
    else {
        return Parsed::Invalid;
    };
    if !version.starts_with("RTSP/") {
        return Parsed::Invalid;
    }
    let mut headers = Vec::new();
    let mut cseq = 0;
    let mut content_length = 0usize;
    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        let name = name.trim().to_ascii_lowercase();
        let value = value.trim().to_owned();
        match name.as_str() {
            "cseq" => cseq = value.parse().unwrap_or(0),
            "content-length" => content_length = value.parse().unwrap_or(0),
            _ => {}
        }
        headers.push((name, value));
    }
    let body_start = head_len + 4;
    // Subtract rather than add: a declared length near `usize::MAX` must not
    // overflow, and anything beyond the largest body a client sends is invalid.
    if content_length > MAX_BODY {
        return Parsed::Invalid;
    }
    if content_length > buf.len() - body_start {
        return Parsed::Incomplete;
    }
    Parsed::Request(
        Request {
            method: method.to_ascii_uppercase(),
            url: url.to_owned(),
            cseq,
            headers,
            body: buf[body_start..body_start + content_length].to_vec(),
        },
        body_start + content_length,
    )
}

/// Serializes a response.
#[must_use]
pub fn response(
    cseq: u32,
    status: u16,
    reason: &str,
    headers: &[(&str, String)],
    body: &[u8],
) -> Vec<u8> {
    let mut out = format!("RTSP/1.0 {status} {reason}\r\nCSeq: {cseq}\r\nServer: iris-replay\r\n");
    for (name, value) in headers {
        out.push_str(name);
        out.push_str(": ");
        out.push_str(value);
        out.push_str("\r\n");
    }
    if !body.is_empty() {
        write!(out, "Content-Length: {}\r\n", body.len()).expect("writing to a String cannot fail");
    }
    out.push_str("\r\n");
    let mut bytes = out.into_bytes();
    bytes.extend_from_slice(body);
    bytes
}

/// The mount name in a request URL: the first path segment, ignoring a
/// trailing `trackID=0` control segment and any query.
#[must_use]
pub fn mount_name(url: &str) -> Option<&str> {
    let rest = url.split_once("://")?.1;
    let path = rest.split_once('/').map_or("", |(_, p)| p);
    let path = path.split('?').next().unwrap_or("");
    let first = path.split('/').next().unwrap_or("");
    (!first.is_empty() && !first.starts_with("trackID=")).then_some(first)
}

/// Returns whether `name` can be addressed as one RTSP path segment.
///
/// Mount names are used as map keys rather than filesystem paths, but accepting
/// an unreachable name would let the server report a healthy listener that can
/// never serve the configured recording.
#[must_use]
pub fn valid_mount_name(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with("trackID=")
        && !name
            .bytes()
            .any(|byte| matches!(byte, b'/' | b'?' | b'#') || byte.is_ascii_whitespace())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_request_with_headers_and_body() {
        let text = b"SET_PARAMETER rtsp://127.0.0.1/cam RTSP/1.0\r\nCSeq: 7\r\nSession: abc\r\nContent-Length: 3\r\n\r\nxyzOPTIONS";
        let Parsed::Request(request, consumed) = parse(text) else {
            panic!("expected a request");
        };
        assert_eq!(consumed, text.len() - 7);
        assert_eq!(request.method, "SET_PARAMETER");
        assert_eq!(request.cseq, 7);
        assert_eq!(request.header("session"), Some("abc"));
        assert_eq!(request.body, b"xyz");
        assert_eq!(
            parse(b"OPTIONS rtsp://x RTSP/1.0\r\nCSeq: 1\r\n"),
            Parsed::Incomplete
        );
        assert_eq!(parse(b"HTTP/1.1 200 OK\r\n\r\n"), Parsed::Invalid);
    }

    #[test]
    fn a_huge_content_length_is_invalid_not_a_panic() {
        // A fuzz finding: the completeness check added the declared length and overflowed.
        let bytes: [u8; 216] = [
            0x28, 0x52, 0x54, 0x00, 0x74, 0x00, 0x00, 0x50, 0x1a, 0x0d, 0x0d, 0x3a, 0x0a, 0x0a,
            0x52, 0x54, 0x53, 0x50, 0x2f, 0x3d, 0x3d, 0x3d, 0x3d, 0x3d, 0x1d, 0x3d, 0x04, 0x00,
            0x00, 0x00, 0x00, 0x53, 0x50, 0x2f, 0x0d, 0x0a, 0x63, 0x6f, 0x6e, 0x0d, 0x0a, 0x63,
            0x6f, 0x6e, 0x74, 0x65, 0x6e, 0x74, 0x2d, 0x6c, 0x65, 0x6e, 0x67, 0x74, 0x68, 0x3a,
            0x30, 0x30, 0x30, 0x30, 0x30, 0x30, 0x30, 0x30, 0x30, 0x30, 0x30, 0x30, 0x30, 0x30,
            0x30, 0x30, 0x30, 0x30, 0x30, 0x30, 0x30, 0x30, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x09, 0x15, 0x30, 0x30, 0x30, 0x30, 0x30, 0x30, 0x30, 0x30, 0x30, 0x30, 0x30, 0x30,
            0x31, 0x32, 0x38, 0x31, 0x33, 0x36, 0x38, 0x33, 0x32, 0x39, 0x0a, 0x0d, 0x2f, 0x0a,
            0x0d, 0x0a, 0x14, 0x0d, 0x0d, 0x0a, 0x63, 0x6f, 0x6e, 0x74, 0x65, 0x6e, 0x74, 0x2d,
            0x6c, 0x6d, 0x6e, 0x53, 0x50, 0x2f, 0x3d, 0x3d, 0x3d, 0x3d, 0x3d, 0x1d, 0x3d, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x53, 0x50, 0x2f, 0x0d, 0x0a, 0x63, 0x6f, 0x6e, 0x74, 0x65,
            0x6e, 0x74, 0x2d, 0x6c, 0x65, 0x6e, 0x67, 0x74, 0x68, 0x3a, 0x30, 0x31, 0x38, 0x34,
            0x34, 0x36, 0x37, 0x34, 0x34, 0x30, 0x37, 0x33, 0x37, 0x30, 0x39, 0x35, 0x35, 0x31,
            0x36, 0x31, 0x35, 0x0d, 0x0a, 0x0d, 0x2f, 0x0a, 0x0e, 0x33, 0x3a, 0x0e, 0x0d, 0x0f,
            0x0c, 0x0a, 0x0d, 0x0a, 0x0d, 0x0a, 0x0c, 0x0a, 0x0d, 0x0a, 0x0a, 0x0d, 0x0a, 0x0d,
            0x00, 0x0d, 0x0a, 0x00, 0x0d, 0x0a,
        ];
        assert_eq!(parse(&bytes), Parsed::Invalid);
        assert_eq!(
            parse(b"SET_PARAMETER rtsp://h/x RTSP/1.0\r\nCSeq: 1\r\nContent-Length: 18446744073709551000\r\n\r\n"),
            Parsed::Invalid
        );
    }

    #[test]
    fn interleaved_frames_are_skipped_by_length() {
        assert_eq!(parse(b"$\x01\x00\x03abcXX"), Parsed::Interleaved(7));
        assert_eq!(parse(b"$\x01\x00\x03ab"), Parsed::Incomplete);
    }

    #[test]
    fn mount_names_ignore_control_segments_and_queries() {
        assert_eq!(mount_name("rtsp://127.0.0.1:8554/main"), Some("main"));
        assert_eq!(mount_name("rtsp://h/main/trackID=0"), Some("main"));
        assert_eq!(mount_name("rtsp://h/main?x=1"), Some("main"));
        assert_eq!(mount_name("rtsp://h/"), None);
        assert_eq!(mount_name("rtsp://h/trackID=0"), None);
    }

    #[test]
    fn configured_mount_names_are_one_reachable_segment() {
        for valid in ["cam", "front-door", "camera_01", "%63am"] {
            assert!(valid_mount_name(valid), "{valid:?}");
        }
        for invalid in [
            "",
            "trackID=0",
            "cam/trackID=0",
            "cam?profile=main",
            "cam#fragment",
            "two words",
            "line\nbreak",
        ] {
            assert!(!valid_mount_name(invalid), "{invalid:?}");
        }
    }

    #[test]
    fn responses_carry_cseq_and_content_length() {
        let bytes = response(3, 200, "OK", &[("Session", "s1".into())], b"v=0\r\n");
        let text = String::from_utf8(bytes).unwrap();
        assert!(text.starts_with("RTSP/1.0 200 OK\r\nCSeq: 3\r\n"));
        assert!(text.contains("Session: s1\r\n"));
        assert!(text.ends_with("Content-Length: 5\r\n\r\nv=0\r\n"));
    }
}
