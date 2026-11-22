//! The timeline of a recorded window, and the model a viewer draws and seeks
//! through. The model maps a wall-clock time to a position in the window's
//! VOD playlist and back, and finds the event nearest a time. Its geometry
//! places a time on a bar of a given width.
//!
//! A wall-clock time is a [`WallUs`] and a playlist position a
//! [`PlaylistOffset`], so the two cannot be swapped. The mapping reads the
//! playlist's `#IRIS-FRAGMENT` timing ([`PlaybackFragment`]) in served order.
//! Whole boundary GOPs count in full, and the gaps between fragments are
//! skipped. The 90 kHz ticks accumulate before each conversion to
//! microseconds, so that no per-fragment rounding drifts.
//!
//! # Example
//!
//! ```
//! use std::time::Duration;
//!
//! use iris_client::timeline_model::{PlaylistOffset, TimelineModel};
//! use iris_client::vod::PlaybackFragment;
//! use iris_core::time::{MediaTicks, WallUs};
//!
//! // A second recorded at 10 s and another at 20 s, which the playlist
//! // plays back to back.
//! let fragment = |start| PlaybackFragment {
//!     wall_start: WallUs::new(start),
//!     duration: MediaTicks::from_secs(1),
//! };
//! let model = TimelineModel {
//!     from: WallUs::new(0),
//!     to: WallUs::new(30_000_000),
//!     playback: vec![fragment(10_000_000), fragment(20_000_000)],
//!     ..TimelineModel::default()
//! };
//! let offset = model.playlist_offset(WallUs::new(20_250_000));
//! // A player seeks 1.25 s into the playlist.
//! assert_eq!(offset.get(), Duration::from_millis(1_250));
//! assert_eq!(model.wall_time(offset), WallUs::new(20_250_000));
//! // A time in the gap maps to the start of the fragment after it.
//! let gap = model.playlist_offset(WallUs::new(15_000_000));
//! assert_eq!(gap, PlaylistOffset::new(Duration::from_secs(1)));
//! // Halfway along a bar 600 pixels wide is the middle of the window.
//! assert_eq!(model.time_at_x(300.0, 600.0), WallUs::new(15_000_000));
//! ```

use std::time::Duration;
use std::{iter, mem};

use iris_core::time::{TIMESCALE, WallUs};

use crate::api::{EventRecord, Segment};
use crate::vod::PlaybackFragment;

/// Microseconds per second, for wall-clock arithmetic.
const MICROS_PER_SECOND: i64 = 1_000_000;
/// The most ticks [`TimelineModel::tick_step_us`] spaces across a window:
/// drawing them stays bounded even for an extreme requested window.
pub const MAX_TIMELINE_TICKS: i128 = 4096;

/// Where a span of wall-clock time lies on a bar of a given width, as
/// [`TimelineModel::span_extent`] gives it: the x coordinates of its start
/// and of its end, each clamped to the window. The example of
/// [`TimelineModel::span_extent`] gives one.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SpanExtent {
    /// The x coordinate of the span's start, or of the window's start for a
    /// span that starts before it.
    pub start_x: f64,
    /// The x coordinate of the span's end, or of the window's end for a span
    /// that ends after it.
    pub end_x: f64,
}

/// A position in a window's VOD playlist: the time from the playlist's start,
/// as a player counts it. The playlist holds the window's fragments back to
/// back, so an offset counts recorded media and never a gap.
/// [`TimelineModel::playlist_offset`] gives one in whole microseconds, and
/// [`TimelineModel::wall_time`] reads one in whole microseconds, rounded down.
/// The example of the [module](self) maps through two.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PlaylistOffset(Duration);

impl PlaylistOffset {
    /// Wraps the time from the playlist's start. The example of the
    /// [module](self) makes one.
    #[must_use]
    pub const fn new(offset: Duration) -> Self {
        Self(offset)
    }

    /// Returns the time from the playlist's start. The example of the
    /// [module](self) reads one.
    #[must_use]
    pub const fn get(self) -> Duration {
        self.0
    }
}

