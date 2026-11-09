//! Fuzzes the bearer middleware's pre-authentication parsers: the `token`
//! query parameter and the encoded capability, which every request reaches
//! before any credential is checked.
#![no_main]

use std::fmt::Write as _;

use libfuzzer_sys::fuzz_target;

#[path = "../../crates/iris-api/src/auth/token.rs"]
mod token;

fuzz_target!(|data: &[u8]| {
    let Ok(text) = std::str::from_utf8(data) else {
        return;
    };
    if let Some(decoded) = token::query_token(Some(text)) {
        assert!(!decoded.trim().is_empty(), "a blank token was accepted");
        assert!(
            decoded.len() <= token::MAX_ENCODED_CAPABILITY_BYTES * 3,
            "a decoded token outgrew its encoding"
        );
    }
    assert!(token::query_token(None).is_none());
    if let Some((expires_at, tag)) = token::parse_capability(text) {
        let hex = tag.iter().fold(String::new(), |mut hex, byte| {
            let _ = write!(hex, "{byte:02x}");
            hex
        });
        let canonical = format!("{}.{expires_at}.{hex}", token::CAPABILITY_VERSION);
        assert_eq!(
            token::parse_capability(&canonical),
            Some((expires_at, tag)),
            "a parsed capability round-trips through its canonical form"
        );
    }
});
