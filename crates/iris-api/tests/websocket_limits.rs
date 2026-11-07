//! WebSocket admission and message limits over loopback: inbound data closes
//! with 1008, socket caps answer 503, oversized live frames are skipped or
//! close with 1009, and a CRC-32C collision still announces its own init segment.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::Bytes;
use futures_util::{SinkExt as _, StreamExt as _};
use iris_api::{
    ApiState, ApiStats, LiveRegistry, MAX_LIVE_MESSAGE_BYTES, MSG_FRAGMENT, MSG_INIT, ServeLimits,
    router,
};
use iris_catalog::CatalogHandle;
use iris_core::id::{CameraId, StreamKey, StreamKind};
use iris_core::time::WallUs;
use iris_ingest::{LiveFrame, LiveRing};
use iris_mp4::init::{SampleEntry, build_init_segment};
use tokio::net::TcpStream;
use tokio::sync::broadcast;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use tokio_tungstenite::tungstenite::protocol::frame::Frame;
use tokio_tungstenite::tungstenite::protocol::frame::coding::{CloseCode, Data, OpCode};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};
use tokio_util::sync::CancellationToken;

type Client = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// Streams registered with a live ring: enough to exceed the process-wide cap
/// at sixteen sockets each.
const STREAMS: usize = 5;

fn key(camera: usize) -> StreamKey {
    StreamKey {
        camera: CameraId::try_new(&format!("cam{camera}")).expect("camera id"),
        kind: StreamKind::Main,
    }
}

fn entry() -> Arc<SampleEntry> {
    Arc::new(SampleEntry::new(
        Bytes::from_static(b"\0\0\0\x10hvc1\0\0\0\0\0\0\0\0"),
        "hvc1.1.6.L150.B0".into(),
        640,
        480,
    ))
}

fn frame(
    entry: &Arc<SampleEntry>,
    decode_time: u64,
    is_sync: bool,
    payload: usize,
) -> Arc<LiveFrame> {
    let mut data = vec![0x55_u8; payload];
    data[4] = if is_sync { 0x26 } else { 0x02 };
    Arc::new(LiveFrame {
        key: key(1),
        sample_entry: Arc::clone(entry),
        data: Bytes::from(data),
        duration: 3_000,
        is_sync,
        decode_time,
        wall: WallUs::new(
            1_700_000_000_000_000 + i64::try_from(decode_time).expect("small") * 100 / 9,
        ),
    })
}

struct Server {
    address: SocketAddr,
    state: Arc<ApiState>,
    rings: Vec<LiveRing>,
    _dir: tempfile::TempDir,
}

async fn server() -> Server {
    let dir = tempfile::tempdir().expect("temporary directory");
    let catalog = CatalogHandle::open(&dir.path().join("catalog.db")).expect("catalog");
    let live = Arc::new(LiveRegistry::new());
    let rings: Vec<LiveRing> = (1..=STREAMS)
        .map(|camera| {
            let ring = LiveRing::new(16);
            live.insert(key(camera), ring.clone());
            ring
        })
        .collect();
    let (events, _) = broadcast::channel(16);
    let state = Arc::new(ApiState {
        catalog,
        live,
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
    });
    let listener = iris_api::bind("127.0.0.1:0".parse().expect("literal address"))
        .await
        .expect("bind loopback");
    let address = listener.local_addr().expect("listener address");
    tokio::spawn(iris_api::serve_listener(
        listener,
        router(Arc::clone(&state)),
        ServeLimits::default(),
        state.shutdown.clone(),
    ));
    Server {
        address,
        state,
        rings,
        _dir: dir,
    }
}

/// Connects with the viewer's inbound limit: one message, in one frame, of at
/// most [`MAX_LIVE_MESSAGE_BYTES`].
async fn connect(address: SocketAddr, path: &str) -> Client {
    let config = WebSocketConfig::default()
        .max_message_size(Some(MAX_LIVE_MESSAGE_BYTES))
        .max_frame_size(Some(MAX_LIVE_MESSAGE_BYTES));
    tokio_tungstenite::connect_async_with_config(
        format!("ws://{address}{path}"),
        Some(config),
        false,
    )
    .await
    .expect("WebSocket upgrade")
    .0
}

