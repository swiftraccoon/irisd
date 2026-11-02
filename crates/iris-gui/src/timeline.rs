//! The timeline: a bar over a wall-clock window showing recorded coverage,
//! event markers, and the playhead, with a click choosing a time.

use std::cell::RefCell;
use std::rc::Rc;

use gtk::prelude::*;
use gtk::{cairo, glib};
use tracing::debug;

use crate::api::{EventRecord, Segment};

/// Height of the drawing area in pixels.
const HEIGHT: i32 = 48;
/// Microseconds per second.
const US: i64 = 1_000_000;

/// What the timeline draws.
#[derive(Clone, Debug, Default)]
pub struct Model {
    /// Window start, Unix microseconds.
    pub from: i64,
    /// Window end, Unix microseconds.
    pub to: i64,
    /// Recorded spans inside the window.
    pub segments: Vec<Segment>,
    /// Events inside the window.
    pub events: Vec<EventRecord>,
    /// The playhead, Unix microseconds, when known.
    pub playhead: Option<i64>,
    /// The export range's start mark, when set.
    pub mark_in: Option<i64>,
    /// The export range's end mark, when set.
    pub mark_out: Option<i64>,
}

impl Model {
    /// Maps a wall-clock time to playlist time: the recorded duration between
    /// the window start and `time`, since the VOD playlist holds the segments
    /// back to back and skips the gaps between them.
    pub fn playlist_offset(&self, time: i64) -> i64 {
        self.segments
            .iter()
            .map(|s| (s.wall_end_us.min(time) - s.wall_start_us.max(self.from)).max(0))
            .sum()
    }

    /// The inverse of [`Model::playlist_offset`]: the wall-clock time at a
    /// playlist position, or the window end when the position is past the
    /// recorded span.
    pub fn wall_time(&self, offset: i64) -> i64 {
        let mut remaining = offset;
        for s in &self.segments {
            let start = s.wall_start_us.max(self.from);
            let len = (s.wall_end_us.min(self.to) - start).max(0);
            if remaining < len {
                return start + remaining;
            }
            remaining -= len;
        }
        self.to
    }
}

/// The timeline widget.
#[derive(Clone)]
pub struct Timeline {
    area: gtk::DrawingArea,
    model: Rc<RefCell<Model>>,
}

