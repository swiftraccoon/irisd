//! Fuzzes the replay server's RTSP request parser and mount-name extraction.
#![no_main]

use iris_replay::rtsp::{mount_name, parse};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let _ = parse(data);
    let _ = mount_name(&String::from_utf8_lossy(data));
});
