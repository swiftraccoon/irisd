//! One RTSP session at a time per stream, reconnecting forever.

use std::num::NonZeroU16;
use std::num::NonZeroU32;
use std::pin::Pin;
use std::sync::{Arc, OnceLock};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use bytes::Bytes;
use futures::Stream as _;
use futures::StreamExt as _;
use iris_core::id::{StreamKey, StreamKind};
use iris_core::text::LogSafe;
use iris_core::time::WallUs;
use iris_mp4::fragment::Fragment;
use iris_mp4::init::{SampleEntry, audio_frame_ticks, build_init_segment};
use retina::client::{
    Credentials, Demuxed, PlayOptions, Session, SessionOptions, SetupOptions, TcpTransportOptions,
    TeardownPolicy, Transport,
};
use retina::codec::{CodecItem, FrameFormat, ParametersRef, VideoFrame, VideoParameters};
use retina::rtcp::ReceivedCompoundPacket;
use thiserror::Error;
use tokio::sync::Notify;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::assembler::{FragmentAssembler, Frame};
use crate::audio::{AudioAssembler, AudioFrame, Pushed, VideoSpan};
use crate::cadence::{self, Cadence};
use crate::live::{LiveFrame, LiveRing};
use crate::regularize::{Regularizer, SNAP_TOLERANCE};
use crate::sink::FragmentSink;
use crate::source::{IngestOptions, StreamSource};
use crate::stats::StreamStats;
use crate::steer::{Rule, Side, Verdict};
use crate::sync::{LeastWatch, Place, epoch_us, near};

/// Longest pause between reconnection attempts.
const MAX_BACKOFF: Duration = Duration::from_secs(30);
/// How long a DESCRIBE, SETUP, or PLAY may take before the session is abandoned.
const SETUP_TIMEOUT: Duration = Duration::from_secs(15);
/// The longest a task waits on a quiet camera, or rests between attempts,
/// before it notes that it is alive ([`StreamStats::beats`]).
const LIVENESS_SLICE: Duration = Duration::from_secs(5);
/// A session that lasted this long resets the backoff.
const HEALTHY_SESSION: Duration = Duration::from_secs(60);
/// A hold of at least this many ticks (one second) is logged as a gap.
const GAP_HOLD_TICKS: i64 = 90_000;
/// A recorder realtime step larger than this ends the run.
const CLOCK_STEP_US: u64 = 2_000_000;

