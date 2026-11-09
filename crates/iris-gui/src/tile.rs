//! One camera tile: a branch of the shared stage pipeline from `appsrc` to
//! `gtk4paintablesink`, the widget that shows it, and the lifecycle that
//! starts and stops both the branch and the feed behind it.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use anyhow::Context;
use gst::prelude::*;
use gtk::prelude::*;
use gtk::{gdk, glib};
use iris_core::id::CameraId;
use tokio::runtime::Runtime;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use crate::live::{self, Feed};
use crate::stage::{BranchEvent, Stage};

/// Maximum queued bytes; exceeding it reconnects from an init segment and sync frame.
pub(crate) const SOURCE_QUEUE_BYTES: u64 = 16 << 20;
/// Maximum queued buffers. At 30 fps this permits more than 17 seconds of backlog.
const SOURCE_QUEUE_BUFFERS: u64 = 512;
/// Default of `--live-budget-mib`: the live payload bytes alive in the viewer at
/// once, the default grid of sixteen full source queues.
pub const DEFAULT_LIVE_BUDGET_MIB: u32 = 256;
/// Least `--live-budget-mib` accepted: one full source queue, so one payload at
/// the protocol ceiling can be admitted.
pub const MIN_LIVE_BUDGET_MIB: u32 = 16;

/// The viewer-wide bound on live payload bytes: every byte a [`Source`] copies
/// for GStreamer counts from the copy until GStreamer frees the buffer, across
/// every grid tile and the focused view. The per-source queue limits bound
/// what waits in `appsrc`; this bounds what is alive anywhere downstream too.
#[derive(Debug)]
pub struct LiveBudget {
    limit: u64,
    used: AtomicU64,
}

impl LiveBudget {
    /// A budget of `limit` bytes.
    #[must_use]
    pub fn new(limit: u64) -> Arc<Self> {
        Arc::new(Self {
            limit,
            used: AtomicU64::new(0),
        })
    }

    /// Bytes reserved at this moment.
    #[cfg(test)]
    pub(crate) fn used(&self) -> u64 {
        self.used.load(Ordering::Acquire)
    }

    /// Reserves `bytes`, or returns `None` when they do not fit beside the
    /// bytes already alive. The reservation is released when it is dropped.
    fn reserve(self: &Arc<Self>, bytes: u64) -> Option<Reservation> {
        let mut used = self.used.load(Ordering::Acquire);
        loop {
            let next = used.checked_add(bytes).filter(|next| *next <= self.limit)?;
            match self
                .used
                .compare_exchange_weak(used, next, Ordering::AcqRel, Ordering::Acquire)
            {
                Ok(_) => {
                    return Some(Reservation {
                        budget: Arc::clone(self),
                        bytes,
                    });
                }
                Err(current) => used = current,
            }
        }
    }
}

/// Bytes taken from a [`LiveBudget`], returned when dropped.
#[derive(Debug)]
struct Reservation {
    budget: Arc<LiveBudget>,
    bytes: u64,
}

impl Drop for Reservation {
    fn drop(&mut self) {
        self.budget.used.fetch_sub(self.bytes, Ordering::AcqRel);
    }
}

/// A copied live payload that keeps its budget reservation for as long as
/// GStreamer keeps the buffer wrapping it.
#[derive(Debug)]
struct Payload {
    bytes: Vec<u8>,
    _reservation: Reservation,
}

impl AsRef<[u8]> for Payload {
    fn as_ref(&self) -> &[u8] {
        &self.bytes
    }
}

/// The branch description. `qtdemux` consumes the daemon's fragmented MP4
/// byte stream, `decodebin3` picks the highest-ranked decoder, `glupload`
/// puts the frames in GL memory (a no-op when the decoder wrote GL memory or
/// DMA-BUF, which then stays on the GPU; a software decoder's frames are
/// uploaded from system memory), and `glcolorconvert` converts to the sink's
/// RGBA on the GPU. `sync=false` renders a frame when it is
/// decoded: a live view wants no clock delay. `async=false` keeps the sink
/// from holding the branch in paused until it prerolls: the live source
/// pushes nothing until the branch plays, so an asynchronous sink added to
/// the running stage would wait forever.
const BRANCH: &str = "appsrc name=src is-live=true stream-type=stream \
    ! qtdemux ! decodebin3 ! glupload ! glcolorconvert \
    ! gtk4paintablesink name=sink sync=false async=false";

