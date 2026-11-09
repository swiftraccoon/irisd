//! Fuzzes the sidecar index codec: the header and record codecs, and a whole
//! file image through `decode_index` with its range checks.
#![no_main]

use iris_mp4::index::{
    IndexHeader, IndexRecord, MAX_PERSISTED_INDEX_RECORDS, RECORD_LEN, decode_index,
};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let _ = IndexHeader::decode(data);
    let (chunks, _) = data.as_chunks::<RECORD_LEN>();
    for chunk in chunks {
        let record = IndexRecord::decode(chunk);
        assert_eq!(IndexRecord::decode(&record.encode()), record);
    }
    let limit = usize::from(data.first().copied().unwrap_or(0) % 4);
    for max_records in [limit, MAX_PERSISTED_INDEX_RECORDS] {
        if let Ok((_, records)) = decode_index(data, max_records) {
            assert!(
                records.len() <= max_records,
                "{} records over {max_records}",
                records.len()
            );
            for pair in records.windows(2) {
                assert_eq!(pair[0].end(), pair[1].offset, "records are contiguous");
            }
        }
    }
});
