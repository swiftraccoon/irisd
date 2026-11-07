//! One camera tile: a branch of the shared stage pipeline from `appsrc` to
//! `gtk4paintablesink`, the widget that shows it, and the lifecycle that
//! starts and stops both the branch and the feed behind it.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;

use anyhow::Context;
use gst::prelude::*;
use gtk::prelude::*;
use gtk::{gdk, glib};
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use crate::live::{self, Feed};
use crate::stage::{BranchEvent, Stage};

/// Maximum queued bytes; exceeding it reconnects from an init segment and sync frame.
pub(crate) const SOURCE_QUEUE_BYTES: u64 = 16 << 20;
/// Maximum queued buffers. At 30 fps this permits more than 17 seconds of backlog.
const SOURCE_QUEUE_BUFFERS: u64 = 512;

/// The branch description. `qtdemux` consumes the daemon's fragmented MP4
/// byte stream, `decodebin3` picks the highest-ranked decoder, `glupload`
/// puts the frames in GL memory (a no-op when the decoder wrote them there),
/// and `glcolorconvert` converts to the sink's RGBA on the GPU, so no frame
/// crosses the CPU after decoding. `sync=false` renders a frame when it is
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
    /// The feed reconnected; the branch must start afresh for the new init segment.
    Restart,
}

/// The feed's handle on the branch input, usable from any thread.
#[derive(Debug)]
pub struct Source {
    src: gst_app::AppSrc,
}

impl Source {
    /// Pushes a complete live payload without blocking the runtime. A full queue
    /// fails the session so reconnection starts a decodable byte stream.
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
        let buffer = gst::Buffer::from_slice(bytes.to_vec());
        self.src
            .push_buffer(buffer)
            .map(|_| ())
            .map_err(|flow| anyhow::anyhow!("appsrc rejected a buffer: {flow:?}"))
    }

    /// Prepares for a fresh byte stream (a reconnection): flushes what is queued.
    pub fn reset(&self) {
        let _ = self.src.send_event(gst::event::FlushStart::new());
        let _ = self.src.send_event(gst::event::FlushStop::new(true));
    }
}

/// A running branch and the feed task that fills it.
struct Running {
    bin: gst::Bin,
    feed: CancellationToken,
}

/// A camera tile: a picture, a label, and, while started, a branch and feed.
pub struct Tile {
    camera: String,
    name: String,
    picture: gtk::Picture,
    frame: gtk::Box,
    label: gtk::Label,
    runtime: Arc<tokio::runtime::Runtime>,
    stage: Rc<Stage>,
    running: Rc<RefCell<Option<Running>>>,
}

impl Tile {
    /// Builds the widgets for `camera`; nothing runs until [`Tile::start`].
    pub fn new(
        camera: &str,
        name: &str,
        runtime: Arc<tokio::runtime::Runtime>,
        stage: Rc<Stage>,
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
            camera: camera.to_owned(),
            name: name.to_owned(),
            picture,
            frame,
            label,
            runtime,
            stage,
            running: Rc::new(RefCell::new(None)),
        }
    }

    /// The widget to place in a container.
    pub fn widget(&self) -> &gtk::Box {
        &self.frame
    }

    /// The camera id.
    pub fn camera(&self) -> &str {
        &self.camera
    }

    /// The camera's display name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Builds the branch on the stage, points the picture at its sink, and
    /// starts the feed. Starting a started tile restarts it.
    pub fn start(&self, feed: Feed) -> anyhow::Result<()> {
        self.stop();
        let bin =
            gst::parse::bin_from_description(BRANCH, false).context("building the tile branch")?;
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
        // Readiness is the first buffer reaching the sink; the stage's bus
        // cannot tell, since stream-start messages aggregate over all sinks.
        if let Some(pad) = sink.static_pad("sink") {
            let ready = branch_tx.clone();
            let camera = self.camera.clone();
            pad.add_probe(gst::PadProbeType::BUFFER, move |_, _| {
                info!(camera = %camera, "first frame decoded");
                let _ = ready.try_send(BranchEvent::Ready);
                gst::PadProbeReturn::Remove
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
                    if let BranchEvent::Error(text) = event {
                        label.set_label(&format!("{name} ({camera}): {text}"));
                    }
                }
            }
        });

        let cancel = CancellationToken::new();
        let (events_tx, events_rx) = async_channel::unbounded::<TileEvent>();
        self.runtime
            .spawn(live::run(feed, Source { src }, events_tx, cancel.clone()));
        glib::spawn_future_local({
            let bin = bin.clone();
            let label = self.label.clone();
            async move {
                while let Ok(TileEvent::Restart) = events_rx.recv().await {
                    if let Err(error) = Stage::restart(&bin) {
                        warn!(%error, "branch restart failed");
                        label.set_label(&format!("restart failed: {error}"));
                    }
                }
            }
        });
        *self.running.borrow_mut() = Some(Running { bin, feed: cancel });
        Ok(())
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
    fn a_stalled_source_rejects_payloads_beyond_the_byte_limit() {
        gst::init().unwrap();
        let source = Source {
            src: gst_app::AppSrc::builder()
                .max_bytes(SOURCE_QUEUE_BYTES)
                .build(),
        };
        let payload = vec![0; 1 << 20];
        for _ in 0..16 {
            source.push(&payload).unwrap();
        }
        assert!(source.push(&payload).is_err());
        assert_eq!(source.src.current_level_bytes(), SOURCE_QUEUE_BYTES);
    }

    #[test]
    fn an_oversized_payload_is_rejected_before_copying() {
        gst::init().unwrap();
        let source = Source {
            src: gst_app::AppSrc::builder().build(),
        };
        assert!(
            source
                .push(&vec![0; SOURCE_QUEUE_BYTES as usize + 1])
                .is_err()
        );
        assert_eq!(source.src.current_level_bytes(), 0);
    }

    #[test]
    fn empty_and_excess_buffers_are_rejected_before_allocation() {
        gst::init().unwrap();
        let src = gst_app::AppSrc::builder()
            .max_bytes(SOURCE_QUEUE_BYTES)
            .build();
        src.set_property("max-buffers", SOURCE_QUEUE_BUFFERS);
        let source = Source { src };
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