/// Why a session ended.
#[derive(Debug, Error)]
enum SessionError {
    #[error("RTSP: {0}")]
    Rtsp(#[from] retina::Error),
    #[error("no H.265 video stream in the SDP")]
    NoH265Stream,
    #[error("video parameters unavailable at the first sync frame")]
    MissingParameters,
    /// An RTSP request went unanswered.
    #[error("{0} not answered within {SETUP_TIMEOUT:?}")]
    SetupTimeout(&'static str),
    /// No RTSP data at all, neither frames nor RTCP, for the inactivity timeout.
    #[error("no data for {0:?}")]
    Inactivity(Duration),
    #[error("camera closed the stream")]
    EndOfStream,
    #[error("recorder realtime clock stepped")]
    ClockStep,
}

/// Runs the stream until `shutdown` is cancelled, reconnecting after failures.
pub async fn run_stream(
    source: StreamSource,
    options: IngestOptions,
    sink: Arc<dyn FragmentSink>,
    stats: Arc<StreamStats>,
    shutdown: CancellationToken,
) {
    let mut backoff = Duration::from_secs(1);
    let mut attempt = 0u64;
    loop {
        if shutdown.is_cancelled() {
            return;
        }
        stats.beat();
        let started = Instant::now();
        // The session owns cancellation once it starts so its pending fragment
        // is flushed before the session state can be dropped.
        let outcome = run_session(&source, &options, sink.as_ref(), &stats, &shutdown).await;
        stats.set_connected(false);
        match outcome {
            Ok(()) => return,
            Err(error) => {
                // Session errors quote camera-supplied text.
                warn!(
                    stream = %source.key,
                    error = %LogSafe(&error),
                    "session ended; reconnecting"
                );
                stats.record_reconnect();
            }
        }
        let delay = next_retry_delay(
            &source.key,
            &mut backoff,
            &mut attempt,
            started.elapsed() >= HEALTHY_SESSION,
        );
        if !rest(delay, &stats, &shutdown).await {
            return;
        }
    }
}

/// Rests `delay` between attempts, noting every [`LIVENESS_SLICE`] of it
/// that the task is alive; `false` when shutdown ends the rest.
async fn rest(delay: Duration, stats: &StreamStats, shutdown: &CancellationToken) -> bool {
    let until = tokio::time::Instant::now() + delay;
    loop {
        let left = until.saturating_duration_since(tokio::time::Instant::now());
        if left.is_zero() {
            return true;
        }
        tokio::select! {
            () = shutdown.cancelled() => return false,
            () = tokio::time::sleep(left.min(LIVENESS_SLICE)) => stats.beat(),
        }
    }
}

fn next_retry_delay(
    key: &StreamKey,
    backoff: &mut Duration,
    attempt: &mut u64,
    healthy: bool,
) -> Duration {
    if healthy {
        *backoff = Duration::from_secs(1);
        *attempt = 0;
    }
    let delay = jittered(*backoff, key, *attempt);
    *attempt = attempt.wrapping_add(1);
    *backoff = (*backoff * 2).min(MAX_BACKOFF);
    delay
}

/// Applies deterministic ±20 % jitter from stream identity and retry attempt.
fn jittered(base: Duration, key: &StreamKey, attempt: u64) -> Duration {
    let mut seed = 0xcbf2_9ce4_8422_2325u64;
    for byte in key.camera.as_str().bytes().chain(key.kind.as_str().bytes()) {
        seed ^= u64::from(byte);
        seed = seed.wrapping_mul(0x100_0000_01b3);
    }
    seed ^= attempt.wrapping_mul(0x9e37_79b9_7f4a_7c15);
    seed ^= seed >> 30;
    seed = seed.wrapping_mul(0xbf58_476d_1ce4_e5b9);
    seed ^= seed >> 27;
    seed = seed.wrapping_mul(0x94d0_49bb_1331_11eb);
    seed ^= seed >> 31;
    let unit = (seed % 401) as i64 - 200; // -200..=200
    let scale = 1000 + unit; // per-mille: 800..=1200
    let micros = base.as_micros() as u64 * scale as u64 / 1000;
    Duration::from_micros(micros)
}

/// Describes, sets up, plays, and demuxes one session. `Ok(())` means
/// shutdown was requested.
async fn run_session(
    source: &StreamSource,
    options: &IngestOptions,
    sink: &dyn FragmentSink,
    stats: &StreamStats,
    shutdown: &CancellationToken,
) -> Result<(), SessionError> {
    let Some(StartedSession {
        mut demuxed,
        nominal,
        stream_index,
        audio_index,
    }) = start_session(source, options, stats, shutdown).await?
    else {
        return Ok(());
    };
    stats.set_connected(true);
    info!(stream = %source.key, "playing");

    let mut session_state = SessionState {
        clock: WallClock::new(),
        assembler: None,
        nominal,
        snaps_seen: 0,
        holds_seen: (0, 0),
        previous_frame: stats.last_frame_wall(),
        stream_index,
        sink_loss: false,
        last_live_decode_time: None,
        cadence: Cadence::default(),
        audio: audio_index.map(|index| AudioTrack {
            index,
            key: StreamKey {
                camera: source.key.camera.clone(),
                kind: StreamKind::Audio,
            },
            assembler: None,
            refused: false,
            cadence: Cadence::default(),
            frames: 0,
            live: options.audio_live.clone(),
        }),
        watches: Watches::new(options.watch_window, options.audio_steps),
    };

    let mut last_item = Instant::now();
    let outcome = loop {
        if shutdown.is_cancelled() {
            break Ok(());
        }
        let left = options
            .inactivity_timeout
            .saturating_sub(last_item.elapsed());
        let item = match read_item(&mut demuxed, options, left, shutdown).await {
            Read::Item(item) => item,
            Read::Shutdown => break Ok(()),
            Read::Quiet => {
                stats.beat();
                if last_item.elapsed() >= options.inactivity_timeout {
                    break Err(SessionError::Inactivity(options.inactivity_timeout));
                }
                continue;
            }
        };
        stats.beat();
        last_item = Instant::now();
        let Some(item) = item else {
            break Err(SessionError::EndOfStream);
        };
        let item = match item {
            Ok(item) => item,
            Err(error) => break Err(SessionError::Rtsp(error)),
        };
        let result = match item {
            CodecItem::VideoFrame(frame) => {
                session_state.on_frame(frame, &demuxed, source, options, sink, stats)
            }
            CodecItem::AudioFrame(frame) => {
                session_state.on_audio(&frame, &demuxed, stats);
                Ok(())
            }
            CodecItem::Rtcp(packet) => session_state.on_rtcp(&packet, &demuxed, &source.key, stats),
            _ => Ok(()),
        };
        if let Err(error) = result {
            break Err(error);
        }
    };
    session_state.flush(sink, source, stats);
    outcome
}

struct StartedSession {
    demuxed: Demuxed,
    nominal: Option<u32>,
    stream_index: usize,
    /// The AAC stream set up beside the video, when audio is recorded.
    audio_index: Option<usize>,
}

/// Returns no stream when cancellation interrupts setup.
async fn start_session(
    source: &StreamSource,
    options: &IngestOptions,
    stats: &StreamStats,
    shutdown: &CancellationToken,
) -> Result<Option<StartedSession>, SessionError> {
    let session_options = SessionOptions::default()
        .creds(Some(Credentials {
            username: source.username.clone(),
            password: source.password.clone(),
        }))
        .user_agent(options.user_agent.clone())
        // Cameras with small session tables free a torn-down session at once but
        // keep a silently closed one until its timeout, so always say goodbye.
        .teardown(TeardownPolicy::Always);
    let mut session = tokio::select! {
        () = shutdown.cancelled() => return Ok(None),
        described = tokio::time::timeout(
            SETUP_TIMEOUT,
            Session::describe(source.url.clone(), session_options),
        ) => described,
    }
    .map_err(|_| SessionError::SetupTimeout("DESCRIBE"))??;
    stats.beat();
    let stream_index = session
        .streams()
        .iter()
        .position(|s| s.media() == "video" && s.encoding_name().eq_ignore_ascii_case("h265"))
        .ok_or(SessionError::NoH265Stream)?;
    let audio_index = if options.audio {
        session.streams().iter().position(|s| {
            s.media() == "audio" && s.encoding_name().eq_ignore_ascii_case("mpeg4-generic")
        })
    } else {
        None
    };
    // What else the camera offers (audio, metadata) is not set up; one line
    // each records it, for at most a few.
    for (_, offered) in session
        .streams()
        .iter()
        .enumerate()
        .filter(|(index, _)| *index != stream_index && Some(*index) != audio_index)
        .take(MAX_OFFERED_LOGGED)
    {
        info!(
            stream = %source.key,
            media = %LogSafe(&clipped(offered.media())),
            encoding = %LogSafe(&clipped(offered.encoding_name())),
            clock_rate = offered.clock_rate_hz(),
            channels = offered.channels().map_or(0, NonZeroU16::get),
            "offered, not recorded"
        );
    }
    let declared_nominal = session.streams()[stream_index]
        .framerate()
        .and_then(nominal_from_fps);
    tokio::select! {
        () = shutdown.cancelled() => return Ok(None),
        setup = tokio::time::timeout(
            SETUP_TIMEOUT,
            session.setup(
                stream_index,
                SetupOptions::default()
                    .transport(Transport::Tcp(TcpTransportOptions::default()))
                    .frame_format(FrameFormat::MP4),
            ),
        ) => setup,
    }
    .map_err(|_| SessionError::SetupTimeout("SETUP"))??;
    stats.beat();
    if let Some(index) = audio_index {
        tokio::select! {
            () = shutdown.cancelled() => return Ok(None),
            setup = tokio::time::timeout(
                SETUP_TIMEOUT,
                session.setup(
                    index,
                    SetupOptions::default()
                        .transport(Transport::Tcp(TcpTransportOptions::default()))
                        .frame_format(FrameFormat::MP4),
                ),
            ) => setup,
        }
        .map_err(|_| SessionError::SetupTimeout("SETUP"))??;
        stats.beat();
    }
    let max_jump_secs = u32::try_from(options.max_jump.as_secs()).unwrap_or(u32::MAX);
    let max_jump = NonZeroU32::new(max_jump_secs.max(1)).expect("at least one second");
    let mut play_options = PlayOptions::default().enforce_timestamps_with_max_jump_secs(max_jump);
    if let Some(bytes) = options.recv_low_water {
        play_options = play_options.recv_low_water(bytes);
    }
    // Timer-driven reads poll the socket on the tick and never wait for
    // readiness, so the socket leaves the reactor: epoll's edge-triggered
    // wake-ups fire per arriving segment while a socket sits above the
    // low-water mark, thousands per second across the streams, all unused.
    play_options = play_options.detach_from_reactor(options.read_interval.is_some());
    let playing = tokio::select! {
        () = shutdown.cancelled() => return Ok(None),
        played = tokio::time::timeout(SETUP_TIMEOUT, session.play(play_options)) => played,
    }
    .map_err(|_| SessionError::SetupTimeout("PLAY"))??;
    stats.beat();
    Ok(Some(StartedSession {
        demuxed: playing.demuxed()?,
        nominal: declared_nominal,
        stream_index,
        audio_index,
    }))
}

/// Waits for the shared ticker when the caller provided one, else for this
/// stream's own timer at the same aligned instant.
async fn next_tick(ticker: Option<&ReadTicker>, interval: Duration) {
    match ticker {
        Some(ticker) => ticker.tick().await,
        None => tokio::time::sleep_until(aligned_tick(interval).into()).await,
    }
}

/// What one read of a session gave.
enum Read {
    /// An item, or the end of the stream.
    Item(Option<Result<CodecItem, retina::Error>>),
    /// Nothing within the wait: a read tick, or [`LIVENESS_SLICE`] of a
    /// camera that sends nothing.
    Quiet,
    /// Shutdown ended the wait.
    Shutdown,
}

/// Reads the next item of a session in the read mode of `options`, with
/// `left` of the inactivity timeout to go. A wait on a quiet camera ends
/// within [`LIVENESS_SLICE`], so the task notes that it is alive while it
/// waits out that timeout.
async fn read_item(
    demuxed: &mut Demuxed,
    options: &IngestOptions,
    left: Duration,
    shutdown: &CancellationToken,
) -> Read {
    if let Some(interval) = options.read_interval {
        if let Poll::Ready(item) = poll_now(demuxed) {
            return Read::Item(item);
        }
        tokio::select! {
            () = shutdown.cancelled() => Read::Shutdown,
            () = next_tick(options.ticker.as_ref(), interval) => Read::Quiet,
        }
    } else {
        tokio::select! {
            () = shutdown.cancelled() => Read::Shutdown,
            item = tokio::time::timeout(left.min(LIVENESS_SLICE), demuxed.next()) => {
                item.map_or(Read::Quiet, Read::Item)
            }
        }
    }
}

/// Polls the demuxer once without arming a wake-up: whatever is already
/// readable comes out, and `Pending` means the socket is drained until more arrives.
fn poll_now(demuxed: &mut Demuxed) -> Poll<Option<Result<CodecItem, retina::Error>>> {
    let mut cx = Context::from_waker(futures::task::noop_waker_ref());
    Pin::new(demuxed).poll_next(&mut cx)
}

/// One timer task that wakes every stream waiting for the next read tick.
///
/// One timer instead of one per stream matters: each timer a stream re-arms
/// with a deadline earlier than the runtime's stale estimate of its next
/// wake-up makes the runtime unpark its I/O driver (an `eventfd` write and an
/// `epoll_wait` return), which with a timer per stream was one unpark per
/// stream per tick. The ticker belongs to the runtime it was spawned on; its
/// task stops when the last clone is dropped.
#[derive(Clone, Debug)]
pub struct ReadTicker {
    notify: Arc<Notify>,
    _task: Arc<TickerTask>,
}

#[derive(Debug)]
struct TickerTask(JoinHandle<()>);

impl Drop for TickerTask {
    fn drop(&mut self) {
        self.0.abort();
    }
}

impl ReadTicker {
    /// Spawns the ticker on the current runtime, ticking at every multiple of
    /// `interval` on the shared clock.
    ///
    /// # Panics
    ///
    /// Panics when called outside a Tokio runtime (it calls [`tokio::spawn`]).
    #[must_use]
    pub fn spawn(interval: Duration) -> Self {
        let notify = Arc::new(Notify::new());
        let task = tokio::spawn({
            let notify = Arc::clone(&notify);
            async move {
                loop {
                    tokio::time::sleep_until(aligned_tick(interval).into()).await;
                    notify.notify_waiters();
                }
            }
        });
        Self {
            notify,
            _task: Arc::new(TickerTask(task)),
        }
    }

    /// Completes at the next tick.
    pub async fn tick(&self) {
        self.notify.notified().await;
    }
}

/// The next multiple of `interval` on a clock shared by every stream in the
/// process, so streams with the same interval wake together.
fn aligned_tick(interval: Duration) -> Instant {
    static EPOCH: OnceLock<Instant> = OnceLock::new();
    let epoch = *EPOCH.get_or_init(Instant::now);
    let interval_nanos = interval.as_nanos().max(1);
    let ticks = epoch.elapsed().as_nanos() / interval_nanos + 1;
    epoch + Duration::from_nanos(u64::try_from(ticks * interval_nanos).unwrap_or(u64::MAX))
}

/// Per-session mutable state kept out of the demux loop for readability.
struct SessionState {
    clock: WallClock,
    assembler: Option<FragmentAssembler>,
    nominal: Option<u32>,
    snaps_seen: u64,
    /// Holes and hold ticks already reported from the assembler's totals.
    holds_seen: (u64, u64),
    /// Wall time of the last frame before this session, until the first frame
    /// of this session reports the gap.
    previous_frame: Option<WallUs>,
    stream_index: usize,
    /// Whether a rejected recording fragment has not been followed by an accepted run.
    sink_loss: bool,
    /// Decode time used to keep live publication moving while recording waits for sync.
    last_live_decode_time: Option<u64>,
    /// The per-frame steps of the group being received, when they are logged.
    cadence: Cadence,
    /// The audio stream recorded beside the video, if any.
    audio: Option<AudioTrack>,
    watches: Watches,
}

/// Logs what the drift rule named for a window, unless it is to hold: as
/// made where the stream's steps are made, and as what would be made where
/// they are only said.
fn log_verdict(key: &StreamKey, makes: bool, verdict: Verdict) {
    match verdict {
        Verdict::Hold => {}
        Verdict::Step {
            side,
            frames,
            error,
        } => {
            let step = match side {
                Side::Late => "left out",
                Side::Early => "recorded twice",
            };
            if makes {
                info!(
                    stream = %key,
                    frames,
                    step,
                    error_us = error,
                    "a step of the audio is made"
                );
            } else {
                info!(
                    stream = %key,
                    frames,
                    step,
                    error_us = error,
                    "a step of the audio would be made"
                );
            }
        }
        Verdict::Stop(why) if makes => info!(
            stream = %key,
            reason = why.as_str(),
            "no further step of the audio is made in this run"
        ),
        Verdict::Stop(why) => info!(
            stream = %key,
            reason = why.as_str(),
            "no further step of the audio would be made in this run"
        ),
    }
}

/// The session's watches of parted clocks (`docs/research/14-drift.md`).
/// Each reports once a window; the audio's place goes to the drift rule,
/// whose steps are made where the stream's options say so.
struct Watches {
    /// The length of a window in 90 kHz ticks.
    window_ticks: u64,
    /// The video's receipt times against its timestamps.
    video_clock: LeastWatch,
    /// The audio's receipt times against its timestamps.
    audio_clock: LeastWatch,
    /// The recorded audio against the video, by the sender reports and by
    /// arrival.
    place: Place,
    /// What takes a drift of the audio out; made at the first window of
    /// each audio run.
    rule: Option<Rule>,
    /// Whether the steps the rule names are made on the recording.
    makes_steps: bool,
    /// The whole timestamp of the video frame received last.
    video_stamp: i64,
    /// Whether the lack of sender reports for both streams was said.
    unmeasured_said: bool,
}

impl Watches {
    fn new(window: Duration, makes_steps: bool) -> Self {
        let micros = u64::try_from(window.as_micros()).unwrap_or(u64::MAX);
        let window_ticks = micros.saturating_mul(9) / 100;
        Self {
            window_ticks,
            video_clock: LeastWatch::new(micros),
            audio_clock: LeastWatch::new(micros),
            place: Place::new(window_ticks),
            rule: None,
            makes_steps,
            video_stamp: 0,
            unmeasured_said: false,
        }
    }

    /// Notes a video frame received at `wall` whose place in the recording
    /// is `decode_time`, if it has one yet.
    fn arrived(&mut self, wall: WallUs, decode_time: Option<u64>) {
        if let Some(decode_time) = decode_time {
            self.place.video_frame(wall.get() - ticks_us(decode_time));
        }
    }

    /// Starts the audio's place and its rule over: a run is placed anew,
    /// so where it stands is read from its own first window.
    fn start_run(&mut self) {
        self.place = Place::new(self.place.span());
        self.rule = None;
    }

    /// Notes a frame of `key` received at `wall` whose timestamp is
    /// `stamp_us` on the camera's clock, and reports a window that closes.
    fn clock(watch: &mut LeastWatch, key: &StreamKey, wall: WallUs, stamp_us: i64) -> Option<i64> {
        let position = u64::try_from(wall.get()).unwrap_or(0);
        let window = watch.note(position, wall.get().saturating_sub(stamp_us))?;
        info!(
            stream = %key,
            moved_us = window.moved,
            spread_us = window.greatest - window.least,
            frames = window.samples,
            "timestamps against the recorder's clock"
        );
        Some(window.moved)
    }
}

/// The session's audio stream and what assembles its frames.
struct AudioTrack {
    /// The stream's index in the session.
    index: usize,
    /// The camera's `audio` stream key.
    key: StreamKey,
    /// Created at the first frame, from the stream's parameters.
    assembler: Option<AudioAssembler>,
    /// Whether the stream's parameters were refused; said once.
    refused: bool,
    /// The per-frame steps of the frames received, when they are logged.
    cadence: Cadence,
    /// Frames received, for the cadence log's groups of 125.
    frames: u64,
    /// Where the frames are published for live listeners.
    live: Option<LiveRing>,
}

impl SessionState {
    /// Delivers and accounts for the pending fragment at every session exit.
    fn flush(&mut self, sink: &dyn FragmentSink, source: &StreamSource, stats: &StreamStats) {
        if let Some(fragment) = self.assembler.as_mut().and_then(FragmentAssembler::flush) {
            self.close_fragment(fragment, sink, source, stats);
        }
        if let Some(audio) = self.audio.as_mut()
            && let Some(fragment) = audio.assembler.as_mut().and_then(AudioAssembler::flush)
        {
            send_audio(sink, &audio.key, fragment);
        }
    }

    /// Cuts the audio that accompanies a closed video fragment's
    /// predecessor and hands it to the sink.
    fn cut_audio(&mut self, span: VideoSpan, sink: &dyn FragmentSink) {
        let Some(audio) = self.audio.as_mut() else {
            return;
        };
        let Some(assembler) = audio.assembler.as_mut() else {
            return;
        };
        if let Some(fragment) = assembler.video_fragment(span) {
            debug!(
                target: "iris_ingest::cadence",
                stream = %audio.key,
                frames = fragment.samples.len(),
                drift_us = assembler.drift_ticks().saturating_mul(100) / 9,
                "audio fragment"
            );
            if fragment.new_run {
                self.watches.start_run();
            }
            send_audio(sink, &audio.key, fragment);
        }
    }

    /// Takes one audio frame of the recorded audio stream; its fragments are
    /// cut as video fragments close (`cut_audio`). Audio never ends the
    /// session: a stream whose parameters cannot be stored is left unrecorded.
    fn on_audio(
        &mut self,
        frame: &retina::codec::AudioFrame,
        demuxed: &Demuxed,
        stats: &StreamStats,
    ) {
        let Some(audio) = self.audio.as_mut().filter(|a| a.index == frame.stream_id()) else {
            return;
        };
        if audio.refused {
            return;
        }
        let Ok(wall) = self.clock.wall_at(frame.ctx().received()) else {
            return;
        };
        if audio.assembler.is_none() {
            match audio_assembler(demuxed, audio.index, frame.frame_length().get()) {
                Ok(assembler) => {
                    let assembler = assembler.with_watch_window(self.watches.window_ticks);
                    info!(stream = %audio.key, "recording audio");
                    if let Some(live) = &audio.live {
                        live.set_init_segment(build_init_segment(assembler.entry()));
                    }
                    audio.assembler = Some(assembler);
                }
                Err(reason) => {
                    info!(stream = %audio.key, reason, "audio not recorded");
                    audio.refused = true;
                    return;
                }
            }
        }
        let Some(assembler) = audio.assembler.as_mut() else {
            return;
        };
        // The cadence log takes 90 kHz ticks and reports a group at a sync
        // frame; audio has none, so every 125th frame closes one.
        let rate = i64::from(demuxed.streams()[audio.index].clock_rate_hz().max(1));
        audio.cadence.frame(
            &audio.key,
            frame.timestamp().timestamp().saturating_mul(90_000) / rate,
            wall,
            frame.data().len(),
            audio.frames.is_multiple_of(125),
        );
        audio.frames += 1;
        let stamp_us = frame.timestamp().timestamp().saturating_mul(1_000_000) / rate;
        let _ = Watches::clock(&mut self.watches.audio_clock, &audio.key, wall, stamp_us);
        let data = Bytes::copy_from_slice(frame.data());
        let pushed = assembler.push(AudioFrame {
            data: data.clone(),
            raw_ts: frame.timestamp().timestamp(),
            wall,
            lost: frame.loss() > 0,
        });
        if let Some(side) = assembler.last_step() {
            stats.record_sound_step_made(side);
        }
        // A frame is noted and published as often as it is recorded, with
        // the times each record of it gets.
        for (decode_time, recorded) in assembler.last_pushed() {
            let decode_us = ticks_us(decode_time);
            self.watches
                .place
                .audio_frame(stamp_us - decode_us, wall.get() - decode_us);
            if let Some(live) = &audio.live {
                live.publish(Arc::new(LiveFrame {
                    key: audio.key.clone(),
                    sample_entry: Arc::clone(assembler.entry()),
                    data: data.clone(),
                    duration: assembler.frame_ticks(),
                    is_sync: true,
                    decode_time,
                    wall: recorded,
                }));
            }
        }
        if let Some(window) = assembler.take_stamp_window() {
            let micros = |ticks: i64| ticks.saturating_mul(100) / 9;
            stats.record_audio_stamps_moved(micros(window.moved));
            info!(
                stream = %audio.key,
                least_us = micros(window.least),
                greatest_us = micros(window.greatest),
                moved_us = micros(window.moved),
                frames = window.samples,
                "audio timestamps against the count"
            );
        }
        if pushed == Pushed::Broke {
            self.watches.start_run();
            // Both streams break at the same point: the video starts a run
            // at its next sync frame.
            info!(stream = %audio.key, "audio continuity lost; starting a run");
            if let Some(video) = self.assembler.as_mut() {
                video.mark_discontinuity();
            }
        }
    }

    fn close_fragment(
        &mut self,
        fragment: Fragment,
        sink: &dyn FragmentSink,
        source: &StreamSource,
        stats: &StreamStats,
    ) {
        let assembler = self
            .assembler
            .as_mut()
            .expect("a fragment can only close after assembler creation");
        let snaps = assembler.snap_count();
        if snaps > self.snaps_seen {
            stats.add_snaps(snaps - self.snaps_seen);
            self.snaps_seen = snaps;
        }
        note_closed_fragment(
            &mut self.holds_seen,
            assembler,
            &fragment,
            &source.key,
            stats,
        );
        let span = VideoSpan::of(&fragment);
        if deliver_fragment(sink, &source.key, fragment, stats) {
            self.sink_loss = false;
        } else {
            if !self.sink_loss {
                info!(stream = %source.key, cause = "sink_rejected", "gap");
                stats.record_gap();
            }
            self.sink_loss = true;
            assembler.mark_sink_rejection();
        }
        self.place_audio(&span, stats);
        self.cut_audio(span, sink);
    }

    /// Notes a closed video fragment for the audio's place against the
    /// video, and when it closes a window reports the place and the step
    /// the drift rule names for it (`docs/research/13-audio.md` §5.3.4).
    /// The step is made on the frames received next where the stream's
    /// steps are made, and only said where they are not.
    fn place_audio(&mut self, span: &VideoSpan, stats: &StreamStats) {
        let Some(audio) = self.audio.as_mut() else {
            return;
        };
        let Some(assembler) = audio.assembler.as_mut() else {
            return;
        };
        let frame_ticks = assembler.frame_ticks();
        if span.new_run {
            self.watches.start_run();
        }
        let stamp_us = near(span.raw_rtp, self.watches.video_stamp).saturating_mul(100) / 9;
        let decode_us = ticks_us(span.decode_time);
        let Some(placed) = self
            .watches
            .place
            .video_fragment(span.decode_time, stamp_us - decode_us)
        else {
            return;
        };
        if let Some(reading) = placed.by_reports {
            stats.record_audio_sync(reading.late, reading.moved);
            info!(
                stream = %audio.key,
                late_us = reading.late,
                moved_us = reading.moved,
                pairs = placed.pairs,
                fragments = placed.fragments,
                "audio against video by sender reports"
            );
        } else if !self.watches.unmeasured_said {
            self.watches.unmeasured_said = true;
            info!(
                stream = %audio.key,
                audio_reports = placed.audio_reports,
                video_reports = placed.video_reports,
                pairs = placed.pairs,
                "audio against video is not read by sender reports: too few pairs of them"
            );
        }
        if let Some(reading) = placed.by_arrival {
            stats.record_audio_arrival_moved(reading.moved);
            info!(
                stream = %audio.key,
                moved_us = reading.moved,
                apart_us = reading.late,
                stamps_moved_us = placed.by_stamps.map_or(0, |stamps| stamps.moved),
                fragments = placed.fragments,
                "audio against video by arrival"
            );
        }
        let window_ticks = self.watches.window_ticks.max(1);
        let makes = self.watches.makes_steps;
        let rule = self.watches.rule.get_or_insert_with(|| {
            let rule = Rule::new(
                i64::from(frame_ticks) * 100 / 9,
                (3_600 * 90_000 / window_ticks).max(1),
            );
            if makes { rule.making_steps() } else { rule }
        });
        let verdict = rule.judge(&placed);
        if let Verdict::Step { side, frames, .. } = verdict {
            stats.record_sound_step_said(side, frames);
            if makes {
                assembler.step(side, frames);
            }
        }
        log_verdict(&audio.key, makes, verdict);
    }

    /// Notes a video frame's receipt against its timestamp for the watch of
    /// the camera's clock against the recorder's.
    fn watch_video_clock(
        &mut self,
        key: &StreamKey,
        wall: WallUs,
        raw_ts: i64,
        stats: &StreamStats,
    ) {
        self.watches.video_stamp = raw_ts;
        let stamp_us = raw_ts.saturating_mul(100) / 9;
        if let Some(moved) = Watches::clock(&mut self.watches.video_clock, key, wall, stamp_us) {
            stats.record_clock_moved(moved);
        }
    }

    /// Records the camera clock offset carried by RTCP sender reports. A
    /// report retina cannot read is logged at debug level and skipped.
    fn on_rtcp(
        &mut self,
        packet: &ReceivedCompoundPacket,
        demuxed: &Demuxed,
        key: &StreamKey,
        stats: &StreamStats,
    ) -> Result<(), SessionError> {
        let nvr = self.clock.wall_at(packet.ctx().received())?;
        // The report's stream: the audio stream's reports go under its key.
        let audio_key = self
            .audio
            .as_ref()
            .filter(|audio| audio.index == packet.stream_id())
            .map(|audio| &audio.key);
        for pkt in packet.pkts() {
            match pkt.as_sender_report() {
                Ok(Some(report)) => {
                    let camera = WallUs::from_ntp64(report.ntp_timestamp().0);
                    if audio_key.is_none() {
                        stats.record_sender_report(camera.micros_since(nvr));
                    }
                    if let (Some(rtp), Some(stream)) = (
                        packet.rtp_timestamp(),
                        demuxed.streams().get(packet.stream_id()),
                    ) {
                        let epoch = epoch_us(camera, rtp.timestamp(), stream.clock_rate_hz());
                        if audio_key.is_some() {
                            self.watches.place.audio_report(epoch, nvr);
                        } else {
                            self.watches.place.video_report(epoch, nvr);
                        }
                        cadence::sender_report(
                            audio_key.unwrap_or(key),
                            camera,
                            rtp.timestamp(),
                            stream.clock_rate_hz(),
                            nvr,
                        );
                    }
                }
                Ok(None) => {}
                Err(error) => {
                    debug!(stream = %key, error = %LogSafe(&error), "unreadable RTCP sender report");
                }
            }
        }
        Ok(())
    }

    fn on_frame(
        &mut self,
        frame: VideoFrame,
        demuxed: &Demuxed,
        source: &StreamSource,
        options: &IngestOptions,
        sink: &dyn FragmentSink,
        stats: &StreamStats,
    ) -> Result<(), SessionError> {
        let wall = self.clock.wall_at(frame.start_ctx().received())?;
        if let Some(previous) = self.previous_frame.take() {
            log_reconnect_gap(&source.key, previous, wall, stats);
        }
        let has_new_parameters = frame.has_new_parameters();
        let is_sync = frame.is_random_access_point();
        let raw_ts = frame.timestamp().timestamp();
        let lost = frame.loss() > 0;
        if lost {
            debug!(stream = %source.key, lost = frame.loss(), "packet loss before frame");
        }
        let data = framed_data(&source.key, frame.into_data());
        stats.record_frame(data.len() as u64, wall, lost);
        self.cadence
            .frame(&source.key, raw_ts, wall, data.len(), is_sync);
        self.watch_video_clock(&source.key, wall, raw_ts, stats);

        let mut retarget_nominal = None;
        if self.assembler.is_none() || has_new_parameters {
            let Some(video) = video_parameters(demuxed, self.stream_index) else {
                if is_sync {
                    return Err(SessionError::MissingParameters);
                }
                return Ok(());
            };
            let entry = Arc::new(sample_entry(video)?);
            stats.set_codec_description(format!(
                "{} {}x{}",
                entry.codecs, entry.width, entry.height
            ));
            if let Some(live) = &options.live {
                live.set_init_segment(build_init_segment(&entry));
            }
            let parameter_nominal = video.frame_rate().and_then(nominal_from_rate);
            if self.assembler.is_some() {
                retarget_nominal =
                    changed_parameter_nominal(self.nominal, parameter_nominal, has_new_parameters);
            }
            if self.nominal.is_none() {
                self.nominal = parameter_nominal;
            }
            if self.assembler.is_none() {
                info!(
                    stream = %source.key,
                    declared_fps = ?demuxed.streams()[self.stream_index].framerate(),
                    sps_frame_rate = ?video.frame_rate(),
                    nominal_ticks = ?self.nominal,
                    "timing hints"
                );
            }
            if let Some(assembler) = self.assembler.as_mut() {
                assembler.set_sample_entry(entry);
            } else {
                let regularizer =
                    Regularizer::new(options.timestamp_mode, self.nominal, SNAP_TOLERANCE);
                self.assembler = Some(FragmentAssembler::new(options, regularizer, entry));
            }
        }
        let Some(assembler) = self.assembler.as_mut() else {
            return Ok(());
        };
        let live_data = options.live.as_ref().map(|_| data.clone());
        let completed = assembler.push(Frame {
            data,
            raw_ts,
            wall,
            is_sync,
            new_params: has_new_parameters,
        });
        if let Some(nominal) = retarget_nominal {
            assembler.retarget_timing(nominal);
            self.nominal = Some(nominal);
        }
        self.watches
            .arrived(wall, assembler.last_provisional_decode_time());
        let live_decode_time = live_decode_time(
            assembler.last_provisional_decode_time(),
            self.last_live_decode_time,
            assembler.pending_starts_run(),
            assembler.nominal(),
        );
        if let Some(live) = &options.live
            && let Some(data) = live_data
            && let Some(decode_time) = live_decode_time
        {
            live.publish(Arc::new(LiveFrame {
                key: source.key.clone(),
                sample_entry: Arc::clone(assembler.sample_entry()),
                data,
                duration: assembler.nominal(),
                is_sync,
                decode_time,
                wall,
            }));
            self.last_live_decode_time = Some(decode_time);
        }
        if let Some(fragment) = completed {
            self.close_fragment(fragment, sink, source, stats);
        }
        Ok(())
    }
}

/// The decode time a frame is published to the live ring with: its
/// provisional time in the recording, or one nominal duration past the last
/// published time while recording waits for a sync frame. Inside a run it
/// never steps back: a fragment that the regulariser shortened by more than
/// a frame ends before the provisional times of its last frames, and the
/// frames after it are published one tick apart until the provisional time
/// has passed the last published one. Only the first fragment of a run,
/// whose decode timeline starts over, publishes a lower time.
fn live_decode_time(
    provisional: Option<u64>,
    last_published: Option<u64>,
    starts_run: bool,
    nominal: u32,
) -> Option<u64> {
    match (provisional, last_published) {
        (Some(time), Some(last)) if !starts_run && time <= last => Some(last.saturating_add(1)),
        (Some(time), _) => Some(time),
        (None, last) => last.map(|time| time.saturating_add(u64::from(nominal))),
    }
}

/// Logs the gap between the last frame of the previous session and `wall`.
fn log_reconnect_gap(key: &StreamKey, previous: WallUs, wall: WallUs, stats: &StreamStats) {
    let secs = wall.micros_since(previous) as f64 / 1e6;
    info!(
        stream = %key,
        from = %previous,
        to = %wall,
        secs = format_args!("{secs:.3}"),
        cause = "reconnect",
        "gap"
    );
    stats.record_gap();
}

/// Feeds the regulariser's state, the fragment's size, and any camera hold of a
/// second or more to `stats`. `holds_seen` carries the assembler's totals already
/// reported, as (holes, hold ticks).
fn note_closed_fragment(
    holds_seen: &mut (u64, u64),
    assembler: &FragmentAssembler,
    fragment: &Fragment,
    key: &StreamKey,
    stats: &StreamStats,
) {
    let totals = (assembler.holes(), assembler.hold_ticks());
    stats.record_timing(
        totals.0 - holds_seen.0,
        totals.1 - holds_seen.1,
        assembler.regularizer().nominal(),
        assembler.regularizer().error_ticks(),
    );
    *holds_seen = totals;
    let hold = assembler.last_hold_ticks();
    if hold >= GAP_HOLD_TICKS {
        // The hold sits on the fragment's last frame: it begins where the
        // assigned timeline would have ended without it.
        let duration = i64::try_from(fragment.duration()).unwrap_or(i64::MAX);
        let at = fragment
            .wall
            .offset((duration - hold).saturating_mul(100) / 9);
        info!(
            stream = %key,
            at = %at,
            secs = format_args!("{:.3}", hold as f64 / 90_000.0),
            cause = "camera_hold",
            "gap"
        );
        stats.record_gap();
    }
    stats.record_fragment_size(fragment.payload_len(), fragment.duration());
}

/// Hands one complete fragment to `sink` and records its acceptance in `stats`.
///
/// This is the delivery/accounting seam used by [`run_stream`], which the
/// daemon's sink tests drive directly. It does not end recording continuity
/// on a rejection: the session does that from the result, so a `false` must
/// not be ignored.
#[must_use = "a rejected fragment ends recording continuity"]
pub fn deliver_fragment(
    sink: &dyn FragmentSink,
    key: &StreamKey,
    fragment: Fragment,
    stats: &StreamStats,
) -> bool {
    let accepted = sink.send(key, fragment).is_ok();
    if !accepted {
        warn!(stream = %key, "sink rejected a fragment; dropping it");
    }
    stats.record_fragment(accepted);
    accepted
}

/// Strips any in-band parameter sets as a safety net; the frame format
/// already asks retina to keep them out of frame data. A framing error means
/// the depacketizer produced an access unit this crate cannot walk, which is
/// logged at debug level; the frame is recorded as received.
fn framed_data(key: &StreamKey, data: Vec<u8>) -> Bytes {
    match iris_h265::strip_parameter_sets(&data) {
        Ok(Some(stripped)) => Bytes::from(stripped),
        Ok(None) => Bytes::from(data),
        Err(error) => {
            debug!(stream = %key, error = %LogSafe(&error), "access unit framing could not be walked");
            Bytes::from(data)
        }
    }
}

/// Hands an audio fragment to the sink; a rejected one is dropped and said.
fn send_audio(sink: &dyn FragmentSink, key: &StreamKey, fragment: Fragment) {
    if sink.send(key, fragment).is_err() {
        debug!(stream = %key, "audio fragment rejected by the sink");
    }
}

/// The assembler for the session's audio stream, or why its audio cannot be
/// stored: no AAC parameters, no sample entry, or a sampling rate whose
/// frames are not whole 90 kHz ticks.
fn audio_assembler(
    demuxed: &Demuxed,
    index: usize,
    frame_samples: u32,
) -> Result<AudioAssembler, &'static str> {
    let Some(ParametersRef::Audio(parameters)) = demuxed
        .streams()
        .get(index)
        .and_then(retina::client::Stream::parameters)
    else {
        return Err("no audio parameters");
    };
    let Some(codecs) = parameters.rfc6381_codec() else {
        return Err("no codec string");
    };
    let Ok(entry) = parameters.mp4_sample_entry().build() else {
        return Err("no MP4 sample entry for the codec");
    };
    let Some(frame_ticks) = audio_frame_ticks(parameters.clock_rate(), frame_samples) else {
        return Err("frames are not whole 90 kHz ticks at the sampling rate");
    };
    Ok(AudioAssembler::new(
        Arc::new(SampleEntry::audio(Bytes::from(entry), codecs.to_owned())),
        parameters.clock_rate(),
        frame_samples,
        frame_ticks,
    ))
}

/// Streams other than the recorded one that a session start logs.
const MAX_OFFERED_LOGGED: usize = 4;

/// The first 32 characters of camera text, for a log field.
fn clipped(text: &str) -> String {
    text.chars().take(32).collect()
}

fn video_parameters(demuxed: &Demuxed, stream_index: usize) -> Option<&VideoParameters> {
    match demuxed.streams().get(stream_index)?.parameters()? {
        ParametersRef::Video(video) => Some(video),
        ParametersRef::Audio(_) | ParametersRef::Message(_) => None,
    }
}

fn sample_entry(video: &VideoParameters) -> Result<SampleEntry, SessionError> {
    let bytes = video.mp4_sample_entry().build()?;
    let (width, height) = video.pixel_dimensions();
    Ok(SampleEntry::new(
        Bytes::from(bytes),
        video.rfc6381_codec().to_owned(),
        u16::try_from(width).unwrap_or(u16::MAX),
        u16::try_from(height).unwrap_or(u16::MAX),
    ))
}

/// Nominal frame duration in ticks from a frames-per-second figure.
fn nominal_from_fps(fps: f32) -> Option<u32> {
    if !(1.0..=240.0).contains(&fps) {
        return None;
    }
    Some((90_000.0 / f64::from(fps)).round() as u32)
}

/// Nominal frame duration from retina's `(a, b)` pair of unspecified
/// orientation: whichever quotient is a plausible frame rate wins.
fn nominal_from_rate((a, b): (u32, u32)) -> Option<u32> {
    if a == 0 || b == 0 {
        return None;
    }
    let forward = f64::from(a) / f64::from(b);
    let backward = f64::from(b) / f64::from(a);
    let fps = if (1.0..=240.0).contains(&forward) {
        forward
    } else if (1.0..=240.0).contains(&backward) {
        backward
    } else {
        return None;
    };
    Some((90_000.0 / fps).round() as u32)
}

fn changed_parameter_nominal(
    current: Option<u32>,
    parsed: Option<u32>,
    has_new_parameters: bool,
) -> Option<u32> {
    if has_new_parameters && parsed.is_some() && parsed != current {
        parsed
    } else {
        None
    }
}

/// Maps monotonic receive instants through fresh realtime correlations and
/// detects clock steps before they enter a run's wall timeline.
struct WallClock {
    instant: Instant,
    wall: WallUs,
}

impl WallClock {
    fn new() -> Self {
        Self {
            instant: Instant::now(),
            wall: WallUs::now(),
        }
    }

