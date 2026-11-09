//! Fuzzes the replay server's RTSP request parser and the header parsers the
//! server applies to a request: mount-name extraction from the URL, the
//! `Transport` channel negotiation, and the `Session` id. A parsed request
//! never consumes more bytes than it was given, and a negotiated channel pair
//! is distinct.
#![no_main]

use iris_replay::rtsp::{self, Parsed};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    match rtsp::parse(data) {
        Parsed::Request(request, consumed) => {
            assert!(consumed <= data.len());
            let _ = rtsp::mount_name(&request.url);
            if let Some(channels) = request
                .header("transport")
                .and_then(rtsp::interleaved_transport)
            {
                assert_ne!(channels.rtp, channels.rtcp);
            }
            if let Some(session) = request.header("session") {
                assert!(rtsp::session_id(session).len() <= session.len());
            }
        }
        Parsed::Interleaved(len) => assert!(len <= data.len()),
        Parsed::Incomplete | Parsed::Invalid => {}
    }
    let text = String::from_utf8_lossy(data);
    let _ = rtsp::mount_name(&text);
    let _ = rtsp::interleaved_transport(&text);
});
