//! Camera listing.

use std::sync::Arc;

use axum::Json;
use axum::extract::{Path, State};
use iris_core::id::CameraId;
use iris_core::time::WallUs;
use serde::Serialize;

use crate::error::ApiError;
use crate::state::ApiState;

/// A camera as the API presents it.
#[derive(Debug, Serialize)]
pub(crate) struct CameraDto {
    id: String,
    name: String,
    vendor: Option<String>,
    has_snapshot: bool,
}

pub(crate) async fn list(
    State(state): State<Arc<ApiState>>,
) -> Result<Json<Vec<CameraDto>>, ApiError> {
    let rows = state.catalog.cameras().await?;
    let cameras = rows
        .into_iter()
        .map(|row| CameraDto {
            has_snapshot: state.snapshot_sources.contains_key(&row.id),
            id: row.id.to_string(),
            name: row.name,
            vendor: row.vendor,
        })
        .collect();
    Ok(Json(cameras))
}

/// One stream's health as the API presents it.
#[derive(Debug, Serialize)]
pub(crate) struct StreamHealthDto {
    kind: String,
    connected: bool,
    frames: u64,
    fragments: u64,
    bytes: u64,
    reconnects: u64,
    loss_events: u64,
    gaps: u64,
    /// Recorder time of the last frame, RFC 3339, when any arrived.
    last_frame: Option<String>,
    last_frame_us: Option<i64>,
    codec: Option<String>,
    /// Nominal frame duration in 90 kHz ticks, once measured.
    nominal_ticks: Option<u32>,
    /// Camera clock minus recorder clock in milliseconds, from RTCP, when known.
    camera_offset_ms: Option<i64>,
    live_viewers: usize,
}

/// A camera with its streams' health.
#[derive(Debug, Serialize)]
pub(crate) struct CameraDetailDto {
    #[serde(flatten)]
    camera: CameraDto,
    streams: Vec<StreamHealthDto>,
}

/// `GET /api/cameras/{camera}`: the camera and the health of its streams.
pub(crate) async fn detail(
    State(state): State<Arc<ApiState>>,
    Path(camera): Path<String>,
) -> Result<Json<CameraDetailDto>, ApiError> {
    let id = CameraId::try_new(&camera).map_err(|e| ApiError::BadRequest(e.to_string()))?;
    let row = state
        .catalog
        .cameras()
        .await?
        .into_iter()
        .find(|row| row.id == id)
        .ok_or(ApiError::NotFound)?;
    let streams = state
        .live
        .camera_stats(&id)
        .into_iter()
        .map(|(key, stats)| StreamHealthDto {
            kind: key.kind.to_string(),
            connected: stats.connected(),
            frames: stats.frames(),
            fragments: stats.fragments(),
            bytes: stats.bytes(),
            reconnects: stats.reconnects(),
            loss_events: stats.loss_events(),
            gaps: stats.gaps(),
            last_frame: stats.last_frame_wall().map(rfc3339),
            last_frame_us: stats.last_frame_wall().map(WallUs::get),
            codec: stats.codec_description(),
            nominal_ticks: stats.nominal_ticks(),
            camera_offset_ms: stats.camera_offset_us().map(|us| us / 1000),
            live_viewers: state
                .live
                .get(&key)
                .map_or(0, |ring| ring.subscriber_count()),
        })
        .collect();
    Ok(Json(CameraDetailDto {
        camera: CameraDto {
            has_snapshot: state.snapshot_sources.contains_key(&row.id),
            id: row.id.to_string(),
            name: row.name,
            vendor: row.vendor,
        },
        streams,
    }))
}

fn rfc3339(wall: WallUs) -> String {
    let c = wall.to_civil();
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:06}Z",
        c.year,
        c.month,
        c.day,
        c.hour,
        c.minute,
        c.second,
        wall.get().rem_euclid(1_000_000)
    )
}
