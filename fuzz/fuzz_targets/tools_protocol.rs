//! Fuzzes the parsers the camera probes apply to camera-controlled bytes
//! (`crates/iris-tools/src/probe_parser.rs`, included by path with the
//! `time.rs` it uses): the interleaved stream, whose items stay inside the
//! buffer and always consume bytes, RTSP response framing, RTP and RTCP
//! packets, H.265 payload units, timestamp unwrapping, and the text parsers
//! for digest challenges, SDP controls, transports, headers, and the ONVIF
//! clock. None may panic.
#![no_main]

use libfuzzer_sys::fuzz_target;

#[path = "../../crates/iris-tools/src/time.rs"]
#[allow(dead_code, reason = "the parsers use only part of the time helpers")]
mod time;

#[path = "../../crates/iris-tools/src/probe_parser.rs"]
mod probe_parser;

use probe_parser::{Interleaved, TimestampUnwrapper};

/// Demultiplexes `data` as the probe's receive loop does, checking that every
/// item lies inside what remains and moves past at least one byte.
fn demultiplex_all(data: &[u8]) {
    let mut rest = data;
    while let Ok(item) = probe_parser::demultiplex(rest) {
        match &item {
            Interleaved::Incomplete => return,
            Interleaved::Data { payload, .. } => {
                assert_eq!(payload.start, 4);
                assert!(payload.end <= rest.len());
            }
            Interleaved::Response(frame) => {
                assert_eq!(frame.body.start, frame.head_end + 4);
                assert!(frame.body.end <= rest.len());
            }
            Interleaved::Skip(_) => {}
        }
        let consumed = item.consumed();
        assert!(consumed > 0 && consumed <= rest.len());
        rest = &rest[consumed..];
    }
}

fuzz_target!(|data: &[u8]| {
    demultiplex_all(data);
    if let Ok(Some(frame)) = probe_parser::rtsp_response(data) {
        assert!(frame.body.end <= data.len());
        assert!(std::str::from_utf8(&data[..frame.head_end]).is_ok());
    }
    if let Ok(packet) = probe_parser::rtp_packet(data) {
        assert!(packet.payload.start <= packet.payload.end);
        assert!(packet.payload.end <= data.len());
    }
    let _ = probe_parser::h265_payload(data);
    if let Ok(packets) = probe_parser::rtcp_packets(data) {
        assert!(packets.len() <= data.len() / 4);
    }
    let mut timeline = TimestampUnwrapper::default();
    let (words, _) = data.as_chunks::<4>();
    for word in words {
        let _ = timeline.extend(u32::from_be_bytes(*word));
    }
    if let Ok(text) = std::str::from_utf8(data) {
        let _ = probe_parser::camera_utc(text);
        let _ = probe_parser::digest_challenge(text);
        let _ = probe_parser::sdp_controls(text);
        let _ = probe_parser::interleaved_channels(text);
        let _ = probe_parser::response_status(text);
        let _ = probe_parser::response_cseq(text);
        let _ = probe_parser::header_value(text, "Content-Base");
        let _ = probe_parser::session_id(text);
    }
});
