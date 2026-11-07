//! The playback view: one camera's recordings over a window, played from the
//! daemon's HLS VOD playlist through `playbin3`, with the timeline for seeking.

use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use futures::StreamExt as _;
use gst::prelude::*;
use gtk::prelude::*;
use gtk::{gdk, glib};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio::task::AbortHandle;
use tokio_util::task::AbortOnDropHandle;
use tracing::{info, warn};

use crate::api::Client;
use crate::snapshot::Snapshot;
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
/// Keyframe requests and decoders that may be active together.
const THUMBNAIL_CONCURRENCY: usize = 2;
/// Height of the thumbnail strip in pixels.
const THUMBNAIL_HEIGHT: i32 = 72;

/// Builds the decoder admission pool shared by every playback view on one runtime.
pub(crate) fn thumbnail_decoder_pool() -> Arc<Semaphore> {
    Arc::new(Semaphore::new(THUMBNAIL_CONCURRENCY))
}

fn export_progress_channel() -> (
    tokio::sync::watch::Sender<u64>,
    tokio::sync::watch::Receiver<u64>,
) {
    tokio::sync::watch::channel(0)
}

fn decode_thumbnail(
    keyframe: Vec<u8>,
    permit: OwnedSemaphorePermit,
) -> anyhow::Result<thumbs::Image> {
    #[cfg(test)]
    let activity = DecoderActivity::enter();
    #[cfg(test)]
    let result = if wait_at_decoder_gate() {
        Err(anyhow::anyhow!("synthetic decoder gate released"))
    } else {
        thumbs::decode(keyframe)
    };
    #[cfg(not(test))]
    let result = thumbs::decode(keyframe);
    #[cfg(test)]
    drop(activity);
    drop(permit);
    result
}

#[cfg(test)]
static ACTIVE_DECODERS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
#[cfg(test)]
static PEAK_DECODERS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
#[cfg(test)]
static DECODER_ADMISSION_ATTEMPTS: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);
#[cfg(test)]
static DECODER_GATE_ENABLED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);
#[cfg(test)]
static DECODER_GATE: std::sync::LazyLock<(std::sync::Mutex<bool>, std::sync::Condvar)> =
    std::sync::LazyLock::new(|| (std::sync::Mutex::new(false), std::sync::Condvar::new()));

#[cfg(test)]
fn wait_at_decoder_gate() -> bool {
    use std::sync::atomic::Ordering;

    if !DECODER_GATE_ENABLED.load(Ordering::SeqCst) {
        return false;
    }
    let (released, changed) = &*DECODER_GATE;
    let mut released = released
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    while !*released {
        released = changed
            .wait(released)
            .unwrap_or_else(std::sync::PoisonError::into_inner);
    }
    true
}

#[cfg(test)]
struct DecoderActivity;

#[cfg(test)]
impl DecoderActivity {
    fn enter() -> Self {
        use std::sync::atomic::Ordering;

        let active = ACTIVE_DECODERS.fetch_add(1, Ordering::SeqCst) + 1;
        PEAK_DECODERS.fetch_max(active, Ordering::SeqCst);
        Self
    }
}

#[cfg(test)]
impl Drop for DecoderActivity {
    fn drop(&mut self) {
        use std::sync::atomic::Ordering;

        ACTIVE_DECODERS.fetch_sub(1, Ordering::SeqCst);
    }
}

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
    thumbnail_decoders: Arc<Semaphore>,
    pipeline: RefCell<Option<Running>>,
    opening: RefCell<Option<AbortHandle>>,
    thumbnails: RefCell<Option<AbortHandle>>,
    thumbnail_receiver: RefCell<Option<glib::JoinHandle<()>>>,
    exporting: RefCell<Option<AbortHandle>>,
    export_generation: Cell<u64>,
    generation: Cell<u64>,
    marks: RefCell<(Option<i64>, Option<i64>)>,
    client: RefCell<Option<Arc<Client>>>,
    runtime: RefCell<Option<Arc<tokio::runtime::Runtime>>>,
}

struct Running {
    playbin: gst::Element,
    _bus_watch: gst::bus::BusWatchGuard,
    position_timer: glib::SourceId,
    _snapshot: Snapshot,
}

