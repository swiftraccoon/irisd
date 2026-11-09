//! Live streaming over WebSocket: an init segment, then one fragment per frame.

use std::sync::Arc;
use std::time::Duration;

use axum::extract::ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade, close_code};
use axum::extract::{Path, State};
use axum::response::Response;
use bytes::Bytes;
use iris_ingest::{LiveError, LiveFrame, LiveRing};
use iris_mp4::fragment::{FragmentError, FragmentHeader, Sample, encode_fragment};
use iris_mp4::init::{SampleEntry, build_init_segment};
use tokio::sync::OwnedSemaphorePermit;

use crate::error::ApiError;
use crate::segments::stream_key;
use crate::state::{ApiState, ApiStats, ExternalViewerGuard, MAX_LIVE_SOCKETS_PER_STREAM};
use crate::websocket::{self, Inbound};

/// Message type byte for an initialization segment.
pub const MSG_INIT: u8 = 0x01;
/// Message type byte for a media fragment.
pub const MSG_FRAGMENT: u8 = 0x02;
/// Largest live WebSocket message: the type byte plus a 16 MiB payload, the
/// most a viewer accepts.
///
/// A dependent frame whose fragment message would exceed it is skipped, and the
/// session resumes at the next sync frame with a fresh init segment, as after
/// lag. An oversized sync frame closes the socket with code 1009 (message too
/// big), because nothing after it is decodable without it.
pub const MAX_LIVE_MESSAGE_BYTES: usize = 1 + 16 * 1024 * 1024;

/// A stalled peer cannot postpone shutdown past the daemon's outer grace.
const SEND_DEADLINE: Duration = Duration::from_secs(2);

/// The socket slots one live session holds until it ends: one of the
/// process-wide live sockets and one of its stream's viewers.
struct LiveAdmission {
    _socket: OwnedSemaphorePermit,
    _viewer: ExternalViewerGuard,
}

pub(crate) async fn live(
    ws: WebSocketUpgrade,
    State(state): State<Arc<ApiState>>,
    Path((camera, kind)): Path<(String, String)>,
) -> Result<Response, ApiError> {
    let key = stream_key(&camera, &kind)?;
    let ring = state.live.get(&key).ok_or(ApiError::NotFound)?;
    let socket = state.stats.admit_live_socket()?;
    let viewer = state
        .live
        .try_track_external_viewer(&key, MAX_LIVE_SOCKETS_PER_STREAM)
        .ok_or_else(|| {
            state
                .stats
                .reject_websocket("too many live WebSockets for this stream")
        })?;
    let admission = LiveAdmission {
        _socket: socket,
        _viewer: viewer,
    };
    Ok(websocket::limit(ws, MAX_LIVE_MESSAGE_BYTES)
        .on_upgrade(move |socket| session(socket, ring, state, admission)))
}

/// Whether `next` is the sample entry already announced to the client. A
/// shared allocation is the fast path; otherwise the full entries are
/// compared, so a CRC-32C collision cannot reuse a stale init segment.
pub(crate) fn same_sample_entry(
    current: Option<&Arc<SampleEntry>>,
    next: &Arc<SampleEntry>,
) -> bool {
    current.is_some_and(|current| Arc::ptr_eq(current, next) || **current == **next)
}

/// Builds a `0x01` message from an init segment.
fn init_message(init: &[u8]) -> Message {
    let mut out = Vec::with_capacity(1 + init.len());
    out.push(MSG_INIT);
    out.extend_from_slice(init);
    Message::Binary(Bytes::from(out))
}

/// Appends one frame to `out` as a complete fragment (`prft`, `moof`,
/// `mdat`, trailer) holding one sample; returns the fragment's length.
pub(crate) fn encode_live_fragment_into(
    out: &mut Vec<u8>,
    frame: &LiveFrame,
    sequence: u32,
) -> Result<usize, FragmentError> {
    let sample = Sample {
        data: frame.data.clone(),
        duration: frame.duration.max(1),
        is_sync: frame.is_sync,
    };
    let encoded = encode_fragment(
        &FragmentHeader {
            sequence,
            decode_time: frame.decode_time,
            wall: frame.wall,
        },
        std::slice::from_ref(&sample),
    )?;
    let total = usize::try_from(encoded.total_len()).expect("a fragment fits in memory");
    out.reserve(total);
    out.extend_from_slice(&encoded.prefix);
    out.extend_from_slice(&sample.data);
    out.extend_from_slice(&encoded.trailer);
    Ok(total)
}

