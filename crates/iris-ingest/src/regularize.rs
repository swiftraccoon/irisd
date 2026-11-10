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
/// The share of the tolerance (one in this many, 62 ms of 500 ms) that the
/// error must have reached before saturated corrections restart the
/// estimate. A camera whose GOPs each run a few percent faster or slower
/// than its mean saturates the correction for several fragments in a row
/// with the error staying small; an estimate that is off lets it grow at
/// every fragment.
const RESTART_ERROR_SHARE: i64 = 8;
/// Consecutive snaps after which the rate estimate restarts.
const RESTART_AFTER_SNAPS: u32 = 3;
/// Consecutive fragments whose correction is at its limit in one direction
/// after which the rate estimate restarts. One timestamp step saturates the
/// correction for a fragment or two (20 ms each at a 2 s GOP); four in a row
/// mean the nominal itself is off, as when a camera returns to its declared
/// rate while the estimate still averages two minutes of a slower one.
/// Restarting keeps the error that builds meanwhile under the tolerance: at
/// a 5.7 % mismatch (28.3 fps adopted, 30 delivered) it reaches 380 ms. A
/// restart cuts the history to these fragments and keeps the warm-up, so
/// the estimate moves to what the camera delivers at this moment and never
/// falls back to a declared rate the camera has already been measured off.
const RESTART_AFTER_SATURATED: u32 = 4;
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
    /// Consecutive fragments corrected by the full limit in one direction,
    /// as a signed count.
    saturated: i32,
    /// Measured nominal last adopted, held until the estimate moves by
    /// [`NOMINAL_HYSTERESIS`] ticks.
    adopted: Option<u32>,
    /// Whether the estimate has covered the warm-up since the last timing
    /// epoch began; it stays set when a restart shortens the history.
    warmed: bool,
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
            saturated: 0,
            adopted: None,
            warmed: false,
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
                if (f64::from(measured) - f64::from(hint)).abs() / f64::from(hint)
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
        self.saturated = 0;
        self.adopted = None;
        self.warmed = false;
    }

    /// Counts a fragment's correction towards a stale estimate and restarts
    /// the estimate after [`RESTART_AFTER_SATURATED`] at the limit in one
    /// direction, once the error left after them (`remaining`) has reached
    /// an eighth of the tolerance: the history is cut to those fragments, so
    /// the estimate is what the camera delivers at this moment.
    fn note_correction(&mut self, correction: i64, max_adjust: i64, remaining: i64) {
        let direction = if correction.abs() < max_adjust {
            0
        } else {
            correction.signum() as i32
        };
        self.saturated = if direction != 0 && direction == self.saturated.signum() {
            self.saturated + direction
        } else {
            direction
        };
        if self.saturated.unsigned_abs() >= RESTART_AFTER_SATURATED
            && remaining.abs() >= self.tolerance / RESTART_ERROR_SHARE
        {
            let stale = self
                .history
                .len()
                .saturating_sub(RESTART_AFTER_SATURATED as usize);
            self.history.drain(..stale);
            self.adopted = None;
            self.saturated = 0;
        }
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
        self.warmed || (self.history.len() >= WARMUP_FRAGMENTS && frames >= WARMUP_FRAMES)
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
        self.note_correction(correction, max_adjust, remaining);
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

    /// Puts the assigned timeline back on the camera's: the fragment lasts
    /// its camera span less the error carried into it, so the error is zero
    /// afterwards. Without the carried error a snap would leave it in the
    /// timeline for good, and snaps that follow a stale rate would add up.
    /// A fragment whose span is shorter than the carried error cannot absorb
    /// all of it: its frames last one tick each and the rest stays carried
    /// for the fragments after it.
    fn snap(&mut self, n: usize, nominal: i64, span: i64, base: i64) -> Assignment {
        let n_i64 = n as i64;
        let nominal_u32 = u32::try_from(nominal).unwrap_or(u32::MAX);
        let mut durations = vec![nominal_u32; n];
        let mut hold_ticks = 0i64;
        let target = span.saturating_sub(self.err);
        let mut carried = 0i64;
        if target > base {
            // The camera paused or dropped frames: the last frame holds.
            hold_ticks = target - base;
            let last = durations.last_mut().expect("n >= 1");
            *last = u32::try_from(i64::from(*last) + hold_ticks).unwrap_or(u32::MAX);
        } else if span >= 0 {
            // The assigned timeline is ahead of the camera's: spread the
            // shorter time evenly, at least one tick a frame.
            let shortened = target.max(n_i64);
            let per = shortened / n_i64;
            let per_u32 = u32::try_from(per).unwrap_or(u32::MAX);
            durations.fill(per_u32);
            spread(&mut durations, shortened - per * n_i64);
            carried = shortened - target;
        }
        // Otherwise the camera timeline went backwards; keep nominal
        // durations and treat the position the camera stepped to as reference.
        self.consecutive_snaps += 1;
        if self.consecutive_snaps >= RESTART_AFTER_SNAPS {
            // The camera is not running at the rate we assumed: forget both
            // the declared hint and the history so the next fragment reseeds.
            self.history.clear();
            self.hint = None;
            self.adopted = None;
            self.warmed = false;
            self.consecutive_snaps = 0;
        }
        self.saturated = 0;
        self.err = carried;
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
        self.warmed = self.warmed_up();
    }

    /// Measured frame duration in ticks. The delivered rate (holes included)
    /// when the typical fragment's delivered frame span differs by more than
    /// [`OVERRIDE_RATIO`] from the declared duration, or from the cadence
    /// when none is declared: the camera then really runs at another rate.
    /// Otherwise the cadence (holes excluded), so that occasional dropped
    /// frames and stalls stay holds instead of slowing the clock. The share
    /// of time spent in holes does not decide: a camera that stalls for a
    /// second now and then spends much of its time in holes while every
    /// other fragment is on rate.
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
        // The median over frames, not fragments: a GOP split at the cap
        // leaves a remainder of a few frames whose span per frame is mostly
        // timestamp quantisation, and it must not count like a full fragment.
        let mut delivered: Vec<(f64, u64)> = self
            .history
            .iter()
            .filter(|window| window.frames > 0)
            .map(|window| {
                (
                    window.span as f64 / f64::from(window.frames),
                    u64::from(window.frames),
                )
            })
            .collect();
        delivered.sort_by(|a, b| a.0.total_cmp(&b.0));
        let mut below = 0u64;
        let typical = delivered
            .iter()
            .find(|(_, weight)| {
                below += weight;
                below * 2 >= frames
            })
            .map_or(0.0, |(span, _)| *span);
        let cadence = (spans - holds) as f64 / frames as f64;
        let reference = self.hint.map_or(cadence, f64::from);
        let off_rate = reference > 0.0 && (typical - reference).abs() / reference > OVERRIDE_RATIO;
        let ticks = if off_rate { spans } else { spans - holds };
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

    /// A stale rate lets the error grow to the tolerance. The snap that
    /// follows takes the carried error out of its fragment, so the assigned
    /// timeline is back on the camera's, not offset by what was carried.
    #[test]
    fn a_snap_absorbs_the_error_carried_into_it() {
        let mut r = Regularizer::new(TimestampMode::Regularize, Some(3_150), tolerance());
        let mut assigned = 0i64;
        let mut snapped_at = None;
        // The camera delivers every 3000 ticks under a declared 3150: each
        // 60-frame fragment runs 9000 ticks long, of which 1 % is corrected.
        for fragment in 0..4i64 {
            let raw: Vec<i64> = (0..60).map(|i| (fragment * 60 + i) * 3000).collect();
            let a = r.assign(&raw, (fragment + 1) * 180_000);
            assigned += a.durations.iter().map(|&d| i64::from(d)).sum::<i64>();
            if a.snapped {
                snapped_at = Some(fragment);
                break;
            }
            assert!(assigned > (fragment + 1) * 180_000, "the error grows");
        }
        let fragment = snapped_at.expect("the error passes 150 ms within four fragments");
        assert!(fragment >= 1, "an error was carried into the snap");
        assert_eq!(assigned, (fragment + 1) * 180_000);
        assert_eq!(r.error_ticks(), 0);
    }

    /// A short fragment whose camera span is less than the error carried
    /// into its snap: its frames last one tick each, and what it could not
    /// absorb stays carried, so the assigned timeline is still measured
    /// against the camera's and the next fragment absorbs the rest.
    #[test]
    fn a_snap_in_a_fragment_shorter_than_the_carried_error_carries_the_rest() {
        let mut r = Regularizer::new(TimestampMode::Regularize, Some(3000), tolerance());
        let mut assigned = 0i64;
        let mut camera = 0i64;
        let mut assign = |r: &mut Regularizer, frames: i64, per_frame: i64| {
            let raw: Vec<i64> = (0..frames).map(|i| camera + i * per_frame).collect();
            camera += frames * per_frame;
            let a = r.assign(&raw, camera);
            assigned += a.durations.iter().map(|&d| i64::from(d)).sum::<i64>();
            assert_eq!(r.error_ticks(), assigned - camera);
            a
        };
        // Three fragments 3 % short of the declared rate leave 120 ms.
        for _ in 0..3 {
            assert!(!assign(&mut r, 60, 2910).snapped);
        }
        assert_eq!(r.error_ticks(), 10_800);
        // Five frames spanning 56 ms cannot absorb 120 ms.
        let short = assign(&mut r, 5, 1000);
        assert!(short.snapped);
        assert_eq!(short.durations, [1; 5]);
        assert_eq!(r.error_ticks(), 10_800 + 5 - 5_000);
        // The fragments after it correct the rest away without a snap.
        for _ in 0..4 {
            assert!(!assign(&mut r, 60, 3000).snapped);
        }
        assert_eq!(r.error_ticks(), 0);
    }

    /// A camera stamped at 10 ms resolution delivering at `ticks` per frame.
    fn stamps(count: usize, ticks_per_frame: f64, from: f64) -> (Vec<i64>, f64) {
        let mut exact = from;
        let raw = (0..count)
            .map(|_| {
                let stamp = (exact / 900.0).floor() as i64 * 900;
                exact += ticks_per_frame;
                stamp
            })
            .collect();
        (raw, exact)
    }

    /// What the test cameras do: the encoder slows for minutes and returns
    /// to its declared rate. Going slow costs holds until the delivered rate
    /// is adopted; coming back, the adopted rate is stale, the correction
    /// saturates, and the estimate restarts before the error reaches the
    /// tolerance. No fragment snaps, and while the camera holds its slow
    /// rate the adopted one is not disturbed.
    #[test]
    fn a_camera_that_slows_and_returns_to_its_declared_rate_never_snaps() {
        for fps in [28.3, 28.9, 29.4] {
            let slow = 90_000.0 / fps;
            let mut r = Regularizer::new(
                TimestampMode::Regularize,
                Some(3000),
                MediaTicks::from_millis(500),
            );
            let mut exact = 1_000_000.0;
            let mut pending: Option<Vec<i64>> = None;
            // Ten minutes on rate, ten slow, ten on rate again, in 2 s GOPs.
            for (phase, per_frame) in [(0, 3000.0), (1, slow), (2, 3000.0)] {
                for fragment in 0..300 {
                    let (raw, next) = stamps(60, per_frame, exact);
                    exact = next;
                    let Some(previous) = pending.replace(raw.clone()) else {
                        continue;
                    };
                    let a = r.assign(&previous, raw[0]);
                    assert!(!a.snapped, "{fps} fps: snap in phase {phase} at {fragment}");
                    assert!(
                        r.error_ticks().abs() < 45_000,
                        "{fps} fps: error {} in phase {phase} at {fragment}",
                        r.error_ticks()
                    );
                    if phase == 1 && fragment >= 100 {
                        let nominal = f64::from(r.nominal().unwrap());
                        assert!(
                            (nominal - slow).abs() / slow < 0.005,
                            "{fps} fps: nominal {nominal} at {fragment} of the slow phase"
                        );
                    }
                    if phase == 2 && fragment >= 100 {
                        assert_eq!(r.nominal(), Some(3000), "{fps} fps at {fragment}");
                    }
                }
            }
        }
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

    /// A camera on its declared rate that stalls for a second and a half in
    /// every sixth fragment spends a ninth of its time in holes. The stalls
    /// stay holds and the declared rate stays in use: re-timing to the mean
    /// delivered rate would put every other fragment 13 % off and snap.
    #[test]
    fn a_camera_that_stalls_now_and_then_keeps_its_declared_rate() {
        let mut r = Regularizer::new(TimestampMode::Regularize, Some(3000), tolerance());
        let mut start = 0;
        for gop in 0..120 {
            let stall = if gop % 6 == 5 { 135_000 } else { 0 };
            let raw: Vec<i64> = (0..60).map(|i| start + i * 3000).collect();
            let next = start + 180_000 + stall;
            let a = r.assign(&raw, next);
            assert!(!a.snapped, "GOP {gop}");
            assert_eq!(a.hold_ticks, stall, "GOP {gop}");
            assert_eq!(r.nominal(), Some(3000), "GOP {gop}");
            assert_eq!(r.error_ticks(), 0, "GOP {gop}");
            start = next;
        }
    }

    /// A GOP five frames longer than the fragment cap leaves a five-frame
    /// remainder after every full fragment, and a remainder whose first
    /// stamp is rounded down spans 3120 ticks a frame. With three frames
    /// dropped in every third full fragment, four fragments in six run more
    /// than 1 % long but most frames are in fragments on rate, so the
    /// declared rate stays in use and the drops stay holds.
    #[test]
    fn short_remainder_fragments_do_not_outvote_the_full_ones() {
        let mut r = Regularizer::new(TimestampMode::Regularize, Some(3000), tolerance());
        let mut start = 0;
        for gop in 0..60 {
            let dropped = if gop % 3 == 0 { 9000 } else { 0 };
            let full: Vec<i64> = (0..75).map(|i| start + i * 3000).collect();
            let remainder_start = start + 225_000 + dropped - 600;
            let a = r.assign(&full, remainder_start);
            assert!(!a.snapped, "GOP {gop}");
            assert_eq!(a.hold_ticks, dropped, "GOP {gop}");
            let remainder: Vec<i64> = (0..5).map(|i| remainder_start + i * 3000).collect();
            start += 240_000 + dropped;
            let a = r.assign(&remainder, start);
            assert!(!a.snapped, "GOP {gop}");
            assert_eq!(r.nominal(), Some(3000), "GOP {gop}");
        }
    }

    /// Without a declared rate the first fragment seeds the estimate. One
    /// whose stamps ran a third fast seeds 2000 ticks for a 3000-tick camera,
    /// and every later fragment would end in a hold a third of its length.
    /// The typical delivered span is off that cadence, so the delivered rate
    /// takes over and the holds stop.
    #[test]
    fn an_undeclared_rate_seeded_too_fast_moves_to_the_delivered_rate() {
        let mut r = Regularizer::new(TimestampMode::Regularize, None, tolerance());
        let first: Vec<i64> = (0..60).map(|i| i * 2000).collect();
        let mut start = 120_000;
        let _ = r.assign(&first, start);
        assert_eq!(r.nominal(), Some(2000));
        let mut late_holds = 0;
        for gop in 0..80 {
            let raw: Vec<i64> = (0..60).map(|i| start + i * 3000).collect();
            start += 180_000;
            let a = r.assign(&raw, start);
            if gop >= 70 {
                late_holds += a.hold_ticks;
            }
        }
        assert_eq!(r.nominal(), Some(3000));
        assert_eq!(late_holds, 0);
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
                let carried = r.error_ticks();
                let a = r.assign(&raw, next);
                assert_eq!(a.durations.len(), frames, "case {case} fragment {fragment}");
                assert!(
                    a.durations.iter().all(|&d| d >= 1),
                    "case {case} fragment {fragment}: {a:?}"
                );
                let span = next - raw[0];
                let total: i64 = a.durations.iter().map(|&d| i64::from(d)).sum();
                if a.snapped {
                    if span - carried >= frames as i64 {
                        assert_eq!(
                            total,
                            span - carried,
                            "case {case} fragment {fragment}: a snap covers the span less \
                             the error carried in"
                        );
                    }
                    // A span shorter than the error carried in leaves the
                    // rest carried, with every frame one tick long.
                    let left = if span >= 0 {
                        (carried - (span - frames as i64)).max(0)
                    } else {
                        0
                    };
                    assert_eq!(r.error_ticks(), left, "case {case} fragment {fragment}");
                    if left > 0 {
                        assert_eq!(total, frames as i64, "case {case} fragment {fragment}");
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

    /// The measured rate may replace a declared one only after sixteen
    /// fragments and 480 frames, both: eight full fragments are too few
    /// fragments, and sixteen short ones too few frames.
    #[test]
    fn the_warm_up_needs_both_its_fragments_and_its_frames() {
        for (fragments, frames, warmed) in [(8, 60, false), (16, 29, false), (16, 30, true)] {
            let mut r = Regularizer::new(TimestampMode::Regularize, Some(3000), tolerance());
            let mut start = 0;
            for _ in 0..fragments {
                // 2.4 % slower than declared.
                let raw: Vec<i64> = (0..frames).map(|i| start + i * 3073).collect();
                start += frames * 3073;
                let _ = r.assign(&raw, start);
            }
            let expected = if warmed { 3073 } else { 3000 };
            assert_eq!(
                r.nominal(),
                Some(expected),
                "{fragments} fragments of {frames} frames"
            );
        }
    }

    /// A camera lead of 0.6 frame or more is a dropped frame, rendered as a
    /// whole-frame hold; less is corrected, at most 1 % of the fragment, and
    /// the assignment reports what it added to the nominal durations.
    #[test]
    fn a_lead_of_six_tenths_of_a_frame_becomes_a_hold_and_less_a_correction() {
        let raw: Vec<i64> = (0..60).map(|i| i * 3000).collect();

        let mut r = Regularizer::new(TimestampMode::Regularize, Some(3000), tolerance());
        let held = r.assign(&raw, 180_000 + 1_800);
        assert_eq!((held.holes, held.hold_ticks), (1, 3000));
        // The hold overshoots the lead by 1,200 ticks, which the frames
        // give back.
        assert_eq!(held.correction, 3000 - 1_200);
        assert_eq!(r.error_ticks(), 0);

        let mut r = Regularizer::new(TimestampMode::Regularize, Some(3000), tolerance());
        let corrected = r.assign(&raw, 180_000 + 1_799);
        assert_eq!((corrected.holes, corrected.hold_ticks), (0, 0));
        assert_eq!(corrected.correction, 1_799);
        assert_eq!(r.error_ticks(), 0);

        // A lag is corrected the other way, and only by the limit.
        let mut r = Regularizer::new(TimestampMode::Regularize, Some(3000), tolerance());
        let limited = r.assign(&raw, 180_000 - 2_500);
        assert_eq!(limited.correction, -1_800);
        assert_eq!(r.error_ticks(), 700);
    }

    /// Only an unbroken run of corrections at the limit in one direction
    /// restarts the estimate, and only once the error left after them has
    /// reached an eighth of the tolerance: a correction the other way or
    /// below the limit starts the count again, and a small error means the
    /// camera is varying around the estimate, not away from it. A restart
    /// keeps the fragments of the run and the warm-up.
    #[test]
    fn the_estimate_restarts_after_four_saturated_corrections_that_leave_an_error() {
        let measured = Window {
            span: 180_000,
            hold: 0,
            frames: 60,
        };
        // The share of the 150 ms test tolerance that allows a restart.
        let enough = tolerance().get() / RESTART_ERROR_SHARE;
        let mut r = Regularizer::new(TimestampMode::Regularize, Some(3000), tolerance());
        r.history.extend([measured; 20]);
        r.warmed = true;
        r.adopted = Some(3000);
        for _ in 0..6 {
            // Two at the limit, then one the other way: never four in a row.
            r.note_correction(1800, 1800, enough);
            r.note_correction(1800, 1800, enough);
            r.note_correction(-1800, 1800, -enough);
        }
        for _ in 0..6 {
            // One below the limit, then three at it.
            r.note_correction(-1799, 1800, -enough);
            for _ in 0..3 {
                r.note_correction(-1800, 1800, -enough);
            }
        }
        r.note_correction(0, 1800, 0);
        // Any number at the limit while the error stays under that share.
        for _ in 0..12 {
            r.note_correction(1800, 1800, enough - 1);
        }
        assert_eq!(r.history.len(), 20, "no restart");
        assert_eq!(r.adopted, Some(3000));
        for direction in [1, -1] {
            r.note_correction(0, 1800, 0);
            r.history.extend([measured; 16]);
            r.adopted = Some(3000);
            for _ in 0..3 {
                r.note_correction(direction * 1800, 1800, -direction * enough);
                assert!(r.history.len() >= 20);
            }
            r.note_correction(direction * 1800, 1800, -direction * enough);
            assert_eq!(r.history.len(), 4, "direction {direction}");
            assert_eq!(r.adopted, None);
            assert!(r.warmed_up(), "the warm-up outlives the restart");
        }
    }

    /// The nominal in use follows the measured one only once they differ by
    /// two ticks, so a one-tick wobble of the estimate does not move it.
    #[test]
    fn the_adopted_nominal_ignores_a_one_tick_wobble_of_the_estimate() {
        let mut r = Regularizer::new(TimestampMode::Regularize, None, tolerance());
        let measure = |r: &mut Regularizer, per_frame: i64| {
            r.history.clear();
            r.history.push_back(Window {
                span: 60 * per_frame,
                hold: 0,
                frames: 60,
            });
            r.adopt_measured();
            r.nominal()
        };
        assert_eq!(measure(&mut r, 3073), Some(3073));
        assert_eq!(measure(&mut r, 3074), Some(3073));
        assert_eq!(measure(&mut r, 3072), Some(3073));
        assert_eq!(measure(&mut r, 3075), Some(3075));
        assert_eq!(measure(&mut r, 3073), Some(3073));
    }

    /// Camera span in ticks and frame count of 144 consecutive fragments of a
    /// 1080p stream declared at 30 fps whose encoder ran between 27.3 and
    /// 29.0 fps, a different rate in every GOP: five minutes of a recording
    /// of 2026-11-10, with two GOPs split at the fragment cap.
    const VARYING_SLOW_CAMERA: [(i64, i64); 144] = [
        (186_300, 60),
        (189_000, 60),
        (192_600, 60),
        (192_600, 60),
        (186_160, 60),
        (188_240, 60),
        (189_000, 60),
        (186_160, 60),
        (188_240, 60),
        (189_000, 60),
        (189_000, 60),
        (189_000, 60),
        (186_160, 60),
        (186_160, 60),
        (222_580, 69),
        (155_232, 50),
        (186_279, 60),
        (186_279, 60),
        (186_279, 60),
        (185_931, 60),
        (188_100, 60),
        (186_300, 60),
        (189_000, 60),
        (191_700, 60),
        (189_000, 60),
        (189_900, 60),
        (191_700, 60),
        (181_800, 60),
        (189_900, 60),
        (191_700, 60),
        (186_300, 60),
        (186_300, 60),
        (191_700, 60),
        (186_300, 60),
        (189_000, 60),
        (189_000, 60),
        (194_400, 60),
        (186_932, 60),
        (186_576, 60),
        (186_219, 60),
        (186_398, 60),
        (187_175, 60),
        (189_000, 60),
        (186_300, 60),
        (189_000, 60),
        (186_300, 60),
        (189_000, 60),
        (188_100, 60),
        (195_300, 60),
        (186_300, 60),
        (189_000, 60),
        (191_700, 60),
        (186_300, 60),
        (185_400, 60),
        (189_000, 60),
        (189_000, 60),
        (189_000, 60),
        (192_600, 60),
        (191_700, 60),
        (189_000, 60),
        (192_600, 60),
        (188_100, 60),
        (187_289, 60),
        (190_711, 60),
        (189_000, 60),
        (189_000, 60),
        (189_000, 60),
        (187_229, 60),
        (186_992, 60),
        (186_992, 60),
        (186_813, 60),
        (181_674, 60),
        (186_300, 60),
        (186_300, 60),
        (189_900, 60),
        (185_400, 60),
        (191_700, 60),
        (189_000, 60),
        (189_000, 60),
        (189_000, 60),
        (226_800, 73),
        (144_000, 46),
        (190_800, 60),
        (186_300, 60),
        (188_100, 60),
        (189_000, 60),
        (189_000, 60),
        (189_000, 60),
        (186_300, 60),
        (191_700, 60),
        (198_000, 60),
        (192_600, 60),
        (191_700, 60),
        (191_700, 60),
        (189_000, 60),
        (189_000, 60),
        (189_000, 60),
        (191_700, 60),
        (194_400, 60),
        (192_600, 60),
        (189_000, 60),
        (189_000, 60),
        (189_000, 60),
        (192_600, 60),
        (191_700, 60),
        (189_000, 60),
        (198_000, 60),
        (195_300, 60),
        (188_120, 60),
        (188_001, 60),
        (190_879, 60),
        (191_700, 60),
        (189_000, 60),
        (188_001, 60),
        (189_999, 60),
        (192_600, 60),
        (188_100, 60),
        (188_001, 60),
        (187_883, 60),
        (191_116, 60),
        (188_001, 60),
        (187_883, 60),
        (188_416, 60),
        (191_700, 60),
        (226_800, 69),
        (160_200, 50),
        (189_000, 60),
        (189_000, 60),
        (189_900, 60),
        (188_061, 60),
        (189_039, 60),
        (192_600, 60),
        (191_700, 60),
        (191_700, 60),
        (188_061, 60),
        (188_061, 60),
        (187_883, 60),
        (187_883, 60),
        (181_412, 60),
        (189_000, 60),
        (189_000, 60),
        (191_700, 60),
        (186_300, 60),
        (163_739, 55),
    ];

    /// A camera whose every GOP runs slow, by an amount that varies from GOP
    /// to GOP by more than the 1 % correction: the correction is often at
    /// its limit several fragments running while the error stays small, and
    /// the delivered rate stays in use. Returning to the declared rate
    /// would end every GOP in a hold of two to five frames until the next
    /// warm-up.
    #[test]
    fn a_slow_camera_whose_rate_varies_by_gop_keeps_the_delivered_rate() {
        let mut r = Regularizer::new(
            TimestampMode::Regularize,
            Some(3000),
            MediaTicks::from_millis(500),
        );
        let mut start = 0i64;
        let mut fragments = 0;
        let mut held = 0;
        // Half an hour: the five minutes six times over.
        for _ in 0..6 {
            for (span, frames) in VARYING_SLOW_CAMERA {
                let raw: Vec<i64> = (0..frames).map(|i| start + i * span / frames).collect();
                start += span;
                let a = r.assign(&raw, start);
                fragments += 1;
                assert!(!a.snapped, "fragment {fragments}");
                assert!(
                    r.error_ticks().abs() < 13_500,
                    "fragment {fragments}: error {} ticks",
                    r.error_ticks()
                );
                if fragments > 20 {
                    let nominal = r.nominal().unwrap();
                    assert!(
                        (3080..=3200).contains(&nominal),
                        "fragment {fragments}: nominal {nominal}"
                    );
                    held += i32::from(a.hold_ticks > 0);
                }
            }
        }
        // A GOP more than 1 % slower than the estimate still ends in a
        // one-frame hold: about one GOP in four.
        assert!(
            held * 100 <= fragments * 28,
            "{held} of {fragments} fragments held"
        );
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
