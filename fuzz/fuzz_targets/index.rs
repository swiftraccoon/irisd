//! Fuzzes the sidecar index codec.
#![no_main]

use iris_mp4::index::{IndexHeader, IndexRecord, RECORD_LEN};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let _ = IndexHeader::decode(data);
    let (chunks, _) = data.as_chunks::<RECORD_LEN>();
    for chunk in chunks {
        let record = IndexRecord::decode(chunk);
        assert_eq!(IndexRecord::decode(&record.encode()), record);
    }
});
