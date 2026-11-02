//! Fuzzes ISO BMFF box-header parsing.
#![no_main]

use iris_mp4::boxes::BoxHeader;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Some(header) = BoxHeader::parse(data) {
        let _ = header.payload_len();
    }
});
