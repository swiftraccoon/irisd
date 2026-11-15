//! Measures of how far the clocks of a recording have parted
//! (`docs/research/14-drift.md`). Each is logged; the audio's place against
//! the video is also what the drift rule judges ([`crate::steer`]).
//!
//! A quantity is watched window by window over a run: every window gives its
//! least, its greatest, and how far the least has moved since the run's
//! first window. The least stands for a window because the noise of these
//! quantities is one-sided: a frame is stamped late and received late,
//! never early. A camera's sender reports scatter each stream's epoch over
//! about a frame, but the reports it makes of its two streams together
//! scatter together, so the streams are set against each other pair by
//! pair. The video's report of a pair is late in some and early in a few,
//! so a window of pairs stands by its lower tenth.

use iris_core::time::WallUs;

/// One closed window of a watch.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Watched {
    /// The least value of the window.
    pub(crate) least: i64,
    /// The greatest value of the window.
    pub(crate) greatest: i64,
    /// The least less that of the run's first window.
    pub(crate) moved: i64,
    /// The values the window took.
    pub(crate) samples: u32,
}

/// Watches a quantity whose noise is one-sided: a window stands by its
/// least.
#[derive(Debug)]
pub(crate) struct LeastWatch {
    /// The length of a window, in the unit of the positions.
    span: u64,
    start: u64,
    least: i64,
    greatest: i64,
    samples: u32,
    first: Option<i64>,
}

impl LeastWatch {
    /// A watch whose windows are `span` long.
    pub(crate) fn new(span: u64) -> Self {
        Self {
            span,
            start: 0,
            least: 0,
            greatest: 0,
            samples: 0,
            first: None,
        }
    }

    /// The length of a window.
    pub(crate) fn span(&self) -> u64 {
        self.span
    }

    /// Notes `value` at `position`, and returns the window when this value
    /// closes it: the one that falls `span` or more after the window's
    /// first.
    pub(crate) fn note(&mut self, position: u64, value: i64) -> Option<Watched> {
        if self.samples == 0 {
            (self.start, self.least, self.greatest) = (position, value, value);
        }
        self.least = self.least.min(value);
        self.greatest = self.greatest.max(value);
        self.samples += 1;
        if position.saturating_sub(self.start) < self.span {
            return None;
        }
        let first = *self.first.get_or_insert(self.least);
        let samples = std::mem::take(&mut self.samples);
        Some(Watched {
            least: self.least,
            greatest: self.greatest,
            moved: self.least - first,
            samples,
        })
    }
}

/// The pairs of sender reports a window needs to be read by them.
const MIN_PAIRS: usize = 8;

/// The values a window retains of each kind it reads by a quantile: its
/// pairs of sender reports and its audio frames' timestamps. A test
/// camera's pair every five seconds makes 120 in a ten-minute window
/// (`docs/research/14-drift.md` §3), and its audio 37,500 frames.
const MAX_SAMPLES: usize = 1_024;

/// A bounded sample of the values noted in a window, uniform in the order
/// they came: every one up to [`MAX_SAMPLES`], then every second, every
/// fourth, and on.
#[derive(Debug)]
struct Thinned {
    kept: Vec<i64>,
    /// The values noted, those not kept among them.
    seen: u32,
    /// One value in this many is kept.
    stride: u32,
}

impl Default for Thinned {
    fn default() -> Self {
        Self {
            kept: Vec::new(),
            seen: 0,
            stride: 1,
        }
    }
}

impl Thinned {
    fn note(&mut self, value: i64) {
        self.seen = self.seen.saturating_add(1);
        if self.kept.len() == MAX_SAMPLES {
            // The values kept are those noted at successive multiples of
            // `stride`. Keep the even multiples, then admit only that
            // stride: the sample spans the window instead of keeping only
            // its beginning or its end.
            for write in 0..MAX_SAMPLES / 2 {
                self.kept[write] = self.kept[write * 2 + 1];
            }
            self.kept.truncate(MAX_SAMPLES / 2);
            self.stride = self.stride.saturating_mul(2);
        }
        if self.seen.is_multiple_of(self.stride) {
            self.kept.push(value);
        }
    }

    /// The values kept and how many were noted; the sample starts over.
    fn take(&mut self) -> (Vec<i64>, u32) {
        let taken = std::mem::take(self);
        (taken.kept, taken.seen)
    }
}

