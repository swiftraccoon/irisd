//! Fuzzes the vendor event-part parser and normaliser.
#![no_main]

use iris_core::id::CameraId;
use iris_core::time::WallUs;
use iris_events::dahua::{kind_for_code, normalize, parse_part, state_for_action};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let text = String::from_utf8_lossy(data);
    let _ = kind_for_code(&text);
    let _ = state_for_action(&text);
    if let Some(raw) = parse_part(&text) {
        let camera: CameraId = "cam1".parse().expect("valid id");
        let _ = normalize(&camera, &raw, WallUs::new(1_700_000_000_000_000));
    }
});
