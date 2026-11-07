//! Liveness endpoint.

use std::sync::Arc;

use axum::Json;
use axum::extract::State;
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
        uptime_s: i64::try_from(state.started_monotonic.elapsed().as_secs()).unwrap_or(i64::MAX),
        version: env!("CARGO_PKG_VERSION"),
    })
}
