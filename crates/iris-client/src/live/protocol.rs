//! The framing of the live feed's messages, and the sink a viewer pushes
//! them into.

use std::fmt;
use std::num::TryFromIntError;

use iris_mp4::decoder_config::DecoderConfigError;
use thiserror::Error;

/// The type byte of a message that holds an init segment (the API's
/// `MSG_INIT`, `crates/iris-api/src/live.rs`).
pub const MSG_INIT: u8 = 0x01;

/// The type byte of a message that holds a fragment of one frame (the API's
/// `MSG_FRAGMENT`).
pub const MSG_FRAGMENT: u8 = 0x02;

/// The largest payload of a live message, 16 MiB: a viewer's sink queues at
/// most this much, so one payload at the ceiling fits a sink that holds
/// nothing.
pub const LIVE_PAYLOAD_BYTES: usize = 16 << 20;

/// The live protocol's message ceiling: the type byte and a payload of at
/// most [`LIVE_PAYLOAD_BYTES`], 16 MiB + 1 bytes. The daemon honours the
/// same ceiling (the API's `MAX_LIVE_MESSAGE_BYTES`). It skips a fragment
/// above it that is not a sync frame, and closes the socket with code 1009
/// (Message Too Big) when a sync frame exceeds it. A viewer's transport
/// refuses a longer message, and either ends the session as
/// [`SessionEnd::Oversized`](crate::live::SessionEnd::Oversized). Raising it
/// raises the memory every feed may hold.
pub const LIVE_MESSAGE_BYTES: usize = LIVE_PAYLOAD_BYTES + 1;

/// One message of the live feed with its type byte read: the payload after
/// it, which a sink pushes into its decoder's input as it stands. Its
/// variants can grow with the protocol, so a match outside this crate keeps
/// an arm for a message it does not know. Its `Debug` output shows the kind
/// of message and the length of its payload, never the payload's bytes. The
/// example on [`LiveSink`] pushes two.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum LiveMessage<'message> {
    /// An init segment, after [`MSG_INIT`]: the first message of a session,
    /// and sent again wherever the stream starts over at a sync frame.
    Init(&'message [u8]),
    /// A fragment holding one frame, after [`MSG_FRAGMENT`]; it starts with
    /// the `prft` box whose media time
    /// [`media_time`](fn@crate::live::media_time) reads.
    Fragment(&'message [u8]),
}

impl fmt::Debug for LiveMessage<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (kind, payload) = match self {
            Self::Init(payload) => ("Init", payload),
            Self::Fragment(payload) => ("Fragment", payload),
        };
        f.debug_struct(kind)
            .field("payload_bytes", &payload.len())
            .finish()
    }
}

impl<'message> TryFrom<&'message [u8]> for LiveMessage<'message> {
    type Error = LiveError;

    /// Reads the type byte of one live message, as [`live_message`] does.
    ///
    /// # Example
    ///
    /// ```
    /// use iris_client::live::{self, LiveMessage};
    ///
    /// let message = LiveMessage::try_from(&[live::MSG_INIT, 7][..])?;
    /// assert_eq!(message, LiveMessage::Init(&[7]));
    /// # Ok::<(), live::LiveError>(())
    /// ```
    fn try_from(message: &'message [u8]) -> Result<Self, LiveError> {
        let Some((&kind, payload)) = message.split_first() else {
            return Err(LiveError::Empty);
        };
        match kind {
            MSG_INIT => Ok(Self::Init(payload)),
            MSG_FRAGMENT => Ok(Self::Fragment(payload)),
            other => Err(LiveError::UnknownType(other)),
        }
    }
}

/// Reads the type byte of one live message and gives the payload after it,
/// an init segment's or a fragment's. A message of the type byte alone has an
/// empty payload, which a sink may refuse. `LiveMessage::try_from` is the
/// same conversion, through [`TryFrom`]. The example on [`LiveSink`] reads
/// two messages with it.
///
/// # Errors
///
/// [`LiveError::Empty`] for a message without a type byte, and
/// [`LiveError::UnknownType`] for a type byte the protocol does not define.
/// Either breaks the fixed protocol, and the feed ends the session.
pub fn live_message(message: &[u8]) -> Result<LiveMessage<'_>, LiveError> {
    LiveMessage::try_from(message)
}