/// The HTTP status and `Retry-After` of an upgrade the server refuses.
async fn refused(address: SocketAddr, path: &str) -> (u16, Option<String>) {
    match tokio_tungstenite::connect_async(format!("ws://{address}{path}")).await {
        Err(tokio_tungstenite::tungstenite::Error::Http(response)) => (
            response.status().as_u16(),
            response
                .headers()
                .get("retry-after")
                .and_then(|value| value.to_str().ok())
                .map(str::to_owned),
        ),
        Err(error) => panic!("unexpected upgrade failure: {error}"),
        Ok(_) => panic!("the upgrade was admitted"),
    }
}

async fn wait_until(mut predicate: impl FnMut() -> bool) {
    tokio::time::timeout(Duration::from_secs(5), async {
        while !predicate() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("condition reached before the deadline");
}

async fn next_message(client: &mut Client) -> Option<Message> {
    tokio::time::timeout(Duration::from_secs(5), client.next())
        .await
        .expect("message deadline")
        .and_then(Result::ok)
}

/// The next binary message's type byte and payload.
async fn next_binary(client: &mut Client) -> (u8, Bytes) {
    match next_message(client).await {
        Some(Message::Binary(bytes)) => (bytes[0], bytes.slice(1..)),
        other => panic!("expected a binary live message, got {other:?}"),
    }
}

fn decode_time(fragment: &[u8]) -> u64 {
    iris_mp4::reader::parse_fragment(fragment)
        .expect("a valid fragment")
        .decode_time
}

async fn expect_close(client: &mut Client, code: CloseCode) {
    match next_message(client).await {
        Some(Message::Close(Some(frame))) => assert_eq!(frame.code, code, "{frame:?}"),
        other => panic!("expected a {code:?} close, got {other:?}"),
    }
}

#[tokio::test]
async fn a_data_message_from_a_live_client_closes_with_policy_violation() {
    let server = server().await;
    let mut client = connect(server.address, "/api/streams/cam1/main/live").await;
    wait_until(|| server.state.stats.live_clients() == 1).await;
    client
        .send(Message::Text("hello".into()))
        .await
        .expect("send text");
    expect_close(&mut client, CloseCode::Policy).await;
    wait_until(|| server.state.stats.live_clients() == 0).await;
}

#[tokio::test]
async fn an_oversized_fragmented_event_message_closes_with_policy_violation() {
    let server = server().await;
    let mut client = connect(server.address, "/api/events").await;
    wait_until(|| server.state.stats.event_clients() == 1).await;
    // Two frames within the 4 KiB frame limit whose message exceeds it.
    for (opcode, last) in [
        (OpCode::Data(Data::Binary), false),
        (OpCode::Data(Data::Continue), true),
    ] {
        client
            .send(Message::Frame(Frame::message(
                vec![7_u8; 3 * 1024],
                opcode,
                last,
            )))
            .await
            .expect("send fragment");
    }
    expect_close(&mut client, CloseCode::Policy).await;
    wait_until(|| server.state.stats.event_clients() == 0).await;
}

#[tokio::test]
async fn an_oversized_inbound_frame_closes_the_socket() {
    let server = server().await;
    let mut client = connect(server.address, "/api/events").await;
    wait_until(|| server.state.stats.event_clients() == 1).await;
    // Small enough to fit the socket buffers, so the send itself completes.
    let _ = client
        .send(Message::Binary(Bytes::from(vec![0_u8; 16 * 1024])))
        .await;
    // The server stops reading at the frame header, so the close frame may be
    // overtaken by a reset; either way the socket ends promptly.
    match next_message(&mut client).await {
        None | Some(Message::Close(None)) => {}
        Some(Message::Close(Some(frame))) => assert_eq!(frame.code, CloseCode::Policy),
        Some(other) => panic!("unexpected message {other:?}"),
    }
    wait_until(|| server.state.stats.event_clients() == 0).await;
}

#[tokio::test]
async fn live_sockets_are_capped_per_stream_and_process_wide() {
    let server = server().await;
    let mut clients = Vec::new();
    for _ in 0..16 {
        clients.push(connect(server.address, "/api/streams/cam1/main/live").await);
    }
    assert_eq!(
        refused(server.address, "/api/streams/cam1/main/live").await,
        (503, Some("1".to_owned()))
    );
    for camera in 2..=4 {
        for _ in 0..16 {
            clients.push(
                connect(
                    server.address,
                    &format!("/api/streams/cam{camera}/main/live"),
                )
                .await,
            );
        }
    }
    assert_eq!(clients.len(), 64);
    assert_eq!(
        refused(server.address, "/api/streams/cam5/main/live").await,
        (503, Some("1".to_owned())),
        "the sixty-fifth live socket exceeds the process-wide cap"
    );
    assert_eq!(server.state.stats.websocket_rejected(), 2);

    let mut leaving = clients.pop().expect("an admitted client");
    leaving.close(None).await.expect("close one socket");
    wait_until(|| server.state.stats.live_clients() == 63).await;
    clients.push(connect(server.address, "/api/streams/cam5/main/live").await);
    drop(clients);
}

#[tokio::test]
async fn event_sockets_are_capped_process_wide() {
    let server = server().await;
    let mut clients = Vec::new();
    for _ in 0..32 {
        clients.push(connect(server.address, "/api/events").await);
    }
    assert_eq!(
        refused(server.address, "/api/events").await,
        (503, Some("1".to_owned()))
    );
    assert_eq!(server.state.stats.websocket_rejected(), 1);
    drop(clients);
}

#[tokio::test]
async fn an_oversized_dependent_frame_is_skipped_until_the_next_sync_frame() {
    let server = server().await;
    let ring = &server.rings[0];
    let entry = entry();
    let mut client = connect(server.address, "/api/streams/cam1/main/live").await;
    wait_until(|| ring.subscriber_count() == 1).await;

    ring.publish(frame(&entry, 0, true, 64));
    assert_eq!(next_binary(&mut client).await.0, MSG_INIT);
    let (kind, fragment) = next_binary(&mut client).await;
    assert_eq!((kind, decode_time(&fragment)), (MSG_FRAGMENT, 0));

    ring.publish(frame(&entry, 3_000, false, 20 << 20));
    ring.publish(frame(&entry, 6_000, false, 64));
    ring.publish(frame(&entry, 9_000, true, 64));
    let (kind, init) = next_binary(&mut client).await;
    assert_eq!(kind, MSG_INIT, "the stream restarts with an init segment");
    assert_eq!(init, build_init_segment(&entry));
    let (kind, fragment) = next_binary(&mut client).await;
    assert_eq!(
        (kind, decode_time(&fragment)),
        (MSG_FRAGMENT, 9_000),
        "the oversized frame and its dependents are skipped"
    );
    assert_eq!(server.state.stats.live_oversized_closes(), 0);
}

#[tokio::test]
async fn an_oversized_sync_frame_closes_with_message_too_big() {
    let server = server().await;
    let ring = &server.rings[0];
    let entry = entry();
    let mut client = connect(server.address, "/api/streams/cam1/main/live").await;
    wait_until(|| ring.subscriber_count() == 1).await;

    ring.publish(frame(&entry, 0, true, 20 << 20));
    let (kind, _) = next_binary(&mut client).await;
    assert_eq!(kind, MSG_INIT);
    expect_close(&mut client, CloseCode::Size).await;
    wait_until(|| server.state.stats.live_clients() == 0).await;
    assert_eq!(server.state.stats.live_oversized_closes(), 1);
}

#[tokio::test]
async fn a_frame_at_the_message_limit_is_delivered() {
    let server = server().await;
    let ring = &server.rings[0];
    let entry = entry();
    let mut client = connect(server.address, "/api/streams/cam1/main/live").await;
    wait_until(|| ring.subscriber_count() == 1).await;

    ring.publish(frame(&entry, 0, true, 64));
    assert_eq!(next_binary(&mut client).await.0, MSG_INIT);
    let (_, small) = next_binary(&mut client).await;
    let overhead = 1 + small.len() - 64;
    ring.publish(frame(
        &entry,
        3_000,
        true,
        MAX_LIVE_MESSAGE_BYTES - overhead,
    ));
    let (kind, fragment) = next_binary(&mut client).await;
    assert_eq!(kind, MSG_FRAGMENT);
    assert_eq!(1 + fragment.len(), MAX_LIVE_MESSAGE_BYTES);
}

/// CRC-32C (Castagnoli, reflected polynomial 0x82F63B78) lookup table.
fn crc32c_table() -> [u32; 256] {
    let mut table = [0_u32; 256];
    for (index, slot) in table.iter_mut().enumerate() {
        let mut crc = u32::try_from(index).expect("table index fits u32");
        for _ in 0..8 {
            crc = if crc & 1 == 0 {
                crc >> 1
            } else {
                (crc >> 1) ^ 0x82f6_3b78
            };
        }
        *slot = crc;
    }
    table
}

/// Appends four bytes to `prefix` so that the result's CRC-32C is `target`,
/// running the register backwards from the target.
fn forge_crc32c(prefix: &[u8], target: u32) -> Vec<u8> {
    let table = crc32c_table();
    let mut reverse = [0_u32; 256];
    for (index, value) in table.iter().enumerate() {
        reverse[(value >> 24) as usize] =
            (value << 8) ^ u32::try_from(index).expect("table index fits u32");
    }
    let mut register = 0xffff_ffff_u32;
    for &byte in prefix {
        register = (register >> 8) ^ table[((register ^ u32::from(byte)) & 0xff) as usize];
    }
    let mut back = target ^ 0xffff_ffff;
    for _ in 0..4 {
        back = (back << 8) ^ reverse[(back >> 24) as usize];
    }
    let mut forged = prefix.to_vec();
    forged.extend_from_slice(&(back ^ register).to_le_bytes());
    forged
}

#[tokio::test]
async fn a_crc_colliding_sample_entry_resends_the_init_segment() {
    let first = b"\0\0\0\x10hvc1\0\0\0\0\0\0\0\0parameter-set-epoch-A".to_vec();
    let second = forge_crc32c(
        b"\0\0\0\x10hvc1\0\0\0\0\0\0\0\0parameter-set-epoch-B",
        iris_mp4::crc::crc32c(&first),
    );
    let first = Arc::new(SampleEntry::new(
        Bytes::from(first),
        "hvc1".into(),
        640,
        480,
    ));
    let second = Arc::new(SampleEntry::new(
        Bytes::from(second),
        "hvc1".into(),
        640,
        480,
    ));
    assert_eq!(first.hash, second.hash, "forged CRC-32C collision");
    assert_ne!(first.bytes, second.bytes);

    let server = server().await;
    let ring = &server.rings[0];
    let mut client = connect(server.address, "/api/streams/cam1/main/live").await;
    wait_until(|| ring.subscriber_count() == 1).await;
    ring.publish(frame(&first, 0, true, 64));
    assert_eq!(
        next_binary(&mut client).await,
        (MSG_INIT, build_init_segment(&first))
    );
    assert_eq!(next_binary(&mut client).await.0, MSG_FRAGMENT);

    ring.publish(frame(&second, 3_000, true, 64));
    assert_eq!(
        next_binary(&mut client).await,
        (MSG_INIT, build_init_segment(&second)),
        "equal hashes must not hide a different sample entry"
    );
    let (kind, fragment) = next_binary(&mut client).await;
    assert_eq!((kind, decode_time(&fragment)), (MSG_FRAGMENT, 3_000));
}
