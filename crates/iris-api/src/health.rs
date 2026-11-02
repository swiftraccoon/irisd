//! Liveness endpoint.

use std::sync::Arc;

use axum::Json;
use axum::extract::State;
use iris_core::time::WallUs;
use serde::Serialize;

use crate::state::ApiState;

/// Body of `/api/health`.
#[derive(Debug, Serialize)]
pub(crate) struct Health {
    status: &'static str,
    uptime_s: i64,
    version: &'static str,
}

pub(crate) async fn health(State(state): State<Arc<ApiState>>) -> Json<Health> {
    Json(Health {
        status: "ok",
        uptime_s: WallUs::now().micros_since(state.started) / 1_000_000,
        version: env!("CARGO_PKG_VERSION"),
    })
}