impl Timeline {
    /// Builds an empty timeline; `on_seek` receives the wall-clock time of a click.
    pub fn new(on_seek: impl Fn(i64) + 'static) -> Self {
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
            let area = area.clone();
            move |_, _, x, _| {
                // The borrow ends before the callback runs: the callback moves
                // the playhead, which borrows the model again.
                let time = {
                    let model = model.borrow();
                    let width = f64::from(area.width()).max(1.0);
                    let span = (model.to - model.from) as f64;
                    model.from + (x.clamp(0.0, width) / width * span) as i64
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
    pub fn set_playhead(&self, time: Option<i64>) {
        self.model.borrow_mut().playhead = time;
        self.area.queue_draw();
    }

    /// Sets the export marks and redraws.
    pub fn set_marks(&self, mark_in: Option<i64>, mark_out: Option<i64>) {
        let mut model = self.model.borrow_mut();
        model.mark_in = mark_in;
        model.mark_out = mark_out;
        drop(model);
        self.area.queue_draw();
    }

    /// A copy of the model.
    pub fn model(&self) -> Model {
        self.model.borrow().clone()
    }
}

impl Model {
    /// The event nearest to `time` within `within` microseconds.
    pub fn nearest_event(&self, time: i64, within: i64) -> Option<&EventRecord> {
        self.events
            .iter()
            .min_by_key(|e| (e.nvr_time - time).abs())
            .filter(|e| (e.nvr_time - time).abs() <= within)
    }

    /// The nearest event as "kind at HH:MM:SS", with "(kept)" when retained.
    pub fn nearest_event_label(&self, time: i64, within: i64) -> Option<String> {
        self.nearest_event(time, within).map(|e| {
            let kept = if e.retain_until_us.is_some() {
                " (kept)"
            } else {
                ""
            };
            format!("{} at {}{kept}", e.kind.label(), clock_label(e.nvr_time))
        })
    }

    /// Replaces the event with the same id, so a retained mark shows at once.
    pub fn replace_event(&mut self, event: EventRecord) {
        if let Some(slot) = self.events.iter_mut().find(|e| e.id == event.id) {
            *slot = event;
        }
    }
}

fn draw(model: &Model, cr: &cairo::Context, width: f64, height: f64) {
    let span = (model.to - model.from).max(1) as f64;
    let x_of = |time: i64| (time - model.from) as f64 / span * width;
    cr.set_source_rgb(0.12, 0.12, 0.12);
    cr.rectangle(0.0, 0.0, width, height);
    let _ = cr.fill();
    for s in &model.segments {
        let x0 = x_of(s.wall_start_us.max(model.from));
        let x1 = x_of(s.wall_end_us.min(model.to));
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
        if e.retain_until_us.is_some() {
            cr.set_source_rgb(1.0, 1.0, 1.0);
            cr.rectangle(x - 1.0, height * 0.05, 2.0, height * 0.3);
        } else {
            cr.set_source_rgb(0.95, 0.75, 0.2);
            cr.rectangle(x - 0.5, height * 0.1, 1.0, height * 0.2);
        }
        let _ = cr.fill();
    }
    // Minute ticks when the window is short enough to show them, else hours.
    let step = if model.to - model.from <= 2 * 3600 * US {
        5 * 60 * US
    } else {
        3600 * US
    };
    cr.set_source_rgb(0.5, 0.5, 0.5);
    let mut tick = (model.from / step + 1) * step;
    while tick < model.to {
        let x = x_of(tick);
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

/// Formats a Unix-microsecond time as local `HH:MM:SS` for labels.
pub fn clock_label(time: i64) -> String {
    glib::DateTime::from_unix_local(time / US)
        .and_then(|t| t.format("%H:%M:%S"))
        .map_or_else(|_| "--:--:--".into(), |s| s.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn segment(from: i64, to: i64) -> Segment {
        Segment {
            wall_start_us: from,
            wall_end_us: to,
            sealed: true,
        }
    }

    #[test]
    fn playlist_offsets_skip_the_gaps_between_segments() {
        let model = Model {
            from: 100,
            to: 1000,
            segments: vec![segment(50, 300), segment(400, 600), segment(900, 1200)],
            ..Model::default()
        };
        assert_eq!(model.playlist_offset(100), 0);
        assert_eq!(model.playlist_offset(200), 100);
        assert_eq!(model.playlist_offset(350), 200);
        assert_eq!(model.playlist_offset(500), 300);
        assert_eq!(model.playlist_offset(950), 450);
        assert_eq!(model.wall_time(0), 100);
        assert_eq!(model.wall_time(100), 200);
        assert_eq!(model.wall_time(200), 400);
        assert_eq!(model.wall_time(450), 950);
        assert_eq!(model.wall_time(10_000), 1000);
    }

    #[test]
    fn the_nearest_event_is_named_only_when_close_enough() {
        let model = Model {
            from: 0,
            to: 100 * US,
            events: vec![
                EventRecord {
                    id: 1,
                    kind: crate::api::EventKind::Named("motion".into()),
                    nvr_time: 10 * US,
                    retain_until_us: None,
                },
                EventRecord {
                    id: 2,
                    kind: crate::api::EventKind::Other {
                        other: "NewFile".into(),
                    },
                    nvr_time: 50 * US,
                    retain_until_us: Some(1),
                },
            ],
            ..Model::default()
        };
        assert!(
            model
                .nearest_event_label(12 * US, 5 * US)
                .unwrap()
                .starts_with("motion at ")
        );
        let label = model.nearest_event_label(48 * US, 5 * US).unwrap();
        assert!(
            label.starts_with("NewFile at ") && label.ends_with(" (kept)"),
            "{label}"
        );
        assert_eq!(model.nearest_event_label(30 * US, 5 * US), None);
    }
}
