//! Synthetic loopback coverage for WebSocket cancellation and send deadlines.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::Bytes;
use iris_api::{ActiveCamera, ApiState, ApiStats, LiveRegistry, ServeLimits, router};
use iris_catalog::CatalogHandle;
use iris_core::id::{CameraId, StreamKey, StreamKind};
use iris_core::time::WallUs;
use iris_ingest::{LiveFrame, LiveRing, StreamStats};
use iris_mp4::init::SampleEntry;
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

fn key() -> StreamKey {
    StreamKey {
        camera: CameraId::try_new("cam1").expect("valid camera id"),
        kind: StreamKind::Main,
    }
}

fn state(catalog: CatalogHandle, live: Arc<LiveRegistry>) -> Arc<ApiState> {
    let (events, _) = broadcast::channel(16);
    Arc::new(ApiState {
        catalog,
        live,
        events,
        tokens: Vec::new(),
        recording_roots: Vec::new(),
        public_metrics: false,
        snapshot_sources: HashMap::new(),
        active_cameras: HashMap::from([(
            key().camera,
            ActiveCamera {
                id: key().camera,
                name: "Synthetic".into(),
                vendor: Some("test".into()),
            },
        )]),
        stats: Arc::new(ApiStats::default()),
        started: WallUs::now(),
        started_monotonic: Instant::now(),
        shutdown: CancellationToken::new(),
    })
}

async fn start_server(state: Arc<ApiState>) -> (std::net::SocketAddr, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind loopback API");
    let address = listener.local_addr().expect("loopback address");
    let server = tokio::spawn(async move {
        iris_api::serve_listener(
            listener,
            router(state),
            ServeLimits::default(),
            CancellationToken::new(),
        )
        .await
        .expect("serve loopback API");
    });
    (address, server)
}

/// A sync frame below the live message limit that still outgrows the loopback
/// socket buffers, so a peer that stops reading blocks the send.
const BLOCKING_FRAME_BYTES: usize = 15 << 20;