/// What a feed asks of its tile.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TileEvent {
    /// The feed's session ended; the branch restarts so that the next
    /// connection's init segment meets a clean demuxer.
    Restart,
    /// A live frame exceeded the protocol's message ceiling; the feed retries
    /// after `retry_in`, doubling the pause while the condition lasts.
    Oversized {
        /// The pause before the next attempt.
        retry_in: Duration,
    },
    /// The feed delivers video again after an oversized frame.
    Live,
}

/// The feed's handle on the branch input, usable from any thread.
#[derive(Debug)]
pub struct Source {
    src: gst_app::AppSrc,
    budget: Arc<LiveBudget>,
}

impl Source {
    /// Pushes a complete live payload without blocking the runtime. A full queue,
    /// or a viewer-wide budget that cannot admit the payload, fails the session
    /// so reconnection starts a decodable byte stream.
    pub fn push(&self, bytes: &[u8]) -> anyhow::Result<()> {
        anyhow::ensure!(!bytes.is_empty(), "live input payload is empty");
        let queued = self.src.current_level_bytes();
        anyhow::ensure!(
            bytes.len() as u64 <= SOURCE_QUEUE_BYTES.saturating_sub(queued),
            "live input queue exceeded its byte limit"
        );
        anyhow::ensure!(
            self.src.property::<u64>("current-level-buffers") < SOURCE_QUEUE_BUFFERS,
            "live input queue exceeded its buffer limit"
        );
        let reservation = self.budget.reserve(bytes.len() as u64).ok_or_else(|| {
            anyhow::anyhow!(
                "live input exceeds the viewer's {} MiB live budget",
                self.budget.limit >> 20
            )
        })?;
        let buffer = gst::Buffer::from_slice(Payload {
            bytes: bytes.to_vec(),
            _reservation: reservation,
        });
        self.src
            .push_buffer(buffer)
            .map(|_| ())
            .map_err(|flow| anyhow::anyhow!("appsrc rejected a buffer: {flow:?}"))
    }

    /// Prepares for a fresh byte stream (a reconnection): flushes what is queued.
    pub fn reset(&self) {
        let _ = self.src.send_event(gst::event::FlushStart::new());
        let _ = self
            .src
            .send_event(gst::event::FlushStop::new(/* reset_time */ true));
    }

    /// A source with the tile's queue limits outside any pipeline and a budget
    /// of its own, for feed tests.
    #[cfg(test)]
    pub(crate) fn for_tests() -> Self {
        let src = gst_app::AppSrc::builder()
            .max_bytes(SOURCE_QUEUE_BYTES)
            .build();
        src.set_property("max-buffers", SOURCE_QUEUE_BUFFERS);
        Self {
            src,
            budget: LiveBudget::new(u64::from(DEFAULT_LIVE_BUDGET_MIB) << 20),
        }
    }

    /// Bytes queued in the source.
    #[cfg(test)]
    pub(crate) fn queued_bytes(&self) -> u64 {
        self.src.current_level_bytes()
    }
}

/// The label of a tile whose stream carries frames above the live message ceiling.
fn oversized_label(name: &str, camera: &CameraId, retry_in: Duration) -> String {
    format!(
        "{name} ({camera}): a live frame exceeds the {} MiB limit; retrying in {} s",
        SOURCE_QUEUE_BYTES >> 20,
        retry_in.as_secs()
    )
}

/// A running branch and the feed task that fills it.
#[derive(Debug)]
struct Running {
    bin: gst::Bin,
    feed: CancellationToken,
}

/// A camera tile: a picture, a label, and, while started, a branch and feed.
#[derive(Debug)]
pub struct Tile {
    camera: CameraId,
    name: String,
    picture: gtk::Picture,
    frame: gtk::Box,
    label: gtk::Label,
    runtime: Arc<Runtime>,
    stage: Rc<Stage>,
    budget: Arc<LiveBudget>,
    running: RefCell<Option<Running>>,
}

impl Tile {
    /// Builds the widgets for `camera`; nothing runs until [`Tile::start`]. The
    /// tile's feed draws its live payload bytes from `budget`, shared by every
    /// tile of the viewer.
    pub fn new(
        camera: &CameraId,
        name: &str,
        runtime: Arc<Runtime>,
        stage: Rc<Stage>,
        budget: Arc<LiveBudget>,
    ) -> Self {
        let picture = gtk::Picture::new();
        picture.set_content_fit(gtk::ContentFit::Contain);
        picture.set_hexpand(true);
        picture.set_vexpand(true);
        let label = gtk::Label::builder()
            .label(format!("{name} ({camera})"))
            .xalign(0.0)
            .margin_start(4)
            .build();
        let frame = gtk::Box::new(gtk::Orientation::Vertical, 2);
        frame.append(&picture);
        frame.append(&label);
        Self {
            camera: camera.clone(),
            name: name.to_owned(),
            picture,
            frame,
            label,
            runtime,
            stage,
            budget,
            running: RefCell::new(None),
        }
    }

