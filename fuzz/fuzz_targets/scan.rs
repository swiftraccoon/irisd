//! Fuzzes the recording scanner: arbitrary bytes must never panic, with or
//! without CRC verification, and under a small fragment limit so the
//! limit-reached path runs too.
#![no_main]

use std::io::Cursor;

use iris_mp4::reader::{ScanOptions, scan, scan_with_limit};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let strict = ScanOptions {
        verify_crc: true,
        collect_samples: true,
    };
    let _ = scan(Cursor::new(data), strict);
    let _ = scan(Cursor::new(data), ScanOptions::default());
    let limit = usize::from(data.first().copied().unwrap_or(0) % 4);
    let _ = scan_with_limit(Cursor::new(data), ScanOptions::default(), limit);
});
