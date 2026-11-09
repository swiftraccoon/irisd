//! Fuzzes the raw recording route's `Range` header parser: any header value
//! and extent give a clamped range, a refusal, or the whole extent, never a
//! panic; a request short enough for the bounded class never serves more.
#![no_main]

use libfuzzer_sys::fuzz_target;

#[path = "../../crates/iris-api/src/segments/range.rs"]
mod range;

/// Longest raw range of the bounded media class (`segments::BOUNDED_RANGE_BYTES`).
const BOUNDED_RANGE_BYTES: u64 = 1024 * 1024;

fuzz_target!(|data: &[u8]| {
    let Some((extent, header)) = data.split_at_checked(8) else {
        return;
    };
    let extent = u64::from_le_bytes(extent.try_into().expect("eight bytes"));
    let Ok(header) = std::str::from_utf8(header) else {
        return;
    };
    let requested = range::requested_len(Some(header));
    if let Ok(range::RangeRequest::Bytes(first, last)) = range::parse_range(Some(header), extent) {
        assert!(first <= last && last < extent, "{header:?} {extent}");
        if requested.is_some_and(|len| len <= BOUNDED_RANGE_BYTES) {
            assert!(last - first < BOUNDED_RANGE_BYTES, "{header:?}");
        }
    }
    assert_eq!(
        range::parse_range(None, extent),
        Ok(range::RangeRequest::Whole)
    );
});
