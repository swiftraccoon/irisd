//! Fuzzes the mapping of WS-Discovery answers to discovered cameras: the
//! endpoint reference, the scopes, and the device service URLs as a peer could
//! send them.
#![no_main]

use iris_events::DiscoveredCamera;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let text = String::from_utf8_lossy(data);
    let mut sections = text.split("\n\n");
    let endpoint = sections.next().unwrap_or_default();
    let scopes: Vec<String> = sections
        .next()
        .unwrap_or_default()
        .split_whitespace()
        .map(str::to_owned)
        .collect();
    let xaddrs: Vec<String> = sections
        .next()
        .unwrap_or_default()
        .split_whitespace()
        .map(str::to_owned)
        .collect();
    if let Some(camera) = DiscoveredCamera::from_parts(endpoint, &scopes, &xaddrs) {
        assert!(!camera.host.is_empty(), "{camera:?}");
        let texts = [
            Some(&camera.endpoint),
            camera.name.as_ref(),
            camera.hardware.as_ref(),
            camera.location.as_ref(),
        ];
        for text in texts.into_iter().flatten() {
            assert!(!text.chars().any(char::is_control), "{text:?}");
        }
    }
});
