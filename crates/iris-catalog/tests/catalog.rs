//! Integration tests over a temporary database file.

use std::path::PathBuf;
use std::sync::Arc;

use bytes::Bytes;
use tempfile::TempDir;

use iris_catalog::{CatalogError, CatalogHandle, Change, EventId, NewSegment, TimeRange};
use iris_core::event::{Event, EventKind, EventSource, EventState};
use iris_core::id::{CameraId, StreamKey, StreamKind};
use iris_core::time::WallUs;
use iris_mp4::init::SampleEntry;

fn open() -> (TempDir, CatalogHandle) {
    let dir = tempfile::tempdir().expect("tempdir");
    let catalog = CatalogHandle::open(&dir.path().join("state").join("irisd.db")).expect("open");
    (dir, catalog)
}

fn camera() -> CameraId {
    CameraId::try_new("cam1").expect("valid")
}

fn key() -> StreamKey {
    StreamKey {
        camera: camera(),
        kind: StreamKind::Main,
    }
}

fn entry() -> Arc<SampleEntry> {
    Arc::new(SampleEntry::new(
        Bytes::from_static(b"\0\0\0\x10hvc1\0\0\0\0\0\0\0\0"),
        "hvc1.1.6.L150.00".into(),
        3840,
        2160,
    ))
}

fn opened(path: &str, wall_start: i64) -> Change {
    Change::SegmentOpened {
        key: key(),
        path: PathBuf::from(path),
        run_seq: 1,
        discontinuity: true,
        wall_start: WallUs::new(wall_start),
        media_start: 0,
        init_len: 768,
        sample_entry: entry(),
    }
}

fn event(camera: &str, nvr_time: i64, kind: EventKind) -> Event {
    Event {
        camera: CameraId::try_new(camera).expect("valid"),
        source: EventSource::DahuaCgi,
        kind,
        state: EventState::Start,
        nvr_time: WallUs::new(nvr_time),
        camera_time: Some(WallUs::new(nvr_time - 500)),
        data: Some("{\"Region\":[0]}".into()),
    }
}

#[tokio::test]
async fn cameras_round_trip() {
    let (_dir, catalog) = open();
    catalog
        .ensure_camera(&camera(), "Example", Some("dahua"))
        .await
        .unwrap();
    catalog
        .ensure_camera(&camera(), "Renamed", None)
        .await
        .unwrap();
    let rows = catalog.cameras().await.unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].name, "Renamed");
    assert_eq!(rows[0].vendor, None);
    catalog.shutdown();
}

#[tokio::test]
async fn segment_lifecycle_is_observed_in_order() {
    let (_dir, catalog) = open();
    catalog.notify(opened("/v/a.mp4", 1_000_000));
    // A query issued right after the notification sees the committed row.
    let row = catalog
        .segment_by_path(&PathBuf::from("/v/a.mp4"))
        .await
        .unwrap()
        .expect("row");
    assert!(!row.sealed);
    assert_eq!(row.wall_end, row.wall_start);
    assert_eq!(row.durable_len, 768);
    assert_eq!(row.codecs, "hvc1.1.6.L150.00");
    assert_eq!((row.width, row.height), (3840, 2160));
    assert_eq!(row.sample_entry_hash, entry().hash);
    assert!(row.discontinuity);

    catalog.notify(Change::SegmentProgress {
        path: PathBuf::from("/v/a.mp4"),
        durable_len: 5_000_000,
        fragments: 3,
        wall_end: WallUs::new(7_000_000),
        media_end: 540_000,
    });
    let row = catalog
        .segment_by_path(&PathBuf::from("/v/a.mp4"))
        .await
        .unwrap()
        .expect("row");
    assert_eq!(
        (row.durable_len, row.fragments, row.media_end),
        (5_000_000, 3, 540_000)
    );
    assert_eq!(row.bytes, 5_000_000);

    // Unsealed rows overlap any range that reaches the present.
    let overlapping = catalog
        .segments(
            &key(),
            TimeRange {
                from: WallUs::new(8_000_000),
                to: WallUs::new(9_000_000),
            },
        )
        .await
        .unwrap();
    assert_eq!(overlapping.len(), 1);

    catalog.notify(Change::SegmentSealed {
        path: PathBuf::from("/v/a.mp4"),
        len: 6_000_000,
        fragments: 4,
        wall_end: WallUs::new(9_000_000),
        media_end: 720_000,
    });
    let row = catalog.segment(row.id).await.unwrap().expect("row");
    assert!(row.sealed);
    assert_eq!(
        (row.bytes, row.durable_len, row.fragments),
        (6_000_000, 6_000_000, 4)
    );

    // Sealed rows overlap only their own interval.
    let before = catalog
        .segments(
            &key(),
            TimeRange {
                from: WallUs::new(0),
                to: WallUs::new(1_000_000),
            },
        )
        .await
        .unwrap();
    assert!(before.is_empty(), "range ending at the start excludes it");
    let inside = catalog
        .segments(
            &key(),
            TimeRange {
                from: WallUs::new(8_000_000),
                to: WallUs::new(20_000_000),
            },
        )
        .await
        .unwrap();
    assert_eq!(inside.len(), 1);
    let after = catalog
        .segments(
            &key(),
            TimeRange {
                from: WallUs::new(9_000_000),
                to: WallUs::new(20_000_000),
            },
        )
        .await
        .unwrap();
    assert_eq!(after.len(), 0, "range starting at the end excludes it");
    catalog.shutdown();
}

