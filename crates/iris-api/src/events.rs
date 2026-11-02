//! Event listing and live event push.

use std::sync::Arc;

use axum::Json;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{FromRequestParts, Path, Query, Request, State};
use axum::response::{IntoResponse, Response};
use iris_catalog::{EventId, EventRow};
use iris_core::event::Event;
use iris_core::id::CameraId;
use iris_core::time::WallUs;
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast::error::RecvError;

use crate::error::ApiError;
use crate::query::{TimeParams, resolve_range};
use crate::state::ApiState;

/// Default and maximum number of events returned by a listing.
const DEFAULT_LIMIT: usize = 200;
const MAX_LIMIT: usize = 10_000;

#[derive(Debug, Default, Deserialize)]
pub(crate) struct EventParams {
    camera: Option<String>,
    limit: Option<usize>,
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

/// The body of a retain request.
#[derive(Debug, Default, Deserialize)]
pub(crate) struct RetainBody {
    /// Keep the footage around the event until this time (Unix microseconds
    /// or RFC 3339); `null` or absent clears the mark.
    until: Option<String>,
}

/// `POST /api/events/{id}/retain`: keeps the footage around an event from
/// retention until `until`, or clears the mark.
pub(crate) async fn retain(
    State(state): State<Arc<ApiState>>,
    Path(id): Path<i64>,
    body: Option<Json<RetainBody>>,
) -> Result<Json<EventDto>, ApiError> {
    let until = match body.and_then(|Json(b)| b.until) {
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
        let upgrade = WebSocketUpgrade::from_request_parts(&mut parts, &())
            .await
            .map_err(|e| ApiError::BadRequest(e.to_string()))?;
        return Ok(upgrade.on_upgrade(move |socket| push(socket, state)));
    }
    let camera = params
        .camera
        .as_deref()
        .map(CameraId::try_new)
        .transpose()
        .map_err(|e| ApiError::BadRequest(e.to_string()))?;
    let range = resolve_range(&params.time, WallUs::now())?;
    let limit = params.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT);
    let rows = state.catalog.events(camera.as_ref(), range, limit).await?;
    let events: Vec<EventDto> = rows.into_iter().map(EventDto::from).collect();
    Ok(Json(events).into_response())
}

async fn push(mut socket: WebSocket, state: Arc<ApiState>) {
    state.stats.event_client_joined();
    let mut rx = state.events.subscribe();
    loop {
        tokio::select! {
            () = state.shutdown.cancelled() => break,
            incoming = socket.recv() => match incoming {
                None | Some(Err(_) | Ok(Message::Close(_))) => break,
                Some(Ok(_)) => {}
            },
            received = rx.recv() => match received {
                Ok(event) => {
                    let Ok(text) = serde_json::to_string(&*event) else { continue };
                    if socket.send(Message::Text(text.into())).await.is_err() {
                        break;
                    }
                }
                Err(RecvError::Lagged(_)) => {}
                Err(RecvError::Closed) => break,
            },
        }
    }
    state.stats.event_client_left();
}
