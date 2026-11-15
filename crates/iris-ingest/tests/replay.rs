//! Replays the camera timeline of a real ten-minute recording through the
//! regulariser and checks the properties the design promises.
//!
//! The sidecar `tests/data/cam1-main-10min.idx` was written by irisd on
//! 2026-10-31 from the probed Dahua camera: 4K H.265, declared 30 fps, 2 s
//! GOP, about 0.6 % of frames dropped by the camera as single-frame holes, IDR
//! timestamps stepping by 10 ms multiples.

use iris_core::config::TimestampMode;
use iris_core::time::MediaTicks;
use iris_ingest::regularize::Regularizer;
use iris_mp4::index::{HEADER_LEN, IndexHeader, IndexRecord, RECORD_LEN};

/// The frame count and the camera's RTP timestamp of each fragment of a sidecar.
fn records(bytes: &[u8]) -> Vec<(u16, u32)> {
    IndexHeader::decode(&bytes[..HEADER_LEN]).expect("a sidecar header");
    let (chunks, _tail) = bytes[HEADER_LEN..].as_chunks::<RECORD_LEN>();
    chunks
        .iter()
        .map(|chunk| {
            let record = IndexRecord::decode(chunk);
            (record.sample_count, record.raw_rtp)
        })
        .collect()
}

/// (frames, camera span in ticks to the next fragment) per fragment.
fn spans(records: &[(u16, u32)]) -> Vec<(usize, i64)> {
    records
        .windows(2)
        .map(|w| (usize::from(w[0].0), i64::from(w[1].1.wrapping_sub(w[0].1))))
        .collect()
}

fn camera_timeline() -> Vec<(usize, i64)> {
    spans(&records(include_bytes!("data/cam1-main-10min.idx")))
}

/// Replays the sidecars of one run of a 30 fps stream, named in order by
/// `IRIS_SIDECARS` (paths separated by `:`), and prints what the regulariser
/// assigns: for checking a timing rule against recordings that are not in
/// the repository.
///
/// ```sh
/// IRIS_SIDECARS=a.idx:b.idx cargo test -p iris-ingest --test replay -- --ignored --nocapture
/// ```
#[test]
#[ignore = "reads the sidecars named by IRIS_SIDECARS"]
fn sidecars_from_the_environment_keep_the_declared_rate() {
    let paths = std::env::var("IRIS_SIDECARS").expect("IRIS_SIDECARS names the sidecars");
    let mut all = Vec::new();
    for path in paths.split(':') {
        all.extend(records(&std::fs::read(path).expect("a readable sidecar")));
    }
    let timeline = spans(&all);
    let mut r = Regularizer::new(
        TimestampMode::Regularize,
        Some(3000),
        MediaTicks::from_millis(500),
    );
    let mut raw_start = 0i64;
    let (mut snaps, mut max_err, mut frames) = (0, 0i64, 0usize);
    // Fragments by the slots they hold: none, one, two, three, more.
    let mut held = [0usize; 5];
    // Full GOPs the camera sent on the declared grid, and those of them whose frames all
    // last the declared period to a tick.
    let (mut on_grid, mut left_exact) = (0, 0);
    for &(n, span) in &timeline {
        let raw: Vec<i64> = (0..n as i64).map(|k| raw_start + k * 3000).collect();
        let a = r.assign(&raw, raw_start + span);
        raw_start += span;
        snaps += usize::from(a.snapped);
        max_err = max_err.max(r.error_ticks().abs());
        frames += n;
        held[(a.holes as usize).min(4)] += 1;
        if n == 60 && (span - 180_000).abs() < 1500 {
            on_grid += 1;
            left_exact += usize::from(a.durations.iter().all(|&d| d.abs_diff(3000) <= 1));
        }
        assert_eq!(r.nominal(), Some(3000), "the declared rate stays in use");
    }
    println!(
        "{} fragments, {frames} frames; holding 0/1/2/3/more slots: {held:?}; {left_exact} of \
         {on_grid} GOPs on the grid left as sent; {snaps} snaps; greatest error {:.1} ms",
        timeline.len(),
        max_err as f64 / 90.0
    );
    assert_eq!(snaps, 0);
    assert!(max_err < 9000, "error reached {max_err} ticks");
}

#[test]
fn real_camera_timeline_stays_locked_without_snaps() {
    let timeline = camera_timeline();
    assert!(
        timeline.len() > 250,
        "expected a long recording, got {}",
        timeline.len()
    );
    let tolerance = MediaTicks::from_millis(500);
    let mut r = Regularizer::new(TimestampMode::Regularize, Some(3000), tolerance);
    let mut raw_start = 1_000_000i64;
    let mut assigned_total = 0i64;
    let mut camera_total = 0i64;
    let mut snaps = 0;
    let mut max_err = 0i64;
    let mut holds = 0;
    let (mut min_nominal, mut max_nominal) = (u32::MAX, 0u32);
    for (i, &(n, span)) in timeline.iter().enumerate() {
        let raw: Vec<i64> = (0..n as i64).map(|k| raw_start + k * 3000).collect();
        let a = r.assign(&raw, raw_start + span);
        assert_eq!(a.durations.len(), n, "fragment {i}");
        let (last, body) = a.durations.split_last().expect("n >= 1");
        // Body durations sit within 1 % of the declared 30 fps, which stays in
        // use through the minutes in which the camera leaves slots empty.
        for d in body {
            assert!(
                (2970..=3030).contains(d),
                "fragment {i}: duration {d} outside the nominal band"
            );
        }
        // The last sample may carry whole-frame holds for the slots the camera left empty.
        assert!(
            (2970..=3030).contains(last) || (*last > 3030 && *last <= 3030 + 4 * 3000),
            "fragment {i}: last duration {last} is neither nominal nor a short hold"
        );
        if *last > 3030 {
            holds += 1;
        }
        if let Some(n) = r.nominal() {
            min_nominal = min_nominal.min(n);
            max_nominal = max_nominal.max(n);
        }
        if a.snapped {
            snaps += 1;
        }
        assigned_total += a.durations.iter().map(|&d| i64::from(d)).sum::<i64>();
        camera_total += span;
        max_err = max_err.max(r.error_ticks().abs());
        raw_start += span;
    }
    assert_eq!(
        snaps, 0,
        "no camera gap in this recording exceeds the tolerance"
    );
    // Every slot the camera left empty is a hold, in bursts and in the
    // minutes in which most GOPs lack one.
    assert!(
        (20..=300).contains(&holds),
        "expected the camera's empty slots as holds, got {holds}"
    );
    assert_eq!(
        (min_nominal, max_nominal),
        (3000, 3000),
        "the declared rate is in use throughout"
    );
    assert!(
        max_err < 3000,
        "the timeline stays within a frame of the camera's, reached {max_err} ticks"
    );
    let ratio = assigned_total as f64 / camera_total as f64;
    assert!(
        (ratio - 1.0).abs() < 0.0005,
        "assigned/camera ratio {ratio}"
    );
    assert_eq!(
        r.nominal(),
        Some(3000),
        "declared rate is kept for this camera"
    );
}
