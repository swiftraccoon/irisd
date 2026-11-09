//! Fuzzes ISO BMFF box-header parsing and the big-endian field readers at
//! arbitrary offsets.
#![no_main]

use iris_mp4::boxes::{BoxHeader, be_u32, be_u64};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Some(header) = BoxHeader::parse(data) {
        let _ = header.payload_len();
    }
    let offset = match data {
        [high, low, ..] => usize::from(u16::from_be_bytes([*high, *low])),
        _ => 0,
    };
    let _ = be_u32(data, offset);
    let _ = be_u64(data, offset);
    assert_eq!(be_u32(data, usize::MAX), None);
    assert_eq!(be_u64(data, usize::MAX - 3), None);
});
