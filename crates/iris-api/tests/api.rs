//! End-to-end tests over the router with a real catalog and recording in a
//! temporary directory.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode, header};
use bytes::Bytes;
use iris_api::{ApiState, ApiStats, LiveRegistry, router};
use iris_catalog::{CatalogHandle, Change};
use iris_core::id::{CameraId, StreamKey, StreamKind};
use iris_core::time::WallUs;
use iris_ingest::{LiveFrame, LiveRing};
use iris_mp4::fragment::{Fragment, Sample};
use iris_mp4::index::read_index;
use iris_mp4::init::SampleEntry;
use iris_store::{StoragePolicy, WriterHandle};
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
    assert_eq!(found.len(), 1, "expected one recording, found {found:?}");
    found.remove(0)
}

/// Records three fragments and registers the file with the catalog.
async fn populate(root: &Path, catalog: &CatalogHandle) -> (PathBuf, u32, u64) {
    let entry = entry();
    let writer = WriterHandle::spawn(
        root.to_path_buf(),
        StoragePolicy {
            sync_interval: Duration::from_millis(20),
            preallocate_bytes: 1 << 20,
            rotate_bytes: 1 << 30,
            rotate_secs: 3600,
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
    let (header, records) = read_index(&iris_store::layout::index_path(&path)).unwrap();
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
    tokio::time::sleep(Duration::from_millis(400)).await;
    (path, header.init_len, len)
}

fn state(catalog: CatalogHandle, tokens: Vec<String>) -> Arc<ApiState> {
    let (events, _) = broadcast::channel(16);
    Arc::new(ApiState {
        catalog,
        live: Arc::new(LiveRegistry::new()),
        events,
        tokens,
        snapshot_sources: HashMap::new(),
        stats: Arc::new(ApiStats::default()),
        started: WallUs::now(),
        shutdown: CancellationToken::new(),
    })
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
    let app = router(state(catalog.clone(), vec!["secret".into()]));
    let response = app
        .oneshot(Request::get("/api/health").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let text = body_string(response).await;
    assert!(text.contains("\"status\":\"ok\""), "{text}");
    assert!(text.contains(env!("CARGO_PKG_VERSION")));
    catalog.shutdown();
}

#[tokio::test]
async fn rejects_missing_or_wrong_tokens_and_accepts_valid_ones() {
    let dir = tempfile::tempdir().unwrap();
    let catalog = CatalogHandle::open(&dir.path().join("c.db")).unwrap();
    let app = router(state(catalog.clone(), vec!["secret".into()]));
    let missing = app
        .clone()
        .oneshot(Request::get("/api/cameras").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(missing.status(), StatusCode::UNAUTHORIZED);
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
    let via_query = app
        .oneshot(
            Request::get("/api/cameras?token=secret")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(via_query.status(), StatusCode::OK);
    catalog.shutdown();
}

async fn get(app: &axum::Router, uri: String, range: Option<String>) -> axum::response::Response {
    let mut request = Request::get(uri);
    if let Some(range) = range {
        request = request.header(header::RANGE, range);
    }
    app.clone()
        .oneshot(request.body(Body::empty()).unwrap())
        .await
        .unwrap()
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
    let app = router(state(catalog.clone(), vec!["t".into()]));
    let (from, to) = (wall(0).get(), wall(3).get());

    let list = get(
        &app,
        format!("/api/streams/cam1/main/segments?from={from}&to={to}&token=t"),
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
        format!("/api/streams/cam1/main/hls/vod.m3u8?from={from}&to={to}&token=t"),
        None,
    )
    .await;
    assert_eq!(playlist.status(), StatusCode::OK);
    assert_eq!(
        playlist.headers()[header::CONTENT_TYPE],
        "application/vnd.apple.mpegurl"
    );
    let text = body_string(playlist).await;
    let map = format!(
        "#EXT-X-MAP:URI=\"/api/segments/{segment_id}/file.mp4?token=t\",BYTERANGE=\"{init_len}@0\""
    );
    assert!(text.contains(&map), "{text}");
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
            "/api/streams/cam1/main/hls/vod.m3u8?from={}&to={to}&token=t",
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
    let app = router(state(catalog.clone(), vec!["t".into()]));
    let (from, to) = (wall(0).get(), wall(3).get());

    let ranged = get(
        &app,
        format!("/api/segments/{segment_id}/file.mp4?token=t"),
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
        format!("/api/streams/cam1/main/export.mp4?from={from}&to={to}&token=t"),
        None,
    )
    .await;
    assert_eq!(export.status(), StatusCode::OK);
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
        format!("/api/streams/cam1/main/export.mp4?from={from}&to={to}&token=t"),
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
async fn unknown_segment_and_bad_ranges_are_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let catalog = CatalogHandle::open(&dir.path().join("c.db")).unwrap();
    let app = router(state(catalog.clone(), Vec::new()));
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
async fn the_keyframe_resource_is_one_sync_sample_behind_the_init_segment() {
    let dir = tempfile::tempdir().unwrap();
    let catalog = CatalogHandle::open(&dir.path().join("c.db")).unwrap();
    let (_, init_len, _) = populate(dir.path(), &catalog).await;
    let app = router(state(catalog.clone(), vec![]));
    // Fragment 1 starts at wall(1); half a second later the newest sync sample is still fragment 1's.
    let t = wall(1).offset(500_000).get();
    let response = get(&app, format!("/api/streams/cam1/main/keyframe?t={t}"), None).await;
    assert_eq!(response.status(), StatusCode::OK);
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

/// A router with one live ring, fed `frames` frames after the feeder started.
async fn live_setup() -> (
    axum::Router,
    LiveRing,
    Arc<SampleEntry>,
    CatalogHandle,
    tempfile::TempDir,
) {
    let dir = tempfile::tempdir().unwrap();
    let catalog = CatalogHandle::open(&dir.path().join("c.db")).unwrap();
    let state = state(catalog.clone(), vec!["t".into()]);
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
        async move {
            get(
                &app,
                "/api/streams/cam1/main/hls/live.m3u8?token=t".into(),
                None,
            )
            .await
        }
    });
    tokio::time::sleep(Duration::from_millis(200)).await;
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
        let response = get(
            app,
            "/api/streams/cam1/main/hls/live.m3u8?token=t".into(),
            None,
        )
        .await;
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
    assert!(
        text.contains("#EXTINF:1.00000,\nseg/0/media.m4s?token=t\n"),
        "{text}"
    );
    assert!(
        text.contains("#EXTINF:1.00000,\nseg/1/media.m4s?token=t\n"),
        "{text}"
    );
    assert!(
        text.contains(
            "#EXT-X-PART:DURATION=0.50000,URI=\"part/2/0/part.m4s?token=t\",INDEPENDENT=YES\n"
        ),
        "{text}"
    );
    assert!(
        text.contains("#EXT-X-PRELOAD-HINT:TYPE=PART,URI=\"part/2/1/part.m4s?token=t\""),
        "{text}"
    );
    // The init segment and a complete segment scan as a recording of single-sample fragments.
    let init = get(&app, format!("{base}/init.mp4?token=t"), None).await;
    assert_eq!(init.status(), StatusCode::OK);
    let mut file = to_bytes(init.into_body(), 1 << 20).await.unwrap().to_vec();
    let seg = get(&app, format!("{base}/seg/1/media.m4s?token=t"), None).await;
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
    let part = get(&app, format!("{base}/part/2/0/part.m4s?token=t"), None).await;
    assert_eq!(part.status(), StatusCode::OK);
    let bytes = to_bytes(part.into_body(), 1 << 20).await.unwrap();
    assert_eq!(bytes.windows(4).filter(|w| w == b"moof").count(), 15);
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
                format!("{base}/live.m3u8?_HLS_msn=2&_HLS_part=1&token=t"),
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
    assert!(
        text.contains("URI=\"part/2/1/part.m4s?token=t\"\n"),
        "{text}"
    );
    assert!(
        text.contains("#EXT-X-PRELOAD-HINT:TYPE=PART,URI=\"part/2/2/part.m4s?token=t\""),
        "{text}"
    );
    let bad = get(&app, format!("{base}/live.m3u8?_HLS_part=1&token=t"), None).await;
    assert_eq!(bad.status(), StatusCode::BAD_REQUEST);
    let far = get(&app, format!("{base}/live.m3u8?_HLS_msn=9&token=t"), None).await;
    assert_eq!(far.status(), StatusCode::BAD_REQUEST);
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
    let state = state(catalog.clone(), vec![]);
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
async fn retaining_an_event_sets_and_clears_its_time() {
    let dir = tempfile::tempdir().unwrap();
    let catalog = CatalogHandle::open(&dir.path().join("c.db")).unwrap();
    let id = catalog
        .insert_event(&iris_core::event::Event {
            camera: CameraId::try_new("cam1").unwrap(),
            source: iris_core::event::EventSource::DahuaCgi,
            kind: iris_core::event::EventKind::Motion,
            state: iris_core::event::EventState::Pulse,
            nvr_time: wall(1),
            camera_time: None,
            data: None,
        })
        .await
        .unwrap();
    let app = router(state(catalog.clone(), vec![]));
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
    let bad = app
        .clone()
        .oneshot(
            Request::post(&uri)
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from("{\"until\": \"yesterday\"}"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(bad.status(), StatusCode::BAD_REQUEST);
    let missing = app
        .oneshot(
            Request::post("/api/events/999/retain")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(missing.status(), StatusCode::NOT_FOUND);
    catalog.shutdown();
}
