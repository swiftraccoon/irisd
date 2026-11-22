//! The live feed as a viewer reads it, without a transport. The module reads
//! the type byte of each of the daemon's WebSocket messages, and defines the
//! sink a viewer pushes them into. It reads each message into a decoder
//! configuration or one sample, decides when the feed connects again, and
//! bounds the bytes its sinks share.
//!
//! Each binary message of `/api/streams/{camera}/{kind}/live` is a type byte
//! and a payload: [`MSG_INIT`] before an init segment, [`MSG_FRAGMENT`] before
//! a fragment holding one frame (`docs/research/07-architecture.md` §5).
//! [`live_message`] reads the type byte, and a viewer hands the
//! [`LiveMessage`] to its [`LiveSink`]. A message without a type byte or with
//! another is a [`LiveError`] of the framing, [`LiveError::Empty`] or
//! [`LiveError::UnknownType`], and a viewer ends the session on it, as on a
//! text message or a [`SinkError`].
//! A fragment starts with a `prft` box, whose media time
//! [`media_time`](fn@media_time) reads for
//! [`SoundClock`](crate::sound_clock::SoundClock) to place the feed by.
//! No message exceeds [`LIVE_MESSAGE_BYTES`], the type byte and a payload of
//! at most [`LIVE_PAYLOAD_BYTES`]. A viewer's transport enforces the limit.
//! The daemon honours it, and closes the socket with code 1009 (Message Too
//! Big) when a sync frame exceeds it.
//!
//! A viewer that decodes the feed itself hands each message to a
//! [`LiveDecoder`] instead of a sink. `docs/research/15-apple-clients.md` §4
//! designs the planned iOS and macOS viewer that way. An init segment gives
//! the [`LiveItem::Config`] its decoder is configured with. A fragment gives
//! the [`LiveItem::Sample`] it holds: a [`LiveSample`] placed by its offset
//! and length in the message. The viewer wraps that range of the buffer it
//! holds for its decoder. Until a sync sample comes after a configuration, the
//! decoder refuses the samples before it with [`LiveError::AwaitingSync`],
//! which the viewer drops. The viewer drops a fragment the decoder cannot read
//! as well, and the decoder awaits the next sync sample. On an init segment
//! the decoder cannot read, or on a fragment while it holds no configuration,
//! the viewer ends the session, as on a refusal of the framing.
//!
//! When a session ends, a viewer tells the feed's [`Reconnect`] how
//! ([`SessionEnd`]) and does what it answers ([`Next`]). It connects again
//! after 2 s, or after a pause that doubles from 2 s to 60 s while the
//! stream's frames exceed the ceiling. It does not connect again to an audio
//! feed the daemon answers with 404. A [`FeedEvent`] tells the view what to
//! show. A [`LiveBudget`] bounds the payload bytes alive in a viewer at once,
//! across all its sinks, and each [`Reservation`] returns its bytes when
//! dropped.
//!
//! # Example
//!
//! ```
//! use std::time::Duration;
//!
//! use iris_client::live::{self, LiveMessage, Next, Reconnect, SessionEnd};
//! use iris_core::id::StreamKind;
//!
//! let message = live::live_message(&[live::MSG_FRAGMENT, 7, 8])?;
//! assert_eq!(message, LiveMessage::Fragment(&[7, 8]));
//!
//! let mut reconnect = Reconnect::new(StreamKind::Main);
//! let pause = |secs| Next::Retry(Duration::from_secs(secs));
//! assert_eq!(reconnect.after(SessionEnd::Oversized), pause(2));
//! assert_eq!(reconnect.after(SessionEnd::Oversized), pause(4));
//! // The next session delivers video, and the back-off ends with it.
//! reconnect.delivered();
//! assert_eq!(reconnect.after(SessionEnd::Oversized), pause(2));
//!
//! let mut sound = Reconnect::new(StreamKind::Audio);
//! assert_eq!(sound.after(SessionEnd::Refused { status: 404 }), Next::Stop);
//! # Ok::<(), live::LiveError>(())
//! ```

mod budget;
mod decoder;
mod media_time;
mod protocol;
mod reconnect;

pub use budget::{LiveBudget, Reservation};
pub use decoder::{LiveDecoder, LiveItem, LiveSample};
pub use media_time::media_time;
pub use protocol::{
    LIVE_MESSAGE_BYTES, LIVE_PAYLOAD_BYTES, LiveError, LiveMessage, LiveSink, MSG_FRAGMENT,
    MSG_INIT, SinkError, live_message,
};
pub use reconnect::{FeedEvent, Next, Reconnect, SessionEnd};
