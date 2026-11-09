//! Bounded, non-panicking parsers for the camera-controlled bytes the probes
//! read: RTSP responses and the interleaved stream around them, digest
//! challenges, transport and SDP fields, RTP and RTCP packets, H.265 payload
//! units, RTP timestamps, and the ONVIF clock.
//!
//! The `tools_protocol` fuzz target includes this file by path beside
//! `time.rs`, so it uses nothing else from the crate.

use std::ops::Range;

use anyhow::Context;

use crate::time;

/// Longest RTSP response head the probe reads.
pub const MAX_RTSP_HEADER: usize = 64 * 1024;
/// Longest RTSP response body the probe reads.
pub const MAX_RTSP_BODY: usize = 4 * 1024 * 1024;

/// Seconds from the NTP epoch (1900) to the Unix epoch (1970).
const NTP_UNIX_OFFSET: f64 = 2_208_988_800.0;

/// Longest namespace prefix [`camera_utc`] accepts on an element name.
const MAX_XML_PREFIX: usize = 32;

/// The index just past the first opening tag named `local`, with or without
/// a namespace prefix (`<tt:Hour>` or `<Hour>`).
fn element_start(xml: &str, local: &str) -> Option<usize> {
    let close = format!("{local}>");
    let mut from = 0;
    while let Some(found) = xml.get(from..)?.find(&close) {
        let name_start = from + found;
        from = name_start + close.len();
        if opens_element(&xml.as_bytes()[..name_start]) {
            return Some(from);
        }
    }
    None
}

/// Whether `before` ends with `<` or with `<prefix:` for a short prefix.
fn opens_element(before: &[u8]) -> bool {
    match before.split_last() {
        Some((b'<', _)) => true,
        Some((b':', head)) => {
            let prefix = head
                .iter()
                .rev()
                .take(MAX_XML_PREFIX + 1)
                .take_while(|byte| {
                    byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.')
                })
                .count();
            (1..=MAX_XML_PREFIX).contains(&prefix)
                && head.len() > prefix
                && head[head.len() - prefix - 1] == b'<'
        }
        _ => false,
    }
}

/// The integer text of the first element named `local` in `xml`.
fn element_number(xml: &str, local: &str) -> Option<i64> {
    let text = xml.get(element_start(xml, local)?..)?;
    text.get(..text.find('<')?)?.trim().parse().ok()
}

/// The camera's UTC clock, in seconds since the Unix epoch, from an ONVIF
/// `GetSystemDateAndTime` response: the `Hour`, `Minute`, `Second`, `Year`,
/// `Month`, and `Day` elements after the first `UTCDateTime` element, whatever
/// their namespace prefix. `None` when one is missing or out of range.
pub fn camera_utc(response: &str) -> Option<f64> {
    let utc = response.get(element_start(response, "UTCDateTime")?..)?;
    let hour = element_number(utc, "Hour")?;
    let minute = element_number(utc, "Minute")?;
    let second = element_number(utc, "Second")?;
    let year = element_number(utc, "Year")?;
    let month = element_number(utc, "Month")?;
    let day = element_number(utc, "Day")?;
    if !(1..=9999).contains(&year)
        || !(0..=23).contains(&hour)
        || !(0..=59).contains(&minute)
        || !(0..=59).contains(&second)
    {
        return None;
    }
    let whole = time::days_from_civil(year, month, day)?
        .checked_mul(86_400)?
        .checked_add(hour.checked_mul(3_600)?)?
        .checked_add(minute.checked_mul(60)?)?
        .checked_add(second)?;
    Some(whole as f64)
}

/// The fields of one RTP packet (RFC 3550 §5.1) the probe reads.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RtpPacket {
    /// The marker bit.
    pub marker: bool,
    /// The sequence number.
    pub sequence: u16,
    /// The media timestamp.
    pub timestamp: u32,
    /// Whether a header extension follows the CSRC list.
    pub extension: bool,
    /// Whether padding ends the packet.
    pub padded: bool,
    /// The payload's byte offsets within the packet: after the header, the
    /// CSRC list, and any extension, before any padding.
    pub payload: Range<usize>,
}