/// What a timeline draws and maps through: one camera's recordings over a
/// window, its events, the playhead, and the export marks. The example of
/// the [module](self) maps a time to a playlist position and back through
/// one; the example below draws one and finds an event at a click.
///
/// # Example
///
/// ```
/// # use std::error::Error;
/// use std::time::Duration;
///
/// use iris_client::api::{EventId, EventKind, EventRecord};
/// use iris_client::timeline_model::TimelineModel;
/// use iris_core::time::WallUs;
///
/// // An hour's window, with a motion event at its half hour.
/// let motion = EventRecord {
///     id: EventId::try_new(7)?,
///     kind: EventKind::Named("motion".into()),
///     nvr_time: WallUs::new(1_800_000_000),
///     retain_until: None,
/// };
/// let mut model = TimelineModel {
///     from: WallUs::new(0),
///     to: WallUs::new(3_600_000_000),
///     events: vec![motion.clone()],
///     ..TimelineModel::new()
/// };
/// assert_eq!(model.span_us(), 3_600_000_000);
/// // A tick every five minutes: eleven of them inside the hour.
/// assert_eq!(model.tick_step_us(), 300_000_000);
/// assert_eq!(model.tick_xs(1200.0).count(), 11);
/// // On a bar 1200 px wide the event is at 600 px. A click at 602 px names
/// // the time 6 s after it, and the event is the nearest within 10 s.
/// assert_eq!(model.time_to_x(motion.nvr_time, 1200.0), 600.0);
/// let clicked = model.time_at_x(602.0, 1200.0);
/// let nearest = model.nearest_event(clicked, Duration::from_secs(10));
/// assert_eq!(nearest.map(|event| event.id), Some(motion.id));
/// // The API answers a retain mark with the row it keeps, which takes the
/// // place of the listed one.
/// let kept = EventRecord {
///     retain_until: Some(WallUs::new(9_000_000_000)),
///     ..motion.clone()
/// };
/// assert_eq!(model.replace_event(kept), Some(motion));
/// # Ok::<(), Box<dyn Error>>(())
/// ```
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub struct TimelineModel {
    /// The window's first instant, where the bar starts.
    pub from: WallUs,
    /// The instant the window ends before, where the bar ends.
    pub to: WallUs,
    /// Recorded spans inside the window.
    pub segments: Vec<Segment>,
    /// Complete VOD fragments in served order, including boundary GOPs.
    pub playback: Vec<PlaybackFragment>,
    /// The events the timeline marks, inside the window.
    pub events: Vec<EventRecord>,
    /// The time the player shows, when it is known.
    pub playhead: Option<WallUs>,
    /// The export range's start mark, when set.
    pub mark_in: Option<WallUs>,
    /// The export range's end mark, when set.
    pub mark_out: Option<WallUs>,
}

impl TimelineModel {
    /// An empty model, the one [`Default`] makes: the window `[0, 0)`, with
    /// no segments, fragments, or events, no playhead, and no marks. The
    /// example on [`TimelineModel`] starts from one.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Maps a wall-clock time to its position in the window's VOD playlist,
    /// in whole microseconds, reading the fragments in served order. A
    /// fragment holds the times from its start up to, not including, its
    /// end, so one of no duration holds none, and a time that overlapping
    /// fragments hold maps into the first of them. A time in a gap maps to
    /// the start of the next fragment, boundary GOPs count in full, and a time
    /// after the last fragment maps to the playlist's end. The example of the
    /// [module](self) maps two.
    #[must_use]
    pub fn playlist_offset(&self, time: WallUs) -> PlaylistOffset {
        let mut ticks = 0u128;
        for fragment in &self.playback {
            let start = media_micros(ticks);
            ticks += duration_ticks(fragment);
            let end = media_micros(ticks);
            if time < fragment.wall_start {
                return PlaylistOffset::new(Duration::from_micros(start));
            }
            let within = time.get().saturating_sub(fragment.wall_start.get());
            let within = u64::try_from(within).unwrap_or(u64::MAX);
            if within < end - start {
                return PlaylistOffset::new(Duration::from_micros(start + within));
            }
        }
        PlaylistOffset::new(Duration::from_micros(media_micros(ticks)))
    }

    /// The inverse of [`TimelineModel::playlist_offset`]: the wall-clock time
    /// at a playlist position, read in whole microseconds rounded down, or the
    /// last fragment's end past the recorded span, and the window's end when
    /// no fragment is served. The example of the [module](self) maps one
    /// back.
    #[must_use]
    pub fn wall_time(&self, position: PlaylistOffset) -> WallUs {
        let offset = u64::try_from(position.get().as_micros()).unwrap_or(u64::MAX);
        let mut ticks = 0u128;
        let mut last_wall = self.to;
        for fragment in &self.playback {
            let start = media_micros(ticks);
            ticks += duration_ticks(fragment);
            let end = media_micros(ticks);
            if offset < end {
                return later(fragment.wall_start, offset.saturating_sub(start));
            }
            last_wall = later(fragment.wall_start, end - start);
        }
        last_wall
    }

    /// The event nearest to `time`, when it lies at most `within` away; of
    /// events equally near, the first listed. The example on
    /// [`TimelineModel`] finds one.
    #[must_use]
    pub fn nearest_event(&self, time: WallUs, within: Duration) -> Option<&EventRecord> {
        let distance = |event: &EventRecord| event.nvr_time.get().abs_diff(time.get());
        self.events
            .iter()
            .min_by_key(|event| distance(event))
            .filter(|event| u128::from(distance(event)) <= within.as_micros())
    }

    /// Puts `event` in place of the event with its id, as the API answers a
    /// retain mark, and returns the event it replaces. A model without one
    /// stays as it is, and the answer is `None`. The example on
    /// [`TimelineModel`] replaces one.
    pub fn replace_event(&mut self, event: EventRecord) -> Option<EventRecord> {
        let slot = self.events.iter_mut().find(|e| e.id == event.id)?;
        Some(mem::replace(slot, event))
    }

