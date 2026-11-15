//! The rule that takes a drift of the sound out, a frame at a time
//! (`docs/research/13-audio.md` §5.3.4).
//!
//! The rule is given each window of a run's [`Place`](crate::sync::Place)
//! and says what step is due. Where the steps are made
//! ([`Rule::making_steps`]) the place it is given next holds them. Where
//! they are not, the rule keeps what it has said and judges the place less
//! that, as if each had been made (§5.3.6).

use std::collections::VecDeque;

use crate::sync::Placed;

/// A step the other way within this many windows of the last stops the
/// rule: a drift does not turn round within hours.
const TURN_WINDOWS: u64 = 6;
/// The steps an hour may hold. The first step of a run judged by sender
/// reports is aside; every step of a run without them counts.
const STEPS_AN_HOUR: usize = 4;
/// The frames a run's first step may take at once: it takes out where the
/// run was placed and not a drift. A step of a run without sender reports
/// may take as many: it takes out sound a camera left out at once.
const FIRST_STEP_FRAMES: i64 = 3;
/// The first windows of a run without sender reports, whose median is
/// where the run started. The run is judged from the last of them on. A
/// test camera has left sound out within a run's first half hour
/// (`docs/research/13-audio.md` §5.3.4): the median of six windows of ten
/// minutes stood after that, and the median of three 21 ms from where the
/// run began.
const BASE_WINDOWS: usize = 3;
/// The windows whose median stands for such a run's reading.
const SMOOTH_WINDOWS: usize = 3;

/// Which way the recording has the sound.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Side {
    /// After its picture: frames are left out.
    Late,
    /// Before its picture: a frame is recorded twice.
    Early,
}

/// Why the rule makes no further step in a run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Stopped {
    /// A step was due the other way within [`TURN_WINDOWS`] of the last.
    TurnedRound,
    /// A fifth step was due within an hour.
    TooMany,
}

impl Stopped {
    /// The reason as a log field.
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::TurnedRound => "a step the other way was due within six windows",
            Self::TooMany => "a fifth step was due within an hour",
        }
    }
}

/// What the rule makes of a window.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Verdict {
    /// No step.
    Hold,
    /// A step of `frames` frames toward zero from an error of `error`
    /// microseconds.
    Step {
        /// The side the sound was on.
        side: Side,
        /// The frames left out or recorded twice.
        frames: u32,
        /// How late the sound was, in microseconds; negative when early.
        error: i64,
    },
    /// The rule stops for the run; said once.
    Stop(Stopped),
}

/// The value in the middle of `values`, the upper of two.
fn median(values: &mut [i64]) -> i64 {
    values.sort_unstable();
    values[values.len() / 2]
}

/// One reading of a run without sender reports, held against where the
/// run started.
#[derive(Debug, Default)]
struct Witness {
    /// The run's first readings, until there are [`BASE_WINDOWS`].
    first: Vec<i64>,
    /// Their median.
    base: Option<i64>,
    /// The last readings, up to [`SMOOTH_WINDOWS`].
    last: VecDeque<i64>,
}

impl Witness {
    /// Notes a window's reading and returns how far the median of the last
    /// [`SMOOTH_WINDOWS`] stands from the run's start, once both are known.
    fn note(&mut self, level: i64) -> Option<i64> {
        if self.base.is_none() {
            self.first.push(level);
            if self.first.len() == BASE_WINDOWS {
                self.base = Some(median(&mut self.first));
            }
        }
        if self.last.len() == SMOOTH_WINDOWS {
            self.last.pop_front();
        }
        self.last.push_back(level);
        let base = self.base?;
        (self.last.len() == SMOOTH_WINDOWS).then(|| {
            let mut last: Vec<i64> = self.last.iter().copied().collect();
            median(&mut last) - base
        })
    }

    /// Forgets the last readings: they span a step.
    fn restart(&mut self) {
        self.last.clear();
    }
}

