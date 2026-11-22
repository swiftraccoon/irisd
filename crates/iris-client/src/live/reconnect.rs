//! When a live feed connects again, and what it tells its view.

use std::time::Duration;

use iris_core::id::StreamKind;

/// The pause before connecting again after a session ends or fails.
const RECONNECT_DELAY: Duration = Duration::from_secs(2);

/// The longest pause between attempts while the stream's frames exceed the
/// message ceiling.
const MAX_OVERSIZED_DELAY: Duration = Duration::from_secs(60);

/// The status with which the daemon answers the handshake of a stream that has
/// no live feed.
const NOT_FOUND: u16 = 404;

/// How a live session ended, as a feed hands it to [`Reconnect::after`].
/// Its variants are a closed set, which every consumer handles whole. The
/// example on [`Reconnect`] hands three to [`Reconnect::after`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SessionEnd {
    /// The daemon closed the socket, or the connection ended.
    Closed,
    /// A message exceeded
    /// [`LIVE_MESSAGE_BYTES`](crate::live::LIVE_MESSAGE_BYTES): the daemon
    /// closed the socket with code 1009 (Message Too Big), or the feed's
    /// limit refused the message.
    Oversized,
    /// The session failed: the connection or its handshake failed, a message
    /// broke the protocol, or the sink refused one.
    Failed,
    /// The daemon answered the handshake with `status` in place of an
    /// upgrade.
    Refused {
        /// The HTTP status of the answer.
        status: u16,
    },
}

/// What a feed does once a session has ended. Its variants are a closed set,
/// which every consumer handles whole. The example on [`Reconnect`] matches
/// both.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Next {
    /// Connect again after the pause.
    Retry(Duration),
    /// Connect no more: the stream has no live feed.
    Stop,
}

/// What a live feed tells its view. Its variants are a closed set, which
/// every consumer handles whole. The example on [`Reconnect`] sends each.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum FeedEvent {
    /// The feed's session ended; the view restarts its decoder, so that the
    /// next session's init segment meets a clean one.
    Restart,
    /// A live frame exceeded the protocol's message ceiling; the feed retries
    /// after `retry_in`, doubling the pause while the condition lasts.
    Oversized {
        /// The pause before the next attempt.
        retry_in: Duration,
    },
    /// The feed delivers video again after an oversized frame.
    Live,
}

/// The policy that decides when a live feed connects again. A feed connects
/// 2 s after a session ends. While the stream's frames exceed the message
/// ceiling, it waits a pause that starts at 2 s and doubles to 60 s. It never
/// connects again to an audio feed the daemon answers with 404, as the daemon
/// does for a camera that records no audio.
///
/// A feed keeps one for its life, and around each session:
///
/// - before it, reads [`Reconnect::is_backing_off`]: while the feed backs
///   off, the session announces [`FeedEvent::Live`] once its sink has taken
///   the session's first fragment;
/// - after it, calls [`Reconnect::delivered`] when the sink took a fragment,
///   and then [`Reconnect::after`] with how the session ended;
/// - on [`Next::Retry`], sends [`FeedEvent::Oversized`] with the pause after
///   a session that ended [`SessionEnd::Oversized`], then
///   [`FeedEvent::Restart`], and connects again once the pause is over; on
///   [`Next::Stop`], ends without an event.
///
/// # Example
///
/// ```
/// use std::time::Duration;
///
/// use iris_client::live::{FeedEvent, Next, Reconnect, SessionEnd};
/// use iris_core::id::StreamKind;
///
/// /// The events a feed sends its view after a session that ended as `end`,
/// /// or none when it connects no more.
/// fn sent(policy: &mut Reconnect, end: SessionEnd) -> Option<Vec<FeedEvent>> {
///     match policy.after(end) {
///         Next::Retry(retry_in) if end == SessionEnd::Oversized => {
///             let oversized = FeedEvent::Oversized { retry_in };
///             Some(vec![oversized, FeedEvent::Restart])
///         }
///         Next::Retry(_) => Some(vec![FeedEvent::Restart]),
///         Next::Stop => None,
///     }
/// }
///
/// let mut reconnect = Reconnect::new(StreamKind::Main);
/// let retry_in = Duration::from_secs(2);
/// let oversized = vec![FeedEvent::Oversized { retry_in }, FeedEvent::Restart];
/// assert_eq!(sent(&mut reconnect, SessionEnd::Oversized), Some(oversized));
/// // The next session announces video's return when its sink takes a
/// // fragment, and that ends the back-off.
/// let announce = reconnect.is_backing_off().then_some(FeedEvent::Live);
/// assert_eq!(announce, Some(FeedEvent::Live));
/// reconnect.delivered();
/// let closed = sent(&mut reconnect, SessionEnd::Closed);
/// assert_eq!(closed, Some(vec![FeedEvent::Restart]));
///
/// // The daemon answers 404 for the sound of a camera that records none.
/// let mut sound = Reconnect::new(StreamKind::Audio);
/// let refused = SessionEnd::Refused { status: 404 };
/// assert_eq!(sent(&mut sound, refused), None);
/// ```
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Reconnect {
    kind: StreamKind,
    /// The last pause after an oversized frame, while the feed backs off.
    oversized: Option<Duration>,
}

