//! Event listing and live event push.

use std::sync::Arc;
use std::time::Duration;

use axum::Json;
use axum::body::Bytes;
use axum::extract::ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade, close_code};
use axum::extract::{FromRequestParts, Path, Query, Request, State};
use axum::response::{IntoResponse, Response};
use iris_catalog::{EventCursor, EventId, EventRow};
use iris_core::event::Event;
use iris_core::id::CameraId;
use iris_core::time::WallUs;
use serde::{Deserialize, Serialize};
use tokio::sync::OwnedSemaphorePermit;
use tokio::sync::broadcast::error::RecvError;
use tokio_util::sync::CancellationToken;

use crate::error::ApiError;
use crate::query::{TimeParams, resolve_range};
use crate::state::{ApiState, ApiStats};
use crate::websocket::{self, Inbound};

/// Default and maximum number of events returned by a listing.
const DEFAULT_LIMIT: usize = 200;
const MAX_LIMIT: usize = 10_000;
/// A stalled peer cannot postpone shutdown past the daemon's outer grace.
const SEND_DEADLINE: Duration = Duration::from_secs(2);
/// Largest outbound event message: more than twice the 64 KiB event data
/// bound plus the 1 KiB kind, after JSON string escaping.
const MAX_EVENT_MESSAGE_BYTES: usize = 256 * 1024;

#[derive(Debug, Default, Deserialize)]
pub(crate) struct EventParams {
    camera: Option<String>,
    limit: Option<usize>,
    after_us: Option<i64>,
    after_id: Option<i64>,
    #[serde(flatten)]
    time: TimeParams,
}

/// An event with its catalog id.
#[derive(Debug, Serialize)]
pub(crate) struct EventDto {
    id: i64,
    #[serde(flatten)]
    event: Event,
    /// Until when the footage around the event is kept, Unix microseconds, when set.
    retain_until_us: Option<i64>,
}

impl From<EventRow> for EventDto {
    fn from(row: EventRow) -> Self {
        Self {
            id: row.id.0,
            event: row.event,
            retain_until_us: row.retain_until.map(WallUs::get),
        }
    }
}

/// The body of a retain request. An unknown field is an error, so a
/// misspelled `until` cannot clear a mark by accident.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RetainBody {
    /// Keep the footage around the event until this time (Unix microseconds
    /// or RFC 3339); `null` or absent clears the mark.
    until: Option<String>,
}

/// `POST /api/events/{id}/retain`: keeps the footage around an event from
/// retention until `until`, or clears the mark. The body is read as JSON
/// whatever its `Content-Type`; an empty body clears the mark, and a body
/// that is not a [`RetainBody`] answers HTTP 400.
pub(crate) async fn retain(
    State(state): State<Arc<ApiState>>,
    Path(id): Path<i64>,
    body: Bytes,
) -> Result<Json<EventDto>, ApiError> {
    let body: RetainBody = if body.iter().all(u8::is_ascii_whitespace) {
        RetainBody::default()
    } else {
        serde_json::from_slice(&body)
            .map_err(|error| ApiError::BadRequest(format!("bad retain body: {error}")))?
    };
    let until = match body.until {
        Some(raw) => Some(
            crate::query::parse_time(&raw)
                .ok_or_else(|| ApiError::BadRequest(format!("bad 'until' {raw:?}")))?,
        ),
        None => None,
    };
    let row = state
        .catalog
        .retain_event(EventId(id), until)
        .await
        .map_err(|e| match e {
            iris_catalog::CatalogError::UnknownEvent(_) => ApiError::NotFound,
            iris_catalog::CatalogError::EventRetentionLimit => {
                ApiError::Unavailable("event retention capacity is exhausted".to_owned())
            }
            other => ApiError::Catalog(other),
        })?;
    Ok(Json(EventDto::from(row)))
}

