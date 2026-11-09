//! Camera listing.

use std::sync::Arc;

use axum::Json;
use axum::extract::{Path, State};
use iris_core::id::CameraId;
use iris_core::time::WallUs;
use serde::Serialize;

use crate::error::ApiError;
use crate::state::{ActiveCamera, ApiState};

/// A camera as the API presents it.
#[derive(Debug, Serialize)]
pub(crate) struct CameraDto {
    id: String,
    name: String,
    vendor: Option<String>,
    has_snapshot: bool,
    /// Added through the API; `DELETE /api/cameras/{camera}` applies to it.
    provisioned: bool,
}

pub(crate) async fn list(
    State(state): State<Arc<ApiState>>,
) -> Result<Json<Vec<CameraDto>>, ApiError> {
    let mut cameras: Vec<CameraDto> = state
        .active_cameras
        .read()
        .expect("no holder panics while the camera registry is locked")
        .values()
        .map(|camera| camera_dto(camera, &state))
        .collect();
    cameras.sort_by(|a, b| a.id.cmp(&b.id));
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
    let camera = state
        .active_cameras
        .read()
        .expect("no holder panics while the camera registry is locked")
        .get(&id)
        .cloned()
        .ok_or(ApiError::NotFound)?;
    let streams = state
        .live
        .camera_stats(&id)
        .into_iter()
        .map(|(key, stats)| {
            let last_frame = stats.last_frame_wall();
            StreamHealthDto {
                kind: key.kind.to_string(),
                connected: stats.connected(),
                frames: stats.frames(),
                fragments: stats.fragments(),
                bytes: stats.bytes(),
                reconnects: stats.reconnects(),
                loss_events: stats.loss_events(),
                gaps: stats.gaps(),
                last_frame: last_frame.map(|wall| wall.to_string()),
                last_frame_us: last_frame.map(WallUs::get),
                codec: stats.codec_description(),
                nominal_ticks: stats.nominal_ticks(),
                camera_offset_ms: stats.camera_offset_us().map(|us| us / 1000),
                live_viewers: state.live.external_viewers(&key),
            }
        })
        .collect();
    Ok(Json(CameraDetailDto {
        camera: camera_dto(&camera, &state),
        streams,
    }))
}

pub(crate) fn camera_dto(camera: &ActiveCamera, state: &ApiState) -> CameraDto {
    CameraDto {
        has_snapshot: state
            .snapshot_sources
            .read()
            .expect("no holder panics while the snapshot registry is locked")
            .contains_key(&camera.id),
        id: camera.id.to_string(),
        name: camera.name.clone(),
        vendor: camera.vendor.clone(),
        provisioned: camera.provisioned,
    }
}
