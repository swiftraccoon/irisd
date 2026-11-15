//! End-to-end tests over the router with a real catalog and recording in a
//! temporary directory.

#![expect(
    clippy::missing_assert_message,
    clippy::unwrap_used,
    reason = "a helper of a test fails as the test does, by panicking"
)]

use std::collections::HashMap;
use std::io::{Read as _, Seek as _, SeekFrom, Write as _};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode, header};
use bytes::Bytes;
use futures_util::StreamExt as _;
use futures_util::future::join_all;
use iris_api::{ActiveCamera, ApiState, ApiStats, LiveRegistry, SnapshotSource, router};
use iris_catalog::{CatalogHandle, Change};
use iris_core::event::{Event, EventKind, EventSource, EventState};
use iris_core::id::{CameraId, StreamKey, StreamKind};
use iris_core::time::WallUs;
use iris_ingest::{LiveFrame, LiveRing};
use iris_mp4::fragment::{Fragment, FrameSteps, Sample};
use iris_mp4::index::{MAX_PERSISTED_INDEX_RECORDS, read_index_bounded};
use iris_mp4::init::SampleEntry;
use iris_store::{ApprovedRoot, RootExpectation, StoragePolicy, WriterHandle};
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;
use tower::ServiceExt as _;

fn entry() -> Arc<SampleEntry> {
    Arc::new(SampleEntry::new(
        Bytes::from_static(b"\0\0\0\x10hvc1\0\0\0\0\0\0\0\0"),
        "hvc1.1.6.L150.B0".into(),
        640,
        480,
    ))
}

fn nal(header: u8, filler: u8, body: usize) -> Vec<u8> {
    let len = u32::try_from(2 + body).unwrap();
    let mut v = len.to_be_bytes().to_vec();
    v.extend_from_slice(&[header, 1]);
    v.resize(4 + 2 + body, filler);
    v
}

fn key() -> StreamKey {
    StreamKey {
        camera: CameraId::try_new("cam1").unwrap(),
        kind: StreamKind::Main,
    }
}

/// Wall time of fragment `index`: a fixed epoch plus two seconds per fragment.
fn wall(index: u64) -> WallUs {
    WallUs::new(1_700_000_000_000_000 + i64::try_from(index).unwrap() * 2_000_000)
}

fn fragment(index: u64, entry: &Arc<SampleEntry>) -> Fragment {
    Fragment {
        samples: vec![
            Sample {
                data: Bytes::from(nal(0x26, 0xaa, 500)),
                duration: 90_000,
                is_sync: true,
            },
            Sample {
                data: Bytes::from(nal(0x02, 0xbb, 300)),
                duration: 90_000,
                is_sync: false,
            },
        ],
        decode_time: index * 180_000,
        wall: wall(index),
        raw_rtp: 42,
        new_run: index == 0,
        steps: FrameSteps::default(),
        sample_entry: Arc::clone(entry),
    }
}

fn find_mp4(root: &Path) -> PathBuf {
    let mut found = find_mp4s(root);
    assert_eq!(found.len(), 1, "expected one recording, found {found:?}");
    found.remove(0)
}

fn find_mp4s(root: &Path) -> Vec<PathBuf> {
    fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                walk(&path, out);
            } else if path.extension().is_some_and(|e| e == "mp4") {
                out.push(path);
            }
        }
    }
    let mut found = Vec::new();
    walk(root, &mut found);
    found.sort();
    found
}

/// The test recording root `dir/video`, given a fresh identity marker on first
/// use and admitted again by later calls.
fn recording_root(dir: &Path) -> ApprovedRoot {
    iris_store::admit_recording_root(&dir.join("video"), RootExpectation::Fresh).unwrap()
}

/// Records synthetic fragments beneath the recording root of `dir`, then
/// registers every sealed file from its sidecar.
async fn record_segments(
    dir: &Path,
    catalog: &CatalogHandle,
    fragments: Vec<Fragment>,
) -> Vec<PathBuf> {
    record_segments_with_rotation(dir, catalog, fragments, 1 << 30).await
}

async fn record_segments_with_rotation(
    dir: &Path,
    catalog: &CatalogHandle,
    fragments: Vec<Fragment>,
    rotate_bytes: u64,
) -> Vec<PathBuf> {
    let root = recording_root(dir);
    let root = root.path();
    let entries: HashMap<u32, Arc<SampleEntry>> = fragments
        .iter()
        .map(|fragment| {
            (
                fragment.sample_entry.hash,
                Arc::clone(&fragment.sample_entry),
            )
        })
        .collect();
    let writer = WriterHandle::spawn(
        root.to_owned(),
        StoragePolicy {
            sync_interval: Duration::from_secs(5),
            preallocate_bytes: 1 << 20,
            rotate_bytes,
            rotate_age: Duration::from_secs(3600),
        },
        16,
    )
    .unwrap();
    for fragment in fragments {
        writer.try_send(key(), fragment).unwrap();
    }
    tokio::task::spawn_blocking(move || writer.shutdown())
        .await
        .unwrap()
        .unwrap();
    let files = find_mp4s(root);
    for path in &files {
        let (header, records) = read_index_bounded(
            &iris_store::layout::index_path(path),
            MAX_PERSISTED_INDEX_RECORDS,
        )
        .unwrap();
        catalog.notify(Change::SegmentOpened {
            key: key(),
            path: path.to_owned(),
            run_seq: header.run_id,
            discontinuity: records[0].flags & iris_mp4::index::FLAG_DISCONTINUITY_BEFORE != 0,
            wall_start: records[0].wall,
            media_start: records[0].decode_time,
            init_len: header.init_len,
            sample_entry: Arc::clone(&entries[&header.sample_entry_hash]),
        });
        let last = records.last().unwrap();
        catalog.notify(Change::SegmentSealed {
            path: path.to_owned(),
            len: last.end(),
            fragments: u32::try_from(records.len()).unwrap(),
            wall_end: last
                .wall
                .offset(iris_core::time::MediaTicks::new(i64::from(last.duration)).as_micros()),
            media_end: last.decode_time + u64::from(last.duration),
        });
    }
    // A query flushes all preceding catalog changes before returning.
    catalog.total_bytes().await.unwrap();
    files
}

/// Records three fragments beneath the recording root of `dir` and registers
/// the file with the catalog.
async fn populate(dir: &Path, catalog: &CatalogHandle) -> (PathBuf, u32, u64) {
    let root = recording_root(dir);
    let root = root.path();
    let entry = entry();
    let writer = WriterHandle::spawn(
        root.to_path_buf(),
        StoragePolicy {
            sync_interval: Duration::from_millis(20),
            preallocate_bytes: 1 << 20,
            rotate_bytes: 1 << 30,
            rotate_age: Duration::from_secs(3600),
        },
        16,
    )
    .unwrap();
    for i in 0..3 {
        writer.try_send(key(), fragment(i, &entry)).unwrap();
    }
    tokio::task::spawn_blocking(move || writer.shutdown())
        .await
        .unwrap()
        .unwrap();
    let path = find_mp4(root);
    let (header, records) = read_index_bounded(
        &iris_store::layout::index_path(&path),
        MAX_PERSISTED_INDEX_RECORDS,
    )
    .unwrap();
    assert_eq!(records.len(), 3);
    let total: u64 = records.iter().map(|r| u64::from(r.len)).sum();
    let len = u64::from(header.init_len) + total;
    catalog.notify(Change::SegmentOpened {
        key: key(),
        path: path.clone(),
        run_seq: 1,
        discontinuity: true,
        wall_start: records[0].wall,
        media_start: records[0].decode_time,
        init_len: header.init_len,
        sample_entry: entry,
    });
    let last = records.last().unwrap();
    catalog.notify(Change::SegmentSealed {
        path: path.clone(),
        len,
        fragments: 3,
        wall_end: last.wall.offset(2_000_000),
        media_end: last.decode_time + u64::from(last.duration),
    });
    // A query flushes all preceding catalog changes before returning.
    catalog.total_bytes().await.unwrap();
    (path, header.init_len, len)
}

/// API state serving the recording root of `dir`.
fn state(dir: &Path, catalog: CatalogHandle, tokens: Vec<String>) -> Arc<ApiState> {
    state_with_cameras(dir, catalog, tokens, HashMap::new())
}

fn state_with_cameras(
    dir: &Path,
    catalog: CatalogHandle,
    tokens: Vec<String>,
    active_cameras: HashMap<CameraId, ActiveCamera>,
) -> Arc<ApiState> {
    let (events, _) = broadcast::channel(16);
    Arc::new(ApiState {
        catalog,
        live: Arc::new(LiveRegistry::new()),
        events,
        tokens,
        capability_secret: iris_api::CapabilitySecret::new([1; 32]),
        recording_roots: vec![recording_root(dir)],
        public_metrics: false,
        snapshot_sources: std::sync::RwLock::new(HashMap::new()),
        active_cameras: std::sync::RwLock::new(active_cameras),
        control: None,
        stats: Arc::new(ApiStats::default()),
        started: WallUs::now(),
        started_monotonic: Instant::now(),
        shutdown: CancellationToken::new(),
    })
}

