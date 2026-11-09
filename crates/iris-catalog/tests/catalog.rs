//! Integration tests over a temporary database file.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::Barrier;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use bytes::Bytes;
use rusqlite::Connection;
use rusqlite::types::Value;
use tempfile::TempDir;

use iris_catalog::{
    CatalogError, CatalogHandle, Change, DeleteMarkResult, EventCursor, EventId, NewSegment,
    OPEN_SEGMENT_GRACE, SegmentRow, TimeRange,
};
use iris_core::event::{Event, EventKind, EventSource, EventState};
use iris_core::id::{CameraId, RootIdentity, StreamKey, StreamKind};
use iris_core::time::WallUs;
use iris_mp4::init::SampleEntry;

/// A limit above any row count these tests create, for whole-stream listings.
const ALL_ROWS: usize = 10_000;

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

fn progress(path: &str, fragments: u32, wall_end: WallUs) -> Change {
    Change::SegmentProgress {
        path: PathBuf::from(path),
        durable_len: 4_096,
        fragments,
        wall_end,
        media_end: 90_000,
    }
}

async fn listed(catalog: &CatalogHandle, from: WallUs, to: WallUs) -> Vec<SegmentRow> {
    catalog
        .segments_limited(&key(), TimeRange { from, to }, ALL_ROWS)
        .await
        .expect("segment listing")
}

/// Every event in `range`, oldest first, through the paged listing.
async fn all_events(
    catalog: &CatalogHandle,
    camera: Option<&CameraId>,
    range: TimeRange,
) -> Vec<iris_catalog::EventRow> {
    catalog
        .events_after(camera, range, None, ALL_ROWS)
        .await
        .expect("event listing")
}

/// The oldest unprotected sealed rows of every root, as retention sees them.
async fn oldest_unprotected(catalog: &CatalogHandle, limit: usize) -> Vec<SegmentRow> {
    catalog
        .retention_candidates(None, None, WallUs::new(i64::MAX), Duration::ZERO, limit)
        .await
        .expect("retention page")
        .rows
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

fn json_string_with_encoded_len(len: usize) -> String {
    assert!(len >= 2, "a JSON string needs two quote bytes");
    format!("\"{}\"", "x".repeat(len - 2))
}

#[tokio::test]
async fn sealed_pagination_preserves_equal_start_times_after_notifications() {
    let (_dir, catalog) = open();
    for name in ["first", "second", "third"] {
        let path = format!("/video/{name}.mp4");
        catalog.notify(opened(&path, 1_000_000));
        catalog.notify(Change::SegmentSealed {
            path: path.into(),
            len: 1_000,
            fragments: 1,
            wall_end: WallUs::new(2_000_000),
            media_end: 90_000,
        });
    }
    let now = WallUs::new(i64::MAX);
    let first = catalog
        .retention_candidates(None, None, now, Duration::ZERO, 1)
        .await
        .unwrap();
    assert_eq!(first.rows.len(), 1, "{first:?}");
    let remaining = catalog
        .retention_candidates(None, first.next, now, Duration::ZERO, 2)
        .await
        .unwrap()
        .rows;
    assert_eq!(remaining.len(), 2, "{remaining:?}");
    let first = &first.rows[0];
    assert!(
        remaining
            .iter()
            .all(|row| row.wall_start == first.wall_start),
        "{remaining:?}"
    );
    assert!(
        first.id < remaining[0].id && remaining[0].id < remaining[1].id,
        "{first:?} then {remaining:?}"
    );
    catalog.shutdown();
}

#[tokio::test]
async fn ensure_camera_replaces_name_and_vendor() {
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

    // An unsealed row with durable fragments extends past its last progress by
    // the listing grace, capped at the present.
    let overlapping = listed(&catalog, WallUs::new(8_000_000), WallUs::new(9_000_000)).await;
    assert_eq!(overlapping.len(), 1, "{overlapping:?}");

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
    let before = listed(&catalog, WallUs::new(0), WallUs::new(1_000_000)).await;
    assert!(before.is_empty(), "range ending at the start excludes it");
    let inside = listed(&catalog, WallUs::new(8_000_000), WallUs::new(20_000_000)).await;
    assert_eq!(inside.len(), 1, "{inside:?}");
    let after = listed(&catalog, WallUs::new(9_000_000), WallUs::new(20_000_000)).await;
    assert_eq!(after.len(), 0, "range starting at the end excludes it");
    catalog.shutdown();
}

#[tokio::test]
async fn segment_limit_is_applied_in_start_order() {
    let (_dir, catalog) = open();
    for (path, start) in [
        ("/v/third.mp4", 3_000_000),
        ("/v/first.mp4", 1_000_000),
        ("/v/second.mp4", 2_000_000),
    ] {
        catalog.notify(opened(path, start));
        catalog.notify(progress(path, 1, WallUs::new(start + 500_000)));
    }

    let rows = catalog
        .segments_limited(
            &key(),
            TimeRange {
                from: WallUs::new(0),
                to: WallUs::new(4_000_000),
            },
            2,
        )
        .await
        .unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].path, PathBuf::from("/v/first.mp4"));
    assert_eq!(rows[1].path, PathBuf::from("/v/second.mp4"));
    catalog.shutdown();
}

