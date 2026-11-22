//! Fuzzes the framing of the live feed's messages as a viewer reads it
//! (`iris_client::live`): any bytes give the payload of an init segment or of
//! a fragment, or a refusal, never a panic. A payload is the message after
//! its type byte, and a refusal is an empty message or names the type byte
//! the protocol does not define.
#![no_main]

use iris_client::live::{self, LiveError, LiveMessage};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    match live::live_message(data) {
        Ok(LiveMessage::Init(payload)) => {
            assert_eq!(
                data.first(),
                Some(&live::MSG_INIT),
                "an init without its type"
            );
            assert_eq!(
                payload,
                &data[1..],
                "an init's payload follows its type byte"
            );
        }
        Ok(LiveMessage::Fragment(payload)) => {
            assert_eq!(
                data.first(),
                Some(&live::MSG_FRAGMENT),
                "a fragment without its type"
            );
            assert_eq!(
                payload,
                &data[1..],
                "a fragment's payload follows its type byte"
            );
        }
        // The framing gives no other kind of message; one it gave would fail
        // the target.
        Ok(other) => panic!("a message the framing does not define: {other:?}"),
        Err(LiveError::Empty) => assert!(data.is_empty(), "{} bytes", data.len()),
        Err(LiveError::UnknownType(kind)) => {
            assert_eq!(data.first(), Some(&kind), "the refusal names another byte");
            assert!(
                kind != live::MSG_INIT && kind != live::MSG_FRAGMENT,
                "{kind:#04x}"
            );
        }
        Err(
            other @ (LiveError::Config(_)
            | LiveError::Fragment
            | LiveError::DecodeTime(_)
            | LiveError::SampleCount(_)
            | LiveError::NoConfig
            | LiveError::AwaitingSync),
        ) => panic!("the framing refuses only an empty message or a type: {other}"),
        Err(other) => panic!("a refusal the framing does not define: {other}"),
    }
});
