//! Fuzzes the recording scanner: arbitrary bytes must never panic.
#![no_main]

use std::io::Cursor;

use iris_mp4::reader::{ScanOptions, scan};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let strict = ScanOptions {
        verify_crc: true,
        collect_samples: true,
    };
    let _ = scan(Cursor::new(data), strict);
    let _ = scan(Cursor::new(data), ScanOptions::default());
});