#[tokio::test]
async fn retention_queries_order_and_delete() {
    let (_dir, catalog) = open();
    for (path, start) in [("/v/c.mp4", 30), ("/v/a.mp4", 10), ("/v/b.mp4", 20)] {
        catalog.notify(opened(path, start));
        catalog.notify(Change::SegmentSealed {
            path: PathBuf::from(path),
            len: 100,
            fragments: 1,
            wall_end: WallUs::new(start + 5),
            media_end: 1,
        });
    }
    catalog.notify(opened("/v/open.mp4", 40));
    let oldest = catalog.oldest_sealed(2).await.unwrap();
    assert_eq!(
        oldest
            .iter()
            .map(|r| r.path.to_str().unwrap())
            .collect::<Vec<_>>(),
        vec!["/v/a.mp4", "/v/b.mp4"]
    );
    let writing = catalog.writing_segments().await.unwrap();
    assert_eq!(writing.len(), 1);
    assert_eq!(writing[0].path, PathBuf::from("/v/open.mp4"));

    catalog.delete_segment(oldest[0].id).await.unwrap();
    assert!(catalog.segment(oldest[0].id).await.unwrap().is_none());
    let stats = catalog.stats().await.unwrap();
    assert_eq!(
        (stats.segments, stats.sealed_bytes, stats.events),
        (3, 200, 0)
    );
    catalog.shutdown();
}

#[tokio::test]
async fn mark_sealed_and_adopt() {
    let (_dir, catalog) = open();
    catalog.notify(opened("/v/crashed.mp4", 100));
    catalog
        .mark_sealed(
            &PathBuf::from("/v/crashed.mp4"),
            4096,
            2,
            WallUs::new(300),
            360_000,
        )
        .await
        .unwrap();
    let row = catalog
        .segment_by_path(&PathBuf::from("/v/crashed.mp4"))
        .await
        .unwrap()
        .unwrap();
    assert!(row.sealed);
    assert_eq!(
        (row.bytes, row.fragments, row.media_end),
        (4096, 2, 360_000)
    );
    let missing = catalog
        .mark_sealed(&PathBuf::from("/v/none.mp4"), 1, 1, WallUs::new(1), 1)
        .await;
    assert!(matches!(missing, Err(CatalogError::NotFound(_))));

    let id = catalog
        .adopt_segment(NewSegment {
            key: StreamKey {
                camera: camera(),
                kind: StreamKind::Sub,
            },
            path: PathBuf::from("/v/found.mp4"),
            run_seq: 0,
            discontinuity: true,
            wall_start: WallUs::new(500),
            wall_end: WallUs::new(900),
            media_start: 0,
            media_end: 180_000,
            init_len: 767,
            len: 123_456,
            fragments: 7,
            sample_entry: None,
        })
        .await
        .unwrap();
    let row = catalog.segment(id).await.unwrap().unwrap();
    assert!(row.sealed);
    assert_eq!(row.key.kind, StreamKind::Sub);
    assert_eq!(row.codecs, "unknown");
    assert_eq!(row.sample_entry_hash, 0);
    assert_eq!(row.bytes, 123_456);
    catalog.shutdown();
}