/// Why a live message is refused. [`live_message`] refuses a message whose
/// framing breaks the protocol, [`LiveError::Empty`] and
/// [`LiveError::UnknownType`], whose texts the Linux viewer logs as the
/// session's failure. A [`LiveDecoder`](crate::live::LiveDecoder) refuses
/// those and the rest: an init segment or a fragment it cannot read, and a
/// fragment it cannot use while it holds no configuration or awaits a sync
/// sample. The example on [`LiveDecoder`](crate::live::LiveDecoder) gives
/// one.
#[derive(Clone, Debug, PartialEq, Eq, Error)]
#[non_exhaustive]
pub enum LiveError {
    /// The message holds no type byte. The viewer ends the session.
    #[error("empty live message")]
    Empty,
    /// The type byte is neither [`MSG_INIT`] nor [`MSG_FRAGMENT`]; the text
    /// writes it as two hexadecimal digits after `0x`. The viewer ends the
    /// session.
    #[error("unknown live message type {0:#04x}")]
    UnknownType(u8),
    /// An init segment yields no decoder configuration, for the reason the
    /// value gives in the words of
    /// [`DecoderConfig::parse`](iris_mp4::decoder_config::DecoderConfig::parse).
    /// The refusal is transparent: its text and its source are the reason's.
    /// The viewer ends the session, whose next one opens with an init segment.
    #[error(transparent)]
    Config(#[from] DecoderConfigError),
    /// A fragment does not parse as one of the daemon's
    /// ([`iris_mp4::reader::parse_fragment`]), its sample is empty, or its
    /// sample would end past the message or past `u32::MAX`. The viewer drops
    /// it and reads on, and the decoder awaits a sync sample.
    #[error("malformed live fragment")]
    Fragment,
    /// A fragment's decode time is past the range of
    /// [`MediaTicks`](iris_core::time::MediaTicks), `i64::MAX` ticks; the
    /// source is the conversion's refusal. The viewer drops the fragment and
    /// reads on, and the decoder awaits a sync sample.
    #[error("live fragment decode time is past the range of media time")]
    DecodeTime(#[source] TryFromIntError),
    /// A fragment holds other than one sample; the value is its count. The
    /// viewer drops it and reads on, and the decoder awaits a sync sample.
    #[error("live fragment holds {0} samples, not one")]
    SampleCount(u16),
    /// A fragment arrived while the decoder held no configuration: before
    /// the first init segment, after
    /// [`LiveDecoder::reset`](crate::live::LiveDecoder::reset), or after an
    /// init segment it refused. The viewer ends the session, since no
    /// fragment decodes until the next init segment, which the next session
    /// opens with.
    #[error("live decoder has no configuration")]
    NoConfig,
    /// A sample that is not a sync sample arrived while the decoder awaited
    /// one: after an init segment or a refused fragment, and before the next
    /// sync sample, without which it does not decode. The viewer drops it and
    /// reads on.
    #[error("live decoder awaits a sync sample")]
    AwaitingSync,
}

/// Where a live feed puts what it receives: the input of a view's decoder.
/// A feed calls [`LiveSink::reset`] as each session opens, before its first
/// message, and [`LiveSink::push`] with each message in the order received.
/// A feed may run on a thread other than the one of the view that holds the
/// sink, so a sink is `Send` and `Sync`.
///
/// # Example
///
/// ```
/// # use std::error::Error;
/// use std::sync::Mutex;
///
/// use iris_client::live::{self, LiveMessage, LiveSink, SinkError};
///
/// /// A sink that keeps the byte stream it is given.
/// #[derive(Default)]
/// struct Kept(Mutex<Vec<u8>>);
///
/// impl LiveSink for Kept {
///     fn reset(&self) {
///         self.0.lock().expect("kept bytes").clear();
///     }
///
///     fn push(&self, message: LiveMessage<'_>) -> Result<(), SinkError> {
///         let payload = match message {
///             LiveMessage::Init(init) => init,
///             LiveMessage::Fragment(fragment) => fragment,
///             // The sink refuses a kind of message it does not know, which
///             // the byte stream it keeps may need.
///             _ => {
///                 let unknown = "live message of an unknown kind";
///                 return Err(SinkError::new(unknown));
///             }
///         };
///         if payload.is_empty() {
///             return Err(SinkError::new("live input payload is empty"));
///         }
///         self.0
///             .lock()
///             .expect("kept bytes")
///             .extend_from_slice(payload);
///         Ok(())
///     }
/// }
///
/// let sink = Kept::default();
/// sink.reset();
/// sink.push(live::live_message(&[live::MSG_INIT, 1, 2])?)?;
/// let empty = live::live_message(&[live::MSG_FRAGMENT])?;
/// let Err(refused) = sink.push(empty) else {
///     return Err("an empty payload is refused".into());
/// };
/// assert_eq!(refused.to_string(), "live input payload is empty");
/// assert_eq!(*sink.0.lock().expect("kept bytes"), [1, 2]);
/// # Ok::<(), Box<dyn Error>>(())
/// ```
pub trait LiveSink: Send + Sync {
    /// Prepares for a fresh byte stream, which a session opens with an init
    /// segment, and drops what the session before queued. The example on
    /// [`LiveSink`] implements it.
    fn reset(&self);

    /// Takes one message, whose payload continues the byte stream. The
    /// example on [`LiveSink`] implements it.
    ///
    /// # Errors
    ///
    /// A [`SinkError`] when the sink cannot take the payload whole. The
    /// feed then ends the session, so that the next one starts a byte stream
    /// a decoder can read; dropping part of one would corrupt it.
    fn push(&self, message: LiveMessage<'_>) -> Result<(), SinkError>;
}

/// A sink's refusal of a message. The feed ends its session, and the text is
/// what a viewer logs as the session's failure. The example on [`LiveSink`]
/// refuses a message with one.
#[derive(Clone, Debug, PartialEq, Eq, Error)]
#[error("{reason}")]
pub struct SinkError {
    reason: String,
}

impl SinkError {
    /// A refusal whose text is `reason`. The example on [`LiveSink`] makes
    /// two.
    #[must_use]
    pub fn new(reason: impl Into<String>) -> Self {
        Self {
            reason: reason.into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::error::Error as _;

    use iris_mp4::decoder_config::HvccError;

    use super::*;

    #[test]
    fn the_payload_limit_is_16_mib() {
        assert_eq!(LIVE_PAYLOAD_BYTES, 16 << 20);
    }

    #[test]
    fn the_message_limit_is_the_payload_limit_plus_the_type_byte() {
        assert_eq!(LIVE_MESSAGE_BYTES, LIVE_PAYLOAD_BYTES + 1);
    }

    #[test]
    fn the_type_byte_of_an_init_segment_is_0x01() {
        assert_eq!(MSG_INIT, 0x01);
    }

    #[test]
    fn the_type_byte_of_a_fragment_is_0x02() {
        assert_eq!(MSG_FRAGMENT, 0x02);
    }

    #[test]
    fn an_empty_message_is_refused() {
        assert_eq!(live_message(&[]), Err(LiveError::Empty));
    }

    /// A type byte the protocol does not define, alone and before a
    /// payload.
    #[test]
    fn an_unknown_type_byte_is_refused() {
        for kind in [0x00, 0x03, 0xff] {
            assert_eq!(live_message(&[kind]), Err(LiveError::UnknownType(kind)));
            assert_eq!(
                live_message(&[kind, MSG_INIT, MSG_FRAGMENT]),
                Err(LiveError::UnknownType(kind))
            );
        }
    }

    #[test]
    fn live_message_strips_the_type_byte_of_each_message_kind() {
        assert_eq!(live_message(&[MSG_INIT, 7]), Ok(LiveMessage::Init(&[7])));
        assert_eq!(
            live_message(&[MSG_FRAGMENT, 8, 9]),
            Ok(LiveMessage::Fragment(&[8, 9]))
        );
    }

    #[test]
    fn a_type_byte_alone_is_a_message_with_an_empty_payload() {
        assert_eq!(live_message(&[MSG_INIT]), Ok(LiveMessage::Init(&[])));
        assert_eq!(
            live_message(&[MSG_FRAGMENT]),
            Ok(LiveMessage::Fragment(&[]))
        );
    }

    /// An init segment's message and a fragment's, each by its kind and the
    /// length of its payload.
    #[test]
    fn a_messages_debug_counts_its_payload_bytes_in_place_of_showing_them() {
        assert_eq!(
            format!("{:?}", LiveMessage::Init(&[7, 8])),
            "Init { payload_bytes: 2 }"
        );
        assert_eq!(
            format!("{:?}", LiveMessage::Fragment(&[0xab; 300])),
            "Fragment { payload_bytes: 300 }"
        );
    }

    #[test]
    fn a_message_converts_into_a_live_message_through_try_from() {
        let message = [MSG_FRAGMENT, 8, 9];
        assert_eq!(
            LiveMessage::try_from(message.as_slice()),
            Ok(LiveMessage::Fragment(&[8, 9]))
        );
    }

    #[test]
    fn a_broken_messages_text_is_the_one_the_viewer_logs() {
        assert_eq!(LiveError::Empty.to_string(), "empty live message");
        assert_eq!(
            LiveError::UnknownType(0x03).to_string(),
            "unknown live message type 0x03"
        );
        assert_eq!(
            LiveError::UnknownType(0x00).to_string(),
            "unknown live message type 0x00"
        );
        assert_eq!(
            LiveError::UnknownType(0xff).to_string(),
            "unknown live message type 0xff"
        );
    }

    /// The refusal of a conversion of a number past `i64::MAX`.
    fn past_i64() -> TryFromIntError {
        i64::try_from(u64::MAX).unwrap_err()
    }

    #[test]
    fn each_decoder_refusal_has_its_text() {
        assert_eq!(
            LiveError::NoConfig.to_string(),
            "live decoder has no configuration"
        );
        assert_eq!(
            LiveError::AwaitingSync.to_string(),
            "live decoder awaits a sync sample"
        );
        assert_eq!(
            LiveError::SampleCount(2).to_string(),
            "live fragment holds 2 samples, not one"
        );
        assert_eq!(LiveError::Fragment.to_string(), "malformed live fragment");
        assert_eq!(
            LiveError::DecodeTime(past_i64()).to_string(),
            "live fragment decode time is past the range of media time"
        );
        // A refused init segment has the text of its configuration's refusal.
        assert_eq!(
            LiveError::Config(DecoderConfigError::Hvcc(HvccError::Truncated)).to_string(),
            "reading the HEVC configuration: hvcC is truncated"
        );
        assert_eq!(
            LiveError::Config(DecoderConfigError::NoSampleEntry).to_string(),
            "the init segment has no sample entry"
        );
    }

    /// A refused init segment has the source of its configuration's refusal.
    #[test]
    fn a_refused_init_keeps_its_configurations_source() {
        let refused = LiveError::Config(DecoderConfigError::Hvcc(HvccError::Truncated));
        let source = refused.source().map(ToString::to_string);
        assert_eq!(source.as_deref(), Some("hvcC is truncated"));
    }

    /// A decode time past the range of media time keeps the conversion's
    /// refusal as its source.
    #[test]
    fn a_decode_time_past_the_range_keeps_its_conversions_refusal() {
        let refused = LiveError::DecodeTime(past_i64());
        let source = refused.source().map(ToString::to_string);
        assert_eq!(source, Some(past_i64().to_string()));
    }

    #[test]
    fn a_sink_refusals_text_is_its_reason() {
        let refusal = SinkError::new("live input payload is empty");
        assert_eq!(refusal.to_string(), "live input payload is empty");
    }

    #[test]
    fn a_sink_refusal_has_no_source() {
        assert!(
            SinkError::new("live input payload is empty")
                .source()
                .is_none()
        );
    }
}
