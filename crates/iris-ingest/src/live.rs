//! The live ring: fan-out of frames to live viewers without copying.
//!
//! Every frame pushed into the assembler is also published here as soon as it
//! arrives, with a provisional duration, so that WebSocket and LL-HLS clients
//! see it without waiting for the fragment to close. Late joiners get the
//! current init segment and the frames since the last sync frame.

use std::collections::VecDeque;
use std::fmt;
use std::sync::{Arc, Mutex};

use bytes::Bytes;
use iris_core::id::StreamKey;
use iris_core::time::WallUs;
use iris_mp4::init::SampleEntry;
use thiserror::Error;
use tokio::sync::Notify;

/// Longest run of frames kept for fast start; a GOP longer than this is not
/// replayed to late joiners, who then wait for the next sync frame.
const MAX_RECENT_FRAMES: usize = 600;
/// Most payload retained for fast-start replay by one stream.
const MAX_RECENT_PAYLOAD_BYTES: usize = 64 * 1_024 * 1_024;
/// Most payload retained for slow live subscribers by one stream.
const MAX_FANOUT_PAYLOAD_BYTES: usize = 64 * 1_024 * 1_024;

/// One frame as published to live viewers. `Debug` prints the payload's
/// length, not its bytes.
#[derive(Clone)]
pub struct LiveFrame {
    /// The stream it belongs to.
    pub key: StreamKey,
    /// Codec configuration in force for this frame.
    pub sample_entry: Arc<SampleEntry>,
    /// Length-prefixed NAL units without parameter sets.
    pub data: Bytes,
    /// Provisional duration in 90 kHz ticks (the regulariser's nominal).
    pub duration: u32,
    /// Whether the frame is a random access point.
    pub is_sync: bool,
    /// Provisional decode time: the pending fragment's start plus the frame's
    /// index times the nominal duration.
    pub decode_time: u64,
    /// NVR wall time of the frame's first packet.
    pub wall: WallUs,
}

impl fmt::Debug for LiveFrame {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LiveFrame")
            .field("key", &self.key)
            .field("sample_entry", &self.sample_entry)
            .field("data", &format_args!("<{} bytes>", self.data.len()))
            .field("duration", &self.duration)
            .field("is_sync", &self.is_sync)
            .field("decode_time", &self.decode_time)
            .field("wall", &self.wall)
            .finish()
    }
}

/// Why a subscription produced no frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Error)]
pub enum LiveError {
    /// The subscriber fell behind and `0` frames were skipped; resume at the
    /// next sync frame.
    #[error("live subscriber lagged by {0} frames")]
    Lagged(u64),
    /// The ring was dropped; the stream is gone.
    #[error("live ring closed")]
    Closed,
}

struct QueuedFrame {
    sequence: u64,
    frame: Arc<LiveFrame>,
}

struct Shared {
    init: Option<Bytes>,
    recent: Vec<Arc<LiveFrame>>,
    recent_payload_bytes: usize,
    fanout: VecDeque<QueuedFrame>,
    fanout_payload_bytes: usize,
    capacity: usize,
    next_sequence: u64,
    rings: usize,
    subscribers: usize,
    closed: bool,
}

impl Shared {
    fn clear_recent(&mut self) {
        self.recent.clear();
        self.recent_payload_bytes = 0;
    }

    fn clear_fanout(&mut self) {
        self.fanout.clear();
        self.fanout_payload_bytes = 0;
    }
}

struct Inner {
    shared: Mutex<Shared>,
    notify: Notify,
}

/// Fan-out point for one stream's frames. Cloning shares the ring. `Debug`
/// summarises the windows (frame and byte counts) without their payload and
/// without waiting for the lock.
pub struct LiveRing {
    inner: Arc<Inner>,
}

impl fmt::Debug for LiveRing {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut ring = f.debug_struct("LiveRing");
        match self.inner.shared.try_lock() {
            Ok(shared) => ring
                .field("rings", &shared.rings)
                .field("subscribers", &shared.subscribers)
                .field("recent_frames", &shared.recent.len())
                .field("recent_payload_bytes", &shared.recent_payload_bytes)
                .field("fanout_frames", &shared.fanout.len())
                .field("fanout_payload_bytes", &shared.fanout_payload_bytes)
                .field("closed", &shared.closed)
                .finish_non_exhaustive(),
            Err(_) => ring.finish_non_exhaustive(),
        }
    }
}

