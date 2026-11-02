//! Segment listing and file serving.

use std::sync::Arc;

use axum::Json;
use axum::extract::{Path, Query, Request, State};
use axum::response::{IntoResponse, Response};
use iris_catalog::{SegmentId, SegmentRow};
use iris_core::id::{CameraId, StreamKey, StreamKind};
use iris_core::time::WallUs;
use serde::Serialize;
use tower::ServiceExt as _;
use tower_http::services::ServeFile;

use crate::error::ApiError;
use crate::query::{TimeParams, resolve_range};
use crate::state::ApiState;

/// A segment as the API presents it (no server paths).
#[derive(Debug, Serialize)]
pub(crate) struct SegmentDto {
    id: i64,
    camera: String,
    kind: String,
    run_seq: u64,
    discontinuity: bool,
    wall_start: String,
    wall_start_us: i64,
    wall_end: String,
    wall_end_us: i64,
    media_start: u64,
    media_end: u64,
    init_len: u32,
    durable_len: u64,
    fragments: u32,
    bytes: u64,
    sealed: bool,
    codecs: String,
    width: u16,
    height: u16,
}

impl From<&SegmentRow> for SegmentDto {
    fn from(row: &SegmentRow) -> Self {
        Self {
            id: row.id.0,
            camera: row.key.camera.to_string(),
            kind: row.key.kind.to_string(),
            run_seq: row.run_seq,
            discontinuity: row.discontinuity,
            wall_start: row.wall_start.to_string(),
            wall_start_us: row.wall_start.get(),
            wall_end: row.wall_end.to_string(),
            wall_end_us: row.wall_end.get(),
            media_start: row.media_start,
            media_end: row.media_end,
            init_len: row.init_len,
            durable_len: row.durable_len,
            fragments: row.fragments,
            bytes: row.bytes,
            sealed: row.sealed,
            codecs: row.codecs.clone(),
            width: row.width,
            height: row.height,
        }
    }
}

/// Parses the `{camera}/{kind}` path pair.
pub(crate) fn stream_key(camera: &str, kind: &str) -> Result<StreamKey, ApiError> {
    let camera = CameraId::try_new(camera).map_err(|e| ApiError::BadRequest(e.to_string()))?;
    let kind: StreamKind = kind
        .parse()
        .map_err(|e: iris_core::id::UnknownStreamKind| ApiError::BadRequest(e.to_string()))?;
    Ok(StreamKey { camera, kind })
}

pub(crate) async fn list(
    State(state): State<Arc<ApiState>>,
    Path((camera, kind)): Path<(String, String)>,
    Query(params): Query<TimeParams>,
) -> Result<Json<Vec<SegmentDto>>, ApiError> {
    let key = stream_key(&camera, &kind)?;
    let range = resolve_range(&params, WallUs::now())?;
    let rows = state.catalog.segments(&key, range).await?;
    Ok(Json(rows.iter().map(SegmentDto::from).collect()))
}

pub(crate) async fn file(
    State(state): State<Arc<ApiState>>,
    Path(id): Path<i64>,
    req: Request,
) -> Result<Response, ApiError> {
    let row = state
        .catalog
        .segment(SegmentId(id))
        .await?
        .ok_or(ApiError::NotFound)?;
    if !tokio::fs::try_exists(&row.path).await.unwrap_or(false) {
        return Err(ApiError::NotFound);
    }
    let mime: mime_guess::Mime = "video/mp4".parse().expect("literal mime");
    let service = ServeFile::new_with_mime(&row.path, &mime);
    let response = match service.oneshot(req).await {
        Ok(response) => response,
        Err(never) => match never {},
    };
    Ok(response.into_response())
}
