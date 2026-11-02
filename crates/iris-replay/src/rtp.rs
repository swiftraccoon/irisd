//! RTP packetization per RFC 7798 and RTCP sender reports per RFC 3550, framed
//! for TCP interleaving per RFC 2326 §10.12.

/// RTP header length.
pub const RTP_HEADER_LEN: usize = 12;
/// Payload type announced in the SDP.
pub const PAYLOAD_TYPE: u8 = 96;
/// NAL unit type of a fragmentation unit.
const FU_TYPE: u8 = 49;

/// Sequence and SSRC state of one RTP stream.
#[derive(Debug)]
pub struct Packetizer {
    ssrc: u32,
    seq: u16,
    /// RTP packets produced.
    pub packets: u64,
    /// RTP payload bytes produced (headers excluded).
    pub octets: u64,
}

impl Packetizer {
    /// Starts a stream at `seq` with `ssrc`.
    #[must_use]
    pub fn new(ssrc: u32, seq: u16) -> Self {
        Self {
            ssrc,
            seq,
            packets: 0,
            octets: 0,
        }
    }

    /// The SSRC.
    #[must_use]
    pub fn ssrc(&self) -> u32 {
        self.ssrc
    }

    /// The sequence number the next packet will carry.
    #[must_use]
    pub fn next_seq(&self) -> u16 {
        self.seq
    }

    /// Packetizes the NAL units of one access unit at `timestamp`. The last
    /// packet carries the marker bit. Each NAL unit of `mtu` bytes or fewer
    /// (RTP header included) becomes one packet; larger ones become
    /// fragmentation units.
    pub fn access_unit(&mut self, nals: &[&[u8]], timestamp: u32, mtu: usize) -> Vec<Vec<u8>> {
        let mut out = Vec::new();
        let max_payload = mtu.saturating_sub(RTP_HEADER_LEN).max(4);
        for (index, nal) in nals.iter().enumerate() {
            let last_nal = index + 1 == nals.len();
            if nal.len() <= max_payload {
                out.push(self.packet(timestamp, last_nal, nal));
                continue;
            }
            let header = [(nal[0] & 0x81) | (FU_TYPE << 1), nal[1]];
            let nal_type = (nal[0] >> 1) & 0x3f;
            let chunk = max_payload - 3;
            let body = &nal[2..];
            let chunks = body.chunks(chunk).collect::<Vec<_>>();
            for (i, part) in chunks.iter().enumerate() {
                let start = i == 0;
                let end = i + 1 == chunks.len();
                let fu = (u8::from(start) << 7) | (u8::from(end) << 6) | nal_type;
                let mut payload = Vec::with_capacity(3 + part.len());
                payload.extend_from_slice(&header);
                payload.push(fu);
                payload.extend_from_slice(part);
                out.push(self.packet(timestamp, last_nal && end, &payload));
            }
        }
        out
    }

    fn packet(&mut self, timestamp: u32, marker: bool, payload: &[u8]) -> Vec<u8> {
        let mut p = Vec::with_capacity(RTP_HEADER_LEN + payload.len());
        p.push(0x80);
        p.push(PAYLOAD_TYPE | (u8::from(marker) << 7));
        p.extend_from_slice(&self.seq.to_be_bytes());
        p.extend_from_slice(&timestamp.to_be_bytes());
        p.extend_from_slice(&self.ssrc.to_be_bytes());
        p.extend_from_slice(payload);
        self.seq = self.seq.wrapping_add(1);
        self.packets += 1;
        self.octets += payload.len() as u64;
        p
    }

    /// An RTCP sender report for the stream's current counters.
    #[must_use]
    pub fn sender_report(&self, ntp: u64, rtp_timestamp: u32) -> Vec<u8> {
        let mut p = Vec::with_capacity(28);
        p.push(0x80); // V=2, no padding, zero report blocks
        p.push(200); // SR
        p.extend_from_slice(&6u16.to_be_bytes()); // length in words minus one
        p.extend_from_slice(&self.ssrc.to_be_bytes());
        p.extend_from_slice(&ntp.to_be_bytes());
        p.extend_from_slice(&rtp_timestamp.to_be_bytes());
        p.extend_from_slice(
            &u32::try_from(self.packets)
                .unwrap_or(u32::MAX)
                .to_be_bytes(),
        );
        p.extend_from_slice(&u32::try_from(self.octets).unwrap_or(u32::MAX).to_be_bytes());
        p
    }
}

