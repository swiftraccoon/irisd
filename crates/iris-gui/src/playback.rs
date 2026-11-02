//! The playback view: one camera's recordings over a window, played from the
//! daemon's HLS VOD playlist through `playbin3`, with the timeline for seeking.

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use gst::prelude::*;
use gtk::prelude::*;
use gtk::{gdk, glib};
use tracing::{info, warn};

use crate::api::Client;
use crate::thumbs;
use crate::timeline::{self, Model, Timeline};

/// Microseconds per second.
const US: i64 = 1_000_000;

/// The video sink handed to `playbin3`; see [`Playback::start`].
const VIDEO_SINK: &str = "glupload ! glcolorconvert ! gtk4paintablesink name=sink sync=true";
/// How often the playhead follows the pipeline position.
const POSITION_PERIOD: Duration = Duration::from_millis(250);
/// Most events drawn on the timeline.
const EVENT_LIMIT: usize = 2000;
/// How close an event must be to a seek target to be named in the title, or
/// to the playhead to be kept.
const EVENT_NEAR_US: i64 = 5_000_000;
/// How long "Keep event" keeps an event's footage.
const KEEP_EVENT_DAYS: i64 = 30;
/// Thumbnails across the timeline.
const THUMBNAILS: usize = 12;
/// Height of the thumbnail strip in pixels.
const THUMBNAIL_HEIGHT: i32 = 72;

/// The playback view for one camera.
pub struct Playback {
    camera: String,
    kind: String,
    thumb_kind: String,
    widget: gtk::Box,
    thumb_pictures: Vec<gtk::Picture>,
    picture: gtk::Picture,
    title: gtk::Label,
    status: gtk::Label,
    timeline: Timeline,
    pipeline: RefCell<Option<Running>>,
    marks: RefCell<(Option<i64>, Option<i64>)>,
    client: RefCell<Option<Arc<Client>>>,
    runtime: RefCell<Option<Arc<tokio::runtime::Runtime>>>,
}

struct Running {
    playbin: gst::Element,
    _bus_watch: gst::bus::BusWatchGuard,
    position_timer: glib::SourceId,
}

impl Playback {
    /// Builds the view's widgets; `extra` is placed in the header before the
    /// title (navigation buttons), and `thumb_kind` is the stream the
    /// thumbnails come from. Nothing plays until [`Playback::open`].
    pub fn new(camera: &str, kind: &str, thumb_kind: &str, extra: &[gtk::Button]) -> Rc<Self> {
        let header = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        for button in extra {
            header.append(button);
        }
        let play = gtk::Button::with_label("Play / Pause");
        header.append(&play);
        let mark_in = gtk::Button::with_label("Mark in");
        header.append(&mark_in);
        let mark_out = gtk::Button::with_label("Mark out");
        header.append(&mark_out);
        let export = gtk::Button::with_label("Export");
        header.append(&export);
        let keep = gtk::Button::with_label("Keep event");
        header.append(&keep);
        let title = gtk::Label::builder().xalign(0.0).hexpand(true).build();
        header.append(&title);
        let status = gtk::Label::builder().xalign(0.0).margin_start(4).build();
        let picture = gtk::Picture::new();
        picture.set_content_fit(gtk::ContentFit::Contain);
        picture.set_hexpand(true);
        picture.set_vexpand(true);
        let thumb_strip = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .homogeneous(true)
            .height_request(THUMBNAIL_HEIGHT)
            .build();
        let thumb_pictures: Vec<gtk::Picture> = (0..THUMBNAILS)
            .map(|_| {
                let picture = gtk::Picture::new();
                picture.set_content_fit(gtk::ContentFit::Cover);
                picture.set_hexpand(true);
                thumb_strip.append(&picture);
                picture
            })
            .collect();
        let widget = gtk::Box::new(gtk::Orientation::Vertical, 4);
        widget.append(&header);
        widget.append(&picture);
        widget.append(&status);
        widget.append(&thumb_strip);
        let this = Rc::new_cyclic(|weak: &std::rc::Weak<Self>| {
            let weak = weak.clone();
            let timeline = Timeline::new(move |time| {
                if let Some(view) = weak.upgrade() {
                    view.seek(time);
                }
            });
            widget.append(timeline.widget());
            Self {
                camera: camera.to_owned(),
                kind: kind.to_owned(),
                thumb_kind: thumb_kind.to_owned(),
                widget,
                thumb_pictures,
                picture,
                title,
                status,
                timeline,
                pipeline: RefCell::new(None),
                marks: RefCell::new((None, None)),
                client: RefCell::new(None),
                runtime: RefCell::new(None),
            }
        });
        play.connect_clicked({
            let this = Rc::clone(&this);
            move |_| this.toggle()
        });
        mark_in.connect_clicked({
            let this = Rc::clone(&this);
            move |_| this.mark(true)
        });
        mark_out.connect_clicked({
            let this = Rc::clone(&this);
            move |_| this.mark(false)
        });
        keep.connect_clicked({
            let this = Rc::clone(&this);
            move |_| {
                let this = Rc::clone(&this);
                glib::spawn_future_local(async move { this.keep_nearest_event().await });
            }
        });
        export.connect_clicked({
            let this = Rc::clone(&this);
            move |button| {
                let this = Rc::clone(&this);
                let window = button
                    .root()
                    .and_then(|root| root.downcast::<gtk::Window>().ok());
                glib::spawn_future_local(async move { this.export_dialog(window.as_ref()).await });
            }
        });
        this
    }