/// Three sealed rows starting at 10, 20, and 30 and one open row.
fn seed_sealed_rows(catalog: &CatalogHandle) {
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
}

#[tokio::test]
async fn retention_candidates_come_oldest_first_and_skip_open_rows() {
    let (_dir, catalog) = open();
    seed_sealed_rows(&catalog);
    let oldest = oldest_unprotected(&catalog, 2).await;
    assert_eq!(
        oldest
            .iter()
            .map(|r| r.path.to_str().unwrap())
            .collect::<Vec<_>>(),
        vec!["/v/a.mp4", "/v/b.mp4"]
    );
    let writing = catalog.writing_segments_page(None, 512).await.unwrap();
    assert_eq!(writing.len(), 1, "{writing:?}");
    assert_eq!(writing[0].row.path, PathBuf::from("/v/open.mp4"));
    assert_eq!(writing[0].root, None, "no root was configured");
    catalog.shutdown();
}

#[tokio::test]
async fn a_marked_row_is_hidden_and_its_deletion_removes_it() {
    let (_dir, catalog) = open();
    seed_sealed_rows(&catalog);
    let oldest = oldest_unprotected(&catalog, 1).await.remove(0);
    assert_eq!(
        catalog
            .mark_deleting_if_unprotected(oldest.id, WallUs::new(i64::MAX), Duration::ZERO)
            .await
            .unwrap(),
        DeleteMarkResult::Marked
    );
    assert!(catalog.segment(oldest.id).await.unwrap().is_none());
    assert!(
        catalog.contains_segment_path(&oldest.path).await.unwrap(),
        "a marked row still owns its path"
    );
    let deleting = catalog.deleting_segments_page(None, 512).await.unwrap();
    assert_eq!(deleting.len(), 1, "{deleting:?}");
    assert_eq!(deleting[0].row.id, oldest.id);
    let stats = catalog.stats().await.unwrap();
    assert_eq!((stats.deleting_segments, stats.deleting_bytes), (1, 100));

    catalog.delete_segment(oldest.id).await.unwrap();
    assert!(!catalog.contains_segment_path(&oldest.path).await.unwrap());
    let stats = catalog.stats().await.unwrap();
    assert_eq!(
        (stats.segments, stats.sealed_bytes, stats.events),
        (3, 200, 0)
    );
    catalog.shutdown();
}

#[tokio::test]
async fn deleting_an_unmarked_row_keeps_it() {
    let (_dir, catalog) = open();
    seed_sealed_rows(&catalog);
    let oldest = oldest_unprotected(&catalog, 1).await.remove(0);
    catalog.delete_segment(oldest.id).await.unwrap();
    assert!(
        catalog.segment(oldest.id).await.unwrap().is_some(),
        "only a row in deletion intent is deleted"
    );
    catalog.shutdown();
}

#[tokio::test]
async fn a_retained_event_protects_its_footage_from_the_atomic_mark() {
    let (_dir, catalog) = open();
    seed_sealed_rows(&catalog);
    let now = WallUs::new(1_000);
    let oldest = oldest_unprotected(&catalog, 1).await.remove(0);
    let id = catalog
        .insert_event(&event("cam1", 12, EventKind::Motion))
        .await
        .unwrap();
    catalog
        .retain_event(id, Some(now.offset(3_600_000_000)))
        .await
        .unwrap();
    assert_eq!(
        catalog
            .mark_deleting_if_unprotected(oldest.id, now, Duration::ZERO)
            .await
            .unwrap(),
        DeleteMarkResult::Protected
    );
    let page = catalog
        .retention_candidates(None, None, now, Duration::ZERO, 10)
        .await
        .unwrap();
    assert_eq!(page.scanned, 3, "every raw row is inspected");
    assert_eq!(
        page.rows.iter().map(|row| row.id).collect::<Vec<_>>(),
        oldest_unprotected(&catalog, 10)
            .await
            .iter()
            .filter(|row| row.id != oldest.id)
            .map(|row| row.id)
            .collect::<Vec<_>>(),
        "the protected row is left out while its neighbours stay"
    );
    catalog.retain_event(id, None).await.unwrap();
    assert_eq!(
        catalog
            .mark_deleting_if_unprotected(oldest.id, now, Duration::ZERO)
            .await
            .unwrap(),
        DeleteMarkResult::Marked
    );
    catalog.shutdown();
}