    /// The length of the window `[from, to)` in microseconds, and at least
    /// one, so that a reversed or empty window divides safely. The example on
    /// [`TimelineModel`] reads it.
    #[must_use]
    pub fn span_us(&self) -> i128 {
        (i128::from(self.to.get()) - i128::from(self.from.get())).max(1)
    }

    /// The x coordinate of `time` on a bar `width` wide over the window
    /// `[from, to)`. On a window of positive length and a positive width,
    /// `from` lies at 0 and `to` at `width`. A time before `from` then lies at
    /// or below 0, and a time after `to` at or above `width`. A time inside
    /// the window has a finite x for a finite width. The example on
    /// [`TimelineModel`] places an event with it.
    #[must_use]
    pub fn time_to_x(&self, time: WallUs, width: f64) -> f64 {
        (i128::from(time.get()) - i128::from(self.from.get())) as f64 / self.span_us() as f64
            * width
    }

    /// The drawn extent of the span of wall-clock time `[start, end)` on a
    /// bar `width` wide. It runs from the x of `start`, clamped to the
    /// window's `from`, to the x of `end`, clamped to its `to`, each where
    /// [`TimelineModel::time_to_x`] places it. A span inside the window keeps
    /// both ends. One that starts before the window starts at 0. One that
    /// ends after it ends where `to` lies, which is `width` for a window of
    /// positive length and 0 for an empty one. A span wholly outside the
    /// window gives an extent that ends at or before its start. Before the
    /// window, it runs from 0 back to where the span ends; after it, from
    /// where the span starts back to where `to` lies. How a bar of that
    /// extent is drawn, and the least width it takes, is the viewer's.
    ///
    /// # Example
    ///
    /// ```
    /// use iris_client::timeline_model::{SpanExtent, TimelineModel};
    /// use iris_core::time::WallUs;
    ///
    /// let model = TimelineModel {
    ///     from: WallUs::new(1_000),
    ///     to: WallUs::new(2_000),
    ///     ..TimelineModel::default()
    /// };
    /// // A recording from before the window into its middle.
    /// let (start, end) = (WallUs::new(500), WallUs::new(1_500));
    /// let extent = model.span_extent(start, end, 100.0);
    /// assert_eq!(
    ///     extent,
    ///     SpanExtent {
    ///         start_x: 0.0,
    ///         end_x: 50.0
    ///     }
    /// );
    /// ```
    #[must_use]
    pub fn span_extent(&self, start: WallUs, end: WallUs, width: f64) -> SpanExtent {
        SpanExtent {
            start_x: self.time_to_x(start.max(self.from), width),
            end_x: self.time_to_x(end.min(self.to), width),
        }
    }

    /// The time at the x coordinate `x` on a bar `width` wide over the window
    /// `[from, to)`: `from` at 0 and `to` at `width`, with `x` clamped to the
    /// bar. A width below one pixel counts as one. A reversed window counts as
    /// empty, so every `x` on it gives `from`. The examples of the
    /// [module](self) and on [`TimelineModel`] each turn a click into a time.
    #[must_use]
    pub fn time_at_x(&self, x: f64, width: f64) -> WallUs {
        let width = width.max(1.0);
        let span = (i128::from(self.to.get()) - i128::from(self.from.get())).max(0);
        let offset = (x.clamp(0.0, width) / width * span as f64) as i128;
        i64::try_from(i128::from(self.from.get()) + offset).map_or(self.to, WallUs::new)
    }

    /// The spacing of the drawn ticks in microseconds: five minutes up to a
    /// two-hour window, an hour beyond, widened to a whole number of those
    /// so that at most [`MAX_TIMELINE_TICKS`] are drawn. The example on
    /// [`TimelineModel`] reads it.
    #[must_use]
    pub fn tick_step_us(&self) -> i128 {
        let span = self.span_us();
        let base = if span <= i128::from(2 * 3600 * MICROS_PER_SECOND) {
            i128::from(5 * 60 * MICROS_PER_SECOND)
        } else {
            i128::from(3600 * MICROS_PER_SECOND)
        };
        let base_ticks = span / base + i128::from(span % base != 0);
        let multiple = (base_ticks / MAX_TIMELINE_TICKS
            + i128::from(base_ticks % MAX_TIMELINE_TICKS != 0))
        .max(1);
        base * multiple
    }

