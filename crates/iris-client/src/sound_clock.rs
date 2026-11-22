//! Sound against picture in a live view: the rule that plays a camera's
//! audio feed and its picture against one clock
//! (`docs/research/13-audio.md` §10.4). The Linux viewer places its feeds by
//! the rule, and `docs/research/15-apple-clients.md` §4 has the planned iOS
//! and macOS viewer do the same.
//!
//! Audio frames reach a viewer in bursts, so sound played as it arrives
//! stutters, and a picture shown the instant it is decoded would lead its
//! sound by the delay that smooths it. Both feeds are therefore held the
//! same [`DELAY`]. Each keeps the least of arrival time less media time over
//! the last ten seconds, and a frame plays at its media time plus that least
//! plus the delay. Through a feed's first [`DELAY`], before any of its frames
//! has played, the mapping follows every change of its estimate. After that
//! it moves only when an estimate has left the applied offset by more than
//! 80 ms, and then for both feeds together.
//!
//! A [`SoundClock`] holds the rule for one camera. A viewer hands it each
//! fragment of either feed with the media time that
//! [`live::media_time`](fn@crate::live::media_time) reads from it and the
//! running time of its arrival, and puts in force each [`Placement`] it
//! returns. The first comes with the sound's first fragment: until then the
//! picture is not held, so a camera without sound shows its picture as it is
//! decoded.
//!
//! # Example
//!
//! ```
//! use std::time::Duration;
//!
//! use iris_client::sound_clock::{Engagement, Medium, Placement, SoundClock};
//! use iris_core::time::MediaTicks;
//!
//! let mut clock = SoundClock::new();
//! // The picture's first fragment, 100 s into its run, arrives at 7 s.
//! let picture = MediaTicks::from_secs(100);
//! let placed = clock.fragment(Medium::Video, picture, Duration::from_secs(7));
//! assert_eq!(placed, None);
//! // The sound's, on another timeline, arrives 40 ms later and puts both
//! // feeds in force, each half a second after its arrival.
//! let sound = MediaTicks::from_secs(250);
//! assert_eq!(
//!     clock.fragment(Medium::Audio, sound, Duration::from_millis(7_040)),
//!     Some(Placement {
//!         video_offset_ns: Some(7_500_000_000),
//!         audio_offset_ns: Some(7_540_000_000),
//!         engaged: Engagement::Engaged,
//!     })
//! );
//! // The sound's feed reconnects. Its next fragment, on another run's
//! // timeline, places it anew, and the mapping stays in force.
//! clock.restart(Medium::Audio);
//! let rerun = MediaTicks::from_secs(3);
//! assert_eq!(
//!     clock.fragment(Medium::Audio, rerun, Duration::from_secs(9)),
//!     Some(Placement {
//!         video_offset_ns: Some(7_500_000_000),
//!         audio_offset_ns: Some(9_500_000_000),
//!         engaged: Engagement::Unchanged,
//!     })
//! );
//! ```

use std::collections::VecDeque;
use std::time::Duration;

use iris_core::time::MediaTicks;

/// How long after its least-delay arrival a frame plays: more than the
/// longest step between audio frames seen from a camera.
pub const DELAY: Duration = Duration::from_millis(500);

/// How far an estimate moves from the applied mapping before the mapping
/// follows it.
const REANCHOR: Duration = Duration::from_millis(80);

/// The seconds of arrivals an estimate looks back over.
const WINDOW_SECONDS: i64 = 10;

/// Nanoseconds in a second.
const NANOS_PER_SECOND: i64 = 1_000_000_000;

/// One of the two feeds of a camera that play together, named as a
/// [`Placement`] names their offsets. The example of the [module](self) names
/// both.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Medium {
    /// The camera's picture, whose offset is [`Placement::video_offset_ns`].
    Video,
    /// The camera's sound, whose first fragment engages the clock; its offset
    /// is [`Placement::audio_offset_ns`].
    Audio,
}

impl Medium {
    /// The index of the feed's track in `Anchors::tracks`, the picture's first.
    fn index(self) -> usize {
        match self {
            Self::Video => 0,
            Self::Audio => 1,
        }
    }
}

/// The offsets a [`SoundClock`] puts in force, in nanoseconds of running time.
/// A demuxer starts a feed's segment at the feed's first fragment since it
/// started, so the feed's running time counts from that fragment's media
/// time. A frame then plays at its media time, less that first fragment's,
/// plus its feed's offset, on the clock the running times were read from.
/// The example of the [module](self) checks two.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Placement {
    /// The running-time offset the picture's frames play at, in nanoseconds;
    /// `None` while no fragment of the picture has placed it since it started.
    pub video_offset_ns: Option<i64>,
    /// The running-time offset the sound's frames play at, in nanoseconds;
    /// `None` while no fragment of the sound has placed it since it started.
    pub audio_offset_ns: Option<i64>,
    /// Whether the placement puts the mapping in force.
    pub engaged: Engagement,
}