impl Reconnect {
    /// The policy of a feed of `kind`, not backing off. The example on
    /// [`Reconnect`] makes two.
    #[must_use]
    pub fn new(kind: StreamKind) -> Self {
        Self {
            kind,
            oversized: None,
        }
    }

    /// Ends the back-off from an oversized frame. A feed calls this when a
    /// session's sink took a fragment; a fragment the sink refused does not
    /// count. The feed calls this before [`Reconnect::after`] for the same
    /// session, whose ending then starts any back-off afresh. The example on
    /// [`Reconnect`] calls it.
    pub fn delivered(&mut self) {
        self.oversized = None;
    }

    /// Whether the feed backs off from an oversized frame: a session ended
    /// [`SessionEnd::Oversized`], and none has delivered video since. The
    /// example on [`Reconnect`] reads it.
    #[must_use]
    pub fn is_backing_off(&self) -> bool {
        self.oversized.is_some()
    }

    /// What the feed does after a session that ended as `end`. After
    /// [`SessionEnd::Oversized`] it retries after the back-off's next pause.
    /// After a 404 to an audio feed it stops, since the camera records no
    /// audio. After any other ending, a 404 to a video feed among them, it
    /// retries after 2 s. Only an oversized frame moves the back-off, and only
    /// [`Reconnect::delivered`] ends it. The example on [`Reconnect`] hands it
    /// three endings.
    #[must_use]
    pub fn after(&mut self, end: SessionEnd) -> Next {
        match end {
            SessionEnd::Oversized => {
                let delay = next_oversized_delay(self.oversized);
                self.oversized = Some(delay);
                Next::Retry(delay)
            }
            SessionEnd::Refused { status: NOT_FOUND } if self.kind == StreamKind::Audio => {
                Next::Stop
            }
            SessionEnd::Closed | SessionEnd::Failed | SessionEnd::Refused { .. } => {
                Next::Retry(RECONNECT_DELAY)
            }
        }
    }
}