#[tokio::test]
async fn root_bound_marks_and_pages_see_only_their_root() {
    let (_dir, catalog) = open();
    let roots = [PathBuf::from("/v"), PathBuf::from("/w")];
    catalog.configure_video_roots(&roots).await.unwrap();
    for (path, start) in [("/v/a.mp4", 10), ("/w/b.mp4", 20)] {
        catalog.notify(opened(path, start));
        catalog.notify(Change::SegmentSealed {
            path: PathBuf::from(path),
            len: 100,
            fragments: 1,
            wall_end: WallUs::new(start + 5),
            media_end: 1,
        });
    }
    let now = WallUs::new(i64::MAX);
    let only_w = catalog
        .retention_candidates(Some(Path::new("/w")), None, now, Duration::ZERO, 10)
        .await
        .unwrap();
    assert_eq!(only_w.rows.len(), 1, "{only_w:?}");
    assert_eq!(only_w.rows[0].path, PathBuf::from("/w/b.mp4"));
    let b = only_w.rows[0].id;
    assert_eq!(
        catalog.segment_root(b).await.unwrap(),
        Some(PathBuf::from("/w"))
    );
    assert_eq!(
        catalog
            .mark_deleting_if_unprotected_at_root(b, Path::new("/v"), now, Duration::ZERO)
            .await
            .unwrap(),
        DeleteMarkResult::NotFound,
        "another root never marks the row"
    );
    assert_eq!(
        catalog
            .mark_deleting_if_unprotected_at_root(b, Path::new("/w"), now, Duration::ZERO)
            .await
            .unwrap(),
        DeleteMarkResult::Marked
    );
    catalog.shutdown();
}

#[tokio::test]
async fn mark_sealed_finalizes_an_open_row() {
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
    catalog.shutdown();
}

#[tokio::test]
async fn mark_sealed_rejects_an_unknown_path() {
    let (_dir, catalog) = open();
    let missing = catalog
        .mark_sealed(&PathBuf::from("/v/none.mp4"), 1, 1, WallUs::new(1), 1)
        .await;
    assert!(
        matches!(missing, Err(CatalogError::NotFound(_))),
        "{missing:?}"
    );
    catalog.shutdown();
}

#[tokio::test]
async fn adopting_without_a_sample_entry_records_unknown_codecs() {
    let (_dir, catalog) = open();
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

    let all = all_events(
        &catalog,
        None,
        TimeRange {
            from: WallUs::new(0),
            to: WallUs::new(10_000),
        },
    )
    .await;
    assert_eq!(all.len(), 4, "{all:?}");
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
        .events_after(
            Some(&camera()),
            TimeRange {
                from: WallUs::new(1_500),
                to: WallUs::new(10_000),
            },
            None,
            1,
        )
        .await
        .unwrap();
    assert_eq!(cam1.len(), 1, "{cam1:?}");
    assert_eq!(cam1[0].event.nvr_time, WallUs::new(2_000));

    let stats = catalog.stats().await.unwrap();
    assert_eq!(stats.events, 4);
    catalog.shutdown();
}

#[tokio::test]
async fn event_pages_cross_an_equal_timestamp_group() {
    let (_dir, catalog) = open();
    for kind in [EventKind::Motion, EventKind::Human, EventKind::Vehicle] {
        catalog
            .insert_event(&event("cam1", 1_000, kind))
            .await
            .unwrap();
    }
    let range = TimeRange {
        from: WallUs::new(0),
        to: WallUs::new(2_000),
    };
    let first = catalog.events_after(None, range, None, 2).await.unwrap();
    assert_eq!(first.len(), 2);
    let cursor = EventCursor {
        time: first[1].event.nvr_time,
        id: first[1].id,
    };
    let second = catalog
        .events_after(None, range, Some(cursor), 2)
        .await
        .unwrap();
    assert_eq!(second.len(), 1);
    assert_eq!(second[0].event.nvr_time, WallUs::new(1_000));
    assert!(first[1].id < second[0].id);
    catalog.shutdown();
}

