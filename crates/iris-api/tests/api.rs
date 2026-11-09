//! End-to-end tests over the router with a real catalog and recording in a
//! temporary directory.

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
use iris_mp4::fragment::{Fragment, Sample};
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
    let app = router(state);
    let response = get(&app, "/api/cameras/cam1".into(), None).await;
    assert_eq!(response.status(), StatusCode::OK);
    let text = body_string(response).await;
    assert!(text.contains("\"id\":\"cam1\""), "{text}");
    assert!(text.contains("\"name\":\"front\""), "{text}");
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
    catalog.shutdown();
}
