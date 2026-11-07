//! Cancellation regressions for the public metrics catalog refresh.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::body::{Body, to_bytes};
use axum::http::Request;
use iris_api::{ApiState, ApiStats, LiveRegistry, router};
use iris_catalog::{CatalogHandle, Change};
use iris_core::event::{Event, EventKind, EventSource, EventState};
use iris_core::id::CameraId;
use iris_core::time::WallUs;
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;
use tower::ServiceExt as _;

fn state(catalog: CatalogHandle) -> Arc<ApiState> {
    let (events, _) = broadcast::channel(1);
    Arc::new(ApiState {
        catalog,
        live: Arc::new(LiveRegistry::new()),
        events,
        tokens: Vec::new(),
        recording_roots: Vec::new(),
        public_metrics: false,
        snapshot_sources: HashMap::new(),
        active_cameras: HashMap::new(),
        stats: Arc::new(ApiStats::default()),
        started: WallUs::now(),
        started_monotonic: Instant::now(),
        shutdown: CancellationToken::new(),
    })
}

async fn metrics_request(app: axum::Router) -> axum::response::Response {
    app.oneshot(
        Request::get("/metrics")
            .body(Body::empty())
            .expect("request"),
    )
    .await
    .expect("router response")
}

#[tokio::test]
async fn cancelled_metrics_leader_keeps_one_cache_owned_refresh() {
    let dir = tempfile::tempdir().expect("tempdir");
    let database = dir.path().join("catalog.db");
    let catalog = CatalogHandle::open(&database).expect("catalog");

    // Hold the worker behind a real SQLite write lock so the stats query stays
    // admitted while request futures are cancelled.
    let lock = rusqlite::Connection::open(&database).expect("lock connection");
    lock.execute_batch("BEGIN IMMEDIATE").expect("write lock");
    catalog.notify(Change::SegmentProgress {
        path: PathBuf::from("/synthetic/missing.mp4"),
        durable_len: 1,
        fragments: 1,
        wall_end: WallUs::new(1),
        media_end: 1,
    });

    let app = router(state(catalog.clone()));
    let metrics = catalog.metrics();
    let initial_depth = metrics.queue_depth();
    let before_queries = metrics.query().snapshot().count();

    let leader = tokio::spawn(metrics_request(app.clone()));
    tokio::time::timeout(Duration::from_secs(1), async {
        while metrics.queue_depth() != initial_depth + 1 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("one catalog refresh admitted");
    leader.abort();
    let _ = leader.await;

    // Every replacement request reaches the same cache-owned in-flight refresh.
    // With request-owned refresh state, cancelling each task enqueues another
    // real catalog command.
    for _ in 0..64 {
        let request = tokio::spawn(metrics_request(app.clone()));
        for _ in 0..4 {
            tokio::task::yield_now().await;
        }
        request.abort();
        let _ = request.await;
    }
    assert_eq!(
        metrics.queue_depth(),
        initial_depth + 1,
        "cancelled public scrapes must retain exactly one catalog refresh"
    );

    lock.execute_batch("COMMIT").expect("release write lock");
    tokio::time::timeout(Duration::from_secs(5), async {
        while metrics.queue_depth() != 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("catalog queue drain");

    let response = metrics_request(app).await;
    let body = to_bytes(response.into_body(), 1 << 20)
        .await
        .expect("metrics body");
    let text = std::str::from_utf8(&body).expect("UTF-8 metrics");
    assert!(text.contains("irisd_catalog_segments"), "{text}");
    assert!(text.contains("irisd_catalog_deleting_segments"), "{text}");
    assert!(text.contains("irisd_catalog_deleting_bytes"), "{text}");
    assert_eq!(
        metrics.query().snapshot().count() - before_queries,
        1,
        "the completed cache-owned refresh must serve the next scrape"
    );

    let before_rejected_events = metrics.rejected_events();
    let before_rejected_progress = metrics.rejected_progress();
    catalog.notify(Change::SegmentProgress {
        path: PathBuf::from("/synthetic/after-refresh.mp4"),
        durable_len: 2,
        fragments: 1,
        wall_end: WallUs::new(2),
        media_end: 2,
    });
    catalog.notify(Change::Event(Event {
        camera: CameraId::try_new("cam1").expect("camera id"),
        source: EventSource::DahuaCgi,
        kind: EventKind::Motion,
        state: EventState::Start,
        nvr_time: WallUs::new(3),
        camera_time: None,
        data: None,
    }));
    assert_eq!(metrics.rejected_events(), before_rejected_events);
    assert_eq!(metrics.rejected_progress(), before_rejected_progress);
    assert_eq!(catalog.stats().await.expect("post-refresh query").events, 1);
    catalog.shutdown();
}
