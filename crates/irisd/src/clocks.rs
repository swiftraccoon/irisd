//! `irisd clocks`: recomputes from the sidecars of a stream how far the
//! camera's timestamps and the recorder's wall clock parted from the
//! recording's timeline (`docs/research/14-drift.md` §3, measures 1, 3,
//! and 6), for a recording whose log is gone.
//!
//! A sidecar keeps, for the first frame of every fragment, its decode time
//! on the recording's timeline, its wall time, and the low 32 bits of the
//! camera's RTP timestamp. For a video stream the timestamp less the decode
//! time is the camera's timeline against the assigned one; for an audio
//! stream, whose decode times are a count of frames, it is the camera's
//! stamps against the count. A step of the audio moves that count by whole
//! frames (`docs/research/13-audio.md` §5.3); the sidecar marks the fragment
//! that holds one, and the report counts the marks of each window.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::fs::File;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::Context;
use clap::Parser;
use iris_core::time::WallUs;
use iris_mp4::index::{
    FLAG_DISCONTINUITY_BEFORE, FLAG_FRAMES_LEFT_OUT, FLAG_FRAMES_TWICE, IndexHeader, IndexRecord,
    MAX_PERSISTED_INDEX_RECORDS, read_index_file_bounded,
};

/// The units a second of the recording's timeline and of a video timestamp.
const TIMESCALE: i64 = 90_000;

/// Arguments of `clocks`.
#[derive(Debug, Parser)]
pub struct Args {
    /// The sidecars (`.idx`) of one stream, in any order. Files of several
    /// runs are reported run by run.
    #[arg(required = true)]
    sidecars: Vec<PathBuf>,
    /// The stream is audio: its timestamps are in units of its sampling
    /// rate, which the duration of a frame gives.
    #[arg(long)]
    audio: bool,
    /// Samples in an audio frame, at least one.
    #[arg(long, default_value_t = 1_024, value_parser = clap::value_parser!(u32).range(1..))]
    frame_samples: u32,
    /// The length of a window.
    #[arg(long, value_parser = humantime::parse_duration, default_value = "10m")]
    window: Duration,
    /// Fail when the timestamps or the wall clock have moved further than
    /// this many milliseconds since a run's first window.
    #[arg(long)]
    bound_ms: Option<u32>,
}

/// One window of a run.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Window {
    /// The wall time of the window's first fragment.
    start: WallUs,
    fragments: u32,
    /// The least and the greatest of the camera's timestamps less the
    /// recording's timeline over the window's fragments, in milliseconds
    /// from the run's first fragment.
    stamps: (f64, f64),
    /// The least of the wall time less the recording's timeline, likewise.
    wall: f64,
    /// The fragments marked as holding frames left out, and frames
    /// recorded twice.
    steps: (u32, u32),
}

/// The units a second of the timestamps of a run: 90 kHz for video, and for
/// audio the sampling rate that gives its frames their duration.
fn stamp_rate(records: &[IndexRecord], audio: bool, frame_samples: u32) -> Option<i64> {
    if !audio {
        return Some(TIMESCALE);
    }
    let full = records
        .iter()
        .find(|record| record.sample_count > 0 && record.duration > 0)?;
    let frame_ticks = i64::from(full.duration) / i64::from(full.sample_count);
    let rate = i64::from(frame_samples) * TIMESCALE / frame_ticks.max(1);
    (frame_ticks > 0 && rate > 0).then_some(rate)
}

