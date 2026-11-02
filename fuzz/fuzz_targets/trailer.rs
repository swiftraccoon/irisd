//! Fuzzes the fragment trailer decoder.
#![no_main]

use iris_mp4::fragment::decode_trailer;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let _ = decode_trailer(data);
});
