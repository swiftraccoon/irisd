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

const RECORD_LEN: usize = 40;
const HEADER_LEN: usize = 64;

/// (frames, camera span in ticks to the next fragment) per fragment.
fn camera_timeline() -> Vec<(usize, i64)> {
    let bytes = include_bytes!("data/cam1-main-10min.idx");
    let (chunks, _tail) = bytes[HEADER_LEN..].as_chunks::<RECORD_LEN>();
    let records: Vec<(u16, u32)> = chunks
        .iter()
        .map(|r| {
            let n = u16::from_be_bytes([r[32], r[33]]);
            let raw = u32::from_be_bytes([r[36], r[37], r[38], r[39]]);
            (n, raw)
        })
        .collect();
    records
        .windows(2)
        .map(|w| (usize::from(w[0].0), i64::from(w[1].1.wrapping_sub(w[0].1))))
        .collect()
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
        assert_eq!(a.durations.len(), n);
        let (last, body) = a.durations.split_last().expect("n >= 1");
        // Body durations sit within 1 % of the nominal in use, which follows the
        // camera's delivered rate when its encoder runs uniformly slow (up to
        // about 2 % on this recording) and stays at the declared 30 fps otherwise.
        for d in body {
            assert!(
                (2960..=3100).contains(d),
                "fragment {i}: duration {d} outside the nominal band"
            );
        }
        // The last sample may carry whole-frame holds for camera-dropped frames.
        assert!(
            (2960..=3100).contains(last) || (*last > 3100 && *last <= 3100 + 4 * 3000),
            "fragment {i}: last duration {last} is neither nominal nor a short hold"
        );
        if *last > 3100 {
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
    // Bursty drops stay holds; the minutes in which the encoder runs uniformly
    // slow are re-timed instead, which takes some former holds away.
    assert!(
        (20..=200).contains(&holds),
        "expected the camera's dropped frames as holds, got {holds}"
    );
    assert_eq!(
        min_nominal, 3000,
        "the declared rate is in use when the camera is on rate"
    );
    assert!(
        (3000..=3060).contains(&max_nominal),
        "re-timing stays within 2 % of the declared rate, reached {max_nominal}"
    );
    assert!(max_err < tolerance.get(), "error reached {max_err} ticks");
    assert!(
        max_err < 9000,
        "error should stay within a few frames, reached {max_err}"
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