    /// The x coordinates of the ticks on a bar `width` wide, in order. One lies
    /// at each multiple of [`TimelineModel::tick_step_us`] after `from` and
    /// before `to`, so a window that starts on a multiple draws none at its
    /// start. Each lies where [`TimelineModel::time_to_x`] places its
    /// time, its distance from `from` taken in `i128`. A reversed or empty
    /// window has none, and no window more than [`MAX_TIMELINE_TICKS`].
    ///
    /// # Example
    ///
    /// ```
    /// use iris_client::timeline_model::TimelineModel;
    /// use iris_core::time::WallUs;
    ///
    /// // Twenty minutes from 1 s past a five-minute mark.
    /// let model = TimelineModel {
    ///     from: WallUs::new(1_000_000),
    ///     to: WallUs::new(1_201_000_000),
    ///     ..TimelineModel::default()
    /// };
    /// let marks = [300_000_000, 600_000_000, 900_000_000, 1_200_000_000];
    /// let placed = marks.map(|mark| model.time_to_x(WallUs::new(mark), 1200.0));
    /// assert!(model.tick_xs(1200.0).eq(placed));
    /// ```
    pub fn tick_xs(&self, width: f64) -> impl Iterator<Item = f64> + use<> {
        let from = i128::from(self.from.get());
        let to = i128::from(self.to.get());
        let step = self.tick_step_us();
        let span = self.span_us() as f64;
        let first = (from.div_euclid(step) + 1) * step;
        iter::successors(Some(first), move |tick| Some(tick + step))
            .take_while(move |tick| *tick < to)
            .map(move |tick| (tick - from) as f64 / span * width)
    }
}

/// A fragment's duration in 90 kHz ticks, and zero for a negative one, which
/// [`vod::playlist_fragments`](crate::vod::playlist_fragments) never returns.
fn duration_ticks(fragment: &PlaybackFragment) -> u128 {
    u128::try_from(fragment.duration.get()).unwrap_or(0)
}

/// Microseconds of media time after `ticks` 90 kHz ticks, converted once for
/// the cumulative sum so that per-fragment rounding cannot drift.
fn media_micros(ticks: u128) -> u64 {
    u64::try_from(ticks * 1_000_000 / u128::from(TIMESCALE)).unwrap_or(u64::MAX)
}

/// `time` plus `micros`, saturating at the end of the range.
fn later(time: WallUs, micros: u64) -> WallUs {
    WallUs::new(
        time.get()
            .saturating_add(i64::try_from(micros).unwrap_or(i64::MAX)),
    )
}

#[cfg(test)]
mod tests {
    use iris_core::time::MediaTicks;

    use super::*;
    use crate::api::{EventId, EventKind};

    fn at(micros: i64) -> WallUs {
        WallUs::new(micros)
    }

    fn event_id(id: i64) -> EventId {
        EventId::try_new(id).unwrap()
    }

    fn position(micros: u64) -> PlaylistOffset {
        PlaylistOffset::new(Duration::from_micros(micros))
    }

    fn segment(from: i64, to: i64) -> Segment {
        Segment {
            start: at(from),
            end: at(to),
            sealed: true,
        }
    }

    fn fragment(wall_start: i64, ticks: i64) -> PlaybackFragment {
        PlaybackFragment {
            wall_start: at(wall_start),
            duration: MediaTicks::new(ticks),
        }
    }

    fn event(id: i64, kind: EventKind, nvr_time: i64, retained: bool) -> EventRecord {
        EventRecord {
            id: event_id(id),
            kind,
            nvr_time: at(nvr_time),
            retain_until: retained.then_some(at(1)),
        }
    }

    /// Three fragments with gaps between them: 200 µs at 100, 200 µs at 400,
    /// and 100 µs at 900.
    fn gapped() -> TimelineModel {
        TimelineModel {
            from: at(100),
            to: at(1000),
            segments: vec![segment(50, 300), segment(400, 600), segment(900, 1200)],
            playback: vec![fragment(100, 18), fragment(400, 18), fragment(900, 9)],
            ..TimelineModel::default()
        }
    }

    #[test]
    fn a_model_made_by_new_is_the_default() {
        assert_eq!(TimelineModel::new(), TimelineModel::default());
    }

    #[test]
    fn playlist_offsets_skip_the_gaps_between_segments() {
        let model = gapped();
        assert_eq!(model.playlist_offset(at(100)), position(0));
        assert_eq!(model.playlist_offset(at(200)), position(100));
        assert_eq!(model.playlist_offset(at(350)), position(200));
        assert_eq!(model.playlist_offset(at(500)), position(300));
        assert_eq!(model.playlist_offset(at(950)), position(450));
    }

    #[test]
    fn wall_times_invert_the_playlist_offsets() {
        let model = gapped();
        assert_eq!(model.wall_time(position(0)), at(100));
        assert_eq!(model.wall_time(position(100)), at(200));
        assert_eq!(model.wall_time(position(200)), at(400));
        assert_eq!(model.wall_time(position(450)), at(950));
    }

    /// The window ends 1,000 µs after the last fragment, so the position maps
    /// to that fragment's end and not to the window's end.
    #[test]
    fn a_position_past_the_recording_is_the_last_fragment_end() {
        let model = TimelineModel {
            to: at(2000),
            ..gapped()
        };
        assert_eq!(model.wall_time(position(10_000)), at(1000));
    }

    /// A window that starts inside a GOP, whose whole 200 ms the playlist
    /// serves from 100,000.
    fn mid_gop() -> TimelineModel {
        TimelineModel {
            from: at(150_000),
            to: at(250_000),
            segments: vec![segment(0, 1_000_000)],
            playback: vec![fragment(100_000, 18_000)],
            ..TimelineModel::default()
        }
    }

