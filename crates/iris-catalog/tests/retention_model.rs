//! Seeded model test of event protection: over generated segments, events,
//! and retain marks, every statement that decides what retention may delete
//! (the candidate pages, the per-stream age pages, and the atomic deletion
//! mark) agrees with one overlap rule written out here.

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::time::Duration;

use iris_catalog::{CatalogHandle, DeleteMarkResult, NewSegment, SegmentId};
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
}

/// The instant retention runs at; marks before it have lapsed.
const NOW: i64 = 2_000_000;
/// Times fall on this grid, so events land exactly on segment bounds and
/// margins often enough to pin the rule's strict comparisons.
const GRID: i64 = 100;

struct Segment {
    id: SegmentId,
    key: StreamKey,
    start: i64,
    end: i64,
}

struct Mark {
    camera: CameraId,
    time: i64,
    until: Option<i64>,
}

/// The rule: an event protects a segment of its camera while its mark is
/// active and its time lies strictly inside the segment widened by `margin`.
fn protected(segment: &Segment, marks: &[Mark], margin: i64) -> bool {
    marks.iter().any(|mark| {
        mark.camera == segment.key.camera
            && mark.until.is_some_and(|until| until > NOW)
            && mark.time > segment.start - margin
            && mark.time < segment.end + margin
    })
}

fn stream(rng: &mut Rng) -> StreamKey {
    StreamKey {
        camera: CameraId::try_new(["cam1", "cam2"][rng.below(2) as usize]).unwrap(),
        kind: [StreamKind::Main, StreamKind::Sub][rng.below(2) as usize],
    }
}

async fn populate(catalog: &CatalogHandle, rng: &mut Rng) -> (Vec<Segment>, Vec<Mark>) {
    for camera in ["cam1", "cam2"] {
        catalog
            .ensure_camera(&CameraId::try_new(camera).unwrap(), camera, None)
            .await
            .unwrap();
    }
    let mut segments = Vec::new();
    for index in 0..30 {
        let key = stream(rng);
        let start = GRID * rng.below(200);
        let end = start + GRID * (1 + rng.below(20));
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
        segments.push(Segment {
            id,
            key,
            start,
            end,
        });
    }
    let mut marks = Vec::new();
    for _ in 0..20 {
        let camera = stream(rng).camera;
        let time = GRID * rng.below(230) - 5 * GRID;
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
        // No mark, a lapsed one, one that lapses at this instant, or an
        // active one.
        let until = match rng.below(4) {
            0 => None,
            1 => Some(NOW - 1 - rng.below(1_000)),
            2 => Some(NOW),
            _ => Some(NOW + 1 + rng.below(1_000)),
        };
        if let Some(until) = until {
            catalog
                .retain_event(id, Some(WallUs::new(until)))
                .await
                .unwrap();
        }
        marks.push(Mark {
            camera,
            time,
            until,
        });
    }
    (segments, marks)
}

/// One generated catalog with what the rule says about it.
struct Case {
    seed: u64,
    catalog: CatalogHandle,
    segments: Vec<Segment>,
    /// The margin in microseconds and as the handle takes it.
    margin: i64,
    margin_duration: Duration,
    /// Ids of the segments the rule leaves unprotected.
    unprotected: BTreeSet<i64>,
}