impl Clone for LiveRing {
    fn clone(&self) -> Self {
        if let Ok(mut shared) = self.inner.shared.lock() {
            shared.rings += 1;
        }
        Self {
            inner: Arc::clone(&self.inner),
        }
    }
}

impl Drop for LiveRing {
    fn drop(&mut self) {
        if let Ok(mut shared) = self.inner.shared.lock() {
            shared.rings -= 1;
            if shared.rings == 0 {
                shared.closed = true;
            }
        }
        self.inner.notify.notify_waiters();
    }
}

impl LiveRing {
    /// Creates a ring whose subscribers may lag by up to `capacity` frames.
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        Self {
            inner: Arc::new(Inner {
                shared: Mutex::new(Shared {
                    init: None,
                    recent: Vec::new(),
                    recent_payload_bytes: 0,
                    fanout: VecDeque::new(),
                    fanout_payload_bytes: 0,
                    capacity: capacity.max(1),
                    next_sequence: 0,
                    rings: 1,
                    subscribers: 0,
                    closed: false,
                }),
                notify: Notify::new(),
            }),
        }
    }

    /// Subscribes to frames published from now on.
    #[must_use]
    pub fn subscribe(&self) -> LiveSubscription {
        let cursor = self.inner.shared.lock().map_or(0, |mut shared| {
            shared.subscribers += 1;
            shared.next_sequence
        });
        LiveSubscription {
            inner: Arc::clone(&self.inner),
            cursor,
            needs_sync: false,
        }
    }

    /// The current initialization segment (`ftyp` + `moov`), once known.
    #[must_use]
    pub fn init_segment(&self) -> Option<Bytes> {
        self.inner
            .shared
            .lock()
            .ok()
            .and_then(|shared| shared.init.clone())
    }

    /// Frames since the last sync frame, oldest first; empty before the first
    /// sync frame or when the run since it exceeded the replay limit.
    #[must_use]
    pub fn recent_from_sync(&self) -> Vec<Arc<LiveFrame>> {
        self.inner
            .shared
            .lock()
            .ok()
            .map(|shared| shared.recent.clone())
            .unwrap_or_default()
    }

    /// Number of live subscribers.
    #[must_use]
    pub fn subscriber_count(&self) -> usize {
        self.inner
            .shared
            .lock()
            .map_or(0, |shared| shared.subscribers)
    }

    /// Replaces the initialization segment; later subscribers see it.
    pub fn set_init_segment(&self, bytes: Bytes) {
        if let Ok(mut shared) = self.inner.shared.lock() {
            shared.init = Some(bytes);
        }
    }

    /// Publishes a frame to every subscriber and to the fast-start buffer.
    #[expect(
        clippy::missing_panics_doc,
        reason = "the expect pops only while the queue holds an earlier frame"
    )]
    pub fn publish(&self, frame: Arc<LiveFrame>) {
        if let Ok(mut shared) = self.inner.shared.lock() {
            if frame.is_sync {
                shared.clear_recent();
            }
            if frame.is_sync || !shared.recent.is_empty() {
                match shared.recent_payload_bytes.checked_add(frame.data.len()) {
                    Some(bytes)
                        if shared.recent.len() < MAX_RECENT_FRAMES
                            && bytes <= MAX_RECENT_PAYLOAD_BYTES =>
                    {
                        shared.recent_payload_bytes = bytes;
                        shared.recent.push(Arc::clone(&frame));
                    }
                    _ => shared.clear_recent(),
                }
            }

            let sequence = shared.next_sequence;
            shared.next_sequence += 1;
            if shared.subscribers == 0 || frame.data.len() > MAX_FANOUT_PAYLOAD_BYTES {
                shared.clear_fanout();
            } else {
                while shared.fanout.len() >= shared.capacity
                    || shared
                        .fanout_payload_bytes
                        .checked_add(frame.data.len())
                        .is_none_or(|bytes| bytes > MAX_FANOUT_PAYLOAD_BYTES)
                {
                    let removed = shared
                        .fanout
                        .pop_front()
                        .expect("a fitting frame only overflows a non-empty queue");
                    shared.fanout_payload_bytes -= removed.frame.data.len();
                }
                shared.fanout_payload_bytes += frame.data.len();
                shared.fanout.push_back(QueuedFrame { sequence, frame });
            }
        }
        self.inner.notify.notify_waiters();
    }
}

/// One subscriber's view of a [`LiveRing`].
pub struct LiveSubscription {
    inner: Arc<Inner>,
    cursor: u64,
    needs_sync: bool,
}

impl fmt::Debug for LiveSubscription {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LiveSubscription").finish_non_exhaustive()
    }
}