/// Builds a `0x02` message: one fragment holding one sample, or `None` when
/// the message would exceed [`MAX_LIVE_MESSAGE_BYTES`]. The fragment is
/// encoded in place behind the type byte, so the frame is copied once.
fn fragment_message(frame: &LiveFrame, sequence: u32) -> Result<Option<Message>, FragmentError> {
    // The payload alone may already exceed the limit; skip encoding it.
    if 1 + frame.data.len() > MAX_LIVE_MESSAGE_BYTES {
        return Ok(None);
    }
    let mut out = Vec::with_capacity(1 + frame.data.len() + 512);
    out.push(MSG_FRAGMENT);
    encode_live_fragment_into(&mut out, frame, sequence)?;
    if out.len() > MAX_LIVE_MESSAGE_BYTES {
        return Ok(None);
    }
    Ok(Some(Message::Binary(Bytes::from(out))))
}

/// The close frame sent when a sync frame cannot fit one live message (code 1009).
fn oversized_close() -> Message {
    Message::Close(Some(CloseFrame {
        code: close_code::SIZE,
        reason: "a live sync frame exceeds the 16 MiB message limit".into(),
    }))
}

async fn session(
    mut socket: WebSocket,
    ring: LiveRing,
    state: Arc<ApiState>,
    _admission: LiveAdmission,
) {
    let _client = LiveClientGauge::new(Arc::clone(&state.stats));
    let mut subscription = ring.subscribe();
    let mut sequence: u32 = 0;
    let mut sample_entry: Option<Arc<SampleEntry>> = None;
    let mut waiting_for_sync = true;

    // The first frame, from the backlog or live, carries the sample entry that
    // selects the init segment, so nothing is sent before it.
    let mut backlog = ring.recent_from_sync().into_iter();

    loop {
        if state.shutdown.is_cancelled() {
            break;
        }
        let frame = if let Some(frame) = backlog.next() {
            frame
        } else {
            tokio::select! {
                () = state.shutdown.cancelled() => break,
                incoming = socket.recv() => match websocket::classify(incoming.as_ref()) {
                    Inbound::Control => continue,
                    Inbound::Closed => break,
                    Inbound::Violation => {
                        let _ = send(&mut socket, websocket::policy_close(), &state.shutdown).await;
                        break;
                    }
                },
                next = subscription.next() => match next {
                    Ok(frame) => frame,
                    Err(LiveError::Lagged(skipped)) => {
                        tracing::debug!(skipped, "live client lagged; resyncing at the next sync frame");
                        waiting_for_sync = true;
                        sample_entry = None;
                        continue;
                    }
                    Err(LiveError::Closed) => break,
                },
            }
        };
        if !same_sample_entry(sample_entry.as_ref(), &frame.sample_entry) {
            let init = build_init_segment(&frame.sample_entry);
            if !send(&mut socket, init_message(&init), &state.shutdown).await {
                break;
            }
            sample_entry = Some(Arc::clone(&frame.sample_entry));
            waiting_for_sync = true;
        }
        if waiting_for_sync && !frame.is_sync {
            continue;
        }
        let message = match fragment_message(&frame, sequence.wrapping_add(1)) {
            Ok(Some(message)) => message,
            Ok(None) if frame.is_sync => {
                tracing::warn!(
                    bytes = frame.data.len(),
                    "live sync frame exceeds the message limit; closing the WebSocket"
                );
                state.stats.record_live_oversized_close();
                let _ = send(&mut socket, oversized_close(), &state.shutdown).await;
                break;
            }
            Ok(None) => {
                tracing::debug!(
                    bytes = frame.data.len(),
                    "live frame exceeds the message limit; resyncing at the next sync frame"
                );
                waiting_for_sync = true;
                sample_entry = None;
                continue;
            }
            Err(e) => {
                tracing::warn!(error = %e, "could not encode live frame");
                continue;
            }
        };
        waiting_for_sync = false;
        sequence = sequence.wrapping_add(1);
        if !send(&mut socket, message, &state.shutdown).await {
            break;
        }
    }
}