/// Sender reports of the two streams received within this many
/// microseconds of each other were made together. A test camera makes a
/// pair every five seconds and the recorder receives the two within 0.3 s
/// (`docs/research/14-drift.md` §3).
const PAIR_REACH_US: i64 = 1_000_000;

/// How far the video's epoch is from the audio's by a window's pairs of
/// sender reports: the value a tenth of the way up them, or `None` of
/// fewer than [`MIN_PAIRS`]. On a test camera six to nine pairs in ten
/// lie within 13 ms of each other, one to four in ten have the video's
/// report later than those by up to 0.14 s, and about one in a hundred
/// has it earlier (`docs/research/14-drift.md` §3): the lower tenth is at
/// the foot of the many and above the few that were early.
fn lower_tenth(apart: &mut [i64]) -> Option<i64> {
    if apart.len() < MIN_PAIRS {
        return None;
    }
    apart.sort_unstable();
    Some(apart[apart.len() / 10])
}

/// A sender report that has not met the other stream's of its pair.
#[derive(Clone, Copy, Debug)]
struct Unpaired {
    epoch: i64,
    received: WallUs,
}

impl Unpaired {
    /// Whether a report received at `received` was made with this one.
    fn pairs_with(self, received: WallUs) -> bool {
        received.micros_since(self.received).abs() <= PAIR_REACH_US
    }
}

/// A stream's epoch by one sender report: the instant on the camera's clock,
/// in microseconds, at which the stream's timestamp is zero (the report's
/// wall time less its timestamp at `rate` units a second).
pub(crate) fn epoch_us(camera: WallUs, rtp: i64, rate: u32) -> i64 {
    let rtp_us = i128::from(rtp) * 1_000_000 / i128::from(rate.max(1));
    i64::try_from(i128::from(camera.get()) - rtp_us).unwrap_or(i64::MAX)
}

/// The 64-bit timestamp whose low 32 bits are `low` and which lies nearest
/// `reference`: a fragment keeps the low half of its first frame's
/// timestamp, and the session knows the whole of a frame near it.
pub(crate) fn near(low: u32, reference: i64) -> i64 {
    reference + i64::from(low.wrapping_sub(reference as u32).cast_signed())
}

/// The least of the values noted, and how many were.
#[derive(Clone, Copy, Debug, Default)]
struct Least {
    value: Option<i64>,
    samples: u32,
}

impl Least {
    fn note(&mut self, value: i64) {
        self.value = Some(self.value.map_or(value, |least| least.min(value)));
        self.samples = self.samples.saturating_add(1);
    }
}

/// One reading of where the sound stands against the picture.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Reading {
    /// Microseconds; positive when the recording has the sound late.
    pub(crate) late: i64,
    /// `late` less that of the run's first window that read it.
    pub(crate) moved: i64,
}

/// One closed window of a [`Place`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Placed {
    /// The sound against the picture by the camera's sender reports;
    /// `None` when the window held fewer than [`MIN_PAIRS`] pairs of them.
    pub(crate) by_reports: Option<Reading>,
    /// The sound against the picture by when the frames of each arrived.
    /// Only `moved` says anything: `late` holds how much longer the camera
    /// takes to send a picture than a sound.
    pub(crate) by_arrival: Option<Reading>,
    /// The sound against the picture by the two streams' timestamps
    /// alone: the median of the video's against its decode times less the
    /// median of the audio's against its own. Each stream's timestamps
    /// start where the camera chose, so only `moved` says anything: it
    /// falls by what a camera leaves out of the sound, on a camera whose
    /// two streams are stamped by one clock. A camera that stamps a share
    /// of its frames a tenth of a second early for hours has the least of
    /// its timestamps held by those frames and their median not
    /// (`docs/research/14-drift.md` §3).
    pub(crate) by_stamps: Option<Reading>,
    /// The audio's sender reports in the window.
    pub(crate) audio_reports: u32,
    /// The video's sender reports in the window.
    pub(crate) video_reports: u32,
    /// The pairs of sender reports in the window: one of each stream,
    /// made together.
    pub(crate) pairs: u32,
    /// The video fragments in the window.
    pub(crate) fragments: u32,
}