    /// A time counts from the GOP's start, and the GOP counts in full.
    #[test]
    fn a_mid_gop_window_maps_times_through_the_whole_boundary_fragment() {
        let model = mid_gop();
        assert_eq!(model.playlist_offset(at(200_000)), position(100_000));
        assert_eq!(model.playlist_offset(at(500_000)), position(200_000));
    }

    /// The playlist starts at the GOP's start and ends at its end.
    #[test]
    fn a_mid_gop_window_maps_positions_through_the_whole_boundary_fragment() {
        let model = mid_gop();
        assert_eq!(model.wall_time(position(0)), at(100_000));
        assert_eq!(model.wall_time(position(200_000)), at(300_000));
    }

    /// Two files, of which the playlist serves two seconds at 0 and two at
    /// 3 s of the first, the gap between them inside the file, and leaves the
    /// second out.
    fn a_file_left_out() -> TimelineModel {
        TimelineModel {
            from: at(0),
            to: at(10_000_000),
            segments: vec![segment(0, 5_000_000), segment(7_000_000, 9_000_000)],
            playback: vec![fragment(0, 180_000), fragment(3_000_000, 180_000)],
            ..TimelineModel::default()
        }
    }

    #[test]
    fn playlist_offsets_skip_a_gap_inside_a_file() {
        let model = a_file_left_out();
        assert_eq!(model.playlist_offset(at(2_500_000)), position(2_000_000));
        assert_eq!(model.playlist_offset(at(3_500_000)), position(2_500_000));
    }

    #[test]
    fn a_time_in_a_file_left_out_maps_to_the_playlist_end() {
        let model = a_file_left_out();
        assert_eq!(model.playlist_offset(at(8_000_000)), position(4_000_000));
    }

    #[test]
    fn wall_times_skip_a_gap_inside_a_file() {
        let model = a_file_left_out();
        assert_eq!(model.wall_time(position(2_000_000)), at(3_000_000));
    }

    /// The playlist ends where the first file ends, at 5 s, and not in the
    /// file left out or at the end of the window.
    #[test]
    fn the_playlist_end_maps_to_the_end_of_the_last_fragment_served() {
        let model = a_file_left_out();
        assert_eq!(model.wall_time(position(4_000_000)), at(5_000_000));
    }

    /// Ninety thousand fragments of 3,001 ticks, 33,344.4 µs each, back to
    /// back: a sum of their durations in whole microseconds would fall 40 ms
    /// short of their 3,001 s.
    fn ninety_thousand_fragments() -> TimelineModel {
        let count: u32 = 90_000;
        TimelineModel {
            playback: (0..count)
                .map(|i| {
                    let wall_start = media_micros(u128::from(i) * 3001);
                    fragment(i64::try_from(wall_start).unwrap(), 3001)
                })
                .collect(),
            ..TimelineModel::default()
        }
    }

    #[test]
    fn a_playlist_offset_accumulates_ticks_without_per_fragment_drift() {
        let model = ninety_thousand_fragments();
        assert_eq!(model.playlist_offset(at(i64::MAX)), position(3_001_000_000));
    }

    #[test]
    fn a_wall_time_accumulates_ticks_without_per_fragment_drift() {
        let model = ninety_thousand_fragments();
        assert_eq!(model.wall_time(position(3_001_000_000)), at(3_001_000_000));
    }

    #[test]
    fn a_position_is_read_in_whole_microseconds_rounded_down() {
        let model = gapped();
        let at_nanos = |nanos| PlaylistOffset::new(Duration::from_nanos(nanos));
        assert_eq!(model.wall_time(at_nanos(100_999)), at(200));
        // 199.999 µs is still the first fragment's: rounding to the nearest
        // microsecond would give the second fragment's start.
        assert_eq!(model.wall_time(at_nanos(199_999)), at(299));
        assert_eq!(model.wall_time(at_nanos(200_000)), at(400));
    }

    #[test]
    fn a_fragment_spans_from_its_start_to_just_before_its_end() {
        // 200 µs from 100, then 200 µs from 250: 300, where the first ends,
        // is 50 µs into the second.
        let model = TimelineModel {
            playback: vec![fragment(100, 18), fragment(250, 18)],
            ..TimelineModel::default()
        };
        assert_eq!(model.playlist_offset(at(100)), position(0));
        assert_eq!(model.playlist_offset(at(299)), position(199));
        assert_eq!(model.playlist_offset(at(300)), position(250));
    }

    #[test]
    fn a_window_without_fragments_maps_a_time_to_offset_zero() {
        assert_eq!(window(100, 1000).playlist_offset(at(500)), position(0));
    }

    #[test]
    fn a_window_without_fragments_maps_a_position_to_its_end() {
        assert_eq!(window(100, 1000).wall_time(position(5)), at(1000));
    }