async fn send(
    socket: &mut WebSocket,
    message: Message,
    shutdown: &tokio_util::sync::CancellationToken,
) -> bool {
    tokio::select! {
        biased;
        () = shutdown.cancelled() => false,
        result = tokio::time::timeout(SEND_DEADLINE, socket.send(message)) => {
            matches!(result, Ok(Ok(())))
        }
    }
}

struct LiveClientGauge(Arc<ApiStats>);

impl LiveClientGauge {
    fn new(stats: Arc<ApiStats>) -> Self {
        stats.live_client_joined();
        Self(stats)
    }
}

impl Drop for LiveClientGauge {
    fn drop(&mut self) {
        self.0.live_client_left();
    }
}

#[cfg(test)]
mod tests {
    use iris_core::id::{CameraId, StreamKey, StreamKind};
    use iris_core::time::WallUs;

    use super::*;

    fn frame(data: Bytes, is_sync: bool) -> LiveFrame {
        LiveFrame {
            key: StreamKey {
                camera: CameraId::try_new("cam1").expect("valid camera id"),
                kind: StreamKind::Main,
            },
            sample_entry: Arc::new(SampleEntry::new(
                Bytes::from_static(b"\0\0\0\x10hvc1\0\0\0\0\0\0\0\0"),
                "hvc1".into(),
                16,
                16,
            )),
            data,
            duration: 3_000,
            is_sync,
            decode_time: 0,
            wall: WallUs::new(1_700_000_000_000_000),
        }
    }

    #[test]
    fn fragment_messages_stop_at_the_live_message_limit() {
        // Dependent frames throughout: a sync sample adds a sample-flags field.
        let small = fragment_message(&frame(Bytes::from_static(&[0, 0, 0, 2, 0x02, 1]), false), 1)
            .expect("encodes")
            .expect("fits");
        let Message::Binary(small) = small else {
            panic!("fragments are binary messages");
        };
        assert_eq!(small[0], MSG_FRAGMENT);
        let overhead = small.len() - 6;

        let largest = MAX_LIVE_MESSAGE_BYTES - overhead;
        let Some(Message::Binary(fits)) =
            fragment_message(&frame(Bytes::from(vec![0x55; largest]), false), 2).expect("encodes")
        else {
            panic!("a message of exactly the limit is sent");
        };
        assert_eq!(fits.len(), MAX_LIVE_MESSAGE_BYTES);
        for oversized in [largest + 1, 20 << 20] {
            assert!(
                fragment_message(&frame(Bytes::from(vec![0x55; oversized]), false), 3)
                    .expect("oversized frames are not an encoding error")
                    .is_none(),
                "{oversized} payload bytes"
            );
        }
    }

    #[test]
    fn sample_entries_compare_by_content_not_only_by_hash() {
        let first = Arc::new(SampleEntry::new(
            Bytes::from_static(b"entry-a"),
            "hvc1".into(),
            16,
            16,
        ));
        let mut collision = SampleEntry::new(Bytes::from_static(b"entry-b"), "hvc1".into(), 16, 16);
        collision.hash = first.hash;
        let collision = Arc::new(collision);
        let copy = Arc::new((*first).clone());
        assert!(same_sample_entry(Some(&first), &first));
        assert!(same_sample_entry(Some(&first), &copy));
        assert!(!same_sample_entry(Some(&first), &collision));
        assert!(!same_sample_entry(None, &first));
    }

    #[test]
    fn client_gauge_is_released_by_drop() {
        let stats = Arc::new(ApiStats::default());
        {
            let _client = LiveClientGauge::new(Arc::clone(&stats));
            assert_eq!(stats.live_clients(), 1);
        }
        assert_eq!(stats.live_clients(), 0);
    }
}
