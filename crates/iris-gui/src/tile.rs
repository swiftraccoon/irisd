//! One camera tile: a branch of the shared stage pipeline from `appsrc` to
//! `gtk4paintablesink`, the widget that shows it, and the lifecycle that
//! starts and stops both the branch and the feed behind it.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::Context;
use gst::prelude::*;
use gtk::prelude::*;
use gtk::{gdk, glib};
use iris_client::live::{
    FeedEvent, LIVE_PAYLOAD_BYTES, LiveBudget, LiveMessage, LiveSink, Reservation, SinkError,
};
use iris_client::sound_clock::Medium;
use iris_core::id::CameraId;
use iris_transport::Feed;
use tokio::runtime::Runtime;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use crate::sound::{self, Pacer, Pairing};
use crate::stage::{BranchEvent, Stage};

/// Maximum queued bytes, one payload at the live protocol's ceiling
/// ([`LIVE_PAYLOAD_BYTES`]); exceeding it reconnects from an init segment and
/// sync frame.
const SOURCE_QUEUE_BYTES: u64 = LIVE_PAYLOAD_BYTES as u64;
/// Maximum queued buffers. At 30 fps this permits more than 17 seconds of backlog.
const SOURCE_QUEUE_BUFFERS: u64 = 512;
/// Default of `--live-budget-mib`: the live payload bytes alive in the viewer at
/// once, the default grid of sixteen full source queues.
pub const DEFAULT_LIVE_BUDGET_MIB: u32 = 256;
/// Least `--live-budget-mib` accepted: one full source queue, so one payload at
/// the protocol ceiling can be admitted.
pub const MIN_LIVE_BUDGET_MIB: u32 = 16;

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

/// The branch of a picture that plays with the camera's sound: [`BRANCH`]
/// with a pad after the demuxer (`shift`) where the frames' place on the
/// clock is set (`crate::sound`). Its sink renders on decode, as the grid's
/// do, until the sound's first frame arrives, and against the clock from
/// then on. `max-lateness=-1` shows a late frame at once instead of dropping
/// it: a feed starts with the frames since the last sync frame, which a
/// decoder works through for some seconds, and a sink that dropped them
/// would show nothing until it had.
const HELD_BRANCH: &str = "appsrc name=src is-live=true stream-type=stream \
    ! qtdemux ! identity name=shift ! decodebin3 ! glupload ! glcolorconvert \
    ! gtk4paintablesink name=sink sync=false async=false max-lateness=-1";

/// The branch that plays a camera's audio feed against the clock: on the
/// default output, or into a sink that discards it when the viewer is muted
/// ([`sound::silent`]). The output is chosen by `autoaudiosink`, a bin that
/// handles its sink's wait for the first buffer itself, so the branch joins
/// the running stage as the picture's does. The queue before the sink is
/// where the branch can hold what an audio output's own latency asks for:
/// without it the output reports that the branch cannot.
fn sound_branch(silent: bool) -> String {
    let sink = if silent {
        "fakesink name=sink sync=true async=false"
    } else {
        "autoaudiosink name=sink sync=true async-handling=true"
    };
    format!(
        "appsrc name=src is-live=true stream-type=stream \
         ! qtdemux ! identity name=shift ! decodebin3 ! audioconvert ! audioresample \
         ! queue ! {sink}"
    )
}

/// The feed's handle on the branch input, usable from any thread: the tile's
/// [`LiveSink`].
#[derive(Debug)]
pub struct Source {
    src: gst_app::AppSrc,
    budget: Arc<LiveBudget>,
    /// Told of every payload, when the feed plays against the clock.
    pacer: Option<Pacer>,
}

impl Source {
    /// Pushes a complete live payload without blocking the runtime. A full queue,
    /// or a viewer-wide budget that cannot admit the payload, fails the session
    /// so reconnection starts a decodable byte stream. The feed reaches it
    /// through the source's [`LiveSink::push`].
    ///
    /// # Errors
    ///
    /// - `bytes` is empty.
    /// - The queued bytes and `bytes` together exceed [`SOURCE_QUEUE_BYTES`].
    /// - At least [`SOURCE_QUEUE_BUFFERS`] buffers are queued.
    /// - The viewer's live budget cannot admit `bytes`.
    /// - `appsrc` refuses the buffer with a flow error, as it does while it
    ///   flushes.
    fn push_payload(&self, bytes: &[u8]) -> anyhow::Result<()> {
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
                self.budget.limit() >> 20
            )
        })?;
        let buffer = gst::Buffer::from_slice(Payload {
            bytes: bytes.to_vec(),
            _reservation: reservation,
        });
        if let Some(pacer) = &self.pacer {
            pacer.payload(bytes);
        }
        self.src
            .push_buffer(buffer)
            .map(|_| ())
            .map_err(|flow| anyhow::anyhow!("appsrc rejected a buffer: {flow:?}"))
    }
}