async fn wait_until(timeout: Duration, mut predicate: impl FnMut() -> bool) {
    tokio::time::timeout(timeout, async {
        while !predicate() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("condition reached before deadline");
}

fn large_frame(payload_len: usize) -> Arc<LiveFrame> {
    frame(0, true, payload_len)
}

fn frame(decode_time: u64, is_sync: bool, payload_len: usize) -> Arc<LiveFrame> {
    let key = key();
    Arc::new(LiveFrame {
        key,
        sample_entry: Arc::new(SampleEntry::new(
            Bytes::from_static(b"\0\0\0\x10hvc1\0\0\0\0\0\0\0\0"),
            "hvc1.1.6.L150.B0".into(),
            640,
            480,
        )),
        data: Bytes::from(vec![0x55; payload_len]),
        duration: 3_000,
        is_sync,
        decode_time,
        wall: WallUs::new(1_700_000_000_000_000),
    })
}

async fn camera_live_viewers(client: &reqwest::Client, address: std::net::SocketAddr) -> u64 {
    let body = client
        .get(format!("http://{address}/api/cameras/cam1"))
        .send()
        .await
        .expect("camera detail request")
        .error_for_status()
        .expect("camera detail status")
        .bytes()
        .await
        .expect("camera detail body");
    let camera: serde_json::Value = serde_json::from_slice(&body).expect("camera detail JSON");
    camera["streams"][0]["live_viewers"]
        .as_u64()
        .expect("numeric live viewer count")
}

#[tokio::test]
async fn event_peer_close_and_shutdown_release_the_client_gauge() {
    let dir = tempfile::tempdir().expect("temporary catalog");
    let catalog = CatalogHandle::open(&dir.path().join("events.db")).expect("open catalog");
    let state = state(catalog.clone(), Arc::new(LiveRegistry::new()));
    let (address, server) = start_server(Arc::clone(&state)).await;

    let (mut first, _) = tokio_tungstenite::connect_async(format!("ws://{address}/api/events"))
        .await
        .expect("connect event WebSocket");
    wait_until(Duration::from_secs(1), || state.stats.event_clients() == 1).await;
    first
        .close(None)
        .await
        .expect("send event WebSocket close frame");
    wait_until(Duration::from_secs(1), || state.stats.event_clients() == 0).await;

    let (_second, _) = tokio_tungstenite::connect_async(format!("ws://{address}/api/events"))
        .await
        .expect("connect second event WebSocket");
    wait_until(Duration::from_secs(1), || state.stats.event_clients() == 1).await;
    state.shutdown.cancel();
    wait_until(Duration::from_secs(1), || state.stats.event_clients() == 0).await;

    server.abort();
    catalog.shutdown();
}

#[tokio::test]
async fn live_non_reader_is_released_by_the_send_deadline() {
    let dir = tempfile::tempdir().expect("temporary catalog");
    let catalog = CatalogHandle::open(&dir.path().join("deadline.db")).expect("open catalog");
    let live = Arc::new(LiveRegistry::new());
    let ring = LiveRing::new(4);
    live.insert(key(), ring.clone());
    let state = state(catalog.clone(), live);
    let (address, server) = start_server(Arc::clone(&state)).await;

    let (_socket, _) =
        tokio_tungstenite::connect_async(format!("ws://{address}/api/streams/cam1/main/live"))
            .await
            .expect("connect live WebSocket");
    wait_until(Duration::from_secs(1), || state.stats.live_clients() == 1).await;
    ring.publish(large_frame(BLOCKING_FRAME_BYTES));
    wait_until(Duration::from_secs(4), || state.stats.live_clients() == 0).await;
    assert_eq!(
        state.stats.live_oversized_closes(),
        0,
        "the send deadline, not the message limit, ends this session"
    );

    server.abort();
    catalog.shutdown();
}

#[tokio::test]
async fn shutdown_interrupts_a_blocked_live_send() {
    let dir = tempfile::tempdir().expect("temporary catalog");
    let catalog = CatalogHandle::open(&dir.path().join("shutdown.db")).expect("open catalog");
    let live = Arc::new(LiveRegistry::new());
    let ring = LiveRing::new(4);
    live.insert(key(), ring.clone());
    let state = state(catalog.clone(), live);
    let (address, server) = start_server(Arc::clone(&state)).await;

    let (_socket, _) =
        tokio_tungstenite::connect_async(format!("ws://{address}/api/streams/cam1/main/live"))
            .await
            .expect("connect live WebSocket");
    wait_until(Duration::from_secs(1), || state.stats.live_clients() == 1).await;
    ring.publish(large_frame(BLOCKING_FRAME_BYTES));
    tokio::time::sleep(Duration::from_millis(50)).await;
    state.shutdown.cancel();
    wait_until(Duration::from_secs(1), || state.stats.live_clients() == 0).await;
    assert_eq!(state.stats.live_oversized_closes(), 0);

    server.abort();
    catalog.shutdown();
}

#[tokio::test]
async fn camera_viewers_exclude_hls_and_track_two_external_websockets() {
    let dir = tempfile::tempdir().expect("temporary catalog");
    let catalog = CatalogHandle::open(&dir.path().join("viewers.db")).expect("open catalog");
    let live = Arc::new(LiveRegistry::new());
    let ring = LiveRing::new(8);
    live.insert(key(), ring.clone());
    live.insert_stats(key(), Arc::new(StreamStats::default()));
    let state = state(catalog.clone(), live);
    let (address, server) = start_server(Arc::clone(&state)).await;
    let client = reqwest::Client::new();

    let playlist_client = client.clone();
    let playlist = tokio::spawn(async move {
        playlist_client
            .get(format!(
                "http://{address}/api/streams/cam1/main/hls/live.m3u8"
            ))
            .send()
            .await
            .expect("live HLS playlist request")
    });
    wait_until(Duration::from_secs(1), || ring.subscriber_count() == 1).await;
    ring.publish(frame(0, true, 32));
    ring.publish(frame(3_000, false, 32));
    ring.publish(frame(6_000, true, 32));
    tokio::time::timeout(Duration::from_secs(1), playlist)
        .await
        .expect("playlist completed before deadline")
        .expect("playlist task")
        .error_for_status()
        .expect("playlist status");
    assert_eq!(camera_live_viewers(&client, address).await, 0);

    let (mut first, _) =
        tokio_tungstenite::connect_async(format!("ws://{address}/api/streams/cam1/main/live"))
            .await
            .expect("connect first live WebSocket");
    let (mut second, _) =
        tokio_tungstenite::connect_async(format!("ws://{address}/api/streams/cam1/main/live"))
            .await
            .expect("connect second live WebSocket");
    wait_until(Duration::from_secs(1), || state.stats.live_clients() == 2).await;
    assert_eq!(camera_live_viewers(&client, address).await, 2);

    first.close(None).await.expect("close first WebSocket");
    second.close(None).await.expect("close second WebSocket");
    wait_until(Duration::from_secs(1), || state.stats.live_clients() == 0).await;
    assert_eq!(camera_live_viewers(&client, address).await, 0);

    server.abort();
    catalog.shutdown();
}