/// Reads one complete RTP packet; a short header, another version, a
/// truncated CSRC list or extension, and invalid padding are errors.
pub fn rtp_packet(packet: &[u8]) -> anyhow::Result<RtpPacket> {
    anyhow::ensure!(
        packet.len() >= 12,
        "RTP packet is shorter than its fixed header"
    );
    anyhow::ensure!(packet[0] >> 6 == 2, "unsupported RTP version");
    // At most 15 CSRCs and a 65,535-word extension: no sum below can overflow.
    let mut start = 12 + 4 * usize::from(packet[0] & 0x0f);
    anyhow::ensure!(start <= packet.len(), "truncated RTP CSRC list");
    let extension = packet[0] & 0x10 != 0;
    if extension {
        anyhow::ensure!(start + 4 <= packet.len(), "truncated RTP extension header");
        let words = usize::from(u16::from_be_bytes([packet[start + 2], packet[start + 3]]));
        start += 4 + 4 * words;
        anyhow::ensure!(start <= packet.len(), "truncated RTP extension data");
    }
    let padded = packet[0] & 0x20 != 0;
    let end = if padded {
        let padding = usize::from(packet[packet.len() - 1]);
        anyhow::ensure!(
            padding > 0 && padding <= packet.len(),
            "invalid RTP padding length"
        );
        packet.len() - padding
    } else {
        packet.len()
    };
    anyhow::ensure!(start <= end, "RTP padding overlaps the header");
    Ok(RtpPacket {
        marker: packet[1] & 0x80 != 0,
        sequence: u16::from_be_bytes([packet[2], packet[3]]),
        timestamp: u32::from_be_bytes([packet[4], packet[5], packet[6], packet[7]]),
        extension,
        padded,
        payload: start..end,
    })
}

/// The framing of one RTSP response at the start of a buffer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResponseFrame {
    /// The head's length: where the blank line that ends it starts.
    pub head_end: usize,
    /// The body's byte offsets; empty without a `Content-Length`.
    pub body: Range<usize>,
}

/// Frames the RTSP response at the start of `input` without allocating from
/// a peer-controlled length; `None` until the whole head and body are there.
/// A head over [`MAX_RTSP_HEADER`] bytes or not UTF-8, a header line without a
/// colon, a duplicate or invalid `Content-Length`, and a body over
/// [`MAX_RTSP_BODY`] bytes are errors.
pub fn rtsp_response(input: &[u8]) -> anyhow::Result<Option<ResponseFrame>> {
    let scan = &input[..input.len().min(MAX_RTSP_HEADER + 4)];
    let Some(head_end) = scan.windows(4).position(|window| window == b"\r\n\r\n") else {
        anyhow::ensure!(
            input.len() < MAX_RTSP_HEADER + 4,
            "RTSP header exceeds {} KiB",
            MAX_RTSP_HEADER / 1024
        );
        return Ok(None);
    };
    let head = std::str::from_utf8(&input[..head_end]).context("RTSP header is not UTF-8")?;
    let mut length = None;
    for line in head.lines().skip(1) {
        let (name, value) = line.split_once(':').context("malformed RTSP header line")?;
        if name.trim().eq_ignore_ascii_case("Content-Length") {
            anyhow::ensure!(length.is_none(), "duplicate RTSP Content-Length");
            length = Some(
                value
                    .trim()
                    .parse::<usize>()
                    .context("invalid RTSP Content-Length")?,
            );
        }
    }
    let length = length.unwrap_or(0);
    anyhow::ensure!(
        length <= MAX_RTSP_BODY,
        "RTSP body exceeds {} MiB",
        MAX_RTSP_BODY / (1024 * 1024)
    );
    let body = head_end + 4..head_end + 4 + length;
    Ok((body.end <= input.len()).then_some(ResponseFrame { head_end, body }))
}