/// The rule's state over one audio run.
#[derive(Debug)]
pub(crate) struct Rule {
    /// The length of an audio frame in microseconds.
    frame_us: i64,
    windows_an_hour: u64,
    /// Whether the run is judged by its sender reports; fixed by the run's
    /// first window. A run with them is not held to its place by arrival:
    /// that one moves by tens of milliseconds when a camera changes how it
    /// takes its picture (`docs/research/14-drift.md` §3). A run without
    /// them is judged by its place by arrival and its place by the
    /// streams' timestamps together, for the same reason.
    by_reports: Option<bool>,
    /// The two readings of a run without sender reports.
    arrival: Witness,
    stamps: Witness,
    /// The windows judged so far.
    window: u64,
    /// Whether the steps the rule names are made on the recording.
    makes: bool,
    /// What the steps said and not made would have taken from the error.
    said_us: i64,
    /// The side the error was past the threshold on in the window before.
    past: Option<Side>,
    /// Whether the window before held a step, which this one spans.
    after_step: bool,
    /// The window and the side of the step said last.
    last: Option<(u64, Side)>,
    /// The windows of the steps that count toward [`STEPS_AN_HOUR`].
    counted: VecDeque<u64>,
    stopped: bool,
}

impl Rule {
    /// A rule for frames of `frame_us` microseconds, given
    /// `windows_an_hour` windows an hour.
    pub(crate) fn new(frame_us: i64, windows_an_hour: u64) -> Self {
        Self {
            frame_us,
            windows_an_hour,
            by_reports: None,
            arrival: Witness::default(),
            stamps: Witness::default(),
            window: 0,
            makes: false,
            said_us: 0,
            past: None,
            after_step: false,
            last: None,
            counted: VecDeque::new(),
            stopped: false,
        }
    }

    /// The same rule for a recording on which every step it names is made
    /// before the next window opens: the place then holds the steps, and
    /// nothing is taken off it.
    pub(crate) fn making_steps(mut self) -> Self {
        self.makes = true;
        self
    }

    /// Judges one window.
    pub(crate) fn judge(&mut self, placed: &Placed) -> Verdict {
        self.window += 1;
        if self.stopped {
            return Verdict::Hold;
        }
        let by_reports = *self.by_reports.get_or_insert(placed.by_reports.is_some());
        let error = if by_reports {
            placed
                .by_reports
                .filter(|_| !std::mem::take(&mut self.after_step))
                .map(|reading| reading.late - self.said_us)
        } else {
            self.witnessed(placed)
        };
        let Some(error) = error else {
            self.past = None;
            return Verdict::Hold;
        };
        let side = if error * 4 > self.frame_us * 3 {
            Some(Side::Late)
        } else if error * 4 < -self.frame_us * 3 {
            Some(Side::Early)
        } else {
            None
        };
        // A run without reports is judged on a median of three windows,
        // which two windows past the threshold have moved already.
        let Some(side) = side.filter(|side| !by_reports || self.past == Some(*side)) else {
            self.past = side;
            return Verdict::Hold;
        };
        if self
            .last
            .is_some_and(|(window, last)| last != side && self.window - window <= TURN_WINDOWS)
        {
            return self.stop(Stopped::TurnedRound);
        }
        let first = by_reports && self.last.is_none();
        if !first {
            while self
                .counted
                .front()
                .is_some_and(|window| self.window - window >= self.windows_an_hour)
            {
                self.counted.pop_front();
            }
            if self.counted.len() >= STEPS_AN_HOUR {
                return self.stop(Stopped::TooMany);
            }
            self.counted.push_back(self.window);
        }
        let frames = if first || !by_reports {
            ((error.abs() + self.frame_us / 2) / self.frame_us).clamp(1, FIRST_STEP_FRAMES)
        } else {
            1
        };
        self.last = Some((self.window, side));
        if !self.makes {
            self.said_us += match side {
                Side::Late => frames * self.frame_us,
                Side::Early => -frames * self.frame_us,
            };
        }
        if by_reports {
            self.after_step = true;
        } else {
            self.arrival.restart();
            self.stamps.restart();
        }
        self.past = None;
        Verdict::Step {
            side,
            frames: u32::try_from(frames).unwrap_or(1),
            error,
        }
    }

