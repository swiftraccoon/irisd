//! Media is served only from descriptors opened beneath an approved recording
//! root and bound to the validated inode: symlinks, swapped paths, truncation,
//! and rows outside every root never yield foreign or partial-but-clean bodies.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode};
use bytes::Bytes;
use iris_api::{ApiState, ApiStats, LiveRegistry, router};
use iris_catalog::{CatalogHandle, Change};
use iris_core::id::{CameraId, StreamKey, StreamKind};
use iris_core::time::WallUs;
use iris_mp4::fragment::{Fragment, Sample};
use iris_mp4::index::{FLAG_STARTS_WITH_SYNC, IndexHeader, IndexRecord, IndexWriter, read_index};
use iris_mp4::init::SampleEntry;
use iris_store::{ApprovedRoot, RootExpectation, StoragePolicy, WriterHandle};
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;
use tower::ServiceExt as _;

/// Bytes that must never appear in a response.
const SECRET: &[u8] = b"[api]\ntokens = [\"synthetic-secret-marker\"]\n";

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

fn admit(dir: &Path, name: &str) -> ApprovedRoot {
    iris_store::admit_recording_root(&dir.join(name), RootExpectation::Fresh).expect("root")
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

/// Records `count` fragments of `payload` bytes beneath `root` and registers
/// the sealed file; returns its path and segment id.
async fn record(
    root: &ApprovedRoot,
    catalog: &CatalogHandle,
    count: u64,
    payload: usize,
) -> (PathBuf, i64) {
    let entry = entry();
    let writer = WriterHandle::spawn(
        root.path().to_path_buf(),
        StoragePolicy {
            sync_interval: Duration::from_secs(5),
            preallocate_bytes: 1 << 20,
            rotate_bytes: 1 << 30,
            rotate_secs: 3600,
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
    let (header, records) =
        read_index(&iris_store::layout::index_path(&path)).expect("written sidecar");
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
    let id = segment_id(catalog, &path).await;
    (path, id)
}

async fn segment_id(catalog: &CatalogHandle, path: &Path) -> i64 {
    catalog
        .segment_by_path(path)
        .await
        .expect("catalog query")
        .expect("segment row")
        .id
        .0
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

fn state(roots: Vec<ApprovedRoot>, catalog: CatalogHandle) -> Arc<ApiState> {
    let (events, _) = broadcast::channel(1);
    Arc::new(ApiState {
        catalog,
        live: Arc::new(LiveRegistry::new()),
        events,
        tokens: Vec::new(),
        recording_roots: roots,
        public_metrics: false,
        snapshot_sources: HashMap::new(),
        active_cameras: HashMap::new(),
        stats: Arc::new(ApiStats::default()),
        started: WallUs::now(),
        started_monotonic: Instant::now(),
        shutdown: CancellationToken::new(),
    })
}

async fn get(app: &axum::Router, uri: &str) -> axum::response::Response {
    app.clone()
        .oneshot(Request::get(uri).body(Body::empty()).expect("request"))
        .await
        .expect("router response")
}

/// Status and body of a response whose body completes.
async fn complete(app: &axum::Router, uri: &str) -> (StatusCode, Vec<u8>) {
    let response = get(app, uri).await;
    let status = response.status();
    let body = to_bytes(response.into_body(), 64 << 20)
        .await
        .expect("complete body");
    (status, body.to_vec())
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

fn range_query(from: WallUs, to: WallUs) -> String {
    format!("from={}&to={}", from.get(), to.get())
}

#[tokio::test]
async fn a_symlinked_recording_is_never_served_on_any_route() {
    let dir = tempfile::tempdir().expect("temporary directory");
    let catalog = CatalogHandle::open(&dir.path().join("catalog.db")).expect("catalog");
    let root = admit(dir.path(), "video");
    let mut secret = SECRET.to_vec();
    secret.resize(4096, b'#');
    let outside = dir.path().join("irisd.toml");
    std::fs::write(&outside, &secret).expect("outside file");

    // An open row whose media is a symlink out of the root, with a sidecar
    // forged to agree with the row.
    let leaf = root.path().join("cam1/main/2023-11-14");
    std::fs::create_dir_all(&leaf).expect("layout");
    let media = leaf.join("221320.000000Z.mp4");
    std::os::unix::fs::symlink(&outside, &media).expect("symlink");
    let sample_entry = entry();
    let start = wall(0);
    catalog.notify(Change::SegmentOpened {
        key: key(),
        path: media.clone(),
        run_seq: 7,
        discontinuity: true,
        wall_start: start,
        media_start: 90_000,
        init_len: 100,
        sample_entry: Arc::clone(&sample_entry),
    });
    let mut index = IndexWriter::create(
        &iris_store::layout::index_path(&media),
        &IndexHeader {
            start_wall: start,
            run_id: 7,
            init_len: 100,
            sample_entry_hash: sample_entry.hash,
            ..IndexHeader::default()
        },
    )
    .expect("forged sidecar");
    index
        .append(&IndexRecord {
            offset: 100,
            decode_time: 90_000,
            wall: start,
            len: 1_000,
            duration: 180_000,
            sample_count: 60,
            flags: FLAG_STARTS_WITH_SYNC,
            raw_rtp: 0,
        })
        .expect("forged record");
    drop(index);
    let id = segment_id(&catalog, &media).await;
    let app = router(state(vec![root], catalog.clone()));
    let range = range_query(start.offset(-1_000_000), start.offset(10_000_000));

    let (status, body) = complete(&app, &format!("/api/segments/{id}/file.mp4")).await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert!(!contains(&body, b"synthetic-secret-marker"));
    for uri in [
        format!("/api/streams/cam1/main/hls/vod.m3u8?{range}"),
        format!("/api/streams/cam1/main/export.mp4?{range}"),
        format!(
            "/api/streams/cam1/main/keyframe?t={}",
            start.offset(1_000_000).get()
        ),
    ] {
        let (status, body) = complete(&app, &uri).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{uri}");
        assert!(!contains(&body, b"synthetic-secret-marker"), "{uri}");
    }
    catalog.shutdown();
}

#[tokio::test]
async fn a_symlinked_sidecar_is_never_read() {
    let dir = tempfile::tempdir().expect("temporary directory");
    let catalog = CatalogHandle::open(&dir.path().join("catalog.db")).expect("catalog");
    let root = admit(dir.path(), "video");
    let (path, id) = record(&root, &catalog, 2, 4096).await;
    let index = iris_store::layout::index_path(&path);
    let outside = dir.path().join("sidecar.idx");
    std::fs::rename(&index, &outside).expect("move the sidecar out of the root");
    std::os::unix::fs::symlink(&outside, &index).expect("symlinked sidecar");
    let app = router(state(vec![root], catalog.clone()));

    let (status, _) = complete(&app, &format!("/api/segments/{id}/file.mp4")).await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    let range = range_query(wall(0), wall(2));
    for uri in [
        format!("/api/streams/cam1/main/hls/vod.m3u8?{range}"),
        format!("/api/streams/cam1/main/keyframe?t={}", wall(1).get()),
    ] {
        assert_eq!(complete(&app, &uri).await.0, StatusCode::NOT_FOUND, "{uri}");
    }

    std::fs::remove_file(&index).expect("remove the symlink");
    std::fs::rename(&outside, &index).expect("restore the sidecar");
    assert_eq!(
        complete(&app, &format!("/api/segments/{id}/file.mp4"))
            .await
            .0,
        StatusCode::OK,
        "the same recording is served once its sidecar is a regular file again"
    );
    catalog.shutdown();
}

#[tokio::test]
async fn raw_bodies_come_from_the_validated_inode_after_a_path_swap() {
    let dir = tempfile::tempdir().expect("temporary directory");
    let catalog = CatalogHandle::open(&dir.path().join("catalog.db")).expect("catalog");
    let root = admit(dir.path(), "video");
    let (path, id) = record(&root, &catalog, 4, 96 * 1024).await;
    let original = std::fs::read(&path).expect("original bytes");
    let app = router(state(vec![root], catalog.clone()));

    let response = get(&app, &format!("/api/segments/{id}/file.mp4")).await;
    assert_eq!(response.status(), StatusCode::OK);
    // From here on the pathname names other bytes of the same length.
    std::fs::rename(&path, path.with_extension("moved")).expect("move the validated file");
    std::fs::write(&path, vec![0x5a; original.len()]).expect("replacement");
    let body = to_bytes(response.into_body(), 64 << 20)
        .await
        .expect("complete body");
    assert_eq!(body.len(), original.len());
    assert!(
        body[..] == original[..],
        "served bytes are the validated file's"
    );
    catalog.shutdown();
}

#[tokio::test]
async fn an_export_refuses_a_recording_replaced_after_validation() {
    let dir = tempfile::tempdir().expect("temporary directory");
    let catalog = CatalogHandle::open(&dir.path().join("catalog.db")).expect("catalog");
    let root = admit(dir.path(), "video");
    let (path, _) = record(&root, &catalog, 3, 4096).await;
    let original = std::fs::read(&path).expect("original bytes");
    let app = router(state(vec![root], catalog.clone()));

    let response = get(
        &app,
        &format!(
            "/api/streams/cam1/main/export.mp4?{}",
            range_query(wall(0), wall(3))
        ),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    // Fragments are read only after the client takes the initialization; by
    // then the path names a different inode with plausible bytes.
    std::fs::rename(&path, path.with_extension("moved")).expect("move the validated file");
    std::fs::write(&path, &original).expect("identical replacement");
    assert!(
        to_bytes(response.into_body(), 64 << 20).await.is_err(),
        "a replaced recording aborts the export"
    );
    catalog.shutdown();
}

#[tokio::test]
async fn truncation_under_a_raw_response_aborts_it() {
    let dir = tempfile::tempdir().expect("temporary directory");
    let catalog = CatalogHandle::open(&dir.path().join("catalog.db")).expect("catalog");
    let root = admit(dir.path(), "video");
    let (path, id) = record(&root, &catalog, 8, 128 * 1024).await;
    let app = router(state(vec![root], catalog.clone()));

    let response = get(&app, &format!("/api/segments/{id}/file.mp4")).await;
    assert_eq!(response.status(), StatusCode::OK);
    std::fs::OpenOptions::new()
        .write(true)
        .open(&path)
        .expect("open for truncation")
        .set_len(100 * 1024)
        .expect("truncate");
    assert!(
        to_bytes(response.into_body(), 64 << 20).await.is_err(),
        "a body cut short by truncation ends with an error, not a clean end"
    );
    catalog.shutdown();
}

#[tokio::test]
async fn a_row_outside_every_approved_root_is_not_found_and_counted() {
    let dir = tempfile::tempdir().expect("temporary directory");
    let catalog = CatalogHandle::open(&dir.path().join("catalog.db")).expect("catalog");
    let recorded = admit(dir.path(), "video");
    let (_, id) = record(&recorded, &catalog, 2, 4096).await;
    let other = admit(dir.path(), "other");
    let state = state(vec![other], catalog.clone());
    let app = router(Arc::clone(&state));

    assert_eq!(
        complete(&app, &format!("/api/segments/{id}/file.mp4"))
            .await
            .0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(state.stats.unapproved_rows(), 1);
    assert_eq!(
        complete(
            &app,
            &format!(
                "/api/streams/cam1/main/hls/vod.m3u8?{}",
                range_query(wall(0), wall(2))
            )
        )
        .await
        .0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(state.stats.unapproved_rows(), 2);
    catalog.shutdown();
}