/// The pause before the next attempt after another oversized frame: 2 s,
/// doubling up to [`MAX_OVERSIZED_DELAY`].
fn next_oversized_delay(previous: Option<Duration>) -> Duration {
    previous.map_or(RECONNECT_DELAY, |delay| {
        delay.saturating_mul(2).min(MAX_OVERSIZED_DELAY)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const TWO_SECONDS: Duration = Duration::from_secs(2);

    #[test]
    fn a_closed_or_failed_session_retries_in_two_seconds() {
        for kind in [StreamKind::Main, StreamKind::Sub, StreamKind::Audio] {
            let mut reconnect = Reconnect::new(kind);
            for end in [SessionEnd::Closed, SessionEnd::Failed] {
                assert_eq!(
                    reconnect.after(end),
                    Next::Retry(TWO_SECONDS),
                    "{kind:?} {end:?}"
                );
            }
        }
    }

    #[test]
    fn an_oversized_session_backs_off_2_4_8_16_32_60_60() {
        let mut reconnect = Reconnect::new(StreamKind::Main);
        let expected = [2, 4, 8, 16, 32, 60, 60].map(|secs| Next::Retry(Duration::from_secs(secs)));
        let delays = expected.map(|_| reconnect.after(SessionEnd::Oversized));
        assert_eq!(delays, expected);
    }

    /// A feed backing off after two oversized frames, the last of 4 s.
    fn backing_off() -> Reconnect {
        let mut reconnect = Reconnect::new(StreamKind::Sub);
        let _ = reconnect.after(SessionEnd::Oversized);
        let _ = reconnect.after(SessionEnd::Oversized);
        reconnect
    }

    #[test]
    fn a_session_that_delivered_ends_the_back_off() {
        let mut reconnect = backing_off();
        reconnect.delivered();
        assert!(!reconnect.is_backing_off());
    }

    /// The session delivers video before it meets another oversized frame.
    #[test]
    fn a_back_off_after_a_delivery_starts_again_at_two_seconds() {
        let mut reconnect = backing_off();
        reconnect.delivered();
        assert_eq!(
            reconnect.after(SessionEnd::Oversized),
            Next::Retry(TWO_SECONDS)
        );
    }

    /// The endings of sessions that deliver nothing.
    const UNDELIVERED: [SessionEnd; 3] = [
        SessionEnd::Closed,
        SessionEnd::Failed,
        SessionEnd::Refused { status: 503 },
    ];

    /// A session that delivers nothing leaves the back-off as it was,
    /// however it ends: the feed still announces video's return.
    #[test]
    fn a_session_that_delivered_nothing_keeps_the_back_off() {
        let mut reconnect = backing_off();
        for end in UNDELIVERED {
            let _ = reconnect.after(end);
            assert!(reconnect.is_backing_off(), "{end:?}");
        }
    }

    /// While the feed backs off, a session that ends any other way retries
    /// in two seconds, not after the back-off's pause.
    #[test]
    fn a_session_that_delivered_nothing_retries_in_two_seconds() {
        let mut reconnect = backing_off();
        for end in UNDELIVERED {
            assert_eq!(reconnect.after(end), Next::Retry(TWO_SECONDS), "{end:?}");
        }
    }

    /// After sessions that deliver nothing, the next oversized frame waits
    /// longer than the last.
    #[test]
    fn a_session_that_delivered_nothing_keeps_the_pause_growing() {
        let mut reconnect = backing_off();
        for end in UNDELIVERED {
            let _ = reconnect.after(end);
        }
        assert_eq!(
            reconnect.after(SessionEnd::Oversized),
            Next::Retry(Duration::from_secs(8))
        );
    }

    /// A feed starts without a back-off, and neither a delivery nor any
    /// ending but an oversized frame starts one.
    #[test]
    fn nothing_but_an_oversized_frame_starts_a_back_off() {
        for kind in [StreamKind::Main, StreamKind::Sub, StreamKind::Audio] {
            let mut reconnect = Reconnect::new(kind);
            assert!(!reconnect.is_backing_off(), "{kind:?} at its start");
            reconnect.delivered();
            assert!(!reconnect.is_backing_off(), "{kind:?} after a delivery");
            for end in [
                SessionEnd::Closed,
                SessionEnd::Failed,
                SessionEnd::Refused { status: 404 },
                SessionEnd::Refused { status: 503 },
            ] {
                let _ = reconnect.after(end);
                assert!(!reconnect.is_backing_off(), "{kind:?} {end:?}");
            }
        }
    }

    #[test]
    fn an_oversized_frame_starts_a_back_off() {
        for kind in [StreamKind::Main, StreamKind::Sub, StreamKind::Audio] {
            let mut reconnect = Reconnect::new(kind);
            let _ = reconnect.after(SessionEnd::Oversized);
            assert!(reconnect.is_backing_off(), "{kind:?}");
        }
    }

    #[test]
    fn an_audio_feed_answered_404_stops() {
        let mut reconnect = Reconnect::new(StreamKind::Audio);
        assert_eq!(
            reconnect.after(SessionEnd::Refused { status: 404 }),
            Next::Stop
        );
    }

    #[test]
    fn a_video_feed_answered_404_retries() {
        for kind in [StreamKind::Main, StreamKind::Sub] {
            let mut reconnect = Reconnect::new(kind);
            assert_eq!(
                reconnect.after(SessionEnd::Refused { status: 404 }),
                Next::Retry(TWO_SECONDS),
                "{kind:?}"
            );
        }
    }

    /// A 404 alone says a stream has no live feed; any other status is a
    /// failure that every feed retries, an audio feed's too.
    #[test]
    fn a_feed_answered_with_another_status_retries() {
        for kind in [StreamKind::Main, StreamKind::Sub, StreamKind::Audio] {
            for status in [200, 401, 403, 405, 500, 503] {
                let mut reconnect = Reconnect::new(kind);
                assert_eq!(
                    reconnect.after(SessionEnd::Refused { status }),
                    Next::Retry(TWO_SECONDS),
                    "{kind:?} {status}"
                );
            }
        }
    }
}
