//! RTP packetization per RFC 7798 and RTCP sender reports per RFC 3550, framed
//! for TCP interleaving per RFC 2326 §10.12.

use std::fmt;
use std::iter::FusedIterator;

use iris_core::time::WallUs;

use crate::media::Ticks;

/// RTP header length.
pub const RTP_HEADER_LEN: usize = 12;
/// Smallest supported RTP packet size, including its header.
///
/// The floor bounds packet-count amplification while retaining ample room for
/// H.265 fragmentation-unit headers.
pub const MIN_MTU: usize = 256;
/// Largest RTP packet representable by RTSP TCP interleaving.
pub const MAX_MTU: usize = u16::MAX as usize;
/// Payload type announced in the SDP.
pub const PAYLOAD_TYPE: u8 = 96;
/// NAL unit type of a fragmentation unit.
const FU_TYPE: u8 = 49;

/// Error while packetizing or framing RTP.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum RtpError {
    /// The MTU is outside the supported operational range.
    #[error(
        "RTP MTU {mtu} is outside the supported {}..={} range",
        MIN_MTU,
        MAX_MTU
    )]
    InvalidMtu {
        /// Rejected value.
        mtu: usize,
    },
    /// An H.265 NAL unit does not contain its two-byte header.
    #[error("NAL unit {index} has {len} bytes; at least 2 required")]
    NalTooShort {
        /// Position in the access unit.
        index: usize,
        /// Available bytes.
        len: usize,
    },
    /// The number of packets cannot be represented by `usize`.
    #[error("RTP packet count overflow")]
    PacketCountOverflow,
    /// A packet cannot fit the RTSP interleaved-frame length field.
    #[error(
        "RTP/RTCP packet has {len} bytes; at most {} fit an interleaved frame",
        MAX_MTU
    )]
    InterleavedPacketTooLarge {
        /// Packet bytes, excluding the four-byte interleaved header.
        len: usize,
    },
}

/// An RTP synchronization source identifier (RFC 3550 §5.1).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Ssrc(u32);

impl Ssrc {
    /// Wraps an identifier.
    #[must_use]
    pub const fn new(id: u32) -> Self {
        Self(id)
    }

    /// The identifier.
    #[must_use]
    pub const fn get(self) -> u32 {
        self.0
    }
}

/// A value of an RTP stream's 90 kHz media clock, which wraps at 32 bits
/// (RFC 3550 §5.1).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct RtpTimestamp(u32);

impl RtpTimestamp {
    /// Wraps a clock value.
    #[must_use]
    pub const fn new(value: u32) -> Self {
        Self(value)
    }

    /// The clock value.
    #[must_use]
    pub const fn get(self) -> u32 {
        self.0
    }

    /// The clock `ticks` later, wrapping as the 32-bit field does.
    #[must_use]
    pub const fn after(self, ticks: Ticks) -> Self {
        Self(self.0.wrapping_add(ticks.get() as u32))
    }
}

/// Validates an RTP MTU used by the packetizer and replay server.
///
/// # Errors
///
/// [`RtpError::InvalidMtu`] when `mtu` is outside [`MIN_MTU`]..=[`MAX_MTU`].
pub fn validate_mtu(mtu: usize) -> Result<(), RtpError> {
    if !(MIN_MTU..=MAX_MTU).contains(&mtu) {
        return Err(RtpError::InvalidMtu { mtu });
    }
    Ok(())
}

/// Sequence, SSRC, and counter state of one RTP stream.
#[derive(Debug)]
pub struct Packetizer {
    ssrc: Ssrc,
    seq: u16,
    packets: u64,
    octets: u64,
}

impl Packetizer {
    /// Starts a stream at sequence number `seq` with `ssrc`.
    #[must_use]
    pub fn new(ssrc: Ssrc, seq: u16) -> Self {
        Self {
            ssrc,
            seq,
            packets: 0,
            octets: 0,
        }
    }