/// Appends `packet` to `out` as an interleaved frame on `channel`.
pub fn interleave(out: &mut Vec<u8>, channel: u8, packet: &[u8]) {
    let len = u16::try_from(packet.len()).expect("packet fits the interleaved length field");
    out.push(b'$');
    out.push(channel);
    out.extend_from_slice(&len.to_be_bytes());
    out.extend_from_slice(packet);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn small_nal_units_become_single_packets_with_marker_on_the_last() {
        let mut p = Packetizer::new(0x1234_5678, 65_534);
        let out = p.access_unit(
            &[&[0x40, 0x01, 0xaa], &[0x42, 0x01, 0xbb, 0xcc]],
            90_000,
            1_400,
        );
        assert_eq!(out.len(), 2);
        assert_eq!(out[0][1], PAYLOAD_TYPE);
        assert_eq!(out[1][1], PAYLOAD_TYPE | 0x80);
        assert_eq!(&out[0][2..4], &65_534u16.to_be_bytes());
        assert_eq!(&out[1][2..4], &65_535u16.to_be_bytes());
        assert_eq!(&out[0][4..8], &90_000u32.to_be_bytes());
        assert_eq!(&out[1][12..], &[0x42, 0x01, 0xbb, 0xcc]);
        assert_eq!(p.next_seq(), 0);
        assert_eq!((p.packets, p.octets), (2, 7));
    }

    #[test]
    fn large_nal_units_become_fragmentation_units_that_reassemble() {
        let mut nal = vec![0x26, 0x01]; // IDR_W_RADL, layer 0, tid 1
        nal.extend((0..5_000u32).map(|i| (i % 251) as u8));
        let mut p = Packetizer::new(1, 0);
        let out = p.access_unit(&[&nal], 7, 1_400);
        assert!(out.len() > 3);
        let mut body = Vec::new();
        for (i, packet) in out.iter().enumerate() {
            let payload = &packet[RTP_HEADER_LEN..];
            assert!(payload.len() <= 1_400 - RTP_HEADER_LEN);
            assert_eq!(payload[0] >> 1 & 0x3f, FU_TYPE);
            assert_eq!(payload[1], 0x01);
            let fu = payload[2];
            assert_eq!(fu & 0x3f, 19);
            assert_eq!(fu & 0x80 != 0, i == 0, "start bit");
            assert_eq!(fu & 0x40 != 0, i + 1 == out.len(), "end bit");
            assert_eq!(packet[1] & 0x80 != 0, i + 1 == out.len(), "marker");
            body.extend_from_slice(&payload[3..]);
        }
        assert_eq!(body, nal[2..]);
    }

    #[test]
    fn sender_report_and_interleaving_have_the_standard_layout() {
        let mut p = Packetizer::new(0xdead_beef, 10);
        p.access_unit(&[&[0x40, 0x01, 0x00]], 0, 1_400);
        let sr = p.sender_report(0x1122_3344_5566_7788, 90);
        assert_eq!(sr.len(), 28);
        assert_eq!(&sr[..4], &[0x80, 200, 0, 6]);
        assert_eq!(&sr[4..8], &0xdead_beefu32.to_be_bytes());
        assert_eq!(&sr[8..16], &0x1122_3344_5566_7788u64.to_be_bytes());
        assert_eq!(&sr[16..20], &90u32.to_be_bytes());
        assert_eq!(&sr[20..24], &1u32.to_be_bytes());
        assert_eq!(&sr[24..28], &3u32.to_be_bytes());
        let mut out = Vec::new();
        interleave(&mut out, 1, &sr);
        assert_eq!(&out[..4], &[b'$', 1, 0, 28]);
        assert_eq!(&out[4..], &sr[..]);
    }
}