    /// How late a run without sender reports has become, by both of its
    /// readings less what the steps said: the lesser of the two where
    /// they agree on the side, and zero where they do not. The reading
    /// by arrival alone moves when a camera changes how it takes its
    /// picture, and the reading by timestamps alone when a camera stamps
    /// a frame early.
    fn witnessed(&mut self, placed: &Placed) -> Option<i64> {
        // A window that lacks either reading enters neither median, so
        // the two stay over the same windows.
        let (arrival, stamps) = placed.by_arrival.zip(placed.by_stamps)?;
        let arrival = self.arrival.note(arrival.late);
        let stamps = self.stamps.note(stamps.late);
        let (arrival, stamps) = (arrival? - self.said_us, stamps? - self.said_us);
        if arrival.signum() != stamps.signum() {
            return Some(0);
        }
        [arrival, stamps].into_iter().min_by_key(|late| late.abs())
    }

    fn stop(&mut self, why: Stopped) -> Verdict {
        self.stopped = true;
        Verdict::Stop(why)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sync::Reading;

    const FRAME: i64 = 16_000;

    /// A window that the reports read `late` and whose reading by arrival
    /// has moved as far from the run's first as the reports' has.
    fn reported(late: i64) -> Placed {
        placed(Some(late), Some(late))
    }

    /// A window with the reports' reading, if any, and how far the reading
    /// by arrival has moved, if there is one, with the reading by
    /// timestamps moved as far. The reports' is taken to have started at
    /// zero.
    fn placed(by_reports: Option<i64>, arrival_moved: Option<i64>) -> Placed {
        Placed {
            by_reports: by_reports.map(|late| Reading { late, moved: late }),
            by_arrival: arrival_moved.map(|moved| Reading {
                late: 70_000 + moved,
                moved,
            }),
            by_stamps: arrival_moved.map(|moved| Reading {
                late: -4_000_000 + moved,
                moved,
            }),
            audio_reports: 100,
            video_reports: 100,
            pairs: 100,
            fragments: 300,
        }
    }

    fn step(side: Side, frames: u32, error: i64) -> Verdict {
        Verdict::Step {
            side,
            frames,
            error,
        }
    }

    /// The verdicts of a rule over windows the reports read.
    fn verdicts(rule: &mut Rule, lates: &[i64]) -> Vec<Verdict> {
        lates
            .iter()
            .map(|late| rule.judge(&reported(*late)))
            .collect()
    }

    #[test]
    fn a_step_takes_two_windows_past_three_quarters_of_a_frame_on_one_side() {
        let mut rule = Rule::new(FRAME, 6);
        // 12 ms is the threshold and not past it; a window back inside it
        // starts the count again.
        assert_eq!(
            verdicts(
                &mut rule,
                &[12_000, 12_001, 12_000, 12_001, -12_001, 12_001]
            ),
            [Verdict::Hold; 6]
        );
        assert_eq!(
            verdicts(&mut rule, &[12_001]),
            [step(Side::Late, 1, 12_001)]
        );
        // The same on the early side: 12 ms is not past the threshold.
        let mut rule = Rule::new(FRAME, 6);
        assert_eq!(
            verdicts(&mut rule, &[-12_000, -12_000, -12_000]),
            [Verdict::Hold; 3]
        );
        assert_eq!(
            verdicts(&mut rule, &[-12_001, -12_001]),
            [Verdict::Hold, step(Side::Early, 1, -12_001)]
        );
    }

    #[test]
    fn a_runs_first_step_takes_up_to_three_frames_and_later_ones_one() {
        for (late, frames) in [(23_999, 1), (24_000, 2), (40_000, 3), (90_000, 3)] {
            let mut rule = Rule::new(FRAME, 6);
            assert_eq!(
                verdicts(&mut rule, &[late, late]),
                [Verdict::Hold, step(Side::Late, frames, late)]
            );
        }
        // 43 ms early: three frames at once, which leaves 5 ms the other
        // way; then a drift of a frame is one frame.
        let mut rule = Rule::new(FRAME, 6);
        assert_eq!(
            verdicts(&mut rule, &[-43_000, -43_000, -43_000, -43_000, -43_000]),
            [
                Verdict::Hold,
                step(Side::Early, 3, -43_000),
                Verdict::Hold,
                Verdict::Hold,
                Verdict::Hold
            ]
        );
        assert_eq!(
            verdicts(&mut rule, &[-61_000, -61_000, -61_000]),
            [Verdict::Hold, step(Side::Early, 1, -13_000), Verdict::Hold]
        );
    }

    #[test]
    fn the_window_after_a_step_is_not_judged() {
        let mut rule = Rule::new(FRAME, 60);
        // Three frames taken of 100 ms leave 52 ms: the window after the
        // step does not count toward the next.
        assert_eq!(
            verdicts(&mut rule, &[100_000, 100_000, 100_000, 100_000, 100_000]),
            [
                Verdict::Hold,
                step(Side::Late, 3, 100_000),
                Verdict::Hold,
                Verdict::Hold,
                step(Side::Late, 1, 52_000)
            ]
        );
    }

    #[test]
    fn a_step_the_other_way_within_six_windows_stops_the_rule() {
        let mut rule = Rule::new(FRAME, 60);
        // The step takes a frame; six windows after it, a step is due the
        // other way.
        let mut lates = vec![20_000, 20_000];
        lates.extend([16_000, 16_000, 16_000, 16_000, -10_000, -10_000]);
        let seen = verdicts(&mut rule, &lates);
        assert_eq!(seen[1], step(Side::Late, 1, 20_000));
        assert_eq!(seen[2..7], [Verdict::Hold; 5]);
        assert_eq!(seen[7], Verdict::Stop(Stopped::TurnedRound));
        // Stopped is said once, and nothing follows it.
        assert_eq!(
            verdicts(&mut rule, &[90_000, 90_000, 90_000]),
            [Verdict::Hold; 3]
        );

        // Seven windows after the step it is a step like any other.
        let mut rule = Rule::new(FRAME, 60);
        let mut lates = vec![20_000, 20_000];
        lates.extend([16_000, 16_000, 16_000, 16_000, 16_000, -10_000, -10_000]);
        let seen = verdicts(&mut rule, &lates);
        assert_eq!(seen[2..8], [Verdict::Hold; 6]);
        assert_eq!(seen[8], step(Side::Early, 1, -26_000));
    }

    #[test]
    fn a_fifth_step_within_an_hour_of_four_stops_the_rule() {
        // A sound that falls a frame further behind every three windows.
        let drifting = |steps: i64| -> Vec<i64> {
            (0..steps)
                .flat_map(|step| [13_000 + step * FRAME; 3])
                .collect()
        };
        let mut rule = Rule::new(FRAME, 60);
        let seen = verdicts(&mut rule, &drifting(6));
        let steps: Vec<usize> = (0..seen.len())
            .filter(|index| matches!(seen[*index], Verdict::Step { .. }))
            .collect();
        // The run's first step and four more; the sixth is one too many.
        assert_eq!(steps, [1, 4, 7, 10, 13]);
        assert_eq!(seen[16], Verdict::Stop(Stopped::TooMany));
        // An hour of twenty windows holds the four steps as well: the
        // first of them is twelve windows before the fifth.
        let mut rule = Rule::new(FRAME, 20);
        assert_eq!(
            verdicts(&mut rule, &drifting(6))[16],
            Verdict::Stop(Stopped::TooMany)
        );

        // With an hour between the first counted step and the fifth, the
        // rule goes on: twelve windows an hour, three windows a step.
        let mut rule = Rule::new(FRAME, 12);
        let seen = verdicts(&mut rule, &drifting(8));
        assert!(
            seen.iter()
                .all(|verdict| !matches!(verdict, Verdict::Stop(_))),
            "{seen:?}"
        );
        assert_eq!(
            seen.iter()
                .filter(|verdict| matches!(verdict, Verdict::Step { .. }))
                .count(),
            8
        );
    }

    /// A camera that changes how it takes its picture moves the reading by
    /// arrival by tens of milliseconds and the reading by reports not at
    /// all: a run the reports judge goes by them alone.
    #[test]
    fn a_run_the_reports_judge_is_not_held_to_its_place_by_arrival() {
        let mut rule = Rule::new(FRAME, 60);
        let moved_by_arrival = |late| placed(Some(late), Some(-70_000));
        assert_eq!(
            [2_000, 2_000, 2_000, 2_000].map(|late| rule.judge(&moved_by_arrival(late))),
            [Verdict::Hold; 4]
        );
        assert_eq!(rule.judge(&moved_by_arrival(20_000)), Verdict::Hold);
        assert_eq!(
            rule.judge(&moved_by_arrival(20_000)),
            step(Side::Late, 1, 20_000)
        );
    }

    /// A window of a run without reports: how far each of its readings
    /// stands from where the test has the run start.
    fn witnessed(arrival: i64, stamps: i64) -> Placed {
        Placed {
            by_stamps: Some(Reading {
                late: -4_000_000 + stamps,
                moved: stamps,
            }),
            ..placed(None, Some(arrival))
        }
    }

    /// The verdicts of a rule over windows of a run without reports.
    fn unreported(rule: &mut Rule, windows: &[(i64, i64)]) -> Vec<Verdict> {
        windows
            .iter()
            .map(|(arrival, stamps)| rule.judge(&witnessed(*arrival, *stamps)))
            .collect()
    }

    /// The median is the middle value, and the upper of the two in the
    /// middle of an even count.
    #[test]
    fn the_median_is_the_middle_value_and_the_upper_of_two() {
        assert_eq!(median(&mut [7]), 7);
        assert_eq!(median(&mut [9, 1, 5]), 5);
        assert_eq!(median(&mut [5, 1, 9, 3]), 5);
        assert_eq!(median(&mut [4, 8, 6, 2, 0]), 4);
    }

    /// A run without reports is judged from its third window on, by the
    /// median of its last three windows against the median of its first
    /// three: sound left out shows in the second window after it, and is
    /// taken out three frames at once.
    #[test]
    fn a_run_without_reports_is_judged_on_medians_from_its_third_window_on() {
        let mut rule = Rule::new(FRAME, 6);
        // Far off from the first window on is where the run started.
        assert_eq!(
            unreported(&mut rule, &[(500_000, 500_000); 6]),
            [Verdict::Hold; 6]
        );
        // Sound left out in the run's second window is where it started
        // too: two of its first three windows stand after it.
        let mut rule = Rule::new(FRAME, 6);
        assert_eq!(
            unreported(
                &mut rule,
                &[(0, 0), (-100_000, -100_000), (-100_000, -100_000)]
            ),
            [Verdict::Hold; 3]
        );
        assert_eq!(
            unreported(&mut rule, &[(-100_000, -100_000); 4]),
            [Verdict::Hold; 4]
        );
        // Sound left out in the fourth window is taken out as the fifth
        // closes.
        let mut rule = Rule::new(FRAME, 6);
        assert_eq!(
            unreported(&mut rule, &[(0, 0), (0, 0), (0, 0), (-100_000, -100_000)]),
            [Verdict::Hold; 4]
        );
        assert_eq!(
            unreported(&mut rule, &[(-100_000, -100_000)]),
            [step(Side::Early, 3, -100_000)]
        );
        let mut rule = Rule::new(FRAME, 6);
        assert_eq!(unreported(&mut rule, &[(0, 0); 6]), [Verdict::Hold; 6]);
        // 0.1 s of sound left out: both readings fall and stay.
        let left_out = (-100_000, -100_000);
        assert_eq!(
            unreported(&mut rule, &[left_out; 2]),
            [Verdict::Hold, step(Side::Early, 3, -100_000)]
        );
        // The three windows after a step fill the median again; the rule
        // has said 48 ms of the 100.
        assert_eq!(
            unreported(&mut rule, &[left_out; 3]),
            [Verdict::Hold, Verdict::Hold, step(Side::Early, 3, -52_000)]
        );
        // 4 ms are left, inside the threshold.
        assert_eq!(unreported(&mut rule, &[left_out; 6]), [Verdict::Hold; 6]);
        // Reports that appear later do not change what the run is judged by.
        let with_reports = Placed {
            by_reports: Some(Reading {
                late: 90_000,
                moved: 0,
            }),
            ..witnessed(-100_000, -100_000)
        };
        assert_eq!(rule.judge(&with_reports), Verdict::Hold);
        assert_eq!(rule.judge(&with_reports), Verdict::Hold);
    }

    /// The start of a run is the median of its first three windows and its
    /// reading the median of the last three, so one window far off moves
    /// neither.
    #[test]
    fn one_window_far_off_moves_neither_the_start_nor_the_reading() {
        let mut rule = Rule::new(FRAME, 6);
        let first = [(0, 0), (90_000, 90_000), (0, 0), (0, 0), (0, 0), (0, 0)];
        assert_eq!(unreported(&mut rule, &first), [Verdict::Hold; 6]);
        assert_eq!(
            unreported(&mut rule, &[(0, 0), (-90_000, -90_000), (0, 0), (0, 0)]),
            [Verdict::Hold; 4]
        );
        // Two of three windows past the threshold are a reading past it:
        // 12 ms is the threshold and not past it.
        assert_eq!(
            unreported(&mut rule, &[(12_000, 12_000), (0, 0), (12_000, 12_000)]),
            [Verdict::Hold; 3]
        );
        assert_eq!(
            unreported(&mut rule, &[(12_001, 12_001), (0, 0), (12_001, 12_001)]),
            [Verdict::Hold, Verdict::Hold, step(Side::Late, 1, 12_001)]
        );
    }

    /// A camera that changes how it takes its picture moves the reading by
    /// arrival alone, and a frame stamped early the reading by timestamps
    /// alone: a run without reports steps only where both have moved the
    /// same way, and by the lesser of the two.
    #[test]
    fn a_run_without_reports_steps_only_where_both_readings_agree() {
        for alone in [(-60_000, 0), (0, 100_000), (-60_000, 100_000), (60_000, -1)] {
            let mut rule = Rule::new(FRAME, 6);
            assert_eq!(unreported(&mut rule, &[(0, 0); 6]), [Verdict::Hold; 6]);
            assert_eq!(
                unreported(&mut rule, &[alone; 8]),
                [Verdict::Hold; 8],
                "{alone:?}"
            );
        }
        for (both, error) in [
            ((-60_000, -20_000), -20_000_i64),
            ((30_000, 90_000), 30_000),
        ] {
            let mut rule = Rule::new(FRAME, 6);
            assert_eq!(unreported(&mut rule, &[(0, 0); 6]), [Verdict::Hold; 6]);
            let side = if error < 0 { Side::Early } else { Side::Late };
            let frames = if error.abs() == 20_000 { 1 } else { 2 };
            assert_eq!(
                unreported(&mut rule, &[both; 2]),
                [Verdict::Hold, step(side, frames, error)],
                "{both:?}"
            );
        }
    }

    /// What the rule has said comes off both readings of a run without
    /// reports: after three frames for the lesser of 70 and 100 ms, the
    /// lesser is 22 ms, whichever reading it is.
    #[test]
    fn what_was_said_comes_off_both_readings_of_a_run_without_reports() {
        for both in [(-70_000, -100_000), (-100_000, -70_000)] {
            let mut rule = Rule::new(FRAME, 6);
            assert_eq!(unreported(&mut rule, &[(0, 0); 6]), [Verdict::Hold; 6]);
            assert_eq!(
                unreported(&mut rule, &[both; 5]),
                [
                    Verdict::Hold,
                    step(Side::Early, 3, -70_000),
                    Verdict::Hold,
                    Verdict::Hold,
                    step(Side::Early, 1, -22_000)
                ],
                "{both:?}"
            );
        }
    }

    /// A window that lacks either reading of a run without reports is not
    /// judged and enters neither median: the two stay over the same
    /// windows.
    #[test]
    fn a_window_that_lacks_a_reading_enters_neither_median() {
        let mut rule = Rule::new(FRAME, 6);
        assert_eq!(unreported(&mut rule, &[(0, 0); 6]), [Verdict::Hold; 6]);
        // The timestamps fall for one window: one of its last three.
        assert_eq!(unreported(&mut rule, &[(0, -100_000)]), [Verdict::Hold]);
        // Two windows whose arrival has fallen and that lack the
        // timestamps: had they entered the arrival's median, the window
        // after them would put both medians at -100 ms.
        let lacking = Placed {
            by_stamps: None,
            ..witnessed(-100_000, -100_000)
        };
        assert_eq!(rule.judge(&lacking), Verdict::Hold);
        assert_eq!(rule.judge(&lacking), Verdict::Hold);
        assert_eq!(rule.judge(&placed(None, None)), Verdict::Hold);
        assert_eq!(
            unreported(&mut rule, &[(-100_000, -100_000); 2]),
            [Verdict::Hold, step(Side::Early, 3, -100_000)]
        );
    }

    /// Every step of a run without reports counts toward the four an hour
    /// may hold, its first among them: a fifth due within the hour stops
    /// the rule.
    #[test]
    fn a_fifth_step_of_a_run_without_reports_within_an_hour_stops_the_rule() {
        let mut rule = Rule::new(FRAME, 60);
        assert_eq!(unreported(&mut rule, &[(0, 0); 6]), [Verdict::Hold; 6]);
        // 0.4 s early and staying: three frames, then three windows to
        // fill the median again, four times over.
        let far = (-400_000, -400_000);
        let seen = unreported(&mut rule, &[far; 13]);
        let steps: Vec<usize> = seen
            .iter()
            .enumerate()
            .filter(|(_, verdict)| matches!(verdict, Verdict::Step { .. }))
            .map(|(window, _)| window)
            .collect();
        assert_eq!(steps, [1, 4, 7, 10]);
        assert_eq!(seen[11..], [Verdict::Hold; 2]);
        assert_eq!(
            unreported(&mut rule, &[far]),
            [Verdict::Stop(Stopped::TooMany)]
        );
    }

    #[test]
    fn a_window_that_the_reports_do_not_read_is_not_judged() {
        let mut rule = Rule::new(FRAME, 60);
        assert_eq!(rule.judge(&reported(20_000)), Verdict::Hold);
        assert_eq!(rule.judge(&placed(None, Some(0))), Verdict::Hold);
        assert_eq!(rule.judge(&reported(20_000)), Verdict::Hold);
        assert_eq!(rule.judge(&reported(20_000)), step(Side::Late, 1, 20_000));
    }

    /// Where the steps are made, the place the rule is given holds them and
    /// nothing is taken off it: three frames out of 100 ms leave a place
    /// that reads 52 ms, and a frame for each further reading past the
    /// threshold. A rule that only says would take what it said off these
    /// readings and name nothing after the first.
    #[test]
    fn a_rule_whose_steps_are_made_judges_the_place_as_it_reads() {
        let after_steps = [
            100_000, 100_000, 52_000, 52_000, 52_000, 36_000, 36_000, 36_000, 20_000, 20_000,
        ];
        let mut rule = Rule::new(FRAME, 60).making_steps();
        assert_eq!(
            verdicts(&mut rule, &after_steps),
            [
                Verdict::Hold,
                step(Side::Late, 3, 100_000),
                Verdict::Hold,
                Verdict::Hold,
                step(Side::Late, 1, 52_000),
                Verdict::Hold,
                Verdict::Hold,
                step(Side::Late, 1, 36_000),
                Verdict::Hold,
                Verdict::Hold,
            ]
        );
        let mut saying = Rule::new(FRAME, 60);
        let said = verdicts(&mut saying, &after_steps[..8]);
        assert_eq!(said[1], step(Side::Late, 3, 100_000));
        assert_eq!(said[2..], [Verdict::Hold; 6]);

        // A run without reports whose steps are made: 100 ms of sound left
        // out reads 52 ms after three frames, then 4 ms after three more.
        let mut rule = Rule::new(FRAME, 6).making_steps();
        assert_eq!(unreported(&mut rule, &[(0, 0); 3]), [Verdict::Hold; 3]);
        assert_eq!(
            unreported(&mut rule, &[(-100_000, -100_000); 2]),
            [Verdict::Hold, step(Side::Early, 3, -100_000)]
        );
        assert_eq!(
            unreported(&mut rule, &[(-52_000, -52_000); 3]),
            [Verdict::Hold, Verdict::Hold, step(Side::Early, 3, -52_000)]
        );
        assert_eq!(
            unreported(&mut rule, &[(-4_000, -4_000); 4]),
            [Verdict::Hold; 4]
        );
    }

    #[test]
    fn every_reason_to_stop_has_its_words() {
        assert!(Stopped::TurnedRound.as_str().contains("other way"));
        assert!(Stopped::TooMany.as_str().contains("fifth step"));
    }
}
