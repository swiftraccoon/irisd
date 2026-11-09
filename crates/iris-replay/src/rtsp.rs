//! Just enough RTSP/1.0 (RFC 2326) to serve a TCP-interleaved PLAY session.

use std::borrow::Borrow;
use std::fmt;
use std::fmt::Write as _;
use std::str::FromStr;

/// Largest request head accepted, its terminating blank line excluded.
const MAX_HEAD: usize = 64 * 1024;
/// Largest request body accepted; RTSP clients send small SDP or parameter bodies.
const MAX_BODY: usize = 64 * 1024;

/// A parsed request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Request {
    /// The method, upper case.
    pub method: String,
    /// The request URL as sent.
    pub url: String,
    /// The `CSeq` header, zero when absent or not a decimal number.
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
    /// The bytes are not RTSP: no RTSP request line, a head longer than 64 KiB,
    /// a `Content-Length` that is not a decimal count, repeats with another
    /// value, or exceeds 64 KiB.
    Invalid,
}

/// Parses the front of `buf`: an interleaved frame, or a request whose head
/// ends within 64 KiB and whose body is at most 64 KiB. The scan never reads
/// past the head limit, so the work per call is bounded.
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
    let window = &buf[..buf.len().min(MAX_HEAD + 4)];
    let Some(head_len) = window.windows(4).position(|w| w == b"\r\n\r\n") else {
        return if buf.len() >= MAX_HEAD + 4 {
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
    let mut content_length: Option<usize> = None;
    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        let name = name.trim().to_ascii_lowercase();
        let value = value.trim().to_owned();
        match name.as_str() {
            "cseq" => cseq = value.parse().unwrap_or(0),
            "content-length" => {
                // Framing depends on this value, so an unreadable or
                // contradictory one refuses the request instead of
                // misplacing the next one.
                let Ok(length) = value.parse::<usize>() else {
                    return Parsed::Invalid;
                };
                if content_length.is_some_and(|previous| previous != length) {
                    return Parsed::Invalid;
                }
                content_length = Some(length);
            }
            _ => {}
        }
        headers.push((name, value));
    }
    let content_length = content_length.unwrap_or(0);
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
    headers: &[(&str, &str)],
    body: &[u8],
) -> Vec<u8> {
    let mut out = format!("RTSP/1.0 {status} {reason}\r\nCSeq: {cseq}\r\nServer: iris-replay\r\n");
    for (name, value) in headers {
        write!(out, "{name}: {value}\r\n").expect("writing to a String cannot fail");
    }
    if !body.is_empty() {
        write!(out, "Content-Length: {}\r\n", body.len()).expect("writing to a String cannot fail");
    }
    out.push_str("\r\n");
    let mut bytes = out.into_bytes();
    bytes.extend_from_slice(body);
    bytes
}

/// The mount name in a request URL: the first path segment, unless it is a
/// `trackID=` control segment; later segments and any query are ignored.
#[must_use]
pub fn mount_name(url: &str) -> Option<&str> {
    let rest = url.split_once("://")?.1;
    let path = rest.split_once('/').map_or("", |(_, p)| p);
    let path = path.split('?').next().unwrap_or("");
    let first = path.split('/').next().unwrap_or("");
    (!first.is_empty() && !first.starts_with("trackID=")).then_some(first)
}

/// The interleaved channel pair a `SETUP` negotiates.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InterleavedChannels {
    /// The channel carrying RTP.
    pub rtp: u8,
    /// The channel carrying RTCP.
    pub rtcp: u8,
}