    /// Sets the in (`start == true`) or out mark at the playhead, or at the
    /// window edge when nothing has played yet.
    fn mark(&self, start: bool) {
        let model = self.timeline.model();
        let time = model
            .playhead
            .unwrap_or(if start { model.from } else { model.to });
        let mut marks = self.marks.borrow_mut();
        if start {
            marks.0 = Some(time);
        } else {
            marks.1 = Some(time);
        }
        let (mark_in, mark_out) = *marks;
        drop(marks);
        self.timeline.set_marks(mark_in, mark_out);
        self.status.set_label(&match (mark_in, mark_out) {
            (Some(i), Some(o)) if o > i => format!(
                "export range {} to {} ({} s)",
                timeline::clock_label(i),
                timeline::clock_label(o),
                (o - i) / US
            ),
            (Some(i), Some(_)) => format!(
                "out mark must be after the in mark ({})",
                timeline::clock_label(i)
            ),
            (Some(i), None) => format!("in mark at {}; set the out mark", timeline::clock_label(i)),
            (None, Some(o)) => format!("out mark at {}; set the in mark", timeline::clock_label(o)),
            (None, None) => String::new(),
        });
    }

    /// Keeps the event nearest to the playhead (within [`EVENT_NEAR_US`]) from
    /// retention for [`KEEP_EVENT_DAYS`] days, or clears the mark of one that
    /// is already kept.
    async fn keep_nearest_event(self: Rc<Self>) {
        let model = self.timeline.model();
        let Some(time) = model.playhead else {
            self.status.set_label("play or seek to an event first");
            return;
        };
        let Some(event) = model.nearest_event(time, EVENT_NEAR_US).cloned() else {
            self.status
                .set_label("no event within five seconds of the playhead");
            return;
        };
        let (client, runtime) = (self.client.borrow().clone(), self.runtime.borrow().clone());
        let (Some(client), Some(runtime)) = (client, runtime) else {
            return;
        };
        let until = match event.retain_until_us {
            Some(_) => None,
            None => Some(glib::real_time() + KEEP_EVENT_DAYS * 86_400 * US),
        };
        let task = runtime.spawn(async move { client.retain_event(event.id, until).await });
        match task.await {
            Ok(Ok(updated)) => {
                let kept = updated.retain_until_us.is_some();
                let mut model = self.timeline.model();
                model.replace_event(updated.clone());
                self.timeline.set_model(model);
                self.status.set_label(&format!(
                    "{} at {}: {}",
                    updated.kind.label(),
                    timeline::clock_label(updated.nvr_time),
                    if kept {
                        format!("kept for {KEEP_EVENT_DAYS} days")
                    } else {
                        "no longer kept".to_owned()
                    }
                ));
            }
            Ok(Err(error)) => self.status.set_label(&format!("keep failed: {error:#}")),
            Err(error) => self.status.set_label(&format!("keep task failed: {error}")),
        }
    }