/// The windows of one run, whose records are in decode order; `None` when
/// the sidecars carry no timestamps, as those rebuilt from a scan do.
fn windows(records: &[IndexRecord], rate: i64, window: Duration) -> Option<Vec<Window>> {
    let first = records.first()?;
    if records.iter().all(|record| record.raw_rtp == 0) {
        return None;
    }
    let window_us = i64::try_from(window.as_micros()).unwrap_or(i64::MAX).max(1);
    let mut stamp = 0i64;
    let mut previous = first.raw_rtp;
    let mut out: BTreeMap<i64, Window> = BTreeMap::new();
    for record in records {
        stamp += i64::from(record.raw_rtp.wrapping_sub(previous).cast_signed());
        previous = record.raw_rtp;
        let media_us =
            (record.decode_time.saturating_sub(first.decode_time) as i64).saturating_mul(100) / 9;
        let stamps_ms = (stamp.saturating_mul(1_000_000) / rate - media_us) as f64 / 1_000.0;
        let wall_ms = (record.wall.micros_since(first.wall) - media_us) as f64 / 1_000.0;
        let entry = out.entry(media_us / window_us).or_insert(Window {
            start: record.wall,
            fragments: 0,
            stamps: (stamps_ms, stamps_ms),
            wall: wall_ms,
            steps: (0, 0),
        });
        entry.fragments += 1;
        entry.steps.0 += u32::from(record.flags & FLAG_FRAMES_LEFT_OUT != 0);
        entry.steps.1 += u32::from(record.flags & FLAG_FRAMES_TWICE != 0);
        entry.stamps = (entry.stamps.0.min(stamps_ms), entry.stamps.1.max(stamps_ms));
        entry.wall = entry.wall.min(wall_ms);
    }
    Some(out.into_values().collect())
}

/// How far the least timestamp and the wall clock of the windows moved from
/// those of the first, at most, in milliseconds.
fn greatest_moves(windows: &[Window]) -> (f64, f64) {
    let Some(first) = windows.first() else {
        return (0.0, 0.0);
    };
    let furthest = |moved: fn(&Window, &Window) -> f64| {
        windows
            .iter()
            .map(|window| moved(window, first))
            .fold(0.0_f64, |furthest, moved| {
                if moved.abs() > furthest.abs() {
                    moved
                } else {
                    furthest
                }
            })
    };
    (
        furthest(|window, first| window.stamps.0 - first.stamps.0),
        furthest(|window, first| window.wall - first.wall),
    )
}

/// The runs of the files, each a sidecar's header and records: the files in
/// the order of their first wall time, and a run for every stretch of
/// fragments on one timeline. A run ends where a file names another run,
/// where a fragment is marked as following a discontinuity, and where the
/// decode time goes backwards, as it does when the daemon starts again and
/// numbers its runs from the start.
fn runs(mut files: Vec<(IndexHeader, Vec<IndexRecord>)>) -> Vec<Vec<IndexRecord>> {
    files.sort_by_key(|(header, _)| header.start_wall.get());
    let mut runs: Vec<Vec<IndexRecord>> = Vec::new();
    let mut last: Option<(u64, u64)> = None;
    for (header, records) in files {
        for record in records {
            let continues = last.is_some_and(|(run, decode)| {
                run == header.run_id
                    && record.decode_time > decode
                    && record.flags & FLAG_DISCONTINUITY_BEFORE == 0
            });
            if !continues {
                runs.push(Vec::new());
            }
            last = Some((header.run_id, record.decode_time));
            runs.last_mut().expect("a run was pushed").push(record);
        }
    }
    runs
}