    /// The SSRC.
    #[must_use]
    pub fn ssrc(&self) -> Ssrc {
        self.ssrc
    }

    /// RTP packets produced.
    #[must_use]
    pub fn packets(&self) -> u64 {
        self.packets
    }

    /// RTP payload bytes produced, headers excluded.
    #[must_use]
    pub fn octets(&self) -> u64 {
        self.octets
    }

    /// Sets the counters, for rollover tests.
    #[cfg(test)]
    fn set_counts(&mut self, packets: u64, octets: u64) {
        self.packets = packets;
        self.octets = octets;
    }

    /// The sequence number the next packet will carry.
    #[must_use]
    pub fn next_seq(&self) -> u16 {
        self.seq
    }

    /// Packetizes the NAL units of one access unit at `timestamp`.
    ///
    /// The returned iterator constructs one packet at a time, so a large
    /// access unit does not allocate an intermediate packet list. The last
    /// packet carries the marker bit. Each NAL unit that fits `mtu` becomes one
    /// packet; larger NAL units become fragmentation units.
    ///
    /// # Errors
    ///
    /// Returns an error when `mtu` is outside [`MIN_MTU`]..=[`MAX_MTU`], a NAL
    /// unit lacks its two-byte header, or the packet count overflows `usize`.
    pub fn access_unit<'packetizer, 'data>(
        &'packetizer mut self,
        nals: &'data [&'data [u8]],
        timestamp: RtpTimestamp,
        mtu: usize,
    ) -> Result<AccessUnitPackets<'packetizer, 'data>, RtpError> {
        validate_mtu(mtu)?;
        let max_payload = mtu - RTP_HEADER_LEN;
        let fragment_payload = max_payload - 3;
        let mut remaining = 0usize;
        for (index, nal) in nals.iter().enumerate() {
            if nal.len() < 2 {
                return Err(RtpError::NalTooShort {
                    index,
                    len: nal.len(),
                });
            }
            let packets = if nal.len() <= max_payload {
                1
            } else {
                (nal.len() - 2).div_ceil(fragment_payload)
            };
            remaining = remaining
                .checked_add(packets)
                .ok_or(RtpError::PacketCountOverflow)?;
        }
        Ok(AccessUnitPackets {
            packetizer: self,
            nals,
            timestamp,
            max_payload,
            nal_index: 0,
            fragment_offset: 0,
            remaining,
        })
    }

    fn packet(&mut self, timestamp: RtpTimestamp, marker: bool, payload: &[u8]) -> Vec<u8> {
        let mut p = Vec::with_capacity(RTP_HEADER_LEN + payload.len());
        p.push(0x80);
        p.push(PAYLOAD_TYPE | (u8::from(marker) << 7));
        p.extend_from_slice(&self.seq.to_be_bytes());
        p.extend_from_slice(&timestamp.get().to_be_bytes());
        p.extend_from_slice(&self.ssrc.get().to_be_bytes());
        p.extend_from_slice(payload);
        self.seq = self.seq.wrapping_add(1);
        self.packets += 1;
        self.octets += payload.len() as u64;
        p
    }

    /// An RTCP sender report for the stream's current counters, mapping the
    /// wall time `wall` (sent as a 64-bit NTP timestamp) to `rtp_timestamp`.
    ///
    /// RFC 3550 wire counters are 32 bits, so the low 32 bits are transmitted
    /// and naturally roll over while the public totals continue in `u64`.
    #[must_use]
    pub fn sender_report(&self, wall: WallUs, rtp_timestamp: RtpTimestamp) -> Vec<u8> {
        let mut p = Vec::with_capacity(28);
        p.push(0x80); // V=2, no padding, zero report blocks
        p.push(200); // SR
        p.extend_from_slice(&6u16.to_be_bytes()); // length in words minus one
        p.extend_from_slice(&self.ssrc.get().to_be_bytes());
        p.extend_from_slice(&wall.to_ntp64().to_be_bytes());
        p.extend_from_slice(&rtp_timestamp.get().to_be_bytes());
        p.extend_from_slice(&(self.packets as u32).to_be_bytes());
        p.extend_from_slice(&(self.octets as u32).to_be_bytes());
        p
    }
}