    #[test]
    fn a_fragment_of_no_duration_holds_no_time() {
        // A fragment `vod::playlist_fragments` returns has a positive
        // duration, and `api::validate_playback_fragments` refuses fragments
        // out of order. The rule holds all the same: a fragment built
        // elsewhere may have any duration, and the public fields of
        // `TimelineModel` take fragments in any order.
        // No ticks at 100, then 200 µs from 50: 100 is 50 µs into the second.
        let model = TimelineModel {
            playback: vec![fragment(100, 0), fragment(50, 18)],
            ..TimelineModel::default()
        };
        assert_eq!(model.playlist_offset(at(100)), position(50));
    }

    /// The id of the event nearest `time` within `micros` of three motion
    /// events at 1,000, 3,000, and 5,000.
    fn nearest(time: i64, micros: u64) -> Option<EventId> {
        let motion = |id, nvr_time| {
            event(
                id,
                EventKind::Named("motion".into()),
                nvr_time,
                /* retained */ false,
            )
        };
        let model = TimelineModel {
            events: vec![motion(1, 1_000), motion(2, 3_000), motion(3, 5_000)],
            ..TimelineModel::default()
        };
        model
            .nearest_event(at(time), Duration::from_micros(micros))
            .map(|event| event.id)
    }

    #[test]
    fn the_nearest_event_is_the_closest() {
        assert_eq!(nearest(2_900, 1_000), Some(event_id(2)));
    }

    /// The events at 1,000 and 3,000 both lie within 2,000 of 2,900, and the
    /// farther is listed first.
    #[test]
    fn of_two_events_within_range_the_nearer_is_the_nearest() {
        assert_eq!(nearest(2_900, 2_000), Some(event_id(2)));
    }

    /// The events at 3,000 and 5,000 are as near 4,000 as each other.
    #[test]
    fn of_events_as_near_the_first_listed_is_the_nearest() {
        assert_eq!(nearest(4_000, 1_000), Some(event_id(2)));
    }

    #[test]
    fn an_event_exactly_within_away_is_near() {
        assert_eq!(nearest(2_900, 100), Some(event_id(2)));
    }

    #[test]
    fn an_event_a_microsecond_past_within_is_not_near() {
        assert_eq!(nearest(4_000, 999), None);
    }

    /// A model of a motion event and a tamper event, and the tamper event
    /// as the API answers a retain mark on it.
    fn motion_and_tamper() -> (TimelineModel, EventRecord) {
        let motion = event(
            1,
            EventKind::Named("motion".into()),
            10,
            /* retained */ false,
        );
        let tamper = event(
            2,
            EventKind::Named("tamper".into()),
            20,
            /* retained */ false,
        );
        let kept = EventRecord {
            retain_until: Some(at(99)),
            ..tamper.clone()
        };
        let model = TimelineModel {
            events: vec![motion, tamper],
            ..TimelineModel::default()
        };
        (model, kept)
    }

    /// An event of an id that no row of `motion_and_tamper` has.
    fn unlisted() -> EventRecord {
        event(
            3,
            EventKind::Named("motion".into()),
            30,
            /* retained */ true,
        )
    }

    #[test]
    fn replacing_an_event_changes_the_row_with_its_id_alone() {
        let (mut model, kept) = motion_and_tamper();
        let motion = model.events[0].clone();
        let _ = model.replace_event(kept.clone());
        assert_eq!(model.events, [motion, kept]);
    }

    #[test]
    fn replacing_an_unlisted_event_changes_no_row() {
        let (mut model, _) = motion_and_tamper();
        let before = model.events.clone();
        let _ = model.replace_event(unlisted());
        assert_eq!(model.events, before);
    }

    /// A replacement gives back the row it replaces, the replacement itself
    /// when it is replaced in turn.
    #[test]
    fn replacing_an_event_returns_the_row_it_replaces() {
        let (mut model, kept) = motion_and_tamper();
        let tamper = model.events[1].clone();
        assert_eq!(model.replace_event(kept.clone()), Some(tamper));
        assert_eq!(model.replace_event(kept.clone()), Some(kept));
    }

    #[test]
    fn replacing_an_unlisted_event_returns_none() {
        let (mut model, _) = motion_and_tamper();
        assert_eq!(model.replace_event(unlisted()), None);
    }

    #[test]
    fn extreme_event_distances_do_not_overflow() {
        let model = TimelineModel {
            from: at(i64::MIN),
            to: at(i64::MAX),
            events: vec![event(
                1,
                EventKind::Named("motion".into()),
                i64::MIN,
                /* retained */ false,
            )],
            ..TimelineModel::default()
        };
        assert!(model.nearest_event(at(i64::MAX), Duration::MAX).is_some());
        assert!(
            model
                .nearest_event(at(i64::MAX), Duration::from_secs(u64::from(u32::MAX)))
                .is_none()
        );
        assert_eq!(
            model
                .nearest_event(at(i64::MIN), Duration::ZERO)
                .map(|event| event.id),
            Some(event_id(1))
        );
    }