#[tokio::test]
async fn event_websocket_delivers_normally_then_closes_explicitly_on_lag() {
    let dir = tempfile::tempdir().unwrap();
    let catalog = CatalogHandle::open(&dir.path().join("c.db")).unwrap();
    let state = state(dir.path(), catalog.clone(), Vec::new());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server_state = state.clone();
    let server = tokio::spawn(async move {
        axum::serve(listener, router(server_state)).await.unwrap();
    });
    let (mut socket, _) = tokio_tungstenite::connect_async(format!("ws://{address}/api/events"))
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(1), async {
        while state.stats.event_clients() != 1 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();

    let event = Arc::new(Event {
        camera: CameraId::try_new("cam1").unwrap(),
        source: EventSource::DahuaCgi,
        kind: EventKind::Motion,
        state: EventState::Start,
        nvr_time: WallUs::new(1),
        camera_time: None,
        data: Some("{}".to_owned()),
    });
    state.events.send(event.clone()).unwrap();
    let normal = tokio::time::timeout(Duration::from_secs(1), socket.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(normal.is_text());

    // This synchronous burst cannot yield to the receiver and exceeds its
    // bounded channel, deterministically exercising the actual push loop.
    for _ in 0..64 {
        state.events.send(event.clone()).unwrap();
    }
    let close = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let message = socket.next().await.unwrap().unwrap();
            if let tokio_tungstenite::tungstenite::Message::Close(frame) = message {
                break frame.unwrap();
            }
        }
    })
    .await
    .unwrap();
    assert_eq!(
        close.code,
        tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode::Again
    );
    assert!(close.reason.contains("event gap"));
    assert!(close.reason.contains("reconnect"));
    assert!(state.stats.event_messages_lagged() > 0);
    tokio::time::timeout(Duration::from_secs(1), async {
        while state.stats.event_clients() != 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();

    server.abort();
    catalog.shutdown();
}

async fn body_string(response: axum::response::Response) -> String {
    String::from_utf8(
        to_bytes(response.into_body(), 64 << 20)
            .await
            .unwrap()
            .to_vec(),
    )
    .unwrap()
}

#[tokio::test]
async fn health_reports_ok_without_a_token() {
    let dir = tempfile::tempdir().unwrap();
    let catalog = CatalogHandle::open(&dir.path().join("c.db")).unwrap();
    let state = state(dir.path(), catalog.clone(), vec!["secret".into()]);
    let app = router(Arc::clone(&state));
    let response = app
        .oneshot(Request::get("/api/health").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let text = body_string(response).await;
    assert!(text.contains("\"status\":\"ok\""), "{text}");
    assert!(text.contains(env!("CARGO_PKG_VERSION")));
    assert_eq!(state.stats.requests(), 0, "public requests are not counted");
    catalog.shutdown();
}

#[tokio::test]
async fn metrics_expose_catalog_queue_and_event_state() {
    let dir = tempfile::tempdir().unwrap();
    let catalog = CatalogHandle::open(&dir.path().join("c.db")).unwrap();
    let app = router(state(dir.path(), catalog.clone(), vec!["secret".into()]));
    let response = app
        .oneshot(
            Request::get("/metrics")
                .header(header::AUTHORIZATION, "Bearer secret")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let text = body_string(response).await;
    for name in [
        "irisd_api_media_rejected_total",
        "irisd_api_websocket_rejected_total",
        "irisd_api_snapshot_rejected_total",
        "irisd_api_unapproved_rows_total",
        "irisd_live_oversized_closes_total",
        "irisd_catalog_queue_commands",
        "irisd_catalog_queue_bytes",
        "irisd_catalog_commands_rejected_total",
        "irisd_catalog_batch_failures_total",
        "irisd_catalog_reconciliation_required",
        "irisd_catalog_event_bytes",
        "irisd_catalog_retained_events",
        "irisd_catalog_events_pruned_total",
        "irisd_event_websocket_lagged_messages_total",
        "irisd_catalog_database_bytes",
        "irisd_catalog_wal_bytes",
    ] {
        assert!(text.contains(name), "missing {name} in {text}");
    }
    catalog.shutdown();
}

#[tokio::test]
async fn concurrent_metrics_scrapes_share_one_catalog_stats_query() {
    let dir = tempfile::tempdir().unwrap();
    let catalog = CatalogHandle::open(&dir.path().join("c.db")).unwrap();
    let app = router(state(dir.path(), catalog.clone(), vec!["secret".into()]));
    let before = catalog.metrics().query().snapshot().count();
    let responses = join_all((0..64).map(|_| {
        let app = app.clone();
        async move {
            app.oneshot(
                Request::get("/metrics")
                    .header(header::AUTHORIZATION, "Bearer secret")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap()
        }
    }))
    .await;
    assert!(
        responses
            .iter()
            .all(|response| response.status() == StatusCode::OK)
    );
    let after = catalog.metrics().query().snapshot().count();
    assert_eq!(after - before, 1, "scrapes must coalesce one stats refresh");
    catalog.shutdown();
}

#[tokio::test]
async fn metrics_require_the_root_token_unless_configured_public() {
    let dir = tempfile::tempdir().unwrap();
    let catalog = CatalogHandle::open(&dir.path().join("c.db")).unwrap();
    let private = state(dir.path(), catalog.clone(), vec!["secret".into()]);
    let app = router(Arc::clone(&private));
    let anonymous = app
        .clone()
        .oneshot(Request::get("/metrics").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(anonymous.status(), StatusCode::UNAUTHORIZED);
    let wrong = app
        .clone()
        .oneshot(
            Request::get("/metrics")
                .header(header::AUTHORIZATION, "Bearer nope")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(wrong.status(), StatusCode::UNAUTHORIZED);
    let root = app
        .clone()
        .oneshot(
            Request::get("/metrics")
                .header(header::AUTHORIZATION, "Bearer secret")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(root.status(), StatusCode::OK);

    // A media capability minted for a playable path never authorizes metrics.
    populate(dir.path(), &catalog).await;
    let playlist = get_with_token(
        &app,
        "secret",
        format!(
            "/api/streams/cam1/main/hls/vod.m3u8?from={}&to={}",
            wall(0).get(),
            wall(3).get()
        ),
    )
    .await;
    let text = body_string(playlist).await;
    let capability = text
        .split("?token=")
        .nth(1)
        .and_then(|tail| tail.split(['"', '\n']).next())
        .expect("playlist capability");
    let with_capability = app
        .clone()
        .oneshot(
            Request::get(format!("/metrics?token={capability}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(with_capability.status(), StatusCode::UNAUTHORIZED);

    let mut public = state(dir.path(), catalog.clone(), vec!["secret".into()]);
    Arc::get_mut(&mut public)
        .expect("sole state owner")
        .public_metrics = true;
    let public_app = router(public);
    let anonymous = public_app
        .clone()
        .oneshot(Request::get("/metrics").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(anonymous.status(), StatusCode::OK);
    assert_eq!(
        public_app
            .oneshot(Request::get("/api/cameras").body(Body::empty()).unwrap())
            .await
            .unwrap()
            .status(),
        StatusCode::UNAUTHORIZED,
        "public metrics leave every other route protected"
    );
    catalog.shutdown();
}

#[tokio::test]
async fn metrics_serve_the_last_snapshot_when_refresh_fails() {
    let dir = tempfile::tempdir().unwrap();
    let catalog = CatalogHandle::open(&dir.path().join("c.db")).unwrap();
    let app = router(state(dir.path(), catalog.clone(), Vec::new()));
    let first = app
        .clone()
        .oneshot(Request::get("/metrics").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert!(body_string(first).await.contains("irisd_catalog_segments"));
    catalog.shutdown();
    tokio::time::sleep(Duration::from_millis(1_100)).await;
    let stale = app
        .oneshot(Request::get("/metrics").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(stale.status(), StatusCode::OK);
    assert!(body_string(stale).await.contains("irisd_catalog_segments"));
}

#[tokio::test]
async fn metrics_cold_failure_is_coalesced_for_the_refresh_cadence() {
    let dir = tempfile::tempdir().unwrap();
    let catalog = CatalogHandle::open(&dir.path().join("c.db")).unwrap();
    let app = router(state(dir.path(), catalog.clone(), Vec::new()));
    catalog.shutdown();
    let before = catalog.metrics().query().snapshot().count();
    let responses = join_all((0..64).map(|_| {
        let app = app.clone();
        async move {
            app.oneshot(Request::get("/metrics").body(Body::empty()).unwrap())
                .await
                .unwrap()
        }
    }))
    .await;
    assert!(
        responses
            .iter()
            .all(|response| response.status() == StatusCode::OK)
    );
    let after = catalog.metrics().query().snapshot().count();
    assert!(after.saturating_sub(before) <= 1);
    let text = body_string(responses.into_iter().next().unwrap()).await;
    assert!(text.contains("irisd_api_requests_total"));
    assert!(!text.contains("irisd_catalog_segments"));
}

#[tokio::test]
async fn health_uptime_ignores_the_wall_clock_start_value() {
    let dir = tempfile::tempdir().unwrap();
    let catalog = CatalogHandle::open(&dir.path().join("c.db")).unwrap();
    let mut state = state(dir.path(), catalog.clone(), Vec::new());
    let state_mut = Arc::get_mut(&mut state).expect("sole state owner");
    state_mut.started = WallUs::new(i64::MAX);
    state_mut.started_monotonic = Instant::now().checked_sub(Duration::from_secs(2)).unwrap();
    let response = router(state)
        .oneshot(Request::get("/api/health").body(Body::empty()).unwrap())
        .await
        .unwrap();
    let body: serde_json::Value = serde_json::from_str(&body_string(response).await).unwrap();
    assert!(body["uptime_s"].as_i64().unwrap() >= 2);
    catalog.shutdown();
}

#[tokio::test]
async fn rejects_missing_or_wrong_tokens_and_accepts_valid_ones() {
    let dir = tempfile::tempdir().unwrap();
    let catalog = CatalogHandle::open(&dir.path().join("c.db")).unwrap();
    let state = state(dir.path(), catalog.clone(), vec!["secret".into()]);
    let app = router(Arc::clone(&state));
    let missing = app
        .clone()
        .oneshot(Request::get("/api/cameras").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(missing.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(state.stats.requests(), 0);
    let wrong = app
        .clone()
        .oneshot(
            Request::get("/api/cameras")
                .header(header::AUTHORIZATION, "Bearer nope")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(wrong.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(state.stats.requests(), 0);
    let via_header = app
        .clone()
        .oneshot(
            Request::get("/api/cameras")
                .header(header::AUTHORIZATION, "Bearer secret")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(via_header.status(), StatusCode::OK);
    assert_eq!(state.stats.requests(), 1);
    let via_query = app
        .oneshot(
            Request::get("/api/cameras?token=secret")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(via_query.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(state.stats.requests(), 1);
    catalog.shutdown();
}

#[tokio::test]
async fn blank_configured_tokens_fail_closed_even_when_state_is_constructed_directly() {
    let dir = tempfile::tempdir().unwrap();
    let catalog = CatalogHandle::open(&dir.path().join("c.db")).unwrap();
    for tokens in [
        vec![String::new()],
        vec!["   ".to_owned()],
        vec!["valid".to_owned(), String::new()],
    ] {
        let app = router(state(dir.path(), catalog.clone(), tokens));
        for authorization in [None, Some("Bearer valid")] {
            let mut request = Request::get("/api/cameras?token=");
            if let Some(authorization) = authorization {
                request = request.header(header::AUTHORIZATION, authorization);
            }
            let response = app
                .clone()
                .oneshot(request.body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        }
    }
    catalog.shutdown();
}

async fn get_with_token(app: &axum::Router, token: &str, uri: String) -> axum::response::Response {
    app.clone()
        .oneshot(
            Request::get(uri)
                .header(header::AUTHORIZATION, format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap()
}

async fn get(app: &axum::Router, uri: String, range: Option<String>) -> axum::response::Response {
    let mut request = Request::get(uri).header(header::AUTHORIZATION, "Bearer t");
    if let Some(range) = range {
        request = request.header(header::RANGE, range);
    }
    app.clone()
        .oneshot(request.body(Body::empty()).unwrap())
        .await
        .unwrap()
}

async fn get_with_capability(
    app: &axum::Router,
    uri: String,
    range: Option<String>,
) -> axum::response::Response {
    let mut request = Request::get(uri);
    if let Some(range) = range {
        request = request.header(header::RANGE, range);
    }
    app.clone()
        .oneshot(request.body(Body::empty()).unwrap())
        .await
        .unwrap()
}

fn quoted_uri(line: &str) -> &str {
    let start = line.find("URI=\"").expect("playlist URI") + 5;
    let tail = &line[start..];
    &tail[..tail.find('"').expect("closing playlist URI quote")]
}

async fn assert_capability_is_path_scoped(
    app: &axum::Router,
    capability: &str,
    from: i64,
    to: i64,
    segment_id: i64,
) {
    for denied in [
        format!("/api/cameras?token={capability}"),
        format!("/api/cameras/cam1/snapshot.jpg?token={capability}"),
        format!("/api/streams/cam1/main/export.mp4?from={from}&to={to}&token={capability}"),
        format!("/api/streams/cam2/main/hls/vod.m3u8?from={from}&to={to}&token={capability}"),
        format!(
            "/api/segments/{}/file.mp4?token={capability}",
            segment_id + 1
        ),
    ] {
        let response = get_with_capability(app, denied, None).await;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }
    let retain = app
        .clone()
        .oneshot(
            Request::post(format!("/api/events/1/retain?token={capability}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(retain.status(), StatusCode::UNAUTHORIZED);

    let mut tampered = capability.as_bytes().to_vec();
    let tail_byte = tampered.last_mut().unwrap();
    *tail_byte = if *tail_byte == b'0' { b'1' } else { b'0' };
    let tampered = String::from_utf8(tampered).unwrap();
    let response = get_with_capability(
        app,
        format!("/api/segments/{segment_id}/file.mp4?token={tampered}"),
        None,
    )
    .await;
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn lists_segments_and_builds_a_byte_range_playlist() {
    let dir = tempfile::tempdir().unwrap();
    let catalog = CatalogHandle::open(&dir.path().join("c.db")).unwrap();
    catalog
        .ensure_camera(&key().camera, "Example", Some("dahua"))
        .await
        .unwrap();
    let (_, init_len, len) = populate(dir.path(), &catalog).await;
    let app = router(state(dir.path(), catalog.clone(), vec!["t".into()]));
    let (from, to) = (wall(0).get(), wall(3).get());

    let list = get(
        &app,
        format!("/api/streams/cam1/main/segments?from={from}&to={to}"),
        None,
    )
    .await;
    assert_eq!(list.status(), StatusCode::OK);
    let rows: serde_json::Value = serde_json::from_str(&body_string(list).await).unwrap();
    let segment_id = rows[0]["id"].as_i64().unwrap();
    assert_eq!(rows[0]["fragments"], 3);
    assert_eq!(rows[0]["sealed"], true);

    let playlist = get(
        &app,
        format!("/api/streams/cam1/main/hls/vod.m3u8?from={from}&to={to}"),
        None,
    )
    .await;
    assert_eq!(playlist.status(), StatusCode::OK);
    assert_eq!(
        playlist.headers()[header::CONTENT_TYPE],
        "application/vnd.apple.mpegurl"
    );
    let text = body_string(playlist).await;
    let map_prefix = format!("#EXT-X-MAP:URI=\"/api/segments/{segment_id}/file.mp4?token=");
    let map_line = text
        .lines()
        .find(|line| line.starts_with(&map_prefix))
        .unwrap();
    assert!(
        !map_line.contains("token=t"),
        "root token leaked in {map_line}"
    );
    let media_uri = map_line
        .strip_prefix("#EXT-X-MAP:URI=\"")
        .and_then(|line| line.split_once('\"'))
        .map(|(uri, _)| uri)
        .unwrap();
    assert!(media_uri.contains("token=v1."), "{media_uri}");
    let media = get_with_capability(
        &app,
        media_uri.to_owned(),
        Some(format!("bytes=0-{}", init_len - 1)),
    )
    .await;
    assert_eq!(media.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(media.headers()[header::CACHE_CONTROL], "private, no-store");
    let capability = media_uri.split_once("?token=").unwrap().1;
    assert_capability_is_path_scoped(&app, capability, from, to, segment_id).await;
    assert!(
        text.contains(&format!(
            "#EXT-X-BYTERANGE:{}@{init_len}",
            len - u64::from(init_len)
        )),
        "{text}"
    );
    assert!(text.contains("#EXTINF:6.00000,"), "{text}");
    assert!(text.ends_with("#EXT-X-ENDLIST\n"));
    assert!(
        !text.contains("#EXT-X-DISCONTINUITY"),
        "a first segment never starts with a discontinuity"
    );

    let half = get(
        &app,
        format!(
            "/api/streams/cam1/main/hls/vod.m3u8?from={}&to={to}",
            wall(2).get()
        ),
        None,
    )
    .await;
    let half_text = body_string(half).await;
    assert_eq!(
        half_text.matches("#EXT-X-BYTERANGE").count(),
        1,
        "range clipping keeps only the last fragment: {half_text}"
    );
    catalog.shutdown();
}

#[tokio::test]
async fn serves_ranges_and_streams_an_export_over_a_real_recording() {
    let dir = tempfile::tempdir().unwrap();
    let catalog = CatalogHandle::open(&dir.path().join("c.db")).unwrap();
    let (path, init_len, len) = populate(dir.path(), &catalog).await;
    let segment_id = catalog
        .segment_by_path(&path)
        .await
        .unwrap()
        .expect("registered")
        .id
        .0;
    let app = router(state(dir.path(), catalog.clone(), vec!["t".into()]));
    let (from, to) = (wall(0).get(), wall(3).get());

    let ranged = get(
        &app,
        format!("/api/segments/{segment_id}/file.mp4"),
        Some(format!("bytes=0-{}", init_len - 1)),
    )
    .await;
    assert_eq!(ranged.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(ranged.headers()[header::CONTENT_TYPE], "video/mp4");
    let init = to_bytes(ranged.into_body(), 1 << 20).await.unwrap();
    assert_eq!(init.len(), init_len as usize);
    assert_eq!(&init[4..8], b"ftyp");

    // A HEAD names the file's length and sends none of it.
    let head = app
        .clone()
        .oneshot(
            Request::head(format!("/api/segments/{segment_id}/file.mp4"))
                .header(header::AUTHORIZATION, "Bearer t")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(head.status(), StatusCode::OK);
    assert_eq!(head.headers()[header::CONTENT_LENGTH], len.to_string());
    assert_eq!(head.headers()[header::ACCEPT_RANGES], "bytes");
    assert!(
        to_bytes(head.into_body(), 1 << 20)
            .await
            .unwrap()
            .is_empty()
    );

    let export = get(
        &app,
        format!("/api/streams/cam1/main/export.mp4?from={from}&to={to}"),
        None,
    )
    .await;
    assert_eq!(export.status(), StatusCode::OK);
    assert_eq!(export.headers()[header::CACHE_CONTROL], "private, no-store");
    let disposition = export.headers()[header::CONTENT_DISPOSITION]
        .to_str()
        .unwrap()
        .to_owned();
    assert!(
        disposition.starts_with("attachment; filename=\"cam1-main-"),
        "{disposition}"
    );
    let bytes = to_bytes(export.into_body(), 64 << 20).await.unwrap();
    // The edit list adds 44 bytes to the init segment; fragments keep their length.
    assert_eq!(bytes.len() as u64, len + 44);
    assert_eq!(&bytes[4..8], b"ftyp");
    // The export is a stand-alone recording: a continuous timeline from zero,
    // sequence numbers from one, trailers that verify.
    let report = iris_mp4::reader::scan(
        std::io::Cursor::new(bytes.to_vec()),
        iris_mp4::reader::ScanOptions {
            verify_crc: true,
            collect_samples: false,
        },
    )
    .unwrap();
    assert_eq!(report.stop, None);
    assert_eq!(report.init_len as usize, init_len as usize + 44);
    let times: Vec<u64> = report.fragments.iter().map(|f| f.decode_time).collect();
    assert_eq!(times, vec![0, 180_000, 360_000]);
    let sequences: Vec<u32> = report.fragments.iter().map(|f| f.sequence).collect();
    assert_eq!(sequences, vec![1, 2, 3]);
    assert_eq!(report.fragments[1].wall, Some(wall(1)));
    // An export starting half a second in and ending half a second early presents
    // from media time 45,000 ticks for five seconds.
    let (from, to) = (
        wall(0).offset(500_000).get(),
        wall(2).offset(1_500_000).get(),
    );
    let export = get(
        &app,
        format!("/api/streams/cam1/main/export.mp4?from={from}&to={to}"),
        None,
    )
    .await;
    let bytes = to_bytes(export.into_body(), 64 << 20).await.unwrap();
    let elst = bytes
        .windows(4)
        .position(|w| w == b"elst")
        .expect("edit list present");
    let body = &bytes[elst + 4..];
    assert_eq!(body[0], 1, "elst version 1");
    assert_eq!(u64::from_be_bytes(body[8..16].try_into().unwrap()), 5_000);
    assert_eq!(u64::from_be_bytes(body[16..24].try_into().unwrap()), 45_000);
    catalog.shutdown();
}

#[tokio::test]
async fn raw_file_responses_stop_at_the_snapshotted_complete_extent() {
    let dir = tempfile::tempdir().unwrap();
    let catalog = CatalogHandle::open(&dir.path().join("c.db")).unwrap();
    let (path, _, published_len) = populate(dir.path(), &catalog).await;
    let segment_id = catalog
        .segment_by_path(&path)
        .await
        .unwrap()
        .expect("registered")
        .id
        .0;
    let mut media = std::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap();
    media.write_all(b"partial-fragment").unwrap();
    media.flush().unwrap();
    let app = router(state(dir.path(), catalog.clone(), Vec::new()));

    let full = get(&app, format!("/api/segments/{segment_id}/file.mp4"), None).await;
    assert_eq!(full.status(), StatusCode::OK);
    assert_eq!(
        full.headers()[header::CONTENT_LENGTH],
        published_len.to_string()
    );
    assert_eq!(full.headers()[header::CACHE_CONTROL], "private, no-store");
    // Appending after response construction cannot extend its body.
    media.write_all(b"later-append").unwrap();
    media.flush().unwrap();
    let bytes = to_bytes(full.into_body(), usize::try_from(published_len).unwrap())
        .await
        .unwrap();
    assert_eq!(bytes.len() as u64, published_len);

    let crossing = get(
        &app,
        format!("/api/segments/{segment_id}/file.mp4"),
        Some(format!(
            "bytes={}-{}",
            published_len - 9,
            published_len + 100
        )),
    )
    .await;
    assert_eq!(crossing.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(
        crossing.headers()[header::CONTENT_RANGE],
        format!(
            "bytes {}-{}/{published_len}",
            published_len - 9,
            published_len - 1
        )
    );
    assert_eq!(to_bytes(crossing.into_body(), 16).await.unwrap().len(), 9);

    let after = get(
        &app,
        format!("/api/segments/{segment_id}/file.mp4"),
        Some(format!("bytes={published_len}-")),
    )
    .await;
    assert_eq!(after.status(), StatusCode::RANGE_NOT_SATISFIABLE);
    assert_eq!(
        after.headers()[header::CONTENT_RANGE],
        format!("bytes */{published_len}")
    );
    catalog.shutdown();
}

#[tokio::test]
async fn slow_export_bodies_hold_process_wide_media_admission() {
    let dir = tempfile::tempdir().unwrap();
    let catalog = CatalogHandle::open(&dir.path().join("c.db")).unwrap();
    populate(dir.path(), &catalog).await;
    let app = router(state(dir.path(), catalog.clone(), Vec::new()));
    let uri = format!(
        "/api/streams/cam1/main/export.mp4?from={}&to={}",
        wall(0).get(),
        wall(3).get()
    );

    let mut held = Vec::new();
    for _ in 0..8 {
        let response = get(&app, uri.clone(), None).await;
        assert_eq!(response.status(), StatusCode::OK);
        held.push(response);
    }
    let rejected = get(&app, uri.clone(), None).await;
    assert_eq!(rejected.status(), StatusCode::SERVICE_UNAVAILABLE);

    drop(held.pop());
    let admitted = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let response = get(&app, uri.clone(), None).await;
            if response.status() == StatusCode::OK {
                break response;
            }
            assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("detached export producer released admission after body drop");
    assert_eq!(admitted.status(), StatusCode::OK);
    drop(admitted);
    drop(held);
    catalog.shutdown();
}

#[tokio::test]
async fn exports_round_positive_tail_and_presentation_durations_up_to_their_timescales() {
    let dir = tempfile::tempdir().unwrap();
    let catalog = CatalogHandle::open(&dir.path().join("c.db")).unwrap();
    let entry = entry();
    record_segments(
        dir.path(),
        &catalog,
        vec![fragment(0, &entry), fragment(1, &entry)],
    )
    .await;
    let app = router(state(dir.path(), catalog.clone(), Vec::new()));
    let mut cases: Vec<(i64, i64, Vec<u32>)> = (1..=11).map(|end| (0, end, vec![1])).collect();
    cases.extend([
        (0, 12, vec![2]),
        (0, 999_999, vec![90_000]),
        (0, 1_000_000, vec![90_000]),
        (0, 1_000_001, vec![90_000, 1]),
        (0, 1_999_999, vec![90_000, 90_000]),
        (0, 2_000_000, vec![90_000, 90_000]),
        (0, 2_000_001, vec![90_000, 90_000, 1]),
        (2_000_000, 2_000_001, vec![1]),
        (11, 12, vec![2]),
        (12, 13, vec![2]),
        (999_999, 1_000_000, vec![90_000]),
        (1_999_999, 2_000_000, vec![90_000, 90_000]),
    ]);
    for (start, end, expected_durations) in cases {
        let response = get(
            &app,
            format!(
                "/api/streams/cam1/main/export.mp4?from={}&to={}",
                wall(0).offset(start).get(),
                wall(0).offset(end).get()
            ),
            None,
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK, "range {start}..{end}");
        let bytes = to_bytes(response.into_body(), 64 << 20)
            .await
            .unwrap_or_else(|error| panic!("export {start}..{end} failed: {error}"));
        let report = iris_mp4::reader::scan(
            std::io::Cursor::new(bytes.to_vec()),
            iris_mp4::reader::ScanOptions {
                verify_crc: true,
                collect_samples: true,
            },
        )
        .unwrap();
        assert_eq!(report.stop, None, "range {start}..{end}");
        let durations: Vec<u32> = report
            .fragments
            .iter()
            .flat_map(|fragment| fragment.sample_durations.iter().copied())
            .collect();
        assert_eq!(durations, expected_durations, "range {start}..{end}");
        assert_eq!(report.fragments[0].decode_time, 0);

        let elst = bytes.windows(4).position(|kind| kind == b"elst").unwrap();
        let body = &bytes[elst + 4..];
        let duration_ms = u64::from_be_bytes(body[8..16].try_into().unwrap());
        let media_time = u64::from_be_bytes(body[16..24].try_into().unwrap());
        let first_wall = report.fragments[0].wall.unwrap();
        let expected_head = u64::try_from(wall(0).offset(start).get() - first_wall.get()).unwrap()
            * 90_000
            / 1_000_000;
        assert_eq!(media_time, expected_head, "range {start}..{end}");
        let media_duration: u64 = durations.iter().map(|&duration| u64::from(duration)).sum();
        assert_eq!(
            duration_ms,
            (media_duration - media_time).div_ceil(90),
            "range {start}..{end} must have a positive, representable presentation"
        );
    }
    catalog.shutdown();
}

#[tokio::test]
async fn unknown_segment_and_bad_ranges_are_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let catalog = CatalogHandle::open(&dir.path().join("c.db")).unwrap();
    let app = router(state(dir.path(), catalog.clone(), Vec::new()));
    let missing = app
        .clone()
        .oneshot(
            Request::get("/api/segments/999/file.mp4")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(missing.status(), StatusCode::NOT_FOUND);
    let inverted = app
        .clone()
        .oneshot(
            Request::get("/api/streams/cam1/main/segments?from=10&to=5")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(inverted.status(), StatusCode::BAD_REQUEST);
    let bad_kind = app
        .oneshot(
            Request::get("/api/streams/cam1/video/segments")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(bad_kind.status(), StatusCode::BAD_REQUEST);
    catalog.shutdown();
}

#[tokio::test]
async fn malformed_unicode_timestamps_and_overflowing_ranges_return_bad_request() {
    let dir = tempfile::tempdir().unwrap();
    let catalog = CatalogHandle::open(&dir.path().join("c.db")).unwrap();
    let app = router(state(dir.path(), catalog.clone(), vec![]));
    for query in [
        "from=2026-01-01T00:00:0%C3%A9Z".to_owned(),
        format!("from={}&to={}", i64::MIN, i64::MAX),
        format!("to={}", i64::MIN),
    ] {
        let response = get(
            &app,
            format!("/api/streams/cam1/main/segments?{query}"),
            None,
        )
        .await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{query}");
    }
    catalog.shutdown();
}

#[tokio::test]
async fn clipped_continuations_keep_sync_preroll_in_vod_metadata_and_export() {
    let dir = tempfile::tempdir().unwrap();
    let catalog = CatalogHandle::open(&dir.path().join("c.db")).unwrap();
    let entry = entry();
    let mut continuation = fragment(1, &entry);
    continuation.samples[0].is_sync = false;
    let paths = record_segments(
        dir.path(),
        &catalog,
        vec![fragment(0, &entry), continuation],
    )
    .await;
    let (_, records) = read_index_bounded(
        &iris_store::layout::index_path(&paths[0]),
        MAX_PERSISTED_INDEX_RECORDS,
    )
    .unwrap();
    let app = router(state(dir.path(), catalog.clone(), vec![]));
    let range = format!(
        "from={}&to={}",
        wall(1).offset(100_000).get(),
        wall(2).get()
    );
    let playlist = get(
        &app,
        format!("/api/streams/cam1/main/hls/vod.m3u8?{range}"),
        None,
    )
    .await;
    assert_eq!(playlist.status(), StatusCode::OK);
    let text = body_string(playlist).await;
    let metadata: Vec<String> = records
        .iter()
        .map(|record| {
            format!(
                "#IRIS-FRAGMENT:WALL-US={},DURATION-TICKS={}",
                record.wall.get(),
                record.duration
            )
        })
        .collect();
    assert_eq!(
        text.lines()
            .filter(|line| line.starts_with("#IRIS-FRAGMENT:"))
            .collect::<Vec<_>>(),
        metadata
    );
    assert!(text.contains(&format!(
        "#EXT-X-BYTERANGE:{}@{}",
        records.last().unwrap().end() - records[0].offset,
        records[0].offset
    )));
    let export = get(
        &app,
        format!("/api/streams/cam1/main/export.mp4?{range}"),
        None,
    )
    .await;
    assert_eq!(export.status(), StatusCode::OK);
    let bytes = to_bytes(export.into_body(), 64 << 20).await.unwrap();
    let report = iris_mp4::reader::scan(
        std::io::Cursor::new(bytes.to_vec()),
        iris_mp4::reader::ScanOptions {
            verify_crc: true,
            collect_samples: false,
        },
    )
    .unwrap();
    assert_eq!(report.fragments.len(), 2);
    assert!(report.fragments[0].starts_with_sync);
    let elst = bytes
        .windows(4)
        .position(|window| window == b"elst")
        .unwrap();
    let body = &bytes[elst + 4..];
    assert_eq!(u64::from_be_bytes(body[8..16].try_into().unwrap()), 1900);
    assert_eq!(
        u64::from_be_bytes(body[16..24].try_into().unwrap()),
        189_000
    );
    catalog.shutdown();
}

#[tokio::test]
async fn vod_marks_one_gap_after_a_skipped_middle_sidecar_but_not_a_skipped_first() {
    let dir = tempfile::tempdir().unwrap();
    let catalog = CatalogHandle::open(&dir.path().join("c.db")).unwrap();
    let entry = entry();
    let paths = record_segments_with_rotation(
        dir.path(),
        &catalog,
        vec![
            fragment(0, &entry),
            fragment(1, &entry),
            fragment(2, &entry),
        ],
        1,
    )
    .await;
    assert_eq!(paths.len(), 3);
    let first_index = iris_store::layout::index_path(&paths[0]);
    let middle_index = iris_store::layout::index_path(&paths[1]);
    let first_bytes = std::fs::read(&first_index).unwrap();
    let middle_bytes = std::fs::read(&middle_index).unwrap();
    std::fs::write(&middle_index, b"corrupt sidecar").unwrap();
    let app = router(state(dir.path(), catalog.clone(), Vec::new()));
    let uri = format!(
        "/api/streams/cam1/main/hls/vod.m3u8?from={}&to={}",
        wall(0).get(),
        wall(3).get()
    );

    let middle_skipped = get(&app, uri.clone(), None).await;
    assert_eq!(middle_skipped.status(), StatusCode::OK);
    let text = body_string(middle_skipped).await;
    assert_eq!(text.matches("#EXT-X-DISCONTINUITY\n").count(), 1, "{text}");

    std::fs::write(&middle_index, middle_bytes).unwrap();
    std::fs::remove_file(&first_index).unwrap();
    let first_skipped = get(&app, uri, None).await;
    assert_eq!(first_skipped.status(), StatusCode::OK);
    let text = body_string(first_skipped).await;
    assert_eq!(text.matches("#EXT-X-DISCONTINUITY\n").count(), 0, "{text}");
    // Keep the fixture mutation explicit: the missing first cache could be rebuilt,
    // but serving must not synchronously scan it as a fallback.
    std::fs::write(first_index, first_bytes).unwrap();
    catalog.shutdown();
}

#[tokio::test]
async fn exports_reject_codec_changes_before_sending_a_body() {
    let dir = tempfile::tempdir().unwrap();
    let catalog = CatalogHandle::open(&dir.path().join("c.db")).unwrap();
    let first = entry();
    let second = Arc::new(SampleEntry::new(
        Bytes::from_static(b"\0\0\0\x10hvc1\0\0\0\0\0\0\0\x01"),
        "hvc1.1.6.L150.B0".into(),
        1280,
        720,
    ));
    assert_ne!(first.hash, second.hash);
    let files = record_segments(
        dir.path(),
        &catalog,
        vec![fragment(0, &first), fragment(1, &second)],
    )
    .await;
    assert_eq!(files.len(), 2);
    let app = router(state(dir.path(), catalog.clone(), vec![]));
    let response = get(
        &app,
        format!(
            "/api/streams/cam1/main/export.mp4?from={}&to={}",
            wall(0).get(),
            wall(2).get()
        ),
        None,
    )
    .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert!(
        body_string(response)
            .await
            .contains("codec configuration change")
    );
    catalog.shutdown();
}

#[tokio::test]
async fn exports_reject_codec_changes_when_adopted_files_have_unknown_hashes() {
    let dir = tempfile::tempdir().unwrap();
    let original = CatalogHandle::open(&dir.path().join("original.db")).unwrap();
    let first = entry();
    let second = Arc::new(SampleEntry::new(
        Bytes::from_static(b"\0\0\0\x10hvc1\0\0\0\0\0\0\0\x01"),
        "hvc1.1.6.L150.B0".into(),
        1280,
        720,
    ));
    let files = record_segments(
        dir.path(),
        &original,
        vec![fragment(0, &first), fragment(1, &second)],
    )
    .await;
    original.shutdown();
    let adopted = CatalogHandle::open(&dir.path().join("adopted.db")).unwrap();
    for path in files {
        let (header, records) = read_index_bounded(
            &iris_store::layout::index_path(&path),
            MAX_PERSISTED_INDEX_RECORDS,
        )
        .unwrap();
        let last = records.last().unwrap();
        adopted
            .adopt_segment(iris_catalog::NewSegment {
                key: key(),
                path,
                run_seq: header.run_id,
                discontinuity: true,
                wall_start: records[0].wall,
                wall_end: last.wall.offset(2_000_000),
                media_start: records[0].decode_time,
                media_end: last.decode_time + u64::from(last.duration),
                init_len: header.init_len,
                len: last.end(),
                fragments: u32::try_from(records.len()).unwrap(),
                sample_entry: None,
            })
            .await
            .unwrap();
    }
    let app = router(state(dir.path(), adopted.clone(), vec![]));
    let response = get(
        &app,
        format!(
            "/api/streams/cam1/main/export.mp4?from={}&to={}",
            wall(0).get(),
            wall(2).get()
        ),
        None,
    )
    .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    adopted.shutdown();
}

#[tokio::test]
async fn the_keyframe_resource_is_one_sync_sample_behind_the_init_segment() {
    let dir = tempfile::tempdir().unwrap();
    let catalog = CatalogHandle::open(&dir.path().join("c.db")).unwrap();
    let (_, init_len, _) = populate(dir.path(), &catalog).await;
    let app = router(state(dir.path(), catalog.clone(), vec![]));
    // Fragment 1 starts at wall(1); half a second later the newest sync sample is still fragment 1's.
    let t = wall(1).offset(500_000).get();
    let response = get(&app, format!("/api/streams/cam1/main/keyframe?t={t}"), None).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers()[header::CACHE_CONTROL],
        "private, max-age=3600"
    );
    assert_eq!(
        response.headers().get("x-iris-wall-us").unwrap(),
        &wall(1).get().to_string()
    );
    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    assert_eq!(&body[4..8], b"ftyp");
    let report = iris_mp4::reader::scan(
        std::io::Cursor::new(body.to_vec()),
        iris_mp4::reader::ScanOptions {
            verify_crc: true,
            collect_samples: true,
        },
    )
    .unwrap();
    assert_eq!(report.init_len, init_len);
    assert_eq!(report.fragments.len(), 1);
    let only = &report.fragments[0];
    assert_eq!(only.sample_count, 1);
    assert!(only.starts_with_sync);
    assert_eq!(only.sample_sizes, vec![nal(0x26, 0xaa, 500).len() as u32]);
    assert_eq!(only.wall, Some(wall(1)));
    // Before any recording: nothing to show.
    let early = wall(0).offset(-1).get();
    let response = get(
        &app,
        format!("/api/streams/cam1/main/keyframe?t={early}"),
        None,
    )
    .await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    catalog.shutdown();
}

#[tokio::test]
async fn keyframes_at_or_near_the_present_are_not_cached() {
    let dir = tempfile::tempdir().unwrap();
    let catalog = CatalogHandle::open(&dir.path().join("c.db")).unwrap();
    let entry = entry();
    let mut current = fragment(0, &entry);
    current.wall = WallUs::now();
    let at = current.wall.get();
    record_segments(dir.path(), &catalog, vec![current]).await;
    let app = router(state(dir.path(), catalog.clone(), Vec::new()));
    let newest = get(&app, "/api/streams/cam1/main/keyframe".into(), None).await;
    assert_eq!(newest.status(), StatusCode::OK);
    assert_eq!(newest.headers()[header::CACHE_CONTROL], "private, no-store");
    // An explicit time within the mutable horizon names a sample that later
    // fragments may still replace.
    let recent = get(
        &app,
        format!("/api/streams/cam1/main/keyframe?t={at}"),
        None,
    )
    .await;
    assert_eq!(recent.status(), StatusCode::OK);
    assert_eq!(recent.headers()[header::CACHE_CONTROL], "private, no-store");
    catalog.shutdown();
}

#[tokio::test]
async fn historical_keyframes_may_cache_privately() {
    let dir = tempfile::tempdir().unwrap();
    let catalog = CatalogHandle::open(&dir.path().join("c.db")).unwrap();
    record_segments(dir.path(), &catalog, vec![fragment(0, &entry())]).await;
    let app = router(state(dir.path(), catalog.clone(), Vec::new()));
    let historical = get(
        &app,
        format!("/api/streams/cam1/main/keyframe?t={}", wall(0).get()),
        None,
    )
    .await;
    assert_eq!(historical.status(), StatusCode::OK);
    assert_eq!(
        historical.headers()[header::CACHE_CONTROL],
        "private, max-age=3600"
    );
    catalog.shutdown();
}

#[tokio::test]
async fn keyframe_lookup_enforces_the_two_minute_lower_bound() {
    let dir = tempfile::tempdir().unwrap();
    let catalog = CatalogHandle::open(&dir.path().join("c.db")).unwrap();
    let entry = entry();
    let first = fragment(0, &entry);
    let mut continuation = fragment(1, &entry);
    continuation.samples[0].is_sync = false;
    continuation.wall = first.wall.offset(121_000_000);
    record_segments(dir.path(), &catalog, vec![first.clone(), continuation]).await;
    let app = router(state(dir.path(), catalog.clone(), Vec::new()));

    let within = get(
        &app,
        format!(
            "/api/streams/cam1/main/keyframe?t={}",
            first.wall.offset(119_000_000).get()
        ),
        None,
    )
    .await;
    assert_eq!(within.status(), StatusCode::OK);
    let outside = get(
        &app,
        format!(
            "/api/streams/cam1/main/keyframe?t={}",
            first.wall.offset(121_000_000).get()
        ),
        None,
    )
    .await;
    assert_eq!(outside.status(), StatusCode::NOT_FOUND);
    catalog.shutdown();
}

#[tokio::test]
async fn keyframe_rejects_corrupt_payload_and_false_sidecar_sync() {
    let corrupt_dir = tempfile::tempdir().unwrap();
    let corrupt_catalog = CatalogHandle::open(&corrupt_dir.path().join("c.db")).unwrap();
    let (path, _, _) = populate(corrupt_dir.path(), &corrupt_catalog).await;
    let (_, records) = read_index_bounded(
        &iris_store::layout::index_path(&path),
        MAX_PERSISTED_INDEX_RECORDS,
    )
    .unwrap();
    let record = records[1];
    let mut media = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&path)
        .unwrap();
    media.seek(SeekFrom::Start(record.offset)).unwrap();
    let mut fragment_bytes = vec![0; record.len as usize];
    media.read_exact(&mut fragment_bytes).unwrap();
    let info = iris_mp4::reader::parse_fragment(&fragment_bytes).unwrap();
    let payload = record.offset + info.payload_offset;
    media.seek(SeekFrom::Start(payload)).unwrap();
    let mut byte = [0];
    media.read_exact(&mut byte).unwrap();
    media.seek(SeekFrom::Start(payload)).unwrap();
    media.write_all(&[byte[0] ^ 0x01]).unwrap();
    media.flush().unwrap();
    let app = router(state(
        corrupt_dir.path(),
        corrupt_catalog.clone(),
        Vec::new(),
    ));
    let response = get(
        &app,
        format!(
            "/api/streams/cam1/main/keyframe?t={}",
            record.wall.offset(1).get()
        ),
        None,
    )
    .await;
    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    let body: serde_json::Value = serde_json::from_str(&body_string(response).await).unwrap();
    assert_eq!(body["error"], "internal error");
    assert!(body["request_id"].is_u64(), "{body}");
    assert!(
        !body.to_string().contains("checksum"),
        "internal detail stays in the log: {body}"
    );
    corrupt_catalog.shutdown();

    let sync_dir = tempfile::tempdir().unwrap();
    let sync_catalog = CatalogHandle::open(&sync_dir.path().join("c.db")).unwrap();
    let entry = entry();
    let mut continuation = fragment(1, &entry);
    continuation.samples[0].is_sync = false;
    let paths = record_segments(
        sync_dir.path(),
        &sync_catalog,
        vec![fragment(0, &entry), continuation],
    )
    .await;
    let index_path = iris_store::layout::index_path(&paths[0]);
    let mut index = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(index_path)
        .unwrap();
    // The flags byte of the second record: header, first record, 34 bytes in.
    index
        .seek(SeekFrom::Start(
            (iris_mp4::index::HEADER_LEN + iris_mp4::index::RECORD_LEN + 34) as u64,
        ))
        .unwrap();
    index
        .write_all(&[iris_mp4::index::FLAG_STARTS_WITH_SYNC])
        .unwrap();
    index.flush().unwrap();
    let app = router(state(sync_dir.path(), sync_catalog.clone(), Vec::new()));
    let response = get(
        &app,
        format!(
            "/api/streams/cam1/main/keyframe?t={}",
            wall(1).offset(1).get()
        ),
        None,
    )
    .await;
    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    sync_catalog.shutdown();
}

/// A live frame for the ring: synthetic sample bytes with real timing.
fn live_frame(entry: &Arc<SampleEntry>, index: u64, is_sync: bool) -> Arc<LiveFrame> {
    Arc::new(LiveFrame {
        key: key(),
        sample_entry: Arc::clone(entry),
        data: Bytes::from(nal(if is_sync { 0x26 } else { 0x02 }, 0xcc, 300)),
        duration: 3000,
        is_sync,
        decode_time: index * 3000,
        wall: WallUs::new(1_700_000_000_000_000 + i64::try_from(index).unwrap() * 33_333),
    })
}

/// A router with one live ring fed 80 frames, a sync frame every 30th, once
/// the feeder has subscribed.
async fn live_setup() -> (
    axum::Router,
    LiveRing,
    Arc<SampleEntry>,
    CatalogHandle,
    tempfile::TempDir,
) {
    let dir = tempfile::tempdir().unwrap();
    let catalog = CatalogHandle::open(&dir.path().join("c.db")).unwrap();
    let state = state(dir.path(), catalog.clone(), vec!["t".into()]);
    // Larger than any burst below: a lagging feeder resets to the next sync frame.
    let ring = LiveRing::new(1024);
    state.live.insert(key(), ring.clone());
    let app = router(state);
    let entry = entry();
    // The feeder subscribes on the first request, so the frames are published
    // once that request is waiting: two complete groups of thirty frames and a
    // third with twenty, parts of fifteen frames.
    let first = tokio::spawn({
        let app = app.clone();
        async move { get(&app, "/api/streams/cam1/main/hls/live.m3u8".into(), None).await }
    });
    tokio::time::timeout(Duration::from_secs(2), async {
        while ring.subscriber_count() == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the feeder subscribed");
    for i in 0..80u64 {
        ring.publish(live_frame(&entry, i, i % 30 == 0));
    }
    assert_eq!(first.await.unwrap().status(), StatusCode::OK);
    (app, ring, entry, catalog, dir)
}

/// The playlist once the feeder has worked through the burst.
async fn settled_playlist(app: &axum::Router) -> String {
    let mut text = String::new();
    for _ in 0..40 {
        let response = get(app, "/api/streams/cam1/main/hls/live.m3u8".into(), None).await;
        assert_eq!(response.status(), StatusCode::OK);
        text = body_string(response).await;
        if text.contains("seg/1/media.m4s") && text.contains("part/2/0/part.m4s") {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    text
}

#[tokio::test]
async fn the_live_playlist_serves_parts_and_segments() {
    let (app, _ring, _entry, catalog, _dir) = live_setup().await;
    let base = "/api/streams/cam1/main/hls";
    let text = settled_playlist(&app).await;
    assert!(text.contains("#EXT-X-PART-INF:PART-TARGET=0.500"), "{text}");
    assert!(text.contains("#EXT-X-MEDIA-SEQUENCE:0\n"), "{text}");
    assert!(!text.contains("token=t"), "root token leaked in {text}");
    assert!(
        text.contains("#EXTINF:1.00000,\nseg/0/media.m4s?token=v1."),
        "{text}"
    );
    assert!(
        text.contains("#EXTINF:1.00000,\nseg/1/media.m4s?token=v1."),
        "{text}"
    );
    assert!(
        text.contains("#EXT-X-PART:DURATION=0.50000,URI=\"part/2/0/part.m4s?token=v1."),
        "{text}"
    );
    assert!(
        text.contains("#EXT-X-PRELOAD-HINT:TYPE=PART,URI=\"part/2/1/part.m4s?token=v1."),
        "{text}"
    );
    // The init segment and a complete segment scan as a recording of single-sample fragments.
    let init_line = text
        .lines()
        .find(|line| line.starts_with("#EXT-X-MAP:"))
        .unwrap();
    let init = get_with_capability(&app, format!("{base}/{}", quoted_uri(init_line)), None).await;
    assert_eq!(init.status(), StatusCode::OK);
    let mut file = to_bytes(init.into_body(), 1 << 20).await.unwrap().to_vec();
    let segment_uri = text
        .lines()
        .find(|line| line.starts_with("seg/1/media.m4s?token="))
        .unwrap();
    let seg = get_with_capability(&app, format!("{base}/{segment_uri}"), None).await;
    assert_eq!(seg.status(), StatusCode::OK);
    assert_eq!(seg.headers()[header::CONTENT_TYPE], "video/iso.segment");
    file.extend_from_slice(&to_bytes(seg.into_body(), 1 << 20).await.unwrap());
    let report = iris_mp4::reader::scan(
        std::io::Cursor::new(file),
        iris_mp4::reader::ScanOptions {
            verify_crc: true,
            collect_samples: false,
        },
    )
    .unwrap();
    assert_eq!(report.stop, None);
    assert_eq!(report.fragments.len(), 30);
    assert!(report.fragments[0].starts_with_sync);
    assert_eq!(report.fragments[0].decode_time, 30 * 3000);
    // A part has fifteen fragments.
    let part_line = text
        .lines()
        .find(|line| line.contains("URI=\"part/2/0/part.m4s?token="))
        .unwrap();
    let part = get_with_capability(&app, format!("{base}/{}", quoted_uri(part_line)), None).await;
    assert_eq!(part.status(), StatusCode::OK);
    let bytes = to_bytes(part.into_body(), 1 << 20).await.unwrap();
    assert_eq!(bytes.windows(4).filter(|w| w == b"moof").count(), 15);
    catalog.shutdown();
}

#[tokio::test]
async fn streaming_and_bounded_media_classes_are_admitted_independently() {
    let (app, _ring, _entry, catalog, dir) = live_setup().await;
    let (path, init_len, _) = populate(dir.path(), &catalog).await;
    let id = catalog
        .segment_by_path(&path)
        .await
        .unwrap()
        .expect("registered")
        .id
        .0;
    let file = format!("/api/segments/{id}/file.mp4");
    let range = format!("from={}&to={}", wall(0).get(), wall(3).get());

    // Eight whole-file bodies that nobody reads fill the streaming class.
    let mut held = Vec::new();
    for _ in 0..8 {
        let response = get(&app, file.clone(), None).await;
        assert_eq!(response.status(), StatusCode::OK);
        held.push(response);
    }
    for streaming in [
        get(&app, file.clone(), None).await,
        get(&app, file.clone(), Some("bytes=0-".into())).await,
        get(&app, file.clone(), Some("bytes=0-1048576".into())).await,
        get(
            &app,
            format!("/api/streams/cam1/main/export.mp4?{range}"),
            None,
        )
        .await,
    ] {
        assert_eq!(streaming.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(streaming.headers()[header::RETRY_AFTER], "1");
        assert!(
            body_string(streaming)
                .await
                .contains("busy: too many concurrent streaming media responses")
        );
    }

    // Bounded resources keep their own class.
    let init = get(
        &app,
        file.clone(),
        Some(format!("bytes=0-{}", init_len - 1)),
    )
    .await;
    assert_eq!(init.status(), StatusCode::PARTIAL_CONTENT);
    let vod = get(
        &app,
        format!("/api/streams/cam1/main/hls/vod.m3u8?{range}"),
        None,
    )
    .await;
    assert_eq!(vod.status(), StatusCode::OK);
    let keyframe = get(
        &app,
        format!("/api/streams/cam1/main/keyframe?t={}", wall(1).get()),
        None,
    )
    .await;
    assert_eq!(keyframe.status(), StatusCode::OK);
    let live_init = get(&app, "/api/streams/cam1/main/hls/init.mp4".into(), None).await;
    assert_eq!(live_init.status(), StatusCode::OK);
    let text = settled_playlist(&app).await;
    let segment = text
        .lines()
        .find(|line| line.starts_with("seg/1/media.m4s?token="))
        .expect("live segment URI");
    let live_segment =
        get_with_capability(&app, format!("/api/streams/cam1/main/hls/{segment}"), None).await;
    assert_eq!(live_segment.status(), StatusCode::OK);

    drop(held.pop());
    let admitted = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let response = get(&app, file.clone(), None).await;
            if response.status() == StatusCode::OK {
                break response;
            }
            assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("a released streaming slot admits the next whole-file request");
    drop((admitted, held));
    catalog.shutdown();
}

#[tokio::test]
async fn snapshots_are_single_flight_per_camera() {
    let camera = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = camera.local_addr().unwrap();
    let (accepted_tx, accepted_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = tokio::sync::oneshot::channel::<()>();
    let fake_camera = tokio::spawn(async move {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

        let (mut socket, _) = camera.accept().await.unwrap();
        let mut request = vec![0_u8; 4096];
        let _ = socket.read(&mut request).await.unwrap();
        accepted_tx.send(()).unwrap();
        release_rx.await.unwrap();
        let mut jpeg = vec![0_u8; 64];
        jpeg[..3].copy_from_slice(&[0xff, 0xd8, 0xff]);
        jpeg[62..].copy_from_slice(&[0xff, 0xd9]);
        socket
            .write_all(
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: image/jpeg\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    jpeg.len()
                )
                .as_bytes(),
            )
            .await
            .unwrap();
        socket.write_all(&jpeg).await.unwrap();
    });

    let dir = tempfile::tempdir().unwrap();
    let catalog = CatalogHandle::open(&dir.path().join("c.db")).unwrap();
    let mut state = state(dir.path(), catalog.clone(), vec![]);
    Arc::get_mut(&mut state)
        .expect("sole state owner")
        .snapshot_sources = std::sync::RwLock::new(HashMap::from([(
        key().camera,
        SnapshotSource::new(
            address.ip().to_string(),
            address.port(),
            false,
            true,
            String::new(),
            String::new(),
            iris_core::config::ResolvedSnapshot::DahuaCgi,
        ),
    )]));
    let counters = Arc::clone(&state.stats);
    let app = router(state);
    let first = tokio::spawn({
        let app = app.clone();
        async move { get(&app, "/api/cameras/cam1/snapshot.jpg".into(), None).await }
    });
    tokio::time::timeout(Duration::from_secs(2), accepted_rx)
        .await
        .expect("the camera request is in flight")
        .unwrap();

    let second = get(&app, "/api/cameras/cam1/snapshot.jpg".into(), None).await;
    assert_eq!(second.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(second.headers()[header::RETRY_AFTER], "1");
    assert_eq!(counters.snapshot_rejected(), 1);

    release_tx.send(()).unwrap();
    let first = tokio::time::timeout(Duration::from_secs(5), first)
        .await
        .expect("the first snapshot completes")
        .unwrap();
    assert_eq!(first.status(), StatusCode::OK);
    assert_eq!(
        to_bytes(first.into_body(), 1 << 20).await.unwrap().len(),
        64
    );
    fake_camera.await.unwrap();
    catalog.shutdown();
}

#[tokio::test]
async fn the_live_playlist_blocks_until_the_requested_part_and_rejects_bad_directives() {
    let (app, ring, entry, catalog, _dir) = live_setup().await;
    let base = "/api/streams/cam1/main/hls";
    settled_playlist(&app).await;
    // Part 1 of segment 2 does not exist until ten more frames arrive.
    let blocked = tokio::spawn({
        let app = app.clone();
        async move {
            let started = tokio::time::Instant::now();
            let r = get(
                &app,
                format!("{base}/live.m3u8?_HLS_msn=2&_HLS_part=1"),
                None,
            )
            .await;
            (r, started.elapsed())
        }
    });
    tokio::time::sleep(Duration::from_millis(300)).await;
    for i in 80..90u64 {
        ring.publish(live_frame(&entry, i, false));
    }
    let (response, waited) = blocked.await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert!(waited >= Duration::from_millis(250), "{waited:?}");
    let text = body_string(response).await;
    assert!(text.contains("URI=\"part/2/1/part.m4s?token=v1."), "{text}");
    assert!(
        text.contains("#EXT-X-PRELOAD-HINT:TYPE=PART,URI=\"part/2/2/part.m4s?token=v1."),
        "{text}"
    );
    let bad = get(&app, format!("{base}/live.m3u8?_HLS_part=1"), None).await;
    assert_eq!(bad.status(), StatusCode::BAD_REQUEST);
    let far = get(&app, format!("{base}/live.m3u8?_HLS_msn=9"), None).await;
    assert_eq!(far.status(), StatusCode::BAD_REQUEST);
    // Six parts of 0.5 s are the advance part limit; part 50 is far beyond it.
    let far_part = get(
        &app,
        format!("{base}/live.m3u8?_HLS_msn=2&_HLS_part=50"),
        None,
    )
    .await;
    assert_eq!(far_part.status(), StatusCode::BAD_REQUEST);
    catalog.shutdown();
}

/// A recording in the middle of a range whose sidecar cannot be used is left
/// out, and the playlist marks the break before the recording after it; one
/// at the start of the range leaves nothing to break from.
#[tokio::test]
async fn an_unusable_recording_leaves_one_discontinuity_where_it_was() {
    let dir = tempfile::tempdir().unwrap();
    let catalog = CatalogHandle::open(&dir.path().join("c.db")).unwrap();
    let entry = entry();
    let fragments = (0..9).map(|index| fragment(index, &entry)).collect();
    // Small files: a few fragments each.
    let files = record_segments_with_rotation(dir.path(), &catalog, fragments, 2_500).await;
    assert!(files.len() >= 3, "{files:?}");
    let app = router(state(dir.path(), catalog.clone(), vec!["t".into()]));
    let playlist = || async {
        let uri = format!(
            "/api/streams/cam1/main/hls/vod.m3u8?from={}&to={}",
            wall(0).get(),
            wall(10).get()
        );
        let response = get(&app, uri, None).await;
        assert_eq!(response.status(), StatusCode::OK);
        body_string(response).await
    };
    let maps = |text: &str| text.matches("#EXT-X-MAP:").count();
    let breaks = |text: &str| text.matches("#EXT-X-DISCONTINUITY\n").count();

    let whole = playlist().await;
    assert_eq!((maps(&whole), breaks(&whole)), (files.len(), 0), "{whole}");

    // The second recording's sidecar is cut inside its header.
    let second = iris_store::layout::index_path(&files[1]);
    let saved = std::fs::read(&second).unwrap();
    std::fs::write(&second, &saved[..10]).unwrap();
    let broken = playlist().await;
    assert_eq!(
        (maps(&broken), breaks(&broken)),
        (files.len() - 1, 1),
        "{broken}"
    );
    // The break stands between the first recording and the third.
    let at = broken.find("#EXT-X-DISCONTINUITY\n").unwrap();
    assert_eq!(maps(&broken[..at]), 1, "{broken}");

    // With the first recording unusable instead, the range starts later.
    std::fs::write(&second, &saved).unwrap();
    let first = iris_store::layout::index_path(&files[0]);
    let head = std::fs::read(&first).unwrap();
    std::fs::write(&first, &head[..10]).unwrap();
    let late = playlist().await;
    assert_eq!((maps(&late), breaks(&late)), (files.len() - 1, 0), "{late}");
    catalog.shutdown();
}

/// A segment or part that is still being filled is served once it is
/// complete, and a part index past the parts of a complete segment does not
/// exist.
#[tokio::test]
async fn live_segments_and_parts_wait_until_they_are_complete() {
    let (app, ring, entry, catalog, _dir) = live_setup().await;
    let text = settled_playlist(&app).await;
    // The first frame's wall time, to the millisecond.
    assert!(
        text.contains("#EXT-X-PROGRAM-DATE-TIME:2023-11-14T22:13:20.000Z\n"),
        "{text}"
    );
    let fetch = |path: &'static str| {
        let app = app.clone();
        tokio::spawn(async move {
            let response = get(&app, format!("/api/streams/cam1/main/hls/{path}"), None).await;
            let status = response.status();
            (
                status,
                to_bytes(response.into_body(), 1 << 20).await.unwrap(),
            )
        })
    };
    let fragments = |init: &[u8], media: &[u8]| {
        let mut file = init.to_vec();
        file.extend_from_slice(media);
        let report = iris_mp4::reader::scan(
            std::io::Cursor::new(file),
            iris_mp4::reader::ScanOptions {
                verify_crc: true,
                collect_samples: false,
            },
        )
        .unwrap();
        assert_eq!(report.stop, None);
        report.fragments
    };
    let (status, init) = fetch("init.mp4").await.unwrap();
    assert_eq!(status, StatusCode::OK);

    // Segment 2 holds twenty frames and its second part five.
    let segment = fetch("seg/2/media.m4s");
    let open_part = fetch("part/2/1/part.m4s");
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(!segment.is_finished() && !open_part.is_finished());
    // Ten more frames complete the part; the sync frame after them
    // completes the segment.
    for index in 80..=90u64 {
        ring.publish(live_frame(&entry, index, index == 90));
    }
    let (status, media) = segment.await.unwrap();
    assert_eq!(status, StatusCode::OK);
    let listed = fragments(&init, &media);
    assert_eq!(listed.len(), 30);
    assert_eq!(listed[0].decode_time, 60 * 3000);
    let (status, media) = open_part.await.unwrap();
    assert_eq!(status, StatusCode::OK);
    let listed = fragments(&init, &media);
    assert_eq!(listed.len(), 15);
    assert_eq!(listed[0].decode_time, 75 * 3000);

    // The complete segment has two parts: a third does not exist.
    let (status, _) = fetch("part/2/2/part.m4s").await.unwrap();
    assert_eq!(status, StatusCode::NOT_FOUND);

    // Segment 3 has one frame; its first part completes at fifteen.
    let first_part = fetch("part/3/0/part.m4s");
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(!first_part.is_finished());
    for index in 91..105u64 {
        ring.publish(live_frame(&entry, index, false));
    }
    let (status, media) = first_part.await.unwrap();
    assert_eq!(status, StatusCode::OK);
    let listed = fragments(&init, &media);
    assert_eq!(listed.len(), 15);
    assert!(listed[0].starts_with_sync);
    assert_eq!(listed[0].decode_time, 90 * 3000);
    catalog.shutdown();
}

/// A blocking reload the feeder cannot satisfy answers 503 after three
/// target durations (draft-pantos-hls-rfc8216bis §6.2.5.2).
#[tokio::test(start_paused = true)]
async fn a_blocking_reload_that_cannot_advance_answers_503() {
    let (app, _ring, _entry, catalog, _dir) = live_setup().await;
    let text = settled_playlist(&app).await;
    assert!(text.contains("#EXT-X-TARGETDURATION:2\n"), "{text}");
    let started = tokio::time::Instant::now();
    let response = get(
        &app,
        "/api/streams/cam1/main/hls/live.m3u8?_HLS_msn=3".into(),
        None,
    )
    .await;
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(started.elapsed(), Duration::from_secs(6));

    // The newest segment is 2, holding one complete part and one open. A
    // reload may name a segment two past the newest and a part six (three
    // seconds of parts) past the segment's parts: those wait and time out,
    // and one further is refused at once.
    let status = |query: &'static str| {
        let app = app.clone();
        async move {
            let started = tokio::time::Instant::now();
            let uri = format!("/api/streams/cam1/main/hls/live.m3u8?{query}");
            (get(&app, uri, None).await.status(), started.elapsed())
        }
    };
    let waited = (StatusCode::SERVICE_UNAVAILABLE, Duration::from_secs(6));
    let refused = (StatusCode::BAD_REQUEST, Duration::ZERO);
    assert_eq!(status("_HLS_msn=4").await, waited);
    assert_eq!(status("_HLS_msn=5").await, refused);
    assert_eq!(status("_HLS_msn=2&_HLS_part=8").await, waited);
    assert_eq!(status("_HLS_msn=2&_HLS_part=9").await, refused);
    catalog.shutdown();
}

#[tokio::test]
async fn a_live_timestamp_reset_marks_a_discontinuity_and_resumes_at_sync() {
    let (app, ring, entry, catalog, _dir) = live_setup().await;
    settled_playlist(&app).await;
    // The reset arrives on a dependent frame; discard it until the next sync.
    for index in 0..=60 {
        ring.publish(live_frame(&entry, index, index > 0 && index % 30 == 0));
    }
    let response = get(
        &app,
        "/api/streams/cam1/main/hls/live.m3u8?_HLS_msn=3".into(),
        None,
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let text = body_string(response).await;
    assert!(text.contains("#EXT-X-DISCONTINUITY-SEQUENCE:0\n"), "{text}");
    assert_eq!(text.matches("#EXT-X-DISCONTINUITY\n").count(), 1, "{text}");
    let marker = text.find("#EXT-X-DISCONTINUITY\n").unwrap();
    assert!(text[..marker].contains("seg/2/media.m4s"), "{text}");
    assert!(text[marker..].contains("seg/3/media.m4s"), "{text}");
    let init = get(&app, "/api/streams/cam1/main/hls/init.mp4".into(), None).await;
    let mut file = to_bytes(init.into_body(), 1 << 20).await.unwrap().to_vec();
    let response = get(
        &app,
        "/api/streams/cam1/main/hls/seg/3/media.m4s".into(),
        None,
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    file.extend_from_slice(&to_bytes(response.into_body(), 1 << 20).await.unwrap());
    let report = iris_mp4::reader::scan(
        std::io::Cursor::new(file),
        iris_mp4::reader::ScanOptions {
            verify_crc: true,
            collect_samples: false,
        },
    )
    .unwrap();
    assert_eq!(report.fragments.len(), 30);
    assert!(report.fragments[0].starts_with_sync);
    assert_eq!(report.fragments[0].decode_time, 90_000);
    catalog.shutdown();
}

#[tokio::test]
async fn camera_detail_reports_stream_health() {
    let dir = tempfile::tempdir().unwrap();
    let catalog = CatalogHandle::open(&dir.path().join("c.db")).unwrap();
    catalog
        .ensure_camera(&CameraId::try_new("cam1").unwrap(), "front", Some("test"))
        .await
        .unwrap();
    let camera_id = CameraId::try_new("cam1").unwrap();
    let state = state_with_cameras(
        dir.path(),
        catalog.clone(),
        vec![],
        HashMap::from([(
            camera_id.clone(),
            ActiveCamera {
                id: camera_id,
                name: "front".into(),
                vendor: Some("test".into()),
                provisioned: false,
            },
        )]),
    );
    let stream_stats = Arc::new(iris_ingest::StreamStats::default());
    state.live.insert(key(), LiveRing::new(8));
    state.live.insert_stats(key(), stream_stats);
    let live = Arc::clone(&state.live);
    let app = router(state);
    let response = get(&app, "/api/cameras/cam1".into(), None).await;
    assert_eq!(response.status(), StatusCode::OK);
    let text = body_string(response).await;
    assert!(text.contains("\"id\":\"cam1\""), "{text}");
    assert!(text.contains("\"name\":\"front\""), "{text}");
    // A camera has audio when the daemon has a live ring for it.
    assert!(text.contains("\"has_audio\":false"), "{text}");
    live.insert(
        StreamKey {
            kind: StreamKind::Audio,
            ..key()
        },
        LiveRing::new(8),
    );
    let listed = body_string(get(&app, "/api/cameras".into(), None).await).await;
    assert!(listed.contains("\"has_audio\":true"), "{listed}");
    assert!(text.contains("\"kind\":\"main\""), "{text}");
    assert!(text.contains("\"connected\":false"), "{text}");
    assert!(text.contains("\"live_viewers\":0"), "{text}");
    let missing = get(&app, "/api/cameras/nope".into(), None).await;
    assert_eq!(missing.status(), StatusCode::NOT_FOUND);
    catalog.shutdown();
}

#[tokio::test]
async fn active_camera_surface_excludes_removed_history_without_deleting_recordings() {
    let dir = tempfile::tempdir().unwrap();
    let catalog = CatalogHandle::open(&dir.path().join("c.db")).unwrap();
    let removed = CameraId::try_new("cam1").unwrap();
    let active = CameraId::try_new("cam2").unwrap();
    catalog
        .ensure_camera(&removed, "removed", Some("test"))
        .await
        .unwrap();
    catalog
        .ensure_camera(&active, "active", Some("test"))
        .await
        .unwrap();
    populate(dir.path(), &catalog).await;
    let state = state_with_cameras(
        dir.path(),
        catalog.clone(),
        Vec::new(),
        HashMap::from([(
            active.clone(),
            ActiveCamera {
                id: active,
                name: "active".into(),
                vendor: Some("test".into()),
                provisioned: false,
            },
        )]),
    );
    let app = router(state);

    let cameras = get(&app, "/api/cameras".into(), None).await;
    let cameras: serde_json::Value = serde_json::from_str(&body_string(cameras).await).unwrap();
    assert_eq!(cameras.as_array().unwrap().len(), 1);
    assert_eq!(cameras[0]["id"], "cam2");
    assert_eq!(
        get(&app, "/api/cameras/cam1".into(), None).await.status(),
        StatusCode::NOT_FOUND
    );

    let historical = get(
        &app,
        format!(
            "/api/streams/cam1/main/segments?from={}&to={}",
            wall(0).get(),
            wall(3).get()
        ),
        None,
    )
    .await;
    assert_eq!(historical.status(), StatusCode::OK);
    let rows: serde_json::Value = serde_json::from_str(&body_string(historical).await).unwrap();
    assert_eq!(rows.as_array().unwrap().len(), 1);
    catalog.shutdown();
}

#[tokio::test]
async fn retaining_an_event_sets_and_clears_its_time() {
    let dir = tempfile::tempdir().unwrap();
    let catalog = CatalogHandle::open(&dir.path().join("c.db")).unwrap();
    let id = catalog
        .insert_event(&Event {
            camera: CameraId::try_new("cam1").unwrap(),
            source: EventSource::DahuaCgi,
            kind: EventKind::Motion,
            state: EventState::Pulse,
            nvr_time: wall(1),
            camera_time: None,
            data: None,
        })
        .await
        .unwrap();
    let app = router(state(dir.path(), catalog.clone(), vec![]));
    let uri = format!("/api/events/{}/retain", id.0);
    let set = app
        .clone()
        .oneshot(
            Request::post(&uri)
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from("{\"until\": \"2030-01-01T00:00:00Z\"}"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(set.status(), StatusCode::OK);
    let text = body_string(set).await;
    assert!(
        text.contains("\"retain_until_us\":1893456000000000"),
        "{text}"
    );
    // The listing carries it too.
    let (from, to) = (wall(0).get(), wall(3).get());
    let listed = get(&app, format!("/api/events?from={from}&to={to}"), None).await;
    assert!(body_string(listed).await.contains("1893456000000000"));
    let cleared = app
        .clone()
        .oneshot(Request::post(&uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(cleared.status(), StatusCode::OK);
    assert!(
        body_string(cleared)
            .await
            .contains("\"retain_until_us\":null")
    );
    catalog.shutdown();
}

/// Builds a retain request for `uri` with `body` and no `Content-Type`.
fn retain_request(uri: &str, body: &'static str) -> Request<Body> {
    Request::post(uri).body(Body::from(body)).unwrap()
}

#[tokio::test]
async fn a_retain_body_without_a_content_type_still_sets_the_mark() {
    let dir = tempfile::tempdir().unwrap();
    let catalog = CatalogHandle::open(&dir.path().join("c.db")).unwrap();
    let id = catalog
        .insert_event(&Event {
            camera: CameraId::try_new("cam1").unwrap(),
            source: EventSource::DahuaCgi,
            kind: EventKind::Motion,
            state: EventState::Pulse,
            nvr_time: wall(1),
            camera_time: None,
            data: None,
        })
        .await
        .unwrap();
    let app = router(state(dir.path(), catalog.clone(), vec![]));
    let uri = format!("/api/events/{}/retain", id.0);
    let set = app
        .clone()
        .oneshot(retain_request(
            &uri,
            "{\"until\": \"2030-01-01T00:00:00Z\"}",
        ))
        .await
        .unwrap();
    assert_eq!(set.status(), StatusCode::OK);
    assert!(
        body_string(set)
            .await
            .contains("\"retain_until_us\":1893456000000000")
    );
    let unknown_field = app
        .clone()
        .oneshot(retain_request(&uri, "{\"until_us\": 1}"))
        .await
        .unwrap();
    assert_eq!(unknown_field.status(), StatusCode::BAD_REQUEST);
    let still_set = get(
        &app,
        format!("/api/events?from={}&to={}", wall(0).get(), wall(3).get()),
        None,
    )
    .await;
    assert!(
        body_string(still_set).await.contains("1893456000000000"),
        "a refused body changes nothing"
    );
    catalog.shutdown();
}

#[tokio::test]
async fn retain_rejects_an_unparseable_time() {
    let dir = tempfile::tempdir().unwrap();
    let catalog = CatalogHandle::open(&dir.path().join("c.db")).unwrap();
    let app = router(state(dir.path(), catalog.clone(), vec![]));
    let bad = app
        .oneshot(
            Request::post("/api/events/1/retain")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from("{\"until\": \"yesterday\"}"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(bad.status(), StatusCode::BAD_REQUEST);
    catalog.shutdown();
}

#[tokio::test]
async fn retaining_an_unknown_event_is_not_found() {
    let dir = tempfile::tempdir().unwrap();
    let catalog = CatalogHandle::open(&dir.path().join("c.db")).unwrap();
    let app = router(state(dir.path(), catalog.clone(), vec![]));
    let missing = app
        .oneshot(retain_request("/api/events/999/retain", ""))
        .await
        .unwrap();
    assert_eq!(missing.status(), StatusCode::NOT_FOUND);
    catalog.shutdown();
}

#[tokio::test]
async fn event_http_cursor_crosses_an_equal_timestamp_page_boundary() {
    let dir = tempfile::tempdir().unwrap();
    let catalog = CatalogHandle::open(&dir.path().join("c.db")).unwrap();
    let at = wall(1);
    for kind in [EventKind::Motion, EventKind::Human, EventKind::Vehicle] {
        catalog
            .insert_event(&Event {
                camera: CameraId::try_new("cam1").unwrap(),
                source: EventSource::OnvifPullPoint,
                kind,
                state: EventState::Pulse,
                nvr_time: at,
                camera_time: None,
                data: None,
            })
            .await
            .unwrap();
    }
    let app = router(state(dir.path(), catalog.clone(), vec![]));
    let from = wall(0).get();
    let to = wall(3).get();
    let first = get(
        &app,
        format!("/api/events?from={from}&to={to}&limit=2"),
        None,
    )
    .await;
    assert_eq!(first.status(), StatusCode::OK);
    let first: serde_json::Value = serde_json::from_str(&body_string(first).await).unwrap();
    let first = first.as_array().unwrap();
    assert_eq!(first.len(), 2);
    let after_id = first[1]["id"].as_i64().unwrap();
    let second = get(
        &app,
        format!(
            "/api/events?from={from}&to={to}&limit=2&after_us={}&after_id={after_id}",
            at.get()
        ),
        None,
    )
    .await;
    assert_eq!(second.status(), StatusCode::OK);
    let second: serde_json::Value = serde_json::from_str(&body_string(second).await).unwrap();
    let second = second.as_array().unwrap();
    assert_eq!(second.len(), 1);
    assert!(second[0]["id"].as_i64().unwrap() > after_id);

    let incomplete = get(
        &app,
        format!("/api/events?from={from}&to={to}&after_id={after_id}"),
        None,
    )
    .await;
    assert_eq!(incomplete.status(), StatusCode::BAD_REQUEST);
    assert!(body_string(incomplete).await.contains("supplied together"));
    // A cursor names a stored event: ids start at one.
    for id in [0, -1] {
        let unreal = get(
            &app,
            format!("/api/events?from={from}&to={to}&after_us={from}&after_id={id}"),
            None,
        )
        .await;
        assert_eq!(unreal.status(), StatusCode::BAD_REQUEST, "after_id={id}");
        assert!(
            body_string(unreal).await.contains("positive event id"),
            "after_id={id}"
        );
    }
    catalog.shutdown();
}

/// An `mp4a` sample entry through its sampling rate: one channel, 16 bits,
/// 64 kHz.
fn audio_entry() -> Arc<SampleEntry> {
    Arc::new(SampleEntry::audio(
        Bytes::from_static(
            b"\0\0\0\x24mp4a\0\0\0\0\0\0\0\x01\0\0\0\0\0\0\0\0\0\x01\0\x10\0\0\0\0\xfa\0\0\0",
        ),
        "mp4a.40.2".into(),
    ))
}

/// Two seconds of audio that starts 5 ms into video fragment `index`: 125
/// frames of 16 ms on the video's timeline.
fn audio_fragment(index: u64, entry: &Arc<SampleEntry>) -> Fragment {
    Fragment {
        samples: (0..125)
            .map(|_| Sample {
                data: Bytes::from_static(&[0x21, 0x10, 0x04, 0x60, 0x8c, 0x1c]),
                duration: 1_440,
                is_sync: true,
            })
            .collect(),
        decode_time: index * 180_000 + 450,
        wall: wall(index).offset(5_000),
        raw_rtp: 7,
        new_run: index == 0,
        steps: FrameSteps::default(),
        sample_entry: Arc::clone(entry),
    }
}

/// Records video fragments and, beside them, audio fragments of the same
/// camera through one writer, and registers every sealed file under the
/// stream its directory names.
async fn record_camera(
    dir: &Path,
    catalog: &CatalogHandle,
    video: Vec<Fragment>,
    audio: Vec<Fragment>,
) {
    let root = recording_root(dir);
    let root = root.path();
    let audio_key = StreamKey {
        camera: key().camera,
        kind: StreamKind::Audio,
    };
    let entries: HashMap<u32, Arc<SampleEntry>> = video
        .iter()
        .chain(&audio)
        .map(|f| (f.sample_entry.hash, Arc::clone(&f.sample_entry)))
        .collect();
    let writer = WriterHandle::spawn(
        root.to_owned(),
        StoragePolicy {
            sync_interval: Duration::from_secs(5),
            preallocate_bytes: 1 << 20,
            rotate_bytes: 1 << 30,
            rotate_age: Duration::from_secs(3600),
        },
        16,
    )
    .unwrap();
    for fragment in video {
        writer.try_send(key(), fragment).unwrap();
    }
    for fragment in audio {
        writer.try_send(audio_key.clone(), fragment).unwrap();
    }
    tokio::task::spawn_blocking(move || writer.shutdown())
        .await
        .unwrap()
        .unwrap();
    for path in find_mp4s(root) {
        let (header, records) = read_index_bounded(
            &iris_store::layout::index_path(&path),
            MAX_PERSISTED_INDEX_RECORDS,
        )
        .unwrap();
        let is_audio = path.components().any(|part| part.as_os_str() == "audio");
        catalog.notify(Change::SegmentOpened {
            key: if is_audio { audio_key.clone() } else { key() },
            path: path.clone(),
            run_seq: header.run_id,
            discontinuity: records[0].flags & iris_mp4::index::FLAG_DISCONTINUITY_BEFORE != 0,
            wall_start: records[0].wall,
            media_start: records[0].decode_time,
            init_len: header.init_len,
            sample_entry: Arc::clone(&entries[&header.sample_entry_hash]),
        });
        let last = records.last().unwrap();
        catalog.notify(Change::SegmentSealed {
            path,
            len: last.end(),
            fragments: u32::try_from(records.len()).unwrap(),
            wall_end: last
                .wall
                .offset(iris_core::time::MediaTicks::new(i64::from(last.duration)).as_micros()),
            media_end: last.decode_time + u64::from(last.duration),
        });
    }
    catalog.total_bytes().await.unwrap();
}

/// A camera with recorded audio gets a multivariant playlist that declares
/// the audio as a rendition of the main stream and names both streams' own
/// playlists for the range; without audio it declares the variant alone.
#[tokio::test]
async fn a_camera_playlist_declares_recorded_audio_as_a_rendition() {
    let dir = tempfile::tempdir().unwrap();
    let catalog = CatalogHandle::open(&dir.path().join("c.db")).unwrap();
    catalog
        .ensure_camera(&key().camera, "Example", None)
        .await
        .unwrap();
    let video_entry = entry();
    let sound_entry = audio_entry();
    record_camera(
        dir.path(),
        &catalog,
        (0..3).map(|i| fragment(i, &video_entry)).collect(),
        (0..3).map(|i| audio_fragment(i, &sound_entry)).collect(),
    )
    .await;
    let app = router(state(dir.path(), catalog.clone(), vec!["t".into()]));
    let (from, to) = (wall(0).get(), wall(3).get());

    let response = get(
        &app,
        format!("/api/cameras/cam1/hls/vod.m3u8?from={from}&to={to}"),
        None,
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers()[header::CONTENT_TYPE],
        "application/vnd.apple.mpegurl"
    );
    let text = body_string(response).await;
    let lines: Vec<&str> = text.lines().collect();
    let video_uri = format!("/api/streams/cam1/main/hls/vod.m3u8?from={from}&to={to}");
    let audio_uri = format!("/api/streams/cam1/audio/hls/vod.m3u8?from={from}&to={to}");
    assert_eq!(lines[..2], ["#EXTM3U", "#EXT-X-VERSION:7"]);
    assert_eq!(
        lines[2],
        format!(
            "#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"audio\",NAME=\"camera\",LANGUAGE=\"und\",\
             DEFAULT=YES,AUTOSELECT=YES,CHANNELS=\"1\",URI=\"{audio_uri}\""
        )
    );
    assert!(
        lines[3].starts_with("#EXT-X-STREAM-INF:BANDWIDTH="),
        "{text}"
    );
    assert!(
        lines[3]
            .ends_with(",CODECS=\"hvc1.1.6.L150.B0,mp4a.40.2\",RESOLUTION=640x480,AUDIO=\"audio\""),
        "{text}"
    );
    assert_eq!(lines[4..], [video_uri.as_str()]);

    // The declared bit rate covers both playlists: 2 s fragments of 810
    // bytes of video and 750 bytes of audio, with their boxes.
    let bandwidth: u64 = lines[3]
        .split_once("BANDWIDTH=")
        .and_then(|(_, rest)| rest.split(',').next())
        .unwrap()
        .parse()
        .unwrap();
    assert!((6_000..20_000).contains(&bandwidth), "{bandwidth}");

    // Each named playlist is the stream's own, and both list six seconds.
    for uri in [video_uri, audio_uri] {
        let child = get(&app, uri.clone(), None).await;
        assert_eq!(child.status(), StatusCode::OK, "{uri}");
        let child = body_string(child).await;
        assert!(child.contains("#EXT-X-PLAYLIST-TYPE:VOD"), "{child}");
        let seconds: f64 = child
            .lines()
            .filter_map(|line| line.strip_prefix("#EXTINF:"))
            .map(|value| value.trim_end_matches(',').parse::<f64>().unwrap())
            .sum();
        assert!((seconds - 6.0).abs() < 0.001, "{uri}: {seconds}");
    }

    // A range without recordings has no playlist.
    let empty = get(
        &app,
        format!(
            "/api/cameras/cam1/hls/vod.m3u8?from={}&to={}",
            wall(100).get(),
            wall(101).get()
        ),
        None,
    )
    .await;
    assert_eq!(empty.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn a_camera_playlist_without_audio_declares_the_variant_alone() {
    let dir = tempfile::tempdir().unwrap();
    let catalog = CatalogHandle::open(&dir.path().join("c.db")).unwrap();
    catalog
        .ensure_camera(&key().camera, "Example", None)
        .await
        .unwrap();
    populate(dir.path(), &catalog).await;
    let app = router(state(dir.path(), catalog.clone(), vec!["t".into()]));
    let (from, to) = (wall(0).get(), wall(3).get());
    let response = get(
        &app,
        format!("/api/cameras/cam1/hls/vod.m3u8?from={from}&to={to}"),
        None,
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let text = body_string(response).await;
    assert!(!text.contains("EXT-X-MEDIA"), "{text}");
    assert!(
        text.contains(",CODECS=\"hvc1.1.6.L150.B0\",RESOLUTION=640x480\n"),
        "{text}"
    );
    assert!(text.ends_with(&format!(
        "/api/streams/cam1/main/hls/vod.m3u8?from={from}&to={to}\n"
    )));

    // The playlist is behind the token like the others.
    let bare = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!(
                    "/api/cameras/cam1/hls/vod.m3u8?from={from}&to={to}"
                ))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(bare.status(), StatusCode::UNAUTHORIZED);
}

/// The track id each fragment of an exported body names in its `tfhd`.
fn track_ids(body: &[u8]) -> Vec<u32> {
    body.windows(4)
        .enumerate()
        .filter(|(_, window)| window == b"tfhd")
        .map(|(at, _)| u32::from_be_bytes(body[at + 8..at + 12].try_into().unwrap()))
        .collect()
}

/// The media time of each edit list in an exported body, in track order.
fn edit_media_times(body: &[u8]) -> Vec<u64> {
    body.windows(4)
        .enumerate()
        .filter(|(_, window)| window == b"elst")
        // The type, version and flags, the count, the duration, the time.
        .map(|(at, _)| u64::from_be_bytes(body[at + 20..at + 28].try_into().unwrap()))
        .collect()
}

fn count_of(body: &[u8], kind: [u8; 4]) -> usize {
    body.windows(4).filter(|window| *window == kind).count()
}

/// A camera with three fragments of picture and, 5 ms into each, of sound,
/// behind a router; the directory lives as long as the recordings must.
async fn camera_with_sound() -> (tempfile::TempDir, axum::Router) {
    camera_with_sound_flagged(true).await
}

/// [`camera_with_sound`], with every audio frame flagged a sync sample or,
/// as a muxer for pictures would write them, only the first of a fragment.
async fn camera_with_sound_flagged(every_frame_sync: bool) -> (tempfile::TempDir, axum::Router) {
    camera_with_audio(|_, sound| {
        for sample in sound.samples.iter_mut().skip(1) {
            sample.is_sync = every_frame_sync;
        }
    })
    .await
}

/// A camera with three fragments of picture and three of sound, each of the
/// sound's as [`audio_fragment`] gives it and `adjust` leaves it.
async fn camera_with_audio(
    adjust: impl Fn(u64, &mut Fragment),
) -> (tempfile::TempDir, axum::Router) {
    camera_recording(&[0, 1, 2], adjust).await
}

/// A camera with the picture fragments `pictures`, each after a gap
/// starting a run, and three fragments of sound, each as
/// [`audio_fragment`] gives it and `adjust` leaves it.
async fn camera_recording(
    pictures: &[u64],
    adjust: impl Fn(u64, &mut Fragment),
) -> (tempfile::TempDir, axum::Router) {
    let dir = tempfile::tempdir().unwrap();
    let catalog = CatalogHandle::open(&dir.path().join("c.db")).unwrap();
    catalog
        .ensure_camera(&key().camera, "Example", None)
        .await
        .unwrap();
    let (video_entry, sound_entry) = (entry(), audio_entry());
    let video = pictures
        .iter()
        .enumerate()
        .map(|(at, &index)| {
            let mut picture = fragment(index, &video_entry);
            picture.new_run = at == 0 || pictures[at - 1] + 1 != index;
            picture
        })
        .collect();
    record_camera(
        dir.path(),
        &catalog,
        video,
        (0..3)
            .map(|i| {
                let mut sound = audio_fragment(i, &sound_entry);
                adjust(i, &mut sound);
                sound
            })
            .collect(),
    )
    .await;
    let app = router(state(dir.path(), catalog, vec!["t".into()]));
    (dir, app)
}

/// The scan of an exported body, with every checksum verified.
fn scan_of(bytes: &[u8]) -> iris_mp4::reader::ScanReport {
    iris_mp4::reader::scan(
        std::io::Cursor::new(bytes.to_vec()),
        iris_mp4::reader::ScanOptions {
            verify_crc: true,
            collect_samples: false,
        },
    )
    .unwrap()
}

/// A camera's export holds its main stream as track 1 and its recorded audio
/// as track 2: each video fragment is followed by the audio that starts
/// inside it, on one continuous timeline, with sequence numbers over both
/// tracks; the end of the range trims both.
#[tokio::test]
async fn a_camera_export_interleaves_recorded_audio_as_a_second_track() {
    let (_dir, app) = camera_with_sound().await;

    let (from, to) = (wall(0).get(), wall(3).get());
    let response = get(
        &app,
        format!("/api/cameras/cam1/export.mp4?from={from}&to={to}"),
        None,
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()[header::CONTENT_TYPE], "video/mp4");
    let disposition = response.headers()[header::CONTENT_DISPOSITION]
        .to_str()
        .unwrap()
        .to_owned();
    assert!(
        disposition.starts_with("attachment; filename=\"cam1-2"),
        "{disposition}"
    );
    let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let report = scan_of(&bytes);
    assert_eq!(report.stop, None);
    let init = &bytes[..report.init_len as usize];
    assert_eq!((count_of(init, *b"trak"), count_of(init, *b"trex")), (2, 2));
    assert_eq!((count_of(init, *b"elst"), count_of(init, *b"mp4a")), (2, 1));
    let facts: Vec<(u32, u64, u16)> = report
        .fragments
        .iter()
        .map(|f| (f.sequence, f.decode_time, f.sample_count))
        .collect();
    assert_eq!(
        facts,
        [
            (1, 0, 2),
            (2, 450, 125),
            (3, 180_000, 2),
            (4, 180_450, 125),
            (5, 360_000, 2),
            (6, 360_450, 125),
        ]
    );
    // The sound of a fragment runs 450 ticks past its picture and is cut
    // only where the export ends, with the last picture.
    let durations: Vec<u32> = report.fragments.iter().map(|f| f.duration).collect();
    assert_eq!(
        durations,
        [180_000, 180_000, 180_000, 180_000, 180_000, 179_550]
    );
    assert_eq!(track_ids(&bytes), [1, 2, 1, 2, 1, 2]);

    // Ending one second into the third video fragment: the picture keeps
    // one of its two frames and the audio the frames that start before then.
    let to = wall(2).offset(1_000_000).get();
    let response = get(
        &app,
        format!("/api/cameras/cam1/export.mp4?from={from}&to={to}"),
        None,
    )
    .await;
    let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let report = scan_of(&bytes);
    assert_eq!(report.stop, None);
    let (video_tail, audio_tail) = (&report.fragments[4], &report.fragments[5]);
    assert_eq!((video_tail.sample_count, video_tail.duration), (1, 90_000));
    // The audio starts 450 ticks in, so 89,550 ticks are left: 62 whole
    // frames and one cut short.
    assert_eq!(
        (audio_tail.decode_time, audio_tail.sample_count),
        (360_450, 63)
    );
    assert_eq!(audio_tail.duration, 89_550);
    assert_eq!(track_ids(&bytes), [1, 2, 1, 2, 1, 2]);

    // The stream's own export is the picture alone, as before.
    let alone = get(
        &app,
        format!("/api/streams/cam1/main/export.mp4?from={from}&to={to}"),
        None,
    )
    .await;
    let alone = to_bytes(alone.into_body(), usize::MAX).await.unwrap();
    assert_eq!(count_of(&alone, *b"trak"), 1);
    assert_eq!(track_ids(&alone), [1, 1, 1]);
}

/// An export that starts inside a fragment hides the picture's start with
/// an edit list and cuts the sound's: the audio loses the frames that start
/// before the export does, and its timeline starts at zero there with no
/// media time in its edit list, as a second track or as a stream alone.
#[tokio::test]
async fn an_export_cuts_the_start_of_its_sound() {
    let (_dir, app) = camera_with_sound().await;
    // Half a second into the first video fragment.
    let from = wall(0).offset(500_000).get();
    let to = wall(3).get();
    let response = get(
        &app,
        format!("/api/cameras/cam1/export.mp4?from={from}&to={to}"),
        None,
    )
    .await;
    let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let report = scan_of(&bytes);
    assert_eq!(report.stop, None);
    assert_eq!(edit_media_times(&bytes), [45_000, 0]);
    let facts: Vec<(u64, u16, bool)> = report
        .fragments
        .iter()
        .map(|f| (f.decode_time, f.sample_count, f.all_sync))
        .collect();
    // The audio starts 450 ticks into the video: 44,550 ticks before the
    // export does. Its frame 31 is the first to start at or after that, 90
    // ticks into the export.
    assert_eq!(
        facts,
        [
            (0, 2, false),
            (90, 94, true),
            (180_000, 2, false),
            (135_450, 125, true),
            (360_000, 2, false),
            (315_450, 125, true),
        ]
    );
    assert_eq!(track_ids(&bytes), [1, 2, 1, 2, 1, 2]);
    assert_eq!(
        report.fragments[1].wall,
        Some(wall(0).offset(5_000 + 31 * 16_000))
    );

    // The audio stream's own export starts the same way, as its one track.
    let sound = get(
        &app,
        format!("/api/streams/cam1/audio/export.mp4?from={from}&to={to}"),
        None,
    )
    .await;
    assert_eq!(sound.status(), StatusCode::OK);
    let sound = to_bytes(sound.into_body(), usize::MAX).await.unwrap();
    let report = scan_of(&sound);
    assert_eq!(report.stop, None);
    assert_eq!(edit_media_times(&sound), [0]);
    let facts: Vec<(u32, u64, u16, u32)> = report
        .fragments
        .iter()
        .map(|f| (f.sequence, f.decode_time, f.sample_count, f.duration))
        .collect();
    // The last fragment is cut at the range's end, 5 ms before its own.
    assert_eq!(
        facts,
        [
            (1, 90, 94, 135_360),
            (2, 135_450, 125, 180_000),
            (3, 315_450, 125, 179_550),
        ]
    );
    assert!(report.fragments.iter().all(|f| f.all_sync));
    assert_eq!(track_ids(&sound), [1, 1, 1]);
}

/// A sound fragment that flags a frame as not a sync sample cannot lose its
/// start: an export that starts inside one leaves it out and starts its
/// sound with the next fragment, as a second track and as a stream alone.
#[tokio::test]
async fn an_export_leaves_out_a_sound_fragment_it_cannot_cut() {
    let (_dir, app) = camera_with_sound_flagged(false).await;
    let from = wall(0).offset(500_000).get();
    let to = wall(3).get();
    let response = get(
        &app,
        format!("/api/cameras/cam1/export.mp4?from={from}&to={to}"),
        None,
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let report = scan_of(&bytes);
    assert_eq!(report.stop, None);
    let facts: Vec<(u32, u64, u16)> = report
        .fragments
        .iter()
        .map(|f| (f.track_id, f.decode_time, f.sample_count))
        .collect();
    assert_eq!(
        facts,
        [
            (1, 0, 2),
            (1, 180_000, 2),
            (2, 135_450, 125),
            (1, 360_000, 2),
            (2, 315_450, 125),
        ]
    );

    let sound = get(
        &app,
        format!("/api/streams/cam1/audio/export.mp4?from={from}&to={to}"),
        None,
    )
    .await;
    assert_eq!(sound.status(), StatusCode::OK);
    let sound = to_bytes(sound.into_body(), usize::MAX).await.unwrap();
    let report = scan_of(&sound);
    assert_eq!(report.stop, None);
    let facts: Vec<(u32, u64, u16)> = report
        .fragments
        .iter()
        .map(|f| (f.sequence, f.decode_time, f.sample_count))
        .collect();
    assert_eq!(facts, [(1, 135_450, 125), (2, 315_450, 125)]);
    // From the start of a fragment nothing is cut, and all of it is there.
    let whole = get(
        &app,
        format!(
            "/api/cameras/cam1/export.mp4?from={}&to={to}",
            wall(0).get()
        ),
        None,
    )
    .await;
    let whole = to_bytes(whole.into_body(), usize::MAX).await.unwrap();
    assert_eq!(track_ids(&whole), [1, 2, 1, 2, 1, 2]);
}

/// The track, decode time, and sample count of each fragment of an export.
async fn export_shape(app: &axum::Router, uri: String) -> Vec<(u32, u64, u16)> {
    let response = get(app, uri, None).await;
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let report = scan_of(&bytes);
    assert_eq!(report.stop, None);
    report
        .fragments
        .iter()
        .map(|f| (f.track_id, f.decode_time, f.sample_count))
        .collect()
}

/// Sound is as far into an export as into the video by the stored decode
/// times, which are exact on one run's timeline, and by the wall times when
/// the two disagree by more than a millisecond.
#[tokio::test]
async fn sound_is_placed_by_stored_decode_times_that_agree_with_the_walls() {
    let (from, to) = (wall(0).get(), wall(3).get());
    let uri = format!("/api/cameras/cam1/export.mp4?from={from}&to={to}");
    // Stored a tick before where the wall times put it: the stored time.
    let (_dir, app) = camera_with_audio(|_, sound| sound.decode_time -= 1).await;
    assert_eq!(
        export_shape(&app, uri.clone()).await,
        [
            (1, 0, 2),
            (2, 449, 125),
            (1, 180_000, 2),
            (2, 180_449, 125),
            (1, 360_000, 2),
            (2, 360_449, 125),
        ]
    );
    // Stored a millisecond and a tick away, as on another run's timeline:
    // the wall times.
    let (_dir, app) = camera_with_audio(|_, sound| sound.decode_time += 91).await;
    assert_eq!(
        export_shape(&app, uri.clone()).await[1],
        (2, 450, 125),
        "91 ticks later"
    );
    let (_dir, app) = camera_with_audio(|_, sound| sound.decode_time += 90).await;
    assert_eq!(
        export_shape(&app, uri.clone()).await[1],
        (2, 540, 125),
        "90 ticks later"
    );
    let (_dir, app) = camera_with_audio(|_, sound| sound.decode_time += 1_000_000).await;
    assert_eq!(export_shape(&app, uri).await[1], (2, 450, 125));
}

/// Sound has a place in an export only beside its picture: what was heard
/// before the first video fragment, or in a gap of the video, which the
/// export closes, is left out; sound that starts with its video fragment
/// to the tick is kept from its first frame.
#[tokio::test]
async fn sound_without_its_picture_is_left_out_of_an_export() {
    let (from, to) = (wall(0).get(), wall(3).get());
    let uri = format!("/api/cameras/cam1/export.mp4?from={from}&to={to}");
    // No picture for the second of the three fragments of sound.
    let (_dir, app) = camera_recording(&[0, 2], |_, _| {}).await;
    assert_eq!(
        export_shape(&app, uri.clone()).await,
        [(1, 0, 2), (2, 450, 125), (1, 180_000, 2), (2, 180_450, 125),]
    );
    // No picture for the first.
    let (_dir, app) = camera_recording(&[1, 2], |_, _| {}).await;
    assert_eq!(
        export_shape(&app, uri.clone()).await,
        [(1, 0, 2), (2, 450, 125), (1, 180_000, 2), (2, 180_450, 125),]
    );
    // Sound that starts where its video fragment does.
    let (_dir, app) = camera_with_audio(|index, sound| {
        sound.decode_time = index * 180_000;
        sound.wall = wall(index);
    })
    .await;
    assert_eq!(
        export_shape(&app, uri).await,
        [
            (1, 0, 2),
            (2, 0, 125),
            (1, 180_000, 2),
            (2, 180_000, 125),
            (1, 360_000, 2),
            (2, 360_000, 125),
        ]
    );
}

/// The playlist of a camera's audio is cut where the main stream's is: a
/// gap in the video starts a segment and a discontinuity in both, where
/// the audio alone, in one file, would be one segment.
#[tokio::test]
async fn the_audio_playlist_is_served_following_the_main_streams() {
    let (_dir, app) = camera_recording(&[0, 2], |_, _| {}).await;
    let (from, to) = (wall(0).get(), wall(3).get());
    let shape = |text: &str| -> Vec<String> {
        text.lines()
            .filter(|line| line.starts_with("#EXTINF") || *line == "#EXT-X-DISCONTINUITY")
            .map(str::to_owned)
            .collect()
    };
    let playlist = |kind: &'static str| {
        let app = app.clone();
        async move {
            let response = get(
                &app,
                format!("/api/streams/cam1/{kind}/hls/vod.m3u8?from={from}&to={to}"),
                None,
            )
            .await;
            assert_eq!(response.status(), StatusCode::OK, "{kind}");
            body_string(response).await
        }
    };
    let video = playlist("main").await;
    assert_eq!(
        shape(&video),
        [
            "#EXTINF:2.00000,",
            "#EXT-X-DISCONTINUITY",
            "#EXTINF:2.00000,"
        ]
    );
    // The sound of the gap belongs to the segment before it.
    let audio = playlist("audio").await;
    assert_eq!(
        shape(&audio),
        [
            "#EXTINF:4.00000,",
            "#EXT-X-DISCONTINUITY",
            "#EXTINF:2.00000,"
        ]
    );
    assert_eq!(audio.matches("#EXT-X-MAP").count(), 1, "{audio}");
}

/// Sound in two files, the second from a run of its own, follows the video
/// through both.
#[tokio::test]
async fn sound_in_two_files_is_exported_through_both() {
    let (_dir, app) = camera_with_audio(|index, sound| sound.new_run = index != 1).await;
    let (from, to) = (wall(0).get(), wall(3).get());
    assert_eq!(
        export_shape(
            &app,
            format!("/api/cameras/cam1/export.mp4?from={from}&to={to}")
        )
        .await,
        [
            (1, 0, 2),
            (2, 450, 125),
            (1, 180_000, 2),
            (2, 180_450, 125),
            (1, 360_000, 2),
            (2, 360_450, 125),
        ]
    );
    assert_eq!(
        export_shape(
            &app,
            format!("/api/streams/cam1/audio/export.mp4?from={from}&to={to}")
        )
        .await
        .len(),
        3
    );
}

/// A range inside one fragment cuts the sound at both ends: the frames
/// that start in the range, the last one shortened, with the wall time of
/// the first kept frame.
#[tokio::test]
async fn an_export_inside_one_fragment_cuts_the_sound_at_both_ends() {
    let (_dir, app) = camera_with_sound().await;
    let from = wall(0).offset(500_000).get();
    let to = wall(0).offset(1_500_000).get();
    // Frame 31 is the first to start in the range, at 501 ms; 999 ms are
    // left: 62 whole frames and one of 630 ticks.
    let first_kept = wall(0).offset(5_000 + 31 * 16_000);
    for (uri, track) in [
        (
            format!("/api/cameras/cam1/export.mp4?from={from}&to={to}"),
            2,
        ),
        (
            format!("/api/streams/cam1/audio/export.mp4?from={from}&to={to}"),
            1,
        ),
    ] {
        let response = get(&app, uri.clone(), None).await;
        assert_eq!(response.status(), StatusCode::OK, "{uri}");
        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let report = scan_of(&bytes);
        assert_eq!(report.stop, None);
        let sound = report.fragments.last().unwrap();
        assert_eq!(sound.track_id, track, "{uri}");
        assert_eq!(
            (sound.decode_time, sound.sample_count, sound.duration),
            (90, 63, 89_910),
            "{uri}"
        );
        assert_eq!(sound.wall, Some(first_kept), "{uri}");
        assert!(sound.all_sync);
    }
}

/// A camera without recorded audio exports its picture alone, and a range
/// without recordings has no export.
#[tokio::test]
async fn a_camera_export_without_audio_is_the_main_stream_alone() {
    let dir = tempfile::tempdir().unwrap();
    let catalog = CatalogHandle::open(&dir.path().join("c.db")).unwrap();
    catalog
        .ensure_camera(&key().camera, "Example", None)
        .await
        .unwrap();
    populate(dir.path(), &catalog).await;
    let app = router(state(dir.path(), catalog.clone(), vec!["t".into()]));
    let (from, to) = (wall(0).get(), wall(3).get());
    let camera = get(
        &app,
        format!("/api/cameras/cam1/export.mp4?from={from}&to={to}"),
        None,
    )
    .await;
    assert_eq!(camera.status(), StatusCode::OK);
    let camera = to_bytes(camera.into_body(), usize::MAX).await.unwrap();
    let stream = get(
        &app,
        format!("/api/streams/cam1/main/export.mp4?from={from}&to={to}"),
        None,
    )
    .await;
    let stream = to_bytes(stream.into_body(), usize::MAX).await.unwrap();
    assert_eq!(camera, stream);
    let empty = get(
        &app,
        format!(
            "/api/cameras/cam1/export.mp4?from={}&to={}",
            wall(100).get(),
            wall(101).get()
        ),
        None,
    )
    .await;
    assert_eq!(empty.status(), StatusCode::NOT_FOUND);
}