#[tokio::test]
async fn events_filter_by_camera_range_and_limit() {
    let (_dir, catalog) = open();
    let first = catalog
        .insert_event(&event("cam1", 1_000, EventKind::Motion))
        .await
        .unwrap();
    catalog.notify(Change::Event(event(
        "cam1",
        2_000,
        EventKind::Other("VideoAbnormalDetection".into()),
    )));
    catalog.notify(Change::Event(event("cam2", 3_000, EventKind::Human)));
    catalog.notify(Change::Event(event("cam1", 4_000, EventKind::TimeChange)));

    let all = catalog
        .events(
            None,
            TimeRange {
                from: WallUs::new(0),
                to: WallUs::new(10_000),
            },
            100,
        )
        .await
        .unwrap();
    assert_eq!(all.len(), 4);
    assert_eq!(all[0].id, first);
    assert_eq!(
        all[1].event.kind,
        EventKind::Other("VideoAbnormalDetection".into())
    );
    assert_eq!(all[2].event.camera.as_str(), "cam2");
    assert_eq!(all[0].event.source, EventSource::DahuaCgi);
    assert_eq!(all[0].event.state, EventState::Start);
    assert_eq!(all[0].event.camera_time, Some(WallUs::new(500)));
    assert_eq!(all[0].event.data.as_deref(), Some("{\"Region\":[0]}"));

    let cam1 = catalog
        .events(
            Some(&camera()),
            TimeRange {
                from: WallUs::new(1_500),
                to: WallUs::new(10_000),
            },
            1,
        )
        .await
        .unwrap();
    assert_eq!(cam1.len(), 1);
    assert_eq!(cam1[0].event.nvr_time, WallUs::new(2_000));

    let stats = catalog.stats().await.unwrap();
    assert_eq!(stats.events, 4);
    catalog.shutdown();
}

#[tokio::test]
async fn event_data_is_bounded() {
    let (_dir, catalog) = open();
    let mut big = event("cam1", 1, EventKind::Motion);
    big.data = Some("x".repeat(iris_core::event::MAX_EVENT_DATA_LEN + 10));
    catalog.insert_event(&big).await.unwrap();
    let rows = catalog
        .events(
            None,
            TimeRange {
                from: WallUs::new(0),
                to: WallUs::new(10),
            },
            10,
        )
        .await
        .unwrap();
    assert_eq!(
        rows[0].event.data.as_ref().map(String::len),
        Some(iris_core::event::MAX_EVENT_DATA_LEN)
    );
    catalog.shutdown();
}

#[tokio::test]
async fn shutdown_is_idempotent_and_closes_clones() {
    let (_dir, catalog) = open();
    let clone = catalog.clone();
    catalog.shutdown();
    catalog.shutdown();
    assert!(matches!(clone.cameras().await, Err(CatalogError::Closed)));
    clone.notify(opened("/v/late.mp4", 1)); // dropped with an error log, never panics
    clone.shutdown();
}

#[tokio::test]
async fn reopening_keeps_data_and_schema() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("irisd.db");
    let catalog = CatalogHandle::open(&path).unwrap();
    catalog
        .ensure_camera(&camera(), "Example", None)
        .await
        .unwrap();
    catalog.shutdown();
    let catalog = CatalogHandle::open(&path).unwrap();
    assert_eq!(catalog.cameras().await.unwrap().len(), 1);
    catalog.shutdown();
}

