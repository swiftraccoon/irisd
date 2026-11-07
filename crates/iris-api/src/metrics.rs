//! Prometheus text exposition.

use std::fmt::Write as _;
use std::sync::Arc;

use axum::extract::State;
use axum::http::header;
use axum::response::{IntoResponse, Response};
use iris_catalog::{CatalogMetrics, CatalogStats};

use crate::state::ApiState;

fn counter(out: &mut String, name: &str, help: &str, value: u64) {
    let _ = writeln!(
        out,
        "# HELP {name} {help}\n# TYPE {name} counter\n{name} {value}"
    );
}

fn gauge(out: &mut String, name: &str, help: &str, value: u64) {
    let _ = writeln!(
        out,
        "# HELP {name} {help}\n# TYPE {name} gauge\n{name} {value}"
    );
}

fn catalog_counters(out: &mut String, metrics: &CatalogMetrics) {
    for (name, help, value) in [
        (
            "irisd_catalog_commands_coalesced_total",
            "Catalog progress commands replaced before execution.",
            metrics.coalesced(),
        ),
        (
            "irisd_catalog_commands_rejected_total",
            "Catalog commands rejected by bounded admission.",
            metrics.rejected(),
        ),
        (
            "irisd_catalog_events_rejected_total",
            "Catalog event notifications rejected by queue or state-capacity admission.",
            metrics.rejected_events(),
        ),
        (
            "irisd_catalog_progress_rejected_total",
            "Catalog progress notifications rejected by bounded admission.",
            metrics.rejected_progress(),
        ),
        (
            "irisd_catalog_queries_rejected_total",
            "Catalog queries rejected by bounded admission.",
            metrics.rejected_queries(),
        ),
        (
            "irisd_catalog_lifecycle_rejected_total",
            "Catalog lifecycle notifications rejected after exhausting reserved capacity.",
            metrics.rejected_critical(),
        ),
        (
            "irisd_catalog_batch_failures_total",
            "Catalog change batches whose SQLite transaction failed.",
            metrics.failures(),
        ),
    ] {
        counter(out, name, help, value);
    }
}

fn catalog_queue_gauges(out: &mut String, metrics: &CatalogMetrics) {
    for (name, help, value) in [
        (
            "irisd_catalog_queue_commands",
            "Catalog commands admitted but not completed.",
            metrics.queue_depth(),
        ),
        (
            "irisd_catalog_queue_bytes",
            "Estimated owned bytes admitted but not completed by the catalog.",
            metrics.queue_bytes(),
        ),
        (
            "irisd_catalog_queue_peak_commands",
            "Catalog command high-water mark since the daemon statistics line.",
            metrics.queue_peak().get(),
        ),
        (
            "irisd_catalog_queue_peak_bytes",
            "Catalog owned-byte high-water mark since the daemon statistics line.",
            metrics.queue_bytes_peak().get(),
        ),
        (
            "irisd_catalog_reconciliation_required",
            "One when reserved lifecycle admission exhaustion requires filesystem reconciliation.",
            u64::from(metrics.reconciliation_required()),
        ),
    ] {
        gauge(out, name, help, value);
    }
}

fn catalog_stats(out: &mut String, stats: CatalogStats) {
    for (name, help, value) in [
        (
            "irisd_catalog_segments",
            "Segments known to the catalog.",
            stats.segments,
        ),
        (
            "irisd_catalog_sealed_bytes",
            "Bytes of sealed recordings.",
            stats.sealed_bytes,
        ),
        (
            "irisd_catalog_events",
            "Events stored in the catalog.",
            stats.events,
        ),
        (
            "irisd_catalog_event_bytes",
            "Logical bytes owned by stored event strings.",
            stats.event_bytes,
        ),
        (
            "irisd_catalog_retained_events",
            "Events with an active footage-retention mark.",
            stats.retained_events,
        ),
        (
            "irisd_catalog_database_bytes",
            "Bytes in the catalog database file.",
            stats.db_bytes,
        ),
        (
            "irisd_catalog_wal_bytes",
            "Bytes in the catalog write-ahead log file.",
            stats.wal_bytes,
        ),
        (
            "irisd_catalog_deleting_segments",
            "Segments with durable deletion intent awaiting pair removal.",
            stats.deleting_segments,
        ),
        (
            "irisd_catalog_deleting_bytes",
            "Bytes of segments in deletion intent; they still count toward the quota.",
            stats.deleting_bytes,
        ),
    ] {
        gauge(out, name, help, value);
    }
    counter(
        out,
        "irisd_catalog_events_pruned_total",
        "Event rows removed by lifecycle maintenance.",
        stats.pruned_events,
    );
}

pub(crate) async fn metrics(State(state): State<Arc<ApiState>>) -> Response {
    let mut out = String::with_capacity(512);
    counter(
        &mut out,
        "irisd_api_requests_total",
        "Authenticated API requests served.",
        state.stats.requests(),
    );
    let catalog_metrics = state.catalog.metrics();
    catalog_counters(&mut out, &catalog_metrics);
    gauge(
        &mut out,
        "irisd_live_clients",
        "Live WebSocket clients connected.",
        state.stats.live_clients(),
    );
    gauge(
        &mut out,
        "irisd_event_clients",
        "Event WebSocket clients connected.",
        state.stats.event_clients(),
    );
    counter(
        &mut out,
        "irisd_event_websocket_lagged_messages_total",
        "Event messages skipped by lagging WebSocket receivers.",
        state.stats.event_messages_lagged(),
    );
    for (name, help, value) in [
        (
            "irisd_api_media_rejected_total",
            "Media requests answered 503 because their admission class was full.",
            state.stats.media_rejected(),
        ),
        (
            "irisd_api_websocket_rejected_total",
            "Live and event WebSocket upgrades answered 503 by the socket caps.",
            state.stats.websocket_rejected(),
        ),
        (
            "irisd_api_snapshot_rejected_total",
            "Snapshot requests answered 503 while the camera's previous request was in flight.",
            state.stats.snapshot_rejected(),
        ),
        (
            "irisd_api_unapproved_rows_total",
            "Recording rows refused because their path is outside every approved root.",
            state.stats.unapproved_rows(),
        ),
        (
            "irisd_live_oversized_closes_total",
            "Live WebSockets closed with 1009 because a sync frame exceeded the message limit.",
            state.stats.live_oversized_closes(),
        ),
    ] {
        counter(&mut out, name, help, value);
    }
    gauge(
        &mut out,
        "irisd_uptime_seconds",
        "Seconds since the API started.",
        state.started_monotonic.elapsed().as_secs(),
    );
    catalog_queue_gauges(&mut out, &catalog_metrics);
    if let Some(snapshot) = state.stats.catalog_stats(&state.catalog).await {
        catalog_stats(&mut out, snapshot);
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
