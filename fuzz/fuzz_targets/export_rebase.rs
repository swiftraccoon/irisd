//! Fuzzes the export rewrites: parsing a fragment held in memory and rebasing
//! it in place, then splicing an edit list into arbitrary bytes taken as an
//! init segment. Both read files the daemon wrote, but a corrupt file must
//! yield an error, never a panic.
#![no_main]

use iris_mp4::export::{rebase_fragment, with_edit_list};
use iris_mp4::reader::parse_fragment;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let _ = parse_fragment(data);
    let mut fragment = data.to_vec();
    if rebase_fragment(&mut fragment, 7, 90_000).is_ok() {
        // A rewritten fragment parses with the new values.
        let info = parse_fragment(&fragment).expect("rebased fragment parses");
        assert_eq!(info.sequence, 7);
        assert_eq!(info.decode_time, 90_000);
    }
    if let Ok(init) = with_edit_list(data, 45_000, 5_500) {
        assert_eq!(init.len(), data.len() + 44);
    }
});
