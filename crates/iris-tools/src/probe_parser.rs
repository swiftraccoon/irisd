//! Bounded, non-panicking parsers for camera-controlled probe input.

use anyhow::Context;

use crate::time::days_from_civil;

const MAX_RTSP_HEADER: usize = 64 * 1024;
const MAX_RTSP_BODY: usize = 4 * 1024 * 1024;

fn tag(text: &str, name: &str) -> Option<i64> {
    let open = format!("<tt:{name}>");
    let start = text.find(&open)?.checked_add(open.len())?;
    let end = text.get(start..)?.find('<')?.checked_add(start)?;
    text.get(start..end)?.trim().parse().ok()
}

/// Parses the camera UTC value in an ONVIF `GetSystemDateAndTime` response.
pub fn camera_utc(response: &str) -> Option<f64> {
    let utc_start = response.find("<tt:UTCDateTime>")?;
    let utc = response.get(utc_start..)?;
    let (hour, minute, second) = (tag(utc, "Hour")?, tag(utc, "Minute")?, tag(utc, "Second")?);
    let (year, month, day) = (tag(utc, "Year")?, tag(utc, "Month")?, tag(utc, "Day")?);
    if !(1..=9999).contains(&year)
        || !(0..=23).contains(&hour)
        || !(0..=59).contains(&minute)
        || !(0..=59).contains(&second)
    {
        return None;
    }
    let whole = days_from_civil(year, month, day)?
        .checked_mul(86_400)?
        .checked_add(hour.checked_mul(3_600)?)?
        .checked_add(minute.checked_mul(60)?)?
        .checked_add(second)?;
    Some(whole as f64)
}

/// Returns the payload bounds of one complete RTP packet.
pub fn rtp_payload_bounds(packet: &[u8]) -> anyhow::Result<(usize, usize)> {
    anyhow::ensure!(
        packet.len() >= 12,
        "RTP packet is shorter than its fixed header"
    );
    anyhow::ensure!(packet[0] >> 6 == 2, "unsupported RTP version");
    let cc = usize::from(packet[0] & 0x0f);
    let mut offset = 12usize
        .checked_add(cc.checked_mul(4).context("RTP CSRC overflow")?)
        .context("RTP header overflow")?;
    anyhow::ensure!(offset <= packet.len(), "truncated RTP CSRC list");
    if packet[0] & 0x10 != 0 {
        let header_end = offset.checked_add(4).context("RTP extension overflow")?;
        anyhow::ensure!(header_end <= packet.len(), "truncated RTP extension header");
        let words = usize::from(u16::from_be_bytes([packet[offset + 2], packet[offset + 3]]));
        offset = header_end
            .checked_add(words.checked_mul(4).context("RTP extension overflow")?)
            .context("RTP extension overflow")?;
        anyhow::ensure!(offset <= packet.len(), "truncated RTP extension data");
    }
    let end = if packet[0] & 0x20 != 0 {
        let padding = usize::from(*packet.last().context("missing RTP padding length")?);
        anyhow::ensure!(
            padding > 0 && padding <= packet.len(),
            "invalid RTP padding length"
        );
        packet.len() - padding
    } else {
        packet.len()
    };
    anyhow::ensure!(offset <= end, "RTP padding overlaps the header");
    Ok((offset, end))
}

/// Parses the framing of a complete RTSP response without allocating from a
/// peer-controlled length. The returned pair is (header end, body length).
pub fn rtsp_response_bounds(input: &[u8]) -> anyhow::Result<(usize, usize)> {
    let scan_len = input.len().min(MAX_RTSP_HEADER.saturating_add(4));
    let marker = input[..scan_len]
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .with_context(|| {
            if input.len() > MAX_RTSP_HEADER {
                "RTSP header exceeds 64 KiB"
            } else {
                "incomplete RTSP header"
            }
        })?;
    anyhow::ensure!(marker <= MAX_RTSP_HEADER, "RTSP header exceeds 64 KiB");
    let header = std::str::from_utf8(&input[..marker]).context("RTSP header is not UTF-8")?;
    let mut length = None;
    for line in header.lines().skip(1) {
        let Some((name, value)) = line.split_once(':') else {
            anyhow::bail!("malformed RTSP header line");
        };
        if name.trim().eq_ignore_ascii_case("Content-Length") {
            anyhow::ensure!(length.is_none(), "duplicate Content-Length");
            length = Some(value.trim().parse::<usize>()?);
        }
    }
    let length = length.unwrap_or(0);
    anyhow::ensure!(length <= MAX_RTSP_BODY, "RTSP body exceeds 4 MiB");
    let body_start = marker.checked_add(4).context("RTSP framing overflow")?;
    anyhow::ensure!(
        body_start
            .checked_add(length)
            .is_some_and(|end| end <= input.len()),
        "incomplete RTSP body"
    );
    Ok((body_start, length))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn response_and_packet_bounds_are_strict() {
        assert_eq!(
            rtsp_response_bounds(b"RTSP/1.0 200 OK\r\nContent-Length: 1\r\n\r\nx").unwrap(),
            (38, 1)
        );
        assert!(
            rtsp_response_bounds(b"RTSP/1.0 200 OK\r\nContent-Length: 99999999\r\n\r\n").is_err()
        );
        let mut padding = vec![0u8; 13];
        padding[0] = 0xa0;
        padding[12] = 255;
        assert!(rtp_payload_bounds(&padding).is_err());
    }
}