/// The report of `runs`, and the furthest any measure moved.
fn report(runs: &[Vec<IndexRecord>], args: &Args) -> (String, f64) {
    let mut out = String::new();
    let mut furthest = 0.0_f64;
    let against = if args.audio {
        "the frame count"
    } else {
        "the timeline"
    };
    for records in runs {
        let Some(first) = records.first() else {
            continue;
        };
        let run = first.wall;
        let Some(rate) = stamp_rate(records, args.audio, args.frame_samples) else {
            let _ = writeln!(out, "run from {run}: no frame with a duration");
            continue;
        };
        let _ = writeln!(
            out,
            "run from {run}: {} fragments, timestamps at {rate} Hz",
            records.len()
        );
        let Some(windows) = windows(records, rate, args.window) else {
            let _ = writeln!(
                out,
                "  no camera timestamps: the sidecar was rebuilt from a scan"
            );
            continue;
        };
        let _ = writeln!(
            out,
            "  window start                 fragments  timestamps less {against}, ms: least  \
             greatest  moved   wall less the timeline, ms: moved   fragments with frames \
             left out, twice"
        );
        let first = windows[0];
        for window in &windows {
            let _ = writeln!(
                out,
                "  {}  {:>9}  {:>10.1}  {:>8.1}  {:>6.1}  {:>10.1}  {:>4}  {:>4}",
                window.start,
                window.fragments,
                window.stamps.0,
                window.stamps.1,
                window.stamps.0 - first.stamps.0,
                window.wall - first.wall,
                window.steps.0,
                window.steps.1,
            );
        }
        let (left_out, twice) = windows.iter().fold((0, 0), |(left_out, twice), window| {
            (left_out + window.steps.0, twice + window.steps.1)
        });
        if left_out + twice > 0 {
            let _ = writeln!(
                out,
                "  steps: {left_out} fragments with frames left out, {twice} with frames \
                 recorded twice"
            );
        }
        let (stamps, wall) = greatest_moves(&windows);
        let _ = writeln!(
            out,
            "  furthest from the first window: timestamps {stamps:+.1} ms, wall clock {wall:+.1} ms"
        );
        furthest = furthest.max(stamps.abs()).max(wall.abs());
    }
    (out, furthest)
}