#[tokio::test]
async fn every_other_kind_round_trips_without_aliasing_builtins() {
    let (_dir, catalog) = open();
    let codes = [
        "motion",
        "human",
        "vehicle",
        "line_cross",
        "intrusion",
        "tamper",
        "audio",
        "face",
        "video_loss",
        "time_change",
        "disconnect",
        "VideoAbnormalDetection",
        "温度/高",
        "other",
        "",
    ];
    for (index, code) in codes.iter().enumerate() {
        catalog
            .insert_event(&event(
                "cam1",
                i64::try_from(index).unwrap() + 1,
                EventKind::Other((*code).to_owned()),
            ))
            .await
            .unwrap();
    }
    let rows = all_events(
        &catalog,
        None,
        TimeRange {
            from: WallUs::new(0),
            to: WallUs::new(100),
        },
    )
    .await;
    let actual: Vec<EventKind> = rows.into_iter().map(|row| row.event.kind).collect();
    let expected: Vec<EventKind> = codes
        .iter()
        .map(|code| EventKind::Other((*code).to_owned()))
        .collect();
    assert_eq!(actual, expected);
    catalog.shutdown();
}

#[tokio::test]
async fn malformed_persisted_values_fail_closed() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("catalog.db");
    let catalog = CatalogHandle::open(&path).unwrap();
    catalog.notify(opened("/v/corrupt.mp4", 1_000));
    catalog.notify(Change::SegmentSealed {
        path: PathBuf::from("/v/corrupt.mp4"),
        len: 100,
        fragments: 1,
        wall_end: WallUs::new(2_000),
        media_end: 90_000,
    });
    let segment = catalog
        .segment_by_path(&PathBuf::from("/v/corrupt.mp4"))
        .await
        .unwrap()
        .unwrap();
    let event_id = catalog
        .insert_event(&event("cam1", 1_500, EventKind::Motion))
        .await
        .unwrap();
    catalog.shutdown();

    {
        let conn = Connection::open(&path).unwrap();
        conn.execute(
            "UPDATE segment SET init_len = -1, discontinuity = 2 WHERE id = ?1",
            [segment.id.0],
        )
        .unwrap();
    }
    let catalog = CatalogHandle::open(&path).unwrap();
    let corrupt_segment = catalog.segment(segment.id).await;
    assert!(
        matches!(corrupt_segment, Err(CatalogError::Corrupt(_))),
        "{corrupt_segment:?}"
    );
    catalog.shutdown();

    {
        let conn = Connection::open(&path).unwrap();
        conn.execute(
            "UPDATE segment SET init_len = 768, discontinuity = 1, deleting = 2 WHERE id = ?1",
            [segment.id.0],
        )
        .unwrap();
    }
    assert!(matches!(
        CatalogHandle::open(&path),
        Err(CatalogError::Corrupt(_))
    ));

    {
        let conn = Connection::open(&path).unwrap();
        conn.execute(
            "UPDATE segment SET deleting = 0 WHERE id = ?1",
            [segment.id.0],
        )
        .unwrap();
        conn.execute(
            "UPDATE event SET source = 'invalid', state = 'invalid' WHERE id = ?1",
            [event_id.0],
        )
        .unwrap();
    }
    let catalog = CatalogHandle::open(&path).unwrap();
    let corrupt_events = catalog
        .events_after(
            None,
            TimeRange {
                from: WallUs::new(0),
                to: WallUs::new(10_000),
            },
            None,
            10,
        )
        .await;
    assert!(
        matches!(corrupt_events, Err(CatalogError::Corrupt(_))),
        "{corrupt_events:?}"
    );
    catalog.shutdown();
}

#[tokio::test]
async fn deleting_domain_is_validated_before_state_filtered_queries() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("catalog.db");
    let catalog = CatalogHandle::open(&path).unwrap();
    catalog.notify(opened("/v/writing.mp4", 1_000));
    catalog.notify(opened("/v/sealed.mp4", 2_000));
    catalog.notify(Change::SegmentSealed {
        path: PathBuf::from("/v/sealed.mp4"),
        len: 100,
        fragments: 1,
        wall_end: WallUs::new(3_000),
        media_end: 90_000,
    });
    assert_eq!(
        catalog
            .writing_segments_page(None, 512)
            .await
            .unwrap()
            .len(),
        1
    );
    assert_eq!(oldest_unprotected(&catalog, 10).await.len(), 1);
    catalog.shutdown();

    let conn = Connection::open(&path).unwrap();
    conn.execute("UPDATE segment SET deleting = 2", []).unwrap();
    conn.pragma_update(None, "journal_mode", "DELETE").unwrap();
    drop(conn);
    assert!(matches!(
        CatalogHandle::open(&path),
        Err(CatalogError::Corrupt(_))
    ));
    let conn = Connection::open(&path).unwrap();
    let journal_mode: String = conn
        .pragma_query_value(None, "journal_mode", |row| row.get(0))
        .unwrap();
    assert_eq!(journal_mode, "delete");
}

