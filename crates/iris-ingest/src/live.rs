//! The live ring: fan-out of frames to live viewers without copying.
//!
//! Every frame pushed into the assembler is also published here as soon as it
//! arrives, with a provisional duration, so that WebSocket and LL-HLS clients
//! see it without waiting for the fragment to close. Late joiners get the
//! current init segment and the frames since the last sync frame.

use std::fmt;
use std::sync::{Arc, Mutex};

use bytes::Bytes;
use iris_core::id::StreamKey;
use iris_core::time::WallUs;
use iris_mp4::init::SampleEntry;
use thiserror::Error;
use tokio::sync::broadcast;

/// Longest run of frames kept for fast start; a GOP longer than this is not
/// replayed to late joiners, who then wait for the next sync frame.
const MAX_RECENT_FRAMES: usize = 600;

/// One frame as published to live viewers.
#[derive(Clone, Debug)]
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

#[derive(Debug, Default)]
struct Shared {
    init: Option<Bytes>,
    recent: Vec<Arc<LiveFrame>>,
}

/// Fan-out point for one stream's frames. Cloning shares the ring.
#[derive(Clone, Debug)]
pub struct LiveRing {
    tx: broadcast::Sender<Arc<LiveFrame>>,
    shared: Arc<Mutex<Shared>>,
}

impl LiveRing {
    /// Creates a ring whose subscribers may lag by up to `capacity` frames.
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        let (tx, _rx) = broadcast::channel(capacity.max(1));
        Self {
            tx,
            shared: Arc::new(Mutex::new(Shared::default())),
        }
    }

    /// Subscribes to frames published from now on.
    #[must_use]
    pub fn subscribe(&self) -> LiveSubscription {
        LiveSubscription {
            rx: self.tx.subscribe(),
        }
    }

    /// The current initialization segment (`ftyp` + `moov`), once known.
    #[must_use]
    pub fn init_segment(&self) -> Option<Bytes> {
        self.shared.lock().ok().and_then(|s| s.init.clone())
    }

    /// Frames since the last sync frame, oldest first; empty before the first
    /// sync frame or when the run since it exceeded the replay limit.
    #[must_use]
    pub fn recent_from_sync(&self) -> Vec<Arc<LiveFrame>> {
        self.shared
            .lock()
            .ok()
            .map(|s| s.recent.clone())
            .unwrap_or_default()
    }

    /// Number of live subscribers.
    #[must_use]
    pub fn subscriber_count(&self) -> usize {
        self.tx.receiver_count()
    }

    /// Replaces the initialization segment; later subscribers see it.
    pub fn set_init_segment(&self, bytes: Bytes) {
        if let Ok(mut s) = self.shared.lock() {
            s.init = Some(bytes);
        }
    }

    /// Publishes a frame to every subscriber and to the fast-start buffer.
    pub fn publish(&self, frame: Arc<LiveFrame>) {
        if let Ok(mut s) = self.shared.lock() {
            if frame.is_sync {
                s.recent.clear();
            }
            if frame.is_sync || !s.recent.is_empty() {
                if s.recent.len() >= MAX_RECENT_FRAMES {
                    s.recent.clear();
                } else {
                    s.recent.push(Arc::clone(&frame));
                }
            }
        }
        // No receivers is not an error: recording continues without viewers.
        let _ = self.tx.send(frame);
    }
}

/// One subscriber's view of a [`LiveRing`].
pub struct LiveSubscription {
    rx: broadcast::Receiver<Arc<LiveFrame>>,
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
        match self.rx.recv().await {
            Ok(frame) => Ok(frame),
            Err(broadcast::error::RecvError::Lagged(n)) => Err(LiveError::Lagged(n)),
            Err(broadcast::error::RecvError::Closed) => Err(LiveError::Closed),
        }
    }
}

#[cfg(test)]
mod tests {
    use iris_core::id::{CameraId, StreamKind};

    use super::*;

    fn frame(index: u64, is_sync: bool) -> Arc<LiveFrame> {
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
            data: Bytes::from(vec![0, 0, 0, 2, 0x26, index as u8]),
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
        assert!(sub.next().await.is_ok());
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
