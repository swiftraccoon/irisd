//! Fuzzes the ONVIF notification normaliser: any topic, property operation,
//! time, and items must give a bounded event or none.
#![no_main]

use std::collections::HashMap;

use iris_core::event::{EventKind, MAX_EVENT_DATA_LEN, MAX_EVENT_KIND_LEN};
use iris_core::id::CameraId;
use iris_core::time::WallUs;
use iris_events::onvif::normalize_notification;
use libfuzzer_sys::fuzz_target;
use oxvif::{ElementItem, NotificationMessage};

fuzz_target!(|data: &[u8]| {
    let text = String::from_utf8_lossy(data);
    let mut sections = text.split("\n\n");
    let topic = sections.next().unwrap_or_default().to_owned();
    let property_operation = sections.next().unwrap_or_default().to_owned();
    let utc_time = sections.next().unwrap_or_default().to_owned();
    let pairs = |section: Option<&str>| -> HashMap<String, String> {
        section
            .unwrap_or_default()
            .lines()
            .filter_map(|line| line.split_once('='))
            .map(|(key, value)| (key.to_owned(), value.to_owned()))
            .collect()
    };
    let source = pairs(sections.next());
    let data = pairs(sections.next());
    let elements = |section: Option<&str>| -> Vec<ElementItem> {
        section
            .unwrap_or_default()
            .lines()
            .map(|line| ElementItem {
                name: line.chars().take(16).collect(),
                content: line.to_owned(),
                truncated: line.len() % 2 == 0,
            })
            .collect()
    };
    let message = NotificationMessage {
        topic,
        utc_time,
        property_operation,
        source,
        source_elements: elements(sections.next()),
        data,
        data_elements: elements(sections.next()),
    };
    let camera: CameraId = "cam1".parse().expect("valid id");
    if let Some(event) =
        normalize_notification(&camera, &message, WallUs::new(1_700_000_000_000_000))
    {
        if let EventKind::Other(code) = &event.kind {
            assert!(code.len() <= MAX_EVENT_KIND_LEN);
        }
        let data = event.data.expect("a notification always carries a payload");
        assert!(data.len() <= MAX_EVENT_DATA_LEN);
        assert!(serde_json::from_str::<serde_json::Value>(&data).is_ok());
    }
});