#[tokio::test]
async fn sealed_domain_is_validated_before_state_filtered_queries() {
    let dir = tempfile::tempdir().unwrap();
    let invalid = [
        ("integer", Value::Integer(2)),
        ("real", Value::Real(0.5)),
        ("text", Value::Text("invalid".to_owned())),
        ("blob", Value::Blob(vec![0xff])),
    ];
    for (name, value) in invalid {
        let path = dir.path().join(format!("sealed-{name}.db"));
        let catalog = CatalogHandle::open(&path).unwrap();
        catalog.notify(opened("/v/writing.mp4", 1_000));
        catalog.notify(opened("/v/sealed.mp4", 2_000));
        catalog.notify(Change::SegmentSealed {
            path: PathBuf::from("/v/sealed.mp4"),
            len: 100,
            fragments: 1,
            wall_end: WallUs::new(3_000),
            media_end: 90_000,
        });
        assert_eq!(
            catalog
                .writing_segments_page(None, 512)
                .await
                .unwrap()
                .len(),
            1
        );
        assert_eq!(oldest_unprotected(&catalog, 10).await.len(), 1);
        catalog.shutdown();

        let conn = Connection::open(&path).unwrap();
        conn.execute("UPDATE segment SET sealed = ?1", [value])
            .unwrap();
        conn.pragma_update(None, "journal_mode", "DELETE").unwrap();
        drop(conn);
        assert!(matches!(
            CatalogHandle::open(&path),
            Err(CatalogError::Corrupt(_))
        ));
        let wal = PathBuf::from(format!("{}-wal", path.to_string_lossy()));
        assert!(!wal.exists(), "{name} corruption created a WAL file");
        let conn = Connection::open(&path).unwrap();
        let journal_mode: String = conn
            .pragma_query_value(None, "journal_mode", |row| row.get(0))
            .unwrap();
        assert_eq!(journal_mode, "delete", "{name}");
    }
}

#[tokio::test]
async fn malformed_segment_aggregate_values_are_corruption() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("catalog.db");
    let catalog = CatalogHandle::open(&path).unwrap();
    catalog.notify(opened("/v/corrupt-aggregate.mp4", 1_000));
    catalog.notify(Change::SegmentSealed {
        path: PathBuf::from("/v/corrupt-aggregate.mp4"),
        len: 100,
        fragments: 1,
        wall_end: WallUs::new(2_000),
        media_end: 90_000,
    });
    let segment = catalog
        .segment_by_path(&PathBuf::from("/v/corrupt-aggregate.mp4"))
        .await
        .unwrap()
        .unwrap();
    catalog.shutdown();
    {
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch("PRAGMA ignore_check_constraints = ON;")
            .unwrap();
        conn.execute(
            "UPDATE segment SET bytes = 'bad' WHERE id = ?1",
            [segment.id.0],
        )
        .unwrap();
    }
    let catalog = CatalogHandle::open(&path).unwrap();
    assert!(matches!(
        catalog.total_bytes().await,
        Err(CatalogError::Corrupt(_))
    ));
    assert!(matches!(
        catalog.stats().await,
        Err(CatalogError::Corrupt(_))
    ));
    catalog.shutdown();
}

#[tokio::test]
async fn malformed_catalog_state_values_are_corruption() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("catalog.db");
    let catalog = CatalogHandle::open(&path).unwrap();
    catalog
        .insert_event(&event("cam1", 1_500, EventKind::Motion))
        .await
        .unwrap();
    catalog.shutdown();

    {
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch("PRAGMA ignore_check_constraints = ON;")
            .unwrap();
        conn.execute(
            "UPDATE catalog_state SET event_rows = 'bad' WHERE singleton = 1",
            [],
        )
        .unwrap();
    }
    let catalog = CatalogHandle::open(&path).unwrap();
    assert!(matches!(
        catalog.stats().await,
        Err(CatalogError::Corrupt(_))
    ));
    assert!(matches!(
        catalog
            .insert_event(&event("cam1", 2_000, EventKind::Motion))
            .await,
        Err(CatalogError::Corrupt(_))
    ));
    catalog.shutdown();
}

