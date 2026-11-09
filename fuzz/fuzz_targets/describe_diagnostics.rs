//! Fuzzes retina's interpretation of a `DESCRIBE` response and bounds what it
//! reports at the daemon's default log level (`info`) and above: no log
//! record or error over 2 KiB, no control character from the `DESCRIBE`
//! parser, and at most one warning from it per response.
#![no_main]

use std::sync::Once;
use std::sync::atomic::{AtomicUsize, Ordering};

use libfuzzer_sys::fuzz_target;

/// Longest log record or error accepted.
const MAX_DIAGNOSTIC_BYTES: usize = 2048;

/// Warnings from the `DESCRIBE` parser for the current input.
static DESCRIBE_WARNINGS: AtomicUsize = AtomicUsize::new(0);

/// Checks every record retina emits at `info` and above.
struct BoundedLog;

impl log::Log for BoundedLog {
    fn enabled(&self, metadata: &log::Metadata<'_>) -> bool {
        metadata.level() <= log::Level::Info
    }

    fn log(&self, record: &log::Record<'_>) {
        let text = record.args().to_string();
        assert!(
            text.len() <= MAX_DIAGNOSTIC_BYTES,
            "{} bytes from {}: {text:.200}",
            text.len(),
            record.target()
        );
        if record.target() == "retina::client::parse" {
            assert!(!text.bytes().any(|b| b.is_ascii_control()), "{text:?}");
            if record.level() <= log::Level::Warn {
                DESCRIBE_WARNINGS.fetch_add(1, Ordering::Relaxed);
            }
        } else {
            // Hex dumps span lines; nothing else may carry a control.
            assert!(
                !text.bytes().any(|b| b.is_ascii_control() && b != b'\n'),
                "{text:?}"
            );
        }
    }

    fn flush(&self) {}
}

/// Room for the response head under retina's 1 MiB control-message limit, so
/// every input reaches SDP interpretation.
const MAX_BODY_BYTES: usize = 1024 * 1024 - 1024;

fuzz_target!(|data: &[u8]| {
    static INIT: Once = Once::new();
    INIT.call_once(|| {
        log::set_logger(&BoundedLog).expect("the only logger");
        log::set_max_level(log::LevelFilter::Info);
    });
    if data.len() > MAX_BODY_BYTES {
        return;
    }
    DESCRIBE_WARNINGS.store(0, Ordering::Relaxed);
    let mut response = format!(
        "RTSP/1.0 200 OK\r\nCSeq: 1\r\nContent-Type: application/sdp\r\nContent-Length: {}\r\n\r\n",
        data.len()
    )
    .into_bytes();
    response.extend_from_slice(data);
    if let Err(error) =
        retina::testutil::parse_describe_response("rtsp://192.0.2.1:554/stream", &response)
    {
        assert!(
            error.len() <= MAX_DIAGNOSTIC_BYTES,
            "{} bytes: {error:.200}",
            error.len()
        );
        assert!(!error.bytes().any(|b| b.is_ascii_control()), "{error:?}");
    }
    let warnings = DESCRIBE_WARNINGS.load(Ordering::Relaxed);
    assert!(warnings <= 1, "{warnings} warnings for one DESCRIBE");
});