/// Runs the subcommand: the report goes to standard output.
pub fn run(args: &Args) -> anyhow::Result<()> {
    let mut files = Vec::new();
    for path in &args.sidecars {
        let file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
        files.push(
            read_index_file_bounded(file, MAX_PERSISTED_INDEX_RECORDS)
                .with_context(|| format!("reading {}", path.display()))?,
        );
    }
    let (text, furthest) = report(&runs(files), args);
    print!("{text}");
    if let Some(bound) = args.bound_ms {
        anyhow::ensure!(
            furthest <= f64::from(bound),
            "a measure moved {furthest:.1} ms, past the bound of {bound} ms"
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const WINDOW: Duration = Duration::from_secs(60);

    fn args(audio: bool, bound_ms: Option<u32>) -> Args {
        Args {
            sidecars: Vec::new(),
            audio,
            frame_samples: 1_024,
            window: WINDOW,
            bound_ms,
        }
    }

    /// Two-second fragments of a run: fragment `i` is stamped `stamp(i)`
    /// units after the first and received `wall_us(i)` after it.
    fn fragments(
        count: u64,
        frames: u16,
        stamp: impl Fn(u64) -> i64,
        wall_us: impl Fn(u64) -> i64,
    ) -> Vec<IndexRecord> {
        (0..count)
            .map(|i| IndexRecord {
                offset: 100 + i * 1_000,
                len: 1_000,
                decode_time: 1_000_000 + i * 180_000,
                wall: WallUs::new(1_700_000_000_000_000 + wall_us(i)),
                duration: 180_000,
                sample_count: frames,
                raw_rtp: 4_294_000_000u32.wrapping_add(stamp(i) as u32),
                ..IndexRecord::default()
            })
            .collect()
    }

    #[test]
    fn a_video_run_on_its_stamps_and_its_wall_clock_moves_nothing() {
        // Three minutes; the stamps cross the 32-bit wrap inside it.
        let records = fragments(90, 60, |i| i as i64 * 180_000, |i| i as i64 * 2_000_000);
        let windows = windows(&records, TIMESCALE, WINDOW).unwrap();
        assert_eq!(windows.len(), 3);
        assert!(windows.iter().all(|w| w.fragments == 30), "{windows:?}");
        assert!(
            windows
                .iter()
                .all(|w| w.stamps == (0.0, 0.0) && w.wall == 0.0),
            "{windows:?}"
        );
        assert_eq!(greatest_moves(&windows), (0.0, 0.0));
    }

    #[test]
    fn stamps_that_lead_the_timeline_and_a_wall_clock_that_lags_are_read_by_window() {
        // The camera's clock 1,000 ppm fast against the timeline, the
        // recorder's 500 ppm slow: 60 ms and -30 ms a minute.
        let records = fragments(90, 60, |i| i as i64 * 180_180, |i| i as i64 * 1_999_000);
        let windows = windows(&records, TIMESCALE, WINDOW).unwrap();
        let least: Vec<f64> = windows.iter().map(|w| w.stamps.0).collect();
        assert_eq!(least, [0.0, 60.0, 120.0]);
        assert_eq!(
            windows[1].stamps.1, 118.0,
            "the last fragment of the second minute"
        );
        let (stamps, wall) = greatest_moves(&windows);
        assert_eq!(stamps, 120.0);
        // The wall falls through each window, so each least is its last fragment.
        assert_eq!(wall, -60.0);
    }

    #[test]
    fn an_audio_run_is_read_at_the_rate_its_frames_give() {
        // 125 frames of 1,440 ticks a fragment: 64 kHz. The stamps run
        // 500 ppm slow against the count: -30 ms a minute, and every third
        // fragment is stamped 20 ms late on top.
        let late = |i: u64| if i.is_multiple_of(3) { 1_280 } else { 0 };
        let records = fragments(
            90,
            125,
            |i| i as i64 * 127_936 + late(i),
            |i| i as i64 * 2_000_000,
        );
        assert_eq!(stamp_rate(&records, true, 1_024), Some(64_000));
        let windows = windows(&records, 64_000, WINDOW).unwrap();
        // The run's first fragment, which is stamped late, is the zero, and
        // each window's least is its last fragment, which is not.
        let least: Vec<f64> = windows.iter().map(|w| w.stamps.0.round()).collect();
        assert_eq!(least, [-49.0, -79.0, -109.0]);
        assert_eq!(greatest_moves(&windows).0.round(), -60.0);
        assert_eq!(windows[0].stamps.1, 0.0, "the first fragment");
    }

    /// Frames that give no rate of timestamps are reported and never
    /// divided by: a frame of no samples, and one of more ticks than its
    /// samples have a second's worth of.
    #[test]
    fn a_run_without_a_timestamp_rate_is_said_and_not_divided_by() {
        let records = fragments(4, 125, |i| i as i64 * 128_000, |i| i as i64 * 2_000_000);
        assert_eq!(stamp_rate(&records, true, 0), None);
        let long = fragments(4, 1, |i| i as i64, |i| i as i64 * 2_000_000);
        assert_eq!(stamp_rate(&long, true, 1), None);
        let mut zero = args(true, None);
        zero.frame_samples = 0;
        let (text, _) = report(&[records], &zero);
        assert!(text.contains("no frame with a duration"), "{text}");
        assert!(Args::try_parse_from(["clocks", "--frame-samples", "0", "a.idx"]).is_err());
    }

    /// A step leaves frames out of the count, so the stamps stand a frame
    /// further over it from there on; the fragment that holds it is marked,
    /// and the report says so.
    #[test]
    fn the_steps_of_an_audio_run_are_counted_by_window() {
        // 64 kHz; after fragment 40 the count is three frames behind the
        // stamps, and after fragment 70 one frame ahead of there.
        let stepped = |i: u64| match i {
            0..40 => 0,
            40..70 => 3 * 1_024,
            _ => 2 * 1_024,
        };
        let mut records = fragments(
            90,
            125,
            |i| i as i64 * 128_000 + stepped(i),
            |i| i as i64 * 2_000_000,
        );
        records[40].flags |= FLAG_FRAMES_LEFT_OUT;
        records[70].flags |= FLAG_FRAMES_TWICE;
        let windows = windows(&records, 64_000, WINDOW).unwrap();
        let steps: Vec<(u32, u32)> = windows.iter().map(|w| w.steps).collect();
        assert_eq!(steps, [(0, 0), (1, 0), (0, 1)]);
        let least: Vec<f64> = windows.iter().map(|w| w.stamps.0).collect();
        assert_eq!(least, [0.0, 0.0, 32.0]);
        let (text, furthest) = report(&[records], &args(true, None));
        assert!(
            text.contains("steps: 1 fragments with frames left out, 1 with frames recorded twice"),
            "{text}"
        );
        assert_eq!(furthest, 32.0);
        let (plain, _) = report(
            &[fragments(
                30,
                125,
                |i| i as i64 * 128_000,
                |i| i as i64 * 2_000_000,
            )],
            &args(true, None),
        );
        assert!(!plain.contains("steps:"), "{plain}");
    }

    /// A sidecar's header and records, as a file read gives them.
    fn file(run_id: u64, records: Vec<IndexRecord>) -> (IndexHeader, Vec<IndexRecord>) {
        let header = IndexHeader {
            run_id,
            start_wall: records[0].wall,
            init_len: 100,
            ..IndexHeader::default()
        };
        (header, records)
    }

    #[test]
    fn a_run_ends_at_another_run_number_a_discontinuity_and_a_timeline_that_restarts() {
        let part = |from: u64, count: u64, wall_s: i64| -> Vec<IndexRecord> {
            fragments(
                from + count,
                60,
                |i| i as i64 * 180_000,
                |i| i as i64 * 2_000_000,
            )
            .into_iter()
            .skip(from as usize)
            .map(|mut record| {
                record.wall = record.wall.offset(wall_s * 1_000_000);
                record
            })
            .collect()
        };
        // Two files of one run, given out of order; then a run of another
        // number; then, an hour on, the daemon started again: the same run
        // number, and a timeline from its start; then a discontinuity.
        let mut marked = part(0, 5, 7_200);
        marked[3].flags |= FLAG_DISCONTINUITY_BEFORE;
        let files = vec![
            file(1, part(30, 30, 0)),
            file(1, part(0, 30, 0)),
            file(2, part(0, 20, 600)),
            file(1, part(0, 10, 3_600)),
            file(1, marked),
        ];
        let lengths: Vec<usize> = runs(files).iter().map(Vec::len).collect();
        assert_eq!(lengths, [60, 20, 10, 3, 2]);
    }

    #[test]
    fn runs_are_reported_apart_and_a_rebuilt_sidecar_says_what_it_lacks() {
        let fast = fragments(60, 60, |i| i as i64 * 180_180, |i| i as i64 * 2_000_000);
        let mut rebuilt = fragments(30, 60, |_| 0, |i| i as i64 * 2_000_000);
        for record in &mut rebuilt {
            record.raw_rtp = 0;
        }
        let (text, furthest) = report(&[fast, rebuilt], &args(false, None));
        assert_eq!(
            text.matches("run from 2023-11-14T22:13:20").count(),
            2,
            "{text}"
        );
        assert!(
            text.contains("60 fragments, timestamps at 90000 Hz"),
            "{text}"
        );
        assert!(
            text.contains("timestamps +60.0 ms, wall clock +0.0 ms"),
            "{text}"
        );
        assert!(
            text.contains("30 fragments, timestamps at 90000 Hz"),
            "{text}"
        );
        assert!(
            text.contains("no camera timestamps: the sidecar was rebuilt from a scan"),
            "{text}"
        );
        assert_eq!(furthest, 60.0);
    }

    #[test]
    fn the_bound_fails_a_run_that_moved_past_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.idx");
        let (header, records) = file(
            3,
            fragments(60, 60, |i| i as i64 * 180_180, |i| i as i64 * 2_000_000),
        );
        let mut bytes = header.encode().to_vec();
        for record in records {
            bytes.extend_from_slice(&record.encode());
        }
        std::fs::write(&path, bytes).unwrap();
        let with = |bound_ms| Args {
            sidecars: vec![path.clone()],
            ..args(false, bound_ms)
        };
        run(&with(None)).unwrap();
        run(&with(Some(60))).unwrap();
        let error = run(&with(Some(59))).unwrap_err();
        assert_eq!(
            error.to_string(),
            "a measure moved 60.0 ms, past the bound of 59 ms"
        );
    }
}
