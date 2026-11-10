//! Seeded model test of event maintenance: over generated events, retain
//! marks, and recordings, one maintenance step leaves exactly the events that
//! the lifecycle rules written out here keep, with exactly the marks that are
//! still active.

use std::path::PathBuf;
use std::time::Duration;

use iris_catalog::{CatalogHandle, NewSegment, TimeRange};
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

const DAY: i64 = 86_400_000_000;
/// An event without a mark is dropped once it is this old.
const MAX_AGE: i64 = 90 * DAY;
/// An event without a mark is dropped once it is this much older than its
/// camera's oldest recording.
const MARGIN: i64 = 30_000_000;

/// The cameras and how long before the maintenance instant their oldest
/// recording starts; the last has none.
const CAMERAS: [(&str, Option<i64>); 3] = [
    ("cam1", Some(10 * DAY)),
    ("cam2", Some(50 * DAY)),
    ("cam3", None),
];

/// What the generator inserted.
struct Inserted {
    id: i64,
    horizon: Option<i64>,
    time: i64,
    until: Option<i64>,
}

/// The rules: a mark that has not lapsed keeps its event; a lapsed mark is
/// cleared; an event without an active mark goes when it is older than the
/// age limit or more than the margin older than its camera's oldest
/// recording. Returns what is left of the event, if anything.
fn kept(event: &Inserted, now: i64) -> Option<(i64, Option<i64>)> {
    if let Some(until) = event.until.filter(|until| *until > now) {
        return Some((event.id, Some(until)));
    }
    let too_old = event.time < now - MAX_AGE;
    let before_recordings = event
        .horizon
        .is_some_and(|start| event.time < start - MARGIN);
    (!too_old && !before_recordings).then_some((event.id, None))
}

async fn populate(catalog: &CatalogHandle, rng: &mut Rng, now: i64) -> Vec<Inserted> {
    for (index, (camera, recorded)) in CAMERAS.into_iter().enumerate() {
        let id = CameraId::try_new(camera).unwrap();
        catalog.ensure_camera(&id, camera, None).await.unwrap();
        let Some(recorded) = recorded else { continue };
        // Two recordings: the older one sets the horizon.
        for (file, age) in [recorded, recorded - DAY].into_iter().enumerate() {
            catalog
                .adopt_segment(NewSegment {
                    key: StreamKey {
                        camera: id.clone(),
                        kind: StreamKind::Main,
                    },
                    path: PathBuf::from(format!("/v/{index}-{file}.mp4")),
                    run_seq: 0,
                    discontinuity: true,
                    wall_start: WallUs::new(now - age),
                    wall_end: WallUs::new(now - age + 2_000_000),
                    media_start: 0,
                    media_end: 180_000,
                    init_len: 767,
                    len: 4_096,
                    fragments: 1,
                    sample_entry: None,
                })
                .await
                .unwrap();
        }
    }
    let mut inserted = Vec::new();
    for _ in 0..60 {
        let (camera, recorded) = CAMERAS[rng.below(3) as usize];
        let horizon = recorded.map(|age| now - age);
        // On a boundary of a rule, one microsecond to either side of it, or
        // anywhere in the last hundred days.
        let nudge = rng.below(3) - 1;
        let time = match (rng.below(4), horizon) {
            (0, _) => now - MAX_AGE + nudge,
            (1, Some(start)) => start - MARGIN + nudge,
            _ => now - rng.below(100) * DAY - rng.below(1_000),
        };
        let id = catalog
            .insert_event(&Event {
                camera: CameraId::try_new(camera).unwrap(),
                source: EventSource::DahuaCgi,
                kind: EventKind::Motion,
                state: EventState::Start,
                nvr_time: WallUs::new(time),
                camera_time: None,
                data: None,
            })
            .await
            .unwrap();
        // No mark, one that lapses before the maintenance instant or exactly
        // at it, or one that is still active then.
        let until = match rng.below(4) {
            0 => None,
            1 => Some(now - 1 - rng.below(1_000_000)),
            2 => Some(now),
            _ => Some(now + 1 + rng.below(1_000_000)),
        };
        if let Some(until) = until {
            catalog
                .retain_event(id, Some(WallUs::new(until)))
                .await
                .unwrap();
        }
        inserted.push(Inserted {
            id: id.0,
            horizon,
            time,
            until,
        });
    }
    inserted
}

async fn stored(catalog: &CatalogHandle) -> Vec<(i64, Option<i64>)> {
    let everything = TimeRange {
        from: WallUs::new(i64::MIN),
        to: WallUs::new(i64::MAX),
    };
    let mut rows: Vec<_> = catalog
        .events_after(None, everything, None, 10_000)
        .await
        .unwrap()
        .iter()
        .map(|row| (row.id.0, row.retain_until.map(WallUs::get)))
        .collect();
    rows.sort_unstable();
    rows
}

#[tokio::test]
async fn one_maintenance_step_keeps_exactly_what_the_lifecycle_rules_keep() {
    for seed in 1..=25 {
        let mut rng = Rng(seed);
        let dir = tempfile::tempdir().unwrap();
        let catalog = CatalogHandle::open(&dir.path().join("state").join("irisd.db")).unwrap();
        // Maintenance runs an hour after the events are stored, so a mark
        // can lapse in between.
        let now = WallUs::now().get() + Duration::from_secs(3600).as_micros() as i64;
        let inserted = populate(&catalog, &mut rng, now).await;
        assert_eq!(stored(&catalog).await.len(), inserted.len(), "seed {seed}");

        catalog.maintain_events(WallUs::new(now)).await.unwrap();
        let mut expected: Vec<_> = inserted.iter().filter_map(|e| kept(e, now)).collect();
        expected.sort_unstable();
        assert_eq!(stored(&catalog).await, expected, "seed {seed}");

        // A second step at the same instant finds nothing more to do.
        catalog.maintain_events(WallUs::new(now)).await.unwrap();
        assert_eq!(stored(&catalog).await, expected, "seed {seed}");
        catalog.shutdown();
    }
}