/// The channels a `Transport` header asks for: `RTP/AVP/TCP` with at most one
/// `interleaved=N-M` pair of distinct channels, `0-1` when it names none, or
/// `None` for any other transport.
#[must_use]
pub fn interleaved_transport(transport: &str) -> Option<InterleavedChannels> {
    let mut parts = transport.split(';').map(str::trim);
    if !parts.next()?.eq_ignore_ascii_case("RTP/AVP/TCP") {
        return None;
    }
    let mut selected = None;
    for part in parts {
        let Some((name, value)) = part.split_once('=') else {
            continue;
        };
        if !name.trim().eq_ignore_ascii_case("interleaved") {
            continue;
        }
        if selected.is_some() {
            return None;
        }
        let (media_text, control_text) = value.trim().split_once('-')?;
        let channels = InterleavedChannels {
            rtp: media_text.parse().ok()?,
            rtcp: control_text.parse().ok()?,
        };
        if channels.rtp == channels.rtcp {
            return None;
        }
        selected = Some(channels);
    }
    Some(selected.unwrap_or(InterleavedChannels { rtp: 0, rtcp: 1 }))
}

/// The session id of a `Session` header value: the text before any
/// `;timeout=` parameter, trimmed.
#[must_use]
pub fn session_id(header: &str) -> &str {
    header.split(';').next().unwrap_or("").trim()
}

/// A mount's name: one RTSP path segment that [`mount_name`] can return, so a
/// client can address it. It is nonempty, holds no `/`, `?`, `#`, or
/// whitespace, and does not start with `trackID=`.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct MountName(String);

/// A string that cannot address a mount.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error(
    "invalid mount name {0:?}: a mount is one path segment without '/', '?', '#', or \
     whitespace, not starting with trackID="
)]
pub struct InvalidMountName(String);

impl MountName {
    /// Validates and wraps a name.
    ///
    /// # Errors
    ///
    /// [`InvalidMountName`] when `name` is empty, holds `/`, `?`, `#`, or
    /// whitespace, or starts with `trackID=`.
    pub fn try_new(name: &str) -> Result<Self, InvalidMountName> {
        let reachable = !name.is_empty()
            && !name.starts_with("trackID=")
            && !name
                .bytes()
                .any(|byte| matches!(byte, b'/' | b'?' | b'#') || byte.is_ascii_whitespace());
        if reachable {
            Ok(Self(name.to_owned()))
        } else {
            Err(InvalidMountName(name.to_owned()))
        }
    }

    /// The name.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl FromStr for MountName {
    type Err = InvalidMountName;

    fn from_str(name: &str) -> Result<Self, Self::Err> {
        Self::try_new(name)
    }
}

impl fmt::Display for MountName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl Borrow<str> for MountName {
    fn borrow(&self) -> &str {
        &self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_request_parses_with_its_headers_and_body() {
        let text = concat!(
            "SET_PARAMETER rtsp://127.0.0.1/cam RTSP/1.0\r\nCSeq: 7\r\nSession: abc\r\n",
            "Content-Length: 3\r\n\r\nxyzOPTIONS"
        )
        .as_bytes();
        let Parsed::Request(request, consumed) = parse(text) else {
            panic!("expected a request");
        };
        assert_eq!(consumed, text.len() - 7);
        assert_eq!(request.method, "SET_PARAMETER");
        assert_eq!(request.cseq, 7);
        assert_eq!(request.header("session"), Some("abc"));
        assert_eq!(request.body, b"xyz");
    }

    #[test]
    fn an_unterminated_head_is_incomplete() {
        assert_eq!(
            parse(b"OPTIONS rtsp://x RTSP/1.0\r\nCSeq: 1\r\n"),
            Parsed::Incomplete
        );
    }

    #[test]
    fn an_http_status_line_is_invalid() {
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
            parse(
                concat!(
                    "SET_PARAMETER rtsp://h/x RTSP/1.0\r\nCSeq: 1\r\n",
                    "Content-Length: 18446744073709551000\r\n\r\n"
                )
                .as_bytes()
            ),
            Parsed::Invalid
        );
    }

    #[test]
    fn an_unreadable_content_length_is_invalid() {
        for length in ["18446744073709551616", "-1", "abc", ""] {
            let request = format!(
                "SET_PARAMETER rtsp://h/x RTSP/1.0\r\nCSeq: 1\r\nContent-Length: {length}\r\n\r\n"
            );
            assert_eq!(parse(request.as_bytes()), Parsed::Invalid, "{length:?}");
        }
    }

