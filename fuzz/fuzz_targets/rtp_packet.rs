//! Fuzzes retina's RTP header validation over a contiguous slice and over the
//! two halves of a ring buffer, which must agree on every input.
#![no_main]

use libfuzzer_sys::fuzz_target;
use retina::inputs::Split;
use retina::rtp::PacketHeader;

fuzz_target!(|data: &[u8]| {
    let Some((&split_at, packet)) = data.split_first() else {
        return;
    };
    let contiguous = PacketHeader::validate(packet);
    let at = usize::from(split_at).min(packet.len());
    let (first, second) = packet.split_at(at);
    let split = PacketHeader::validate(Split::new(first, second));
    assert_eq!(
        contiguous.as_ref().map(|(_, range)| range.clone()),
        split.as_ref().map(|(_, range)| range.clone()),
        "a split view validates like the contiguous packet"
    );
    assert_eq!(contiguous.is_ok(), split.is_ok());
    if let Ok((_, payload)) = contiguous {
        assert!(usize::from(payload.end) <= packet.len());
        assert!(payload.start <= payload.end);
    }
});