/// Whether a [`Placement`] puts the mapping in force. The example of the
/// [module](self) checks both.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Engagement {
    /// The mapping was in force already and stays in force; the placement
    /// moves its offsets.
    Unchanged,
    /// The sound's first fragment: the mapping comes into force with this
    /// placement, and the picture is held from here on.
    Engaged,
}

/// The placement of one camera's picture and sound on one clock: the least
/// delay of each feed, the offsets applied, and whether they are in force,
/// which they are from the sound's first fragment on. The example of the
/// [module](self) places a camera's two feeds with one.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SoundClock {
    anchors: Anchors,
    /// Whether the mapping is in force: from the sound's first fragment.
    engaged: bool,
}

impl SoundClock {
    /// A clock that has placed nothing. The example of the [module](self)
    /// makes one.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Notes a fragment of `feed` whose media time is `media`, in its 90 kHz
    /// ticks, received at the running time `running`. Returns the offsets to
    /// put in force, which the clock then takes as applied. On the sound's
    /// first fragment it returns both feeds' offsets ([`Engagement::Engaged`]).
    /// After it, it returns both feeds' offsets whenever the mapping moves
    /// ([`Engagement::Unchanged`]). The mapping moves with every change of a
    /// feed's estimate in the feed's first [`DELAY`]. After that it moves when
    /// the estimate has left the feed's applied offset by more than 80 ms.
    /// Before the sound's first fragment the answer is `None`, however the
    /// picture's estimate moves.
    ///
    /// The media time counts in nanoseconds rounded down. A media time below
    /// zero, and a media time or a running time whose nanoseconds pass
    /// `i64::MAX`, give `None` and leave the clock as it was. The example of
    /// the [module](self) notes three fragments.
    #[must_use]
    pub fn fragment(
        &mut self,
        feed: Medium,
        media: MediaTicks,
        running: Duration,
    ) -> Option<Placement> {
        let media_ns = media_nanos(media)?;
        let running_ns = i64::try_from(running.as_nanos()).ok()?;
        let moved = self.anchors.note(feed, media_ns, running_ns);
        let engaged = if self.engaged {
            Engagement::Unchanged
        } else if feed == Medium::Audio {
            self.engaged = true;
            Engagement::Engaged
        } else {
            return None;
        };
        let [video_offset_ns, audio_offset_ns] = match engaged {
            // Engaging puts the picture's mapping in force with the sound's.
            Engagement::Engaged => [
                self.anchors.tracks[0].applied_ns,
                self.anchors.tracks[1].applied_ns,
            ],
            Engagement::Unchanged => moved?,
        };
        Some(Placement {
            video_offset_ns,
            audio_offset_ns,
            engaged,
        })
    }

    /// Starts `feed` over, as after its reconnection: its next fragment
    /// places it anew, since its demuxer starts a segment there, and its
    /// estimate starts again. The mapping stays in force. The example of the
    /// [module](self) restarts the sound.
    pub fn restart(&mut self, feed: Medium) {
        self.anchors.restart(feed);
    }
}

/// The least delay of the arrivals of one second of running time.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct SecondLeast {
    /// The second of running time, its nanoseconds divided by a second's
    /// and rounded toward negative infinity.
    second: i64,
    /// The least arrival time less media time seen in it, in nanoseconds.
    least_ns: i64,
}

/// The least of arrival time less media time over the last
/// [`WINDOW_SECONDS`], kept as one least for each second of arrivals.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct LeastDelay {
    /// The least of each second that holds an arrival, oldest first.
    seconds: VecDeque<SecondLeast>,
}

impl LeastDelay {
    /// Notes an arrival at `running_ns` with a delay of `delay_ns`, both in
    /// nanoseconds, and returns the least delay of the window that ends
    /// there, in nanoseconds.
    fn note(&mut self, running_ns: i64, delay_ns: i64) -> i64 {
        let second = running_ns.div_euclid(NANOS_PER_SECOND);
        match self.seconds.back_mut() {
            Some(last) if last.second == second => last.least_ns = last.least_ns.min(delay_ns),
            _ => self.seconds.push_back(SecondLeast {
                second,
                least_ns: delay_ns,
            }),
        }
        while self
            .seconds
            .front()
            .is_some_and(|oldest| oldest.second <= second - WINDOW_SECONDS)
        {
            self.seconds.pop_front();
        }
        self.seconds
            .iter()
            .map(|kept| kept.least_ns)
            .min()
            .unwrap_or(delay_ns)
    }
}