/// What starts a buffer of RTSP traffic with interleaved data (RFC 2326 §10.12).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Interleaved {
    /// More bytes are needed.
    Incomplete,
    /// A `$` frame on `channel`; the frame ends where its payload does.
    Data {
        /// The channel byte.
        channel: u8,
        /// The payload's byte offsets.
        payload: Range<usize>,
    },
    /// An RTSP response; it ends where its body does.
    Response(ResponseFrame),
    /// Bytes that start neither a frame nor a response: those before the next
    /// `$` or `RTSP/`, or all but the last four when neither follows.
    Skip(usize),
}

impl Interleaved {
    /// How many bytes this item takes from the front of the buffer.
    pub fn consumed(&self) -> usize {
        match self {
            Self::Incomplete => 0,
            Self::Data { payload, .. } => payload.end,
            Self::Response(frame) => frame.body.end,
            Self::Skip(count) => *count,
        }
    }
}

/// Reads the item at the front of `buffer`. Only a response's framing can
/// fail, as [`rtsp_response`] describes.
pub fn demultiplex(buffer: &[u8]) -> anyhow::Result<Interleaved> {
    if buffer.len() < 4 {
        return Ok(Interleaved::Incomplete);
    }
    if buffer[0] == b'$' {
        let length = usize::from(u16::from_be_bytes([buffer[2], buffer[3]]));
        return Ok(if buffer.len() < 4 + length {
            Interleaved::Incomplete
        } else {
            Interleaved::Data {
                channel: buffer[1],
                payload: 4..4 + length,
            }
        });
    }
    if buffer.starts_with(b"RTSP/") {
        return Ok(rtsp_response(buffer)?.map_or(Interleaved::Incomplete, Interleaved::Response));
    }
    let next_data = buffer[1..].iter().position(|byte| *byte == b'$');
    let next_response = buffer[1..].windows(5).position(|window| window == b"RTSP/");
    Ok(match next_data.into_iter().chain(next_response).min() {
        Some(offset) => Interleaved::Skip(offset + 1),
        None if buffer.len() > 4 => Interleaved::Skip(buffer.len() - 4),
        None => Interleaved::Incomplete,
    })
}

/// The header lines after a response's status line, as name and trimmed value.
fn headers(head: &str) -> impl Iterator<Item = (&str, &str)> {
    head.lines().skip(1).filter_map(|line| {
        let (name, value) = line.split_once(':')?;
        Some((name.trim(), value.trim()))
    })
}

/// The value of the first header named `name`, in any case.
pub fn header_value<'a>(head: &'a str, name: &str) -> Option<&'a str> {
    headers(head).find_map(|(key, value)| key.eq_ignore_ascii_case(name).then_some(value))
}

/// The three-digit status code of an `RTSP/1.0` status line.
pub fn response_status(head: &str) -> anyhow::Result<u16> {
    let mut fields = head.lines().next().unwrap_or_default().split_whitespace();
    anyhow::ensure!(
        fields.next() == Some("RTSP/1.0"),
        "invalid RTSP status line"
    );
    let code = fields.next().context("missing RTSP status code")?;
    anyhow::ensure!(
        code.len() == 3 && code.bytes().all(|byte| byte.is_ascii_digit()),
        "invalid RTSP status code"
    );
    code.parse().context("invalid RTSP status code")
}

/// The response's `CSeq`; a response without exactly one is an error.
pub fn response_cseq(head: &str) -> anyhow::Result<u32> {
    let mut values = headers(head)
        .filter_map(|(name, value)| name.eq_ignore_ascii_case("CSeq").then_some(value));
    let (Some(value), None) = (values.next(), values.next()) else {
        anyhow::bail!("RTSP response must contain one CSeq");
    };
    value.parse().context("RTSP response has an invalid CSeq")
}

/// The identifier of a `Session` header value, without its parameters.
pub fn session_id(value: &str) -> &str {
    value.split(';').next().unwrap_or_default().trim()
}

