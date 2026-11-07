//! Fuzzes the ONVIF timestamp parser and topic mapping.
#![no_main]

use iris_core::event::EventKind;
use iris_events::onvif::{kind_for_topic, parse_utc};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let text = String::from_utf8_lossy(data);
    if let Some(parsed) = parse_utc(&text)
        && parsed.get() >= 0
    {
        let civil = parsed.to_civil();
        let canonical = format!(
            "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:06}Z",
            civil.year,
            civil.month,
            civil.day,
            civil.hour,
            civil.minute,
            civil.second,
            civil.micros
        );
        assert_eq!(parse_utc(&canonical), Some(parsed));
    }
    let kind = kind_for_topic(&text);
    if let EventKind::Other(raw) = kind {
        assert_eq!(raw, text);
    }
});
