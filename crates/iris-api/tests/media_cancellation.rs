//! Release of process-wide media admission by stalled, slow, and disconnected
//! clients, through the serving loop on loopback sockets.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::Bytes;
use iris_api::{ApiState, ApiStats, LiveRegistry, ServeLimits, router};
use iris_catalog::{CatalogHandle, Change};
use iris_core::id::{CameraId, StreamKey, StreamKind};
use iris_core::time::WallUs;
use iris_mp4::fragment::{Fragment, Sample};
use iris_mp4::index::{MAX_PERSISTED_INDEX_RECORDS, read_index_bounded};
use iris_mp4::init::SampleEntry;
use iris_store::{ApprovedRoot, RootExpectation, StoragePolicy, WriterHandle};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::{TcpSocket, TcpStream};
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

const TOKEN: &str = "synthetic-valid-token";
/// Send buffer of the accepted sockets and receive buffer of the clients:
/// small enough that a reader which stops backs up into the server quickly.
const SOCKET_BUFFER_BYTES: u32 = 64 * 1024;

fn key() -> StreamKey {
    StreamKey {
        camera: CameraId::try_new("cam1").expect("camera id"),
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

fn wall(index: u64) -> WallUs {
    WallUs::new(1_700_000_000_000_000 + i64::try_from(index).expect("small index") * 2_000_000)
}

/// One fragment holding a sync sample of `payload` bytes.
fn fragment(index: u64, entry: &Arc<SampleEntry>, payload: usize) -> Fragment {
    let mut nal = u32::try_from(payload - 4)
        .expect("payload fits a NAL length")
        .to_be_bytes()
        .to_vec();
    nal.extend_from_slice(&[0x26, 1]);
    nal.resize(payload, 0xaa);
    Fragment {
        samples: vec![Sample {
            data: Bytes::from(nal),
            duration: 180_000,
            is_sync: true,
        }],
        decode_time: index * 180_000,
        wall: wall(index),
        raw_rtp: 0,
        new_run: index == 0,
        sample_entry: Arc::clone(entry),
    }
}

/// Records `count` fragments of `payload` bytes beneath a fresh root in `dir`
/// and registers the sealed file; returns the root, segment id, and length.
async fn record(
    dir: &Path,
    catalog: &CatalogHandle,
    count: u64,
    payload: usize,
) -> (ApprovedRoot, i64, u64) {
    let root = iris_store::admit_recording_root(&dir.join("video"), RootExpectation::Fresh)
        .expect("recording root");
    let entry = entry();
    let writer = WriterHandle::spawn(
        root.path().to_path_buf(),
        StoragePolicy {
            sync_interval: Duration::from_secs(5),
            preallocate_bytes: 1 << 20,
            rotate_bytes: 1 << 30,
            rotate_age: Duration::from_secs(3600),
        },
        64,
    )
    .expect("writer");
    for index in 0..count {
        writer
            .try_send(key(), fragment(index, &entry, payload))
            .expect("queue fragment");
    }
    tokio::task::spawn_blocking(move || writer.shutdown())
        .await
        .expect("writer join")
        .expect("writer shutdown");
    let path = only_recording(root.path());
    let (header, records) = read_index_bounded(
        &iris_store::layout::index_path(&path),
        MAX_PERSISTED_INDEX_RECORDS,
    )
    .expect("written sidecar");
    let last = *records.last().expect("records");
    catalog.notify(Change::SegmentOpened {
        key: key(),
        path: path.clone(),
        run_seq: header.run_id,
        discontinuity: true,
        wall_start: records[0].wall,
        media_start: records[0].decode_time,
        init_len: header.init_len,
        sample_entry: entry,
    });
    catalog.notify(Change::SegmentSealed {
        path: path.clone(),
        len: last.end(),
        fragments: u32::try_from(records.len()).expect("record count"),
        wall_end: last.wall.offset(2_000_000),
        media_end: last.decode_time + u64::from(last.duration),
    });
    let id = catalog
        .segment_by_path(&path)
        .await
        .expect("catalog query")
        .expect("segment row")
        .id
        .0;
    (root, id, last.end())
}

fn only_recording(root: &Path) -> PathBuf {
    let mut stack = vec![root.to_path_buf()];
    let mut found = Vec::new();
    while let Some(directory) = stack.pop() {
        for entry in std::fs::read_dir(directory).expect("read directory") {
            let path = entry.expect("directory entry").path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().is_some_and(|extension| extension == "mp4") {
                found.push(path);
            }
        }
    }
    assert_eq!(found.len(), 1, "{found:?}");
    found.remove(0)
}

fn state(root: ApprovedRoot, catalog: CatalogHandle, stats: ApiStats) -> Arc<ApiState> {
    let (events, _) = broadcast::channel(1);
    Arc::new(ApiState {
        catalog,
        live: Arc::new(LiveRegistry::new()),
        events,
        tokens: vec![TOKEN.to_owned()],
        capability_secret: iris_api::CapabilitySecret::new([1; 32]),
        recording_roots: vec![root],
        public_metrics: false,
        snapshot_sources: std::sync::RwLock::new(HashMap::new()),
        active_cameras: std::sync::RwLock::new(HashMap::new()),
        control: None,
        stats: Arc::new(stats),
        started: WallUs::now(),
        started_monotonic: Instant::now(),
        shutdown: CancellationToken::new(),
    })
}

/// Serves `state` on loopback; accepted sockets inherit the listener's small
/// send buffer.
fn serve(state: Arc<ApiState>, limits: ServeLimits) -> SocketAddr {
    let socket = TcpSocket::new_v4().expect("socket");
    socket
        .set_send_buffer_size(SOCKET_BUFFER_BYTES)
        .expect("send buffer");
    socket
        .bind("127.0.0.1:0".parse().expect("literal address"))
        .expect("bind loopback");
    let listener = socket.listen(64).expect("listen");
    let address = listener.local_addr().expect("listener address");
    let shutdown = state.shutdown.clone();
    tokio::spawn(iris_api::serve_listener(
        listener,
        router(state),
        limits,
        shutdown,
    ));
    address
}

/// Sends one authenticated request on a client with a small receive buffer and
/// returns the stream once the response head has arrived, plus the head.
async fn request_head(address: SocketAddr, path: &str, extra: &str) -> (TcpStream, String) {
    let socket = TcpSocket::new_v4().expect("socket");
    socket
        .set_recv_buffer_size(SOCKET_BUFFER_BYTES)
        .expect("receive buffer");
    let mut stream = socket.connect(address).await.expect("connect");
    stream
        .write_all(
            format!(
                "GET {path} HTTP/1.1\r\nHost: {address}\r\nAuthorization: Bearer {TOKEN}\r\n{extra}\r\n"
            )
            .as_bytes(),
        )
        .await
        .expect("request");
    let mut head = Vec::new();
    let mut byte = [0_u8; 1];
    tokio::time::timeout(Duration::from_secs(5), async {
        while !head.ends_with(b"\r\n\r\n") {
            stream.read_exact(&mut byte).await.expect("response head");
            head.push(byte[0]);
        }
    })
    .await
    .expect("response head deadline");
    (stream, String::from_utf8(head).expect("ASCII head"))
}

fn status(head: &str) -> u16 {
    head.split_whitespace()
        .nth(1)
        .and_then(|status| status.parse().ok())
        .expect("status code")
}

/// One complete request on its own connection; returns the head and body length.
async fn request(address: SocketAddr, path: &str, extra: &str) -> (String, usize) {
    let (mut stream, head) =
        request_head(address, path, &format!("{extra}Connection: close\r\n")).await;
    let mut body = Vec::new();
    tokio::time::timeout(Duration::from_secs(10), stream.read_to_end(&mut body))
        .await
        .expect("response body deadline")
        .expect("response body");
    (head, body.len())
}

#[tokio::test]
async fn a_stalled_reader_keeps_its_streaming_slot_only_until_the_write_deadline() {
    let dir = tempfile::tempdir().expect("temporary directory");
    let catalog = CatalogHandle::open(&dir.path().join("catalog.db")).expect("catalog");
    // Eight MiB outgrow every kernel and server buffer between the file and a
    // reader that has stopped.
    let (root, id, len) = record(dir.path(), &catalog, 8, 1 << 20).await;
    let deadline = Duration::from_secs(1);
    let limits = ServeLimits {
        write_stall_timeout: deadline,
        ..ServeLimits::default()
    };
    let state = state(root, catalog.clone(), ApiStats::new_with_media_limits(1, 4));
    let address = serve(Arc::clone(&state), limits);
    let file = format!("/api/segments/{id}/file.mp4");

    let (mut stalled, head) = request_head(address, &file, "").await;
    let stalled_at = Instant::now();
    assert_eq!(status(&head), 200, "{head}");

    let (busy, _) = request(address, &file, "").await;
    assert_eq!(status(&busy), 503, "{busy}");
    assert!(busy.contains("retry-after: 1\r\n"), "{busy}");
    let (range, range_len) = request(address, &file, "Range: bytes=0-1023\r\n").await;
    assert_eq!(
        status(&range),
        206,
        "bounded responses use their own class: {range}"
    );
    assert_eq!(range_len, 1024);
    assert!(state.stats.media_rejected() >= 1);

    let admitted = tokio::time::timeout(Duration::from_secs(8), async {
        loop {
            let (head, body_len) = request(address, &file, "").await;
            if status(&head) == 200 {
                break body_len;
            }
            assert_eq!(status(&head), 503, "{head}");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("the stalled reader's slot returned after the write deadline");
    assert_eq!(admitted, usize::try_from(len).expect("length fits usize"));
    assert!(
        stalled_at.elapsed() >= Duration::from_millis(900),
        "the slot stayed held until the one-second deadline: {:?}",
        stalled_at.elapsed()
    );

    // The server closed the stalled connection: draining it reaches its end
    // without the whole body.
    let mut drained = Vec::new();
    let ended = tokio::time::timeout(Duration::from_secs(10), stalled.read_to_end(&mut drained))
        .await
        .expect("the stalled connection ends");
    assert!(ended.is_err() || drained.len() < usize::try_from(len).expect("length"));
    catalog.shutdown();
}

#[tokio::test]
async fn a_slow_reader_that_keeps_reading_is_served_to_the_end() {
    let dir = tempfile::tempdir().expect("temporary directory");
    let catalog = CatalogHandle::open(&dir.path().join("catalog.db")).expect("catalog");
    let (root, id, len) = record(dir.path(), &catalog, 4, 512 * 1024).await;
    let limits = ServeLimits {
        write_stall_timeout: Duration::from_millis(500),
        ..ServeLimits::default()
    };
    let state = state(root, catalog.clone(), ApiStats::new_with_media_limits(1, 4));
    let address = serve(state, limits);
    let (mut stream, head) = request_head(
        address,
        &format!("/api/segments/{id}/file.mp4"),
        "Connection: close\r\n",
    )
    .await;
    assert_eq!(status(&head), 200, "{head}");
    let mut received = 0_usize;
    let mut chunk = vec![0_u8; 64 * 1024];
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            // Each pause is far shorter than the deadline; the whole transfer
            // spans many deadlines.
            tokio::time::sleep(Duration::from_millis(50)).await;
            match stream.read(&mut chunk).await.expect("slow read") {
                0 => break,
                read => received += read,
            }
        }
    })
    .await
    .expect("slow transfer deadline");
    assert_eq!(received, usize::try_from(len).expect("length fits usize"));
    catalog.shutdown();
}

#[tokio::test]
async fn a_disconnected_reader_returns_its_streaming_slot() {
    let dir = tempfile::tempdir().expect("temporary directory");
    let catalog = CatalogHandle::open(&dir.path().join("catalog.db")).expect("catalog");
    let (root, id, _) = record(dir.path(), &catalog, 8, 1 << 20).await;
    let state = state(root, catalog.clone(), ApiStats::new_with_media_limits(1, 4));
    let address = serve(state, ServeLimits::default());
    let file = format!("/api/segments/{id}/file.mp4");

    let (stalled, head) = request_head(address, &file, "").await;
    assert_eq!(status(&head), 200, "{head}");
    let (busy, _) = request(address, &file, "").await;
    assert_eq!(status(&busy), 503, "{busy}");
    drop(stalled);
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let (head, _) = request(address, &file, "Range: bytes=0-\r\n").await;
            if status(&head) == 206 {
                break;
            }
            assert_eq!(status(&head), 503, "{head}");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("admission returned after the reader disconnected");
    catalog.shutdown();
}