#[tokio::test]
async fn metrics_count_batches_queries_and_waits() {
    let (_dir, catalog) = open();
    catalog.notify(opened("/v/m.mp4", 10));
    let rows = catalog.writing_segments().await.unwrap();
    assert_eq!(rows.len(), 1);
    let metrics = catalog.metrics();
    assert_eq!(metrics.batches(), 1);
    assert_eq!(metrics.changes(), 1);
    assert_eq!(metrics.failures(), 0);
    assert_eq!(metrics.batch_peak().get(), 1);
    assert_eq!(metrics.transaction().snapshot().count(), 1);
    assert_eq!(metrics.query().snapshot().count(), 1);
    assert_eq!(metrics.wait().snapshot().count(), 1);
    assert_eq!(metrics.event_commit().snapshot().count(), 0);
    catalog.shutdown();
}

#[tokio::test]
async fn event_commit_latency_is_recorded() {
    let (_dir, catalog) = open();
    let nvr_time = WallUs::now().get() - 1_000;
    catalog.notify(Change::Event(event("cam1", nvr_time, EventKind::Motion)));
    let stats = catalog.stats().await.unwrap();
    assert_eq!(stats.events, 1);
    let commit = catalog.metrics().event_commit().snapshot();
    assert_eq!(commit.count(), 1);
    assert!(commit.max() >= std::time::Duration::from_millis(1));
    catalog.shutdown();
}

#[tokio::test]
async fn newest_sealed_returns_the_latest_sealed_segment() {
    let (_dir, catalog) = open();
    for (path, start) in [("/v/c.mp4", 30), ("/v/a.mp4", 10), ("/v/b.mp4", 20)] {
        catalog.notify(opened(path, start));
        catalog.notify(Change::SegmentSealed {
            path: PathBuf::from(path),
            len: 100,
            fragments: 1,
            wall_end: WallUs::new(start + 5),
            media_end: 1,
        });
    }
    catalog.notify(opened("/v/open.mp4", 40));
    let newest = catalog.newest_sealed(&key()).await.unwrap().unwrap();
    assert_eq!(newest.path, PathBuf::from("/v/c.mp4"));
    assert!(newest.sealed);
    let other = StreamKey {
        camera: CameraId::try_new("cam2").expect("valid"),
        kind: StreamKind::Main,
    };
    assert!(catalog.newest_sealed(&other).await.unwrap().is_none());
    catalog.shutdown();
}

#[tokio::test]
async fn stats_report_the_database_and_wal_sizes() {
    let (_dir, catalog) = open();
    catalog.notify(opened("/v/s.mp4", 10));
    let stats = catalog.stats().await.unwrap();
    assert!(stats.db_bytes > 0, "db_bytes = {}", stats.db_bytes);
    assert!(stats.wal_bytes > 0, "wal_bytes = {}", stats.wal_bytes);
    catalog.shutdown();
}

#[tokio::test]
async fn retained_events_are_marked_listed_and_cleared() {
    let (_dir, catalog) = open();
    catalog
        .ensure_camera(&camera(), "front", None)
        .await
        .unwrap();
    let id = catalog
        .insert_event(&event("cam1", 1_700_000_000_000_000, EventKind::Motion))
        .await
        .unwrap();
    let now = WallUs::new(1_700_000_100_000_000);
    assert_eq!(catalog.retained_events(now).await.unwrap().len(), 0);
    let row = catalog
        .retain_event(id, Some(now.offset(3_600_000_000)))
        .await
        .unwrap();
    assert_eq!(row.id, id);
    assert_eq!(row.retain_until, Some(now.offset(3_600_000_000)));
    let retained = catalog.retained_events(now).await.unwrap();
    assert_eq!(retained.len(), 1);
    assert_eq!(retained[0].id, id);
    // Past its time it no longer counts; cleared, it is gone for good.
    assert_eq!(
        catalog
            .retained_events(now.offset(7_200_000_000))
            .await
            .unwrap()
            .len(),
        0
    );
    let cleared = catalog.retain_event(id, None).await.unwrap();
    assert_eq!(cleared.retain_until, None);
    assert_eq!(catalog.retained_events(now).await.unwrap().len(), 0);
    assert!(matches!(
        catalog.retain_event(EventId(999), None).await,
        Err(CatalogError::UnknownEvent(999))
    ));
    catalog.shutdown();
}