#[tokio::test]
async fn event_data_is_bounded() {
    let (_dir, catalog) = open();
    let mut big = event("cam1", 1, EventKind::Motion);
    big.data = Some(json_string_with_encoded_len(
        iris_core::event::MAX_EVENT_DATA_LEN + 10,
    ));
    big.kind = EventKind::Other("温".repeat(iris_core::event::MAX_EVENT_KIND_LEN));
    catalog.insert_event(&big).await.unwrap();
    let rows = all_events(
        &catalog,
        None,
        TimeRange {
            from: WallUs::new(0),
            to: WallUs::new(10),
        },
    )
    .await;
    assert_eq!(rows[0].event.data.as_deref(), Some(r#"{"truncated":true}"#));
    assert!(
        serde_json::from_str::<serde_json::Value>(
            rows[0].event.data.as_deref().expect("bounded event data")
        )
        .is_ok()
    );
    let EventKind::Other(code) = &rows[0].event.kind else {
        panic!("other kind changed variant");
    };
    assert!(code.len() <= iris_core::event::MAX_EVENT_KIND_LEN);
    catalog.shutdown();
}

#[tokio::test]
async fn event_listing_enforces_an_aggregate_payload_budget() {
    let (_dir, catalog) = open();
    let payload = json_string_with_encoded_len(iris_core::event::MAX_EVENT_DATA_LEN);
    for at in 1..=140 {
        let mut value = event("cam1", at, EventKind::Motion);
        value.data = Some(payload.clone());
        catalog.insert_event(&value).await.unwrap();
    }
    let rows = all_events(
        &catalog,
        None,
        TimeRange {
            from: WallUs::new(0),
            to: WallUs::new(1_000),
        },
    )
    .await;
    let payload_bytes: usize = rows
        .iter()
        .filter_map(|row| row.event.data.as_ref())
        .map(String::len)
        .sum();
    rows.first().expect("event budget returned no rows");
    assert!(rows.len() < 140, "aggregate byte cap must end the page");
    assert!(payload_bytes <= 8 * 1024 * 1024);
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
async fn reopening_keeps_camera_rows() {
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
    let rows = catalog.writing_segments_page(None, 512).await.unwrap();
    assert_eq!(rows.len(), 1, "{rows:?}");
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
async fn stalled_sqlite_consumer_has_bounded_ingress_and_query_overload() {
    const PRODUCERS: usize = 16;

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("catalog.db");
    let catalog = CatalogHandle::open(&path).unwrap();
    let lock = Connection::open(&path).unwrap();
    lock.execute_batch("BEGIN IMMEDIATE").unwrap();
    let metrics = catalog.metrics();
    let payload = Arc::new("x".repeat(iris_core::event::MAX_EVENT_DATA_LEN - 2));
    let start = Arc::new(Barrier::new(PRODUCERS + 1));
    let stop = Arc::new(AtomicBool::new(false));
    let submitted = Arc::new(AtomicUsize::new(0));
    let producers: Vec<_> = (0..PRODUCERS)
        .map(|_| {
            let catalog = catalog.clone();
            let payload = Arc::clone(&payload);
            let start = Arc::clone(&start);
            let stop = Arc::clone(&stop);
            let submitted = Arc::clone(&submitted);
            thread::spawn(move || {
                start.wait();
                while !stop.load(Ordering::Acquire) {
                    let at = submitted.fetch_add(1, Ordering::Relaxed);
                    let mut value = event(
                        "cam1",
                        i64::try_from(at).unwrap_or(i64::MAX),
                        EventKind::Motion,
                    );
                    value.data = Some(payload.as_ref().clone());
                    catalog.notify(Change::Event(value));
                }
            })
        })
        .collect();
    start.wait();
    let overload_deadline = Instant::now() + Duration::from_secs(10);
    while metrics.rejected_events() == 0 && Instant::now() < overload_deadline {
        thread::yield_now();
    }
    let overloaded_query = catalog.stats().await;
    stop.store(true, Ordering::Release);
    for producer in producers {
        producer.join().unwrap();
    }
    assert!(
        metrics.queue_depth() <= 2_048,
        "queue depth {}",
        metrics.queue_depth()
    );
    assert!(
        metrics.queue_bytes() <= 16 * 1024 * 1024,
        "queue bytes {}",
        metrics.queue_bytes()
    );
    assert!(metrics.rejected_events() > 0, "no event was rejected");
    assert!(
        matches!(overloaded_query, Err(CatalogError::Overloaded)),
        "query under a stalled consumer returned {overloaded_query:?}"
    );

    lock.execute_batch("COMMIT").unwrap();
    let stats = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            match catalog.stats().await {
                Err(CatalogError::Overloaded) => tokio::task::yield_now().await,
                result => break result,
            }
        }
    })
    .await
    .expect("bounded backlog did not drain")
    .unwrap();
    assert!(stats.events > 0, "no event was committed");
    let submitted = u64::try_from(submitted.load(Ordering::Relaxed)).unwrap();
    assert!(
        stats.events < submitted,
        "{} events committed of {submitted} submitted",
        stats.events
    );
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
    assert!(commit.max() >= Duration::from_millis(1), "{commit:?}");
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
    let listed = || {
        catalog.events_after(
            None,
            TimeRange {
                from: WallUs::new(0),
                to: WallUs::new(i64::MAX),
            },
            None,
            16,
        )
    };
    assert_eq!(listed().await.unwrap()[0].retain_until, None);
    let row = catalog
        .retain_event(id, Some(now.offset(3_600_000_000)))
        .await
        .unwrap();
    assert_eq!(row.id, id);
    assert_eq!(row.retain_until, Some(now.offset(3_600_000_000)));
    let rows = listed().await.unwrap();
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0].id, id);
    assert_eq!(rows[0].retain_until, Some(now.offset(3_600_000_000)));
    // Cleared, the mark is gone for good.
    let cleared = catalog.retain_event(id, None).await.unwrap();
    assert_eq!(cleared.retain_until, None);
    assert_eq!(listed().await.unwrap()[0].retain_until, None);
    assert!(matches!(
        catalog.retain_event(EventId(999), None).await,
        Err(CatalogError::UnknownEvent(EventId(999)))
    ));
    catalog.shutdown();
}