/// The `interleaved=` channel pair of the first transport in a `Transport`
/// header value; a single channel `n` pairs RTCP with `n + 1`.
pub fn interleaved_channels(transport: &str) -> Option<(u8, u8)> {
    let first = transport.split(',').next()?;
    let value = first
        .split(';')
        .find_map(|parameter| parameter.trim().strip_prefix("interleaved="))?;
    if let Some((first, second)) = value.split_once('-') {
        return Some((first.trim().parse().ok()?, second.trim().parse().ok()?));
    }
    let channel: u8 = value.trim().parse().ok()?;
    Some((channel, channel.checked_add(1)?))
}

/// The parameters of a `Digest` challenge the probe answers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DigestChallenge {
    /// The protection space.
    pub realm: String,
    /// The server's nonce.
    pub nonce: String,
}

/// The realm and nonce of the first `Digest` challenge among the head's
/// `WWW-Authenticate` headers (RFC 2617 §3.2.1); `None` without one.
pub fn digest_challenge(head: &str) -> Option<DigestChallenge> {
    headers(head)
        .filter(|(name, _)| name.eq_ignore_ascii_case("WWW-Authenticate"))
        .find_map(|(_, value)| {
            let (scheme, parameters) = value.split_once(char::is_whitespace)?;
            if !scheme.eq_ignore_ascii_case("Digest") {
                return None;
            }
            let (mut realm, mut nonce) = (None, None);
            for (key, value) in auth_parameters(parameters) {
                if key.eq_ignore_ascii_case("realm") {
                    realm.get_or_insert(value);
                } else if key.eq_ignore_ascii_case("nonce") {
                    nonce.get_or_insert(value);
                }
            }
            Some(DigestChallenge {
                realm: realm?.to_owned(),
                nonce: nonce?.to_owned(),
            })
        })
}

/// The `key=value` and `key="value"` pairs of a comma-separated parameter
/// list. A quoted value ends at the next quote; an unterminated one, or text
/// without `=`, ends the list.
fn auth_parameters(text: &str) -> impl Iterator<Item = (&str, &str)> {
    let mut rest = text;
    std::iter::from_fn(move || {
        rest = rest.trim_start_matches([' ', '\t', ',']);
        let (key, after) = rest.split_once('=')?;
        let key = key.rsplit(',').next().unwrap_or_default().trim();
        let after = after.trim_start();
        let (value, remaining) = if let Some(quoted) = after.strip_prefix('"') {
            let end = quoted.find('"')?;
            (&quoted[..end], &quoted[end + 1..])
        } else {
            let end = after.find(',').unwrap_or(after.len());
            (after[..end].trim_end(), &after[end..])
        };
        rest = remaining;
        Some((key, value))
    })
}

/// Each SDP media description's kind (`video`, `audio`, `application`) and
/// its first `a=control:` attribute, in order.
pub fn sdp_controls(sdp: &str) -> Vec<(String, Option<String>)> {
    let mut tracks: Vec<(String, Option<String>)> = Vec::new();
    for line in sdp.lines().map(str::trim) {
        if let Some(media) = line.strip_prefix("m=") {
            let kind = media.split_whitespace().next().unwrap_or_default();
            tracks.push((kind.to_owned(), None));
        } else if let Some(control) = line.strip_prefix("a=control:")
            && let Some((_, track_control)) = tracks.last_mut()
            && track_control.is_none()
        {
            *track_control = Some(control.to_owned());
        }
    }
    tracks
}

/// How an H.265 RTP payload carries its NAL units (RFC 7798 §4.4).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PayloadKind {
    /// One whole NAL unit.
    Single,
    /// An aggregation packet: several whole NAL units.
    Aggregation,
    /// A fragmentation unit: part of one NAL unit.
    Fragment,
}

