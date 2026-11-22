//! The timeline: a bar over a wall-clock window showing recorded coverage,
//! event markers, and the playhead, with a click choosing a time. The bar
//! draws a [`TimelineModel`] from [`iris_client::timeline_model`]. The model
//! also maps between a wall-clock time and a playlist position, and gives the
//! bar's geometry, its ticks included.

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

use gtk::prelude::*;
use gtk::{cairo, glib};
use iris_client::api::EventRecord;
use iris_client::timeline_model::TimelineModel;
use iris_core::time::WallUs;
use tracing::debug;

/// Height of the drawing area in pixels.
const HEIGHT: i32 = 48;
/// Microseconds per second, for wall-clock arithmetic.
pub const MICROS_PER_SECOND: i64 = 1_000_000;

/// The timeline widget.
#[derive(Clone, Debug)]
pub struct Timeline {
    area: gtk::DrawingArea,
    model: Rc<RefCell<TimelineModel>>,
}

impl Timeline {
    /// Builds an empty timeline; `on_seek` receives the wall-clock time of a click.
    pub fn new(on_seek: impl Fn(WallUs) + 'static) -> Self {
        let area = gtk::DrawingArea::builder()
            .content_height(HEIGHT)
            .hexpand(true)
            .build();
        let model = Rc::new(RefCell::new(TimelineModel::default()));
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
                    model.time_at_x(x, width)
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
    pub fn set_model(&self, model: TimelineModel) {
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
    pub fn with_model<R>(&self, read: impl FnOnce(&TimelineModel) -> R) -> R {
        read(&self.model.borrow())
    }
}

fn draw(model: &TimelineModel, cr: &cairo::Context, width: f64, height: f64) {
    let x_of = |time: WallUs| model.time_to_x(time, width);
    cr.set_source_rgb(0.12, 0.12, 0.12);
    cr.rectangle(0.0, 0.0, width, height);
    let _ = cr.fill();
    for s in &model.segments {
        let bar = model.span_extent(s.start, s.end, width);
        if s.sealed {
            cr.set_source_rgb(0.25, 0.55, 0.85);
        } else {
            cr.set_source_rgb(0.35, 0.7, 0.45);
        }
        cr.rectangle(
            bar.start_x,
            height * 0.35,
            (bar.end_x - bar.start_x).max(1.0),
            height * 0.45,
        );
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
    cr.set_source_rgb(0.5, 0.5, 0.5);
    for x in model.tick_xs(width) {
        cr.rectangle(x, height * 0.85, 1.0, height * 0.15);
        let _ = cr.fill();
    }
    if let (Some(mark_in), Some(mark_out)) = (model.mark_in, model.mark_out) {
        cr.set_source_rgba(1.0, 1.0, 1.0, 0.18);
        let range = model.span_extent(mark_in, mark_out, width);
        cr.rectangle(
            range.start_x,
            0.0,
            (range.end_x - range.start_x).max(1.0),
            height,
        );
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

/// The event of `model` nearest to `time` within `within` as
/// "kind at HH:MM:SS", with "(kept)" when retained.
#[must_use]
pub fn nearest_event_label(
    model: &TimelineModel,
    time: WallUs,
    within: Duration,
) -> Option<String> {
    model.nearest_event(time, within).map(|e| {
        let kept = if e.retain_until.is_some() {
            " (kept)"
        } else {
            ""
        };
        format!("{} at {}{kept}", e.kind, clock_label(e.nvr_time))
    })
}

#[cfg(test)]
mod tests {
    use iris_client::api::{EventId, EventKind};

    use super::*;

    fn at(micros: i64) -> WallUs {
        WallUs::new(micros)
    }

    fn event(id: i64, kind: EventKind, nvr_time: i64, retained: bool) -> EventRecord {
        EventRecord {
            id: EventId::try_new(id).unwrap(),
            kind,
            nvr_time: at(nvr_time),
            retain_until: retained.then_some(at(1)),
        }
    }

    /// A window of 100 s with a motion event at 10 s and a kept event of a
    /// vendor code at 50 s.
    fn two_events() -> TimelineModel {
        TimelineModel {
            from: at(0),
            to: at(100 * MICROS_PER_SECOND),
            events: vec![
                event(
                    1,
                    EventKind::Named("motion".into()),
                    10 * MICROS_PER_SECOND,
                    /* retained */ false,
                ),
                event(
                    2,
                    EventKind::Other {
                        other: "NewFile".into(),
                    },
                    50 * MICROS_PER_SECOND,
                    /* retained */ true,
                ),
            ],
            ..TimelineModel::default()
        }
    }

    const NEAR: Duration = Duration::from_secs(5);

    #[test]
    fn an_event_close_enough_is_named_by_its_kind() {
        let label = nearest_event_label(&two_events(), at(12 * MICROS_PER_SECOND), NEAR).unwrap();
        assert!(label.starts_with("motion at "), "{label}");
    }

    #[test]
    fn a_kept_event_is_named_with_kept_after_its_kind() {
        let label = nearest_event_label(&two_events(), at(48 * MICROS_PER_SECOND), NEAR).unwrap();
        assert!(
            label.starts_with("NewFile at ") && label.ends_with(" (kept)"),
            "{label}"
        );
    }

    #[test]
    fn no_event_is_named_when_none_is_close_enough() {
        assert_eq!(
            nearest_event_label(&two_events(), at(30 * MICROS_PER_SECOND), NEAR),
            None
        );
    }
}
