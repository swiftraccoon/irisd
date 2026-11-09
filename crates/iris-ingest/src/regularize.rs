//! Constant-frame-rate timestamp regularisation.
//!
//! Cameras stamp frames with their encoder-output time at coarse resolution:
//! the probed Dahua unit quantises to 10 ms, repeats a timestamp on 6 % of
//! frames, and bunches frames by up to 100 ms, while its clock as a whole runs
//! within a few ppm of the NVR. Using those deltas verbatim makes players
//! stutter. The regulariser assigns each frame the nominal duration, keeps a
//! slow lock on the camera's long-term timeline, renders camera-dropped frames
//! as frame-long holds, and snaps to the camera's timeline only when the
//! remaining error exceeds the tolerance.

use std::collections::VecDeque;

use iris_core::config::TimestampMode;
use iris_core::time::MediaTicks;

/// Fragments remembered for the running rate estimate (about two minutes at
/// a 2 s GOP, enough to average the camera's per-GOP timestamp jitter down to
/// a few hundredths of a percent).
const HISTORY_LEN: usize = 64;
/// Frames the estimate must cover before it may override a declared rate.
const WARMUP_FRAMES: u64 = 480;
/// Fragments the estimate must cover before it may override a declared rate.
const WARMUP_FRAGMENTS: usize = 16;
/// Relative difference between the typical (median) delivered frame span and
/// the declared one above which the delivered rate replaces the declared one.
/// It equals the per-fragment correction limit: a camera whose typical
/// fragment is on rate but drops frames in bursts (0.6 % of frames, bunched)
/// stays on the declared rate with holds for the drops; one whose every
/// fragment runs long (an encoder delivering 29.3 fps under a 30 fps
/// declaration, or the probed camera during the minutes its encoder slows)
/// is re-timed, because a hold at every GOP boundary would stutter on playback.
const OVERRIDE_RATIO: f64 = 0.01;
/// Per-fragment correction limit, in parts per thousand of the fragment's
/// nominal duration. One percent (20 ms over a 2 s fragment at 30 fps) absorbs
/// sub-frame jitter and backward timestamp steps over one or two fragments,
/// which keeps the assigned timeline locked to the camera's without ever
/// accumulating a snap-sized error, while a 1 % rate change over two seconds
/// is invisible to a viewer. Leads of [`HOLE_THRESHOLD_PER_MILLE`] of a frame
/// or more are rendered as holds instead.
const MAX_CORRECTION_PER_MILLE: i64 = 10;
/// Smallest change of the measured nominal, in ticks, that moves the nominal in
/// use (hysteresis against one-tick wobble as the window fills).
const NOMINAL_HYSTERESIS: u32 = 2;
/// Share of a nominal frame, in parts per thousand, by which the camera
/// timeline must lead the assigned one before the lead is treated as dropped
/// frames and rendered as a hold rather than corrected gradually.
const HOLE_THRESHOLD_PER_MILLE: i64 = 600;
/// Share of camera time spent in holes above which the declared rate is
/// taken to be wrong (a camera declaring 30 fps and delivering 20 spends a
/// third of its time in holes; one dropping 0.6 % of frames spends 0.6 %).
const HOLE_RATIO_OVERRIDE: f64 = 0.05;
/// Consecutive snaps after which the rate estimate restarts.
const RESTART_AFTER_SNAPS: u32 = 3;
/// Fragments whose average frame span deviates from the nominal by more than
/// this ratio are left out of the rate estimate.
const ESTIMATE_OUTLIER_RATIO: f64 = 0.2;
/// Fallback nominal duration (30 fps) when nothing better is known.
pub(crate) const FALLBACK_NOMINAL: u32 = 3000;
/// How far the assigned timeline may drift from the camera's before a
/// fragment snaps to it; the sessions pass it to [`Regularizer::new`].
pub(crate) const SNAP_TOLERANCE: MediaTicks = MediaTicks::from_millis(500);

/// One measured fragment.
#[derive(Clone, Copy, Debug)]
struct Window {
    /// Camera-timeline span to the next fragment, in ticks.
    span: i64,
    /// Ticks of that span rendered as dropped-frame holds.
    hold: i64,
    /// Frames in the fragment.
    frames: u32,
}

