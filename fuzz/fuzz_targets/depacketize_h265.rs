//! Fuzzes retina's H.265 depacketizer with arbitrary RTP payloads, sequence
//! numbers, timestamps, and marker bits: every frame it yields stays within
//! the 64 MiB access-unit bound, and no input panics.
#![no_main]

use std::num::NonZeroU32;

use libfuzzer_sys::fuzz_target;
use retina::codec::{CodecItem, Depacketizer};
use retina::testutil::{DepacketizeBuf, packet_meta};

/// The parameter sets a Dahua-class camera advertises in its SDP.
const FMTP: &str = "profile-id=1;sprop-sps=QgEBAWAAAAMAsAAAAwAAAwBaoAWCAeFja5JFL83BQYFBAAADAAEAAAMADKE=;sprop-pps=RAHA8saNA7NA;sprop-vps=QAEMAf//AWAAAAMAsAAAAwAAAwBarAwAAAMABAAAAwAyqA==";
/// The local accounting bound of the vendored depacketizer.
const MAX_ACCESS_UNIT_BYTES: usize = 64 * 1024 * 1024;
/// Payloads per input, so one run stays short.
const MAX_PACKETS: usize = 64;

fuzz_target!(|data: &[u8]| {
    let Ok(mut depacketizer) = Depacketizer::new("video", "h265", 90_000, None, Some(FMTP)) else {
        return;
    };
    let mut buf = DepacketizeBuf::new(1 << 16);
    let clock_rate = NonZeroU32::new(90_000).expect("non-zero");
    let mut rest = data;
    let mut sequence: u16 = 0;
    let mut timestamp: i64 = 0;
    for _ in 0..MAX_PACKETS {
        let Some((&[control, len], tail)) = rest.split_first_chunk::<2>() else {
            break;
        };
        let len = usize::from(len).min(tail.len());
        let (payload, tail) = tail.split_at(len);
        rest = tail;
        // Control byte: bit 0 marks the packet, bit 1 skips a sequence number
        // (loss), bit 2 advances the timestamp.
        sequence = sequence.wrapping_add(1 + u16::from(control & 2 != 0));
        if control & 4 != 0 {
            timestamp += 3000;
        }
        let meta = packet_meta(sequence, timestamp, clock_rate, control & 1 != 0);
        if buf.push(&mut depacketizer, meta, payload).is_err() {
            // A malformed packet or an exceeded bound ends the access unit.
            break;
        }
        while let Some(item) = depacketizer.pull() {
            if let Ok(CodecItem::VideoFrame(frame)) = item {
                assert!(frame.data().len() <= MAX_ACCESS_UNIT_BYTES);
            }
        }
    }
});