    fn wall_at(&mut self, received: Instant) -> Result<WallUs, SessionError> {
        self.wall_at_sample(received, Instant::now(), WallUs::now())
    }

    fn wall_at_sample(
        &mut self,
        received: Instant,
        instant: Instant,
        wall: WallUs,
    ) -> Result<WallUs, SessionError> {
        let expected = self.wall.offset(signed_micros(instant, self.instant));
        if wall.micros_since(expected).unsigned_abs() > CLOCK_STEP_US {
            return Err(SessionError::ClockStep);
        }
        self.instant = instant;
        self.wall = wall;
        Ok(wall.offset(signed_micros(received, instant)))
    }
}

/// 90 kHz ticks of a timeline as microseconds.
fn ticks_us(ticks: u64) -> i64 {
    i64::try_from(ticks.saturating_mul(100) / 9).unwrap_or(i64::MAX)
}

fn signed_micros(at: Instant, anchor: Instant) -> i64 {
    if at >= anchor {
        i64::try_from(at.duration_since(anchor).as_micros()).unwrap_or(i64::MAX)
    } else {
        -i64::try_from(anchor.duration_since(at).as_micros()).unwrap_or(i64::MAX)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use iris_core::config::TimestampMode;
    use iris_core::time::MediaTicks;
    use tokio::io::AsyncReadExt as _;
    use tokio::net::TcpListener;
    use tokio::sync::oneshot;

    use super::*;

    fn stream_key(camera: &str) -> StreamKey {
        StreamKey {
            camera: camera.parse().expect("camera id"),
            kind: StreamKind::Main,
        }
    }

    struct CountingSink {
        calls: AtomicUsize,
        reject: bool,
    }

    impl FragmentSink for CountingSink {
        fn send(&self, _key: &StreamKey, _fragment: Fragment) -> Result<(), crate::SinkError> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            if self.reject {
                Err(crate::SinkError)
            } else {
                Ok(())
            }
        }
    }

    fn sample_entry_for_test() -> Arc<SampleEntry> {
        Arc::new(SampleEntry::new(
            Bytes::from_static(b"\0\0\0\x08hvc1"),
            "hvc1".into(),
            16,
            16,
        ))
    }

    fn session_state_with_timing(nominal: u32, tolerance_ms: i64) -> SessionState {
        let options = IngestOptions::default();
        let regularizer = Regularizer::new(
            TimestampMode::Regularize,
            Some(nominal),
            MediaTicks::from_millis(tolerance_ms),
        );
        SessionState {
            clock: WallClock::new(),
            assembler: Some(FragmentAssembler::new(
                &options,
                regularizer,
                sample_entry_for_test(),
            )),
            nominal: Some(nominal),
            snaps_seen: 0,
            holds_seen: (0, 0),
            previous_frame: None,
            stream_index: 0,
            sink_loss: false,
            last_live_decode_time: None,
            cadence: Cadence::default(),
            audio: None,
            watches: Watches::new(options.watch_window, options.audio_steps),
        }
    }

    fn source_for_test() -> StreamSource {
        StreamSource {
            key: stream_key("cam1"),
            url: "rtsp://192.0.2.1/video".parse().expect("URL"),
            username: String::new(),
            password: String::new(),
        }
    }

    #[test]
    fn camera_text_in_the_offer_line_is_clipped_to_32_characters() {
        assert_eq!(clipped("mpeg4-generic"), "mpeg4-generic");
        assert_eq!(clipped(&"é".repeat(40)), "é".repeat(32));
    }

    #[test]
    fn live_decode_times_never_step_back_inside_a_run() {
        const IN_RUN: bool = false;
        const STARTS_RUN: bool = true;
        // The provisional time of the recording, as it is.
        assert_eq!(
            live_decode_time(Some(6000), Some(3000), IN_RUN, 3000),
            Some(6000)
        );
        assert_eq!(live_decode_time(Some(0), None, STARTS_RUN, 3000), Some(0));
        // A shortened fragment: the next frames follow the last published
        // time by a tick until the provisional time has passed it.
        assert_eq!(
            live_decode_time(Some(356_400), Some(357_000), IN_RUN, 3000),
            Some(357_001)
        );
        assert_eq!(
            live_decode_time(Some(357_001), Some(357_001), IN_RUN, 3000),
            Some(357_002)
        );
        assert_eq!(
            live_decode_time(Some(359_400), Some(357_002), IN_RUN, 3000),
            Some(359_400)
        );
        // A run starts its timeline over.
        assert_eq!(
            live_decode_time(Some(0), Some(357_000), STARTS_RUN, 3000),
            Some(0)
        );
        assert_eq!(
            live_decode_time(Some(3000), Some(0), STARTS_RUN, 3000),
            Some(3000)
        );
        // No recording frame is pending: one nominal duration onwards.
        assert_eq!(
            live_decode_time(None, Some(357_000), IN_RUN, 3000),
            Some(360_000)
        );
        assert_eq!(live_decode_time(None, None, STARTS_RUN, 3000), None);
    }

    fn pending_frame(raw_ts: i64, index: i64) -> Frame {
        Frame {
            data: Bytes::from_static(b"\0\0\0\x02\x26\x01"),
            raw_ts,
            wall: WallUs::new(1_000_000 + index * 33_333),
            is_sync: index == 0,
            new_params: false,
        }
    }

    #[tokio::test]
    async fn shutdown_interrupts_an_unanswered_describe() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (request_seen, request_received) = oneshot::channel();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = [0; 4_096];
            assert!(socket.read(&mut request).await.unwrap() > 0);
            request_seen.send(()).unwrap();
            std::future::pending::<()>().await;
        });
        let shutdown = CancellationToken::new();
        let source = StreamSource {
            key: StreamKey {
                camera: "cam1".parse().unwrap(),
                kind: StreamKind::Main,
            },
            url: format!("rtsp://{address}/video").parse().unwrap(),
            username: String::new(),
            password: String::new(),
        };
        let task = tokio::spawn({
            let shutdown = shutdown.clone();
            async move {
                let stats = StreamStats::default();
                start_session(&source, &IngestOptions::default(), &stats, &shutdown).await
            }
        });
        tokio::time::timeout(Duration::from_secs(2), request_received)
            .await
            .unwrap()
            .unwrap();
        shutdown.cancel();
        assert!(
            tokio::time::timeout(Duration::from_secs(1), task)
                .await
                .unwrap()
                .unwrap()
                .unwrap()
                .is_none()
        );
        server.abort();
    }

    #[tokio::test]
    async fn the_ticker_wakes_waiters_at_each_interval() {
        let ticker = ReadTicker::spawn(Duration::from_millis(5));
        let started = Instant::now();
        ticker.tick().await;
        ticker.tick().await;
        let elapsed = started.elapsed();
        assert!(elapsed >= Duration::from_millis(5), "{elapsed:?}");
        assert!(elapsed < Duration::from_secs(1), "{elapsed:?}");
    }

    #[test]
    fn aligned_ticks_are_shared_multiples_of_the_interval() {
        let interval = Duration::from_millis(50);
        let a = aligned_tick(interval);
        let b = aligned_tick(interval);
        assert!(b >= a && b.duration_since(a) <= interval);
        assert!(a > Instant::now().checked_sub(interval).unwrap());
        assert!(aligned_tick(Duration::from_millis(10)) <= a + Duration::from_millis(10));
    }

    #[test]
    fn nominal_from_declared_rates() {
        assert_eq!(nominal_from_fps(30.0), Some(3000));
        assert_eq!(nominal_from_fps(29.97), Some(3003));
        assert_eq!(nominal_from_fps(0.0), None);
        assert_eq!(nominal_from_rate((30, 1)), Some(3000));
        assert_eq!(nominal_from_rate((1, 15)), Some(6000));
        assert_eq!(nominal_from_rate((1001, 30000)), Some(3003));
        assert_eq!(nominal_from_rate((0, 7)), None);
        assert_eq!(nominal_from_rate((7, 0)), None);
        assert_eq!(nominal_from_rate((1, 241)), None);
        // Either orientation of a fraction that is not in lowest terms.
        assert_eq!(nominal_from_rate((60, 2)), Some(3000));
        assert_eq!(nominal_from_rate((2, 60)), Some(3000));
    }

    #[test]
    fn retargets_only_on_a_changed_valid_parameter_rate() {
        assert_eq!(
            changed_parameter_nominal(None, Some(6000), true),
            Some(6000)
        );
        assert_eq!(
            changed_parameter_nominal(Some(3000), Some(6000), true),
            Some(6000)
        );
        assert_eq!(
            changed_parameter_nominal(Some(3000), Some(3000), true),
            None
        );
        assert_eq!(changed_parameter_nominal(Some(3000), None, true), None);
        assert_eq!(changed_parameter_nominal(None, None, true), None);
        assert_eq!(
            changed_parameter_nominal(Some(3000), Some(6000), false),
            None
        );
    }

    #[test]
    fn jitter_stays_bounded_and_varies_by_stream_and_attempt() {
        let mut values = HashSet::new();
        for camera in ["cam1", "cam2", "cam3", "cam4"] {
            let key = stream_key(camera);
            for attempt in 0..64 {
                let millis = jittered(Duration::from_secs(10), &key, attempt).as_millis();
                assert!((8_000..=12_000).contains(&millis), "{millis} ms");
                values.insert(millis);
            }
        }
        assert!(values.len() > 100, "only {} jitter values", values.len());
        // The values use the whole range, not one end of it.
        let low = values.iter().filter(|millis| **millis < 9_000).count();
        let high = values.iter().filter(|millis| **millis > 11_000).count();
        assert!(low >= 20 && high >= 20, "{low} low and {high} high values");

        // Fourteen streams that lose their cameras together do not retry
        // together: their first delays differ.
        let mut first_delays = HashSet::new();
        for camera in 1..=7 {
            for kind in [StreamKind::Main, StreamKind::Sub] {
                let key = StreamKey {
                    camera: format!("cam{camera}").parse().expect("camera id"),
                    kind,
                };
                first_delays.insert(jittered(Duration::from_secs(1), &key, 0));
            }
        }
        assert!(first_delays.len() >= 12, "{first_delays:?}");
    }

    /// A rest between attempts notes that the task is alive every five
    /// seconds of it and at its end, and ends early at shutdown.
    #[tokio::test(start_paused = true)]
    async fn a_rest_between_attempts_beats_every_slice_and_ends_at_shutdown() {
        let stats = StreamStats::default();
        let shutdown = CancellationToken::new();
        let began = tokio::time::Instant::now();
        assert!(rest(Duration::from_secs(36), &stats, &shutdown).await);
        assert_eq!(began.elapsed(), Duration::from_secs(36));
        // Seven slices of five seconds and the second that is left.
        assert_eq!(stats.beats(), 8);
        assert!(rest(Duration::ZERO, &stats, &shutdown).await);
        assert_eq!(stats.beats(), 8);

        let ending = shutdown.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(7)).await;
            ending.cancel();
        });
        let began = tokio::time::Instant::now();
        assert!(!rest(Duration::from_secs(36), &stats, &shutdown).await);
        assert_eq!(began.elapsed(), Duration::from_secs(7));
        assert_eq!(stats.beats(), 9);
    }

    #[test]
    fn healthy_session_resets_backoff_and_jitter_attempt() {
        let key = stream_key("cam1");
        let mut backoff = MAX_BACKOFF;
        let mut attempt = 40;
        let delay = next_retry_delay(&key, &mut backoff, &mut attempt, true);
        assert_eq!(delay, jittered(Duration::from_secs(1), &key, 0));
        assert_eq!(backoff, Duration::from_secs(2));
        assert_eq!(attempt, 1);
    }

    #[test]
    fn wall_clock_maps_instants_in_both_directions() {
        let mut clock = WallClock::new();
        let anchor = clock.instant;
        let wall = clock.wall;
        let later = clock.instant + Duration::from_millis(5);
        assert_eq!(
            clock
                .wall_at_sample(later, anchor, wall)
                .unwrap()
                .micros_since(wall),
            5_000
        );
        let earlier = clock
            .instant
            .checked_sub(Duration::from_millis(3))
            .expect("process uptime exceeds 3 ms");
        assert_eq!(
            clock
                .wall_at_sample(earlier, anchor, wall)
                .unwrap()
                .micros_since(wall),
            -3_000
        );
    }

    #[test]
    fn wall_clock_tracks_realtime_steps_after_the_session_starts() {
        let now = Instant::now();
        let realtime = WallUs::now();
        // The saved correlation predates a 60 s forward realtime step.
        for step in [-60_000_000, 60_000_000] {
            let mut clock = WallClock {
                instant: now.checked_sub(Duration::from_secs(10)).unwrap(),
                wall: realtime.offset(-10_000_000),
            };
            assert!(matches!(
                clock.wall_at_sample(now, now, realtime.offset(step)),
                Err(SessionError::ClockStep)
            ));
        }
    }

    #[test]
    fn wall_clock_refreshes_slew_and_preserves_receipt_age() {
        let instant = Instant::now();
        let wall = WallUs::new(1_000_000_000);
        let mut clock = WallClock { instant, wall };
        let sample = instant + Duration::from_secs(10);
        let received = sample.checked_sub(Duration::from_millis(20)).unwrap();
        let realtime = wall.offset(10_005_000);
        assert_eq!(
            clock.wall_at_sample(received, sample, realtime).unwrap(),
            realtime.offset(-20_000)
        );
        assert_eq!(clock.wall, realtime);
        assert_eq!(clock.instant, sample);
    }

    /// The stream's hold counters are the assembler's totals, fragment
    /// after fragment: each close adds what that fragment held and no more.
    #[test]
    fn holds_are_counted_once_across_fragments() {
        let mut session = session_state_with_timing(3000, 500);
        let sink = CountingSink {
            calls: AtomicUsize::new(0),
            reject: false,
        };
        let stats = StreamStats::new();
        let source = source_for_test();
        for (index, raw_ts) in [0, 3000, 100_000].into_iter().enumerate() {
            let assembler = session.assembler.as_mut().expect("assembler");
            assert!(
                assembler
                    .push(pending_frame(raw_ts, index as i64))
                    .is_none()
            );
        }
        session.flush(&sink, &source, &stats);
        assert_eq!((stats.holds(), stats.hold_ticks()), (31, 93_000));
        for (index, raw_ts) in [200_000, 203_000, 290_000].into_iter().enumerate() {
            let assembler = session.assembler.as_mut().expect("assembler");
            assert!(
                assembler
                    .push(pending_frame(raw_ts, index as i64))
                    .is_none()
            );
        }
        session.flush(&sink, &source, &stats);
        let assembler = session.assembler.as_ref().expect("assembler");
        assert_eq!(
            (stats.holds(), stats.hold_ticks()),
            (assembler.holes(), assembler.hold_ticks())
        );
        assert_eq!(stats.holds(), 31 + 28);
        assert_eq!(stats.fragments(), 2);
        assert_eq!(stats.snaps(), assembler.snap_count());
    }

    #[test]
    fn final_flush_accounts_for_holds_and_is_exactly_once() {
        let mut session = session_state_with_timing(3000, 500);
        for (index, raw_ts) in [0, 3000, 100_000].into_iter().enumerate() {
            assert!(
                session
                    .assembler
                    .as_mut()
                    .expect("assembler")
                    .push(pending_frame(raw_ts, index as i64))
                    .is_none()
            );
        }
        let sink = CountingSink {
            calls: AtomicUsize::new(0),
            reject: false,
        };
        let stats = StreamStats::new();
        let source = source_for_test();
        session.flush(&sink, &source, &stats);
        assert_eq!(sink.calls.load(Ordering::Relaxed), 1);
        assert_eq!(stats.fragments(), 1);
        assert_eq!(stats.dropped_sink(), 0);
        assert_eq!((stats.holds(), stats.hold_ticks()), (31, 93_000));
        assert_eq!(stats.gaps(), 1);
        assert_eq!(stats.snaps(), 0);
        assert_eq!(stats.nominal_ticks(), Some(3000));
        assert_eq!(stats.error_ticks(), -910);
        assert_eq!(stats.fragment_bytes_peak().get(), 18);
        assert_eq!(stats.fragment_ticks_peak().get(), 102_090);

        session.flush(&sink, &source, &stats);
        assert_eq!(sink.calls.load(Ordering::Relaxed), 1);
        assert_eq!(stats.fragments(), 1);
        assert_eq!(
            (stats.holds(), stats.hold_ticks(), stats.gaps()),
            (31, 93_000, 1)
        );
    }

    #[test]
    fn final_flush_accounts_for_snap_and_sink_rejection_once() {
        let mut session = session_state_with_timing(45_000, 150);
        for (index, raw_ts) in [0, 45_000, 90_000, 200_000].into_iter().enumerate() {
            assert!(
                session
                    .assembler
                    .as_mut()
                    .expect("assembler")
                    .push(pending_frame(raw_ts, index as i64))
                    .is_none()
            );
        }
        let sink = CountingSink {
            calls: AtomicUsize::new(0),
            reject: true,
        };
        let stats = StreamStats::new();
        let source = source_for_test();
        session.flush(&sink, &source, &stats);
        assert_eq!(sink.calls.load(Ordering::Relaxed), 1);
        assert_eq!((stats.fragments(), stats.dropped_sink()), (1, 1));
        assert_eq!((stats.holds(), stats.hold_ticks()), (1, 65_000));
        assert_eq!((stats.gaps(), stats.snaps()), (1, 1));
        assert_eq!(stats.nominal_ticks(), Some(45_000));
        assert_eq!(stats.error_ticks(), 0);
        assert_eq!(stats.fragment_ticks_peak().get(), 245_000);

        session.flush(&sink, &source, &stats);
        assert_eq!(sink.calls.load(Ordering::Relaxed), 1);
        assert_eq!((stats.fragments(), stats.dropped_sink()), (1, 1));
    }

    #[test]
    fn final_flush_without_pending_frames_is_a_no_op() {
        let mut session = session_state_with_timing(3000, 500);
        let sink = CountingSink {
            calls: AtomicUsize::new(0),
            reject: false,
        };
        let stats = StreamStats::new();
        session.flush(&sink, &source_for_test(), &stats);
        assert_eq!(sink.calls.load(Ordering::Relaxed), 0);
        assert_eq!(stats.fragments(), 0);
    }

    #[test]
    fn a_logged_session_error_is_one_line() {
        let error = SessionError::SetupTimeout("DESCRIBE");
        assert_eq!(
            LogSafe(&error).to_string(),
            "DESCRIBE not answered within 15s"
        );
    }
}