/// Lists events, or upgrades to a WebSocket that pushes them as they happen.
pub(crate) async fn list_or_subscribe(
    State(state): State<Arc<ApiState>>,
    Query(params): Query<EventParams>,
    req: Request,
) -> Result<Response, ApiError> {
    let is_upgrade = req
        .headers()
        .get(axum::http::header::UPGRADE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.eq_ignore_ascii_case("websocket"));
    if is_upgrade {
        let (mut parts, _) = req.into_parts();
        // axum answers a request that cannot be upgraded with its own status
        // (426 for a missing upgrade header), as the live route does.
        let upgrade = match WebSocketUpgrade::from_request_parts(&mut parts, &()).await {
            Ok(upgrade) => upgrade,
            Err(rejection) => return Ok(rejection.into_response()),
        };
        let admission = state.stats.admit_event_socket()?;
        return Ok(websocket::limit(upgrade, MAX_EVENT_MESSAGE_BYTES)
            .on_upgrade(move |socket| push(socket, state, admission)));
    }
    let camera = params
        .camera
        .as_deref()
        .map(CameraId::try_new)
        .transpose()
        .map_err(|e| ApiError::BadRequest(e.to_string()))?;
    let range = resolve_range(&params.time, WallUs::now())?;
    let limit = params.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT);
    let after = match (params.after_us, params.after_id) {
        (None, None) => None,
        (Some(time), Some(id)) if id > 0 => Some(EventCursor {
            time: WallUs::new(time),
            id: EventId(id),
        }),
        (Some(_), Some(_)) => {
            return Err(ApiError::BadRequest(
                "'after_id' must be a positive event id".to_owned(),
            ));
        }
        _ => {
            return Err(ApiError::BadRequest(
                "'after_us' and 'after_id' must be supplied together".to_owned(),
            ));
        }
    };
    let rows = state
        .catalog
        .events_after(camera.as_ref(), range, after, limit)
        .await?;
    let events: Vec<EventDto> = rows.into_iter().map(EventDto::from).collect();
    Ok(Json(events).into_response())
}

/// Pushes published events until the peer closes, lags, or shutdown; holds one
/// of the process-wide event socket slots throughout.
async fn push(mut socket: WebSocket, state: Arc<ApiState>, _admission: OwnedSemaphorePermit) {
    let _client = EventClientGauge::new(Arc::clone(&state.stats));
    let mut rx = state.events.subscribe();
    loop {
        tokio::select! {
            () = state.shutdown.cancelled() => break,
            incoming = socket.recv() => match websocket::classify(incoming.as_ref()) {
                Inbound::Control => {}
                Inbound::Closed => break,
                Inbound::Violation => {
                    let _ = send(&mut socket, websocket::policy_close(), &state.shutdown).await;
                    break;
                }
            },
            received = rx.recv() => match received {
                Ok(event) => {
                    let text = match serde_json::to_string(&*event) {
                        Ok(text) => text,
                        Err(error) => {
                            tracing::warn!(
                                %error,
                                camera = %event.camera,
                                "event could not be encoded; skipped"
                            );
                            continue;
                        }
                    };
                    if !send(&mut socket, Message::Text(text.into()), &state.shutdown).await {
                        break;
                    }
                }
                Err(RecvError::Lagged(skipped)) => {
                    let _ = send(
                        &mut socket,
                        event_gap_close(&state.stats, skipped),
                        &state.shutdown,
                    )
                    .await;
                    break;
                }
                Err(RecvError::Closed) => break,
            },
        }
    }
}

async fn send(socket: &mut WebSocket, message: Message, shutdown: &CancellationToken) -> bool {
    tokio::select! {
        biased;
        () = shutdown.cancelled() => false,
        result = tokio::time::timeout(SEND_DEADLINE, socket.send(message)) => {
            matches!(result, Ok(Ok(())))
        }
    }
}

struct EventClientGauge(Arc<ApiStats>);

impl EventClientGauge {
    fn new(stats: Arc<ApiStats>) -> Self {
        stats.event_client_joined();
        Self(stats)
    }
}

impl Drop for EventClientGauge {
    fn drop(&mut self) {
        self.0.event_client_left();
    }
}

fn event_gap_close(stats: &ApiStats, skipped: u64) -> Message {
    stats.record_event_lag(skipped);
    Message::Close(Some(CloseFrame {
        code: close_code::AGAIN,
        reason: format!(
            "event gap: {skipped} messages skipped; reconnect and list the relevant event window"
        )
        .into(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn lagged_receiver_gets_bounded_retry_close_and_metric() {
        let (tx, mut rx) = tokio::sync::broadcast::channel(2);
        tx.send(1).unwrap();
        tx.send(2).unwrap();
        tx.send(3).unwrap();
        let Err(RecvError::Lagged(skipped)) = rx.recv().await else {
            panic!("receiver should report its skipped message count");
        };

        let stats = ApiStats::default();
        let Message::Close(Some(frame)) = event_gap_close(&stats, skipped) else {
            panic!("lag must produce a close frame");
        };
        assert_eq!(frame.code, close_code::AGAIN);
        assert!(frame.reason.contains("event gap"));
        assert!(frame.reason.contains("reconnect"));
        assert!(
            frame
                .reason
                .contains(&format!("{skipped} messages skipped"))
        );
        assert!(frame.reason.len() <= 123, "WebSocket close reason bound");
        assert_eq!(stats.event_messages_lagged(), skipped);
    }

    #[test]
    fn client_gauge_is_released_by_drop() {
        let stats = Arc::new(ApiStats::default());
        {
            let _client = EventClientGauge::new(Arc::clone(&stats));
            assert_eq!(stats.event_clients(), 1);
        }
        assert_eq!(stats.event_clients(), 0);
    }
}