/// The NAL units an H.265 RTP payload starts.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct H265Payload {
    /// How the payload carries its units.
    pub kind: PayloadKind,
    /// The type of each NAL unit that starts in the payload: the one unit of a
    /// single-unit packet, every unit of an aggregation packet, and the
    /// fragmented unit when the fragment's start bit is set.
    pub nal_types: Vec<u8>,
}

/// Reads an H.265 RTP payload without DONL fields; a payload shorter than its
/// header, a truncated fragmentation unit, and an aggregation packet with a
/// truncated, empty, or trailing unit are errors.
pub fn h265_payload(payload: &[u8]) -> anyhow::Result<H265Payload> {
    anyhow::ensure!(payload.len() >= 2, "truncated H.265 payload header");
    let payload_type = (payload[0] >> 1) & 0x3f;
    match payload_type {
        48 => {
            let mut nal_types = Vec::new();
            let mut offset = 2;
            while offset < payload.len() {
                anyhow::ensure!(
                    offset + 2 <= payload.len(),
                    "truncated H.265 aggregation unit size"
                );
                let size = usize::from(u16::from_be_bytes([payload[offset], payload[offset + 1]]));
                let next = offset + 2 + size;
                anyhow::ensure!(
                    size >= 2 && next <= payload.len(),
                    "truncated H.265 aggregation unit"
                );
                nal_types.push((payload[offset + 2] >> 1) & 0x3f);
                offset = next;
            }
            anyhow::ensure!(!nal_types.is_empty(), "empty H.265 aggregation packet");
            Ok(H265Payload {
                kind: PayloadKind::Aggregation,
                nal_types,
            })
        }
        49 => {
            anyhow::ensure!(payload.len() >= 3, "truncated H.265 fragmentation unit");
            let header = payload[2];
            Ok(H265Payload {
                kind: PayloadKind::Fragment,
                nal_types: if header & 0x80 == 0 {
                    Vec::new()
                } else {
                    vec![header & 0x3f]
                },
            })
        }
        _ => Ok(H265Payload {
            kind: PayloadKind::Single,
            nal_types: vec![payload_type],
        }),
    }
}

/// The clock and counters of an RTCP sender report (RFC 3550 §6.4.1).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SenderReport {
    /// The sender's NTP timestamp, as seconds since the Unix epoch.
    pub ntp_seconds: f64,
    /// The RTP timestamp of the same instant.
    pub rtp_timestamp: u32,
    /// Packets sent.
    pub packet_count: u32,
    /// Payload octets sent.
    pub octet_count: u32,
}

/// One packet of an RTCP compound packet.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RtcpPacket {
    /// The packet type: 200 for a sender report, 201 for a receiver report.
    pub packet_type: u8,
    /// The sender report's fields, for a sender report long enough to hold them.
    pub sender_report: Option<SenderReport>,
}

/// Splits an RTCP compound packet; a length past the end and trailing bytes
/// are errors.
pub fn rtcp_packets(data: &[u8]) -> anyhow::Result<Vec<RtcpPacket>> {
    let mut packets = Vec::new();
    let mut offset = 0;
    while offset < data.len() {
        anyhow::ensure!(offset + 4 <= data.len(), "trailing RTCP bytes");
        let packet_type = data[offset + 1];
        let words = usize::from(u16::from_be_bytes([data[offset + 2], data[offset + 3]]));
        let end = offset + (words + 1) * 4;
        anyhow::ensure!(end <= data.len(), "truncated RTCP packet");
        let packet = &data[offset..end];
        let word = |at: usize| {
            u32::from_be_bytes([packet[at], packet[at + 1], packet[at + 2], packet[at + 3]])
        };
        let sender_report = (packet_type == 200 && packet.len() >= 28).then(|| SenderReport {
            ntp_seconds: f64::from(word(8)) - NTP_UNIX_OFFSET
                + f64::from(word(12)) / 4_294_967_296.0,
            rtp_timestamp: word(16),
            packet_count: word(20),
            octet_count: word(24),
        });
        packets.push(RtcpPacket {
            packet_type,
            sender_report,
        });
        offset = end;
    }
    Ok(packets)
}

