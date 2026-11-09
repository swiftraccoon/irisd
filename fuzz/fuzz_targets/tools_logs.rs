//! Fuzzes the report tools' log parsers (`crates/iris-tools/src/logs.rs` and
//! the RFC 3339 parser in `time.rs`, included by path) over daemon logs that
//! carry camera-controlled text: lines stay within the limit, removing escape
//! strings leaves no control or bidirectional formatting character, text
//! without an escape is only escaped, and splitting, statistics
//! classification, and pair parsing never panic.
#![no_main]

use libfuzzer_sys::fuzz_target;

#[path = "../../crates/iris-tools/src/logs.rs"]
#[allow(
    dead_code,
    reason = "the report tools use helpers this target does not"
)]
mod logs;

#[path = "../../crates/iris-tools/src/time.rs"]
#[allow(
    dead_code,
    reason = "the report tools use helpers this target does not"
)]
mod time;

/// The characters a terminal could act on: controls and bidirectional
/// formatting, listed independently of `logs.rs`.
fn acts_on_a_terminal(c: char) -> bool {
    c.is_control()
        || matches!(
            c,
            '\u{061c}' | '\u{200e}' | '\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}'
        )
}

fuzz_target!(|data: &[u8]| {
    let mut reader = logs::LineReader::new(data);
    while let Ok(Some(line)) = reader.next_line() {
        assert!(line.bytes.len() <= logs::MAX_LINE_BYTES);
        let Ok(text) = std::str::from_utf8(line.bytes) else {
            continue;
        };
        let stripped = logs::strip_ansi(text);
        assert!(!stripped.chars().any(acts_on_a_terminal), "{stripped:?}");
        if !text.contains('\x1b') {
            assert_eq!(stripped, logs::terminal_safe(text));
        }
        if let Some(split) = logs::split(&stripped) {
            let _ = time::parse_seconds(split.timestamp);
            let _ = logs::statistics(&split);
            let _ = logs::kv(split.rest);
            let _ = logs::root_identity(split.rest);
        }
    }
});