/// Durations assigned to one fragment.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Assignment {
    /// One duration per input frame, all at least 1 tick.
    pub durations: Vec<u32>,
    /// Whether the fragment was snapped to the camera timeline.
    pub snapped: bool,
    /// Ticks added to (or removed from) `n × nominal` for this fragment.
    pub correction: i64,
    /// Ticks rendered as holds (camera-dropped frames) in this fragment.
    pub hold_ticks: i64,
    /// Whole dropped frames rendered as holds in this fragment.
    pub holes: u32,
}

/// Assigns frame durations fragment by fragment.
#[derive(Clone, Debug)]
pub struct Regularizer {
    mode: TimestampMode,
    hint: Option<u32>,
    tolerance: i64,
    /// Assigned timeline minus camera timeline at the next fragment start.
    err: i64,
    history: VecDeque<Window>,
    consecutive_snaps: u32,
    /// Measured nominal last adopted, held until the estimate moves by
    /// [`NOMINAL_HYSTERESIS`] ticks.
    adopted: Option<u32>,
}

impl Regularizer {
    /// Creates a regulariser. `nominal_hint` is the declared frame duration in
    /// ticks (for example 3000 for 30 fps); `tolerance` bounds how far the
    /// assigned timeline may drift from the camera's before snapping.
    #[must_use]
    pub fn new(mode: TimestampMode, nominal_hint: Option<u32>, tolerance: MediaTicks) -> Self {
        Self {
            mode,
            hint: nominal_hint.filter(|&n| n > 0),
            tolerance: tolerance.get().max(1),
            err: 0,
            history: VecDeque::with_capacity(HISTORY_LEN),
            consecutive_snaps: 0,
            adopted: None,
        }
    }

    /// Assigned timeline minus camera timeline, in ticks, after the last fragment.
    #[must_use]
    pub fn error_ticks(&self) -> i64 {
        self.err
    }

    /// The nominal frame duration in use, if known.
    #[must_use]
    pub fn nominal(&self) -> Option<u32> {
        let Some(hint) = self.hint else {
            return self.adopted.or_else(|| self.measured());
        };
        if !self.warmed_up() {
            return Some(hint);
        }
        match self.adopted.or_else(|| self.measured()) {
            Some(measured)
                if self.hole_ratio() > HOLE_RATIO_OVERRIDE
                    || (f64::from(measured) - f64::from(hint)).abs() / f64::from(hint)
                        > OVERRIDE_RATIO =>
            {
                Some(measured)
            }
            _ => Some(hint),
        }
    }

    /// Starts a parameter timing epoch with a different valid nominal duration.
    pub(crate) fn retarget(&mut self, nominal_hint: u32) {
        self.hint = Some(nominal_hint.max(1));
        self.err = 0;
        self.history.clear();
        self.consecutive_snaps = 0;
        self.adopted = None;
    }

    /// Re-reads the measured estimate, moving the adopted nominal only when the
    /// estimate has shifted by at least [`NOMINAL_HYSTERESIS`] ticks.
    fn adopt_measured(&mut self) {
        let Some(measured) = self.measured() else {
            return;
        };
        match self.adopted {
            Some(current) if current.abs_diff(measured) < NOMINAL_HYSTERESIS => {}
            _ => self.adopted = Some(measured),
        }
    }

    /// Whether enough fragments were measured to judge a declared rate.
    fn warmed_up(&self) -> bool {
        let frames: u64 = self.history.iter().map(|w| u64::from(w.frames)).sum();
        self.history.len() >= WARMUP_FRAGMENTS && frames >= WARMUP_FRAMES
    }

    /// Share of measured camera time that was rendered as holds.
    fn hole_ratio(&self) -> f64 {
        let (span, hold) = self
            .history
            .iter()
            .fold((0i64, 0i64), |(s, h), w| (s + w.span, h + w.hold));
        if span <= 0 {
            0.0
        } else {
            hold as f64 / span as f64
        }
    }

