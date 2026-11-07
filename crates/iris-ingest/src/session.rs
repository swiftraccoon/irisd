//! One RTSP session at a time per stream, reconnecting forever.

use std::num::NonZeroU32;
use std::pin::Pin;
use std::sync::{Arc, OnceLock};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use bytes::Bytes;
use futures::Stream as _;
use futures::StreamExt as _;
use iris_core::id::StreamKey;
use iris_core::time::WallUs;
use iris_mp4::init::{SampleEntry, build_init_segment};
use retina::client::{
    Credentials, Demuxed, PlayOptions, Session, SessionOptions, SetupOptions, TcpTransportOptions,
    TeardownPolicy, Transport,
};
use retina::codec::{CodecItem, FrameFormat, ParametersRef, VideoParameters};
use thiserror::Error;
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::assembler::{FragmentAssembler, Frame};
use crate::live::LiveFrame;
use crate::regularize::Regularizer;
use crate::sink::FragmentSink;
use crate::source::{IngestOptions, StreamSource};
use crate::stats::StreamStats;

/// Default tolerance before the regulariser snaps to the camera timeline.
const SNAP_TOLERANCE_MS: i64 = 500;
/// Longest pause between reconnection attempts.
const MAX_BACKOFF: Duration = Duration::from_secs(30);
/// How long a DESCRIBE, SETUP, or PLAY may take before the session is abandoned.
const SETUP_TIMEOUT: Duration = Duration::from_secs(15);
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
    #[error("no frames for {0:?}")]
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
        let started = Instant::now();
        // The session owns cancellation once it starts so its pending fragment
        // is flushed before the session state can be dropped.
        let outcome = run_session(&source, &options, sink.as_ref(), &stats, &shutdown).await;
        stats.set_connected(false);
        match outcome {
            Ok(()) => return,
            Err(error) => {
                warn!(stream = %source.key, %error, "session ended; reconnecting");
                stats.record_reconnect();
            }
        }
        let delay = next_retry_delay(
            &source.key,
            &mut backoff,
            &mut attempt,
            started.elapsed() >= HEALTHY_SESSION,
        );
        tokio::select! {
            () = shutdown.cancelled() => return,
            () = tokio::time::sleep(delay) => {}
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
    }) = start_session(source, options, shutdown).await?
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
    };

    let mut last_item = Instant::now();
    let outcome = 'session: loop {
        if shutdown.is_cancelled() {
            break Ok(());
        }
        let item = if let Some(interval) = options.read_interval {
            let Poll::Ready(item) = poll_now(&mut demuxed) else {
                tokio::select! {
                    () = shutdown.cancelled() => break 'session Ok(()),
                    () = next_tick(options.ticker.as_ref(), interval) => {}
                }
                if last_item.elapsed() >= options.inactivity_timeout {
                    break Err(SessionError::Inactivity(options.inactivity_timeout));
                }
                continue;
            };
            item
        } else {
            tokio::select! {
                () = shutdown.cancelled() => break 'session Ok(()),
                item = tokio::time::timeout(options.inactivity_timeout, demuxed.next()) => match item {
                    Ok(item) => item,
                    Err(_elapsed) => {
                        break 'session Err(SessionError::Inactivity(options.inactivity_timeout));
                    }
                },
            }
        };
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
            CodecItem::Rtcp(packet) => session_state.on_rtcp(&packet, stats),
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
}

/// Returns no stream when cancellation interrupts setup.
async fn start_session(
    source: &StreamSource,
    options: &IngestOptions,
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
    let stream_index = session
        .streams()
        .iter()
        .position(|s| s.media() == "video" && s.encoding_name().eq_ignore_ascii_case("h265"))
        .ok_or(SessionError::NoH265Stream)?;
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
    let max_jump = NonZeroU32::new(options.max_jump_secs.max(1)).expect("non-zero");
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
    Ok(Some(StartedSession {
        demuxed: playing.demuxed()?,
        nominal: declared_nominal,
        stream_index,
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

/// Polls the demuxer once without arming a wake-up: whatever is already
/// readable comes out, and `Pending` means the socket is drained for now.
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
struct TickerTask(tokio::task::JoinHandle<()>);

impl Drop for TickerTask {
    fn drop(&mut self) {
        self.0.abort();
    }
}

impl ReadTicker {
    /// Spawns the ticker on the current runtime, ticking at every multiple of
    /// `interval` on the shared clock.
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
}

impl SessionState {
    /// Delivers and accounts for the pending fragment at every session exit.
    fn flush(&mut self, sink: &dyn FragmentSink, source: &StreamSource, stats: &StreamStats) {
        if let Some(fragment) = self.assembler.as_mut().and_then(FragmentAssembler::flush) {
            self.close_fragment(fragment, sink, source, stats);
        }
    }

    fn close_fragment(
        &mut self,
        fragment: iris_mp4::fragment::Fragment,
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
    }

    /// Records the camera clock offset carried by RTCP sender reports.
    fn on_rtcp(
        &mut self,
        packet: &retina::rtcp::ReceivedCompoundPacket,
        stats: &StreamStats,
    ) -> Result<(), SessionError> {
        let nvr = self.clock.wall_at(packet.ctx().received())?;
        for pkt in packet.pkts() {
            if let Ok(Some(report)) = pkt.as_sender_report() {
                let camera = WallUs::from_ntp64(report.ntp_timestamp().0);
                stats.record_sender_report(camera.micros_since(nvr));
            }
        }
        Ok(())
    }

    fn on_frame(
        &mut self,
        frame: retina::codec::VideoFrame,
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
        let data = framed_data(frame.into_data());
        stats.record_frame(data.len() as u64, wall, lost);

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
                tracing::info!(
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
                let regularizer = Regularizer::new(
                    options.timestamp_mode,
                    self.nominal,
                    iris_core::time::MediaTicks::from_millis(SNAP_TOLERANCE_MS),
                );
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
        let live_decode_time = assembler.last_provisional_decode_time().or_else(|| {
            self.last_live_decode_time
                .map(|decode_time| decode_time.saturating_add(u64::from(assembler.nominal())))
        });
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
    fragment: &iris_mp4::fragment::Fragment,
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
/// This is the delivery/accounting seam used by [`run_stream`].
pub fn deliver_fragment(
    sink: &dyn FragmentSink,
    key: &StreamKey,
    fragment: iris_mp4::fragment::Fragment,
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
/// already asks retina to keep them out of frame data.
fn framed_data(data: Vec<u8>) -> Bytes {
    match iris_h265::strip_parameter_sets(&data) {
        Ok(Some(stripped)) => Bytes::from(stripped),
        Ok(None) | Err(_) => Bytes::from(data),
    }
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
    use iris_mp4::fragment::Fragment;
    use tokio::io::AsyncReadExt as _;
    use tokio::net::TcpListener;
    use tokio::sync::oneshot;

    use super::*;

    fn stream_key(camera: &str) -> StreamKey {
        StreamKey {
            camera: camera.parse().expect("camera id"),
            kind: iris_core::id::StreamKind::Main,
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
                kind: iris_core::id::StreamKind::Main,
            },
            url: format!("rtsp://{address}/video").parse().unwrap(),
            username: String::new(),
            password: String::new(),
        };
        let task = tokio::spawn({
            let shutdown = shutdown.clone();
            async move { start_session(&source, &IngestOptions::default(), &shutdown).await }
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
        assert_eq!(nominal_from_rate((1, 241)), None);
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
}
