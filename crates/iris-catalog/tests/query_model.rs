//! Seeded model tests of the range queries: over generated events and
//! segments, the event pages and the segment listings return exactly what a
//! filter and sort written out here select, for generated ranges, cursors,
//! and limits, with many rows sharing a timestamp.

use std::path::PathBuf;
use std::time::Duration;

use iris_catalog::{CatalogHandle, EventCursor, EventId, NewSegment, TimeRange};
use iris_core::event::{Event, EventKind, EventSource, EventState};
use iris_core::id::{CameraId, StreamKey, StreamKind};
use iris_core::time::WallUs;

/// A small deterministic generator (xorshift64*), so a failure reproduces
/// from its seed.
struct Rng(u64);

impl Rng {
    /// A value in `0..n`.
    fn below(&mut self, n: u64) -> i64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        i64::try_from(self.0.wrapping_mul(0x2545_f491_4f6c_dd1d) % n).unwrap()
    }

    fn camera(&mut self) -> CameraId {
        CameraId::try_new(["cam1", "cam2"][self.below(2) as usize]).unwrap()
    }

    /// A range on the grid, sometimes empty or inverted.
    fn range(&mut self, span: u64) -> TimeRange {
        let from = self.below(span);
        TimeRange {
            from: WallUs::new(from),
            to: WallUs::new(from + self.below(span) - 2),
        }
    }
}

async fn open(dir: &tempfile::TempDir) -> CatalogHandle {
    // A grace of zero keeps an open row's interval independent of the clock.
    let catalog =
        CatalogHandle::open_with_grace(&dir.path().join("state").join("irisd.db"), Duration::ZERO)
            .unwrap();
    for camera in ["cam1", "cam2"] {
        catalog
            .ensure_camera(&CameraId::try_new(camera).unwrap(), camera, None)
            .await
            .unwrap();
    }
    catalog
}

#[tokio::test]
async fn event_pages_agree_with_a_sorted_filter() {
    for seed in 1..=30 {
        let mut rng = Rng(seed);
        let dir = tempfile::tempdir().unwrap();
        let catalog = open(&dir).await;
        // Sixty events on twenty instants, so most share a timestamp.
        let mut events: Vec<(i64, i64, CameraId)> = Vec::new();
        for _ in 0..60 {
            let camera = rng.camera();
            let time = rng.below(20);
            let id = catalog
                .insert_event(&Event {
                    camera: camera.clone(),
                    source: EventSource::DahuaCgi,
                    kind: EventKind::Motion,
                    state: EventState::Start,
                    nvr_time: WallUs::new(time),
                    camera_time: None,
                    data: None,
                })
                .await
                .unwrap();
            events.push((time, id.0, camera));
        }
        events.sort();

        for _ in 0..40 {
            let camera = (rng.below(3) > 0).then(|| rng.camera());
            let range = rng.range(24);
            // No cursor, one naming a row, or one between rows.
            let after = match rng.below(3) {
                0 => None,
                1 => {
                    let (time, id, _) = events[rng.below(60) as usize];
                    Some((time, id))
                }
                _ => Some((rng.below(22) - 1, rng.below(70) - 5)),
            };
            let limit = usize::try_from(rng.below(12)).unwrap();
            let expected = |after: Option<(i64, i64)>, limit: usize| -> Vec<(i64, i64)> {
                events
                    .iter()
                    .filter(|(time, id, owner)| {
                        camera.as_ref().is_none_or(|camera| camera == owner)
                            && *time >= range.from.get()
                            && *time < range.to.get()
                            && after.is_none_or(|cursor| (*time, *id) > cursor)
                    })
                    .map(|(time, id, _)| (*time, *id))
                    .take(limit)
                    .collect()
            };
            let page = |after: Option<(i64, i64)>, limit: usize| {
                let catalog = &catalog;
                let camera = camera.as_ref();
                async move {
                    let cursor = after.map(|(time, id)| EventCursor {
                        time: WallUs::new(time),
                        id: EventId(id),
                    });
                    catalog
                        .events_after(camera, range, cursor, limit)
                        .await
                        .unwrap()
                        .iter()
                        .map(|row| (row.event.nvr_time.get(), row.id.0))
                        .collect::<Vec<_>>()
                }
            };
            let context = format!("seed {seed}: {camera:?} {range:?} after {after:?}");
            assert_eq!(
                page(after, limit).await,
                expected(after, limit),
                "{context}"
            );

            // Pages of three, each continued from its last row, reach every
            // row of the range once and in order.
            let mut paged = Vec::new();
            let mut cursor = None;
            loop {
                let rows = page(cursor, 3).await;
                let Some(last) = rows.last() else { break };
                cursor = Some(*last);
                paged.extend(rows);
                assert!(paged.len() <= events.len(), "{context}: paging repeats");
            }
            assert_eq!(paged, expected(None, usize::MAX), "{context}");
        }
        catalog.shutdown();
    }
}

#[tokio::test]
async fn segment_listings_agree_with_an_overlap_filter() {
    for seed in 1..=30 {
        let mut rng = Rng(seed);
        let dir = tempfile::tempdir().unwrap();
        let catalog = open(&dir).await;
        // (start, id, key, end): sealed rows on a grid, with shared starts.
        let mut segments: Vec<(i64, i64, StreamKey, i64)> = Vec::new();
        for index in 0..40 {
            let key = StreamKey {
                camera: rng.camera(),
                kind: [StreamKind::Main, StreamKind::Sub][rng.below(2) as usize],
            };
            let start = rng.below(30);
            let end = start + 1 + rng.below(6);
            let id = catalog
                .adopt_segment(NewSegment {
                    key: key.clone(),
                    path: PathBuf::from(format!("/v/{index}.mp4")),
                    run_seq: 0,
                    discontinuity: true,
                    wall_start: WallUs::new(start),
                    wall_end: WallUs::new(end),
                    media_start: 0,
                    media_end: 180_000,
                    init_len: 767,
                    len: 4_096,
                    fragments: 1,
                    sample_entry: None,
                })
                .await
                .unwrap();
            segments.push((start, id.0, key, end));
        }
        segments.sort_by_key(|(start, id, ..)| (*start, *id));

        for _ in 0..60 {
            let key = StreamKey {
                camera: rng.camera(),
                kind: [StreamKind::Main, StreamKind::Sub][rng.below(2) as usize],
            };
            let range = rng.range(38);
            let limit = usize::try_from(rng.below(8)).unwrap();
            // A row is listed when it starts before the exclusive end and
            // ends after the start.
            let expected: Vec<i64> = segments
                .iter()
                .filter(|(start, _, owner, end)| {
                    *owner == key && *start < range.to.get() && *end > range.from.get()
                })
                .map(|(_, id, ..)| *id)
                .take(limit)
                .collect();
            let listed: Vec<i64> = catalog
                .segments_limited(&key, range, limit)
                .await
                .unwrap()
                .iter()
                .map(|row| row.id.0)
                .collect();
            assert_eq!(
                listed, expected,
                "seed {seed}: {key} {range:?} limit {limit}"
            );
        }
        catalog.shutdown();
    }
}