/// Where a run's recording has the sound against the picture, window by
/// window, read three ways (`docs/research/14-drift.md` §3, measure 5).
/// The readings by sender reports and by arrival share nothing but the
/// recording; the reading by timestamps alone is the reports' without the
/// epochs, for a camera that sends none.
///
/// By the sender reports: a report gives a stream's epoch, the instant on
/// the camera's clock at which its timestamp is zero (RFC 3550 §6.4.1).
/// A camera's reports scatter a stream's epoch over about a frame, and
/// its clock is set while it runs, by tens of milliseconds at a time; both
/// move the two streams' epochs alike in the reports it makes together.
/// So each pair of reports gives the video's epoch less the audio's, and
/// a window stands by the lower tenth of its pairs. The audio's
/// timestamps are late against its decode times by a varying time (a frame is
/// stamped as it is packed), so the least of a window stands for them; the
/// video's decode times follow its timestamps, so the median of a window's
/// fragments does. The camera's instant for the sound at a place in the
/// recording less its instant for the picture there is how early the
/// recording has the sound.
///
/// By arrival: the least time by which the frames of each stream were
/// received after their place in the recording. A drift of the sound
/// against the picture moves one least against the other whatever the
/// camera stamps or reports.
#[derive(Debug)]
pub(crate) struct Place {
    /// The length of a window in ticks of the video's timeline.
    span: u64,
    start: Option<u64>,
    /// Each stream's last sender report, until the other stream's of its
    /// pair arrives.
    audio_report: Option<Unpaired>,
    video_report: Option<Unpaired>,
    /// The video's epoch less the audio's by the pairs of the window.
    apart: Thinned,
    /// The sender reports of each stream in the window.
    audio_reports: u32,
    video_reports: u32,
    /// The audio frames' timestamps less their decode times: their least,
    /// and a sample of them for their median.
    audio_stamp: Least,
    audio_stamps: Thinned,
    /// A sample of the same of each video fragment's first frame.
    video_stamp: Thinned,
    /// The audio frames' receipt times less their decode times.
    audio_arrival: Least,
    video_arrival: Least,
    first_by_reports: Option<i64>,
    first_by_arrival: Option<i64>,
    first_by_stamps: Option<i64>,
}

impl Place {
    /// A place whose windows are `span` ticks of the video's timeline.
    pub(crate) fn new(span: u64) -> Self {
        Self {
            span,
            start: None,
            audio_report: None,
            video_report: None,
            apart: Thinned::default(),
            audio_reports: 0,
            video_reports: 0,
            audio_stamp: Least::default(),
            audio_stamps: Thinned::default(),
            video_stamp: Thinned::default(),
            audio_arrival: Least::default(),
            video_arrival: Least::default(),
            first_by_reports: None,
            first_by_arrival: None,
            first_by_stamps: None,
        }
    }

    /// The length of a window.
    pub(crate) fn span(&self) -> u64 {
        self.span
    }

    /// Notes a sender report of the audio by its epoch (the report's wall
    /// time less its timestamp, in microseconds) and by when the recorder
    /// received it.
    pub(crate) fn audio_report(&mut self, epoch_us: i64, received: WallUs) {
        self.audio_reports = self.audio_reports.saturating_add(1);
        match self.video_report.take() {
            Some(video) if video.pairs_with(received) => {
                self.apart.note(video.epoch.saturating_sub(epoch_us));
            }
            _ => {
                self.audio_report = Some(Unpaired {
                    epoch: epoch_us,
                    received,
                });
            }
        }
    }

    /// Notes a sender report of the video by its epoch and by when the
    /// recorder received it.
    pub(crate) fn video_report(&mut self, epoch_us: i64, received: WallUs) {
        self.video_reports = self.video_reports.saturating_add(1);
        match self.audio_report.take() {
            Some(audio) if audio.pairs_with(received) => {
                self.apart.note(epoch_us.saturating_sub(audio.epoch));
            }
            _ => {
                self.video_report = Some(Unpaired {
                    epoch: epoch_us,
                    received,
                });
            }
        }
    }

    /// Notes an audio frame that has its place in the recording: its
    /// timestamp and its receipt time, each less its decode time, in
    /// microseconds.
    pub(crate) fn audio_frame(&mut self, stamp_less_decode: i64, receipt_less_decode: i64) {
        self.audio_stamp.note(stamp_less_decode);
        self.audio_stamps.note(stamp_less_decode);
        self.audio_arrival.note(receipt_less_decode);
    }