    /// Assigns durations to the frames with timestamps `raw` (unwrapped 90 kHz
    /// ticks) given the first timestamp of the following fragment. The
    /// estimator state moves with every call, so the result must be used.
    #[must_use]
    pub fn assign(&mut self, raw: &[i64], next_raw: i64) -> Assignment {
        let n = raw.len();
        if n == 0 {
            return Assignment {
                durations: Vec::new(),
                snapped: false,
                correction: 0,
                hold_ticks: 0,
                holes: 0,
            };
        }
        match self.mode {
            TimestampMode::Passthrough => Assignment {
                durations: raw
                    .iter()
                    .copied()
                    .zip(raw.iter().copied().skip(1).chain(std::iter::once(next_raw)))
                    .map(|(a, b)| u32::try_from((b - a).max(1)).unwrap_or(u32::MAX))
                    .collect(),
                snapped: false,
                correction: 0,
                hold_ticks: 0,
                holes: 0,
            },
            TimestampMode::Regularize => self.assign_regularized(raw, next_raw),
        }
    }

    fn assign_regularized(&mut self, raw: &[i64], next_raw: i64) -> Assignment {
        let n = raw.len();
        let n_i64 = n as i64;
        let span = next_raw - raw[0];
        if self.nominal().is_none() && span > 0 {
            // Nothing declared and nothing measured: this fragment seeds the estimate.
            self.push_history(span, 0, n);
            self.adopt_measured();
        }
        let nominal = i64::from(self.nominal().unwrap_or(FALLBACK_NOMINAL));
        let base = n_i64 * nominal;
        let mut durations = vec![u32::try_from(nominal).unwrap_or(u32::MAX); n];

        // Camera-dropped frames leave whole-frame holes in the camera timeline.
        // Render each as a frame-long hold on the fragment's last sample: the
        // hole is what the viewer saw, and honouring it keeps the assigned
        // timeline locked no matter how many frames the camera drops.
        let mut predicted_err = self.err + base - span;
        let mut hold = 0i64;
        let mut holes = 0i64;
        if -predicted_err >= nominal * HOLE_THRESHOLD_PER_MILLE / 1000 {
            holes = ((-predicted_err + nominal / 2) / nominal).max(1);
            hold = holes * nominal;
            predicted_err += hold;
        }

        let max_adjust = (base * MAX_CORRECTION_PER_MILLE / 1000).max(1);
        let correction = (-predicted_err).clamp(-max_adjust, max_adjust);
        let remaining = predicted_err + correction;

        if remaining.abs() > self.tolerance {
            return self.snap(n, nominal, span, base);
        }

        self.consecutive_snaps = 0;
        self.update_estimate(span, hold, n);
        self.adopt_measured();
        spread(&mut durations, correction);
        if hold > 0 {
            let last = durations.last_mut().expect("n >= 1");
            *last = u32::try_from(i64::from(*last) + hold).unwrap_or(u32::MAX);
        }
        let total: i64 = durations.iter().map(|&d| i64::from(d)).sum();
        self.err += total - span;
        Assignment {
            durations,
            snapped: false,
            correction: total - base,
            hold_ticks: hold,
            holes: u32::try_from(holes).unwrap_or(u32::MAX),
        }
    }

    fn snap(&mut self, n: usize, nominal: i64, span: i64, base: i64) -> Assignment {
        let n_i64 = n as i64;
        let nominal_u32 = u32::try_from(nominal).unwrap_or(u32::MAX);
        let mut durations = vec![nominal_u32; n];
        let mut hold_ticks = 0i64;
        if span > base {
            // The camera paused or dropped frames: the last frame holds.
            hold_ticks = span - base;
            let last = durations.last_mut().expect("n >= 1");
            *last = u32::try_from(i64::from(*last) + hold_ticks).unwrap_or(u32::MAX);
        } else if span >= n_i64 {
            // The camera's timeline ran ahead: spread the shorter span evenly.
            let per = span / n_i64;
            let per_u32 = u32::try_from(per).unwrap_or(u32::MAX);
            durations.fill(per_u32);
            spread(&mut durations, span - per * n_i64);
        }
        // Otherwise the camera timeline went backwards or stalled; keep
        // nominal durations and treat the camera's new position as reference.
        self.consecutive_snaps += 1;
        if self.consecutive_snaps >= RESTART_AFTER_SNAPS {
            // The camera is not running at the rate we assumed: forget both
            // the declared hint and the history so the next fragment reseeds.
            self.history.clear();
            self.hint = None;
            self.adopted = None;
            self.consecutive_snaps = 0;
        }
        self.err = 0;
        let total: i64 = durations.iter().map(|&d| i64::from(d)).sum();
        let holes = if hold_ticks > 0 {
            u32::try_from((hold_ticks / nominal.max(1)).max(1)).unwrap_or(u32::MAX)
        } else {
            0
        };
        Assignment {
            durations,
            snapped: true,
            correction: total - base,
            hold_ticks,
            holes,
        }
    }