    /// A model of the window `[from, to)` alone, which is all the geometry
    /// reads.
    fn window(from: i64, to: i64) -> TimelineModel {
        TimelineModel {
            from: at(from),
            to: at(to),
            ..TimelineModel::default()
        }
    }

    /// The window of the whole range of `WallUs`.
    fn widest() -> TimelineModel {
        window(i64::MIN, i64::MAX)
    }

    #[test]
    fn the_widest_windows_ends_lie_at_the_bars_ends() {
        let model = widest();
        assert_eq!(model.time_to_x(model.from, 1000.0), 0.0);
        assert_eq!(model.time_to_x(model.to, 1000.0), 1000.0);
    }

    #[test]
    fn a_click_at_either_end_of_the_widest_window_names_its_ends() {
        let model = widest();
        assert_eq!(model.time_at_x(0.0, 1000.0), model.from);
        assert_eq!(model.time_at_x(1000.0, 1000.0), model.to);
    }

    #[test]
    fn a_time_inside_the_widest_window_has_a_finite_x() {
        assert!(widest().time_to_x(at(0), 1000.0).is_finite());
    }

    #[test]
    fn the_widest_windows_tick_step_spaces_at_most_the_bound_of_ticks() {
        let model = widest();
        let step = model.tick_step_us();
        let span = model.span_us();
        let ticks = span / step + i128::from(span % step != 0);
        assert!(
            ticks <= MAX_TIMELINE_TICKS,
            "extreme window generated {ticks} ticks"
        );
    }

    #[test]
    fn a_click_on_an_empty_window_names_its_start() {
        assert_eq!(window(10, 10).time_at_x(50.0, 100.0), at(10));
    }

    #[test]
    fn a_click_on_a_reversed_window_names_its_start() {
        assert_eq!(window(10, -10).time_at_x(50.0, 100.0), at(10));
    }

    #[test]
    fn a_reversed_window_places_a_time_at_a_finite_x() {
        let x = window(10, -10).time_to_x(at(i64::MIN), 100.0);
        assert!(x.is_finite(), "{x}");
    }

    #[test]
    fn a_reversed_or_empty_window_spans_one_microsecond() {
        assert_eq!(window(10, 10).span_us(), 1);
        assert_eq!(window(10, -10).span_us(), 1);
    }

    /// The empty window spans one microsecond, so a time one microsecond
    /// after its start lies a whole bar along.
    #[test]
    fn an_empty_window_places_a_time_a_bar_along_for_each_microsecond() {
        assert_eq!(window(10, 10).time_to_x(at(11), 100.0), 100.0);
    }

    /// A bar half a pixel wide counts as one pixel wide, so a click a
    /// quarter of a pixel along is a quarter of the way along the window.
    #[test]
    fn a_width_below_one_pixel_counts_as_one() {
        assert_eq!(window(0, 1_000).time_at_x(0.25, 0.5), at(250));
    }

    /// A negative width counts as one pixel, so a click half a pixel along
    /// names the window's middle.
    #[test]
    fn a_negative_width_counts_as_one_pixel() {
        assert_eq!(window(0, 1_000).time_at_x(0.5, -3.0), at(500));
    }

    #[test]
    fn a_click_names_the_time_at_its_share_of_the_bar() {
        assert_eq!(window(1_000, 2_000).time_at_x(25.0, 100.0), at(1_250));
    }

    #[test]
    fn a_time_is_placed_at_its_share_of_the_bar() {
        assert_eq!(window(1_000, 2_000).time_to_x(at(1_250), 100.0), 25.0);
    }

    #[test]
    fn a_click_past_either_end_of_the_bar_is_clamped_to_it() {
        let model = window(1_000, 2_000);
        assert_eq!(model.time_at_x(-5.0, 100.0), at(1_000));
        assert_eq!(model.time_at_x(150.0, 100.0), at(2_000));
    }

    /// The extent of `[start, end)` on a bar 100 wide over `[1000, 2000)`.
    fn extent(start: i64, end: i64) -> SpanExtent {
        window(1_000, 2_000).span_extent(at(start), at(end), 100.0)
    }

    /// The x coordinates `start_x` to `end_x`.
    fn xs(start_x: f64, end_x: f64) -> SpanExtent {
        SpanExtent { start_x, end_x }
    }

    #[test]
    fn a_span_inside_the_window_keeps_both_ends() {
        assert_eq!(extent(1_250, 1_750), xs(25.0, 75.0));
    }

    #[test]
    fn a_span_that_starts_before_the_window_starts_at_the_bar() {
        assert_eq!(extent(500, 1_500), xs(0.0, 50.0));
    }

    #[test]
    fn a_span_that_ends_after_the_window_ends_at_the_bar_end() {
        assert_eq!(extent(1_500, 2_500), xs(50.0, 100.0));
    }

