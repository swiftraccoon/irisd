//! The playback view: one camera's recordings over a window, played from the
//! daemon's HLS VOD playlist through `playbin3`, with the timeline for seeking.

use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::rc::{Rc, Weak};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use futures::StreamExt as _;
use gst::prelude::*;
use gtk::prelude::*;
use gtk::{gdk, glib};
use iris_client::api::{ApiError, EventRecord, Retention, Segment, Window};
use iris_client::playback::{PlaybackSelection, ThumbnailTime, export_name, thumbnail_times};
use iris_client::timeline_model::{PlaylistOffset, TimelineModel};
use iris_client::vod::PlaybackFragment;
use iris_core::id::{CameraId, StreamKind};
use iris_core::time::WallUs;
use iris_transport::Client;
use tokio::runtime::Runtime;
use tokio::sync::{OwnedSemaphorePermit, Semaphore, watch};
use tokio::task::AbortHandle;
use tokio_util::task::AbortOnDropHandle;
use tracing::{info, warn};

use crate::snapshot::Snapshot;
use crate::thumbs;
use crate::timeline::{self, MICROS_PER_SECOND, Timeline};

/// The video sink handed to `playbin3`; see [`Playback::start`].
const VIDEO_SINK: &str = "glupload ! glcolorconvert ! gtk4paintablesink name=sink sync=true";

/// How often the playhead follows the pipeline position.
const POSITION_PERIOD: Duration = Duration::from_millis(250);
/// Most events drawn on the timeline.
const EVENT_LIMIT: usize = 2000;
/// How close an event must be to a seek target to be named in the title, or
/// to the playhead to be kept.
const EVENT_NEAR: Duration = Duration::from_secs(5);
/// How long "Keep event" keeps an event's footage, in days.
const KEEP_EVENT_DAYS: i64 = 30;
/// Microseconds per day.
const MICROS_PER_DAY: i64 = 86_400 * MICROS_PER_SECOND;
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