    #[test]
    fn contradictory_content_lengths_are_invalid() {
        let request = concat!(
            "SET_PARAMETER rtsp://h/x RTSP/1.0\r\nCSeq: 1\r\n",
            "Content-Length: 1\r\nContent-Length: 2\r\n\r\nxy"
        );
        assert_eq!(parse(request.as_bytes()), Parsed::Invalid);
    }

    #[test]
    fn a_head_beyond_the_limit_is_invalid() {
        let mut head = b"OPTIONS rtsp://h/x RTSP/1.0\r\nX-Pad: ".to_vec();
        head.resize(MAX_HEAD + 8, b'a');
        assert_eq!(parse(&head), Parsed::Invalid);
        head.extend_from_slice(b"\r\n\r\n");
        assert_eq!(parse(&head), Parsed::Invalid, "a terminator past the limit");
    }

    #[test]
    fn a_head_at_the_limit_is_accepted() {
        let mut head = b"OPTIONS rtsp://h/x RTSP/1.0\r\nX-Pad: ".to_vec();
        head.resize(MAX_HEAD, b'a');
        head.extend_from_slice(b"\r\n\r\n");
        assert!(matches!(parse(&head), Parsed::Request(_, _)));
    }

    #[test]
    fn interleaved_frames_are_skipped_by_length() {
        assert_eq!(parse(b"$\x01\x00\x03abcXX"), Parsed::Interleaved(7));
        assert_eq!(parse(b"$\x01\x00\x03ab"), Parsed::Incomplete);
    }

    #[test]
    fn mount_names_are_the_first_path_segment() {
        assert_eq!(mount_name("rtsp://127.0.0.1:8554/main"), Some("main"));
        assert_eq!(mount_name("rtsp://h/main/trackID=0"), Some("main"));
        assert_eq!(mount_name("rtsp://h/main/x/y"), Some("main"));
        assert_eq!(mount_name("rtsp://h/main?x=1"), Some("main"));
        assert_eq!(mount_name("rtsp://h/"), None);
        assert_eq!(mount_name("rtsp://h/trackID=0"), None);
    }

    #[test]
    fn configured_mount_names_are_one_reachable_segment() {
        for valid in ["cam", "front-door", "camera_01", "%63am"] {
            assert!(MountName::try_new(valid).is_ok(), "{valid:?}");
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
            assert!(MountName::try_new(invalid).is_err(), "{invalid:?}");
        }
    }

    #[test]
    fn transport_channels_are_negotiated_or_refused() {
        assert_eq!(
            interleaved_transport("RTP/AVP/TCP;unicast;interleaved=4-5"),
            Some(InterleavedChannels { rtp: 4, rtcp: 5 })
        );
        assert_eq!(
            interleaved_transport("rtp/avp/tcp;unicast"),
            Some(InterleavedChannels { rtp: 0, rtcp: 1 })
        );
        for invalid in [
            "RTP/AVP;unicast;interleaved=0-1",
            "RTP/AVP/TCP;interleaved=4",
            "RTP/AVP/TCP;interleaved=4-4",
            "RTP/AVP/TCP;interleaved=x-5",
            "RTP/AVP/TCP;interleaved=256-1",
            "RTP/AVP/TCP;interleaved=0-1;interleaved=2-3",
        ] {
            assert_eq!(interleaved_transport(invalid), None, "{invalid}");
        }
    }

    #[test]
    fn session_ids_drop_their_parameters() {
        assert_eq!(session_id(" abc ;timeout=60"), "abc");
        assert_eq!(session_id("abc"), "abc");
        assert_eq!(session_id(""), "");
    }

    #[test]
    fn responses_carry_cseq_and_content_length() {
        let bytes = response(3, 200, "OK", &[("Session", "s1")], b"v=0\r\n");
        let text = String::from_utf8(bytes).unwrap();
        assert!(text.starts_with("RTSP/1.0 200 OK\r\nCSeq: 3\r\n"));
        assert!(text.contains("Session: s1\r\n"));
        assert!(text.ends_with("Content-Length: 5\r\n\r\nv=0\r\n"));
    }
}
