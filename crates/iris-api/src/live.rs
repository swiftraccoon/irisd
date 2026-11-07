//! Live streaming over WebSocket: an init segment, then one fragment per frame.

use std::sync::Arc;
use std::time::Duration;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, State};
use axum::response::Response;
use bytes::Bytes;
use iris_core::id::StreamKey;
use iris_ingest::{LiveError, LiveFrame, LiveRing};
use iris_mp4::fragment::{FragmentError, FragmentHeader, Sample, encode_fragment};
use iris_mp4::init::build_init_segment;

use crate::error::ApiError;
use crate::segments::stream_key;
use crate::state::ApiState;
use crate::state::ApiStats;

/// Message type byte for an initialization segment.
pub const MSG_INIT: u8 = 0x01;
/// Message type byte for a media fragment.
pub const MSG_FRAGMENT: u8 = 0x02;

/// A stalled peer cannot postpone shutdown past the daemon's outer grace.
const SEND_DEADLINE: Duration = Duration::from_secs(2);

pub(crate) async fn live(
    ws: WebSocketUpgrade,
    State(state): State<Arc<ApiState>>,
    Path((camera, kind)): Path<(String, String)>,
) -> Result<Response, ApiError> {
    let key = stream_key(&camera, &kind)?;
    let ring = state.live.get(&key).ok_or(ApiError::NotFound)?;
    Ok(ws.on_upgrade(move |socket| session(socket, ring, state, key)))
}

/// Builds a `0x01` message from an init segment.
fn init_message(init: &[u8]) -> Message {
    let mut out = Vec::with_capacity(1 + init.len());
    out.push(MSG_INIT);
    out.extend_from_slice(init);
    Message::Binary(Bytes::from(out))
}

/// Encodes one frame as a complete fragment (`prft`, `moof`, `mdat`,
/// trailer) holding one sample.
pub(crate) fn encode_live_fragment(
    frame: &LiveFrame,
    sequence: u32,
) -> Result<Vec<u8>, FragmentError> {
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
    let mut out = Vec::with_capacity(usize::try_from(encoded.total_len()).expect("fits usize"));
    out.extend_from_slice(&encoded.prefix);
    out.extend_from_slice(&sample.data);
    out.extend_from_slice(&encoded.trailer);
    Ok(out)
}

/// Builds a `0x02` message: one fragment holding one sample.
pub(crate) fn fragment_message(frame: &LiveFrame, sequence: u32) -> Result<Message, FragmentError> {
    let fragment = encode_live_fragment(frame, sequence)?;
    let mut out = Vec::with_capacity(1 + fragment.len());
    out.push(MSG_FRAGMENT);
    out.extend_from_slice(&fragment);
    Ok(Message::Binary(Bytes::from(out)))
}

async fn session(mut socket: WebSocket, ring: LiveRing, state: Arc<ApiState>, key: StreamKey) {
    let _client = LiveClientGauge::new(Arc::clone(&state.stats));
    let _external_viewer = state.live.track_external_viewer(&key);
    let mut subscription = ring.subscribe();
    let mut sequence: u32 = 0;
    let mut init_hash: Option<u32> = None;
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
                incoming = socket.recv() => match incoming {
                    None | Some(Err(_) | Ok(Message::Close(_))) => break,
                    Some(Ok(_)) => continue,
                },
                next = subscription.next() => match next {
                    Ok(frame) => frame,
                    Err(LiveError::Lagged(skipped)) => {
                        tracing::debug!(skipped, "live client lagged; resyncing at the next sync frame");
                        waiting_for_sync = true;
                        init_hash = None;
                        continue;
                    }
                    Err(LiveError::Closed) => break,
                },
            }
        };
        if init_hash != Some(frame.sample_entry.hash) {
            let init = build_init_segment(&frame.sample_entry);
            if !send(&mut socket, init_message(&init), &state.shutdown).await {
                break;
            }
            init_hash = Some(frame.sample_entry.hash);
            waiting_for_sync = true;
        }
        if waiting_for_sync && !frame.is_sync {
            continue;
        }
        waiting_for_sync = false;
        sequence = sequence.wrapping_add(1);
        let message = match fragment_message(&frame, sequence) {
            Ok(m) => m,
            Err(e) => {
                tracing::warn!(error = %e, "could not encode live frame");
                continue;
            }
        };
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
    use super::*;

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
