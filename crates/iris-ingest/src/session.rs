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
    loop {
        if shutdown.is_cancelled() {
            return;
        }
        let started = Instant::now();
        let outcome = tokio::select! {
            () = shutdown.cancelled() => return,
            outcome = run_session(&source, &options, sink.as_ref(), &stats, &shutdown) => outcome,
        };
        stats.set_connected(false);
        match outcome {
            Ok(()) => return,
            Err(error) => {
                warn!(stream = %source.key, %error, "session ended; reconnecting");
                stats.record_reconnect();
            }
        }
        if started.elapsed() >= HEALTHY_SESSION {
            backoff = Duration::from_secs(1);
        }
        let delay = jittered(backoff);
        tokio::select! {
            () = shutdown.cancelled() => return,
            () = tokio::time::sleep(delay) => {}
        }
        backoff = (backoff * 2).min(MAX_BACKOFF);
    }
}

/// Applies ±20 % jitter derived from the clock's sub-second bits.
fn jittered(base: Duration) -> Duration {
    let nanos =
        u64::from(Instant::now().elapsed().subsec_nanos()).wrapping_mul(0x9e37_79b9_7f4a_7c15);
    let unit = (nanos % 401) as i64 - 200; // -200..=200
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
    let session_options = SessionOptions::default()
        .creds(Some(Credentials {
            username: source.username.clone(),
            password: source.password.clone(),
        }))
        .user_agent(options.user_agent.clone())
        // Cameras with small session tables free a torn-down session at once but
        // keep a silently closed one until its timeout, so always say goodbye.
        .teardown(TeardownPolicy::Always);
    let mut session = tokio::time::timeout(
        SETUP_TIMEOUT,
        Session::describe(source.url.clone(), session_options),
    )
    .await
    .map_err(|_| SessionError::SetupTimeout("DESCRIBE"))??;
    let stream_index = session
        .streams()
        .iter()
        .position(|s| s.media() == "video" && s.encoding_name().eq_ignore_ascii_case("h265"))
        .ok_or(SessionError::NoH265Stream)?;
    let declared_nominal = session.streams()[stream_index]
        .framerate()
        .and_then(nominal_from_fps);
    tokio::time::timeout(
        SETUP_TIMEOUT,
        session.setup(
            stream_index,
            SetupOptions::default()
                .transport(Transport::Tcp(TcpTransportOptions::default()))
                .frame_format(FrameFormat::MP4),
        ),
    )
    .await
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
    let playing = tokio::time::timeout(SETUP_TIMEOUT, session.play(play_options))
        .await
        .map_err(|_| SessionError::SetupTimeout("PLAY"))??;
    let mut demuxed = playing.demuxed()?;
    stats.set_connected(true);
    info!(stream = %source.key, "playing");

    let mut session_state = SessionState {
        clock: WallClock::new(),
        assembler: None,
        nominal: declared_nominal,
        snaps_seen: 0,
        holds_seen: (0, 0),
        previous_frame: stats.last_frame_wall(),
        stream_index,
    };

    let mut last_item = Instant::now();
    loop {
        let item = if let Some(interval) = options.read_interval {
            let Poll::Ready(item) = poll_now(&mut demuxed) else {
                tokio::select! {
                    () = shutdown.cancelled() => {
                        session_state.flush(sink, source, stats);
                        return Ok(());
                    }
                    () = next_tick(options.ticker.as_ref(), interval) => {}
                }
                if last_item.elapsed() >= options.inactivity_timeout {
                    return Err(SessionError::Inactivity(options.inactivity_timeout));
                }
                continue;
            };
            item
        } else {
            tokio::select! {
                () = shutdown.cancelled() => {
                    session_state.flush(sink, source, stats);
                    return Ok(());
                }
                item = tokio::time::timeout(options.inactivity_timeout, demuxed.next()) => match item {
                    Ok(item) => item,
                    Err(_elapsed) => return Err(SessionError::Inactivity(options.inactivity_timeout)),
                },
            }
        };
        last_item = Instant::now();
        let Some(item) = item else {
            return Err(SessionError::EndOfStream);
        };
        let item = item?;
        match item {
            CodecItem::VideoFrame(frame) => {
                session_state.on_frame(frame, &demuxed, source, options, sink, stats)?;
            }
            CodecItem::Rtcp(packet) => session_state.on_rtcp(&packet, stats),
            _ => {}
        }
    }
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
}

impl SessionState {
    /// Delivers the pending fragment at shutdown.
    fn flush(&mut self, sink: &dyn FragmentSink, source: &StreamSource, stats: &StreamStats) {
        if let Some(fragment) = self.assembler.as_mut().and_then(FragmentAssembler::flush) {
            stats.record_fragment_size(fragment.payload_len(), fragment.duration());
            deliver(sink, &source.key, fragment, stats);
        }
    }

    /// Records the camera clock offset carried by RTCP sender reports.
    fn on_rtcp(&self, packet: &retina::rtcp::ReceivedCompoundPacket, stats: &StreamStats) {
        let nvr = self.clock.wall_at(packet.ctx().received());
        for pkt in packet.pkts() {
            if let Ok(Some(report)) = pkt.as_sender_report() {
                let camera = WallUs::from_ntp64(report.ntp_timestamp().0);
                stats.record_sender_report(camera.micros_since(nvr));
            }
        }
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
        let wall = self.clock.wall_at(frame.start_ctx().received());
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
            if self.nominal.is_none() {
                self.nominal = video.frame_rate().and_then(nominal_from_rate);
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
        if let Some(live) = &options.live
            && let Some(data) = live_data
            && let Some(decode_time) = assembler.last_provisional_decode_time()
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
        }
        let snaps = assembler.snap_count();
        if snaps > self.snaps_seen {
            stats.add_snaps(snaps - self.snaps_seen);
            self.snaps_seen = snaps;
        }
        if let Some(fragment) = completed {
            note_closed_fragment(
                &mut self.holds_seen,
                assembler,
                &fragment,
                &source.key,
                stats,
            );
            deliver(sink, &source.key, fragment, stats);
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

fn deliver(
    sink: &dyn FragmentSink,
    key: &StreamKey,
    fragment: iris_mp4::fragment::Fragment,
    stats: &StreamStats,
) {
    let accepted = sink.send(key, fragment).is_ok();
    if !accepted {
        warn!(stream = %key, "sink rejected a fragment; dropping it");
    }
    stats.record_fragment(accepted);
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

/// Maps monotonic receive instants to wall time by a fixed offset taken when
/// the session started playing.
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

    fn wall_at(&self, received: Instant) -> WallUs {
        let micros = if received >= self.instant {
            i64::try_from(received.duration_since(self.instant).as_micros()).unwrap_or(i64::MAX)
        } else {
            -i64::try_from(self.instant.duration_since(received).as_micros()).unwrap_or(i64::MAX)
        };
        self.wall.offset(micros)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
    }

    #[test]
    fn jitter_stays_within_twenty_percent() {
        for _ in 0..50 {
            let d = jittered(Duration::from_secs(10)).as_millis();
            assert!((8_000..=12_000).contains(&d), "{d} ms");
        }
    }

    #[test]
    fn wall_clock_maps_instants_in_both_directions() {
        let clock = WallClock::new();
        let later = clock.instant + Duration::from_millis(5);
        assert_eq!(clock.wall_at(later).micros_since(clock.wall), 5_000);
        let earlier = clock
            .instant
            .checked_sub(Duration::from_millis(3))
            .expect("process uptime exceeds 3 ms");
        assert_eq!(clock.wall_at(earlier).micros_since(clock.wall), -3_000);
    }
}