impl LiveSink for Source {
    /// Prepares for a fresh byte stream (a reconnection): flushes what is queued.
    fn reset(&self) {
        if let Some(pacer) = &self.pacer {
            pacer.restart();
        }
        let _ = self.src.send_event(gst::event::FlushStart::new());
        let _ = self
            .src
            .send_event(gst::event::FlushStop::new(/* reset_time */ true));
    }

    /// Pushes the payload of an init segment or a fragment as
    /// [`Source::push_payload`] does. When that push fails, the message is
    /// refused with the text of the push's failure. A message of any other
    /// kind is refused too, rather than skipped silently: the demuxer's byte
    /// stream may need it.
    fn push(&self, message: LiveMessage<'_>) -> Result<(), SinkError> {
        let (LiveMessage::Init(payload) | LiveMessage::Fragment(payload)) = message else {
            return Err(SinkError::new(
                "live message of a kind the tile does not play",
            ));
        };
        self.push_payload(payload)
            .map_err(|error| SinkError::new(format!("{error:#}")))
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
    /// The branch and feed of the camera's sound, when the tile plays it.
    sound: Option<(gst::Bin, CancellationToken)>,
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
    ///
    /// # Errors
    ///
    /// - The branch does not build from its description, or lacks its
    ///   `appsrc` or its sink.
    /// - The stage cannot add the branch or bring it to its state
    ///   ([`Stage::add`]).
    pub fn start(&self, feed: Feed) -> anyhow::Result<()> {
        self.start_branches(feed, None)
    }

    /// Starts the tile as [`Tile::start`] does and, beside it, the camera's
    /// audio feed `sound`: once that feed delivers, sound and picture play
    /// against one clock, [`iris_client::sound_clock::DELAY`] after they
    /// arrive. A camera without audio shows its picture as [`Tile::start`]
    /// does.
    ///
    /// # Errors
    ///
    /// - The picture's branch or the sound's does not build from its
    ///   description, or lacks its `appsrc`; the picture's lacks its sink.
    /// - Either branch lacks the pad where its frames are placed on the
    ///   clock.
    /// - The stage cannot add either branch or bring it to its state
    ///   ([`Stage::add`]).
    pub fn start_with_sound(&self, feed: Feed, sound: Feed) -> anyhow::Result<()> {
        self.start_branches(feed, Some(sound))
    }

    /// Builds one branch of the tile and its feed's input.
    fn branch(description: &str) -> anyhow::Result<(gst::Bin, gst_app::AppSrc)> {
        let bin =
            gst::parse::bin_from_description(description, /* ghost_unlinked_pads */ false)
                .context("building the tile branch")?;
        let src = bin
            .by_name("src")
            .and_then(|e| e.downcast::<gst_app::AppSrc>().ok())
            .context("appsrc missing")?;
        src.set_max_bytes(SOURCE_QUEUE_BYTES);
        src.set_property("max-buffers", SOURCE_QUEUE_BUFFERS);
        Ok((bin, src))
    }

    /// The pad of a branch where its frames' place on the clock is set.
    fn shift_pad(bin: &gst::Bin) -> anyhow::Result<gst::Pad> {
        bin.by_name("shift")
            .and_then(|shift| shift.static_pad("src"))
            .context("shift pad missing")
    }

    /// Adds the branch of the camera's sound to the stage and starts its
    /// feed; the branch restarts with each of the feed's sessions.
    fn start_sound(
        &self,
        feed: Feed,
        pairing: &Arc<Pairing>,
    ) -> anyhow::Result<(gst::Bin, CancellationToken)> {
        let (bin, src) = Self::branch(&sound_branch(sound::silent()))?;
        let pacer = pairing.pacer(Medium::Audio, Self::shift_pad(&bin)?);
        let (branch_tx, branch_rx) = async_channel::unbounded::<BranchEvent>();
        self.stage
            .add(&format!("sound-{}", self.camera), &bin, branch_tx)?;
        glib::spawn_future_local({
            let camera = self.camera.clone();
            async move {
                while let Ok(event) = branch_rx.recv().await {
                    if let BranchEvent::Error(text) = event {
                        warn!(camera = %camera, error = %text, "sound branch error");
                    }
                }
            }
        });
        let cancel = CancellationToken::new();
        let (events_tx, events_rx) = async_channel::unbounded::<FeedEvent>();
        self.runtime.spawn(feed.run(
            Arc::new(Source {
                src,
                budget: Arc::clone(&self.budget),
                pacer: Some(pacer),
            }),
            events_tx,
            cancel.clone(),
        ));
        glib::spawn_future_local({
            let bin = bin.clone();
            let camera = self.camera.clone();
            async move {
                while let Ok(event) = events_rx.recv().await {
                    if event == FeedEvent::Restart
                        && let Err(error) = Stage::restart(&bin)
                    {
                        warn!(
                            camera = %camera,
                            error = %format!("{error:#}"),
                            "sound branch restart failed"
                        );
                    }
                }
            }
        });
        Ok((bin, cancel))
    }

    /// Pairs the picture branch `bin` with the camera's sound: starts the
    /// sound's branch and feed, and returns the picture's pacer with them.
    /// The picture's `sink` renders against the clock from the sound's
    /// first frame on.
    fn pair_with_sound(
        &self,
        bin: &gst::Bin,
        sink: &gst::Element,
        sound: Feed,
    ) -> anyhow::Result<(Pacer, (gst::Bin, CancellationToken))> {
        let pairing = Pairing::new(self.stage.pipeline(), {
            let sink = sink.clone();
            let camera = self.camera.clone();
            move || {
                info!(camera = %camera, "playing with sound");
                sink.set_property("sync", true);
            }
        });
        let pacer = pairing.pacer(Medium::Video, Self::shift_pad(bin)?);
        Ok((pacer, self.start_sound(sound, &pairing)?))
    }

    fn start_branches(&self, feed: Feed, sound: Option<Feed>) -> anyhow::Result<()> {
        self.stop();
        let description = if sound.is_some() { HELD_BRANCH } else { BRANCH };
        let (bin, src) = Self::branch(description)?;
        let sink = bin.by_name("sink").context("sink missing")?;
        let (pacer, sound_running) = match sound {
            Some(sound) => {
                let (pacer, running) = self.pair_with_sound(&bin, &sink, sound)?;
                (Some(pacer), Some(running))
            }
            None => (None, None),
        };
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
        let (events_tx, events_rx) = async_channel::unbounded::<FeedEvent>();
        self.runtime.spawn(feed.run(
            Arc::new(Source {
                src,
                budget: Arc::clone(&self.budget),
                pacer,
            }),
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
                        FeedEvent::Restart => {
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
                        FeedEvent::Oversized { retry_in } => {
                            label.set_label(&oversized_label(&name, &camera, retry_in));
                        }
                        FeedEvent::Live => label.set_label(&format!("{name} ({camera})")),
                    }
                }
            }
        });
        *self.running.borrow_mut() = Some(Running {
            bin,
            feed: cancel,
            sound: sound_running,
        });
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