    /// The widget to place in a container.
    pub fn widget(&self) -> &gtk::Box {
        &self.frame
    }

    /// The camera id.
    pub fn camera(&self) -> &CameraId {
        &self.camera
    }

    /// The camera's display name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Builds the branch on the stage, points the picture at its sink, and
    /// starts the feed. Starting a started tile restarts it. The label names
    /// the camera while video flows and the last branch error until it does
    /// again.
    pub fn start(&self, feed: Feed) -> anyhow::Result<()> {
        self.stop();
        let bin = gst::parse::bin_from_description(BRANCH, /* ghost_unlinked_pads */ false)
            .context("building the tile branch")?;
        let src = bin
            .by_name("src")
            .and_then(|e| e.downcast::<gst_app::AppSrc>().ok())
            .context("appsrc missing")?;
        src.set_max_bytes(SOURCE_QUEUE_BYTES);
        src.set_property("max-buffers", SOURCE_QUEUE_BUFFERS);
        let sink = bin.by_name("sink").context("sink missing")?;
        let paintable = sink.property::<gdk::Paintable>("paintable");
        self.picture.set_paintable(Some(&paintable));
        self.label
            .set_label(&format!("{} ({})", self.name, self.camera));

        let (branch_tx, branch_rx) = async_channel::unbounded::<BranchEvent>();
        // Readiness is the first buffer reaching the sink after the branch
        // starts or restarts; the stage's bus cannot tell, since stream-start
        // messages aggregate over all sinks. Each restart arms the probe again.
        let armed = Arc::new(AtomicBool::new(true));
        if let Some(pad) = sink.static_pad("sink") {
            let ready = branch_tx.clone();
            let armed = Arc::clone(&armed);
            let camera = self.camera.clone();
            pad.add_probe(gst::PadProbeType::BUFFER, move |_, _| {
                if armed.swap(false, Ordering::AcqRel) {
                    info!(camera = %camera, "first frame decoded");
                    let _ = ready.try_send(BranchEvent::Ready);
                }
                gst::PadProbeReturn::Ok
            });
        }
        self.stage.add(
            &format!("tile-{}-{}", self.camera, feed.kind),
            &bin,
            branch_tx,
        )?;
        glib::spawn_future_local({
            let label = self.label.clone();
            let name = self.name.clone();
            let camera = self.camera.clone();
            async move {
                while let Ok(event) = branch_rx.recv().await {
                    match event {
                        BranchEvent::Error(text) => {
                            label.set_label(&format!("{name} ({camera}): {text}"));
                        }
                        BranchEvent::Ready => label.set_label(&format!("{name} ({camera})")),
                    }
                }
            }
        });

        let cancel = CancellationToken::new();
        let (events_tx, events_rx) = async_channel::unbounded::<TileEvent>();
        self.runtime.spawn(live::run(
            feed,
            Source {
                src,
                budget: Arc::clone(&self.budget),
            },
            events_tx,
            cancel.clone(),
        ));
        glib::spawn_future_local({
            let bin = bin.clone();
            let label = self.label.clone();
            let name = self.name.clone();
            let camera = self.camera.clone();
            async move {
                while let Ok(event) = events_rx.recv().await {
                    match event {
                        TileEvent::Restart => {
                            armed.store(true, Ordering::Release);
                            if let Err(error) = Stage::restart(&bin) {
                                warn!(
                                    camera = %camera,
                                    error = %format!("{error:#}"),
                                    "branch restart failed"
                                );
                                label.set_label(&format!(
                                    "{name} ({camera}): restart failed: {error:#}"
                                ));
                            }
                        }
                        TileEvent::Oversized { retry_in } => {
                            label.set_label(&oversized_label(&name, &camera, retry_in));
                        }
                        TileEvent::Live => label.set_label(&format!("{name} ({camera})")),
                    }
                }
            }
        });
        *self.running.borrow_mut() = Some(Running { bin, feed: cancel });
        Ok(())
    }

    /// Shows why the tile has no feed; [`Tile::start`] restores the label.
    pub fn idle(&self, reason: &str) {
        self.label
            .set_label(&format!("{} ({}): {reason}", self.name, self.camera));
    }

    /// Whether a branch and feed are running.
    #[cfg(test)]
    pub(crate) fn is_running(&self) -> bool {
        self.running.borrow().is_some()
    }

