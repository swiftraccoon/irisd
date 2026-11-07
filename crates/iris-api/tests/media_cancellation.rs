//! Cancellation regressions for process-wide media-work admission.

use std::collections::HashMap;
use std::fs;
use std::net::SocketAddr;
use std::path::Path;
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::Bytes;
use iris_api::{ApiState, ApiStats, LiveRegistry, router};
use iris_catalog::{CatalogHandle, Change};
use iris_core::id::{CameraId, StreamKey, StreamKind};
use iris_core::time::WallUs;
use iris_mp4::init::SampleEntry;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::TcpStream;
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

const TOKEN: &str = "synthetic-valid-token";

fn task_count() -> usize {
    fs::read_dir("/proc/self/task")
        .expect("process task directory")
        .count()
}

fn stream_key() -> StreamKey {
    StreamKey {
        camera: CameraId::try_new("cam1").expect("camera id"),
        kind: StreamKind::Main,
    }
}

fn make_fifo(path: &Path) {
    let status = Command::new("mkfifo")
        .arg(path)
        .status()
        .expect("run mkfifo");
    assert!(status.success(), "mkfifo failed");
}

async fn connect_and_write(address: SocketAddr, path: &str) -> TcpStream {
    let mut stream = TcpStream::connect(address).await.expect("loopback connect");
    stream
        .write_all(
            format!(
                "GET {path} HTTP/1.1\r\nHost: {address}\r\nAuthorization: Bearer {TOKEN}\r\nConnection: close\r\n\r\n"
            )
            .as_bytes(),
        )
        .await
        .expect("request write");
    stream.flush().await.expect("request flush");
    stream
}

async fn request_status(address: SocketAddr, path: &str) -> u16 {
    let mut stream = connect_and_write(address, path).await;
    let mut response = Vec::new();
    tokio::time::timeout(Duration::from_secs(2), stream.read_to_end(&mut response))
        .await
        .expect("HTTP response deadline")
        .expect("HTTP response read");
    let status = std::str::from_utf8(&response)
        .expect("HTTP response text")
        .split_whitespace()
        .nth(1)
        .expect("HTTP status");
    status.parse().expect("numeric HTTP status")
}

#[tokio::test]
async fn authenticated_disconnects_keep_blocking_media_work_within_eight_slots() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let database = directory.path().join("catalog.db");
    let media = directory.path().join("synthetic.mp4");
    fs::write(&media, []).expect("synthetic media");
    let index = media.with_extension("idx");
    make_fifo(&index);

    let catalog = CatalogHandle::open(&database).expect("catalog");
    let sample_entry = Arc::new(SampleEntry::new(
        Bytes::from_static(b"\0\0\0\x10hvc1\0\0\0\0\0\0\0\0"),
        "hvc1".into(),
        16,
        16,
    ));
    catalog.notify(Change::SegmentOpened {
        key: stream_key(),
        path: media.clone(),
        run_seq: 1,
        discontinuity: true,
        wall_start: WallUs::new(1),
        media_start: 0,
        init_len: 16,
        sample_entry,
    });
    catalog.total_bytes().await.expect("catalog flush");
    let segment_id = catalog
        .segment_by_path(&media)
        .await
        .expect("catalog query")
        .expect("segment row")
        .id
        .0;

    let (events, _) = broadcast::channel(1);
    let shutdown = CancellationToken::new();
    let state = Arc::new(ApiState {
        catalog: catalog.clone(),
        live: Arc::new(LiveRegistry::new()),
        events,
        tokens: vec![TOKEN.to_owned()],
        snapshot_sources: HashMap::new(),
        active_cameras: HashMap::new(),
        stats: Arc::new(ApiStats::default()),
        started: WallUs::now(),
        started_monotonic: Instant::now(),
        shutdown: shutdown.clone(),
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("listener");
    let address = listener.local_addr().expect("listener address");
    let server = tokio::spawn(async move {
        axum::serve(listener, router(state))
            .with_graceful_shutdown(shutdown.cancelled_owned())
            .await
            .expect("server");
    });

    let baseline_tasks = task_count();
    let path = format!("/api/segments/{segment_id}/file.mp4");
    for expected in 1..=8 {
        let connection = connect_and_write(address, &path).await;
        tokio::time::timeout(Duration::from_secs(2), async {
            while task_count() < baseline_tasks + expected {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("sidecar read entered blocking work");
        drop(connection);
    }
    assert_eq!(
        request_status(address, &path).await,
        503,
        "a ninth disconnected request must not start more blocking work"
    );
    assert_eq!(task_count(), baseline_tasks + 8);

    let writer_path = index.clone();
    std::thread::spawn(move || {
        let _ = fs::OpenOptions::new().write(true).open(writer_path);
    })
    .join()
    .expect("FIFO writer");
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let status = request_status(
                address,
                "/api/streams/cam1/main/keyframe?t=1970-01-01T00:00:00Z",
            )
            .await;
            if status != 503 {
                assert_eq!(status, 404);
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("media admission recovered after blocking work ended");

    server.abort();
    let _ = server.await;
    catalog.shutdown();
}