    /// Whether the tile runs a branch and feed for the camera's sound.
    #[cfg(test)]
    pub(crate) fn has_sound(&self) -> bool {
        self.running
            .borrow()
            .as_ref()
            .is_some_and(|running| running.sound.is_some())
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
            if let Some((bin, feed)) = running.sound {
                feed.cancel();
                self.stage.remove(&bin);
            }
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
    use iris_client::sound_clock::DELAY;

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

    /// Both sound branches build: into the output `autoaudiosink` chooses,
    /// and into the sink that discards. Each has the pad where the frames
    /// are placed on the clock, a queue before the sink, and a sink that
    /// plays against the clock.
    #[test]
    #[ignore = "requires GStreamer's demuxer and audio elements; `just display-test` runs it"]
    fn both_sound_branches_build() {
        gst::init().unwrap();
        for silent in [true, false] {
            let description = sound_branch(silent);
            assert!(description.contains("! queue ! "), "{description}");
            let (bin, _src) = Tile::branch(&description).unwrap();
            Tile::shift_pad(&bin).unwrap();
            let sink = bin.by_name("sink").unwrap();
            assert!(sink.property::<bool>("sync"), "{description}");
            let factory = sink.factory().unwrap().name();
            assert_eq!(factory, if silent { "fakesink" } else { "autoaudiosink" });
        }
    }

    /// The `prft` box a live fragment starts with, for the media time
    /// `ticks`.
    fn prft(ticks: u64) -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&32_u32.to_be_bytes());
        bytes.extend_from_slice(b"prft");
        bytes.extend_from_slice(&[1, 0, 0, 0x18, 0, 0, 0, 1]);
        bytes.extend_from_slice(&0_u64.to_be_bytes());
        bytes.extend_from_slice(&ticks.to_be_bytes());
        bytes
    }

    /// A source that plays against the clock of a playing pipeline: its
    /// pacer, the sound's, sets the offset of `pad`.
    struct PacedSource {
        pipeline: gst::Pipeline,
        pad: gst::Pad,
        source: Source,
    }

    impl PacedSource {
        fn new() -> Self {
            Self::with_budget(u64::MAX)
        }

        /// A paced source whose live budget admits `bytes`.
        fn with_budget(bytes: u64) -> Self {
            gst::init().unwrap();
            let pipeline = gst::Pipeline::new();
            pipeline.set_state(gst::State::Playing).unwrap();
            let (_, state, _) = pipeline.state(gst::ClockTime::from_seconds(5));
            assert_eq!(state, gst::State::Playing);
            let pairing = Pairing::new(&pipeline, || {});
            let pad = gst::Pad::builder(gst::PadDirection::Src).build();
            let source = Source {
                src: gst_app::AppSrc::builder()
                    .max_bytes(SOURCE_QUEUE_BYTES)
                    .build(),
                budget: Arc::new(LiveBudget::new(bytes)),
                pacer: Some(pairing.pacer(Medium::Audio, pad.clone())),
            };
            Self {
                pipeline,
                pad,
                source,
            }
        }

        /// Pushes the feed's first fragment and, 40 ms on, its second, and
        /// returns the offset the first one set.
        fn first_two_fragments(&self) -> i64 {
            self.source.push_payload(&prft(90_000_000)).unwrap();
            let first = self.pad.offset();
            std::thread::sleep(Duration::from_millis(40));
            self.source.push_payload(&prft(90_003_600)).unwrap();
            first
        }
    }

    impl Drop for PacedSource {
        fn drop(&mut self) {
            let _ = self.pipeline.set_state(gst::State::Null);
        }
    }

    /// A source that plays against the clock tells its pacer of a fragment
    /// it pushes: the feed's first fragment places the feed.
    #[test]
    fn a_source_tells_its_pacer_of_a_fragment() {
        let paced = PacedSource::new();
        paced.source.push_payload(&prft(90_000_000)).unwrap();
        let first = paced.pad.offset();
        let delay_ns = i64::try_from(DELAY.as_nanos()).unwrap();
        assert!(first >= delay_ns, "placed {first} ns in");
    }

    /// A source tells its pacer of no payload it refuses: a fragment that
    /// the live budget cannot admit leaves the feed unplaced.
    #[test]
    fn a_refused_fragment_leaves_the_feed_unplaced() {
        let paced = PacedSource::with_budget(0);
        let _ = paced.source.push_payload(&prft(90_000_000));
        assert_eq!(
            paced.pad.offset(),
            0,
            "the refused fragment placed the feed"
        );
    }

    /// The pacer's estimate holds when the feed's second fragment arrives on
    /// time: the source's offset stays where the first fragment put it.
    #[test]
    fn a_second_fragment_on_time_leaves_the_offset() {
        let paced = PacedSource::new();
        let first = paced.first_two_fragments();
        assert!(
            (paced.pad.offset() - first).abs() < 5_000_000,
            "the estimate held"
        );
    }

    /// A source tells its pacer of a fresh byte stream: after a reset, the
    /// feed's next fragment places the feed again, on another run's
    /// timeline.
    #[test]
    fn a_reset_source_tells_its_pacer_of_a_fresh_stream() {
        let paced = PacedSource::new();
        let first = paced.first_two_fragments();
        paced.source.reset();
        paced.source.push_payload(&prft(9_000)).unwrap();
        assert!(paced.pad.offset() >= first + 30_000_000, "placed anew");
    }

    /// A source whose queue holds [`SOURCE_QUEUE_BYTES`], in payloads of
    /// 1 MiB.
    fn source_with_a_full_queue() -> Source {
        gst::init().unwrap();
        let source = Source {
            src: gst_app::AppSrc::builder()
                .max_bytes(SOURCE_QUEUE_BYTES)
                .build(),
            budget: Arc::new(LiveBudget::new(u64::MAX)),
            pacer: None,
        };
        for _ in 0..16 {
            source.push_payload(&vec![0; 1 << 20]).unwrap();
        }
        source
    }

    /// A full queue refuses a payload with the text of its byte limit.
    #[test]
    fn a_full_queue_rejects_a_payload() {
        let source = source_with_a_full_queue();
        let refused = source.push_payload(&vec![0; 1 << 20]).unwrap_err();
        assert_eq!(
            refused.to_string(),
            "live input queue exceeded its byte limit"
        );
    }

    #[test]
    fn a_payload_a_full_queue_rejects_is_not_queued() {
        let source = source_with_a_full_queue();
        let _ = source.push_payload(&vec![0; 1 << 20]);
        assert_eq!(source.src.current_level_bytes(), SOURCE_QUEUE_BYTES);
    }

    fn budgeted_source(budget: &Arc<LiveBudget>) -> Source {
        Source {
            src: gst_app::AppSrc::builder()
                .max_bytes(SOURCE_QUEUE_BYTES)
                .build(),
            budget: Arc::clone(budget),
            pacer: None,
        }
    }

    /// A budget of 2 MiB and two sources on it, each of which has queued a
    /// payload of 1 MiB.
    fn two_sources_on_a_full_budget() -> (Arc<LiveBudget>, Source, Source) {
        gst::init().unwrap();
        let budget = Arc::new(LiveBudget::new(2 << 20));
        let (first, second) = (budgeted_source(&budget), budgeted_source(&budget));
        let payload = vec![0; 1 << 20];
        first.push_payload(&payload).unwrap();
        second.push_payload(&payload).unwrap();
        (budget, first, second)
    }

    #[test]
    fn the_live_budget_counts_the_payloads_of_every_source() {
        let (budget, _first, _second) = two_sources_on_a_full_budget();
        assert_eq!(budget.used(), 2 << 20);
    }

    /// A source whose payload the shared budget cannot admit refuses it with
    /// the budget's text, although the source's queue has room.
    #[test]
    fn a_full_live_budget_refuses_a_payload() {
        let (_budget, first, _second) = two_sources_on_a_full_budget();
        let refused = first.push_payload(&[1]).unwrap_err();
        assert!(refused.to_string().contains("live budget"), "{refused}");
    }

    #[test]
    fn a_payload_the_budget_refuses_is_not_queued() {
        let (_budget, first, _second) = two_sources_on_a_full_budget();
        let _ = first.push_payload(&[1]);
        assert_eq!(
            first.src.current_level_bytes(),
            1 << 20,
            "the refused payload was not queued"
        );
    }

    #[test]
    fn freeing_queued_buffers_returns_their_budget() {
        let (budget, first, second) = two_sources_on_a_full_budget();
        drop(first);
        assert_eq!(
            budget.used(),
            1 << 20,
            "freeing the queued buffers returns their bytes"
        );
        second.push_payload(&vec![0; 1 << 20]).unwrap();
        drop(second);
        assert_eq!(budget.used(), 0);
    }

    #[test]
    fn a_freed_budget_admits_a_payload_again() {
        let (budget, first, second) = two_sources_on_a_full_budget();
        drop(first);
        second.push_payload(&vec![0; 1 << 20]).unwrap();
        assert_eq!(budget.used(), 2 << 20);
    }

    /// A source over an `appsrc` with its default limits.
    fn default_source() -> Source {
        gst::init().unwrap();
        Source {
            src: gst_app::AppSrc::builder().build(),
            budget: Arc::new(LiveBudget::new(u64::MAX)),
            pacer: None,
        }
    }

    /// A payload one byte above the live protocol's ceiling.
    fn oversized_payload() -> Vec<u8> {
        vec![0; SOURCE_QUEUE_BYTES as usize + 1]
    }

    /// A payload above the byte limit is refused with the text of that
    /// limit, although the queue is empty.
    #[test]
    fn an_oversized_payload_is_rejected() {
        let refused = default_source()
            .push_payload(&oversized_payload())
            .unwrap_err();
        assert_eq!(
            refused.to_string(),
            "live input queue exceeded its byte limit"
        );
    }

    #[test]
    fn an_oversized_payload_is_not_queued() {
        let source = default_source();
        let _ = source.push_payload(&oversized_payload());
        assert_eq!(source.src.current_level_bytes(), 0);
    }

    /// A source with the queue limits a tile's branch gives its `appsrc`.
    fn buffer_limited_source() -> Source {
        gst::init().unwrap();
        let src = gst_app::AppSrc::builder()
            .max_bytes(SOURCE_QUEUE_BYTES)
            .build();
        src.set_property("max-buffers", SOURCE_QUEUE_BUFFERS);
        assert_eq!(src.property::<u64>("max-buffers"), SOURCE_QUEUE_BUFFERS);
        Source {
            src,
            budget: Arc::new(LiveBudget::new(u64::MAX)),
            pacer: None,
        }
    }

    /// A source whose queue holds [`SOURCE_QUEUE_BUFFERS`] buffers.
    fn source_at_its_buffer_limit() -> Source {
        let source = buffer_limited_source();
        for _ in 0..SOURCE_QUEUE_BUFFERS {
            source.push_payload(&[1]).unwrap();
        }
        source
    }

    fn queued_buffers(source: &Source) -> u64 {
        source.src.property::<u64>("current-level-buffers")
    }

    #[test]
    fn an_empty_payload_is_rejected() {
        assert!(buffer_limited_source().push_payload(&[]).is_err());
    }

    #[test]
    fn an_empty_payload_is_not_queued() {
        let source = buffer_limited_source();
        let _ = source.push_payload(&[]);
        assert_eq!(queued_buffers(&source), 0);
    }

    #[test]
    fn a_source_queues_up_to_its_buffer_limit() {
        let source = source_at_its_buffer_limit();
        assert_eq!(queued_buffers(&source), SOURCE_QUEUE_BUFFERS);
    }

    /// A queue at its buffer limit refuses a payload with the text of that
    /// limit.
    #[test]
    fn a_payload_beyond_the_buffer_limit_is_rejected() {
        let refused = source_at_its_buffer_limit().push_payload(&[1]).unwrap_err();
        assert_eq!(
            refused.to_string(),
            "live input queue exceeded its buffer limit"
        );
    }

    #[test]
    fn a_payload_beyond_the_buffer_limit_is_not_queued() {
        let source = source_at_its_buffer_limit();
        let _ = source.push_payload(&[1]);
        assert_eq!(queued_buffers(&source), SOURCE_QUEUE_BUFFERS);
    }

    /// An `appsrc` after its end of stream refuses a buffer with a flow
    /// error, and the source refuses the payload with the name of the flow.
    #[test]
    fn a_flow_error_of_appsrc_refuses_the_payload() {
        let source = default_source();
        source.src.end_of_stream().unwrap();
        let refused = source.push_payload(&[1]).unwrap_err();
        assert_eq!(refused.to_string(), "appsrc rejected a buffer: Eos");
    }

    /// A fragment whose push fails is refused with the text of the push's
    /// failure: for an empty one, "live input payload is empty".
    #[test]
    fn a_refused_push_has_the_text_of_its_failure() {
        gst::init().unwrap();
        let source = budgeted_source(&Arc::new(LiveBudget::new(u64::MAX)));
        let refused = LiveSink::push(&source, LiveMessage::Fragment(&[])).unwrap_err();
        assert_eq!(refused.to_string(), "live input payload is empty");
    }

    /// The source, as the feed's [`LiveSink`], queues the payloads of an init
    /// segment and of a fragment alike: four bytes and one.
    #[test]
    fn a_source_queues_the_payload_of_either_message_kind() {
        let source = default_source();
        LiveSink::push(&source, LiveMessage::Init(&[0, 0, 0, 8])).unwrap();
        LiveSink::push(&source, LiveMessage::Fragment(&[1])).unwrap();
        assert_eq!(source.src.current_level_bytes(), 5);
    }
}