/// One page of everything holds exactly the unprotected rows, oldest first,
/// and small pages reach the same rows in the same order.
async fn check_candidate_pages(case: &Case) {
    let Case { seed, margin, .. } = *case;
    let now = WallUs::new(NOW);
    let whole = case
        .catalog
        .retention_candidates(None, None, now, case.margin_duration, 1_000)
        .await
        .unwrap();
    assert_eq!(whole.scanned, case.segments.len(), "seed {seed}");
    let listed: Vec<i64> = whole.rows.iter().map(|row| row.id.0).collect();
    assert_eq!(
        listed.iter().copied().collect::<BTreeSet<_>>(),
        case.unprotected,
        "seed {seed}: margin {margin}"
    );
    assert!(
        whole.rows.windows(2).all(|pair| {
            (pair[0].wall_start, pair[0].id.0) < (pair[1].wall_start, pair[1].id.0)
        }),
        "seed {seed}: oldest first"
    );

    let mut paged = Vec::new();
    let mut after = None;
    let mut scanned = 0;
    loop {
        let page = case
            .catalog
            .retention_candidates(None, after, now, case.margin_duration, 7)
            .await
            .unwrap();
        scanned += page.scanned;
        paged.extend(page.rows.iter().map(|row| row.id.0));
        match page.next {
            Some(next) => after = Some(next),
            None => break,
        }
        assert!(
            scanned <= case.segments.len(),
            "seed {seed}: paging does not end"
        );
    }
    assert_eq!(paged, listed, "seed {seed}");
}

/// The age pages of each stream hold its unprotected rows wholly before
/// `cutoff`.
async fn check_age_pages(case: &Case, cutoff: i64) {
    let seed = case.seed;
    for camera in ["cam1", "cam2"] {
        for kind in [StreamKind::Main, StreamKind::Sub] {
            let key = StreamKey {
                camera: CameraId::try_new(camera).unwrap(),
                kind,
            };
            let page = case
                .catalog
                .expired_candidates(
                    &key,
                    WallUs::new(cutoff),
                    None,
                    WallUs::new(NOW),
                    case.margin_duration,
                    1_000,
                )
                .await
                .unwrap();
            let expected: BTreeSet<i64> = case
                .segments
                .iter()
                .filter(|segment| {
                    segment.key == key
                        && segment.start < cutoff
                        && segment.end < cutoff
                        && case.unprotected.contains(&segment.id.0)
                })
                .map(|segment| segment.id.0)
                .collect();
            assert_eq!(
                page.rows
                    .iter()
                    .map(|row| row.id.0)
                    .collect::<BTreeSet<_>>(),
                expected,
                "seed {seed}: {camera} {kind:?} before {cutoff}"
            );
        }
    }
}

/// The atomic mark refuses exactly the protected rows, and marks a row once.
async fn check_marks(case: &Case) {
    let seed = case.seed;
    let now = WallUs::new(NOW);
    for segment in &case.segments {
        let expected = if case.unprotected.contains(&segment.id.0) {
            DeleteMarkResult::Marked
        } else {
            DeleteMarkResult::Protected
        };
        let mark = case
            .catalog
            .mark_deleting_if_unprotected(segment.id, now, case.margin_duration)
            .await
            .unwrap();
        assert_eq!(
            mark, expected,
            "seed {seed}: segment {} of {}..{}",
            segment.id.0, segment.start, segment.end
        );
        if expected == DeleteMarkResult::Marked {
            let again = case
                .catalog
                .mark_deleting_if_unprotected(segment.id, now, case.margin_duration)
                .await
                .unwrap();
            assert_eq!(again, DeleteMarkResult::NotFound, "seed {seed}");
        }
    }
}

#[tokio::test]
async fn every_retention_statement_agrees_with_the_overlap_rule() {
    for seed in 1..=40 {
        let mut rng = Rng(seed);
        let dir = tempfile::tempdir().unwrap();
        let catalog = CatalogHandle::open(&dir.path().join("state").join("irisd.db")).unwrap();
        let (segments, marks) = populate(&catalog, &mut rng).await;
        let margin = GRID * rng.below(6);
        let unprotected = segments
            .iter()
            .filter(|segment| !protected(segment, &marks, margin))
            .map(|segment| segment.id.0)
            .collect();
        let case = Case {
            seed,
            catalog,
            segments,
            margin,
            margin_duration: Duration::from_micros(u64::try_from(margin).unwrap()),
            unprotected,
        };
        check_candidate_pages(&case).await;
        check_age_pages(&case, GRID * rng.below(220)).await;
        check_marks(&case).await;
        case.catalog.shutdown();
    }
}
