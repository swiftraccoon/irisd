//! Fuzzes the live decoder of `iris-client`
//! (`iris_client::live::LiveDecoder`): a run of messages cut from the input
//! feeds one decoder, which gives a configuration, a sample, or a refusal for
//! each, never a panic. A sample holds at least one byte and lies inside its
//! message, past the type byte. So a viewer can hand that range of its buffer
//! to its decoder. An empty sample is refused as a malformed fragment. A
//! sample comes only while the decoder holds a configuration, and the first
//! since each configuration or refused fragment is a sync sample. Every
//! refusal is the one that the documented rules of the decoder give in its
//! state, which `Held` models.
//!
//! Each message is a big-endian `u16` header and then its bytes. The low 15
//! bits of the header are the message's length, cut short where the input
//! ends. The top bit resets the decoder before the message.
#![no_main]

use iris_client::live::{self, LiveDecoder, LiveError, LiveItem};
use iris_mp4::decoder_config::DecoderConfig;
use libfuzzer_sys::fuzz_target;

/// The bit of a message's header that resets the decoder before it.
const RESET: u16 = 0x8000;

/// What the decoder holds between messages, by its documented rules.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Held {
    /// No configuration.
    Nothing,
    /// A configuration, and no sync sample since it or since a refused
    /// fragment.
    AwaitingSync,
    /// A configuration and a sync sample since it.
    Decoding,
}

/// Decodes `message` with a decoder that holds `before`, checks the answer
/// against the rules, and returns what the decoder holds after it.
fn check(decoder: &mut LiveDecoder, message: &[u8], before: Held) -> Held {
    let kind = message.first().copied();
    match decoder.decode(message) {
        Ok(LiveItem::Config(config)) => {
            assert_eq!(
                kind,
                Some(live::MSG_INIT),
                "a configuration from a {kind:02x?}"
            );
            if let DecoderConfig::Hevc(hevc) = &config {
                assert!(hevc.parameter_sets().len() >= 3, "{config:?}");
            }
            Held::AwaitingSync
        }
        Ok(LiveItem::Sample(sample)) => {
            assert_eq!(
                kind,
                Some(live::MSG_FRAGMENT),
                "a sample from a {kind:02x?}"
            );
            assert_ne!(before, Held::Nothing, "a sample without a configuration");
            assert!(
                sample.is_sync() || before == Held::Decoding,
                "a dependent sample while a sync sample is awaited"
            );
            assert!(sample.offset() >= 1, "the sample holds the type byte");
            assert!(sample.len() > 0, "an empty sample at {}", sample.offset());
            let end = u64::from(sample.offset()) + u64::from(sample.len());
            assert!(
                end <= message.len() as u64,
                "a sample ending at {end} of a message of {} bytes",
                message.len()
            );
            assert!(sample.decode_time().get() >= 0, "{sample:?}");
            assert!(sample.duration().get() >= 0, "{sample:?}");
            Held::Decoding
        }
        Err(LiveError::Empty) => {
            assert_eq!(kind, None, "a message of {kind:02x?} refused as empty");
            before
        }
        Err(LiveError::UnknownType(byte)) => {
            assert_eq!(kind, Some(byte), "the refusal names another byte");
            before
        }
        Err(LiveError::Config(_)) => {
            assert_eq!(
                kind,
                Some(live::MSG_INIT),
                "a {kind:02x?} refused as an init"
            );
            Held::Nothing
        }
        Err(LiveError::NoConfig) => {
            assert_eq!(
                (kind, before),
                (Some(live::MSG_FRAGMENT), Held::Nothing),
                "a fragment refused for a configuration the decoder holds"
            );
            before
        }
        Err(LiveError::Fragment | LiveError::DecodeTime(_) | LiveError::SampleCount(_)) => {
            assert_eq!(
                kind,
                Some(live::MSG_FRAGMENT),
                "a {kind:02x?} refused as a fragment"
            );
            assert_ne!(
                before,
                Held::Nothing,
                "a fragment read without a configuration"
            );
            Held::AwaitingSync
        }
        Err(LiveError::AwaitingSync) => {
            assert_eq!(
                (kind, before),
                (Some(live::MSG_FRAGMENT), Held::AwaitingSync),
                "a sample refused for a sync sample the decoder does not await"
            );
            before
        }
        Err(other) => panic!("a refusal the decoder does not give: {other}"),
    }
}

fuzz_target!(|data: &[u8]| {
    let mut decoder = LiveDecoder::new();
    let mut held = Held::Nothing;
    let mut rest = data;
    while let [high, low, after @ ..] = rest {
        let header = u16::from_be_bytes([*high, *low]);
        let (message, next) = after.split_at(usize::from(header & !RESET).min(after.len()));
        rest = next;
        if header & RESET != 0 {
            decoder.reset();
            held = Held::Nothing;
        }
        held = check(&mut decoder, message, held);
    }
});
