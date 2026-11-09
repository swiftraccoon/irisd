//! The timeline: a bar over a wall-clock window showing recorded coverage,
//! event markers, and the playhead, with a click choosing a time.

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

use gtk::prelude::*;
use gtk::{cairo, glib};
use iris_core::time::{TIMESCALE, WallUs};
use tracing::debug;

use crate::api::{EventRecord, Segment};
use crate::vod::PlaybackFragment;

/// Height of the drawing area in pixels.
const HEIGHT: i32 = 48;
/// Microseconds per second, for wall-clock arithmetic.
pub const MICROS_PER_SECOND: i64 = 1_000_000;
/// Drawing work remains bounded even for an extreme requested window.
const MAX_TIMELINE_TICKS: i128 = 4096;

/// What the timeline draws.
#[derive(Clone, Debug, Default)]
pub struct Model {
    /// Window start.
    pub from: WallUs,
    /// Window end.
    pub to: WallUs,
    /// Recorded spans inside the window.
    pub segments: Vec<Segment>,
    /// Complete VOD fragments in served order, including boundary GOPs.
    pub playback: Vec<PlaybackFragment>,
    /// Events inside the window.
    pub events: Vec<EventRecord>,
    /// The playhead, when known.
    pub playhead: Option<WallUs>,
    /// The export range's start mark, when set.
    pub mark_in: Option<WallUs>,
    /// The export range's end mark, when set.
    pub mark_out: Option<WallUs>,
}

impl Model {
    /// Maps a wall-clock time to its position in the actual VOD playlist.
    /// Times in a gap map to the next fragment, and boundary GOPs count in full.
    pub fn playlist_offset(&self, time: WallUs) -> gst::ClockTime {
        let mut ticks = 0u128;
        for fragment in &self.playback {
            let start = media_micros(ticks);
            ticks += duration_ticks(fragment);
            let end = media_micros(ticks);
            if time < fragment.wall_start {
                return gst::ClockTime::from_useconds(start);
            }
            let within = time.get().saturating_sub(fragment.wall_start.get());
            let within = u64::try_from(within).unwrap_or(u64::MAX);
            if within < end - start {
                return gst::ClockTime::from_useconds(start + within);
            }
        }
        gst::ClockTime::from_useconds(media_micros(ticks))
    }

    /// The inverse of [`Model::playlist_offset`]: the wall-clock time at a
    /// playlist position, or the last fragment's end past the recorded span.
    pub fn wall_time(&self, position: gst::ClockTime) -> WallUs {
        let offset = position.useconds();
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

    /// The event nearest to `time` within `within`.
    pub fn nearest_event(&self, time: WallUs, within: Duration) -> Option<&EventRecord> {
        let distance = |event: &EventRecord| event.nvr_time.get().abs_diff(time.get());
        self.events
            .iter()
            .min_by_key(|event| distance(event))
            .filter(|event| u128::from(distance(event)) <= within.as_micros())
    }

    /// The nearest event as "kind at HH:MM:SS", with "(kept)" when retained.
    pub fn nearest_event_label(&self, time: WallUs, within: Duration) -> Option<String> {
        self.nearest_event(time, within).map(|e| {
            let kept = if e.retain_until.is_some() {
                " (kept)"
            } else {
                ""
            };
            format!("{} at {}{kept}", e.kind.label(), clock_label(e.nvr_time))
        })
    }

    /// Replaces the event with the same id.
    pub fn replace_event(&mut self, event: EventRecord) {
        if let Some(slot) = self.events.iter_mut().find(|e| e.id == event.id) {
            *slot = event;
        }
    }
}

/// A fragment's duration in 90 kHz ticks; parsing keeps it positive.
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

/// The timeline widget.
#[derive(Clone, Debug)]
pub struct Timeline {
    area: gtk::DrawingArea,
    model: Rc<RefCell<Model>>,
}

impl Timeline {
    /// Builds an empty timeline; `on_seek` receives the wall-clock time of a click.
    pub fn new(on_seek: impl Fn(WallUs) + 'static) -> Self {
        let area = gtk::DrawingArea::builder()
            .content_height(HEIGHT)
            .hexpand(true)
            .build();
        let model = Rc::new(RefCell::new(Model::default()));
        area.set_draw_func({
            let model = Rc::clone(&model);
            move |_, cr, width, height| {
                draw(&model.borrow(), cr, f64::from(width), f64::from(height));
            }
        });
        let click = gtk::GestureClick::new();
        click.connect_released({
            let model = Rc::clone(&model);
            let area = area.downgrade();
            move |_, _, x, _| {
                let Some(area) = area.upgrade() else { return };
                // The borrow ends before the callback runs: the callback moves
                // the playhead, which borrows the model again.
                let time = {
                    let model = model.borrow();
                    let width = f64::from(area.width()).max(1.0);
                    time_at_x(model.from, model.to, x, width)
                };
                debug!(x, time = %clock_label(time), "timeline click");
                on_seek(time);
            }
        });
        area.add_controller(click);
        Self { area, model }
    }