#[tokio::test]
async fn root_registration_is_idempotent_and_rejects_conflicts() {
    let (_dir, catalog) = open();
    let identity: RootIdentity = "0123456789abcdef0123456789abcdef".parse().unwrap();
    let other: RootIdentity = "fedcba9876543210fedcba9876543210".parse().unwrap();
    let root = Path::new("/srv/video");
    catalog
        .register_recording_root(root, identity)
        .await
        .unwrap();
    catalog
        .register_recording_root(root, identity)
        .await
        .unwrap();
    let mismatch = catalog.register_recording_root(root, other).await;
    assert!(
        matches!(&mismatch, Err(CatalogError::RootIdentityMismatch(path)) if path == root),
        "{mismatch:?}"
    );
    let in_use = catalog
        .register_recording_root(Path::new("/srv/other"), identity)
        .await;
    assert!(
        matches!(&in_use, Err(CatalogError::RootIdentityInUse(path)) if path == root),
        "{in_use:?}"
    );
    catalog.shutdown();
}

#[tokio::test]
async fn open_rows_without_a_durable_fragment_are_not_listed() {
    let (_dir, catalog) = open();
    let now = WallUs::now();
    catalog.notify(opened("/v/fresh.mp4", now.offset(-1_000_000).get()));
    let window = (now.offset(-60_000_000), now.offset(60_000_000));
    assert!(
        listed(&catalog, window.0, window.1).await.is_empty(),
        "a freshly opened row has nothing durable to serve"
    );

    catalog.notify(progress("/v/fresh.mp4", 1, now));
    let rows = listed(&catalog, window.0, window.1).await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].fragments, 1);
    catalog.shutdown();
}

#[tokio::test]
async fn an_open_row_without_recent_progress_ends_after_the_grace() {
    let (_dir, catalog) = open();
    let now = WallUs::now();
    let last_progress = now.offset(-540_000_000);
    catalog.notify(opened("/v/stale.mp4", now.offset(-600_000_000).get()));
    catalog.notify(progress("/v/stale.mp4", 3, last_progress));
    let grace = i64::try_from(OPEN_SEGMENT_GRACE.as_micros()).unwrap();

    assert!(
        listed(&catalog, now.offset(-300_000_000), now)
            .await
            .is_empty(),
        "a row whose writer stopped reporting must not claim a later window"
    );
    let covering = listed(&catalog, last_progress.offset(grace - 1_000_000), now).await;
    assert_eq!(covering.len(), 1);
    assert_eq!(
        covering[0].wall_end, last_progress,
        "the row keeps its recorded end"
    );
    assert!(
        listed(&catalog, last_progress.offset(grace), now)
            .await
            .is_empty(),
        "the grace ends exactly {grace} µs after the last progress"
    );
    catalog.shutdown();
}