/// A fragment's media time and the running time of its arrival.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Arrival {
    /// The media time, in nanoseconds.
    media_ns: i64,
    /// The running time of the arrival, in nanoseconds.
    running_ns: i64,
}

/// One feed's place on the clock.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct Track {
    /// The byte stream's first fragment, where the demuxer starts its
    /// segment.
    first: Option<Arrival>,
    delays: LeastDelay,
    /// The offset the estimate asks for, in nanoseconds.
    wanted_ns: Option<i64>,
    /// The offset in force, in nanoseconds.
    applied_ns: Option<i64>,
}

/// The mapping of both feeds, without the pipeline: what a clock places
/// them by.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct Anchors {
    tracks: [Track; 2],
}

impl Anchors {
    /// A feed starts a byte stream over: its demuxer starts a segment at
    /// the next fragment, and its estimate starts with it.
    fn restart(&mut self, feed: Medium) {
        self.tracks[feed.index()] = Track::default();
    }

    /// Notes a fragment of `feed` with the media time `media_ns` arriving at
    /// the running time `running_ns`, both in nanoseconds. When the mapping
    /// moves, returns the offset of each feed in nanoseconds to put in force.
    /// The mapping moves with every change of the estimate while the feed's
    /// first frames are held: the first [`DELAY`] of the feed, while nothing
    /// of it has played. After that it moves when the feed's estimate has left
    /// what is applied by more than [`REANCHOR`].
    fn note(&mut self, feed: Medium, media_ns: i64, running_ns: i64) -> Option<[Option<i64>; 2]> {
        let track = &mut self.tracks[feed.index()];
        let first = *track.first.get_or_insert(Arrival {
            media_ns,
            running_ns,
        });
        let least_ns = track
            .delays
            .note(running_ns, running_ns.saturating_sub(media_ns));
        let settling = running_ns.saturating_sub(first.running_ns) < nanos(DELAY);
        let tolerance_ns = if settling { 0 } else { nanos(REANCHOR) };
        // A frame's running time after the demuxer is its media time less
        // the first frame's; it is to play at media time plus the least
        // delay plus the delay.
        let wanted_ns = first
            .media_ns
            .saturating_add(least_ns)
            .saturating_add(nanos(DELAY));
        track.wanted_ns = Some(wanted_ns);
        let moved = track
            .applied_ns
            .is_none_or(|applied_ns| wanted_ns.abs_diff(applied_ns) > tolerance_ns.unsigned_abs());
        if !moved {
            return None;
        }
        for track in &mut self.tracks {
            track.applied_ns = track.wanted_ns;
        }
        Some([self.tracks[0].applied_ns, self.tracks[1].applied_ns])
    }
}

fn nanos(duration: Duration) -> i64 {
    i64::try_from(duration.as_nanos()).unwrap_or(i64::MAX)
}