    /// Folds a regular (not snapped) fragment into the rate estimate, ignoring
    /// fragments whose average deviates more than [`ESTIMATE_OUTLIER_RATIO`]
    /// from the nominal.
    fn update_estimate(&mut self, span: i64, hold: i64, n: usize) {
        if span <= 0 {
            return;
        }
        if let Some(nominal) = self.nominal() {
            let cadence = (span - hold) as f64 / n as f64;
            if (cadence - f64::from(nominal)).abs() / f64::from(nominal) > ESTIMATE_OUTLIER_RATIO {
                return;
            }
        }
        self.push_history(span, hold, n);
    }

    fn push_history(&mut self, span: i64, hold: i64, n: usize) {
        if self.history.len() == HISTORY_LEN {
            self.history.pop_front();
        }
        self.history.push_back(Window {
            span,
            hold,
            frames: n as u32,
        });
    }

    /// Measured frame duration in ticks. The delivered rate (holes included)
    /// when the camera spends more than [`HOLE_RATIO_OVERRIDE`] of its time
    /// in holes or, under a declared rate, when the delivered rate differs
    /// from it by more than [`OVERRIDE_RATIO`]: both mean the camera really
    /// runs at another rate. Otherwise the cadence (holes excluded), so that
    /// occasional dropped frames stay holds instead of slowing the clock.
    fn measured(&self) -> Option<u32> {
        if self.history.is_empty() {
            return None;
        }
        let (spans, holds, frames) = self
            .history
            .iter()
            .fold((0i64, 0i64, 0u64), |(s, h, f), w| {
                (s + w.span, h + w.hold, f + u64::from(w.frames))
            });
        if frames == 0 {
            return None;
        }
        let mut delivered: Vec<f64> = self
            .history
            .iter()
            .filter(|window| window.frames > 0)
            .map(|window| window.span as f64 / f64::from(window.frames))
            .collect();
        delivered.sort_by(f64::total_cmp);
        let middle = delivered.len() / 2;
        let typical = if delivered.len().is_multiple_of(2) {
            f64::midpoint(delivered[middle - 1], delivered[middle])
        } else {
            delivered[middle]
        };
        let off_rate = self.hint.is_some_and(|hint| {
            (typical - f64::from(hint)).abs() / f64::from(hint) > OVERRIDE_RATIO
        });
        let ticks = if off_rate || self.hole_ratio() > HOLE_RATIO_OVERRIDE {
            spans
        } else {
            spans - holds
        };
        let estimate = (ticks as f64 / frames as f64).round();
        Some(u32::try_from(estimate.max(1.0) as i64).unwrap_or(u32::MAX))
    }
}