#[tokio::test]
async fn a_progressing_open_row_reaches_the_present() {
    let (_dir, catalog) = open();
    let now = WallUs::now();
    catalog.notify(opened("/v/live.mp4", now.offset(-20_000_000).get()));
    catalog.notify(progress("/v/live.mp4", 4, now.offset(-2_000_000)));
    let rows = listed(&catalog, now.offset(-1_000_000), now.offset(1_000_000)).await;
    assert_eq!(rows.len(), 1, "the live edge lists the growing file");
    assert!(!rows[0].sealed);

    catalog.notify(Change::SegmentSealed {
        path: PathBuf::from("/v/live.mp4"),
        len: 8_192,
        fragments: 5,
        wall_end: now.offset(-1_500_000),
        media_end: 450_000,
    });
    assert!(
        listed(&catalog, now.offset(-1_000_000), now.offset(1_000_000))
            .await
            .is_empty(),
        "a sealed row covers only its recorded interval"
    );
    assert_eq!(
        listed(&catalog, now.offset(-30_000_000), now).await.len(),
        1
    );
    catalog.shutdown();
}

/// Holds SQLite's write lock from a second connection for `hold`, as an operator
/// shell, a checkpoint, or a backup tool would, and returns the instant just
/// before the lock was released.
fn hold_write_lock(path: &Path, hold: Duration) -> JoinHandle<Instant> {
    let lock = Connection::open(path).expect("second connection");
    lock.execute_batch("BEGIN IMMEDIATE")
        .expect("external write lock");
    thread::spawn(move || {
        thread::sleep(hold);
        let released = Instant::now();
        lock.execute_batch("COMMIT")
            .expect("release the external write lock");
        released
    })
}

/// Runs one catalog write while another connection holds the write lock and
/// checks that it waited for the lock instead of failing.
async fn waits_for_the_write_lock<T>(
    name: &str,
    path: &Path,
    operation: impl Future<Output = Result<T, CatalogError>>,
) -> T {
    const HOLD: Duration = Duration::from_millis(250);
    const MIN_WAIT: Duration = Duration::from_millis(150);

    let holder = hold_write_lock(path, HOLD);
    let started = Instant::now();
    let value = match operation.await {
        Ok(value) => value,
        Err(error) => panic!("{name} failed under an external write lock: {error}"),
    };
    let completed = Instant::now();
    let released = holder.join().expect("lock holder thread");
    assert!(
        completed >= released,
        "{name} completed while another connection held the write lock"
    );
    assert!(
        completed - started >= MIN_WAIT,
        "{name} returned after {:?}",
        completed - started
    );
    value
}

#[tokio::test]
async fn write_paths_wait_for_an_external_write_lock() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("catalog.db");
    let catalog = CatalogHandle::open(&path).unwrap();
    let id = catalog
        .insert_event(&event("cam1", 1_000, EventKind::Motion))
        .await
        .unwrap();

    waits_for_the_write_lock(
        "insert_event",
        &path,
        catalog.insert_event(&event("cam1", 2_000, EventKind::Motion)),
    )
    .await;
    let row = waits_for_the_write_lock(
        "retain_event",
        &path,
        catalog.retain_event(id, Some(WallUs::new(i64::MAX))),
    )
    .await;
    assert_eq!(row.retain_until, Some(WallUs::new(i64::MAX)));
    let identity: RootIdentity = "0123456789abcdef0123456789abcdef".parse().unwrap();
    waits_for_the_write_lock(
        "register_recording_root",
        &path,
        catalog.register_recording_root(Path::new("/srv/video"), identity),
    )
    .await;
    waits_for_the_write_lock(
        "ensure_camera",
        &path,
        catalog.ensure_camera(&camera(), "front", None),
    )
    .await;
    catalog.notify(opened("/v/locked.mp4", 1_000));
    let open_row = catalog
        .segment_by_path(Path::new("/v/locked.mp4"))
        .await
        .unwrap()
        .unwrap();
    waits_for_the_write_lock(
        "mark_sealed",
        &path,
        catalog.mark_sealed(Path::new("/v/locked.mp4"), 100, 1, WallUs::new(2_000), 1),
    )
    .await;
    let marked = waits_for_the_write_lock(
        "mark_deleting_if_unprotected",
        &path,
        catalog.mark_deleting_if_unprotected(open_row.id, WallUs::new(i64::MAX), Duration::ZERO),
    )
    .await;
    assert_eq!(marked, DeleteMarkResult::Marked);
    waits_for_the_write_lock("delete_segment", &path, catalog.delete_segment(open_row.id)).await;

    let failures = catalog.metrics().failures();
    let stats = waits_for_the_write_lock("an event-first batch", &path, async {
        catalog.notify(Change::Event(event("cam1", 3_000, EventKind::Motion)));
        catalog.stats().await
    })
    .await;
    assert_eq!(
        catalog.metrics().failures(),
        failures,
        "the batch committed"
    );
    assert_eq!(stats.events, 3);
    catalog.shutdown();
}