/// A streaming iterator over one packetized access unit.
#[must_use = "packetization does not advance until the iterator is consumed"]
pub struct AccessUnitPackets<'packetizer, 'data> {
    packetizer: &'packetizer mut Packetizer,
    nals: &'data [&'data [u8]],
    timestamp: RtpTimestamp,
    max_payload: usize,
    nal_index: usize,
    fragment_offset: usize,
    remaining: usize,
}

impl Iterator for AccessUnitPackets<'_, '_> {
    type Item = Vec<u8>;

    fn next(&mut self) -> Option<Self::Item> {
        let nal = *self.nals.get(self.nal_index)?;
        let last_nal = self.nal_index + 1 == self.nals.len();
        self.remaining -= 1;
        if nal.len() <= self.max_payload {
            self.nal_index += 1;
            return Some(self.packetizer.packet(self.timestamp, last_nal, nal));
        }

        let body = &nal[2..];
        let fragment_payload = self.max_payload - 3;
        let end_offset = self
            .fragment_offset
            .saturating_add(fragment_payload)
            .min(body.len());
        let start = self.fragment_offset == 0;
        let end = end_offset == body.len();
        let header = [(nal[0] & 0x81) | (FU_TYPE << 1), nal[1]];
        let nal_type = (nal[0] >> 1) & 0x3f;
        let fu = (u8::from(start) << 7) | (u8::from(end) << 6) | nal_type;
        let mut payload = Vec::with_capacity(3 + end_offset - self.fragment_offset);
        payload.extend_from_slice(&header);
        payload.push(fu);
        payload.extend_from_slice(&body[self.fragment_offset..end_offset]);
        if end {
            self.nal_index += 1;
            self.fragment_offset = 0;
        } else {
            self.fragment_offset = end_offset;
        }
        Some(
            self.packetizer
                .packet(self.timestamp, last_nal && end, &payload),
        )
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        (self.remaining, Some(self.remaining))
    }
}

impl ExactSizeIterator for AccessUnitPackets<'_, '_> {}
impl FusedIterator for AccessUnitPackets<'_, '_> {}

impl fmt::Debug for AccessUnitPackets<'_, '_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AccessUnitPackets")
            .field("remaining", &self.remaining)
            .finish_non_exhaustive()
    }
}