/// Distributes `correction` ticks over `durations` as evenly as possible,
/// never letting a duration drop below one tick.
fn spread(durations: &mut [u32], correction: i64) {
    if durations.is_empty() || correction == 0 {
        return;
    }
    let n = durations.len() as i64;
    let per = correction / n;
    let mut remainder = correction - per * n;
    for d in durations.iter_mut() {
        let mut delta = per;
        if remainder != 0 {
            delta += remainder.signum();
            remainder -= remainder.signum();
        }
        *d = u32::try_from((i64::from(*d) + delta).max(1)).unwrap_or(u32::MAX);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tolerance() -> MediaTicks {
        MediaTicks::from_millis(150)
    }

    /// Camera-like deltas: 10 ms quantised, mean exactly 3000, with duplicates.
    fn camera_like_timestamps(frames: usize, start: i64) -> Vec<i64> {
        let mut ts = Vec::with_capacity(frames + 1);
        let mut t = start;
        for i in 0..=frames {
            ts.push(t);
            let triple = i % 3;
            let delta = if i % 30 < 3 {
                [0, 6300, 2700][triple]
            } else {
                [2700, 3600, 2700][triple]
            };
            t += delta;
        }
        ts
    }

    #[test]
    fn camera_like_jitter_yields_near_nominal_durations() {
        let ts = camera_like_timestamps(60, 1000);
        for hint in [Some(3000), None] {
            let mut r = Regularizer::new(TimestampMode::Regularize, hint, tolerance());
            let a = r.assign(&ts[..60], ts[60]);
            assert!(!a.snapped);
            assert_eq!(a.durations.len(), 60);
            for d in &a.durations {
                assert!(
                    (2970..=3030).contains(d),
                    "duration {d} outside nominal ±1 %"
                );
            }
            let total: i64 = a.durations.iter().map(|&d| i64::from(d)).sum();
            let span = ts[60] - ts[0];
            assert!(
                (total - span).abs() <= tolerance().get(),
                "total {total} vs span {span}"
            );
            assert!(r.error_ticks().abs() <= tolerance().get());
            assert_eq!(r.nominal(), Some(3000));
        }
    }

    #[test]
    fn drift_stays_locked_without_snapping() {
        let mut r = Regularizer::new(TimestampMode::Regularize, Some(3000), tolerance());
        let per_frame = 3000.15_f64;
        let mut index = 0u64;
        let stamp = |i: u64| (i as f64 * per_frame).round() as i64;
        let mut assigned_total = 0i64;
        for _ in 0..200 {
            let raw: Vec<i64> = (index..index + 60).map(stamp).collect();
            let a = r.assign(&raw, stamp(index + 60));
            assert!(!a.snapped, "snapped at fragment starting {index}");
            assert!(a.durations.iter().all(|&d| d > 0));
            assigned_total += a.durations.iter().map(|&d| i64::from(d)).sum::<i64>();
            assert!(
                r.error_ticks().abs() < 2000,
                "error {} ticks",
                r.error_ticks()
            );
            index += 60;
        }
        let camera_total = stamp(index);
        assert!((assigned_total - camera_total).abs() < 2000);
    }

    #[test]
    fn a_gap_is_honoured_by_extending_the_last_frame() {
        let mut r = Regularizer::new(TimestampMode::Regularize, Some(3000), tolerance());
        let raw: Vec<i64> = (0..60).map(|i| i * 3000).collect();
        let a = r.assign(&raw, 60 * 3000 + 90_000);
        // A one-second gap is thirty dropped frames: rendered as a hold, not a snap.
        assert!(!a.snapped);
        assert_eq!(a.durations[59], 93_000);
        assert!(a.durations[..59].iter().all(|&d| d == 3000));
        assert_eq!((a.holes, a.hold_ticks), (30, 30 * 3000));
        assert_eq!(r.error_ticks(), 0);
        // The next fragment proceeds normally.
        let raw2: Vec<i64> = (0..60).map(|i| 60 * 3000 + 90_000 + i * 3000).collect();
        let b = r.assign(&raw2, raw2[59] + 3000);
        assert!(!b.snapped);
        assert_eq!((b.holes, b.hold_ticks), (0, 0));
    }

    #[test]
    fn a_snap_past_the_tolerance_reports_its_hold() {
        // At 2 fps a hold's remainder exceeds the 150 ms tolerance, so the
        // fragment snaps with the camera's full lead on its last frame.
        let mut r = Regularizer::new(TimestampMode::Regularize, Some(45_000), tolerance());
        let raw: Vec<i64> = (0..4).map(|i| i * 45_000).collect();
        let a = r.assign(&raw, 4 * 45_000 + 65_000);
        assert!(a.snapped);
        assert_eq!(a.durations[3], 45_000 + 65_000);
        assert_eq!((a.holes, a.hold_ticks), (1, 65_000));
    }

    #[test]
    fn backwards_camera_timeline_keeps_nominal_durations() {
        let mut r = Regularizer::new(TimestampMode::Regularize, Some(3000), tolerance());
        let raw: Vec<i64> = (0..30).map(|i| 1_000_000 + i * 3000).collect();
        let a = r.assign(&raw, 1_000_000 - 500_000);
        assert!(a.snapped);
        assert!(a.durations.iter().all(|&d| d == 3000));
        assert_eq!((a.holes, a.hold_ticks), (0, 0));
        assert_eq!(r.error_ticks(), 0);
    }

    #[test]
    fn a_uniformly_slow_camera_is_retimed_instead_of_held_every_gop() {
        // An encoder delivering 29.29 fps under a 30 fps declaration: 3073 ticks per frame,
        // 2.4 % slow, beyond the 1 % the per-fragment correction absorbs.
        let mut r = Regularizer::new(TimestampMode::Regularize, Some(3000), tolerance());
        let mut holes_after_warmup = 0u32;
        let mut durations_after_warmup = Vec::new();
        for gop in 0..40i64 {
            let start = gop * 60 * 3073;
            let raw: Vec<i64> = (0..60).map(|i| start + i * 3073).collect();
            let a = r.assign(&raw, start + 60 * 3073);
            assert!(!a.snapped, "snapped at GOP {gop}");
            if gop >= 24 {
                holes_after_warmup += a.holes;
                durations_after_warmup.extend(a.durations.iter().copied());
            }
        }
        assert_eq!(
            holes_after_warmup, 0,
            "a slow encoder must not be padded with holds"
        );
        assert!(
            (3070..=3076).contains(&r.nominal().unwrap()),
            "nominal {:?}",
            r.nominal()
        );
        assert!(
            durations_after_warmup
                .iter()
                .all(|&d| (3040..=3110).contains(&d)),
            "durations stay near the delivered rate"
        );
    }

    #[test]
    fn a_single_gap_does_not_override_the_typical_delivered_rate() {
        let mut r = Regularizer::new(TimestampMode::Regularize, Some(3000), tolerance());
        let mut start = 0;
        for gop in 0..70 {
            let raw: Vec<i64> = (0..60).map(|i| start + i * 3000).collect();
            let next = start + 180_000 + if gop == 0 { 90_000 } else { 0 };
            let assignment = r.assign(&raw, next);
            assert!(
                !assignment.snapped,
                "a burst gap must stay a hold at GOP {gop}"
            );
            assert_eq!(r.nominal(), Some(3000), "GOP {gop}");
            start = next;
        }
    }

    #[test]
    fn a_wrong_declared_rate_is_replaced_by_the_measured_one() {
        // Declared 30 fps, camera delivers 20 fps (4500 ticks).
        let mut r = Regularizer::new(TimestampMode::Regularize, Some(3000), tolerance());
        let mut snapped = 0;
        let mut last = None;
        for k in 0..24 {
            let raw: Vec<i64> = (0..60).map(|i| (k * 60 + i) * 4500).collect();
            let a = r.assign(&raw, (k * 60 + 60) * 4500);
            if a.snapped {
                snapped += 1;
            }
            last = Some(a);
        }
        assert_eq!(snapped, 0, "holes, not snaps, cover the shortfall");
        assert_eq!(r.nominal(), Some(4500));
        // Once the delivered rate is adopted no holds remain.
        let last = last.expect("fragments assigned");
        assert!(
            last.durations.iter().all(|&d| (4455..=4545).contains(&d)),
            "{:?}",
            &last.durations[..4]
        );
    }

    #[test]
    fn a_changed_parameter_rate_starts_a_fresh_timing_epoch() {
        let mut r = Regularizer::new(TimestampMode::Regularize, Some(3000), tolerance());
        let mut start = 0i64;
        for _ in 0..HISTORY_LEN {
            let raw: Vec<i64> = (0..60).map(|index| start + index * 3000).collect();
            start += 180_000;
            let _ = r.assign(&raw, start);
        }
        assert_eq!(r.history.len(), HISTORY_LEN);

        r.retarget(6000);
        assert!(r.history.is_empty());
        assert_eq!(r.error_ticks(), 0);
        assert_eq!(r.nominal(), Some(6000));
        let raw: Vec<i64> = (0..30).map(|index| start + index * 6000).collect();
        let assignment = r.assign(&raw, start + 180_000);
        assert_eq!((assignment.holes, assignment.hold_ticks), (0, 0));
        assert!(
            assignment
                .durations
                .iter()
                .all(|&duration| duration == 6000)
        );
    }

    #[test]
    fn passthrough_reproduces_raw_deltas_with_a_floor_of_one() {
        let mut r = Regularizer::new(TimestampMode::Passthrough, Some(3000), tolerance());
        let raw = [0, 2700, 6300, 6300, 7200];
        let a = r.assign(&raw, 10_200);
        assert_eq!(a.durations, vec![2700, 3600, 1, 900, 3000]);
        assert!(!a.snapped);
        assert_eq!((a.holes, a.hold_ticks), (0, 0));
    }

    #[test]
    fn empty_input_is_harmless() {
        let mut r = Regularizer::new(TimestampMode::Regularize, None, tolerance());
        assert_eq!(r.assign(&[], 10).durations, Vec::<u32>::new());
    }

    /// A small deterministic generator (xorshift64*) for the property tests.
    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> u64 {
            let mut x = self.0;
            x ^= x >> 12;
            x ^= x << 25;
            x ^= x >> 27;
            self.0 = x;
            x.wrapping_mul(0x2545_f491_4f6c_dd1d)
        }

        fn below(&mut self, n: u64) -> u64 {
            self.next() % n
        }
    }

    /// A camera-like timeline: frames of the nominal duration with ±30 %
    /// jitter, repeated timestamps, and occasional dropped frames.
    fn generated_timeline(
        rng: &mut Rng,
        nominal: i64,
        frames: usize,
        start: i64,
    ) -> (Vec<i64>, i64) {
        let mut raw = Vec::with_capacity(frames);
        let mut t = start;
        for _ in 0..frames {
            raw.push(t);
            t += match rng.below(20) {
                0 => 0,
                1 => 2 * nominal,
                _ => nominal * (70 + rng.below(61) as i64) / 100,
            };
        }
        (raw, t)
    }

    #[test]
    fn generated_timelines_keep_every_duration_positive_and_the_error_bounded() {
        let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
        for case in 0..200 {
            let nominal = [3000i64, 3600, 1500, 6000][case % 4];
            let mut r =
                Regularizer::new(TimestampMode::Regularize, Some(nominal as u32), tolerance());
            let mut start = 1_000_000i64;
            for fragment in 0..40 {
                let frames = 1 + rng.below(90) as usize;
                let (raw, next) = generated_timeline(&mut rng, nominal, frames, start);
                let a = r.assign(&raw, next);
                assert_eq!(a.durations.len(), frames, "case {case} fragment {fragment}");
                assert!(
                    a.durations.iter().all(|&d| d >= 1),
                    "case {case} fragment {fragment}: {a:?}"
                );
                let span = next - raw[0];
                let total: i64 = a.durations.iter().map(|&d| i64::from(d)).sum();
                if a.snapped {
                    if span >= frames as i64 {
                        assert_eq!(
                            total, span,
                            "case {case} fragment {fragment}: a snap covers the span"
                        );
                    }
                } else {
                    assert!(
                        r.error_ticks().abs() <= tolerance().get(),
                        "case {case} fragment {fragment}: error {} after a fragment that did not snap",
                        r.error_ticks()
                    );
                }
                start = next;
            }
        }
    }

    #[test]
    fn passthrough_reproduces_generated_deltas_with_a_floor_of_one() {
        let mut rng = Rng(0x5851_f42d_4c95_7f2d);
        for case in 0..100 {
            let mut r = Regularizer::new(TimestampMode::Passthrough, Some(3000), tolerance());
            let frames = 1 + rng.below(60) as usize;
            let (raw, next) = generated_timeline(&mut rng, 3000, frames, 500_000);
            let a = r.assign(&raw, next);
            let expected: Vec<u32> = raw
                .iter()
                .zip(raw.iter().skip(1).chain(std::iter::once(&next)))
                .map(|(a, b)| u32::try_from((b - a).max(1)).unwrap_or(u32::MAX))
                .collect();
            assert_eq!(a.durations, expected, "case {case}");
            assert!(!a.snapped);
        }
    }

    #[test]
    fn spread_never_produces_zero() {
        let mut d = vec![1u32; 5];
        spread(&mut d, -7);
        assert!(d.iter().all(|&x| x >= 1));
        let mut e = vec![3000u32; 4];
        spread(&mut e, 6);
        assert_eq!(e, vec![3002, 3002, 3001, 3001]);
    }
}