/// The nanoseconds of a media time in 90 kHz ticks, rounded down: none for
/// a time below zero or one whose nanoseconds pass `i64::MAX`.
fn media_nanos(media: MediaTicks) -> Option<i64> {
    let ticks = u64::try_from(media.get()).ok()?;
    i64::try_from(u128::from(ticks) * 100_000 / 9).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    const NANOS_PER_MS: i64 = 1_000_000;

    #[test]
    fn the_least_delay_is_the_least_of_the_last_ten_seconds() {
        let mut least = LeastDelay::default();
        assert_eq!(least.note(0, 50), 50);
        assert_eq!(least.note(NANOS_PER_SECOND / 2, 70), 50);
        assert_eq!(least.note(NANOS_PER_SECOND, 30), 30);
        assert_eq!(least.note(4 * NANOS_PER_SECOND, 90), 30);
        // At 10.5 s the second that held 30 is still in the window; at 11 s,
        // ten seconds after it, it has left.
        assert_eq!(
            least.note(10 * NANOS_PER_SECOND + NANOS_PER_SECOND / 2, 80),
            30
        );
        assert_eq!(least.note(11 * NANOS_PER_SECOND, 85), 80);
        assert_eq!(least.note(30 * NANOS_PER_SECOND, 5), 5);
    }

    /// Arrivals each second from 0 to 11 s, then one at 30 s: every second
    /// before it has left the window, which keeps one least.
    #[test]
    fn a_gap_of_more_than_ten_seconds_leaves_one_least() {
        let mut least = LeastDelay::default();
        for second in 0..=11 {
            least.note(second * NANOS_PER_SECOND, 1_000);
        }
        least.note(30 * NANOS_PER_SECOND, 5);
        assert_eq!(least.seconds.len(), 1);
    }

    /// The picture's first media time in the placement tests: 100 s.
    const PICTURE: i64 = 100 * NANOS_PER_SECOND;

    /// The running time at which the picture's first frame arrives: 7.04 s.
    const PICTURE_ARRIVES: i64 = 7_040 * NANOS_PER_MS;

    /// The offset of the picture's first frame: its media time, plus its
    /// arrival less its media time, plus half a second.
    const FIRST: i64 = PICTURE + (PICTURE_ARRIVES - PICTURE) + 500 * NANOS_PER_MS;

    /// Anchors that have noted the picture's first frame.
    fn picture_started() -> Anchors {
        let mut anchors = Anchors::default();
        anchors.note(Medium::Video, PICTURE, PICTURE_ARRIVES);
        anchors
    }

    /// The picture's second frame, 33 ms of media later at 7.033 s: 40 ms
    /// less late than the first.
    fn second_picture(anchors: &mut Anchors) -> Option<[Option<i64>; 2]> {
        anchors.note(
            Medium::Video,
            PICTURE + 33 * NANOS_PER_MS,
            7_033 * NANOS_PER_MS,
        )
    }

    /// The picture's third frame, half a second after the first to the
    /// nanosecond and 30 ms less late than the second.
    fn third_picture(anchors: &mut Anchors) -> Option<[Option<i64>; 2]> {
        anchors.note(
            Medium::Video,
            PICTURE + 570 * NANOS_PER_MS,
            7_540 * NANOS_PER_MS,
        )
    }

    /// A feed's frames play at media time plus the least delay plus the
    /// delay: the offset is the first media time plus those two, since the
    /// demuxer counts running time from the first frame.
    #[test]
    fn a_feed_is_placed_by_its_least_delay() {
        let mut anchors = Anchors::default();
        assert_eq!(
            anchors.note(Medium::Video, PICTURE, PICTURE_ARRIVES),
            Some([Some(FIRST), None])
        );
    }

    /// The first frames are held, so the mapping follows the estimate, 40 ms
    /// earlier.
    #[test]
    fn a_feed_follows_its_estimate_through_its_first_half_second() {
        let mut anchors = picture_started();
        assert_eq!(
            second_picture(&mut anchors),
            Some([Some(FIRST - 40 * NANOS_PER_MS), None])
        );
    }

    /// Half a second after the feed's first frame, nothing is held any more,
    /// and an estimate 30 ms earlier is inside the threshold.
    #[test]
    fn past_its_first_half_second_a_feed_moves_nothing_inside_the_threshold() {
        let mut anchors = picture_started();
        second_picture(&mut anchors);
        assert_eq!(third_picture(&mut anchors), None);
    }

    /// The sound's first media time in the placement tests: 250 s.
    const SOUND: i64 = 250 * NANOS_PER_SECOND;

    /// The running time at which the sound's first frame arrives: 8.06 s.
    const SOUND_ARRIVES: i64 = 8_060 * NANOS_PER_MS;

    /// Anchors that have noted the picture's three frames.
    fn picture_past_its_first_half_second() -> Anchors {
        let mut anchors = picture_started();
        second_picture(&mut anchors);
        third_picture(&mut anchors);
        anchors
    }

    /// The sound's first fragment places the sound; the picture takes the
    /// estimate its third frame reached, 30 ms below the offset in force.
    #[test]
    fn a_move_puts_both_feeds_estimates_in_force() {
        let mut anchors = picture_past_its_first_half_second();
        assert_eq!(
            anchors.note(Medium::Audio, SOUND, SOUND_ARRIVES),
            Some([
                Some(FIRST - 70 * NANOS_PER_MS),
                Some(SOUND + (SOUND_ARRIVES - SOUND) + 500 * NANOS_PER_MS),
            ])
        );
    }

    /// The frames of the sound's first group come together; the last of
    /// them is the least delayed, and the sound follows it at once.
    #[test]
    fn a_feeds_first_burst_follows_its_least_delayed_frame() {
        let mut anchors = picture_past_its_first_half_second();
        anchors.note(Medium::Audio, SOUND, SOUND_ARRIVES);
        let moved = anchors.note(Medium::Audio, SOUND + 64 * NANOS_PER_MS, SOUND_ARRIVES);
        assert_eq!(
            moved.map(|offsets| offsets[1]),
            Some(Some(
                SOUND + (SOUND_ARRIVES - 64 * NANOS_PER_MS - SOUND) + 500 * NANOS_PER_MS
            ))
        );
    }

    /// The media time of the connection's burst: a frame of 50 s, captured
    /// two seconds before it arrives at 2 s.
    const BURST: i64 = 50 * NANOS_PER_SECOND;

    /// Anchors that have noted the connection's burst.
    fn after_the_burst() -> Anchors {
        let mut anchors = Anchors::default();
        anchors.note(Medium::Video, BURST, 2 * NANOS_PER_SECOND);
        anchors
    }

    /// The first live frame, two seconds of media after the burst's and
    /// 5 ms after it.
    fn first_live_frame(anchors: &mut Anchors) -> Option<[Option<i64>; 2]> {
        anchors.note(
            Medium::Video,
            BURST + 2 * NANOS_PER_SECOND,
            2 * NANOS_PER_SECOND + 5 * NANOS_PER_MS,
        )
    }

    /// The answers to 199 frames after the first live one, each up to 270 ms
    /// late.
    fn late_frames(anchors: &mut Anchors) -> Vec<Option<[Option<i64>; 2]>> {
        let mut moved = Vec::new();
        for step in 1..200 {
            let at = 2 * NANOS_PER_SECOND + 5 * NANOS_PER_MS + step * 33 * NANOS_PER_MS;
            let late = (step % 10) * 30 * NANOS_PER_MS;
            moved.push(anchors.note(
                Medium::Video,
                BURST + 2 * NANOS_PER_SECOND + step * 33 * NANOS_PER_MS,
                at + late,
            ));
        }
        moved
    }

    /// A burst of earlier frames at the start puts the first estimate late,
    /// and the first live frame moves it. That frame plays half a second
    /// after it arrived, at 2.505 s: two seconds of media after the first
    /// frame, plus the offset.
    #[test]
    fn the_first_live_frame_after_a_burst_moves_the_estimate() {
        let mut anchors = after_the_burst();
        let live = first_live_frame(&mut anchors).map(|offsets| offsets[0]);
        assert_eq!(live, Some(Some(505 * NANOS_PER_MS)));
    }

    #[test]
    fn late_frames_move_nothing() {
        let mut anchors = after_the_burst();
        first_live_frame(&mut anchors);
        let moved = late_frames(&mut anchors);
        assert!(moved.iter().all(Option::is_none), "{moved:?}");
    }

    /// The camera's clock runs slow by a thousandth: every frame is a little
    /// later, and after the window has passed the estimate follows. 191 s at
    /// a thousandth is 191 ms of drift: two moves of 80 ms.
    #[test]
    fn a_slow_drift_moves_the_mapping_each_time_it_passes_the_threshold() {
        let mut anchors = after_the_burst();
        first_live_frame(&mut anchors);
        late_frames(&mut anchors);
        let mut moves = 0;
        for step in 200..6_000 {
            let media_at = BURST + 2 * NANOS_PER_SECOND + step * 33 * NANOS_PER_MS;
            let at = 2 * NANOS_PER_SECOND
                + 5 * NANOS_PER_MS
                + step * 33 * NANOS_PER_MS
                + (step - 200) * 33 * NANOS_PER_MS / 1_000;
            if anchors.note(Medium::Video, media_at, at).is_some() {
                moves += 1;
            }
        }
        assert_eq!(moves, 2);
    }

    /// A feed's first media time and later fragments may span the whole
    /// nonnegative input range. Comparing two resulting offsets stays exact
    /// even when their distance exceeds a signed integer.
    #[test]
    fn extreme_media_times_do_not_overflow_the_mapping_distance() {
        let mut anchors = Anchors::default();
        let first = i64::MAX / 2;
        assert_eq!(
            anchors.note(Medium::Video, first, 0),
            Some([Some(nanos(DELAY)), None])
        );

        // Each later arrival begins a fresh least-delay window. The first
        // moves the applied offset below zero; the second moves it above zero,
        // a move of more than `i64::MAX`.
        let below = first + (11 * NANOS_PER_SECOND - i64::MAX) + nanos(DELAY);
        assert_eq!(
            anchors.note(Medium::Video, i64::MAX, 11 * NANOS_PER_SECOND),
            Some([Some(below), None])
        );
        let above = first + 22 * NANOS_PER_SECOND + nanos(DELAY);
        assert!(above.abs_diff(below) > i64::MAX.unsigned_abs());
        assert_eq!(
            anchors.note(Medium::Video, 0, 22 * NANOS_PER_SECOND),
            Some([Some(above), None])
        );
    }

    /// A feed that starts over is placed anew by its next fragment.
    #[test]
    fn a_restarted_feed_is_placed_anew() {
        let mut anchors = Anchors::default();
        anchors.note(Medium::Audio, 10 * NANOS_PER_SECOND, NANOS_PER_SECOND);
        anchors.note(
            Medium::Audio,
            10 * NANOS_PER_SECOND + 16 * NANOS_PER_MS,
            NANOS_PER_SECOND + 16 * NANOS_PER_MS,
        );
        anchors.restart(Medium::Audio);
        // Another run: media time starts elsewhere.
        let moved = anchors.note(Medium::Audio, 3 * NANOS_PER_SECOND, 20 * NANOS_PER_SECOND);
        assert_eq!(
            moved,
            Some([
                None,
                Some(3 * NANOS_PER_SECOND + 17 * NANOS_PER_SECOND + 500 * NANOS_PER_MS)
            ])
        );
    }

    /// A hundred arrivals in one second, each less delayed than the last.
    #[test]
    fn the_arrivals_of_a_second_are_kept_as_one_least() {
        let mut least = LeastDelay::default();
        for step in 0..100 {
            least.note(step * NANOS_PER_SECOND / 100, 100 - step);
        }
        assert_eq!(least.seconds.len(), 1);
    }

    /// One arrival a second for half a minute.
    #[test]
    fn the_window_keeps_one_least_for_each_of_the_last_ten_seconds() {
        let mut least = LeastDelay::default();
        for second in 0..30 {
            least.note(second * NANOS_PER_SECOND, 1_000);
        }
        assert_eq!(least.seconds.len(), 10);
    }

    /// The media time of `millis` milliseconds, in 90 kHz ticks.
    fn media_ms(millis: i64) -> MediaTicks {
        MediaTicks::from_millis(millis)
    }

    /// The running time of `millis` milliseconds.
    fn at_ms(millis: u64) -> Duration {
        Duration::from_millis(millis)
    }

    /// The placement of the picture at `video` and the sound at `audio`
    /// nanoseconds.
    fn placed(video: Option<i64>, audio: Option<i64>, engaged: Engagement) -> Placement {
        Placement {
            video_offset_ns: video,
            audio_offset_ns: audio,
            engaged,
        }
    }

    /// The answers to the picture's fragments alone. The first is 1,000 s
    /// into its run and arrives at 7 s, which places it at 7.5 s. The second
    /// is 10 ms less late, in its first half second, at 7.49 s. The third,
    /// after its first half second, is 300 ms less late than the first: 290 ms
    /// from 7.49 s, past the threshold, at 7.2 s.
    fn picture_alone(clock: &mut SoundClock) -> [Option<Placement>; 3] {
        [
            clock.fragment(Medium::Video, media_ms(1_000_000), at_ms(7_000)),
            clock.fragment(Medium::Video, media_ms(1_000_033), at_ms(7_023)),
            clock.fragment(Medium::Video, media_ms(1_001_000), at_ms(7_700)),
        ]
    }

    /// The picture's fragments alone place nothing, though its estimate
    /// moves: with a change in its first half second, and after that past
    /// the threshold.
    #[test]
    fn the_picture_is_not_placed_until_sound_delivers() {
        assert_eq!(picture_alone(&mut SoundClock::new()), [None; 3]);
    }

    /// The sound's first fragment, 5,000 s into its run, at 7.8 s:
    /// 5,000 s + (7.8 s - 5,000 s) + 0.5 s. The picture is placed at the
    /// estimate it has reached.
    #[test]
    fn the_sounds_first_fragment_places_the_picture_at_its_reached_estimate() {
        let mut clock = SoundClock::new();
        let _ = picture_alone(&mut clock);
        assert_eq!(
            clock.fragment(Medium::Audio, media_ms(5_000_000), at_ms(7_800)),
            Some(placed(
                Some(7_200 * NANOS_PER_MS),
                Some(8_300 * NANOS_PER_MS),
                Engagement::Engaged
            ))
        );
    }

    /// A clock that has noted the picture's first two fragments, the first
    /// 20 ms more delayed than the second.
    fn picture_at_7_s() -> SoundClock {
        let mut clock = SoundClock::new();
        let _ = clock.fragment(Medium::Video, media_ms(1_000_000), at_ms(7_040));
        let _ = clock.fragment(Medium::Video, media_ms(1_000_033), at_ms(7_053));
        clock
    }

    /// The sound's first fragment puts the mapping in force. Each feed's
    /// offset is its first media time plus the least of arrival less media
    /// time plus half a second, so that a frame plays half a second after
    /// the least-delay arrival of its instant. For the picture that is
    /// 1,000 s + (7.053 s - 1,000.033 s) + 0.5 s, and for the sound it is
    /// 5,000 s + (7.1 s - 5,000 s) + 0.5 s.
    #[test]
    fn sound_engages_both_feeds_half_a_second_late() {
        assert_eq!(
            picture_at_7_s().fragment(Medium::Audio, media_ms(5_000_000), at_ms(7_100)),
            Some(placed(
                Some(7_520 * NANOS_PER_MS),
                Some(7_600 * NANOS_PER_MS),
                Engagement::Engaged
            ))
        );
    }

    /// `picture_at_7_s`, engaged by the sound's first fragment at 7.1 s.
    fn engaged_at_7_1_s() -> SoundClock {
        let mut clock = picture_at_7_s();
        let _ = clock.fragment(Medium::Audio, media_ms(5_000_000), at_ms(7_100));
        clock
    }

    /// The next audio frame came with the first: the sound's estimate falls
    /// by its 16 ms, which its first half second follows.
    #[test]
    fn after_engaging_a_move_gives_both_feeds_offsets() {
        assert_eq!(
            engaged_at_7_1_s().fragment(Medium::Audio, media_ms(5_000_016), at_ms(7_100)),
            Some(placed(
                Some(7_520 * NANOS_PER_MS),
                Some(7_584 * NANOS_PER_MS),
                Engagement::Unchanged
            ))
        );
    }

    /// A picture as late as the least moves nothing.
    #[test]
    fn after_engaging_a_fragment_that_moves_nothing_gives_none() {
        let level = engaged_at_7_1_s().fragment(Medium::Video, media_ms(1_000_066), at_ms(7_086));
        assert_eq!(level, None);
    }

    /// A clock with both feeds in force and past their first half second:
    /// each feed's first fragment arrived at 7 s and one 600 ms later as
    /// late, so both are placed at 7.5 s.
    fn settled() -> SoundClock {
        let mut clock = SoundClock::new();
        let picture = clock.fragment(Medium::Video, media_ms(1_000_000), at_ms(7_000));
        assert_eq!(picture, None);
        assert_eq!(
            clock.fragment(Medium::Audio, media_ms(5_000_000), at_ms(7_000)),
            Some(placed(
                Some(7_500 * NANOS_PER_MS),
                Some(7_500 * NANOS_PER_MS),
                Engagement::Engaged
            ))
        );
        let picture = clock.fragment(Medium::Video, media_ms(1_000_600), at_ms(7_600));
        assert_eq!(picture, None);
        let sound = clock.fragment(Medium::Audio, media_ms(5_000_600), at_ms(7_600));
        assert_eq!(sound, None);
        clock
    }

    /// After a feed's first half second, an estimate that leaves the applied
    /// offset by up to 80 ms moves nothing, 80 ms to the nanosecond included.
    #[test]
    fn an_estimate_moving_up_to_80_ms_changes_nothing() {
        let mut clock = settled();
        // The picture 30 ms less late than before, then 80 ms.
        let under = clock.fragment(Medium::Video, media_ms(1_000_700), at_ms(7_670));
        assert_eq!(under, None);
        let at = clock.fragment(Medium::Video, media_ms(1_000_800), at_ms(7_720));
        assert_eq!(at, None);
        // The sound 80 ms less late at once.
        let at = clock.fragment(Medium::Audio, media_ms(5_000_700), at_ms(7_620));
        assert_eq!(at, None);
    }

    /// After a feed's first half second, an estimate that leaves the applied
    /// offset by more than 80 ms moves the mapping, and both feeds take the
    /// estimates they have reached.
    #[test]
    fn an_estimate_moving_past_80_ms_moves_both_feeds() {
        let mut clock = settled();
        // The picture's estimate moves 30 ms, inside the threshold.
        let _ = clock.fragment(Medium::Video, media_ms(1_000_700), at_ms(7_670));
        // The sound's moves 80 ms and a nanosecond.
        let running = Duration::from_nanos(7_620_000_000 - 1);
        assert_eq!(
            clock.fragment(Medium::Audio, media_ms(5_000_700), running),
            Some(placed(
                Some(7_470 * NANOS_PER_MS),
                Some(7_420 * NANOS_PER_MS - 1),
                Engagement::Unchanged
            ))
        );
    }

    /// A feed that starts over is placed anew by its next fragment, on the
    /// timeline of its next run, and the mapping stays in force.
    #[test]
    fn a_restarted_feed_is_placed_anew_in_force() {
        let mut clock = settled();
        clock.restart(Medium::Audio);
        // Another run: its media time starts elsewhere, 3 s, at 9 s.
        assert_eq!(
            clock.fragment(Medium::Audio, media_ms(3_000), at_ms(9_000)),
            Some(placed(
                Some(7_500 * NANOS_PER_MS),
                Some(9_500 * NANOS_PER_MS),
                Engagement::Unchanged
            ))
        );
        clock.restart(Medium::Video);
        assert_eq!(
            clock.fragment(Medium::Video, media_ms(20), at_ms(9_100)),
            Some(placed(
                Some(9_600 * NANOS_PER_MS),
                Some(9_500 * NANOS_PER_MS),
                Engagement::Unchanged
            ))
        );
    }

    /// A media time counts in nanoseconds rounded down: eight ticks are
    /// 88,888.89 ns to two places, and count as 88,888.
    #[test]
    fn a_media_time_counts_in_nanoseconds_rounded_down() {
        let mut clock = SoundClock::new();
        let _ = clock.fragment(Medium::Audio, MediaTicks::new(90_000_000), at_ms(7_000));
        // Eight ticks later, in the same burst.
        assert_eq!(
            clock.fragment(Medium::Audio, MediaTicks::new(90_000_008), at_ms(7_000)),
            Some(placed(
                None,
                Some(7_500 * NANOS_PER_MS - 88_888),
                Engagement::Unchanged
            ))
        );
    }

    /// The most ticks whose nanoseconds fit `i64`: 830,103,483,316,929 ticks
    /// are 9,223,372,036,854,766,666 ns, and a tick more are
    /// 9,223,372,036,854,777,777 ns, past `i64::MAX`.
    const MOST: MediaTicks = MediaTicks::new(830_103_483_316_929);

    /// The running time of `i64::MAX` nanoseconds, the most that fits.
    fn last_running() -> Duration {
        Duration::from_nanos(i64::MAX.unsigned_abs())
    }

    /// A media time past the nanoseconds of `i64`, one below zero, and a
    /// running time past them, each with a time that fits.
    fn past_the_range() -> [(MediaTicks, Duration); 3] {
        [
            (MediaTicks::new(MOST.get() + 1), at_ms(7_000)),
            (MediaTicks::new(-1), at_ms(7_000)),
            (MediaTicks::new(0), last_running() + Duration::from_nanos(1)),
        ]
    }

    /// A clock engaged by the sound's fragment at `MOST`.
    fn engaged_at_the_most() -> SoundClock {
        let mut clock = SoundClock::new();
        let _ = clock.fragment(Medium::Audio, MOST, at_ms(7_000));
        clock
    }

    /// Before the clock is engaged and after, for either feed.
    #[test]
    fn times_past_the_range_of_nanoseconds_place_nothing() {
        for mut clock in [SoundClock::new(), engaged_at_the_most()] {
            for feed in [Medium::Video, Medium::Audio] {
                for (media, running) in past_the_range() {
                    let answer = clock.fragment(feed, media, running);
                    assert_eq!(answer, None, "{feed:?} {media:?} {running:?}");
                }
            }
        }
    }

    /// Before the clock is engaged and after, for either feed.
    #[test]
    fn times_past_the_range_of_nanoseconds_leave_the_clock_as_it_was() {
        for before in [SoundClock::new(), engaged_at_the_most()] {
            for feed in [Medium::Video, Medium::Audio] {
                for (media, running) in past_the_range() {
                    let mut clock = before.clone();
                    let _ = clock.fragment(feed, media, running);
                    assert_eq!(clock, before, "{feed:?} {media:?} {running:?}");
                }
            }
        }
    }

    #[test]
    fn the_last_media_time_whose_nanoseconds_fit_is_placed() {
        assert_eq!(
            SoundClock::new().fragment(Medium::Audio, MOST, at_ms(7_000)),
            Some(placed(
                None,
                Some(7_500 * NANOS_PER_MS),
                Engagement::Engaged
            ))
        );
    }

    /// A running time of `i64::MAX` nanoseconds is placed, at the most the
    /// offset holds.
    #[test]
    fn a_running_time_of_i64_max_nanoseconds_is_placed() {
        assert_eq!(
            engaged_at_the_most().fragment(Medium::Video, MediaTicks::new(0), last_running()),
            Some(placed(
                Some(i64::MAX),
                Some(7_500 * NANOS_PER_MS),
                Engagement::Unchanged
            ))
        );
    }
}
