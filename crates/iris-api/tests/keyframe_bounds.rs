//! Extreme request timestamps must fail before constructing a keyframe lookup range.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use iris_api::{ApiState, ApiStats, LiveRegistry, router};
use iris_catalog::CatalogHandle;
use iris_core::time::WallUs;
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;
use tower::ServiceExt as _;

async fn keyframe_status_at(t: i64) -> StatusCode {
    let dir = tempfile::tempdir().unwrap();
    let catalog = CatalogHandle::open(&dir.path().join("catalog.db")).unwrap();
    let (events, _) = broadcast::channel(16);
    let app = router(Arc::new(ApiState {
        catalog: catalog.clone(),
        live: Arc::new(LiveRegistry::new()),
        events,
        tokens: vec![],
        snapshot_sources: HashMap::new(),
        active_cameras: HashMap::new(),
        stats: Arc::new(ApiStats::default()),
        started: WallUs::now(),
        started_monotonic: Instant::now(),
        shutdown: CancellationToken::new(),
    }));
    let response = app
        .oneshot(
            Request::get(format!("/api/streams/cam1/main/keyframe?t={t}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    catalog.shutdown();
    response.status()
}

#[tokio::test]
async fn keyframe_rejects_a_timestamp_whose_lookback_underflows() {
    assert_eq!(keyframe_status_at(i64::MIN).await, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn keyframe_rejects_a_timestamp_whose_exclusive_end_overflows() {
    assert_eq!(keyframe_status_at(i64::MAX).await, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn keyframe_accepts_timestamps_with_representable_boundary_ranges() {
    assert_eq!(
        keyframe_status_at(i64::MIN + 120_000_000).await,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        keyframe_status_at(i64::MAX - 1).await,
        StatusCode::NOT_FOUND
    );
}