/// Appends `packet` to `out` as an interleaved frame on `channel`.
///
/// # Errors
///
/// Returns [`RtpError::InterleavedPacketTooLarge`] when the packet cannot fit
/// the interleaved frame's 16-bit length field. `out` remains unchanged.
pub fn interleave(out: &mut Vec<u8>, channel: u8, packet: &[u8]) -> Result<(), RtpError> {
    let len = u16::try_from(packet.len())
        .map_err(|_| RtpError::InterleavedPacketTooLarge { len: packet.len() })?;
    out.push(b'$');
    out.push(channel);
    out.extend_from_slice(&len.to_be_bytes());
    out.extend_from_slice(packet);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn small_nal_units_become_single_packets_with_marker_on_the_last() {
        let mut p = Packetizer::new(Ssrc::new(0x1234_5678), 65_534);
        let out = p
            .access_unit(
                &[&[0x40, 0x01, 0xaa], &[0x42, 0x01, 0xbb, 0xcc]],
                RtpTimestamp::new(90_000),
                1_400,
            )
            .unwrap()
            .collect::<Vec<_>>();
        assert_eq!(out.len(), 2);
        assert_eq!(out[0][1], PAYLOAD_TYPE);
        assert_eq!(out[1][1], PAYLOAD_TYPE | 0x80);
        assert_eq!(&out[0][2..4], &65_534u16.to_be_bytes());
        assert_eq!(&out[1][2..4], &65_535u16.to_be_bytes());
        assert_eq!(&out[0][4..8], &90_000u32.to_be_bytes());
        assert_eq!(&out[1][12..], &[0x42, 0x01, 0xbb, 0xcc]);
        assert_eq!(p.next_seq(), 0);
        assert_eq!((p.packets(), p.octets()), (2, 7));
    }

    #[test]
    fn large_nal_units_become_fragmentation_units_that_reassemble() {
        let mut nal = vec![0x26, 0x01]; // IDR_W_RADL, layer 0, tid 1
        nal.extend((0..5_000u32).map(|i| (i % 251) as u8));
        let mut p = Packetizer::new(Ssrc::new(1), 0);
        let out = p
            .access_unit(&[&nal], RtpTimestamp::new(7), 1_400)
            .unwrap()
            .collect::<Vec<_>>();
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
        assert_eq!(usize::from(p.next_seq()), out.len());
    }

    /// A NAL unit whose body fills its fragments exactly needs no further
    /// packet, and the announced length counts down as packets are taken.
    #[test]
    fn the_packet_count_is_exact_at_a_fragment_boundary_and_counts_down() {
        let fragment_payload = 1_400 - RTP_HEADER_LEN - 3;
        let nal = vec![0x26; 2 + 3 * fragment_payload];
        let nals = [nal.as_slice()];
        let mut p = Packetizer::new(Ssrc::new(1), 0);
        let mut packets = p.access_unit(&nals, RtpTimestamp::new(0), 1_400).unwrap();
        assert_eq!(packets.len(), 3);
        assert!(format!("{packets:?}").contains("remaining: 3"));
        assert!(packets.next().is_some());
        assert_eq!(packets.len(), 2);
        assert_eq!(packets.count(), 2);
    }

    /// A sender report after one three-byte packet, mapping `wall` to RTP time 90.
    fn report_after_one_packet(wall: WallUs) -> Vec<u8> {
        let mut p = Packetizer::new(Ssrc::new(0xdead_beef), 10);
        let nals = [&[0x40, 0x01, 0x00][..]];
        assert_eq!(
            p.access_unit(&nals, RtpTimestamp::new(0), 1_400)
                .unwrap()
                .count(),
            1
        );
        p.sender_report(wall, RtpTimestamp::new(90))
    }

    #[test]
    fn sender_reports_have_the_rfc_3550_layout() {
        let wall = WallUs::new(1_700_000_000_000_000);
        let sr = report_after_one_packet(wall);
        assert_eq!(sr.len(), 28);
        assert_eq!(&sr[..4], &[0x80, 200, 0, 6]);
        assert_eq!(&sr[4..8], &0xdead_beefu32.to_be_bytes());
        assert_eq!(&sr[8..16], &wall.to_ntp64().to_be_bytes());
        assert_eq!(&sr[16..20], &90u32.to_be_bytes());
        assert_eq!(&sr[20..24], &1u32.to_be_bytes());
        assert_eq!(&sr[24..28], &3u32.to_be_bytes());
    }

    #[test]
    fn interleaving_prefixes_the_channel_and_length() {
        let sr = report_after_one_packet(WallUs::new(0));
        let mut out = Vec::new();
        interleave(&mut out, 1, &sr).unwrap();
        assert_eq!(&out[..4], &[b'$', 1, 0, 28]);
        assert_eq!(&out[4..], &sr[..]);
    }

    #[test]
    fn rtp_timestamps_wrap_at_32_bits() {
        assert_eq!(
            RtpTimestamp::new(u32::MAX).after(Ticks::new(2)),
            RtpTimestamp::new(1)
        );
        assert_eq!(
            RtpTimestamp::new(5).after(Ticks::new(u64::from(u32::MAX) + 1)),
            RtpTimestamp::new(5)
        );
    }

    #[test]
    fn mtus_are_admitted_only_inside_the_supported_range() {
        assert!(matches!(
            validate_mtu(MIN_MTU - 1),
            Err(RtpError::InvalidMtu { .. })
        ));
        assert_eq!(validate_mtu(MIN_MTU), Ok(()));
        assert_eq!(validate_mtu(MAX_MTU), Ok(()));
        assert!(matches!(
            validate_mtu(MAX_MTU + 1),
            Err(RtpError::InvalidMtu { .. })
        ));
    }

    #[test]
    fn a_packet_too_large_to_interleave_leaves_the_buffer_unchanged() {
        let mut framed = vec![1, 2, 3];
        interleave(&mut framed, 4, &vec![0; MAX_MTU]).unwrap();
        let committed_len = framed.len();
        assert!(matches!(
            interleave(&mut framed, 4, &vec![0; MAX_MTU + 1]),
            Err(RtpError::InterleavedPacketTooLarge { .. })
        ));
        assert_eq!(framed.len(), committed_len, "failed framing is atomic");
    }

    #[test]
    fn nal_units_shorter_than_their_header_are_rejected() {
        let mut p = Packetizer::new(Ssrc::new(1), 0);
        let short = [&[0x40][..]];
        assert!(matches!(
            p.access_unit(&short, RtpTimestamp::new(0), 1_400),
            Err(RtpError::NalTooShort { index: 0, len: 1 })
        ));
    }

    #[test]
    fn packetization_streams_an_exact_packet_count() {
        let mut p = Packetizer::new(Ssrc::new(1), 0);
        let nal = vec![0x26; 10_000];
        let nals = [nal.as_slice()];
        let packets = p.access_unit(&nals, RtpTimestamp::new(0), MIN_MTU).unwrap();
        assert_eq!(packets.len(), (nal.len() - 2).div_ceil(MIN_MTU - 15));
        assert_eq!(packets.count(), 42);
    }

    #[test]
    fn packets_at_the_largest_mtu_fit_interleaving() {
        let mut p = Packetizer::new(Ssrc::new(1), 0);
        let maximum_nal = vec![0x26; MAX_MTU * 2];
        let maximum_nals = [maximum_nal.as_slice()];
        let maximum_packets = p
            .access_unit(&maximum_nals, RtpTimestamp::new(0), MAX_MTU)
            .unwrap()
            .collect::<Vec<_>>();
        assert!(maximum_packets.len() > 1);
        for packet in maximum_packets {
            assert!(packet.len() <= MAX_MTU);
            let mut framed = Vec::new();
            interleave(&mut framed, 0, &packet).unwrap();
            assert_eq!(
                usize::from(u16::from_be_bytes([framed[2], framed[3]])),
                packet.len()
            );
        }
    }

    #[test]
    fn sender_report_counts_roll_over_instead_of_freezing() {
        let report = |p: &Packetizer| p.sender_report(WallUs::new(0), RtpTimestamp::new(0));
        let mut p = Packetizer::new(Ssrc::new(1), 0);
        p.set_counts(u64::from(u32::MAX), u64::from(u32::MAX));
        let exact = report(&p);
        assert_eq!(&exact[20..24], &u32::MAX.to_be_bytes());
        assert_eq!(&exact[24..28], &u32::MAX.to_be_bytes());

        p.set_counts(u64::from(u32::MAX) + 1, u64::from(u32::MAX) + 2);
        let first_rollover = report(&p);
        assert_eq!(&first_rollover[20..24], &0u32.to_be_bytes());
        assert_eq!(&first_rollover[24..28], &1u32.to_be_bytes());

        p.set_counts(u64::from(u32::MAX) + 7, u64::from(u32::MAX) + 9);
        let later = report(&p);
        assert_eq!(&later[20..24], &6u32.to_be_bytes());
        assert_eq!(&later[24..28], &8u32.to_be_bytes());
        assert_ne!(first_rollover[20..28], later[20..28]);
    }
}
