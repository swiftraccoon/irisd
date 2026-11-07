//! Fuzzes the camera-controlled parsers used by iris-tools itself.
#![no_main]

use iris_tools::probe_parser::{camera_utc, rtp_payload_bounds, rtsp_response_bounds};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let _ = rtp_payload_bounds(data);
    let _ = rtsp_response_bounds(data);
    if let Ok(text) = std::str::from_utf8(data) {
        let _ = camera_utc(text);
    }
});