impl LiveSubscription {
    /// Waits for the next frame.
    ///
    /// # Errors
    ///
    /// [`LiveError::Lagged`] when frames were skipped (the next call resumes
    /// with the oldest retained frame; wait for a sync frame before decoding)
    /// and [`LiveError::Closed`] when the ring is gone.
    pub async fn next(&mut self) -> Result<Arc<LiveFrame>, LiveError> {
        loop {
            let inner = Arc::clone(&self.inner);
            let notified = inner.notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            let action = match inner.shared.lock() {
                Ok(shared) => self.next_action(&shared),
                Err(_) => NextAction::Closed,
            };
            match action {
                NextAction::Frame(frame) => return Ok(frame),
                NextAction::Lagged(skipped) => return Err(LiveError::Lagged(skipped)),
                NextAction::Closed => return Err(LiveError::Closed),
                NextAction::Continue => {}
                NextAction::Wait => notified.as_mut().await,
            }
        }
    }

    fn next_action(&mut self, shared: &Shared) -> NextAction {
        let next = shared
            .fanout
            .iter()
            .find(|queued| queued.sequence >= self.cursor);
        let available = next.map_or(shared.next_sequence, |queued| queued.sequence);
        if available > self.cursor {
            let skipped = available - self.cursor;
            self.cursor = available;
            self.needs_sync = true;
            return NextAction::Lagged(skipped);
        }
        if let Some(queued) = next {
            self.cursor += 1;
            if self.needs_sync && !queued.frame.is_sync {
                return NextAction::Continue;
            }
            self.needs_sync = false;
            return NextAction::Frame(Arc::clone(&queued.frame));
        }
        if shared.closed {
            NextAction::Closed
        } else {
            NextAction::Wait
        }
    }
}

impl Drop for LiveSubscription {
    fn drop(&mut self) {
        if let Ok(mut shared) = self.inner.shared.lock() {
            shared.subscribers -= 1;
            if shared.subscribers == 0 {
                shared.clear_fanout();
            }
        }
    }
}

enum NextAction {
    Frame(Arc<LiveFrame>),
    Lagged(u64),
    Closed,
    Continue,
    Wait,
}

#[cfg(test)]
mod tests {
    use iris_core::id::{CameraId, StreamKind};

    use super::*;

    fn frame(index: u64, is_sync: bool) -> Arc<LiveFrame> {
        sized_frame(index, is_sync, 6)
    }

    fn sized_frame(index: u64, is_sync: bool, payload_len: usize) -> Arc<LiveFrame> {
        Arc::new(LiveFrame {
            key: StreamKey {
                camera: CameraId::try_new("cam1").unwrap(),
                kind: StreamKind::Sub,
            },
            sample_entry: Arc::new(SampleEntry::new(
                Bytes::from_static(b"\0\0\0\x08hvc1"),
                "hvc1.1.6.L150.B0".into(),
                704,
                480,
            )),
            data: Bytes::from(vec![index as u8; payload_len]),
            duration: 3000,
            is_sync,
            decode_time: index * 3000,
            wall: WallUs::new(1_000_000 + index as i64 * 33_333),
        })
    }

    #[tokio::test]
    async fn publishes_to_subscribers_in_order() {
        let ring = LiveRing::new(8);
        let mut sub = ring.subscribe();
        ring.publish(frame(0, true));
        ring.publish(frame(1, false));
        assert_eq!(sub.next().await.unwrap().decode_time, 0);
        assert_eq!(sub.next().await.unwrap().decode_time, 3000);
        assert_eq!(ring.subscriber_count(), 1);
    }

    #[test]
    fn recent_frames_start_at_the_last_sync() {
        let ring = LiveRing::new(8);
        assert!(ring.recent_from_sync().is_empty());
        ring.publish(frame(0, false)); // before any sync frame: not retained
        assert!(ring.recent_from_sync().is_empty());
        ring.publish(frame(1, true));
        ring.publish(frame(2, false));
        ring.publish(frame(3, false));
        ring.publish(frame(4, true));
        ring.publish(frame(5, false));
        let recent = ring.recent_from_sync();
        assert_eq!(recent.len(), 2);
        assert!(recent[0].is_sync);
        assert_eq!(recent[1].decode_time, 5 * 3000);
    }