fn export_progress_channel() -> (watch::Sender<u64>, watch::Receiver<u64>) {
    watch::channel(0)
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
        .expect("the decoder gate is never held across a panic");
    while !*released {
        released = changed
            .wait(released)
            .expect("the decoder gate is never held across a panic");
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

/// Which end of the export range a mark sets.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mark {
    /// The start of the range.
    In,
    /// The end of the range.
    Out,
}

/// The playback view for one camera.
#[derive(Debug)]
pub struct Playback {
    camera: CameraId,
    kind: StreamKind,
    thumb_kind: StreamKind,
    widget: gtk::Box,
    thumb_pictures: Vec<gtk::Picture>,
    picture: gtk::Picture,
    title: gtk::Label,
    status: gtk::Label,
    /// The "Sound" button: shown while the window plays with recorded
    /// audio, and muting the player while it is off.
    sound: gtk::ToggleButton,
    /// The window, its recordings, the playhead, and the export marks.
    timeline: Timeline,
    thumbnail_decoders: Arc<Semaphore>,
    pipeline: RefCell<Option<Running>>,
    opening: RefCell<Option<AbortHandle>>,
    thumbnails: RefCell<Option<AbortHandle>>,
    thumbnail_receiver: RefCell<Option<glib::JoinHandle<()>>>,
    exporting: RefCell<Option<AbortHandle>>,
    export_generation: Cell<u64>,
    generation: Cell<u64>,
    client: RefCell<Option<Client>>,
    runtime: RefCell<Option<Arc<Runtime>>>,
}

#[derive(Debug)]
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
        camera: &CameraId,
        kind: StreamKind,
        thumb_kind: StreamKind,
        extra: &[gtk::Button],
        thumbnail_decoders: Arc<Semaphore>,
    ) -> Rc<Self> {
        let header = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        for button in extra {
            header.append(button);
        }
        let play = gtk::Button::with_label("Play / Pause");
        header.append(&play);
        // Shown while the window plays with recorded audio; off mutes it.
        let sound = gtk::ToggleButton::with_label("Sound");
        sound.set_active(true);
        sound.set_visible(false);
        header.append(&sound);
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
        let this = Rc::new_cyclic(|weak: &Weak<Self>| {
            let weak = weak.clone();
            let timeline = Timeline::new(move |time| {
                if let Some(view) = weak.upgrade() {
                    view.seek(time);
                }
            });
            widget.append(timeline.widget());
            Self {
                camera: camera.clone(),
                kind,
                thumb_kind,
                widget,
                thumb_pictures,
                picture,
                title,
                status,
                sound: sound.clone(),
                timeline,
                thumbnail_decoders,
                pipeline: RefCell::new(None),
                opening: RefCell::new(None),
                thumbnails: RefCell::new(None),
                thumbnail_receiver: RefCell::new(None),
                exporting: RefCell::new(None),
                export_generation: Cell::new(0),
                generation: Cell::new(0),
                client: RefCell::new(None),
                runtime: RefCell::new(None),
            }
        });
        Self::connect_buttons(&this, [play, mark_in, mark_out, keep, export]);
        sound.connect_toggled({
            let weak = Rc::downgrade(&this);
            move |button| {
                if let Some(this) = weak.upgrade()
                    && let Some(running) = this.pipeline.borrow().as_ref()
                {
                    running.playbin.set_property("mute", !button.is_active());
                }
            }
        });
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
        for (button, which) in [(mark_in, Mark::In), (mark_out, Mark::Out)] {
            button.connect_clicked({
                let weak = Rc::downgrade(this);
                move |_| {
                    if let Some(this) = weak.upgrade() {
                        this.mark(which);
                    }
                }
            });
        }
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

    /// Sets one end of the export range at the playhead, or at the window's
    /// edge when nothing has played yet.
    fn mark(&self, which: Mark) {
        let (mark_in, mark_out) = self.timeline.with_model(|model| match which {
            Mark::In => (Some(model.playhead.unwrap_or(model.from)), model.mark_out),
            Mark::Out => (model.mark_in, Some(model.playhead.unwrap_or(model.to))),
        });
        self.timeline.set_marks(mark_in, mark_out);
        self.status.set_label(&marks_status(mark_in, mark_out));
    }

    /// Keeps the event nearest to the playhead (within [`EVENT_NEAR`]) from
    /// retention for [`KEEP_EVENT_DAYS`] days, or clears the mark of one that
    /// is already kept.
    async fn keep_nearest_event(self: Rc<Self>) {
        let nearest = self.timeline.with_model(|model| {
            model
                .playhead
                .map(|time| model.nearest_event(time, EVENT_NEAR).cloned())
        });
        let Some(nearest) = nearest else {
            self.status.set_label("play or seek to an event first");
            return;
        };
        let Some(event) = nearest else {
            self.status.set_label(&format!(
                "no event within {} s of the playhead",
                EVENT_NEAR.as_secs()
            ));
            return;
        };
        let (client, runtime) = (self.client.borrow().clone(), self.runtime.borrow().clone());
        let (Some(client), Some(runtime)) = (client, runtime) else {
            return;
        };
        let retention = match event.retain_until {
            Some(_) => Retention::Release,
            None => Retention::Until(
                WallUs::new(glib::real_time()).offset(KEEP_EVENT_DAYS * MICROS_PER_DAY),
            ),
        };
        let task = runtime.spawn(async move {
            let updated = client.retain_event(event.id, retention).await;
            updated.map_err(anyhow::Error::from)
        });
        match task.await {
            Ok(Ok(updated)) => {
                self.status.set_label(&format!(
                    "{} at {}: {}",
                    updated.kind,
                    timeline::clock_label(updated.nvr_time),
                    if updated.retain_until.is_some() {
                        format!("kept for {KEEP_EVENT_DAYS} days")
                    } else {
                        "no longer kept".to_owned()
                    }
                ));
                self.timeline.replace_event(updated);
            }
            Ok(Err(error)) => self.status.set_label(&format!("keep failed: {error:#}")),
            Err(error) => self.status.set_label(&format!("keep task failed: {error}")),
        }
    }

    /// The export range of the timeline's marks, as [`export_window`] gives
    /// it.
    #[must_use]
    fn export_range(&self) -> Option<Result<Window, ApiError>> {
        self.timeline
            .with_model(|model| export_window(model.mark_in, model.mark_out))
    }

    /// Asks where to save, then exports the marked range there. A range longer
    /// than seven days is refused in the status line before any dialog opens.
    /// The refusal starts no export and cancels none: a running export goes
    /// on, and its progress replaces the refusal in the status line.
    async fn export_dialog(self: Rc<Self>, window: Option<&gtk::Window>) {
        let range = match self.export_range() {
            Some(Ok(range)) => range,
            Some(Err(refused)) => {
                warn!(camera = %self.camera, error = %refused, "export failed");
                self.status.set_label(&format!("export failed: {refused}"));
                return;
            }
            None => {
                self.status.set_label("set the in and out marks first");
                return;
            }
        };
        let dialog = gtk::FileDialog::builder()
            .title("Export recording")
            .initial_name(export_name(&self.camera, self.kind, range.from()))
            .build();
        match dialog.save_future(window).await {
            Ok(file) => match file.path() {
                Some(path) => self.export_to(path, range).await,
                None => self
                    .status
                    .set_label("export needs a local file; the chosen location has no path"),
            },
            Err(error) if error.matches(gtk::DialogError::Dismissed) => info!("export cancelled"),
            Err(error) => {
                warn!(camera = %self.camera, %error, "the save dialog failed");
                self.status
                    .set_label(&format!("the save dialog failed: {error}"));
            }
        }
    }

    /// Exports `range` to `path`, reporting progress in the status line.
    pub async fn export_to(self: Rc<Self>, path: PathBuf, range: Window) {
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
        let (camera, kind) = (self.camera.clone(), self.kind);
        let target = path.clone();
        let task = AbortOnDropHandle::new(runtime.spawn(async move {
            client
                .export(&camera, kind, range, &target, |written| {
                    tx.send_replace(written);
                })
                .await
                .map_err(anyhow::Error::from)
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
                info!(camera = %self.camera, path = %path.display(), bytes, "export done");
                self.status.set_label(&format!(
                    "exported {} MiB to {}",
                    bytes >> 20,
                    path.display()
                ));
            }
            Ok(Err(error)) => {
                warn!(
                    camera = %self.camera,
                    error = %format!("{error:#}"),
                    "export failed"
                );
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

    /// Loads `window`: the timeline from the segment and event lists, and the
    /// player from the VOD playlist. Runs on the main loop; the HTTP requests
    /// run on `runtime`.
    pub async fn open(self: &Rc<Self>, client: Client, runtime: Arc<Runtime>, window: Window) {
        self.close();
        let generation = self.generation.get();
        // The previous window's playhead and marks go at once, so an export
        // started while this window loads cannot reuse them.
        self.timeline.set_model(TimelineModel {
            from: window.from(),
            to: window.to(),
            ..TimelineModel::default()
        });
        *self.client.borrow_mut() = Some(client.clone());
        *self.runtime.borrow_mut() = Some(Arc::clone(&runtime));
        self.title.set_label(&format!(
            "{} {} stream, {} to {}",
            self.camera,
            self.kind,
            timeline::clock_label(window.from()),
            timeline::clock_label(window.to())
        ));
        let lists = AbortOnDropHandle::new(runtime.spawn(fetch_window(
            client.clone(),
            self.camera.clone(),
            self.kind,
            window,
        )));
        *self.opening.borrow_mut() = Some(lists.abort_handle());
        let loaded = lists.await;
        if generation != self.generation.get() {
            return;
        }
        self.opening.borrow_mut().take();
        let lists = match loaded {
            Ok(lists) => lists,
            Err(error) => {
                self.title.set_label(&format!("listing failed: {error}"));
                return;
            }
        };
        let Some((segments, snapshot)) = self.show_window(lists, window) else {
            return;
        };
        self.load_thumbnails(&client, &runtime, window, &segments);
        let Some(snapshot) = snapshot else {
            return;
        };
        if let Err(error) = self.start(snapshot) {
            self.title.set_label(&format!("{error:#}"));
        }
    }

    /// Puts the fetched window on the timeline and returns its recorded
    /// segments with the snapshot to play, or `None`, with the reason in the
    /// title, when nothing can play.
    fn show_window(
        &self,
        lists: WindowLists,
        window: Window,
    ) -> Option<(Vec<Segment>, Option<Snapshot>)> {
        let mut segments = match lists.segments {
            Ok(segments) => segments,
            Err(error) => {
                self.title.set_label(&format!("{error:#}"));
                return None;
            }
        };
        segments.sort_by_key(|s| s.start);
        let events = lists.events.unwrap_or_else(|error| {
            warn!(
                camera = %self.camera,
                error = %format!("{error:#}"),
                "events unavailable"
            );
            Vec::new()
        });
        let playback = match lists.playback {
            Ok(playback) => playback,
            Err(error) => {
                warn!(
                    camera = %self.camera,
                    error = %format!("{error:#}"),
                    "playback timing unavailable"
                );
                self.title
                    .set_label(&format!("playback timing unavailable: {error:#}"));
                return None;
            }
        };
        info!(
            camera = %self.camera,
            segments = segments.len(),
            events = events.len(),
            "window loaded"
        );
        let (fragments, snapshot) = playback.map_or((Vec::new(), None), |(fragments, snapshot)| {
            (fragments, Some(snapshot))
        });
        self.timeline.set_model(TimelineModel {
            from: window.from(),
            to: window.to(),
            segments: segments.clone(),
            playback: fragments,
            events,
            playhead: None,
            mark_in: None,
            mark_out: None,
        });
        if segments.is_empty() {
            self.title
                .set_label(&format!("{}: nothing recorded in this window", self.camera));
            return None;
        }
        Some((segments, snapshot))
    }

    /// Fetches and decodes one keyframe per thumbnail slot whose time is
    /// recorded, on the runtime, and shows each as it arrives.
    fn load_thumbnails(
        self: &Rc<Self>,
        client: &Client,
        runtime: &Runtime,
        window: Window,
        segments: &[Segment],
    ) {
        self.stop_thumbnails();
        for picture in &self.thumb_pictures {
            picture.set_paintable(None::<&gdk::Paintable>);
        }
        let generation = self.generation.get();
        let (tx, rx) = async_channel::bounded::<(usize, thumbs::Image)>(THUMBNAIL_CONCURRENCY);
        let jobs = thumbnail_times(window, segments, THUMBNAILS);
        let (camera, kind) = (self.camera.clone(), self.thumb_kind);
        let thumbnail_decoders = Arc::clone(&self.thumbnail_decoders);
        let task = runtime.spawn({
            let client = client.clone();
            async move {
                futures::stream::iter(jobs)
                    .for_each_concurrent(THUMBNAIL_CONCURRENCY, |ThumbnailTime { slot, time }| {
                        let client = client.clone();
                        let thumbnail_decoders = Arc::clone(&thumbnail_decoders);
                        let tx = tx.clone();
                        let camera = camera.clone();
                        async move {
                            #[cfg(test)]
                            DECODER_ADMISSION_ATTEMPTS
                                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                            let Ok(permit) = thumbnail_decoders.acquire_owned().await else {
                                return;
                            };
                            let keyframe = client.keyframe(&camera, kind, time).await;
                            let bytes = match keyframe.map_err(anyhow::Error::from) {
                                Ok(Some(bytes)) => bytes,
                                Ok(None) => return,
                                Err(error) => {
                                    warn!(
                                        camera = %camera,
                                        error = %format!("{error:#}"),
                                        slot,
                                        "keyframe fetch failed"
                                    );
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
                                    warn!(
                                        camera = %camera,
                                        error = %format!("{error:#}"),
                                        slot,
                                        "keyframe decode failed"
                                    );
                                }
                                Err(error) => {
                                    warn!(
                                        camera = %camera,
                                        %error,
                                        slot,
                                        "keyframe decode task failed"
                                    );
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
                if view.show_thumbnail(generation, slot, image) {
                    shown += 1;
                }
            }
            info!(shown, "thumbnails loaded");
        });
        *self.thumbnail_receiver.borrow_mut() = Some(receiver);
    }

    fn show_thumbnail(&self, generation: u64, slot: usize, image: thumbs::Image) -> bool {
        if generation != self.generation.get() {
            return false;
        }
        let Some(picture) = self.thumb_pictures.get(slot) else {
            return false;
        };
        picture.set_paintable(Some(&image.into_texture()));
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
        let sink_bin =
            gst::parse::bin_from_description(VIDEO_SINK, /* ghost_unlinked_pads */ true)
                .context("building the video sink")?;
        let sink = sink_bin.by_name("sink").context("sink missing")?;
        let paintable = sink.property::<gdk::Paintable>("paintable");
        self.picture.set_paintable(Some(&paintable));
        playbin.set_property("video-sink", &sink_bin);
        if crate::sound::silent() {
            let silent = gst::ElementFactory::make("fakesink")
                .property("sync", true)
                .build()
                .context("creating the silent audio sink")?;
            playbin.set_property("audio-sink", &silent);
        }
        self.sound.set_visible(snapshot.has_audio());
        playbin.set_property("mute", !self.sound.is_active());
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
        if let Err(error) = playbin.set_state(gst::State::Playing) {
            // A failed start can leave the elements between states; NULL
            // releases them before the last reference goes.
            if let Err(stop) = playbin.set_state(gst::State::Null) {
                warn!(
                    camera = %self.camera,
                    error = %stop,
                    "cannot stop a player that failed to start"
                );
            }
            return Err(error).context("starting playback");
        }
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
                    // The player's nanoseconds, which the model reads in whole
                    // microseconds rounded down.
                    let offset = PlaylistOffset::new(Duration::from(position));
                    let time = view.timeline.with_model(|model| model.wall_time(offset));
                    view.timeline.set_playhead(Some(time));
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
    fn seek(&self, time: WallUs) {
        let (offset, near, from, to) = self.timeline.with_model(|model| {
            (
                model.playlist_offset(time),
                timeline::nearest_event_label(model, time, EVENT_NEAR),
                model.from,
                model.to,
            )
        });
        // The player's clock counts nanoseconds and holds the offset's whole
        // microseconds exactly. A playlist of at most
        // `iris_client::vod::MAX_PLAYBACK_FRAGMENTS` fragments of 32-bit ticks
        // ends inside the clock's range; past it, the target is the clock's
        // end.
        let target = gst::ClockTime::try_from(offset.get()).unwrap_or(gst::ClockTime::MAX);
        if let Some(running) = self.pipeline.borrow().as_ref() {
            if let Err(error) = running
                .playbin
                .seek_simple(gst::SeekFlags::FLUSH | gst::SeekFlags::KEY_UNIT, target)
            {
                warn!(camera = %self.camera, %error, "seek failed");
            } else {
                info!(
                    camera = %self.camera,
                    time = %timeline::clock_label(time),
                    offset_s = target.seconds(),
                    "seek"
                );
            }
        }
        self.timeline.set_playhead(Some(time));
        let near = near.map_or_else(String::new, |event| format!(", {event}"));
        self.title.set_label(&format!(
            "{} {} stream, {} to {}, at {}{near}",
            self.camera,
            self.kind,
            timeline::clock_label(from),
            timeline::clock_label(to),
            timeline::clock_label(time)
        ));
    }

    /// Toggles between playing and paused; a refused change shows in the
    /// status line.
    fn toggle(&self) {
        let pipeline = self.pipeline.borrow();
        let Some(running) = pipeline.as_ref() else {
            return;
        };
        let (_, state, _) = running.playbin.state(gst::ClockTime::ZERO);
        let (next, verb) = if state == gst::State::Playing {
            (gst::State::Paused, "pause")
        } else {
            (gst::State::Playing, "play")
        };
        if let Err(error) = running.playbin.set_state(next) {
            warn!(camera = %self.camera, %error, verb, "the player refused the change");
            self.status.set_label(&format!("cannot {verb}: {error}"));
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
            if let Err(error) = running.playbin.set_state(gst::State::Null) {
                warn!(camera = %self.camera, %error, "cannot stop the player");
            }
        }
        self.client.borrow_mut().take();
        self.runtime.borrow_mut().take();
    }
}

/// What a playback window needs from the daemon, fetched together on the runtime.
struct WindowLists {
    segments: anyhow::Result<Vec<Segment>>,
    events: anyhow::Result<Vec<EventRecord>>,
    /// The selection's fragment timing and the snapshot serving its bodies;
    /// `None` when nothing is recorded in the window.
    playback: anyhow::Result<Option<(Vec<PlaybackFragment>, Snapshot)>>,
}

/// Lists the segments of the `kind` stream of `camera` in `window` and the
/// camera's events in it. Unless the listing shows nothing recorded, it also
/// fetches the window's VOD playlist, with the audio playlist for a main
/// stream, as one selection, and starts the snapshot that serves its bodies.
async fn fetch_window(
    client: Client,
    camera: CameraId,
    kind: StreamKind,
    window: Window,
) -> WindowLists {
    let segments = client
        .segments(&camera, kind, window)
        .await
        .map_err(anyhow::Error::from);
    let events = client
        .events(&camera, window, EVENT_LIMIT)
        .await
        .map_err(anyhow::Error::from);
    let playback = if segments.as_ref().is_ok_and(Vec::is_empty) {
        Ok(None)
    } else {
        async {
            let playlist = client.vod_playlist(&camera, kind, window).await?;
            // The camera's audio lies on the main stream's timeline, so it
            // plays with that stream alone. A window without recorded audio
            // answers 404, and no fault of the audio keeps the picture away.
            let audio = if kind == StreamKind::Main {
                client
                    .vod_playlist(&camera, StreamKind::Audio, window)
                    .await
                    .ok()
            } else {
                None
            };
            let selection = PlaybackSelection::new(playlist, audio);
            let (fragments, bodies) = selection.into_parts();
            let snapshot = Snapshot::serve(bodies).await?;
            Ok(Some((fragments, snapshot)))
        }
        .await
    };
    WindowLists {
        segments,
        events,
        playback,
    }
}

/// The export range of the marks `mark_in` and `mark_out`. It is `None`
/// unless both are set and the out mark is after the in mark. Otherwise it is
/// the window between the marks, or the refusal of a range longer than
/// [`iris_client::api::MAX_RANGE_US`], seven days. A mark is set at the
/// playhead, or at the window's edge when nothing has played. The playhead
/// can lie outside the playback window, in a boundary fragment.
#[must_use]
fn export_window(
    mark_in: Option<WallUs>,
    mark_out: Option<WallUs>,
) -> Option<Result<Window, ApiError>> {
    match (mark_in, mark_out) {
        (Some(mark_in), Some(mark_out)) if mark_out > mark_in => {
            Some(Window::try_new(mark_in, mark_out))
        }
        _ => None,
    }
}

/// The status line after a mark changes.
fn marks_status(mark_in: Option<WallUs>, mark_out: Option<WallUs>) -> String {
    match (mark_in, mark_out) {
        (Some(i), Some(o)) if o > i => format!(
            "export range {} to {} ({} s)",
            timeline::clock_label(i),
            timeline::clock_label(o),
            (i128::from(o.get()) - i128::from(i.get())) / i128::from(MICROS_PER_SECOND)
        ),
        (Some(i), Some(_)) => format!(
            "out mark must be after the in mark ({})",
            timeline::clock_label(i)
        ),
        (Some(i), None) => format!("in mark at {}; set the out mark", timeline::clock_label(i)),
        (None, Some(o)) => format!("out mark at {}; set the in mark", timeline::clock_label(o)),
        (None, None) => String::new(),
    }
}

impl Drop for Playback {
    fn drop(&mut self) {
        self.close();
    }
}

/// Playback checks that need a display; `crate::tests::gtk_lifecycle_checks`
/// runs them on GTK's one initializer thread, and the `#[test]`s here run as
/// ordinary tests.
#[cfg(test)]
pub(crate) mod tests {
    use std::convert::Infallible;
    use std::net::SocketAddr;
    use std::pin::Pin;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::task::{Context, Poll};

    use axum::routing;
    use futures::future::Either;
    use iris_client::api::{MAX_RANGE_US, Rule};
    use iris_client::origin::{ApiOrigin, PlainHttp};
    use url::Url;

    use super::*;

    fn camera(id: &str) -> CameraId {
        CameraId::try_new(id).unwrap()
    }

    fn at(micros: i64) -> WallUs {
        WallUs::new(micros)
    }

    /// The window `[from, to)`, in microseconds.
    fn window_of(from: i64, to: i64) -> Window {
        Window::try_new(at(from), at(to)).unwrap()
    }

    /// A playback view of `id`'s main stream with sub-stream thumbnails.
    fn view_of(id: &str, pool: Arc<Semaphore>) -> Rc<Playback> {
        Playback::new(&camera(id), StreamKind::Main, StreamKind::Sub, &[], pool)
    }

    fn client_for(address: SocketAddr) -> Client {
        Client::new(
            ApiOrigin::parse(&format!("http://{address}"), PlainHttp::LoopbackOnly).unwrap(),
            None,
            Arc::unwrap_or_clone(iris_tls::client_config(None).unwrap()),
        )
    }

    fn bind(runtime: &Runtime) -> tokio::net::TcpListener {
        runtime
            .block_on(tokio::net::TcpListener::bind("127.0.0.1:0"))
            .unwrap()
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
                .expect("the decoder gate is never held across a panic") = false;
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
                .expect("the decoder gate is never held across a panic") = true;
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

    /// Waits on the main loop until `condition` holds or `limit` passes, and
    /// tells whether it held.
    async fn holds_within(condition: impl Fn() -> bool, limit: Duration) -> bool {
        let reached = async {
            while !condition() {
                glib::timeout_future(Duration::from_millis(5)).await;
            }
        };
        let timeout = glib::timeout_future(limit);
        futures::pin_mut!(reached, timeout);
        matches!(
            futures::future::select(reached, timeout).await,
            Either::Left(_)
        )
    }

    /// Waits on the main loop until `condition` holds, failing with `failure`
    /// after `limit`.
    async fn wait_until(condition: impl Fn() -> bool, limit: Duration, failure: &str) {
        assert!(holds_within(condition, limit).await, "{failure}");
    }

    pub(crate) fn dropping_playback_releases_the_view_and_button_callbacks() {
        gtk::init().unwrap();
        let view = view_of("fixture", thumbnail_decoder_pool());
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

    pub(crate) fn closing_playback_cancels_a_pending_playlist() {
        gtk::init().unwrap();
        let runtime = Arc::new(Runtime::new().unwrap());
        let listener = bind(&runtime);
        let address = listener.local_addr().unwrap();
        let (tx, rx) = async_channel::bounded(1);
        let server = runtime.spawn(async move {
            let app = axum::Router::new()
                .route(
                    "/api/streams/fixture/main/segments",
                    routing::get(|| async {
                        r#"[{"wall_start_us":0,"wall_end_us":1000000,"sealed":true}]"#
                    }),
                )
                .route("/api/events", routing::get(|| async { "[]" }))
                .route(
                    "/api/streams/fixture/main/hls/vod.m3u8",
                    routing::get(move || {
                        let tx = tx.clone();
                        async move {
                            tx.send(()).await.unwrap();
                            futures::future::pending::<String>().await
                        }
                    }),
                );
            axum::serve(listener, app).await.unwrap();
        });
        let client = client_for(address);
        let view = view_of("fixture", thumbnail_decoder_pool());
        glib::MainContext::default().block_on(async {
            let opening = glib::spawn_future_local({
                let view = Rc::clone(&view);
                let runtime = Arc::clone(&runtime);
                async move { view.open(client, runtime, window_of(0, 1_000_000)).await }
            });
            rx.recv().await.unwrap();
            view.close();
            let timeout = glib::timeout_future(Duration::from_secs(1));
            futures::pin_mut!(timeout);
            assert!(
                matches!(
                    futures::future::select(opening, timeout).await,
                    Either::Left(_)
                ),
                "closing playback must cancel its pending playlist request"
            );
        });
        assert!(view.pipeline.borrow().is_none());
        assert!(view.opening.borrow().is_none());
        server.abort();
    }

    pub(crate) fn closing_playback_cancels_bounded_thumbnail_requests() {
        gtk::init().unwrap();
        let runtime = Arc::new(Runtime::new().unwrap());
        let listener = bind(&runtime);
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
                    routing::get(move || {
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
        let client = client_for(address);
        let view = view_of("fixture", thumbnail_decoder_pool());
        let old_generation = view.generation.get();
        let segments = [Segment {
            start: at(0),
            end: at(12_000_000),
            sealed: true,
        }];
        view.load_thumbnails(&client, &runtime, window_of(0, 12_000_000), &segments);
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
                    Either::Left(_)
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
                    Either::Left(_)
                ),
                "closing playback must drop all active keyframe responses"
            );
        });
        assert_eq!(active.load(Ordering::SeqCst), 0);
        assert_eq!(peak.load(Ordering::SeqCst), THUMBNAIL_CONCURRENCY);
        assert!(view.thumbnails.borrow().is_none());
        assert!(view.thumbnail_receiver.borrow().is_none());
        assert!(!view.show_thumbnail(old_generation, 0, thumbs::Image::black(1, 1)));
        assert!(view.thumb_pictures[0].paintable().is_none());
        server.abort();
    }

    /// Readings of three playback views that share one decoder pool. Each
    /// view opens in turn and closes while its thumbnails wait for a permit.
    /// The first view's two decoders wait at a gate until all three close.
    struct Replacements {
        /// Keyframes fetched while the first view's decoders run.
        first_fetches: usize,
        /// Permits left while the first view's decoders run.
        first_permits: usize,
        /// Decoders running once the three views are closed.
        closed_decoders: usize,
        /// Permits left once the three views are closed.
        closed_permits: usize,
        /// Keyframes fetched once the three views are closed.
        closed_fetches: usize,
        /// Most decoders that ran at once up to the opening of the gate.
        closed_peak: usize,
        /// Whether the closed first view shows a thumbnail of its generation.
        stale_shown: bool,
        /// Whether every permit returned within two seconds of the decoders
        /// ending.
        permits_returned: bool,
        /// Most decoders that ran at once over the whole run, read once the
        /// permits returned.
        released_peak: usize,
        /// Keyframes fetched 100 ms after the permits returned.
        released_fetches: usize,
    }

    fn open_replacements() -> Replacements {
        gtk::init().unwrap();
        gst::init().unwrap();
        gstgtk4::plugin_register_static().unwrap();
        let mut decode_gate = DecodeGate::new();

        let runtime = Arc::new(Runtime::new().unwrap());
        let listener = bind(&runtime);
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
        let client = client_for(address);
        let pool = thumbnail_decoder_pool();
        let first = view_of("fixture-one", Arc::clone(&pool));
        let second = view_of("fixture-two", Arc::clone(&pool));
        let third = view_of("fixture-three", Arc::clone(&pool));
        let window = window_of(0, 12_000_000);
        let fetches = || keyframe_fetches.load(Ordering::SeqCst);

        let seen = glib::MainContext::default().block_on(async {
            first
                .open(client.clone(), Arc::clone(&runtime), window)
                .await;
            wait_for_atomic_at_least(&DECODER_ADMISSION_ATTEMPTS, 2, Duration::from_secs(2)).await;
            wait_for_decoder_count(2, Duration::from_secs(2)).await;
            let (first_fetches, first_permits) = (fetches(), pool.available_permits());

            let stale_generation = first.generation.get();
            first.close();
            second
                .open(client.clone(), Arc::clone(&runtime), window)
                .await;
            wait_for_atomic_at_least(&DECODER_ADMISSION_ATTEMPTS, 4, Duration::from_secs(2)).await;
            second.close();
            third
                .open(client.clone(), Arc::clone(&runtime), window)
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
            let closed_decoders = ACTIVE_DECODERS.load(Ordering::SeqCst);
            let closed_peak = PEAK_DECODERS.load(Ordering::SeqCst);
            let closed_permits = pool.available_permits();
            let closed_fetches = fetches();
            let stale_shown = first.show_thumbnail(stale_generation, 0, thumbs::Image::black(1, 1));

            decode_gate.release();
            wait_for_decoder_count(0, Duration::from_secs(2)).await;
            let permits_returned = holds_within(
                || pool.available_permits() == THUMBNAIL_CONCURRENCY,
                Duration::from_secs(2),
            )
            .await;
            let released_peak = PEAK_DECODERS.load(Ordering::SeqCst);
            // A cancelled waiter that still held a turn would fetch soon after
            // the permits return; nothing fetches within this window.
            glib::timeout_future(Duration::from_millis(100)).await;
            Replacements {
                first_fetches,
                first_permits,
                closed_decoders,
                closed_permits,
                closed_fetches,
                closed_peak,
                stale_shown,
                permits_returned,
                released_peak,
                released_fetches: fetches(),
            }
        });
        drop(decode_gate);
        server.abort();
        seen
    }

    /// Under a display: each of the first view's two decoders fetches its
    /// keyframe.
    pub(crate) fn the_first_views_decoders_fetch_two_keyframes() {
        assert_eq!(open_replacements().first_fetches, 2);
    }

    /// Under a display: the first view's two decoders hold every permit of
    /// the pool.
    pub(crate) fn the_first_views_decoders_hold_every_permit() {
        assert_eq!(open_replacements().first_permits, 0);
    }

    /// Under a display: closing a view leaves its running decoders running.
    pub(crate) fn closing_a_view_leaves_its_decoders_running() {
        assert_eq!(open_replacements().closed_decoders, 2);
    }

    /// Under a display: the decoders of a closed view keep their permits
    /// until they end.
    pub(crate) fn a_closed_views_decoders_keep_their_permits() {
        assert_eq!(open_replacements().closed_permits, 0);
    }

    /// Under a display: the views that replace one another never run more
    /// decoders at once than the pool admits, before or after the first
    /// view's decoders end.
    pub(crate) fn replacement_views_run_no_more_decoders_than_the_pool_admits() {
        let seen = open_replacements();
        assert_eq!((seen.closed_peak, seen.released_peak), (2, 2));
    }

    /// Under a display: a thumbnail job cancelled while it waits for a
    /// permit fetches no keyframe, before or after the permits return.
    pub(crate) fn a_cancelled_permit_waiter_fetches_no_keyframe() {
        let seen = open_replacements();
        assert_eq!(
            seen.closed_fetches, 2,
            "cancelled permit waiters must not fetch keyframe bodies"
        );
        assert_eq!(
            seen.released_fetches, 2,
            "cancelled permit waiters must not fetch after permits are released"
        );
    }

    /// Under a display: a closed view shows no thumbnail of the generation
    /// it had when it closed.
    pub(crate) fn a_closed_view_shows_no_thumbnail_of_its_generation() {
        assert!(!open_replacements().stale_shown);
    }

    /// Under a display: decoders that end return their permits to the pool.
    pub(crate) fn ended_decoders_return_their_permits() {
        assert!(
            open_replacements().permits_returned,
            "released decoders must return their permits"
        );
    }

    fn replacement_fixture(keyframe_fetches: Arc<AtomicUsize>) -> axum::Router {
        use axum::extract::State;

        axum::Router::new()
            .route(
                "/api/streams/{camera}/{kind}/segments",
                routing::get(|| async {
                    r#"[{"wall_start_us":0,"wall_end_us":12000000,"sealed":true}]"#
                }),
            )
            .route("/api/events", routing::get(|| async { "[]" }))
            .route(
                "/api/streams/{camera}/{kind}/hls/vod.m3u8",
                routing::get(|| async {
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
                routing::get(|State(fetches): State<Arc<AtomicUsize>>| async move {
                    fetches.fetch_add(1, Ordering::SeqCst);
                    axum::http::Response::builder()
                        .header(axum::http::header::CONTENT_TYPE, "video/mp4")
                        .body(axum::body::Body::from(vec![0; 4096]))
                        .unwrap()
                }),
            )
            .route(
                "/{*media}",
                routing::get(|| async {
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
                Either::Left(_)
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
                Either::Left(_)
            ),
            "decoder count did not reach {expected}; active={} peak={}",
            ACTIVE_DECODERS.load(Ordering::SeqCst),
            PEAK_DECODERS.load(Ordering::SeqCst)
        );
    }

    pub(crate) fn export_terminal_status_cannot_be_overwritten_by_progress() {
        const CHUNKS: usize = 4096;

        gtk::init().unwrap();
        let runtime = Arc::new(Runtime::new().unwrap());
        let listener = bind(&runtime);
        let address = listener.local_addr().unwrap();
        let server = runtime.spawn(async move {
            let app = axum::Router::new().route(
                "/api/cameras/fixture/export.mp4",
                routing::get(|| async {
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
        let view = view_of("fixture", thumbnail_decoder_pool());
        *view.client.borrow_mut() = Some(client_for(address));
        *view.runtime.borrow_mut() = Some(Arc::clone(&runtime));
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("export.mp4");
        glib::MainContext::default().block_on(async {
            Rc::clone(&view)
                .export_to(path.clone(), window_of(1, 2))
                .await;
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

    /// Presses "Export" on `view` with marks a microsecond more than seven
    /// days apart, and runs the main loop until the press is handled. The
    /// refusal comes before the save dialog; if a dialog opens instead, the
    /// check fails after a second rather than wait on it.
    async fn export_a_range_over_seven_days(view: &Rc<Playback>) {
        let week = i64::try_from(MAX_RANGE_US).unwrap();
        view.timeline.set_marks(Some(at(0)), Some(at(week + 1)));
        let pressed = Rc::clone(view).export_dialog(None);
        let timeout = glib::timeout_future(Duration::from_secs(1));
        futures::pin_mut!(pressed, timeout);
        assert!(
            matches!(
                futures::future::select(pressed, timeout).await,
                Either::Left(_)
            ),
            "the refusal must come before the save dialog"
        );
    }

    /// Under a display: "Export" with marks a microsecond more than seven
    /// days apart shows the refusal of the range in the status line.
    pub(crate) fn export_shows_the_refusal_of_a_range_over_seven_days() {
        gtk::init().unwrap();
        let view = view_of("fixture", thumbnail_decoder_pool());
        glib::MainContext::default().block_on(export_a_range_over_seven_days(&view));
        assert_eq!(
            view.status.text(),
            "export failed: time range exceeds seven days"
        );
        view.close();
    }

    /// Under a display: "Export" with marks more than seven days apart,
    /// pressed while an export runs, leaves that export running.
    pub(crate) fn a_refused_range_leaves_a_running_export_running() {
        gtk::init().unwrap();
        let runtime = Arc::new(Runtime::new().unwrap());
        let listener = bind(&runtime);
        let address = listener.local_addr().unwrap();
        // The export's answer never comes, so the export runs until it is
        // cancelled.
        let server = runtime.spawn(async move {
            let app = axum::Router::new().route(
                "/api/cameras/fixture/export.mp4",
                routing::get(futures::future::pending::<String>),
            );
            axum::serve(listener, app).await.unwrap();
        });
        let view = view_of("fixture", thumbnail_decoder_pool());
        *view.client.borrow_mut() = Some(client_for(address));
        *view.runtime.borrow_mut() = Some(Arc::clone(&runtime));
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("export.mp4");
        let export = glib::spawn_future_local(Rc::clone(&view).export_to(path, window_of(1, 2)));
        glib::MainContext::default().block_on(async {
            wait_until(
                || view.exporting.borrow().is_some(),
                Duration::from_secs(1),
                "the export must start",
            )
            .await;
            export_a_range_over_seven_days(&view).await;
            // An export that the refusal cancelled would end in this time.
            glib::timeout_future(Duration::from_millis(25)).await;
        });
        assert!(
            view.exporting
                .borrow()
                .as_ref()
                .is_some_and(|running| !running.is_finished()),
            "the refused range must leave the running export running"
        );
        view.close();
        export.abort();
        server.abort();
    }

    /// Readings of a view whose media are missing, taken once its player
    /// fails.
    struct FailedPlayback {
        /// Whether the snapshot's listener refused requests within a second
        /// of the failure.
        listener_closed: bool,
        /// Whether the view released its pipeline.
        stopped: bool,
    }

    /// Plays a snapshot whose media are missing until the player fails.
    fn fail_playback() -> FailedPlayback {
        gtk::init().unwrap();
        gst::init().unwrap();
        gstgtk4::plugin_register_static().unwrap();
        let runtime = Arc::new(Runtime::new().unwrap());
        let listener = bind(&runtime);
        let address = listener.local_addr().unwrap();
        let server = runtime.spawn(async move {
            axum::serve(listener, axum::Router::new()).await.unwrap();
        });
        let origin = ApiOrigin::parse(&format!("http://{address}"), PlainHttp::LoopbackOnly);
        let snapshot = runtime
            .block_on(Snapshot::serve_video(
                &origin.unwrap(),
                &format!(
                    "#EXTM3U\n#EXT-X-VERSION:7\n#EXT-X-TARGETDURATION:2\n\
                     #EXT-X-PLAYLIST-TYPE:VOD\n#EXT-X-MAP:URI=\"http://{address}/missing.mp4\"\n\
                     #IRIS-FRAGMENT:WALL-US=0,DURATION-TICKS=180000\n\
                     #EXTINF:2.00000,\nhttp://{address}/missing.m4s\n#EXT-X-ENDLIST\n"
                ),
            ))
            .unwrap();
        let url = snapshot.url().clone();
        let view = view_of("fixture", thumbnail_decoder_pool());
        view.start(snapshot).unwrap();
        let listener_closed = glib::MainContext::default().block_on(async {
            wait_until(
                || view.title.text().starts_with("playback error:"),
                Duration::from_secs(10),
                "missing media must produce a playback error",
            )
            .await;
            runtime
                .spawn(async move {
                    let http = reqwest::Client::builder().no_proxy().build().unwrap();
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
        FailedPlayback {
            listener_closed,
            stopped,
        }
    }

    /// Under a display: a playback error closes the listener of the view's
    /// snapshot.
    pub(crate) fn playback_error_releases_its_snapshot_listener() {
        assert!(
            fail_playback().listener_closed,
            "playback failure must close its snapshot listener"
        );
    }

    /// Under a display: a playback error releases the view's pipeline.
    pub(crate) fn playback_error_releases_its_pipeline() {
        assert!(
            fail_playback().stopped,
            "playback failure must release its pipeline"
        );
    }

    pub(crate) fn a_window_holding_only_a_stale_open_segment_shows_nothing_recorded() {
        gtk::init().unwrap();
        let runtime = Arc::new(Runtime::new().unwrap());
        let listener = bind(&runtime);
        let address = listener.local_addr().unwrap();
        let vod_requests = Arc::new(AtomicUsize::new(0));
        let server = runtime.spawn({
            let vod_requests = Arc::clone(&vod_requests);
            async move {
                // Opened 3 h before the window's end, last progress 2 h before it.
                let stale = concat!(
                    r#"[{"wall_start_us":-7200000000,"wall_end_us":-3600000000,"#,
                    r#""sealed":false}]"#
                );
                let app = axum::Router::new()
                    .route(
                        "/api/streams/fixture/main/segments",
                        routing::get(move || async move { stale }),
                    )
                    .route("/api/events", routing::get(|| async { "[]" }))
                    .route(
                        "/api/streams/fixture/main/hls/vod.m3u8",
                        routing::get(move || {
                            vod_requests.fetch_add(1, Ordering::SeqCst);
                            async { axum::http::StatusCode::NOT_FOUND }
                        }),
                    );
                axum::serve(listener, app).await.unwrap();
            }
        });
        let client = client_for(address);
        let view = view_of("fixture", thumbnail_decoder_pool());
        glib::MainContext::default().block_on(async {
            let opening = view.open(client, Arc::clone(&runtime), window_of(0, 3_600_000_000));
            let timeout = glib::timeout_future(Duration::from_secs(5));
            futures::pin_mut!(opening, timeout);
            assert!(
                matches!(
                    futures::future::select(opening, timeout).await,
                    Either::Left(_)
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
    fn export_progress_retains_only_the_latest_value() {
        let (tx, mut rx) = export_progress_channel();
        for written in 1..=100_000 {
            tx.send_replace(written);
        }
        assert_eq!(*rx.borrow_and_update(), 100_000);
        assert!(!rx.has_changed().unwrap());
    }

    #[test]
    fn the_mark_status_names_the_range_or_the_missing_mark() {
        let (i, o) = (at(0), at(90 * MICROS_PER_SECOND));
        assert!(marks_status(Some(i), Some(o)).ends_with("(90 s)"));
        assert!(marks_status(Some(o), Some(i)).starts_with("out mark must be after"));
        assert!(marks_status(Some(i), None).ends_with("set the out mark"));
        assert!(marks_status(None, Some(o)).ends_with("set the in mark"));
        assert_eq!(marks_status(None, None), "");
    }

    #[test]
    fn no_marks_give_no_export_range() {
        assert!(export_window(None, None).is_none());
    }

    #[test]
    fn one_mark_alone_gives_no_export_range() {
        assert!(export_window(Some(at(0)), None).is_none());
        assert!(export_window(None, Some(at(1))).is_none());
    }

    #[test]
    fn an_out_mark_not_after_the_in_mark_gives_no_export_range() {
        for (mark_in, mark_out) in [(5, 5), (5, 4)] {
            assert!(
                export_window(Some(at(mark_in)), Some(at(mark_out))).is_none(),
                "[{mark_in}, {mark_out})"
            );
        }
    }

    #[test]
    fn marks_a_window_admits_give_that_window() {
        let range = export_window(Some(at(1)), Some(at(2)));
        assert!(
            matches!(range, Some(Ok(window)) if window == window_of(1, 2)),
            "{range:?}"
        );
    }

    /// Marks a microsecond more than seven days apart give the refusal whose
    /// text the status shows after "export failed: ".
    #[test]
    fn marks_more_than_seven_days_apart_give_the_ranges_refusal() {
        let week = i64::try_from(MAX_RANGE_US).unwrap();
        let range = export_window(Some(at(0)), Some(at(week + 1)));
        let Some(Err(refused)) = range else {
            panic!("{range:?}");
        };
        assert!(
            matches!(refused, ApiError::Invalid(Rule::TimeRangeTooLong)),
            "{refused:?}"
        );
        assert_eq!(refused.to_string(), "time range exceeds seven days");
    }

    /// A VOD body of one fragment of `segment`, as the daemon writes it.
    fn vod_body(segment: u32) -> String {
        format!(
            "#EXTM3U\n#EXT-X-VERSION:7\n#EXT-X-TARGETDURATION:1\n#EXT-X-MEDIA-SEQUENCE:0\n\
             #EXT-X-PLAYLIST-TYPE:VOD\n#EXT-X-INDEPENDENT-SEGMENTS\n\
             #EXT-X-MAP:URI=\"/api/segments/{segment}/file.mp4?token=t{segment}\",BYTERANGE=\"700@0\"\n\
             #IRIS-FRAGMENT:WALL-US=0,DURATION-TICKS=90000\n#EXTINF:1.00000,\n\
             #EXT-X-BYTERANGE:1000@700\n/api/segments/{segment}/file.mp4?token=t{segment}\n\
             #EXT-X-ENDLIST\n"
        )
    }

    /// A daemon that lists one recording of every stream of `fixture` and
    /// serves the main and sub playlists, the audio one only when `audio`;
    /// `audio_requests` counts the fetches of the audio playlist.
    async fn window_fixture(
        audio: bool,
        audio_requests: Arc<AtomicUsize>,
    ) -> (SocketAddr, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let segments = || async { r#"[{"wall_start_us":0,"wall_end_us":1000000,"sealed":true}]"# };
        let app = axum::Router::new()
            .route("/api/streams/fixture/main/segments", routing::get(segments))
            .route("/api/streams/fixture/sub/segments", routing::get(segments))
            .route("/api/events", routing::get(|| async { "[]" }))
            .route(
                "/api/streams/fixture/main/hls/vod.m3u8",
                routing::get(|| async { vod_body(1) }),
            )
            .route(
                "/api/streams/fixture/sub/hls/vod.m3u8",
                routing::get(|| async { vod_body(3) }),
            )
            .route(
                "/api/streams/fixture/audio/hls/vod.m3u8",
                routing::get(move || {
                    audio_requests.fetch_add(1, Ordering::SeqCst);
                    async move {
                        if audio {
                            Ok(vod_body(2))
                        } else {
                            Err(axum::http::StatusCode::NOT_FOUND)
                        }
                    }
                }),
            );
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        (address, server)
    }

    async fn fetch_text(url: Url) -> String {
        let http = reqwest::Client::builder().no_proxy().build().unwrap();
        http.get(url).send().await.unwrap().text().await.unwrap()
    }

    /// Main-stream playback of a window with recorded audio is a multivariant
    /// playlist with the audio beside it; without recorded audio, and for the
    /// sub stream, whose timeline the audio is not on, it is the video
    /// playlist alone.
    #[tokio::test]
    async fn the_main_stream_plays_with_recorded_audio_and_the_sub_stream_without() {
        let requests = Arc::new(AtomicUsize::new(0));
        let (address, server) = window_fixture(true, Arc::clone(&requests)).await;
        let lists = fetch_window(
            client_for(address),
            camera("fixture"),
            StreamKind::Main,
            window_of(0, 1_000_000),
        )
        .await;
        let (fragments, snapshot) = lists.playback.unwrap().unwrap();
        assert_eq!(fragments.len(), 1);
        assert!(snapshot.has_audio(), "the view shows its Sound button");
        let multivariant = fetch_text(snapshot.url().clone()).await;
        assert!(
            multivariant.contains("#EXT-X-MEDIA:TYPE=AUDIO"),
            "{multivariant}"
        );
        let audio = fetch_text(snapshot.url().join("audio.m3u8").unwrap()).await;
        assert!(
            audio.contains(&format!(
                "http://{address}/api/segments/2/file.mp4?token=t2"
            )),
            "{audio}"
        );
        let video = fetch_text(snapshot.url().join("video.m3u8").unwrap()).await;
        assert!(video.contains(&format!(
            "http://{address}/api/segments/1/file.mp4?token=t1"
        )));

        let lists = fetch_window(
            client_for(address),
            camera("fixture"),
            StreamKind::Sub,
            window_of(0, 1_000_000),
        )
        .await;
        let (_, snapshot) = lists.playback.unwrap().unwrap();
        assert!(!snapshot.has_audio());
        let alone = fetch_text(snapshot.url().clone()).await;
        assert!(!alone.contains("EXT-X-MEDIA:"), "{alone}");
        assert!(alone.contains("/api/segments/3/file.mp4"), "{alone}");
        assert_eq!(
            requests.load(Ordering::SeqCst),
            1,
            "the sub stream asks for no audio"
        );
        server.abort();

        let requests = Arc::new(AtomicUsize::new(0));
        let (address, server) = window_fixture(false, Arc::clone(&requests)).await;
        let lists = fetch_window(
            client_for(address),
            camera("fixture"),
            StreamKind::Main,
            window_of(0, 1_000_000),
        )
        .await;
        let (_, snapshot) = lists.playback.unwrap().unwrap();
        let alone = fetch_text(snapshot.url().clone()).await;
        assert!(!alone.contains("EXT-X-MEDIA:"), "{alone}");
        assert!(alone.contains("/api/segments/1/file.mp4"), "{alone}");
        assert_eq!(requests.load(Ordering::SeqCst), 1);
        server.abort();
    }
}