    /// The export range when both marks are set and ordered.
    fn export_range(&self) -> Option<(i64, i64)> {
        match *self.marks.borrow() {
            (Some(i), Some(o)) if o > i => Some((i, o)),
            _ => None,
        }
    }

    /// Asks where to save, then exports the marked range there.
    async fn export_dialog(self: Rc<Self>, window: Option<&gtk::Window>) {
        let Some((from, to)) = self.export_range() else {
            self.status.set_label("set the in and out marks first");
            return;
        };
        let dialog = gtk::FileDialog::builder()
            .title("Export recording")
            .initial_name(export_name(&self.camera, &self.kind, from))
            .build();
        match dialog.save_future(window).await {
            Ok(file) => {
                if let Some(path) = file.path() {
                    self.export_to(path, from, to).await;
                }
            }
            Err(error) => info!(%error, "export cancelled"),
        }
    }

    /// Exports `[from, to)` to `path`, reporting progress in the status line.
    pub async fn export_to(self: Rc<Self>, path: PathBuf, from: i64, to: i64) {
        let (client, runtime) = (self.client.borrow().clone(), self.runtime.borrow().clone());
        let (Some(client), Some(runtime)) = (client, runtime) else {
            self.status.set_label("nothing is open");
            return;
        };
        let (tx, rx) = async_channel::unbounded::<u64>();
        let (camera, kind) = (self.camera.clone(), self.kind.clone());
        let target = path.clone();
        let task = runtime.spawn(async move {
            client
                .export(&camera, &kind, from, to, &target, |written| {
                    let _ = tx.try_send(written);
                })
                .await
        });
        let status = self.status.clone();
        let display = path.display().to_string();
        glib::spawn_future_local(async move {
            while let Ok(written) = rx.recv().await {
                status.set_label(&format!("exporting to {display}: {} MiB", written >> 20));
            }
        });
        match task.await {
            Ok(Ok(bytes)) => {
                info!(path = %path.display(), bytes, "export done");
                self.status.set_label(&format!(
                    "exported {} MiB to {}",
                    bytes >> 20,
                    path.display()
                ));
            }
            Ok(Err(error)) => {
                warn!(%error, "export failed");
                self.status.set_label(&format!("export failed: {error:#}"));
            }
            Err(error) => self
                .status
                .set_label(&format!("export task failed: {error}")),
        }
    }

    /// The widget to show.
    pub fn widget(&self) -> &gtk::Box {
        &self.widget
    }

    /// Loads the window `[from, to)`: the timeline from the segment and event
    /// lists, and the player from the VOD playlist. Runs on the main loop;
    /// the HTTP requests run on `runtime`.
    pub async fn open(
        self: &Rc<Self>,
        client: Arc<Client>,
        runtime: Arc<tokio::runtime::Runtime>,
        from: i64,
        to: i64,
    ) {
        self.close();
        *self.client.borrow_mut() = Some(Arc::clone(&client));
        *self.runtime.borrow_mut() = Some(Arc::clone(&runtime));
        self.title.set_label(&format!(
            "{} {} stream, {} to {}",
            self.camera,
            self.kind,
            timeline::clock_label(from),
            timeline::clock_label(to)
        ));
        let (camera, kind) = (self.camera.clone(), self.kind.clone());
        let lists = runtime.spawn({
            let client = Arc::clone(&client);
            async move {
                let segments = client.segments(&camera, &kind, from, to).await;
                let events = client.events(&camera, from, to, EVENT_LIMIT).await;
                (segments, events)
            }
        });
        let (segments, events) = match lists.await {
            Ok(lists) => lists,
            Err(error) => {
                self.title.set_label(&format!("listing failed: {error}"));
                return;
            }
        };
        let mut segments = match segments {
            Ok(segments) => segments,
            Err(error) => {
                self.title.set_label(&format!("{error:#}"));
                return;
            }
        };
        segments.sort_by_key(|s| s.wall_start_us);
        let events = events.unwrap_or_else(|error| {
            warn!(%error, "events unavailable");
            Vec::new()
        });
        info!(camera = %self.camera, segments = segments.len(), events = events.len(), "window loaded");
        self.timeline.set_model(Model {
            from,
            to,
            segments: segments.clone(),
            events,
            playhead: None,
            mark_in: None,
            mark_out: None,
        });
        if segments.is_empty() {
            self.title
                .set_label(&format!("{}: nothing recorded in this window", self.camera));
            return;
        }
        self.load_thumbnails(&client, &runtime, from, to, &segments);
        let url = match client.vod_url(&self.camera, &self.kind, from, to) {
            Ok(url) => url,
            Err(error) => {
                self.title.set_label(&format!("{error:#}"));
                return;
            }
        };
        if let Err(error) = self.start(url.as_str()) {
            self.title.set_label(&format!("{error:#}"));
        }
    }