    /// The text under the picture.
    #[cfg(test)]
    pub(crate) fn label_text(&self) -> String {
        self.label.text().to_string()
    }

    /// Stops the feed and removes the branch; the last frame stays on screen.
    pub fn stop(&self) {
        if let Some(running) = self.running.borrow_mut().take() {
            running.feed.cancel();
            self.stage.remove(&running.bin);
        }
    }
}

impl Drop for Tile {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_oversized_state_names_the_limit_and_the_retry() {
        assert_eq!(
            oversized_label(
                "Front",
                &CameraId::try_new("cam1").unwrap(),
                Duration::from_secs(8)
            ),
            "Front (cam1): a live frame exceeds the 16 MiB limit; retrying in 8 s"
        );
    }

    #[test]
    fn a_stalled_source_rejects_payloads_beyond_the_byte_limit() {
        gst::init().unwrap();
        let source = Source {
            src: gst_app::AppSrc::builder()
                .max_bytes(SOURCE_QUEUE_BYTES)
                .build(),
            budget: LiveBudget::new(u64::MAX),
        };
        let payload = vec![0; 1 << 20];
        for _ in 0..16 {
            source.push(&payload).unwrap();
        }
        assert!(source.push(&payload).is_err());
        assert_eq!(source.src.current_level_bytes(), SOURCE_QUEUE_BYTES);
    }

    fn budgeted_source(budget: &Arc<LiveBudget>) -> Source {
        Source {
            src: gst_app::AppSrc::builder()
                .max_bytes(SOURCE_QUEUE_BYTES)
                .build(),
            budget: Arc::clone(budget),
        }
    }

    #[test]
    fn the_live_budget_spans_sources() {
        gst::init().unwrap();
        let budget = LiveBudget::new(2 << 20);
        let (first, second) = (budgeted_source(&budget), budgeted_source(&budget));
        let payload = vec![0; 1 << 20];
        first.push(&payload).unwrap();
        second.push(&payload).unwrap();
        assert_eq!(budget.used(), 2 << 20);
        let refused = first.push(&[1]).unwrap_err();
        assert!(refused.to_string().contains("live budget"), "{refused}");
        assert_eq!(
            first.src.current_level_bytes(),
            1 << 20,
            "the refused payload was not queued"
        );
    }

    #[test]
    fn freeing_queued_buffers_returns_their_budget() {
        gst::init().unwrap();
        let budget = LiveBudget::new(2 << 20);
        let (first, second) = (budgeted_source(&budget), budgeted_source(&budget));
        let payload = vec![0; 1 << 20];
        first.push(&payload).unwrap();
        second.push(&payload).unwrap();
        drop(first);
        assert_eq!(
            budget.used(),
            1 << 20,
            "freeing the queued buffers returns their bytes"
        );
        second.push(&payload).unwrap();
        assert_eq!(budget.used(), 2 << 20);
        drop(second);
        assert_eq!(budget.used(), 0);
    }

    #[test]
    fn an_oversized_payload_is_rejected_without_queueing() {
        gst::init().unwrap();
        let source = Source {
            src: gst_app::AppSrc::builder().build(),
            budget: LiveBudget::new(u64::MAX),
        };
        assert!(
            source
                .push(&vec![0; SOURCE_QUEUE_BYTES as usize + 1])
                .is_err()
        );
        assert_eq!(source.src.current_level_bytes(), 0);
    }

    #[test]
    fn empty_and_excess_buffers_are_rejected_without_queueing() {
        gst::init().unwrap();
        let src = gst_app::AppSrc::builder()
            .max_bytes(SOURCE_QUEUE_BYTES)
            .build();
        src.set_property("max-buffers", SOURCE_QUEUE_BUFFERS);
        let source = Source {
            src,
            budget: LiveBudget::new(u64::MAX),
        };
        assert_eq!(
            source.src.property::<u64>("max-buffers"),
            SOURCE_QUEUE_BUFFERS
        );
        assert!(source.push(&[]).is_err());
        assert_eq!(source.src.property::<u64>("current-level-buffers"), 0);

        for _ in 0..SOURCE_QUEUE_BUFFERS {
            source.push(&[1]).unwrap();
        }
        assert_eq!(
            source.src.property::<u64>("current-level-buffers"),
            SOURCE_QUEUE_BUFFERS
        );
        assert!(source.push(&[1]).is_err());
        assert_eq!(
            source.src.property::<u64>("current-level-buffers"),
            SOURCE_QUEUE_BUFFERS
        );
    }
}