    /// The widget.
    pub fn widget(&self) -> &gtk::DrawingArea {
        &self.area
    }

    /// Replaces the model and redraws.
    pub fn set_model(&self, model: Model) {
        *self.model.borrow_mut() = model;
        self.area.queue_draw();
    }

    /// Moves the playhead and redraws.
    pub fn set_playhead(&self, time: Option<WallUs>) {
        self.model.borrow_mut().playhead = time;
        self.area.queue_draw();
    }

    /// Sets the export marks and redraws.
    pub fn set_marks(&self, mark_in: Option<WallUs>, mark_out: Option<WallUs>) {
        let mut model = self.model.borrow_mut();
        model.mark_in = mark_in;
        model.mark_out = mark_out;
        drop(model);
        self.area.queue_draw();
    }

    /// Replaces the event with the same id and redraws, so a retained mark
    /// shows at once.
    pub fn replace_event(&self, event: EventRecord) {
        self.model.borrow_mut().replace_event(event);
        self.area.queue_draw();
    }

    /// Reads the model without copying it; `read` must not change the timeline.
    pub fn with_model<R>(&self, read: impl FnOnce(&Model) -> R) -> R {
        read(&self.model.borrow())
    }
}

fn span_us(from: WallUs, to: WallUs) -> i128 {
    (i128::from(to.get()) - i128::from(from.get())).max(1)
}

fn x_at_time(from: WallUs, to: WallUs, time: WallUs, width: f64) -> f64 {
    (i128::from(time.get()) - i128::from(from.get())) as f64 / span_us(from, to) as f64 * width
}

fn time_at_x(from: WallUs, to: WallUs, x: f64, width: f64) -> WallUs {
    let width = width.max(1.0);
    let span = (i128::from(to.get()) - i128::from(from.get())).max(0);
    let offset = (x.clamp(0.0, width) / width * span as f64) as i128;
    i64::try_from(i128::from(from.get()) + offset).map_or(to, WallUs::new)
}

/// The spacing of the drawn ticks: five minutes up to a two-hour window, an
/// hour beyond, widened so that at most [`MAX_TIMELINE_TICKS`] are drawn.
fn tick_step(from: WallUs, to: WallUs) -> i128 {
    let span = span_us(from, to);
    let base = if span <= i128::from(2 * 3600 * MICROS_PER_SECOND) {
        i128::from(5 * 60 * MICROS_PER_SECOND)
    } else {
        i128::from(3600 * MICROS_PER_SECOND)
    };
    let base_ticks = span / base + i128::from(span % base != 0);
    let multiple =
        (base_ticks / MAX_TIMELINE_TICKS + i128::from(base_ticks % MAX_TIMELINE_TICKS != 0)).max(1);
    base * multiple
}

fn draw(model: &Model, cr: &cairo::Context, width: f64, height: f64) {
    let x_of = |time: WallUs| x_at_time(model.from, model.to, time, width);
    cr.set_source_rgb(0.12, 0.12, 0.12);
    cr.rectangle(0.0, 0.0, width, height);
    let _ = cr.fill();
    for s in &model.segments {
        let x0 = x_of(s.start.max(model.from));
        let x1 = x_of(s.end.min(model.to));
        if s.sealed {
            cr.set_source_rgb(0.25, 0.55, 0.85);
        } else {
            cr.set_source_rgb(0.35, 0.7, 0.45);
        }
        cr.rectangle(x0, height * 0.35, (x1 - x0).max(1.0), height * 0.45);
        let _ = cr.fill();
    }
    // Events: amber marks; a retained event is a taller white mark.
    for e in &model.events {
        let x = x_of(e.nvr_time);
        if e.retain_until.is_some() {
            cr.set_source_rgb(1.0, 1.0, 1.0);
            cr.rectangle(x - 1.0, height * 0.05, 2.0, height * 0.3);
        } else {
            cr.set_source_rgb(0.95, 0.75, 0.2);
            cr.rectangle(x - 0.5, height * 0.1, 1.0, height * 0.2);
        }
        let _ = cr.fill();
    }
    // Ticks at the spacing `tick_step` picks for the window.
    cr.set_source_rgb(0.5, 0.5, 0.5);
    let step = tick_step(model.from, model.to);
    let mut tick = (i128::from(model.from.get()).div_euclid(step) + 1) * step;
    while tick < i128::from(model.to.get()) {
        let x = (tick - i128::from(model.from.get())) as f64 / span_us(model.from, model.to) as f64
            * width;
        cr.rectangle(x, height * 0.85, 1.0, height * 0.15);
        let _ = cr.fill();
        tick += step;
    }
    if let (Some(mark_in), Some(mark_out)) = (model.mark_in, model.mark_out) {
        cr.set_source_rgba(1.0, 1.0, 1.0, 0.18);
        let x0 = x_of(mark_in.max(model.from));
        let x1 = x_of(mark_out.min(model.to));
        cr.rectangle(x0, 0.0, (x1 - x0).max(1.0), height);
        let _ = cr.fill();
    }
    cr.set_source_rgb(1.0, 1.0, 1.0);
    for mark in [model.mark_in, model.mark_out].into_iter().flatten() {
        cr.rectangle(x_of(mark) - 0.5, 0.0, 1.0, height);
        let _ = cr.fill();
    }
    if let Some(playhead) = model.playhead {
        cr.set_source_rgb(1.0, 0.3, 0.3);
        cr.rectangle(x_of(playhead) - 1.0, 0.0, 2.0, height);
        let _ = cr.fill();
    }
}

/// Formats a wall-clock time as local `HH:MM:SS` for labels.
pub fn clock_label(time: WallUs) -> String {
    glib::DateTime::from_unix_local(time.get().div_euclid(MICROS_PER_SECOND))
        .and_then(|t| t.format("%H:%M:%S"))
        .map_or_else(|_| "--:--:--".into(), |s| s.to_string())
}

#[cfg(test)]
mod tests {
    use iris_core::time::MediaTicks;