    /// Fetches and decodes one keyframe per thumbnail slot whose time is
    /// recorded, on the runtime, and shows each as it arrives.
    fn load_thumbnails(
        self: &Rc<Self>,
        client: &Arc<Client>,
        runtime: &tokio::runtime::Runtime,
        from: i64,
        to: i64,
        segments: &[crate::api::Segment],
    ) {
        for picture in &self.thumb_pictures {
            picture.set_paintable(None::<&gdk::Paintable>);
        }
        let (tx, rx) = async_channel::unbounded::<(usize, thumbs::Image)>();
        let span = to - from;
        for slot in 0..THUMBNAILS {
            let t = from + span * (2 * slot as i64 + 1) / (2 * THUMBNAILS as i64);
            if !segments
                .iter()
                .any(|s| s.wall_start_us <= t && t < s.wall_end_us)
            {
                continue;
            }
            let client = Arc::clone(client);
            let tx = tx.clone();
            let (camera, kind) = (self.camera.clone(), self.thumb_kind.clone());
            runtime.spawn(async move {
                let bytes = match client.keyframe(&camera, &kind, t).await {
                    Ok(Some(bytes)) => bytes,
                    Ok(None) => return,
                    Err(error) => {
                        warn!(%error, slot, "keyframe fetch failed");
                        return;
                    }
                };
                match tokio::task::spawn_blocking(move || thumbs::decode(&bytes)).await {
                    Ok(Ok(image)) => {
                        let _ = tx.send((slot, image)).await;
                    }
                    Ok(Err(error)) => warn!(%error, slot, "keyframe decode failed"),
                    Err(error) => warn!(%error, slot, "keyframe decode task failed"),
                }
            });
        }
        drop(tx);
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let mut shown = 0usize;
            while let Ok((slot, image)) = rx.recv().await {
                let Some(view) = weak.upgrade() else { return };
                if let Some(picture) = view.thumb_pictures.get(slot) {
                    picture.set_paintable(Some(&image.texture()));
                    shown += 1;
                }
            }
            info!(shown, "thumbnails loaded");
        });
    }

    fn start(self: &Rc<Self>, uri: &str) -> anyhow::Result<()> {
        let playbin = gst::ElementFactory::make("playbin3")
            .build()
            .context("creating playbin3")?;
        // The sink is a bin that keeps frames on the GPU: `glupload` is a no-op
        // when the decoder wrote GL memory, `glcolorconvert` converts to RGBA
        // there, and `gtk4paintablesink` hands the texture to GTK. Given as
        // the video sink, it keeps `playbin3` from inserting its CPU converters.
        let sink_bin = gst::parse::bin_from_description(VIDEO_SINK, true)
            .context("building the video sink")?;
        let sink = sink_bin.by_name("sink").context("sink missing")?;
        let paintable = sink.property::<gdk::Paintable>("paintable");
        self.picture.set_paintable(Some(&paintable));
        playbin.set_property("video-sink", &sink_bin);
        playbin.set_property("uri", uri);
        let bus = playbin.bus().context("playbin bus")?;
        let bus_watch = bus
            .add_watch_local({
                let camera = self.camera.clone();
                let title = self.title.clone();
                move |_, message| {
                    match message.view() {
                        gst::MessageView::Error(error) => {
                            warn!(camera = %camera, error = %error.error(), "playback error");
                            title.set_label(&format!("playback error: {}", error.error()));
                        }
                        gst::MessageView::Eos(_) => info!(camera = %camera, "end of window"),
                        gst::MessageView::AsyncDone(_) => {
                            info!(camera = %camera, "playback ready");
                        }
                        _ => {}
                    }
                    glib::ControlFlow::Continue
                }
            })
            .context("watching the playback bus")?;
        playbin
            .set_state(gst::State::Playing)
            .context("starting playback")?;
        let position_timer = glib::timeout_add_local(POSITION_PERIOD, {
            let weak = Rc::downgrade(self);
            move || {
                let Some(view) = weak.upgrade() else {
                    return glib::ControlFlow::Break;
                };
                let position = view
                    .pipeline
                    .borrow()
                    .as_ref()
                    .and_then(|r| r.playbin.query_position::<gst::ClockTime>());
                if let Some(position) = position {
                    let offset = i64::try_from(position.useconds()).unwrap_or(i64::MAX);
                    let model = view.timeline.model();
                    view.timeline.set_playhead(Some(model.wall_time(offset)));
                }
                glib::ControlFlow::Continue
            }
        });
        *self.pipeline.borrow_mut() = Some(Running {
            playbin,
            _bus_watch: bus_watch,
            position_timer,
        });
        Ok(())
    }

    /// Seeks to a wall-clock time inside the window.
    pub fn seek(&self, time: i64) {
        let model = self.timeline.model();
        let offset = model.playlist_offset(time).max(0);
        if let Some(running) = self.pipeline.borrow().as_ref() {
            let target = gst::ClockTime::from_useconds(u64::try_from(offset).unwrap_or(0));
            if let Err(error) = running
                .playbin
                .seek_simple(gst::SeekFlags::FLUSH | gst::SeekFlags::KEY_UNIT, target)
            {
                warn!(%error, "seek failed");
            } else {
                info!(time = %timeline::clock_label(time), offset_s = offset / 1_000_000, "seek");
            }
        }
        self.timeline.set_playhead(Some(time));
        let near = model
            .nearest_event_label(time, EVENT_NEAR_US)
            .map_or_else(String::new, |event| format!(", {event}"));
        self.title.set_label(&format!(
            "{} {} stream, {} to {}, at {}{near}",
            self.camera,
            self.kind,
            timeline::clock_label(model.from),
            timeline::clock_label(model.to),
            timeline::clock_label(time)
        ));
    }

    /// Toggles between playing and paused.
    pub fn toggle(&self) {
        if let Some(running) = self.pipeline.borrow().as_ref() {
            let (_, state, _) = running.playbin.state(gst::ClockTime::ZERO);
            let next = if state == gst::State::Playing {
                gst::State::Paused
            } else {
                gst::State::Playing
            };
            let _ = running.playbin.set_state(next);
        }
    }

    /// Stops playback and drops the pipeline.
    pub fn close(&self) {
        if let Some(running) = self.pipeline.borrow_mut().take() {
            running.position_timer.remove();
            let _ = running.playbin.set_state(gst::State::Null);
        }
    }
}

/// A file name for an export: camera, kind, and the start time in UTC.
fn export_name(camera: &str, kind: &str, from: i64) -> String {
    let stamp = glib::DateTime::from_unix_utc(from / US)
        .and_then(|t| t.format("%Y%m%dT%H%M%SZ"))
        .map_or_else(|_| from.to_string(), |s| s.to_string());
    format!("{camera}-{kind}-{stamp}.mp4")
}

impl Drop for Playback {
    fn drop(&mut self) {
        self.close();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn export_names_carry_camera_kind_and_utc_start() {
        assert_eq!(
            export_name("cam1", "main", 1_793_571_648_056_937),
            "cam1-main-20261101T222048Z.mp4"
        );
    }
}
