//! Fuzzes HTTP digest challenge parsing and the authorization header built from it.
#![no_main]

use iris_events::digest::{DigestAuth, parse_challenge};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let text = String::from_utf8_lossy(data);
    if let Some(challenge) = parse_challenge(&text) {
        let mut auth = DigestAuth::new("user", "password");
        auth.set_challenge(challenge);
        let _ = auth.authorization_with("GET", "/cgi-bin/eventManager.cgi", "0a1b2c3d");
    }
});