    use super::*;
    use crate::api::{EventId, EventKind};

    fn at(micros: i64) -> WallUs {
        WallUs::new(micros)
    }

    fn position(micros: u64) -> gst::ClockTime {
        gst::ClockTime::from_useconds(micros)
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
            id: EventId(id),
            kind,
            nvr_time: at(nvr_time),
            retain_until: retained.then_some(at(1)),
        }
    }

    /// Three fragments with gaps between them: 200 µs at 100, 200 µs at 400,
    /// and 100 µs at 900.
    fn gapped() -> Model {
        Model {
            from: at(100),
            to: at(1000),
            segments: vec![segment(50, 300), segment(400, 600), segment(900, 1200)],
            playback: vec![fragment(100, 18), fragment(400, 18), fragment(900, 9)],
            ..Model::default()
        }
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

    #[test]
    fn a_position_past_the_recording_is_the_last_fragment_end() {
        assert_eq!(gapped().wall_time(position(10_000)), at(1000));
    }

    #[test]
    fn a_mid_gop_window_maps_to_the_complete_served_boundary_fragment() {
        let model = Model {
            from: at(150_000),
            to: at(250_000),
            segments: vec![segment(0, 1_000_000)],
            playback: vec![fragment(100_000, 18_000)],
            ..Model::default()
        };
        assert_eq!(model.wall_time(position(0)), at(100_000));
        assert_eq!(model.playlist_offset(at(200_000)), position(100_000));
        assert_eq!(model.wall_time(position(200_000)), at(300_000));
        assert_eq!(model.playlist_offset(at(500_000)), position(200_000));
    }

    #[test]
    fn fragment_timing_skips_gaps_inside_a_file_and_unavailable_files() {
        let model = Model {
            from: at(0),
            to: at(10_000_000),
            segments: vec![segment(0, 5_000_000), segment(7_000_000, 9_000_000)],
            playback: vec![fragment(0, 180_000), fragment(3_000_000, 180_000)],
            ..Model::default()
        };
        assert_eq!(model.playlist_offset(at(2_500_000)), position(2_000_000));
        assert_eq!(model.wall_time(position(2_000_000)), at(3_000_000));
        assert_eq!(model.playlist_offset(at(3_500_000)), position(2_500_000));
        assert_eq!(model.playlist_offset(at(8_000_000)), position(4_000_000));
        assert_eq!(model.wall_time(position(4_000_000)), at(5_000_000));
    }

    #[test]
    fn cumulative_tick_conversion_avoids_per_fragment_rounding_drift() {
        let count: u32 = 90_000;
        let model = Model {
            playback: (0..count)
                .map(|i| {
                    let wall_start = media_micros(u128::from(i) * 3001);
                    fragment(i64::try_from(wall_start).unwrap(), 3001)
                })
                .collect(),
            ..Model::default()
        };
        assert_eq!(model.playlist_offset(at(i64::MAX)), position(3_001_000_000));
        assert_eq!(model.wall_time(position(3_001_000_000)), at(3_001_000_000));
    }

    #[test]
    fn the_nearest_event_is_named_only_when_close_enough() {
        let model = Model {
            from: at(0),
            to: at(100 * MICROS_PER_SECOND),
            events: vec![
                event(
                    1,
                    EventKind::Named("motion".into()),
                    10 * MICROS_PER_SECOND,
                    false,
                ),
                event(
                    2,
                    EventKind::Other {
                        other: "NewFile".into(),
                    },
                    50 * MICROS_PER_SECOND,
                    true,
                ),
            ],
            ..Model::default()
        };
        let near = Duration::from_secs(5);
        assert!(
            model
                .nearest_event_label(at(12 * MICROS_PER_SECOND), near)
                .unwrap()
                .starts_with("motion at ")
        );
        let label = model
            .nearest_event_label(at(48 * MICROS_PER_SECOND), near)
            .unwrap();
        assert!(
            label.starts_with("NewFile at ") && label.ends_with(" (kept)"),
            "{label}"
        );
        assert_eq!(
            model.nearest_event_label(at(30 * MICROS_PER_SECOND), near),
            None
        );
    }

    #[test]
    fn extreme_event_distances_do_not_overflow() {
        let model = Model {
            from: at(i64::MIN),
            to: at(i64::MAX),
            events: vec![event(1, EventKind::Named("motion".into()), i64::MIN, false)],
            ..Model::default()
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
            Some(EventId(1))
        );
    }

    #[test]
    fn extreme_windows_keep_the_geometry_finite_and_the_ticks_bounded() {
        let (from, to) = (at(i64::MIN), at(i64::MAX));
        let width = 1000.0;
        assert_eq!(x_at_time(from, to, from, width), 0.0);
        assert_eq!(x_at_time(from, to, to, width), width);
        assert_eq!(time_at_x(from, to, 0.0, width), from);
        assert_eq!(time_at_x(from, to, width, width), to);
        assert!(x_at_time(from, to, at(0), width).is_finite());
        let step = tick_step(from, to);
        let ticks = span_us(from, to) / step + i128::from(span_us(from, to) % step != 0);
        assert!(
            ticks <= MAX_TIMELINE_TICKS,
            "extreme window generated {ticks} ticks"
        );
    }

    #[test]
    fn reversed_or_empty_geometry_stays_bounded() {
        assert_eq!(time_at_x(at(10), at(10), 50.0, 100.0), at(10));
        assert_eq!(time_at_x(at(10), at(-10), 50.0, 100.0), at(10));
        assert!(x_at_time(at(10), at(-10), at(i64::MIN), 100.0).is_finite());
    }
}
