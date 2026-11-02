//! The session description for one mount (RFC 4566, RFC 7798 §7.1).

/// Builds the SDP for a mount whose parameter sets are `sets` (VPS, SPS, PPS in
/// any order; each is announced under its type).
#[must_use]
pub fn describe(sets: &[Vec<u8>], frame_rate: f64) -> String {
    let mut fmtp = String::from("a=fmtp:96");
    let mut first = true;
    for nal in sets {
        let key = match (nal[0] >> 1) & 0x3f {
            32 => "sprop-vps",
            33 => "sprop-sps",
            34 => "sprop-pps",
            _ => continue,
        };
        fmtp.push(if first { ' ' } else { ';' });
        first = false;
        fmtp.push_str(key);
        fmtp.push('=');
        fmtp.push_str(&base64(nal));
    }
    format!(
        "v=0\r\n\
         o=- 1 1 IN IP4 127.0.0.1\r\n\
         s=iris-replay\r\n\
         t=0 0\r\n\
         a=control:*\r\n\
         m=video 0 RTP/AVP 96\r\n\
         a=rtpmap:96 H265/90000\r\n\
         {fmtp}\r\n\
         a=framerate:{frame_rate:.3}\r\n\
         a=control:trackID=0\r\n"
    )
}

/// Standard base64 with padding.
#[must_use]
pub fn base64(input: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        out.push(char::from(TABLE[(n >> 18) as usize & 63]));
        out.push(char::from(TABLE[(n >> 12) as usize & 63]));
        out.push(if chunk.len() > 1 {
            char::from(TABLE[(n >> 6) as usize & 63])
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            char::from(TABLE[n as usize & 63])
        } else {
            '='
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_matches_rfc_4648_vectors() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn sdp_announces_each_parameter_set_under_its_type() {
        let sdp = describe(
            &[
                vec![0x44, 0x01, 0xc0],
                vec![0x40, 0x01, 0x0c],
                vec![0x42, 0x01, 0x01],
            ],
            30.0,
        );
        assert!(
            sdp.contains("a=fmtp:96 sprop-pps=RAHA;sprop-vps=QAEM;sprop-sps=QgEB\r\n"),
            "{sdp}"
        );
        assert!(sdp.contains("m=video 0 RTP/AVP 96\r\n"));
        assert!(sdp.contains("a=rtpmap:96 H265/90000\r\n"));
        assert!(sdp.ends_with("a=control:trackID=0\r\n"));
    }
}