impl Playback {
    /// Builds the view's widgets; `extra` is placed in the header before the
    /// title (navigation buttons), and `thumb_kind` is the stream the
    /// thumbnails come from. `thumbnail_decoders` is shared by all playback
    /// views on the application runtime. Nothing plays until [`Playback::open`].
    pub fn new(
        camera: &str,
        kind: &str,
        thumb_kind: &str,
        extra: &[gtk::Button],
        thumbnail_decoders: Arc<Semaphore>,
    ) -> Rc<Self> {
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
                thumbnail_decoders,
                pipeline: RefCell::new(None),
                opening: RefCell::new(None),
                thumbnails: RefCell::new(None),
                thumbnail_receiver: RefCell::new(None),
                exporting: RefCell::new(None),
                export_generation: Cell::new(0),
                generation: Cell::new(0),
                marks: RefCell::new((None, None)),
                client: RefCell::new(None),
                runtime: RefCell::new(None),
            }
        });
        Self::connect_buttons(&this, [play, mark_in, mark_out, keep, export]);
        this
    }

    fn connect_buttons(this: &Rc<Self>, buttons: [gtk::Button; 5]) {
        let [play, mark_in, mark_out, keep, export] = buttons;
        play.connect_clicked({
            let weak = Rc::downgrade(this);
            move |_| {
                if let Some(this) = weak.upgrade() {
                    this.toggle();
                }
            }
        });
        mark_in.connect_clicked({
            let weak = Rc::downgrade(this);
            move |_| {
                if let Some(this) = weak.upgrade() {
                    this.mark(true);
                }
            }
        });
        mark_out.connect_clicked({
            let weak = Rc::downgrade(this);
            move |_| {
                if let Some(this) = weak.upgrade() {
                    this.mark(false);
                }
            }
        });
        keep.connect_clicked({
            let weak = Rc::downgrade(this);
            move |_| {
                let Some(this) = weak.upgrade() else { return };
                glib::spawn_future_local(async move { this.keep_nearest_event().await });
            }
        });
        export.connect_clicked({
            let weak = Rc::downgrade(this);
            move |button| {
                let Some(this) = weak.upgrade() else { return };
                let window = button
                    .root()
                    .and_then(|root| root.downcast::<gtk::Window>().ok());
                glib::spawn_future_local(async move { this.export_dialog(window.as_ref()).await });
            }
        });
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
                (i128::from(o) - i128::from(i)) / i128::from(US)
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
        let export_generation = self.export_generation.get().wrapping_add(1);
        self.export_generation.set(export_generation);
        if let Some(exporting) = self.exporting.borrow_mut().take() {
            exporting.abort();
        }
        let (tx, mut rx) = export_progress_channel();
        let (camera, kind) = (self.camera.clone(), self.kind.clone());
        let target = path.clone();
        let task = AbortOnDropHandle::new(runtime.spawn(async move {
            client
                .export(&camera, &kind, from, to, &target, |written| {
                    tx.send_replace(written);
                })
                .await
        }));
        *self.exporting.borrow_mut() = Some(task.abort_handle());
        let status = self.status.clone();
        let display = path.display().to_string();
        let weak = Rc::downgrade(&self);
        let progress = glib::spawn_future_local(async move {
            while rx.changed().await.is_ok() {
                let Some(view) = weak.upgrade() else { return };
                if view.export_generation.get() != export_generation {
                    return;
                }
                let written = *rx.borrow_and_update();
                status.set_label(&format!("exporting to {display}: {} MiB", written >> 20));
            }
        });
        let result = task.await;
        progress.abort();
        if self.export_generation.get() != export_generation {
            return;
        }
        self.exporting.borrow_mut().take();
        match result {
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
        let generation = self.generation.get();
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
        let lists = AbortOnDropHandle::new(runtime.spawn({
            let client = Arc::clone(&client);
            async move {
                let segments = client.segments(&camera, &kind, from, to).await;
                let events = client.events(&camera, from, to, EVENT_LIMIT).await;
                let playback: anyhow::Result<_> = if segments.as_ref().is_ok_and(Vec::is_empty) {
                    Ok(None)
                } else {
                    async {
                        let playlist = client.vod_playlist(&camera, &kind, from, to).await?;
                        let snapshot = Snapshot::new(playlist.body).await?;
                        Ok(Some((playlist.fragments, snapshot)))
                    }
                    .await
                };
                (segments, events, playback)
            }
        }));
        *self.opening.borrow_mut() = Some(lists.abort_handle());
        let loaded = lists.await;
        if generation != self.generation.get() {
            return;
        }
        self.opening.borrow_mut().take();
        let (segments, events, playback) = match loaded {
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
        let playback = match playback {
            Ok(playback) => playback,
            Err(error) => {
                warn!(camera = %self.camera, error = %format!("{error:#}"), "playback timing unavailable");
                self.title
                    .set_label(&format!("playback timing unavailable: {error:#}"));
                return;
            }
        };
        info!(camera = %self.camera, segments = segments.len(), events = events.len(), "window loaded");
        self.timeline.set_model(Model {
            from,
            to,
            segments: segments.clone(),
            playback: playback
                .as_ref()
                .map_or_else(Vec::new, |(fragments, _)| fragments.clone()),
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
        let Some((_, snapshot)) = playback else {
            return;
        };
        if let Err(error) = self.start(snapshot) {
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
        self.stop_thumbnails();
        for picture in &self.thumb_pictures {
            picture.set_paintable(None::<&gdk::Paintable>);
        }
        let generation = self.generation.get();
        let (tx, rx) = async_channel::bounded::<(usize, thumbs::Image)>(THUMBNAIL_CONCURRENCY);
        let span = i128::from(to) - i128::from(from);
        let mut jobs = Vec::with_capacity(THUMBNAILS);
        for slot in 0..THUMBNAILS {
            let numerator = i128::try_from(2 * slot + 1).expect("thumbnail slot fits i128");
            let denominator = i128::try_from(2 * THUMBNAILS).expect("thumbnail count fits i128");
            let t = i64::try_from(i128::from(from) + span * numerator / denominator).unwrap_or(to);
            if !segments
                .iter()
                .any(|s| s.wall_start_us <= t && t < s.wall_end_us)
            {
                continue;
            }
            jobs.push((slot, t));
        }
        let (camera, kind) = (self.camera.clone(), self.thumb_kind.clone());
        let thumbnail_decoders = Arc::clone(&self.thumbnail_decoders);
        let task = runtime.spawn({
            let client = Arc::clone(client);
            async move {
                futures::stream::iter(jobs)
                    .for_each_concurrent(THUMBNAIL_CONCURRENCY, |(slot, t)| {
                        let client = Arc::clone(&client);
                        let thumbnail_decoders = Arc::clone(&thumbnail_decoders);
                        let tx = tx.clone();
                        let camera = camera.clone();
                        let kind = kind.clone();
                        async move {
                            #[cfg(test)]
                            DECODER_ADMISSION_ATTEMPTS
                                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                            let Ok(permit) = thumbnail_decoders.acquire_owned().await else {
                                return;
                            };
                            let bytes = match client.keyframe(&camera, &kind, t).await {
                                Ok(Some(bytes)) => bytes,
                                Ok(None) => return,
                                Err(error) => {
                                    warn!(%error, slot, "keyframe fetch failed");
                                    return;
                                }
                            };
                            match tokio::task::spawn_blocking(move || {
                                decode_thumbnail(bytes, permit)
                            })
                            .await
                            {
                                Ok(Ok(image)) => {
                                    let _ = tx.send((slot, image)).await;
                                }
                                Ok(Err(error)) => {
                                    warn!(%error, slot, "keyframe decode failed");
                                }
                                Err(error) => {
                                    warn!(%error, slot, "keyframe decode task failed");
                                }
                            }
                        }
                    })
                    .await;
            }
        });
        *self.thumbnails.borrow_mut() = Some(task.abort_handle());
        let weak = Rc::downgrade(self);
        let receiver = glib::spawn_future_local(async move {
            let mut shown = 0usize;
            while let Ok((slot, image)) = rx.recv().await {
                let Some(view) = weak.upgrade() else { return };
                if view.show_thumbnail(generation, slot, &image) {
                    shown += 1;
                }
            }
            info!(shown, "thumbnails loaded");
        });
        *self.thumbnail_receiver.borrow_mut() = Some(receiver);
    }

    fn show_thumbnail(&self, generation: u64, slot: usize, image: &thumbs::Image) -> bool {
        if generation != self.generation.get() {
            return false;
        }
        let Some(picture) = self.thumb_pictures.get(slot) else {
            return false;
        };
        picture.set_paintable(Some(&image.texture()));
        true
    }

    fn stop_thumbnails(&self) {
        if let Some(task) = self.thumbnails.borrow_mut().take() {
            task.abort();
        }
        if let Some(receiver) = self.thumbnail_receiver.borrow_mut().take() {
            receiver.abort();
        }
    }

    fn start(self: &Rc<Self>, snapshot: Snapshot) -> anyhow::Result<()> {
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
        playbin.set_property("uri", snapshot.url().as_str());
        let bus = playbin.bus().context("playbin bus")?;
        let bus_watch = bus
            .add_watch_local({
                let camera = self.camera.clone();
                let title = self.title.clone();
                let weak = Rc::downgrade(self);
                let generation = self.generation.get();
                move |_, message| {
                    match message.view() {
                        gst::MessageView::Error(error) => {
                            warn!(camera = %camera, error = %error.error(), "playback error");
                            title.set_label(&format!("playback error: {}", error.error()));
                            let weak = weak.clone();
                            // Drop the bus watch after this callback returns. A
                            // replacement opened before then owns another generation.
                            glib::idle_add_local_once(move || {
                                if let Some(view) = weak.upgrade()
                                    && view.generation.get() == generation
                                {
                                    view.close();
                                }
                            });
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
            _snapshot: snapshot,
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
        self.generation.set(self.generation.get().wrapping_add(1));
        self.export_generation
            .set(self.export_generation.get().wrapping_add(1));
        if let Some(opening) = self.opening.borrow_mut().take() {
            opening.abort();
        }
        self.stop_thumbnails();
        if let Some(exporting) = self.exporting.borrow_mut().take() {
            exporting.abort();
        }
        if let Some(running) = self.pipeline.borrow_mut().take() {
            running.position_timer.remove();
            let _ = running.playbin.set_state(gst::State::Null);
        }
        self.client.borrow_mut().take();
        self.runtime.borrow_mut().take();
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
    use std::convert::Infallible;
    use std::pin::Pin;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::task::{Context, Poll};

    use super::*;

    #[test]
    #[ignore = "requires a display and GStreamer HLS plugins; run under xvfb-run"]
    fn playback_lifecycle_releases_widgets_pending_loads_and_failed_snapshots() {
        // GTK permits one initializer thread per process; the test harness uses
        // a separate thread for each test even with --test-threads=1.
        dropping_playback_releases_the_view_and_button_callbacks();
        closing_playback_cancels_an_unfinished_snapshot_load();
        closing_playback_cancels_bounded_thumbnail_requests();
        replacement_views_share_uninterruptible_decoder_permits();
        export_terminal_status_cannot_be_overwritten_by_progress();
        crate::tests::navigation_event_cancels_a_stalled_playback_request();
        crate::tests::grid_decodes_at_most_max_tiles_feeds();
        a_window_holding_only_a_stale_open_segment_shows_nothing_recorded();
        playback_error_releases_its_snapshot_listener();
    }

    struct RequestGuard {
        active: Arc<AtomicUsize>,
        dropped: async_channel::Sender<()>,
    }

    impl Drop for RequestGuard {
        fn drop(&mut self) {
            self.active.fetch_sub(1, Ordering::SeqCst);
            let _ = self.dropped.try_send(());
        }
    }

    struct PendingBody {
        _guard: RequestGuard,
    }

    struct DecodeGate {
        released: bool,
    }

    impl DecodeGate {
        fn new() -> Self {
            assert_eq!(ACTIVE_DECODERS.load(Ordering::SeqCst), 0);
            DECODER_ADMISSION_ATTEMPTS.store(0, Ordering::SeqCst);
            PEAK_DECODERS.store(0, Ordering::SeqCst);
            let (released, _) = &*DECODER_GATE;
            *released
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = false;
            DECODER_GATE_ENABLED.store(true, Ordering::SeqCst);
            Self { released: false }
        }

        fn release(&mut self) {
            if self.released {
                return;
            }
            let (released, changed) = &*DECODER_GATE;
            *released
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = true;
            self.released = true;
            changed.notify_all();
        }
    }

    impl Drop for DecodeGate {
        fn drop(&mut self) {
            self.release();
            DECODER_GATE_ENABLED.store(false, Ordering::SeqCst);
        }
    }

    impl futures::Stream for PendingBody {
        type Item = Result<axum::body::Bytes, Infallible>;

        fn poll_next(self: Pin<&mut Self>, _context: &mut Context<'_>) -> Poll<Option<Self::Item>> {
            Poll::Pending
        }
    }

    fn dropping_playback_releases_the_view_and_button_callbacks() {
        gtk::init().unwrap();
        let view = Playback::new("fixture", "main", "sub", &[], thumbnail_decoder_pool());
        let weak = Rc::downgrade(&view);
        let timeline = view.timeline.widget().downgrade();
        let widget = view.widget().clone();
        view.close();
        drop(view);
        assert!(
            weak.upgrade().is_none(),
            "widgets must not own the playback view"
        );
        let header = widget
            .first_child()
            .unwrap()
            .downcast::<gtk::Box>()
            .unwrap();
        let mut child = header.first_child();
        while let Some(widget) = child {
            if let Ok(button) = widget.clone().downcast::<gtk::Button>() {
                button.emit_clicked();
            }
            child = widget.next_sibling();
        }
        drop(header);
        drop(widget);
        assert!(
            timeline.upgrade().is_none(),
            "timeline widgets must also be released"
        );
    }

    fn closing_playback_cancels_an_unfinished_snapshot_load() {
        gtk::init().unwrap();
        let runtime = Arc::new(tokio::runtime::Runtime::new().unwrap());
        let listener = runtime
            .block_on(tokio::net::TcpListener::bind("127.0.0.1:0"))
            .unwrap();
        let address = listener.local_addr().unwrap();
        let (tx, rx) = async_channel::bounded(1);
        let server = runtime.spawn(async move {
            let app = axum::Router::new().route(
                "/api/streams/fixture/main/segments",
                axum::routing::get(move || {
                    let tx = tx.clone();
                    async move {
                        tx.send(()).await.unwrap();
                        futures::future::pending::<String>().await
                    }
                }),
            );
            axum::serve(listener, app).await.unwrap();
        });
        let client = Arc::new(Client::new(
            url::Url::parse(&format!("http://{address}")).unwrap(),
            None,
        ));
        let view = Playback::new("fixture", "main", "sub", &[], thumbnail_decoder_pool());
        glib::MainContext::default().block_on(async {
            let opening = glib::spawn_future_local({
                let view = Rc::clone(&view);
                let runtime = Arc::clone(&runtime);
                async move { view.open(client, runtime, 0, 1_000_000).await }
            });
            rx.recv().await.unwrap();
            view.close();
            let timeout = glib::timeout_future(Duration::from_secs(1));
            futures::pin_mut!(timeout);
            assert!(
                matches!(
                    futures::future::select(opening, timeout).await,
                    futures::future::Either::Left(_)
                ),
                "closing playback must cancel its pending request"
            );
        });
        assert!(view.pipeline.borrow().is_none());
        assert!(view.opening.borrow().is_none());
        server.abort();
    }

    fn closing_playback_cancels_bounded_thumbnail_requests() {
        gtk::init().unwrap();
        let runtime = Arc::new(tokio::runtime::Runtime::new().unwrap());
        let listener = runtime
            .block_on(tokio::net::TcpListener::bind("127.0.0.1:0"))
            .unwrap();
        let address = listener.local_addr().unwrap();
        let active = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let (started_tx, started_rx) = async_channel::unbounded();
        let (dropped_tx, dropped_rx) = async_channel::unbounded();
        let server = runtime.spawn({
            let active = Arc::clone(&active);
            let peak = Arc::clone(&peak);
            async move {
                let app = axum::Router::new().route(
                    "/api/streams/{camera}/{kind}/keyframe",
                    axum::routing::get(move || {
                        let active = Arc::clone(&active);
                        let peak = Arc::clone(&peak);
                        let started = started_tx.clone();
                        let dropped = dropped_tx.clone();
                        async move {
                            let count = active.fetch_add(1, Ordering::SeqCst) + 1;
                            peak.fetch_max(count, Ordering::SeqCst);
                            started.send(()).await.unwrap();
                            axum::http::Response::builder()
                                .header(axum::http::header::CONTENT_TYPE, "video/mp4")
                                .body(axum::body::Body::from_stream(PendingBody {
                                    _guard: RequestGuard { active, dropped },
                                }))
                                .unwrap()
                        }
                    }),
                );
                axum::serve(listener, app).await.unwrap();
            }
        });
        let client = Arc::new(Client::new(
            url::Url::parse(&format!("http://{address}")).unwrap(),
            None,
        ));
        let view = Playback::new("fixture", "main", "sub", &[], thumbnail_decoder_pool());
        let old_generation = view.generation.get();
        let segments = [crate::api::Segment {
            wall_start_us: 0,
            wall_end_us: 12_000_000,
            sealed: true,
        }];
        view.load_thumbnails(&client, &runtime, 0, 12_000_000, &segments);
        glib::MainContext::default().block_on(async {
            let started = async {
                for _ in 0..THUMBNAIL_CONCURRENCY {
                    started_rx.recv().await.unwrap();
                }
            };
            let timeout = glib::timeout_future(Duration::from_secs(2));
            futures::pin_mut!(started, timeout);
            assert!(
                matches!(
                    futures::future::select(started, timeout).await,
                    futures::future::Either::Left(_)
                ),
                "the bounded thumbnail requests must start"
            );
            view.close();
            let dropped = async {
                for _ in 0..THUMBNAIL_CONCURRENCY {
                    dropped_rx.recv().await.unwrap();
                }
            };
            let timeout = glib::timeout_future(Duration::from_secs(2));
            futures::pin_mut!(dropped, timeout);
            assert!(
                matches!(
                    futures::future::select(dropped, timeout).await,
                    futures::future::Either::Left(_)
                ),
                "closing playback must drop all active keyframe responses"
            );
        });
        assert_eq!(active.load(Ordering::SeqCst), 0);
        assert_eq!(peak.load(Ordering::SeqCst), THUMBNAIL_CONCURRENCY);
        assert!(view.thumbnails.borrow().is_none());
        assert!(view.thumbnail_receiver.borrow().is_none());
        let stale = thumbs::Image {
            width: 1,
            height: 1,
            stride: 3,
            rgb: vec![0; 3],
        };
        assert!(!view.show_thumbnail(old_generation, 0, &stale));
        assert!(view.thumb_pictures[0].paintable().is_none());
        server.abort();
    }

    fn replacement_views_share_uninterruptible_decoder_permits() {
        gtk::init().unwrap();
        gst::init().unwrap();
        gstgtk4::plugin_register_static().unwrap();
        let mut decode_gate = DecodeGate::new();

        let runtime = Arc::new(tokio::runtime::Runtime::new().unwrap());
        let listener = runtime
            .block_on(tokio::net::TcpListener::bind("127.0.0.1:0"))
            .unwrap();
        let address = listener.local_addr().unwrap();
        let keyframe_fetches = Arc::new(AtomicUsize::new(0));
        let server = runtime.spawn({
            let keyframe_fetches = Arc::clone(&keyframe_fetches);
            async move {
                axum::serve(listener, replacement_fixture(keyframe_fetches))
                    .await
                    .unwrap();
            }
        });
        let client = Arc::new(Client::new(
            url::Url::parse(&format!("http://{address}")).unwrap(),
            None,
        ));
        let pool = thumbnail_decoder_pool();
        let first = Playback::new("fixture-one", "main", "sub", &[], Arc::clone(&pool));
        let second = Playback::new("fixture-two", "main", "sub", &[], Arc::clone(&pool));
        let third = Playback::new("fixture-three", "main", "sub", &[], Arc::clone(&pool));

        glib::MainContext::default().block_on(async {
            first
                .open(Arc::clone(&client), Arc::clone(&runtime), 0, 12_000_000)
                .await;
            wait_for_atomic_at_least(&DECODER_ADMISSION_ATTEMPTS, 2, Duration::from_secs(2)).await;
            wait_for_decoder_count(2, Duration::from_secs(2)).await;
            assert_eq!(keyframe_fetches.load(Ordering::SeqCst), 2);
            assert_eq!(pool.available_permits(), 0);

            let stale_generation = first.generation.get();
            first.close();
            second
                .open(Arc::clone(&client), Arc::clone(&runtime), 0, 12_000_000)
                .await;
            wait_for_atomic_at_least(&DECODER_ADMISSION_ATTEMPTS, 4, Duration::from_secs(2)).await;
            second.close();
            third
                .open(Arc::clone(&client), Arc::clone(&runtime), 0, 12_000_000)
                .await;
            wait_for_atomic_at_least(&DECODER_ADMISSION_ATTEMPTS, 6, Duration::from_secs(2)).await;
            third.close();

            runtime
                .spawn(async {
                    for _ in 0..16 {
                        tokio::task::yield_now().await;
                    }
                })
                .await
                .unwrap();
            assert_eq!(ACTIVE_DECODERS.load(Ordering::SeqCst), 2);
            assert_eq!(PEAK_DECODERS.load(Ordering::SeqCst), 2);
            assert_eq!(pool.available_permits(), 0);
            assert_eq!(
                keyframe_fetches.load(Ordering::SeqCst),
                2,
                "cancelled permit waiters must not fetch keyframe bodies"
            );
            let stale = thumbs::Image {
                width: 1,
                height: 1,
                stride: 3,
                rgb: vec![0; 3],
            };
            assert!(!first.show_thumbnail(stale_generation, 0, &stale));

            decode_gate.release();
            wait_for_decoder_count(0, Duration::from_secs(2)).await;
            glib::timeout_future(Duration::from_millis(100)).await;
            assert_eq!(pool.available_permits(), THUMBNAIL_CONCURRENCY);
            assert_eq!(PEAK_DECODERS.load(Ordering::SeqCst), 2);
            assert_eq!(
                keyframe_fetches.load(Ordering::SeqCst),
                2,
                "cancelled permit waiters must not fetch after permits are released"
            );
        });
        drop(decode_gate);
        server.abort();
    }

    fn replacement_fixture(keyframe_fetches: Arc<AtomicUsize>) -> axum::Router {
        axum::Router::new()
            .route(
                "/api/streams/{camera}/{kind}/segments",
                axum::routing::get(|| async {
                    r#"[{"wall_start_us":0,"wall_end_us":12000000,"sealed":true}]"#
                }),
            )
            .route("/api/events", axum::routing::get(|| async { "[]" }))
            .route(
                "/api/streams/{camera}/{kind}/hls/vod.m3u8",
                axum::routing::get(|| async {
                    axum::http::Response::builder()
                        .header(
                            axum::http::header::CONTENT_TYPE,
                            "application/vnd.apple.mpegurl",
                        )
                        .body(axum::body::Body::from(concat!(
                            "#EXTM3U\n",
                            "#EXT-X-VERSION:7\n",
                            "#EXT-X-TARGETDURATION:12\n",
                            "#EXT-X-PLAYLIST-TYPE:VOD\n",
                            "#EXT-X-MAP:URI=\"/init.mp4\"\n",
                            "#IRIS-FRAGMENT:WALL-US=0,DURATION-TICKS=1080000\n",
                            "#EXTINF:12.00000,\n",
                            "/media.m4s\n",
                            "#EXT-X-ENDLIST\n",
                        )))
                        .unwrap()
                }),
            )
            .route(
                "/api/streams/{camera}/{kind}/keyframe",
                axum::routing::get(
                    |axum::extract::State(fetches): axum::extract::State<Arc<AtomicUsize>>| async move {
                        fetches.fetch_add(1, Ordering::SeqCst);
                        axum::http::Response::builder()
                            .header(axum::http::header::CONTENT_TYPE, "video/mp4")
                            .body(axum::body::Body::from(vec![0; 4096]))
                            .unwrap()
                    },
                ),
            )
            .route(
                "/{*media}",
                axum::routing::get(|| async {
                    let stream =
                        futures::stream::pending::<Result<axum::body::Bytes, Infallible>>();
                    axum::http::Response::builder()
                        .header(axum::http::header::CONTENT_TYPE, "video/mp4")
                        .body(axum::body::Body::from_stream(stream))
                        .unwrap()
                }),
            )
            .with_state(keyframe_fetches)
    }

    async fn wait_for_atomic_at_least(value: &AtomicUsize, expected: usize, limit: Duration) {
        let reached = async {
            while value.load(Ordering::SeqCst) < expected {
                glib::timeout_future(Duration::from_millis(5)).await;
            }
        };
        let timeout = glib::timeout_future(limit);
        futures::pin_mut!(reached, timeout);
        assert!(
            matches!(
                futures::future::select(reached, timeout).await,
                futures::future::Either::Left(_)
            ),
            "counter did not reach {expected}; current={}",
            value.load(Ordering::SeqCst)
        );
    }

    async fn wait_for_decoder_count(expected: usize, limit: Duration) {
        let reached = async {
            while ACTIVE_DECODERS.load(Ordering::SeqCst) != expected {
                glib::timeout_future(Duration::from_millis(5)).await;
            }
        };
        let timeout = glib::timeout_future(limit);
        futures::pin_mut!(reached, timeout);
        assert!(
            matches!(
                futures::future::select(reached, timeout).await,
                futures::future::Either::Left(_)
            ),
            "decoder count did not reach {expected}; active={} peak={}",
            ACTIVE_DECODERS.load(Ordering::SeqCst),
            PEAK_DECODERS.load(Ordering::SeqCst)
        );
    }

    fn export_terminal_status_cannot_be_overwritten_by_progress() {
        const CHUNKS: usize = 4096;

        gtk::init().unwrap();
        let runtime = Arc::new(tokio::runtime::Runtime::new().unwrap());
        let listener = runtime
            .block_on(tokio::net::TcpListener::bind("127.0.0.1:0"))
            .unwrap();
        let address = listener.local_addr().unwrap();
        let server = runtime.spawn(async move {
            let app = axum::Router::new().route(
                "/api/streams/fixture/main/export.mp4",
                axum::routing::get(|| async {
                    let chunks = futures::stream::iter(
                        (0..CHUNKS)
                            .map(|_| Ok::<_, Infallible>(axum::body::Bytes::from_static(b"x"))),
                    );
                    axum::http::Response::builder()
                        .header(axum::http::header::CONTENT_TYPE, "video/mp4")
                        .body(axum::body::Body::from_stream(chunks))
                        .unwrap()
                }),
            );
            axum::serve(listener, app).await.unwrap();
        });
        let client = Arc::new(Client::new(
            url::Url::parse(&format!("http://{address}")).unwrap(),
            None,
        ));
        let view = Playback::new("fixture", "main", "sub", &[], thumbnail_decoder_pool());
        *view.client.borrow_mut() = Some(client);
        *view.runtime.borrow_mut() = Some(Arc::clone(&runtime));
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("export.mp4");
        glib::MainContext::default().block_on(async {
            Rc::clone(&view).export_to(path.clone(), 1, 2).await;
            glib::timeout_future(Duration::from_millis(25)).await;
        });
        assert_eq!(std::fs::metadata(&path).unwrap().len(), CHUNKS as u64);
        assert!(
            view.status.text().starts_with("exported "),
            "terminal status must remain after progress producers finish: {}",
            view.status.text()
        );
        assert!(view.exporting.borrow().is_none());
        view.close();
        server.abort();
    }

    fn playback_error_releases_its_snapshot_listener() {
        gtk::init().unwrap();
        gst::init().unwrap();
        gstgtk4::plugin_register_static().unwrap();
        let runtime = Arc::new(tokio::runtime::Runtime::new().unwrap());
        let listener = runtime
            .block_on(tokio::net::TcpListener::bind("127.0.0.1:0"))
            .unwrap();
        let address = listener.local_addr().unwrap();
        let server = runtime.spawn(async move {
            axum::serve(listener, axum::Router::new()).await.unwrap();
        });
        let snapshot = runtime
            .block_on(Snapshot::new(format!(
                "#EXTM3U\n#EXT-X-VERSION:7\n#EXT-X-TARGETDURATION:2\n\
                 #EXT-X-PLAYLIST-TYPE:VOD\n#EXT-X-MAP:URI=\"http://{address}/missing.mp4\"\n\
                 #EXTINF:2.00000,\nhttp://{address}/missing.m4s\n#EXT-X-ENDLIST\n"
            )))
            .unwrap();
        let url = snapshot.url().clone();
        let view = Playback::new("fixture", "main", "sub", &[], thumbnail_decoder_pool());
        view.start(snapshot).unwrap();
        let listener_closed = glib::MainContext::default().block_on(async {
            let error = async {
                while !view.title.text().starts_with("playback error:") {
                    glib::timeout_future(Duration::from_millis(10)).await;
                }
            };
            let timeout = glib::timeout_future(Duration::from_secs(10));
            futures::pin_mut!(error, timeout);
            assert!(
                matches!(
                    futures::future::select(error, timeout).await,
                    futures::future::Either::Left(_)
                ),
                "missing media must produce a playback error"
            );
            runtime
                .spawn(async move {
                    let http = reqwest::Client::new();
                    tokio::time::timeout(Duration::from_secs(1), async {
                        while http.get(url.clone()).send().await.is_ok() {
                            tokio::task::yield_now().await;
                        }
                    })
                    .await
                    .is_ok()
                })
                .await
                .unwrap()
        });
        let stopped = view.pipeline.borrow().is_none();
        view.close();
        server.abort();
        assert!(
            listener_closed,
            "playback failure must close its snapshot listener"
        );
        assert!(stopped, "playback failure must release its pipeline");
    }

    fn a_window_holding_only_a_stale_open_segment_shows_nothing_recorded() {
        gtk::init().unwrap();
        let runtime = Arc::new(tokio::runtime::Runtime::new().unwrap());
        let listener = runtime
            .block_on(tokio::net::TcpListener::bind("127.0.0.1:0"))
            .unwrap();
        let address = listener.local_addr().unwrap();
        let vod_requests = Arc::new(AtomicUsize::new(0));
        let server = runtime.spawn({
            let vod_requests = Arc::clone(&vod_requests);
            async move {
                let app = axum::Router::new()
                    .route(
                        "/api/streams/fixture/main/segments",
                        axum::routing::get(|| async {
                            // Opened 3 h before the window's end, last progress 2 h before it.
                            r#"[{"wall_start_us":-7200000000,"wall_end_us":-3600000000,"sealed":false}]"#
                        }),
                    )
                    .route("/api/events", axum::routing::get(|| async { "[]" }))
                    .route(
                        "/api/streams/fixture/main/hls/vod.m3u8",
                        axum::routing::get(move || {
                            vod_requests.fetch_add(1, Ordering::SeqCst);
                            async { axum::http::StatusCode::NOT_FOUND }
                        }),
                    );
                axum::serve(listener, app).await.unwrap();
            }
        });
        let client = Arc::new(Client::new(
            url::Url::parse(&format!("http://{address}")).unwrap(),
            None,
        ));
        let view = Playback::new("fixture", "main", "sub", &[], thumbnail_decoder_pool());
        glib::MainContext::default().block_on(async {
            let opening = view.open(client, Arc::clone(&runtime), 0, 3_600_000_000);
            let timeout = glib::timeout_future(Duration::from_secs(5));
            futures::pin_mut!(opening, timeout);
            assert!(
                matches!(
                    futures::future::select(opening, timeout).await,
                    futures::future::Either::Left(_)
                ),
                "the window must load before the deadline"
            );
        });
        assert_eq!(
            view.title.text(),
            "fixture: nothing recorded in this window"
        );
        assert_eq!(vod_requests.load(Ordering::SeqCst), 0);
        view.close();
        server.abort();
    }

    #[test]
    fn export_names_carry_camera_kind_and_utc_start() {
        assert_eq!(
            export_name("cam1", "main", 1_793_571_648_056_937),
            "cam1-main-20261101T222048Z.mp4"
        );
    }

    #[test]
    fn export_progress_retains_only_the_latest_value() {
        let (tx, mut rx) = export_progress_channel();
        for written in 1..=100_000 {
            tx.send_replace(written);
        }
        assert_eq!(*rx.borrow_and_update(), 100_000);
        assert!(!rx.has_changed().unwrap());
    }
}