/// Extends 32-bit RTP timestamps onto a signed 64-bit timeline: each step from
/// the previous timestamp is read as a signed 32-bit difference, so a wrap
/// continues forward and a backward step stays backward.
#[derive(Clone, Copy, Debug, Default)]
pub struct TimestampUnwrapper {
    last: Option<(u32, i64)>,
}

impl TimestampUnwrapper {
    /// `timestamp` on the extended timeline; the first one keeps its value.
    pub fn extend(&mut self, timestamp: u32) -> i64 {
        let extended = self.last.map_or(i64::from(timestamp), |(last, extended)| {
            extended.saturating_add(i64::from(timestamp.wrapping_sub(last) as i32))
        });
        self.last = Some((timestamp, extended));
        extended
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const UTC: &str = "<tt:UTCDateTime><tt:Time><tt:Hour>1</tt:Hour><tt:Minute>2</tt:Minute>\
        <tt:Second>3</tt:Second></tt:Time><tt:Date><tt:Year>1970</tt:Year><tt:Month>1</tt:Month>\
        <tt:Day>2</tt:Day></tt:Date></tt:UTCDateTime>";

    #[test]
    fn camera_utc_reads_the_utc_block() {
        assert_eq!(camera_utc(UTC), Some(86_400.0 + 3_723.0));
    }

    #[test]
    fn camera_utc_accepts_any_namespace_prefix() {
        assert_eq!(camera_utc(&UTC.replace("tt:", "onvif:")), Some(90_123.0));
        assert_eq!(camera_utc(&UTC.replace("tt:", "")), Some(90_123.0));
    }

    #[test]
    fn camera_utc_skips_the_local_time_before_it() {
        let response = format!(
            "<tt:LocalDateTime><tt:Time><tt:Hour>9</tt:Hour></tt:Time></tt:LocalDateTime>{UTC}"
        );
        assert_eq!(camera_utc(&response), Some(90_123.0));
    }

    #[test]
    fn camera_utc_rejects_overflowing_fields() {
        for (field, value) in [
            ("Hour", "999999999999999999"),
            ("Year", "9223372036854775807"),
        ] {
            let response = UTC.replace(
                &format!(
                    "<tt:{field}>{}<",
                    if field == "Hour" { "1" } else { "1970" }
                ),
                &format!("<tt:{field}>{value}<"),
            );
            assert_eq!(camera_utc(&response), None, "{field}");
        }
    }

    fn rtp(first: u8, len: usize) -> Vec<u8> {
        let mut packet = vec![0u8; len];
        packet[0] = first;
        packet
    }

    #[test]
    fn rtp_packet_reads_the_header_and_payload_bounds() {
        // Version 2; the marker bit with payload type 96.
        let packet = [0x80, 0x80 | 0x60, 0, 7, 0, 0, 0, 9, 0, 0, 0, 1, 42];
        assert_eq!(
            rtp_packet(&packet).unwrap(),
            RtpPacket {
                marker: true,
                sequence: 7,
                timestamp: 9,
                extension: false,
                padded: false,
                payload: 12..13,
            }
        );
    }

    #[test]
    fn rtp_packet_rejects_padding_longer_than_the_packet_or_zero() {
        let mut long = rtp(0xa0, 13);
        long[12] = 255;
        assert!(rtp_packet(&long).is_err());
        assert!(rtp_packet(&rtp(0xa0, 13)).is_err());
    }

    #[test]
    fn rtp_packet_rejects_a_truncated_extension() {
        let mut extension = rtp(0x90, 16);
        extension[14..16].copy_from_slice(&u16::MAX.to_be_bytes());
        assert!(rtp_packet(&extension).is_err());
    }

    #[test]
    fn a_complete_response_is_framed_with_its_body() {
        assert_eq!(
            rtsp_response(b"RTSP/1.0 200 OK\r\nContent-Length: 1\r\n\r\nxtail").unwrap(),
            Some(ResponseFrame {
                head_end: 34,
                body: 38..39
            })
        );
    }

    #[test]
    fn a_response_waits_for_its_head_and_body() {
        assert_eq!(
            rtsp_response(b"RTSP/1.0 200 OK\r\nCSeq: 1\r\n").unwrap(),
            None
        );
        assert_eq!(
            rtsp_response(b"RTSP/1.0 200 OK\r\nContent-Length: 4\r\n\r\nab").unwrap(),
            None
        );
    }

    #[test]
    fn response_limits_and_lengths_are_strict() {
        let long = vec![b'x'; MAX_RTSP_HEADER + 4];
        for input in [
            &long[..],
            b"RTSP/1.0 200 OK\r\nContent-Length: 99999999\r\n\r\n",
            b"RTSP/1.0 200 OK\r\nContent-Length: 1\r\nContent-Length: 1\r\n\r\nx",
            b"RTSP/1.0 200 OK\r\nContent-Length: abc\r\n\r\n",
            b"RTSP/1.0 200 OK\r\nno colon\r\n\r\n",
        ] {
            assert!(
                rtsp_response(input).is_err(),
                "{:?}",
                &input[..input.len().min(60)]
            );
        }
    }

    #[test]
    fn demultiplex_separates_frames_responses_and_noise() {
        assert_eq!(
            demultiplex(b"$\x01\x00\x02abc").unwrap(),
            Interleaved::Data {
                channel: 1,
                payload: 4..6
            }
        );
        assert_eq!(
            demultiplex(b"$\x01\x00\x09ab").unwrap(),
            Interleaved::Incomplete
        );
        assert_eq!(demultiplex(b"noise$\x00").unwrap(), Interleaved::Skip(5));
        assert_eq!(demultiplex(b"noisenoise").unwrap(), Interleaved::Skip(6));
        assert_eq!(demultiplex(b"abc").unwrap(), Interleaved::Incomplete);
        let response = demultiplex(b"RTSP/1.0 200 OK\r\nCSeq: 2\r\n\r\n$").unwrap();
        assert_eq!(response.consumed(), 28);
    }

    #[test]
    fn status_cseq_and_session_fields_are_read() {
        let head = "RTSP/1.0 401 Unauthorized\r\nCSeq: 7\r\nSession: ABC;timeout=60";
        assert_eq!(response_status(head).unwrap(), 401);
        assert_eq!(response_cseq(head).unwrap(), 7);
        assert_eq!(session_id(header_value(head, "session").unwrap()), "ABC");
    }

    #[test]
    fn a_response_needs_exactly_one_cseq() {
        assert!(response_cseq("RTSP/1.0 200 OK").is_err());
        assert!(response_cseq("RTSP/1.0 200 OK\r\nCSeq: 7\r\nCSeq: 7").is_err());
        assert!(response_status("HTTP/1.1 200 OK").is_err());
        assert!(response_status("RTSP/1.0 2000 OK").is_err());
    }

    #[test]
    fn interleaved_channels_come_from_the_first_transport() {
        assert_eq!(
            interleaved_channels("RTP/AVP/TCP;unicast;interleaved=2-3;ssrc=1"),
            Some((2, 3))
        );
        assert_eq!(
            interleaved_channels("RTP/AVP/TCP;interleaved=4"),
            Some((4, 5))
        );
        assert_eq!(interleaved_channels("RTP/AVP/TCP;interleaved=255"), None);
        assert_eq!(
            interleaved_channels("RTP/AVP;unicast;client_port=1-2"),
            None
        );
    }

    #[test]
    fn the_digest_challenge_is_chosen_over_basic() {
        let head = "RTSP/1.0 401 Unauthorized\r\nWWW-Authenticate: Basic realm=\"basic\"\r\n\
                    WWW-Authenticate: Digest realm=\"Login to camera\", nonce=\"abc\", stale=FALSE";
        assert_eq!(
            digest_challenge(head),
            Some(DigestChallenge {
                realm: "Login to camera".into(),
                nonce: "abc".into()
            })
        );
    }

    #[test]
    fn an_incomplete_digest_challenge_is_none() {
        let head =
            "RTSP/1.0 401 Unauthorized\r\nWWW-Authenticate: Digest realm=\"x\", nonce=\"open";
        assert_eq!(digest_challenge(head), None);
    }

    #[test]
    fn sdp_controls_keep_order_and_the_first_control() {
        let sdp = "v=0\r\na=control:*\r\nm=video 0 RTP/AVP 96\r\na=control:trackID=0\r\n\
                   a=control:other\r\nm=application 0 RTP/AVP 107\r\nm=audio 0 RTP/AVP 0\r\n";
        assert_eq!(
            sdp_controls(sdp),
            [
                ("video".to_owned(), Some("trackID=0".to_owned())),
                ("application".to_owned(), None),
                ("audio".to_owned(), None),
            ]
        );
    }

    #[test]
    fn h265_payloads_report_the_units_they_start() {
        assert_eq!(
            h265_payload(&[19 << 1, 1, 0]).unwrap(),
            H265Payload {
                kind: PayloadKind::Single,
                nal_types: vec![19]
            }
        );
        assert_eq!(
            h265_payload(&[49 << 1, 1, 0x80 | 0x13, 0])
                .unwrap()
                .nal_types,
            [19]
        );
        assert_eq!(
            h265_payload(&[49 << 1, 1, 19, 0]).unwrap().nal_types,
            Vec::<u8>::new()
        );
        assert_eq!(
            h265_payload(&[48 << 1, 1, 0, 2, 32 << 1, 1, 0, 2, 33 << 1, 1])
                .unwrap()
                .nal_types,
            [32, 33]
        );
    }

    #[test]
    fn malformed_aggregation_packets_are_refused() {
        for payload in [
            &[48 << 1, 1][..],
            &[48 << 1, 1, 0, 9, 1, 1],
            &[48 << 1, 1, 0, 2, 32 << 1, 1, 0],
            &[49 << 1, 1],
            &[1],
        ] {
            assert!(h265_payload(payload).is_err(), "{payload:?}");
        }
    }

    #[test]
    fn rtcp_sender_reports_are_read_from_a_compound_packet() {
        let mut data = vec![0x80, 200, 0, 6];
        data.extend_from_slice(&[0, 0, 0, 1]);
        data.extend_from_slice(&2_208_988_801u32.to_be_bytes());
        data.extend_from_slice(&(1u32 << 31).to_be_bytes());
        data.extend_from_slice(&90_000u32.to_be_bytes());
        data.extend_from_slice(&5u32.to_be_bytes());
        data.extend_from_slice(&6u32.to_be_bytes());
        data.extend_from_slice(&[0x80, 202, 0, 0]);
        let packets = rtcp_packets(&data).unwrap();
        assert_eq!(packets.len(), 2);
        assert_eq!(
            packets[0].sender_report,
            Some(SenderReport {
                ntp_seconds: 1.5,
                rtp_timestamp: 90_000,
                packet_count: 5,
                octet_count: 6
            })
        );
        assert_eq!(packets[1].packet_type, 202);
    }

    #[test]
    fn truncated_and_trailing_rtcp_bytes_are_refused() {
        assert!(rtcp_packets(&[0x80, 200, 0, 6, 0, 0]).is_err());
        assert!(rtcp_packets(&[0x80, 202, 0, 0, 1]).is_err());
    }

    #[test]
    fn timestamps_wrap_forward_and_step_back() {
        let mut timeline = TimestampUnwrapper::default();
        assert_eq!(timeline.extend(u32::MAX - 1), i64::from(u32::MAX) - 1);
        assert_eq!(timeline.extend(2), i64::from(u32::MAX) + 3);
        assert_eq!(timeline.extend(u32::MAX), i64::from(u32::MAX));
    }
}
