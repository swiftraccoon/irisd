//! Fuzzes the ONVIF timestamp parser and topic mapping.
#![no_main]

use iris_events::onvif::{kind_for_topic, parse_utc};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let text = String::from_utf8_lossy(data);
    let _ = parse_utc(&text);
    let _ = kind_for_topic(&text);
});
