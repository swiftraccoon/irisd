//! Prometheus text exposition.

use std::fmt::Write as _;
use std::sync::Arc;

use axum::extract::State;
use axum::http::header;
use axum::response::{IntoResponse, Response};
use iris_core::time::WallUs;

use crate::state::ApiState;

pub(crate) async fn metrics(State(state): State<Arc<ApiState>>) -> Response {
    let mut out = String::with_capacity(512);
    let mut counter = |name: &str, help: &str, value: u64| {
        let _ = writeln!(
            out,
            "# HELP {name} {help}\n# TYPE {name} counter\n{name} {value}"
        );
    };
    counter(
        "irisd_api_requests_total",
        "Authenticated API requests served.",
        state.stats.requests(),
    );
    let mut gauge = |name: &str, help: &str, value: u64| {
        let _ = writeln!(
            out,
            "# HELP {name} {help}\n# TYPE {name} gauge\n{name} {value}"
        );
    };
    gauge(
        "irisd_live_clients",
        "Live WebSocket clients connected.",
        state.stats.live_clients(),
    );
    gauge(
        "irisd_event_clients",
        "Event WebSocket clients connected.",
        state.stats.event_clients(),
    );
    let uptime = WallUs::now().micros_since(state.started) / 1_000_000;
    gauge(
        "irisd_uptime_seconds",
        "Seconds since the API started.",
        u64::try_from(uptime).unwrap_or(0),
    );
    match state.catalog.stats().await {
        Ok(catalog) => {
            gauge(
                "irisd_catalog_segments",
                "Segments known to the catalog.",
                catalog.segments,
            );
            gauge(
                "irisd_catalog_sealed_bytes",
                "Bytes of sealed recordings.",
                catalog.sealed_bytes,
            );
            gauge(
                "irisd_catalog_events",
                "Events stored in the catalog.",
                catalog.events,
            );
        }
        Err(e) => tracing::warn!(error = %e, "catalog stats unavailable for metrics"),
    }
    (
        [(
            header::CONTENT_TYPE,
            "text/plain; version=0.0.4; charset=utf-8",
        )],
        out,
    )
        .into_response()
}