    /// A span wholly before the window runs from the bar's start back to
    /// where the span ends. One wholly after it runs from where the span
    /// starts back to the bar's end. Either extent ends at or before its
    /// start.
    #[test]
    fn a_span_outside_the_window_ends_at_or_before_its_start() {
        assert_eq!(extent(100, 200), xs(0.0, -80.0));
        assert_eq!(extent(3_000, 4_000), xs(200.0, 100.0));
        // A span that ends where the window starts runs from 0 to 0.
        assert_eq!(extent(500, 1_000), xs(0.0, 0.0));
    }

    #[test]
    fn ticks_widen_from_five_minutes_to_an_hour_past_two_hours() {
        let (five_minutes, hour) = (300_000_000, 3_600_000_000);
        let step = |micros| window(0, micros).tick_step_us();
        assert_eq!(step(0), five_minutes);
        assert_eq!(step(7_200_000_000), five_minutes);
        assert_eq!(step(7_200_000_001), hour);
        assert_eq!(step(7 * 86_400_000_000), hour);
    }

    #[test]
    fn ticks_widen_by_whole_hours_to_stay_within_the_bound() {
        let hour = 3_600_000_000;
        let bound = i64::try_from(MAX_TIMELINE_TICKS).unwrap();
        assert_eq!(window(0, bound * hour).tick_step_us(), i128::from(hour));
        assert_eq!(
            window(0, bound * hour + 1).tick_step_us(),
            i128::from(2 * hour)
        );
    }

    /// The ticks of `model` on a bar `width` wide, the first
    /// `MAX_TIMELINE_TICKS + 1` of them at most, so that a rule that draws more
    /// or never stops fails at once.
    fn ticks_drawn(model: &TimelineModel, width: f64) -> Vec<f64> {
        let bound = usize::try_from(MAX_TIMELINE_TICKS).unwrap() + 1;
        model.tick_xs(width).take(bound).collect()
    }

    /// Twenty minutes from a five-minute mark: the ticks fall at the three
    /// marks after the start, none at the start and none at the end, each at
    /// its share of the bar.
    #[test]
    fn ticks_fall_at_the_marks_strictly_inside_the_window() {
        let model = window(0, 1_200_000_000);
        assert_eq!(ticks_drawn(&model, 1200.0), [300.0, 600.0, 900.0]);
    }

    /// A window from a microsecond before a mark to a microsecond after it
    /// draws the mark's tick, at the middle of the bar.
    #[test]
    fn a_window_a_microsecond_either_side_of_a_mark_draws_its_tick() {
        let around = window(299_999_999, 300_000_001);
        assert_eq!(ticks_drawn(&around, 100.0), [50.0]);
    }

    /// A window from 650 s before zero to 550 s after it.
    fn across_zero() -> TimelineModel {
        window(-650_000_000, 550_000_000)
    }

    /// The first tick of a window that starts before zero is the first mark
    /// after its start, rounding toward negative infinity. Each tick lies
    /// where the bar places its time.
    #[test]
    fn ticks_before_zero_fall_at_the_marks_after_the_start() {
        let model = across_zero();
        let marks = [-600_000_000, -300_000_000, 0, 300_000_000];
        let placed = marks.map(|mark| model.time_to_x(at(mark), 1000.0));
        assert_eq!(ticks_drawn(&model, 1000.0), placed);
    }

    #[test]
    fn ticks_before_zero_lie_on_the_bar() {
        let ticks = ticks_drawn(&across_zero(), 1000.0);
        assert!(ticks.iter().all(|x| (0.0..1000.0).contains(x)), "{ticks:?}");
    }

    #[test]
    fn a_reversed_or_empty_window_has_no_ticks() {
        for (from, to) in [(0, 0), (300_000_000, 300_000_000), (900_000_000, 0)] {
            let ticks = ticks_drawn(&window(from, to), 100.0);
            assert!(ticks.is_empty(), "[{from}, {to}): {ticks:?}");
        }
    }

    #[test]
    fn the_widest_window_draws_at_most_the_bound_of_ticks() {
        let count = i128::try_from(ticks_drawn(&widest(), 1000.0).len()).unwrap();
        assert!(count <= MAX_TIMELINE_TICKS, "{count} ticks");
    }

    /// The widest window draws at least `MAX_TIMELINE_TICKS - 2` ticks, so
    /// its step widens no more than the bound needs.
    #[test]
    fn the_widest_windows_step_widens_no_more_than_the_bound_needs() {
        let count = i128::try_from(ticks_drawn(&widest(), 1000.0).len()).unwrap();
        assert!(count >= MAX_TIMELINE_TICKS - 2, "{count} ticks");
    }

    /// Each tick of the widest window lies after the one before it.
    #[test]
    fn the_widest_windows_ticks_are_in_order() {
        let ticks = ticks_drawn(&widest(), 1000.0);
        assert!(
            ticks.windows(2).all(|pair| pair[0] < pair[1]),
            "the ticks are in order"
        );
    }

    #[test]
    fn the_widest_windows_ticks_lie_on_the_bar() {
        let ticks = ticks_drawn(&widest(), 1000.0);
        assert!(
            ticks.iter().all(|x| (0.0..=1000.0).contains(x)),
            "the ticks lie on the bar"
        );
    }
}