    /// Notes a video frame's receipt time less its decode time.
    pub(crate) fn video_frame(&mut self, receipt_less_decode: i64) {
        self.video_arrival.note(receipt_less_decode);
    }

    /// Notes a closed video fragment by its decode time in ticks and by
    /// its first frame's timestamp less that decode time in microseconds,
    /// and returns the window when the fragment closes it: the one that
    /// starts `span` or more after the window's first.
    pub(crate) fn video_fragment(
        &mut self,
        decode_time: u64,
        stamp_less_decode: i64,
    ) -> Option<Placed> {
        let start = *self.start.get_or_insert(decode_time);
        self.video_stamp.note(stamp_less_decode);
        if decode_time.saturating_sub(start) < self.span {
            return None;
        }
        self.start = None;
        let (mut stamps, _) = self.video_stamp.take();
        stamps.sort_unstable();
        let video_stamp = stamps[stamps.len() / 2];
        let (mut apart, pairs) = self.apart.take();
        let audio_stamp = std::mem::take(&mut self.audio_stamp);
        let (mut audio_stamps, _) = self.audio_stamps.take();
        let audio_arrival = std::mem::take(&mut self.audio_arrival);
        let video_arrival = std::mem::take(&mut self.video_arrival);
        audio_stamps.sort_unstable();
        let by_stamps = audio_stamps
            .get(audio_stamps.len() / 2)
            .map(|audio_stamp| video_stamp - audio_stamp)
            .map(|late| Reading {
                late,
                moved: late - *self.first_by_stamps.get_or_insert(late),
            });
        // The camera's instant for the picture at a place in the recording
        // less its instant for the sound there.
        let by_reports = lower_tenth(&mut apart)
            .zip(audio_stamp.value)
            .map(|(epochs_apart, audio_stamp)| epochs_apart + video_stamp - audio_stamp)
            .map(|late| Reading {
                late,
                moved: late - *self.first_by_reports.get_or_insert(late),
            });
        let by_arrival = audio_arrival
            .value
            .zip(video_arrival.value)
            .map(|(audio, video)| video - audio)
            .map(|late| Reading {
                late,
                moved: late - *self.first_by_arrival.get_or_insert(late),
            });
        Some(Placed {
            by_reports,
            by_arrival,
            by_stamps,
            audio_reports: std::mem::take(&mut self.audio_reports),
            video_reports: std::mem::take(&mut self.video_reports),
            pairs,
            fragments: u32::try_from(stamps.len()).unwrap_or(u32::MAX),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_least_watch_reports_each_window_against_the_first() {
        let mut watch = LeastWatch::new(100);
        let mut seen = Vec::new();
        // Values 5..9 in the first window, 12..16 in the second, 3..7 in the third.
        for (window, base) in [5, 12, 3].into_iter().enumerate() {
            for step in 0..=10u64 {
                let position = window as u64 * 110 + step * 10;
                seen.extend(watch.note(position, base + (step as i64 * 3) % 5));
            }
        }
        let summary: Vec<(i64, i64, i64, u32)> = seen
            .iter()
            .map(|w| (w.least, w.greatest, w.moved, w.samples))
            .collect();
        assert_eq!(summary, [(5, 9, 0, 11), (12, 16, 7, 11), (3, 7, -2, 11)]);
    }

    #[test]
    fn a_window_closes_at_its_span_and_not_before() {
        let mut watch = LeastWatch::new(100);
        assert_eq!(watch.note(1_000, 4), None);
        assert_eq!(watch.note(1_099, 2), None);
        let closed = watch.note(1_100, 9).expect("the span has passed");
        assert_eq!((closed.least, closed.greatest, closed.samples), (2, 9, 3));
        // The next value opens a window of its own.
        assert_eq!(watch.note(1_101, 50), None);
        let next = watch.note(1_201, 60).expect("the span has passed");
        assert_eq!(
            (next.least, next.greatest, next.moved, next.samples),
            (50, 60, 48, 2)
        );
    }

    #[test]
    fn a_whole_timestamp_is_found_from_its_low_half() {
        assert_eq!(near(5_000, 4_000), 5_000);
        assert_eq!(near(4_000, 5_000), 4_000);
        // Past a wrap of the low half, on either side of it.
        let wrapped = (1i64 << 32) + 1_000;
        assert_eq!(near(1_000, wrapped + 90_000), wrapped);
        assert_eq!(near(u32::MAX - 99, wrapped), wrapped - 1_100);
        assert_eq!(near(2_000, (1i64 << 32) - 50), (1i64 << 32) + 2_000);
    }

    /// A camera whose clock reads `CAMERA` at the recording's start, whose
    /// streams have their own epochs, and which sends each stream's frames
    /// and reports as the test cameras do.
    mod camera {
        use super::*;

        pub(super) const WINDOW: u64 = 10 * 90_000;
        const CAMERA: i64 = 1_700_000_000_000_000;
        const AUDIO_EPOCH: i64 = CAMERA - 7_000_000_000;
        const VIDEO_EPOCH: i64 = CAMERA - 3_000_000_000;
        /// The recorder receives a sound this long after the camera took
        /// it, at least, and a picture this long after.
        const AUDIO_TRAVEL: i64 = 40_000;
        const VIDEO_TRAVEL: i64 = 95_000;

        /// What a window of a camera is like.
        #[derive(Clone, Copy)]
        pub(super) struct Kind {
            /// How late the recording has the sound, in microseconds.
            pub(super) late: i64,
            /// Pairs of sender reports.
            pub(super) reports: i64,
            /// How long after a pair's first report the recorder receives
            /// its second, in microseconds.
            pub(super) between: i64,
            /// Whether one audio frame in ten is stamped 100 ms early.
            pub(super) early_stamps: bool,
        }

        /// A window of 40 pairs of reports received 30 ms apart.
        pub(super) fn kind(late: i64) -> Kind {
            Kind {
                late,
                reports: 40,
                between: 30_000,
                early_stamps: false,
            }
        }

        /// Feeds the window that starts `start` seconds into the run and
        /// returns what its last fragment closed.
        pub(super) fn window(place: &mut Place, start: i64, kind: Kind) -> Option<Placed> {
            for report in 0..kind.reports {
                // Both reports of a pair read their epochs late by up to
                // a video frame, and in one pair of three the camera's clock
                // is 50 ms behind.
                let shared = report * 7 % 33 * 1_000 - if report % 3 == 1 { 50_000 } else { 0 };
                // The video's report is late by up to 6 ms more, in one
                // pair of five by 60 ms, and in one of fifteen 20 ms early.
                let video = match report {
                    _ if report % 15 == 14 => -20_000,
                    _ if report % 5 == 3 => 60_000,
                    _ => report % 4 * 2_000,
                };
                let received = CAMERA + (start + report) * 1_000_000;
                let audio_epoch = AUDIO_EPOCH + shared;
                let video_epoch = VIDEO_EPOCH + shared + video;
                // The audio's report comes first in every other pair.
                if report % 2 == 0 {
                    place.audio_report(audio_epoch, WallUs::new(received));
                    place.video_report(video_epoch, WallUs::new(received + kind.between));
                } else {
                    place.video_report(video_epoch, WallUs::new(received));
                    place.audio_report(audio_epoch, WallUs::new(received + kind.between));
                }
            }
            // 625 audio frames of 16 ms: the sound at a place was taken
            // `late` before the picture there, stamped up to 120 ms after
            // it was taken, and received up to 60 ms after the least.
            for frame in 0..625 {
                let decode = start * 1_000_000 + frame * 16_000;
                let taken = CAMERA + decode - kind.late;
                let early = if kind.early_stamps && frame % 10 == 3 {
                    100_000
                } else {
                    0
                };
                let stamp = taken + (frame * 13) % 120 * 1_000 - early - AUDIO_EPOCH;
                let receipt = taken + AUDIO_TRAVEL + (frame * 17) % 60 * 1_000;
                place.audio_frame(stamp - decode, receipt - decode);
            }
            // 300 video frames, a fragment every thirty: stamped as taken.
            let mut closed = None;
            for frame in 0..=300 {
                let decode = start * 1_000_000 + frame * 33_334;
                place.video_frame(
                    CAMERA + decode + VIDEO_TRAVEL + (frame * 7) % 80 * 1_000 - decode,
                );
                if frame % 30 == 0 {
                    let ticks = u64::try_from(decode * 9 / 100).expect("a place in the run");
                    // A fragment in five starts a frame under its timestamp.
                    let off = if frame % 150 == 30 { -33_334 } else { 0 };
                    let stamp = CAMERA + decode + off - VIDEO_EPOCH;
                    assert!(closed.is_none(), "a window closed before its last fragment");
                    closed = place.video_fragment(ticks, stamp - decode);
                }
            }
            closed
        }
    }

    #[test]
    fn the_sound_is_placed_against_the_picture_through_late_reports_and_stamps() {
        let mut place = Place::new(camera::WINDOW);
        let kind = camera::kind;
        let first = camera::window(&mut place, 0, kind(25_000)).expect("a closed window");
        assert_eq!(
            first.by_reports,
            Some(Reading {
                late: 25_000,
                moved: 0
            })
        );
        // By arrival the reading holds how much longer a picture travels.
        assert_eq!(
            first.by_arrival,
            Some(Reading {
                late: 25_000 + 55_000,
                moved: 0
            })
        );
        assert_eq!(
            (first.audio_reports, first.video_reports, first.pairs),
            (40, 40, 40)
        );
        assert_eq!(first.fragments, 11);
        // The sound a frame later, then early: both readings follow.
        let second = camera::window(&mut place, 11, kind(41_000)).expect("a closed window");
        assert_eq!(
            second.by_reports.map(|r| (r.late, r.moved)),
            Some((41_000, 16_000))
        );
        assert_eq!(second.by_arrival.map(|r| r.moved), Some(16_000));
        let third = camera::window(&mut place, 22, kind(-7_000)).expect("a closed window");
        assert_eq!(
            third.by_reports.map(|r| (r.late, r.moved)),
            Some((-7_000, -32_000))
        );
        assert_eq!(third.by_arrival.map(|r| r.moved), Some(-32_000));
    }

    /// The two streams' timestamps alone follow the sound from a zero of
    /// their own (the streams' epochs are four thousand seconds apart),
    /// with reports or without.
    #[test]
    fn the_timestamps_alone_read_how_far_the_sound_has_moved() {
        for reports in [40, 0] {
            let mut place = Place::new(camera::WINDOW);
            let kind = |late| camera::Kind {
                reports,
                ..camera::kind(late)
            };
            // The audio frame in the middle is stamped 59 ms after it was
            // taken.
            let first = camera::window(&mut place, 0, kind(25_000)).expect("a closed window");
            assert_eq!(
                first.by_stamps,
                Some(Reading {
                    late: 25_000 - 59_000 - 4_000_000_000,
                    moved: 0
                })
            );
            let second = camera::window(&mut place, 11, kind(-75_000)).expect("a closed window");
            assert_eq!(second.by_stamps.map(|r| r.moved), Some(-100_000));
            assert_eq!(second.by_reports.is_some(), reports > 0);
        }
    }

    /// One audio frame in ten stamped 100 ms early moves the least of the
    /// timestamps by 91 ms (the earliest of those frames is stamped 9 ms
    /// after it was taken), and with it the reading by reports. Their
    /// median, and with it the reading by timestamps alone, moves by 7 ms.
    #[test]
    fn frames_stamped_early_hold_the_least_and_not_the_median() {
        let mut place = Place::new(camera::WINDOW);
        let first = camera::window(&mut place, 0, camera::kind(25_000)).expect("a closed window");
        let early = camera::Kind {
            early_stamps: true,
            ..camera::kind(25_000)
        };
        let second = camera::window(&mut place, 11, early).expect("a closed window");
        assert_eq!(first.by_reports.map(|r| r.moved), Some(0));
        assert_eq!(second.by_reports.map(|r| r.moved), Some(91_000));
        assert_eq!(second.by_stamps.map(|r| r.moved), Some(7_000));
        assert_eq!(second.by_arrival.map(|r| r.moved), Some(0));
    }

    #[test]
    fn a_window_of_few_pairs_is_read_by_arrival_alone() {
        let mut place = Place::new(camera::WINDOW);
        let few = camera::window(
            &mut place,
            0,
            camera::Kind {
                reports: MIN_PAIRS as i64 - 1,
                ..camera::kind(5_000)
            },
        )
        .expect("a closed window");
        assert_eq!(few.by_reports, None);
        assert_eq!(few.by_arrival.map(|r| r.moved), Some(0));
        assert_eq!((few.audio_reports, few.pairs), (7, 7));
        // The first window that the reports read is the one theirs move from.
        let enough = camera::window(
            &mut place,
            11,
            camera::Kind {
                reports: MIN_PAIRS as i64,
                ..camera::kind(9_000)
            },
        )
        .expect("a closed window");
        assert_eq!(
            enough.by_reports.map(|r| (r.late, r.moved)),
            Some((9_000, 0))
        );
        assert_eq!(enough.by_arrival.map(|r| r.moved), Some(4_000));
    }

    /// Reports of the two streams are of one making when the recorder
    /// receives them within a second of each other, whichever comes first.
    #[test]
    fn reports_received_over_a_second_apart_are_not_paired() {
        let mut place = Place::new(camera::WINDOW);
        let at_reach = camera::Kind {
            between: PAIR_REACH_US,
            ..camera::kind(5_000)
        };
        let paired = camera::window(&mut place, 0, at_reach).expect("a closed window");
        assert_eq!(paired.pairs, 40);
        assert_eq!(paired.by_reports.map(|r| r.late), Some(5_000));
        // A second report that comes a microsecond later waits for the next
        // of the other stream, which is no nearer: of 40 reports of each
        // stream none meets its pair.
        let past_reach = camera::Kind {
            between: PAIR_REACH_US + 1,
            ..camera::kind(5_000)
        };
        let apart = camera::window(&mut place, 11, past_reach).expect("a closed window");
        assert_eq!(
            (apart.audio_reports, apart.video_reports, apart.pairs),
            (40, 40, 0)
        );
        assert_eq!(apart.by_reports, None);
        assert_eq!(apart.by_arrival.map(|r| r.moved), Some(0));
    }

    /// Pairs that arrive while no video fragment closes the window are
    /// kept up to the bound and then thinned: every second, every fourth,
    /// and on, so those kept stay spread over the whole window.
    #[test]
    fn reports_without_video_keep_a_bounded_sample() {
        let mut place = Place::new(90_000);
        let reports = u32::try_from(MAX_SAMPLES * 8 + 17).expect("the test count fits");
        // Pair `n`, counted from one, has the video's epoch `n` after the
        // audio's, so what is kept says which pairs were.
        for pair in 1..=reports {
            let received = i64::from(pair) * 2_000_000;
            place.audio_report(0, WallUs::new(received));
            place.video_report(i64::from(pair), WallUs::new(received + 1));
            assert!(
                place.apart.kept.len() <= MAX_SAMPLES,
                "the retained sender-report sample is bounded"
            );
            if pair as usize == MAX_SAMPLES {
                let all: Vec<i64> = (1..=i64::from(pair)).collect();
                assert_eq!(place.apart.kept, all, "every pair is kept up to the bound");
            }
        }
        assert_eq!(place.apart.seen, reports);
        // 8,209 pairs: every sixteenth, from the sixteenth to the 8,208th.
        let kept: Vec<i64> = (1..=513).map(|n| n * 16).collect();
        assert_eq!(place.apart.kept, kept);
        assert_eq!(place.apart.stride, 16);

        // A later video window reads the pairs by those kept, a tenth of
        // the way up them, and reports how many it received.
        place.audio_frame(0, 0);
        place.video_frame(0);
        assert_eq!(place.video_fragment(0, 0), None);
        let closed = place
            .video_fragment(90_000, 0)
            .expect("the video window closes");
        assert_eq!(closed.pairs, reports);
        assert_eq!(closed.by_reports.map(|reading| reading.late), Some(52 * 16));
        assert_eq!(place.apart.kept, Vec::<i64>::new());
        assert_eq!((place.apart.seen, place.apart.stride), (0, 1));
    }

    #[test]
    fn a_place_without_audio_reads_nothing() {
        let mut place = Place::new(90_000);
        place.video_report(1, WallUs::new(5));
        place.video_frame(7);
        assert_eq!(place.video_fragment(0, 3), None);
        assert_eq!(place.video_fragment(89_999, 3), None);
        let closed = place
            .video_fragment(90_000, 3)
            .expect("the span has passed");
        assert_eq!((closed.by_reports, closed.by_arrival), (None, None));
        assert_eq!(closed.by_stamps, None);
        assert_eq!(
            (closed.video_reports, closed.pairs, closed.fragments),
            (1, 0, 3)
        );
        // The next fragment opens a window of its own.
        assert_eq!(place.video_fragment(90_001, 3), None);
        assert_eq!(place.span(), 90_000);
    }
}