    #[test]
    fn an_overlong_run_is_not_replayed() {
        let ring = LiveRing::new(8);
        ring.publish(frame(0, true));
        for i in 1..=(MAX_RECENT_FRAMES as u64 + 5) {
            ring.publish(frame(i, false));
        }
        assert!(ring.recent_from_sync().is_empty());
        ring.publish(frame(999, true));
        assert_eq!(ring.recent_from_sync().len(), 1);
    }

    #[test]
    fn fast_start_payload_is_bounded_and_waits_for_a_new_sync() {
        const MIB: usize = 1_024 * 1_024;
        const PAYLOAD_LIMIT: usize = 64 * MIB;
        let ring = LiveRing::new(256);
        let mut weak = Vec::new();
        for index in 0..=64 {
            let frame = sized_frame(index, index == 0, MIB);
            weak.push(Arc::downgrade(&frame));
            ring.publish(frame);
        }
        let retained: usize = ring
            .recent_from_sync()
            .iter()
            .map(|frame| frame.data.len())
            .sum();
        assert!(retained <= PAYLOAD_LIMIT, "retained {retained} bytes");
        assert!(weak.iter().all(|frame| frame.strong_count() == 0));

        let oversized_sync = sized_frame(65, true, PAYLOAD_LIMIT + 1);
        let oversized_sync_weak = Arc::downgrade(&oversized_sync);
        ring.publish(oversized_sync);
        assert!(ring.recent_from_sync().is_empty());
        assert_eq!(oversized_sync_weak.strong_count(), 0);

        ring.publish(sized_frame(66, false, 1));
        assert!(ring.recent_from_sync().is_empty());
        ring.publish(sized_frame(67, true, 1));
        assert_eq!(ring.recent_from_sync().len(), 1);
    }

    #[tokio::test]
    async fn a_slow_subscriber_sees_lag_then_resumes() {
        let ring = LiveRing::new(2);
        let mut sub = ring.subscribe();
        for i in 0..5 {
            ring.publish(frame(i, i == 0));
        }
        match sub.next().await {
            Err(LiveError::Lagged(n)) => assert!(n >= 1),
            other => panic!("expected lag, got {other:?}"),
        }
        ring.publish(frame(5, true));
        let resumed = loop {
            match sub.next().await {
                Ok(frame) => break frame,
                Err(LiveError::Lagged(_)) => {}
                Err(error) => panic!("unexpected live error: {error}"),
            }
        };
        assert!(resumed.is_sync);
    }

    #[tokio::test]
    async fn slow_subscriber_payload_is_bounded_and_resumes_at_sync() {
        const MIB: usize = 1_024 * 1_024;
        const PAYLOAD_LIMIT: usize = 64 * MIB;
        let ring = LiveRing::new(256);
        let mut sub = ring.subscribe();
        let mut weak = Vec::new();
        for index in 0..=64 {
            let frame = sized_frame(index, false, MIB);
            weak.push(Arc::downgrade(&frame));
            ring.publish(frame);
        }
        let retained = weak.iter().filter(|frame| frame.strong_count() > 0).count() * MIB;
        assert!(retained <= PAYLOAD_LIMIT, "retained {retained} bytes");
        assert!(matches!(sub.next().await, Err(LiveError::Lagged(n)) if n >= 1));

        ring.publish(sized_frame(65, false, 1));
        ring.publish(sized_frame(66, true, 1));
        let resumed = loop {
            match sub.next().await {
                Ok(frame) => break frame,
                Err(LiveError::Lagged(_)) => {}
                Err(error) => panic!("unexpected live error: {error}"),
            }
        };
        assert!(resumed.is_sync);
    }

    #[test]
    fn fanout_retains_nothing_without_subscribers() {
        let ring = LiveRing::new(256);
        let frame = sized_frame(0, false, 1_024);
        let weak = Arc::downgrade(&frame);
        ring.publish(frame);
        assert_eq!(weak.strong_count(), 0);
    }

    #[tokio::test]
    async fn dropped_ring_closes_subscriptions() {
        let ring = LiveRing::new(2);
        let mut sub = ring.subscribe();
        drop(ring);
        assert_eq!(sub.next().await.unwrap_err(), LiveError::Closed);
    }

    #[test]
    fn init_segment_updates_are_visible() {
        let ring = LiveRing::new(2);
        assert!(ring.init_segment().is_none());
        ring.set_init_segment(Bytes::from_static(b"one"));
        assert_eq!(ring.init_segment().as_deref(), Some(&b"one"[..]));
        let other = ring.clone();
        other.set_init_segment(Bytes::from_static(b"two"));
        assert_eq!(ring.init_segment().as_deref(), Some(&b"two"[..]));
    }
}
