//! The TCP listener, per-connection RTSP state machine, and the paced sender.

use std::collections::HashMap;
use std::error::Error;
use std::fmt;
use std::fmt::Write as _;
use std::io;
use std::mem;
use std::net::SocketAddr;
use std::num::NonZeroU64;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use iris_core::time::{TIMESCALE, WallUs};
use iris_h265::LengthPrefixedNals;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Semaphore, mpsc, oneshot};
use tokio::task::JoinHandle;
use tracing::{debug, info, warn};

use crate::media::{Frame, Media, Ticks};
use crate::rtp::{self, Packetizer, RtpError, RtpTimestamp, Ssrc};
use crate::rtsp::{self, InterleavedChannels, MountName, Parsed, Request};
use crate::sdp;

/// Interval between RTCP sender reports.
const SENDER_REPORT_INTERVAL: Duration = Duration::from_secs(5);
/// Outgoing frames buffered per connection before the sender waits.
const OUTGOING_QUEUE: usize = 256;
/// Time allowed for a queued terminal RTSP response to reach the socket.
const CONTROL_DRAIN_TIMEOUT: Duration = Duration::from_secs(1);
/// Default cap on connections served at once; `replay-bench` drives at most
/// 256 cameras, one session each.
pub const DEFAULT_MAX_CONNECTIONS: usize = 256;
/// Longest accepted request or idle deadline.
const MAX_DEADLINE: Duration = Duration::from_hours(24);

/// How a frame's RTP packets reach the socket.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Delivery {
    /// One write per packet, back to back, as cameras send them: one TCP
    /// segment per RTP packet, which reproduces the receiver's wake-up pattern.
    #[default]
    PerPacket,
    /// One write per access unit: cheaper to send, and it understates the
    /// receiver's per-packet cost.
    Burst,
    /// One write per packet, the packets spread over the frame interval, as a
    /// camera on a slow link delivers them.
    Paced,
}

/// What stamps a frame of the synthetic audio stream.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum AudioStamps {
    /// The sample clock: frame `n` is stamped `n × 1,024`, whatever the
    /// instant it is sent at (RFC 3550 §5.1).
    #[default]
    SampleClock,
    /// The camera's media clock at the instant the frame starts, as a
    /// camera that stamps its audio from its system clock does.
    CameraClock,
}

/// Sound the synthetic camera leaves out at once, as a camera does that
/// loses what it had taken: the frames are never packed, so sequence
/// numbers run on and every later frame is sent in its own time.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LeftOut {
    /// The number of the first frame left out.
    pub at: u64,
    /// How many frames are left out.
    pub frames: u64,
}

/// How the synthetic camera's clocks run against each other and against
/// true time, for checks of what detects their parting. Every clock is
/// exact by default.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Clocks {
    /// How fast the camera's media clock runs against true time, in parts
    /// per million: frames keep their timestamps and are delivered that
    /// much sooner, and sender reports pair true time with the fast clock.
    pub camera_fast_ppm: i32,
    /// How fast the audio's sample clock runs against the camera's media
    /// clock, in parts per million: audio frames are produced that much
    /// faster than their nominal rate.
    pub audio_fast_ppm: i32,
    /// What stamps an audio frame.
    pub audio_stamps: AudioStamps,
    /// Whether the audio stream sends sender reports of its own, each
    /// pairing true time with the clock that stamps its frames.
    pub audio_reports: bool,
    /// Whether the sender reports are stamped in the ways a camera's are
    /// (`docs/research/14-drift.md` §3), the rarer ways more often than
    /// measured. The two reports made together are behind their clocks by
    /// the same part of the time between two video frames, every sixteenth
    /// in turn. The video's is behind by up to 6 ms more, in one pair of
    /// five by 60 ms more, and in one of fifteen 20 ms ahead instead. One
    /// pair in three carries a wall time 50 ms behind, as a camera's does
    /// whose clock moves against its media clocks.
    pub late_reports: bool,
}

/// How far ahead of the audio's the video's sender report is stamped in
/// one pair of fifteen under [`Clocks::late_reports`].
const EARLY_REPORT: Duration = Duration::from_millis(20);
/// How far behind the audio's the video's is stamped in one pair of five.
const FAR_LATE_REPORT: Duration = Duration::from_millis(60);
/// How far behind true time the wall time of one pair in three is.
const WALL_BEHIND_US: i64 = 50_000;

/// What the pair of sender reports numbered `index` carries under
/// [`Clocks::late_reports`], for clocks that read `elapsed` and a wall
/// time of `wall`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct LatePair {
    /// The reading of the clock the video's report is stamped with.
    video: Duration,
    /// The reading the audio's report is stamped with.
    audio: Duration,
    /// The wall time of both.
    wall: WallUs,
}

impl LatePair {
    fn new(elapsed: Duration, wall: WallUs, index: u64, video_period: Duration) -> Self {
        let audio = elapsed.saturating_sub(video_period * (index * 7 % 16) as u32 / 16);
        let video = if index % 15 == 14 {
            audio + EARLY_REPORT
        } else if index % 5 == 3 {
            audio.saturating_sub(FAR_LATE_REPORT)
        } else {
            audio.saturating_sub(Duration::from_millis(index % 4 * 2))
        };
        let wall = if index % 3 == 1 {
            WallUs::new(wall.get() - WALL_BEHIND_US)
        } else {
            wall
        };
        Self { video, audio, wall }
    }
}

/// What a clock that runs `fast_ppm` fast reads after `duration` of the
/// clock it is set against.
fn fast_clock(duration: Duration, fast_ppm: i32) -> Duration {
    scaled(duration, 1_000_000 + i64::from(fast_ppm), 1_000_000)
}

/// The time, on the clock it is set against, at which a clock that runs
/// `fast_ppm` fast reads `duration`.
fn slow_clock(duration: Duration, fast_ppm: i32) -> Duration {
    scaled(duration, 1_000_000, 1_000_000 + i64::from(fast_ppm))
}

fn scaled(duration: Duration, numerator: i64, denominator: i64) -> Duration {
    let (numerator, denominator) = (numerator.max(1) as u128, denominator.max(1) as u128);
    let nanos = duration.as_nanos() * numerator / denominator;
    Duration::from_nanos(u64::try_from(nanos).unwrap_or(u64::MAX))
}

/// Replay behaviour shared by all sessions.
#[derive(Clone, Debug)]
pub struct Options {
    /// Largest RTP packet, header included.
    pub mtu: usize,
    /// Replay the file again when it ends, with continuous timestamps.
    pub repeat: bool,
    /// Drop every n-th RTP packet. A dropped packet keeps its sequence
    /// number, as a packet lost on the network does.
    pub drop_every: Option<NonZeroU64>,
    /// Pause sending once, this long after PLAY, for `stall_for`; the paused
    /// frames are then sent in a burst with their original timestamps, as a
    /// network stall would deliver them.
    pub stall_after: Option<Duration>,
    /// Length of the stall.
    pub stall_for: Duration,
    /// How packets reach the socket.
    pub delivery: Delivery,
    /// The RTP timestamp of the first frame of every session; a value drawn
    /// per session when `None`. A value just below 2^32 puts the 32-bit wrap
    /// inside the stream.
    pub base_rtp: Option<RtpTimestamp>,
    /// Offer a synthetic mono AAC audio stream at this sampling rate beside
    /// the video, as `trackID=1`: frames of [`audio_frame`] bytes, one every
    /// 1,024 samples. A rate MPEG-4 audio has no index for offers none.
    pub audio_sample_rate: Option<u32>,
    /// Stamp some audio frames early, as cameras do: every seventh frame so
    /// that its RTP timestamp is 384 units of the sampling clock before the
    /// frame ahead of it, and every thirty-first a whole frame before.
    pub audio_timestamp_wander: bool,
    /// Leave out the audio frame with this number, once: its packet is never
    /// sent, so the sequence number and the timestamp skip it, as a packet
    /// the camera lost does.
    pub audio_lose_frame: Option<u64>,
    /// Leave sound out, once. Stamped by the camera's clock, the frames
    /// after it carry the timestamps of their own instants; stamped by the
    /// sample clock, they carry on the count of what was sent.
    pub audio_leave_out: Option<LeftOut>,
    /// How the camera's clocks run.
    pub clocks: Clocks,
    /// The time between the sender reports of a stream.
    pub report_interval: Duration,
    /// Connections served at once; the listener closes further connections
    /// as it accepts them.
    pub max_connections: usize,
    /// Before `PLAY`, the time a connection has to complete each request.
    /// Neither trickled bytes nor interleaved frames extend it, and a reply
    /// that cannot be queued within it ends the connection.
    pub request_timeout: Duration,
    /// After `PLAY`, the longest silence between client requests or
    /// interleaved frames, and the longest wait to queue a reply. It stays
    /// above the recorder's keepalive, which retina sends every 30 s for the
    /// default 60 s session timeout.
    pub idle_timeout: Duration,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            mtu: 1_400,
            repeat: true,
            drop_every: None,
            stall_after: None,
            stall_for: Duration::from_secs(10),
            delivery: Delivery::PerPacket,
            base_rtp: None,
            audio_sample_rate: None,
            audio_timestamp_wander: false,
            audio_lose_frame: None,
            audio_leave_out: None,
            clocks: Clocks::default(),
            report_interval: SENDER_REPORT_INTERVAL,
            max_connections: DEFAULT_MAX_CONNECTIONS,
            request_timeout: Duration::from_secs(10),
            idle_timeout: Duration::from_secs(120),
        }
    }
}

/// Mount names to recordings.
pub type Mounts = HashMap<MountName, Arc<Media>>;

/// Why a server cannot start or keep accepting connections.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ServerError {
    /// The MTU is outside the supported range.
    #[error(transparent)]
    Mtu(#[from] RtpError),
    /// The connection cap is zero or above [`Semaphore::MAX_PERMITS`].
    #[error("the replay connection cap {0} is outside 1..={max}", max = Semaphore::MAX_PERMITS)]
    ConnectionCap(usize),
    /// A deadline is zero or longer than a day.
    #[error("replay request and idle deadlines must be positive and at most one day")]
    Deadline,
    /// No mount was given.
    #[error("at least one replay mount is required")]
    NoMounts,
    /// The address could not be bound.
    #[error("binding {addr}")]
    Bind {
        /// The address.
        addr: SocketAddr,
        /// The bind failure.
        #[source]
        source: io::Error,
    },
    /// Accepting a connection failed.
    #[error("accepting a connection")]
    Accept(#[source] io::Error),
}

/// A bound listener. Its `Debug` output names the address, the mounts, the
/// options, and the free connection slots.
pub struct Server {
    listener: TcpListener,
    mounts: Arc<Mounts>,
    options: Arc<Options>,
    counter: Arc<AtomicU64>,
    connections: Arc<Semaphore>,
}

impl fmt::Debug for Server {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut mounts: Vec<&str> = self.mounts.keys().map(MountName::as_str).collect();
        mounts.sort_unstable();
        f.debug_struct("Server")
            .field("local_addr", &self.listener.local_addr().ok())
            .field("mounts", &mounts)
            .field("options", &self.options)
            .field("free_connections", &self.connections.available_permits())
            .finish_non_exhaustive()
    }
}

impl Server {
    /// Binds `addr` for `mounts`. Any address is bound as given: the binary
    /// refuses one outside loopback unless `--allow-remote` accepts it, and
    /// code that serves beyond loopback makes that decision itself.
    ///
    /// # Errors
    ///
    /// [`ServerError::Mtu`] for an MTU outside `256..=65535`,
    /// [`ServerError::ConnectionCap`] for a cap of zero or above
    /// [`Semaphore::MAX_PERMITS`], [`ServerError::Deadline`] for a request or
    /// idle deadline outside a millisecond to a day, [`ServerError::NoMounts`]
    /// for an empty mount set, and [`ServerError::Bind`] when the address cannot
    /// be bound.
    pub async fn bind(
        addr: SocketAddr,
        mounts: Mounts,
        options: Options,
    ) -> Result<Self, ServerError> {
        rtp::validate_mtu(options.mtu)?;
        if !(1..=Semaphore::MAX_PERMITS).contains(&options.max_connections) {
            return Err(ServerError::ConnectionCap(options.max_connections));
        }
        let deadline_range = Duration::from_millis(1)..=MAX_DEADLINE;
        if !deadline_range.contains(&options.request_timeout)
            || !deadline_range.contains(&options.idle_timeout)
        {
            return Err(ServerError::Deadline);
        }
        if mounts.is_empty() {
            return Err(ServerError::NoMounts);
        }
        let listener = TcpListener::bind(addr)
            .await
            .map_err(|source| ServerError::Bind { addr, source })?;
        Ok(Self {
            listener,
            mounts: Arc::new(mounts),
            connections: Arc::new(Semaphore::new(options.max_connections)),
            options: Arc::new(options),
            counter: Arc::new(AtomicU64::new(seed())),
        })
    }

    /// The bound address.
    ///
    /// # Errors
    ///
    /// The socket's error when its address cannot be read.
    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.listener.local_addr()
    }

    /// Accepts connections until the listener fails. A connection beyond
    /// [`Options::max_connections`] is closed when it is accepted.
    ///
    /// # Errors
    ///
    /// [`ServerError::Accept`] with the first accept failure; serving stops.
    pub async fn run(self) -> Result<(), ServerError> {
        loop {
            let (stream, peer) = self.listener.accept().await.map_err(ServerError::Accept)?;
            let Ok(permit) = Arc::clone(&self.connections).try_acquire_owned() else {
                warn!(
                    %peer,
                    limit = self.options.max_connections,
                    "connection limit reached; closing"
                );
                drop(stream);
                continue;
            };
            let mounts = Arc::clone(&self.mounts);
            let options = Arc::clone(&self.options);
            let counter = Arc::clone(&self.counter);
            tokio::spawn(async move {
                let _permit = permit;
                debug!(%peer, "connected");
                match serve(stream, &mounts, &options, &counter).await {
                    Ok(()) => debug!(%peer, "closed"),
                    Err(error) => warn!(%peer, error = %Chain(&error), "connection ended"),
                }
            });
        }
    }
}

/// An error and its sources, outermost first, on one line.
struct Chain<'a>(&'a (dyn Error + 'static));

impl fmt::Display for Chain<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)?;
        let mut source = self.0.source();
        while let Some(error) = source {
            write!(f, ": {error}")?;
            source = error.source();
        }
        Ok(())
    }
}

/// Why a connection ended before its peer closed it.
#[derive(Debug, thiserror::Error)]
enum ConnectionError {
    /// The socket option could not be set.
    #[error("setting TCP_NODELAY")]
    NoDelay(#[source] io::Error),
    /// Reading the socket failed.
    #[error("reading the connection")]
    Read(#[source] io::Error),
    /// No complete request arrived within the current deadline.
    #[error("no complete request within {} s {state}", .limit.as_secs_f64())]
    RequestDeadline {
        /// Before or after `PLAY`.
        state: &'static str,
        /// The deadline.
        limit: Duration,
    },
    /// A reply could not be queued within the current deadline: the peer
    /// stopped reading.
    #[error("a reply could not be queued within {} s {state}", .limit.as_secs_f64())]
    ReplyDeadline {
        /// Before or after `PLAY`.
        state: &'static str,
        /// The deadline.
        limit: Duration,
    },
    /// The bytes are not RTSP.
    #[error("not an RTSP request")]
    Invalid,
}

/// A pseudo-random seed from the clock, for SSRCs, sequence numbers, and session ids.
fn seed() -> u64 {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    (nanos as u64) ^ 0x9e37_79b9_7f4a_7c15
}

/// The next pseudo-random value.
fn next_random(counter: &AtomicU64) -> u64 {
    let n = counter.fetch_add(0x9e37_79b9_7f4a_7c15, Ordering::Relaxed);
    let mut x = n;
    x ^= x >> 30;
    x = x.wrapping_mul(0xbf58_476d_1ce4_e5b9);
    x ^= x >> 27;
    x = x.wrapping_mul(0x94d0_49bb_1331_11eb);
    x ^ (x >> 31)
}

/// Per-connection state.
struct Connection {
    session_id: String,
    mount: Option<String>,
    media: Option<Arc<Media>>,
    channels: Option<InterleavedChannels>,
    /// The channels of the audio stream, when it was set up.
    audio_channels: Option<InterleavedChannels>,
    ssrc: Ssrc,
    seq: u16,
    base_rtp: RtpTimestamp,
    player: Option<JoinHandle<()>>,
}

struct TerminalFrame {
    bytes: Vec<u8>,
    delivered: oneshot::Sender<()>,
}

enum Outgoing {
    Control(Vec<u8>),
    Media(Vec<u8>),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RequestLoopEnd {
    InputClosed,
    Terminal,
}

async fn serve(
    stream: TcpStream,
    mounts: &Mounts,
    options: &Arc<Options>,
    counter: &AtomicU64,
) -> Result<(), ConnectionError> {
    stream.set_nodelay(true).map_err(ConnectionError::NoDelay)?;
    let (mut reader, writer) = stream.into_split();
    let (tx, rx) = mpsc::channel::<Outgoing>(OUTGOING_QUEUE);
    let (terminal_tx, terminal_rx) = mpsc::channel(1);
    let mut sender = tokio::spawn(write_queued(writer, rx, terminal_rx));
    let random = next_random(counter);
    let mut connection = Connection {
        session_id: format!("{:016x}", next_random(counter)),
        mount: None,
        media: None,
        channels: None,
        audio_channels: None,
        ssrc: Ssrc::new((random >> 32) as u32),
        seq: (random & 0xffff) as u16,
        base_rtp: options
            .base_rtp
            .unwrap_or_else(|| RtpTimestamp::new((next_random(counter) & 0xffff_ffff) as u32)),
        player: None,
    };
    let result = request_loop(
        &mut reader,
        &tx,
        &terminal_tx,
        &mut connection,
        mounts,
        options,
    )
    .await;
    let terminal = matches!(&result, Ok(RequestLoopEnd::Terminal));
    if let Some(player) = connection.player.take() {
        player.abort();
        let _ = player.await;
    }
    drop(tx);
    drop(terminal_tx);
    if terminal {
        if !sender.is_finished() {
            sender.abort();
        }
        let _ = sender.await;
    } else if tokio::time::timeout(CONTROL_DRAIN_TIMEOUT, &mut sender)
        .await
        .is_err()
    {
        sender.abort();
        let _ = sender.await;
    }
    result.map(|_| ())
}

async fn write_queued(
    mut writer: OwnedWriteHalf,
    mut rx: mpsc::Receiver<Outgoing>,
    mut terminal_rx: mpsc::Receiver<TerminalFrame>,
) {
    let mut terminal_open = true;
    loop {
        tokio::select! {
            biased;
            terminal = terminal_rx.recv(), if terminal_open => {
                let Some(terminal) = terminal else {
                    terminal_open = false;
                    continue;
                };
                while let Ok(queued) = rx.try_recv() {
                    if let Outgoing::Control(bytes) = queued
                        && let Err(error) = writer.write_all(&bytes).await
                    {
                        debug!(%error, "socket write failed");
                        return;
                    }
                }
                match writer.write_all(&terminal.bytes).await {
                    Ok(()) => {
                        let _ = terminal.delivered.send(());
                    }
                    Err(error) => debug!(%error, "socket write failed"),
                }
                return;
            }
            frame = rx.recv() => {
                let Some(frame) = frame else {
                    return;
                };
                let bytes = match frame {
                    Outgoing::Control(bytes) | Outgoing::Media(bytes) => bytes,
                };
                if let Err(error) = writer.write_all(&bytes).await {
                    debug!(%error, "socket write failed");
                    return;
                }
            }
        }
    }
}

async fn deliver_terminal(tx: &mpsc::Sender<TerminalFrame>, bytes: Vec<u8>) -> bool {
    let (delivered, delivery) = oneshot::channel();
    let terminal = TerminalFrame { bytes, delivered };
    matches!(
        tokio::time::timeout(CONTROL_DRAIN_TIMEOUT, async {
            tx.send(terminal).await.ok()?;
            delivery.await.ok()
        })
        .await,
        Ok(Some(()))
    )
}

async fn request_loop(
    reader: &mut OwnedReadHalf,
    tx: &mpsc::Sender<Outgoing>,
    terminal_tx: &mpsc::Sender<TerminalFrame>,
    connection: &mut Connection,
    mounts: &Mounts,
    options: &Arc<Options>,
) -> Result<RequestLoopEnd, ConnectionError> {
    let mut buf = Vec::with_capacity(4096);
    let mut deadline = next_request_deadline(connection, options);
    loop {
        match rtsp::parse(&buf) {
            Parsed::Incomplete => {
                let Ok(read) = tokio::time::timeout_at(deadline, reader.read_buf(&mut buf)).await
                else {
                    let (state, limit) = current_deadline(connection, options);
                    return Err(ConnectionError::RequestDeadline { state, limit });
                };
                if read.map_err(ConnectionError::Read)? == 0 {
                    return Ok(RequestLoopEnd::InputClosed);
                }
            }
            Parsed::Interleaved(len) => {
                buf.drain(..len);
                // Before `PLAY` the deadline covers the next request, which a
                // client's interleaved frames do not bring.
                if connection.player.is_some() {
                    deadline = next_request_deadline(connection, options);
                }
            }
            Parsed::Invalid => return Err(ConnectionError::Invalid),
            Parsed::Request(request, consumed) => {
                buf.drain(..consumed);
                let teardown = request.method == "TEARDOWN"
                    && connection.channels.is_some()
                    && session_matches(&request, connection);
                let reply = handle(&request, tx, connection, mounts, options);
                if teardown {
                    if !deliver_terminal(terminal_tx, reply).await {
                        warn!("terminal RTSP response was not delivered before the deadline");
                    }
                    return Ok(RequestLoopEnd::Terminal);
                }
                // A peer that stops reading fills the queue; its next reply
                // then ends the connection instead of holding the slot.
                let queued = next_request_deadline(connection, options);
                match tokio::time::timeout_at(queued, tx.send(Outgoing::Control(reply))).await {
                    Ok(Ok(())) => {}
                    Ok(Err(_)) => return Ok(RequestLoopEnd::InputClosed),
                    Err(_) => {
                        let (state, limit) = current_deadline(connection, options);
                        return Err(ConnectionError::ReplyDeadline { state, limit });
                    }
                }
                deadline = next_request_deadline(connection, options);
            }
        }
    }
}

/// The deadline in force: the per-request deadline before `PLAY`, the idle
/// deadline once playing.
fn current_deadline(connection: &Connection, options: &Options) -> (&'static str, Duration) {
    if connection.player.is_some() {
        ("after PLAY", options.idle_timeout)
    } else {
        ("before PLAY", options.request_timeout)
    }
}

/// When the next complete request, or after `PLAY` an interleaved frame, must
/// have arrived.
fn next_request_deadline(connection: &Connection, options: &Options) -> tokio::time::Instant {
    tokio::time::Instant::now() + current_deadline(connection, options).1
}

/// The reply that leaves a request without effect: a state error.
fn not_valid_in_this_state(cseq: u32) -> Vec<u8> {
    rtsp::response(cseq, 455, "Method Not Valid in This State", &[], b"")
}

fn not_found(cseq: u32) -> Vec<u8> {
    rtsp::response(cseq, 404, "Not Found", &[], b"")
}

fn handle(
    request: &Request,
    tx: &mpsc::Sender<Outgoing>,
    connection: &mut Connection,
    mounts: &Mounts,
    options: &Arc<Options>,
) -> Vec<u8> {
    let cseq = request.cseq;
    let session_id = connection.session_id.clone();
    let session = ("Session", session_id.as_str());
    match request.method.as_str() {
        "OPTIONS" => rtsp::response(
            cseq,
            200,
            "OK",
            &[(
                "Public",
                "OPTIONS, DESCRIBE, SETUP, PLAY, TEARDOWN, GET_PARAMETER, SET_PARAMETER",
            )],
            b"",
        ),
        "DESCRIBE" => describe(request, connection, mounts, options),
        "SETUP" => setup(request, connection, mounts, options, session),
        "PLAY" => {
            if let Some(reply) = session_error(request, connection) {
                return reply;
            }
            start_play(request, tx, connection, mounts, options, session)
        }
        "TEARDOWN" => {
            if let Some(reply) = session_error(request, connection) {
                return reply;
            }
            if let Some(player) = connection.player.as_ref() {
                player.abort();
            }
            rtsp::response(cseq, 200, "OK", &[session], b"")
        }
        "GET_PARAMETER" | "SET_PARAMETER" => {
            if let Some(reply) = session_error(request, connection) {
                return reply;
            }
            rtsp::response(cseq, 200, "OK", &[session], b"")
        }
        _ => rtsp::response(cseq, 405, "Method Not Allowed", &[], b""),
    }
}

fn setup(
    request: &Request,
    connection: &mut Connection,
    mounts: &Mounts,
    options: &Options,
    session: (&str, &str),
) -> Vec<u8> {
    let cseq = request.cseq;
    if is_audio_control(&request.url) {
        return setup_audio(request, connection, options, session);
    }
    if connection.channels.is_some() {
        return not_valid_in_this_state(cseq);
    }
    let Some(name) = rtsp::mount_name(&request.url) else {
        return not_found(cseq);
    };
    let Some(media) = mounts.get(name) else {
        return not_found(cseq);
    };
    if connection
        .mount
        .as_deref()
        .is_some_and(|selected| selected != name)
    {
        return not_valid_in_this_state(cseq);
    }
    let Some(channels) = request
        .header("transport")
        .and_then(rtsp::interleaved_transport)
    else {
        return rtsp::response(cseq, 461, "Unsupported Transport", &[], b"");
    };
    connection.mount = Some(name.to_owned());
    connection.media = Some(Arc::clone(media));
    connection.channels = Some(channels);
    let transport = format!(
        "RTP/AVP/TCP;unicast;interleaved={}-{}",
        channels.rtp, channels.rtcp
    );
    rtsp::response(cseq, 200, "OK", &[("Transport", &transport), session], b"")
}

/// Whether a `SETUP` URL names the audio stream's control, `trackID=1`.
fn is_audio_control(url: &str) -> bool {
    url.split('?')
        .next()
        .unwrap_or(url)
        .trim_end_matches('/')
        .ends_with("/trackID=1")
}

/// Sets up the offered audio stream: after the video's `SETUP`, on its
/// mount, in the same session, before `PLAY`, and once.
fn setup_audio(
    request: &Request,
    connection: &mut Connection,
    options: &Options,
    session: (&str, &str),
) -> Vec<u8> {
    let cseq = request.cseq;
    if options.audio_sample_rate.is_none() {
        return not_found(cseq);
    }
    if connection.channels.is_none()
        || connection.audio_channels.is_some()
        || connection.player.is_some()
        || rtsp::mount_name(&request.url) != connection.mount.as_deref()
    {
        return not_valid_in_this_state(cseq);
    }
    if !session_matches(request, connection) {
        return rtsp::response(cseq, 454, "Session Not Found", &[], b"");
    }
    let Some(channels) = request
        .header("transport")
        .and_then(rtsp::interleaved_transport)
    else {
        return rtsp::response(cseq, 461, "Unsupported Transport", &[], b"");
    };
    connection.audio_channels = Some(channels);
    let transport = format!(
        "RTP/AVP/TCP;unicast;interleaved={}-{}",
        channels.rtp, channels.rtcp
    );
    rtsp::response(cseq, 200, "OK", &[("Transport", &transport), session], b"")
}

fn session_error(request: &Request, connection: &Connection) -> Option<Vec<u8>> {
    if connection.channels.is_none() {
        return Some(not_valid_in_this_state(request.cseq));
    }
    (!session_matches(request, connection))
        .then(|| rtsp::response(request.cseq, 454, "Session Not Found", &[], b""))
}

fn session_matches(request: &Request, connection: &Connection) -> bool {
    request
        .header("session")
        .map(rtsp::session_id)
        .is_some_and(|session| session == connection.session_id)
}

fn describe(
    request: &Request,
    connection: &mut Connection,
    mounts: &Mounts,
    options: &Options,
) -> Vec<u8> {
    let cseq = request.cseq;
    let Some(name) = rtsp::mount_name(&request.url) else {
        return not_found(cseq);
    };
    let Some(media) = mounts.get(name) else {
        return not_found(cseq);
    };
    if connection.channels.is_some()
        && connection
            .mount
            .as_deref()
            .is_some_and(|selected| selected != name)
    {
        return not_valid_in_this_state(cseq);
    }
    connection.mount = Some(name.to_owned());
    connection.media = Some(Arc::clone(media));
    let frame_rate = media.frames().first().map_or(30.0, |frame| {
        f64::from(TIMESCALE) / frame.duration.get().max(1) as f64
    });
    let mut body = sdp::describe(media.parameter_sets(), frame_rate);
    if let Some(audio) = options.audio_sample_rate.and_then(sdp::describe_audio) {
        body.push_str(&audio);
    }
    let base = format!(
        "{}/",
        request
            .url
            .split('?')
            .next()
            .unwrap_or(&request.url)
            .trim_end_matches('/')
    );
    rtsp::response(
        cseq,
        200,
        "OK",
        &[("Content-Type", "application/sdp"), ("Content-Base", &base)],
        body.as_bytes(),
    )
}

fn start_play(
    request: &Request,
    tx: &mpsc::Sender<Outgoing>,
    connection: &mut Connection,
    mounts: &Mounts,
    options: &Arc<Options>,
    session: (&str, &str),
) -> Vec<u8> {
    let cseq = request.cseq;
    let Some(name) = rtsp::mount_name(&request.url) else {
        return not_found(cseq);
    };
    if !mounts.contains_key(name) {
        return not_found(cseq);
    }
    if connection.mount.as_deref() != Some(name) || connection.player.is_some() {
        return not_valid_in_this_state(cseq);
    }
    let (Some(media), Some(channels)) = (connection.media.clone(), connection.channels) else {
        return not_valid_in_this_state(cseq);
    };
    let player = tokio::spawn(play(
        media,
        Arc::clone(options),
        tx.clone(),
        Packetizer::new(connection.ssrc, connection.seq),
        connection.base_rtp,
        channels,
        connection.audio_channels,
    ));
    connection.player = Some(player);
    info!(session = %connection.session_id, "playing");
    let mut rtp_info = format!(
        "url={}/trackID=0;seq={};rtptime={}",
        request.url.trim_end_matches('/'),
        connection.seq,
        connection.base_rtp.get()
    );
    if connection.audio_channels.is_some() {
        // The audio stream starts at sequence number and timestamp zero.
        write!(
            rtp_info,
            ",url={}/trackID=1;seq=0;rtptime=0",
            request.url.trim_end_matches('/')
        )
        .expect("writing to a String cannot fail");
    }
    rtsp::response(
        cseq,
        200,
        "OK",
        &[("Range", "npt=0.000-"), ("RTP-Info", &rtp_info), session],
        b"",
    )
}

/// The NAL units sent for `frame`: the parameter sets before a sync frame,
/// then the stored access unit's. `None`, with a warning, when the frame
/// lies outside the recording or is not completely framed.
fn frame_nals<'media>(media: &'media Media, frame: &Frame) -> Option<Vec<&'media [u8]>> {
    let Some(sample) = media.sample(frame) else {
        warn!("a frame lies outside its recording");
        return None;
    };
    let mut nals: Vec<&[u8]> = Vec::with_capacity(4);
    if frame.is_sync {
        nals.extend(media.parameter_sets().iter().map(Vec::as_slice));
    }
    for nal in LengthPrefixedNals::new(sample) {
        match nal {
            Ok(nal) => nals.push(nal),
            Err(error) => {
                warn!(%error, "a stored access unit is not completely framed");
                return None;
            }
        }
    }
    Some(nals)
}

/// The bytes of synthetic audio frame `index`: its number, eight times over.
/// A recorder stores them as received, so a test can tell every frame apart.
#[must_use]
pub fn audio_frame(index: u64) -> Vec<u8> {
    index.to_be_bytes().repeat(8)
}

/// The synthetic audio stream of one playing session.
struct AudioPlayer {
    sample_rate: u32,
    channels: InterleavedChannels,
    ssrc: Ssrc,
    /// Whether some frames are stamped early.
    wander: bool,
    /// The frame that is never sent.
    lose: Option<u64>,
    /// How fast the sample clock runs against the camera's media clock.
    fast_ppm: i32,
    stamps: AudioStamps,
    /// The sound that is left out.
    leave_out: Option<LeftOut>,
    /// Frames left out so far.
    left_out: u64,
    /// Frames sent or lost on the way: the next sequence number. With
    /// those left out it numbers the next frame, and frame `n` starts
    /// `n × 1,024` samples after `PLAY`.
    sent: u64,
}

impl AudioPlayer {
    /// The audio stream of a session that set it up, when one is offered.
    fn new(
        options: &Options,
        channels: Option<InterleavedChannels>,
        video_ssrc: Ssrc,
    ) -> Option<Self> {
        Some(Self {
            sample_rate: options.audio_sample_rate?,
            channels: channels?,
            ssrc: Ssrc::new(!video_ssrc.get()),
            wander: options.audio_timestamp_wander,
            lose: options.audio_lose_frame,
            fast_ppm: options.clocks.audio_fast_ppm,
            stamps: options.clocks.audio_stamps,
            leave_out: options.audio_leave_out,
            left_out: 0,
            sent: 0,
        })
    }

    /// The reading of the clock that stamps the frames, `elapsed` of the
    /// camera's media clock after `PLAY`: what a sender report pairs with
    /// the wall time.
    fn report_timestamp(&self, elapsed: Duration) -> RtpTimestamp {
        let stamping = match self.stamps {
            AudioStamps::SampleClock => fast_clock(elapsed, self.fast_ppm),
            AudioStamps::CameraClock => elapsed,
        };
        let units = stamping.as_nanos() * u128::from(self.sample_rate) / 1_000_000_000;
        RtpTimestamp::new(units as u32)
    }

    /// Queues the frames due `elapsed` of the camera's media clock after
    /// `PLAY`; `false` when the session is over.
    async fn send_due(&mut self, elapsed: Duration, tx: &mpsc::Sender<Outgoing>) -> bool {
        let Some(frames) = self.frames_due(elapsed) else {
            warn!("audio framing failed");
            return false;
        };
        frames.is_empty() || tx.send(Outgoing::Media(frames)).await.is_ok()
    }

    /// How many units before its place the next frame is stamped.
    fn early(&self) -> u32 {
        match (self.wander, self.sent % 31, self.sent % 7) {
            (true, 5, _) => 2 * rtp::AAC_FRAME_SAMPLES,
            (true, _, 3) => rtp::AAC_FRAME_SAMPLES + 384,
            _ => 0,
        }
    }

    /// The interleaved packets of every frame that starts at or before
    /// `elapsed` of the camera's media clock since `PLAY` and was not sent;
    /// `None` when framing fails. A frame starts when the sample clock,
    /// which may run fast against the camera's, has counted its samples.
    fn frames_due(&mut self, elapsed: Duration) -> Option<Vec<u8>> {
        let mut out = Vec::new();
        let frame_samples = u64::from(rtp::AAC_FRAME_SAMPLES);
        loop {
            let frame = self.sent.checked_add(self.left_out)?;
            let samples = frame.checked_mul(frame_samples)?;
            let counted = Duration::from_micros(
                samples.checked_mul(1_000_000)? / u64::from(self.sample_rate.max(1)),
            );
            let starts = slow_clock(counted, self.fast_ppm);
            if starts > elapsed {
                return Some(out);
            }
            if let Some(leave_out) = self.leave_out.take_if(|leave_out| leave_out.at == frame) {
                self.left_out += leave_out.frames;
                continue;
            }
            if self.lose == Some(self.sent) {
                self.sent += 1;
                continue;
            }
            let stamp = match self.stamps {
                AudioStamps::SampleClock => self.sent.checked_mul(frame_samples)? as u32,
                AudioStamps::CameraClock => {
                    (starts.as_nanos() * u128::from(self.sample_rate) / 1_000_000_000) as u32
                }
            };
            let packet = rtp::aac_packet(
                self.ssrc,
                self.sent as u16,
                RtpTimestamp::new(stamp.wrapping_sub(self.early())),
                &audio_frame(frame),
            )
            .ok()?;
            rtp::interleave(&mut out, self.channels.rtp, &packet).ok()?;
            self.sent += 1;
        }
    }
}

/// The absolute replay timestamp and pacing deadline for one frame of a
/// camera whose media clock runs `fast_ppm` fast: the frame is due when
/// that clock reads its timestamp.
fn frame_due(
    start: Instant,
    decode_time: Ticks,
    loop_offset: Ticks,
    fast_ppm: i32,
) -> Option<(Ticks, Instant)> {
    let ticks = decode_time.checked_add(loop_offset)?;
    let due = start.checked_add(slow_clock(ticks.to_duration()?, fast_ppm))?;
    Some((ticks, due))
}

/// The deadline of one packet distributed across its frame interval.
fn packet_due(
    frame_due: Instant,
    frame_ticks: Ticks,
    packet_index: usize,
    packet_count: usize,
) -> Option<Instant> {
    if packet_index >= packet_count {
        return None;
    }
    let offset_ticks =
        u128::from(frame_ticks.get()).checked_mul(packet_index as u128)? / packet_count as u128;
    let offset_ticks = Ticks::new(u64::try_from(offset_ticks).ok()?);
    frame_due.checked_add(offset_ticks.to_duration()?)
}

/// Advances the continuous RTP timeline for a repeated file.
fn next_loop_offset(loop_offset: Ticks, total_ticks: Ticks) -> Option<Ticks> {
    let next = loop_offset.checked_add(total_ticks)?;
    (next > loop_offset).then_some(next)
}

/// RTP clock value `elapsed` of true time after `PLAY`, sampled at the same
/// monotonic instant as an RTCP wall time, for a camera whose media clock
/// runs `fast_ppm` fast.
fn replay_clock_timestamp(
    base_rtp: RtpTimestamp,
    elapsed: Duration,
    fast_ppm: i32,
) -> RtpTimestamp {
    let camera = fast_clock(elapsed, fast_ppm);
    let ticks = camera.as_nanos() * u128::from(TIMESCALE) / 1_000_000_000;
    base_rtp.after(Ticks::new(ticks as u64))
}

/// When a pair of sender reports is made.
#[derive(Clone, Copy)]
struct ReportAt {
    /// True time since `PLAY`.
    elapsed: Duration,
    /// The wall time then.
    wall: WallUs,
    /// How many pairs were made before this one.
    index: u64,
    /// The time between two video frames.
    video_period: Duration,
}

/// The sender reports a session has made.
#[derive(Default)]
struct Reports {
    last: Option<Instant>,
    made: u64,
}

impl Reports {
    /// When the next pair of reports is made, if `interval` has passed
    /// since the last, for a session that began at `start` with video
    /// frames `video_period` apart. The first pair precedes the first RTP
    /// packet, so its mapping uses the correlation of the session's start.
    fn due(
        &mut self,
        interval: Duration,
        start: (Instant, WallUs),
        video_period: Duration,
    ) -> Option<ReportAt> {
        if self.last.is_some_and(|last| last.elapsed() < interval) {
            return None;
        }
        let (sampled_at, wall) = self.last.map_or(start, |_| (Instant::now(), WallUs::now()));
        let at = ReportAt {
            elapsed: sampled_at.duration_since(start.0),
            wall,
            index: self.made,
            video_period,
        };
        self.last = Some(sampled_at);
        self.made += 1;
        Some(at)
    }
}

/// Appends the sender reports made at `at`: the video's, and the audio's
/// when it sends its own. Both sides of each mapping are sampled on the
/// delivery clock, so a fault-injection stall does not pair a delayed wall
/// time with an earlier frame timestamp.
fn append_reports(
    out: &mut Vec<u8>,
    video: (&Packetizer, RtpTimestamp, InterleavedChannels),
    audio: Option<&AudioPlayer>,
    clocks: Clocks,
    at: ReportAt,
) -> Result<(), RtpError> {
    let (packetizer, base_rtp, channels) = video;
    let pair = if clocks.late_reports {
        LatePair::new(at.elapsed, at.wall, at.index, at.video_period)
    } else {
        LatePair {
            video: at.elapsed,
            audio: at.elapsed,
            wall: at.wall,
        }
    };
    let stamp = replay_clock_timestamp(base_rtp, pair.video, clocks.camera_fast_ppm);
    let report = packetizer.sender_report(pair.wall, stamp);
    rtp::interleave(out, channels.rtcp, &report)?;
    if let Some(audio) = audio.filter(|_| clocks.audio_reports) {
        let stamp = audio.report_timestamp(fast_clock(pair.audio, clocks.camera_fast_ppm));
        let report = rtp::sender_report(audio.ssrc, pair.wall, stamp, audio.sent as u32, 0);
        rtp::interleave(out, audio.channels.rtcp, &report)?;
    }
    Ok(())
}

/// Sends the media paced by its decode times until the connection goes away.
async fn play(
    media: Arc<Media>,
    options: Arc<Options>,
    tx: mpsc::Sender<Outgoing>,
    mut packetizer: Packetizer,
    base_rtp: RtpTimestamp,
    channels: InterleavedChannels,
    audio_channels: Option<InterleavedChannels>,
) {
    let start = Instant::now();
    let start_wall = WallUs::now();
    let mut audio = AudioPlayer::new(&options, audio_channels, packetizer.ssrc());
    let mut loop_offset = Ticks::ZERO;
    let mut reports = Reports::default();
    let mut stalled = false;
    let mut packet_index = 0u64;
    let clocks = options.clocks;
    let fast_ppm = clocks.camera_fast_ppm;
    loop {
        for frame in media.frames() {
            let Some((ticks, due)) = frame_due(start, frame.decode_time, loop_offset, fast_ppm)
            else {
                warn!("replay timeline exceeds the pacing clock");
                return;
            };
            if let Some(after) = options.stall_after
                && !stalled
                && due.duration_since(start) >= after
            {
                stalled = true;
                info!(secs = options.stall_for.as_secs_f64(), "stalling");
                tokio::time::sleep(options.stall_for).await;
            }
            tokio::time::sleep_until(due.into()).await;
            let camera_elapsed = ticks.to_duration().unwrap_or(Duration::MAX);
            if let Some(audio) = audio.as_mut()
                && !audio.send_due(camera_elapsed, &tx).await
            {
                return;
            }
            let timestamp = base_rtp.after(ticks);
            let mut out = Vec::new();
            let period = frame.duration.to_duration().unwrap_or_default();
            if let Some(at) = reports.due(options.report_interval, (start, start_wall), period) {
                let video = (&packetizer, base_rtp, channels);
                if let Err(error) = append_reports(&mut out, video, audio.as_ref(), clocks, at) {
                    warn!(%error, "sender report framing failed");
                    return;
                }
            }
            let Some(nals) = frame_nals(&media, frame) else {
                return;
            };
            let packets = match packetizer.access_unit(&nals, timestamp, options.mtu) {
                Ok(packets) => packets,
                Err(error) => {
                    warn!(%error, "RTP packetization failed");
                    return;
                }
            };
            let count = packets.len();
            for (i, packet) in packets.into_iter().enumerate() {
                packet_index += 1;
                if options
                    .drop_every
                    .is_some_and(|n| packet_index.is_multiple_of(n.get()))
                {
                    continue;
                }
                if options.delivery == Delivery::Paced && i > 0 {
                    let Some(packet_due) = packet_due(due, frame.duration, i, count) else {
                        warn!("paced packet deadline exceeds the pacing clock");
                        return;
                    };
                    tokio::time::sleep_until(packet_due.into()).await;
                }
                if let Err(error) = rtp::interleave(&mut out, channels.rtp, &packet) {
                    warn!(%error, "RTP framing failed");
                    return;
                }
                if options.delivery != Delivery::Burst
                    && tx.send(Outgoing::Media(mem::take(&mut out))).await.is_err()
                {
                    return;
                }
            }
            if !out.is_empty() && tx.send(Outgoing::Media(out)).await.is_err() {
                return;
            }
        }
        if !options.repeat {
            info!("file ended");
            return;
        }
        let Some(next_offset) = next_loop_offset(loop_offset, media.total_ticks()) else {
            warn!("repeat timeline is exhausted or does not advance");
            return;
        };
        loop_offset = next_offset;
    }
}

#[cfg(test)]
mod tests {
    use bytes::Bytes;

    use super::*;
    use crate::media::Frame;

    fn test_media(fill: u8) -> Arc<Media> {
        Arc::new(Media::for_tests(
            Bytes::from(vec![0, 0, 0, 3, 0x26, 0x01, fill]),
            vec![vec![0x40, 0x01], vec![0x42, 0x01], vec![0x44, 0x01]],
            vec![Frame {
                offset: 0,
                len: 7,
                duration: Ticks::new(3_000),
                decode_time: Ticks::ZERO,
                is_sync: true,
            }],
            Ticks::new(3_000),
        ))
    }

    fn mount(name: &str) -> MountName {
        MountName::try_new(name).unwrap()
    }

    fn mounts() -> Mounts {
        Mounts::from([
            (mount("a"), test_media(0xaa)),
            (mount("b"), test_media(0xbb)),
        ])
    }

    fn connection() -> Connection {
        Connection {
            session_id: "session-1".to_owned(),
            mount: None,
            media: None,
            channels: None,
            audio_channels: None,
            ssrc: Ssrc::new(7),
            seq: 11,
            base_rtp: RtpTimestamp::new(13),
            player: None,
        }
    }

    fn request(method: &str, url: &str, headers: &[(&str, &str)]) -> Request {
        Request {
            method: method.to_owned(),
            url: url.to_owned(),
            cseq: 1,
            headers: headers
                .iter()
                .map(|(name, value)| (name.to_ascii_lowercase(), (*value).to_owned()))
                .collect(),
            body: Vec::new(),
        }
    }

    fn status(reply: &[u8]) -> u16 {
        let line = std::str::from_utf8(reply)
            .expect("RTSP response text")
            .lines()
            .next()
            .expect("status line");
        line.split_ascii_whitespace()
            .nth(1)
            .expect("status code")
            .parse()
            .expect("numeric status")
    }

    /// Handles `request` on `connection` with the default options.
    fn answer(
        connection: &mut Connection,
        tx: &mpsc::Sender<Outgoing>,
        method: &str,
        url: &str,
        headers: &[(&str, &str)],
    ) -> Vec<u8> {
        let options = Arc::new(Options::default());
        handle(
            &request(method, url, headers),
            tx,
            connection,
            &mounts(),
            &options,
        )
    }

    /// A connection set up on mount `a` with channels 4 and 5.
    fn set_up(tx: &mpsc::Sender<Outgoing>) -> Connection {
        let mut connection = connection();
        let reply = answer(
            &mut connection,
            tx,
            "SETUP",
            "rtsp://host/a/trackID=0",
            &[("transport", "RTP/AVP/TCP;unicast;interleaved=4-5")],
        );
        assert_eq!(status(&reply), 200);
        connection
    }

    const SESSION: [(&str, &str); 1] = [("session", "session-1")];

    /// Handles `request` on `connection` of a server that offers audio.
    fn answer_with_audio(
        connection: &mut Connection,
        tx: &mpsc::Sender<Outgoing>,
        method: &str,
        url: &str,
        headers: &[(&str, &str)],
    ) -> Vec<u8> {
        let options = Arc::new(Options {
            audio_sample_rate: Some(64_000),
            ..Options::default()
        });
        handle(
            &request(method, url, headers),
            tx,
            connection,
            &mounts(),
            &options,
        )
    }

    const AUDIO_SETUP: [(&str, &str); 2] = [
        ("session", "session-1"),
        ("transport", "RTP/AVP/TCP;unicast;interleaved=6-7"),
    ];

    /// The offered audio is set up once, after the video, on the video's
    /// mount, in its session, and before `PLAY`; a server that offers none
    /// has no such stream.
    #[tokio::test]
    async fn audio_is_set_up_once_after_the_video_on_its_mount_and_before_play() {
        let (tx, _rx) = mpsc::channel(16);
        let audio = "rtsp://host/a/trackID=1";
        // Before the video's SETUP.
        let mut early = connection();
        let reply = answer_with_audio(&mut early, &tx, "SETUP", audio, &AUDIO_SETUP);
        assert_eq!(status(&reply), 455);
        assert!(early.audio_channels.is_none());

        let mut connection = set_up(&tx);
        // Not offered.
        let reply = answer(&mut connection, &tx, "SETUP", audio, &AUDIO_SETUP);
        assert_eq!(status(&reply), 404);
        // Another mount, and no mount.
        for url in ["rtsp://host/b/trackID=1", "rtsp://host/trackID=1"] {
            let reply = answer_with_audio(&mut connection, &tx, "SETUP", url, &AUDIO_SETUP);
            assert_eq!(status(&reply), 455, "{url}");
        }
        // Another session.
        let other = [("session", "session-2"), AUDIO_SETUP[1]];
        let reply = answer_with_audio(&mut connection, &tx, "SETUP", audio, &other);
        assert_eq!(status(&reply), 454);
        // Another transport.
        let udp = [AUDIO_SETUP[0], ("transport", "RTP/AVP;unicast")];
        let reply = answer_with_audio(&mut connection, &tx, "SETUP", audio, &udp);
        assert_eq!(status(&reply), 461);
        assert!(connection.audio_channels.is_none());

        let reply = answer_with_audio(&mut connection, &tx, "SETUP", audio, &AUDIO_SETUP);
        assert_eq!(status(&reply), 200);
        assert!(
            String::from_utf8(reply)
                .unwrap()
                .contains("interleaved=6-7")
        );
        assert_eq!(
            connection.audio_channels,
            Some(InterleavedChannels { rtp: 6, rtcp: 7 })
        );
        // Once.
        let reply = answer_with_audio(&mut connection, &tx, "SETUP", audio, &AUDIO_SETUP);
        assert_eq!(status(&reply), 455);
    }

    /// Audio is not added to a session that plays.
    #[tokio::test]
    async fn audio_is_not_set_up_after_play() {
        let (tx, _rx) = mpsc::channel(16);
        let mut connection = set_up(&tx);
        let reply = answer_with_audio(&mut connection, &tx, "PLAY", "rtsp://host/a", &SESSION);
        assert_eq!(status(&reply), 200);
        let reply = answer_with_audio(
            &mut connection,
            &tx,
            "SETUP",
            "rtsp://host/a/trackID=1",
            &AUDIO_SETUP,
        );
        assert_eq!(status(&reply), 455);
        assert!(connection.audio_channels.is_none());
    }

    #[tokio::test]
    async fn options_does_not_advertise_pause() {
        let (tx, _rx) = mpsc::channel(16);
        let reply = answer(&mut connection(), &tx, "OPTIONS", "rtsp://host/a", &[]);
        assert!(!String::from_utf8(reply).unwrap().contains("PAUSE"));
    }

    #[tokio::test]
    async fn describe_selects_its_mount() {
        let (tx, _rx) = mpsc::channel(16);
        let mut connection = connection();
        let reply = answer(&mut connection, &tx, "DESCRIBE", "rtsp://host/a", &[]);
        assert_eq!(status(&reply), 200);
        assert_eq!(connection.mount.as_deref(), Some("a"));
    }

    #[tokio::test]
    async fn setup_of_a_missing_mount_binds_no_channels() {
        let (tx, _rx) = mpsc::channel(16);
        let mut connection = connection();
        let reply = answer(
            &mut connection,
            &tx,
            "SETUP",
            "rtsp://host/missing/trackID=0",
            &[("transport", "RTP/AVP/TCP;interleaved=0-1")],
        );
        assert_eq!(status(&reply), 404);
        assert!(connection.channels.is_none());
    }

    #[tokio::test]
    async fn setup_of_another_mount_than_described_is_refused() {
        let (tx, _rx) = mpsc::channel(16);
        let mut connection = connection();
        answer(&mut connection, &tx, "DESCRIBE", "rtsp://host/a", &[]);
        let reply = answer(
            &mut connection,
            &tx,
            "SETUP",
            "rtsp://host/b/trackID=0",
            &[("transport", "RTP/AVP/TCP;interleaved=0-1")],
        );
        assert_eq!(status(&reply), 455);
        assert!(connection.channels.is_none());
    }

    #[tokio::test]
    async fn setup_without_tcp_interleaving_is_refused() {
        let (tx, _rx) = mpsc::channel(16);
        let mut connection = connection();
        let reply = answer(
            &mut connection,
            &tx,
            "SETUP",
            "rtsp://host/a/trackID=0",
            &[("transport", "RTP/AVP;unicast")],
        );
        assert_eq!(status(&reply), 461);
        assert!(connection.channels.is_none());
    }

    #[tokio::test]
    async fn play_before_setup_is_not_valid_in_this_state() {
        let (tx, _rx) = mpsc::channel(16);
        let reply = answer(&mut connection(), &tx, "PLAY", "rtsp://host/a", &[]);
        assert_eq!(status(&reply), 455);
    }

    #[tokio::test]
    async fn setup_replies_with_the_channels_and_session() {
        let (tx, _rx) = mpsc::channel(16);
        let mut connection = connection();
        let reply = answer(
            &mut connection,
            &tx,
            "SETUP",
            "rtsp://host/a/trackID=0",
            &[("transport", "RTP/AVP/TCP;unicast;interleaved=4-5")],
        );
        let text = String::from_utf8(reply).unwrap();
        assert!(text.contains("interleaved=4-5\r\n"), "{text}");
        assert!(text.contains("Session: session-1\r\n"), "{text}");
        assert!(!text.contains("timeout="), "{text}");
    }

    #[tokio::test]
    async fn play_with_another_session_is_refused() {
        let (tx, _rx) = mpsc::channel(16);
        let mut connection = set_up(&tx);
        let reply = answer(
            &mut connection,
            &tx,
            "PLAY",
            "rtsp://host/a",
            &[("session", "wrong")],
        );
        assert_eq!(status(&reply), 454);
    }

    #[tokio::test]
    async fn play_of_another_mount_is_refused() {
        let (tx, _rx) = mpsc::channel(16);
        let mut connection = set_up(&tx);
        let reply = answer(&mut connection, &tx, "PLAY", "rtsp://host/b", &SESSION);
        assert_eq!(status(&reply), 455);
    }

    #[tokio::test]
    async fn playing_media_uses_the_negotiated_channels() {
        let (tx, mut rx) = mpsc::channel(16);
        let mut connection = set_up(&tx);
        let reply = answer(&mut connection, &tx, "PLAY", "rtsp://host/a", &SESSION);
        assert_eq!(status(&reply), 200);
        let first = tokio::time::timeout(Duration::from_secs(1), rx.recv())
            .await
            .expect("media arrives")
            .expect("sender open");
        let Outgoing::Media(first) = first else {
            panic!("player emitted a control response");
        };
        assert_eq!(first[1], 5, "RTCP uses the selected channel");
        let rtcp_len = usize::from(u16::from_be_bytes([first[2], first[3]]));
        assert_eq!(first[4 + rtcp_len + 1], 4, "RTP uses the selected channel");
    }

    #[tokio::test]
    async fn a_second_play_is_refused() {
        let (tx, _rx) = mpsc::channel(16);
        let mut connection = set_up(&tx);
        answer(&mut connection, &tx, "PLAY", "rtsp://host/a", &SESSION);
        let reply = answer(&mut connection, &tx, "PLAY", "rtsp://host/a", &SESSION);
        assert_eq!(status(&reply), 455);
    }

    #[tokio::test]
    async fn options_lists_the_methods_and_parameters_need_the_session() {
        let (tx, _rx) = mpsc::channel(16);
        let reply = answer(&mut connection(), &tx, "OPTIONS", "rtsp://host/a", &[]);
        assert_eq!(status(&reply), 200);
        assert!(
            String::from_utf8(reply)
                .unwrap()
                .contains("Public: OPTIONS, DESCRIBE, SETUP, PLAY, TEARDOWN, GET_PARAMETER")
        );
        let mut connection = set_up(&tx);
        for method in ["GET_PARAMETER", "SET_PARAMETER"] {
            let reply = answer(&mut connection, &tx, method, "rtsp://host/a", &SESSION);
            assert_eq!(status(&reply), 200, "{method}");
            let reply = answer(&mut connection, &tx, method, "rtsp://host/a", &[]);
            assert_ne!(status(&reply), 200, "{method} without its session");
        }
    }

    /// Before SETUP any mount may be described; after it, only the one set up.
    #[tokio::test]
    async fn describe_changes_mount_only_before_setup() {
        let (tx, _rx) = mpsc::channel(16);
        let mut fresh = connection();
        answer(&mut fresh, &tx, "DESCRIBE", "rtsp://host/a", &[]);
        let reply = answer(&mut fresh, &tx, "DESCRIBE", "rtsp://host/b", &[]);
        assert_eq!(status(&reply), 200);
        assert_eq!(fresh.mount.as_deref(), Some("b"));

        let mut bound = set_up(&tx);
        let same = answer(&mut bound, &tx, "DESCRIBE", "rtsp://host/a", &[]);
        assert_eq!(status(&same), 200);
        let other = answer(&mut bound, &tx, "DESCRIBE", "rtsp://host/b", &[]);
        assert_eq!(status(&other), 455);
        assert_eq!(bound.mount.as_deref(), Some("a"));
    }

    #[tokio::test]
    async fn describe_announces_the_rate_of_the_first_frame() {
        let (tx, _rx) = mpsc::channel(16);
        let reply = answer(&mut connection(), &tx, "DESCRIBE", "rtsp://host/a", &[]);
        let text = String::from_utf8(reply).unwrap();
        assert!(text.contains("a=framerate:30.000\r\n"), "{text}");
    }

    #[test]
    fn an_error_chain_is_written_on_one_line() {
        let error = ConnectionError::Read(io::Error::other("reset by peer"));
        assert_eq!(
            Chain(&error).to_string(),
            "reading the connection: reset by peer"
        );
    }

    /// The generator is `SplitMix64`: from its published constants, the first
    /// outputs for a zero seed.
    #[test]
    fn random_values_follow_splitmix64() {
        let counter = AtomicU64::new(0x9e37_79b9_7f4a_7c15);
        assert_eq!(next_random(&counter), 0xe220_a839_7b1d_cdaf);
        assert_eq!(next_random(&counter), 0x6e78_9e6a_a1b9_65f4);
    }

    /// The seed mixes the clock into the constant: it is neither the
    /// constant's bits all set nor nothing outside them.
    #[test]
    fn the_seed_mixes_the_clock_with_its_constant() {
        const MIX: u64 = 0x9e37_79b9_7f4a_7c15;
        let seed = seed();
        assert_ne!(seed & MIX, MIX);
        assert_ne!(seed & !MIX, 0);
    }

    #[tokio::test]
    async fn pause_is_not_allowed() {
        let (tx, _rx) = mpsc::channel(16);
        let mut connection = set_up(&tx);
        let reply = answer(&mut connection, &tx, "PAUSE", "rtsp://host/a", &SESSION);
        assert_eq!(status(&reply), 405);
    }

    #[tokio::test]
    async fn teardown_of_the_session_succeeds() {
        let (tx, _rx) = mpsc::channel(16);
        let mut connection = set_up(&tx);
        answer(&mut connection, &tx, "PLAY", "rtsp://host/a", &SESSION);
        let reply = answer(&mut connection, &tx, "TEARDOWN", "rtsp://host/a", &SESSION);
        assert_eq!(status(&reply), 200);
    }

    #[tokio::test]
    async fn play_after_natural_completion_returns_a_state_error() {
        let options = Arc::new(Options {
            repeat: false,
            ..Options::default()
        });
        let (tx, mut rx) = mpsc::channel(16);
        let mut connection = set_up(&tx);
        let mounts = mounts();
        let first = handle(
            &request("PLAY", "rtsp://host/a", &SESSION),
            &tx,
            &mut connection,
            &mounts,
            &options,
        );
        assert_eq!(status(&first), 200);
        tokio::time::timeout(Duration::from_secs(1), rx.recv())
            .await
            .expect("one-frame playback")
            .expect("sender open");
        for _ in 0..10 {
            if connection
                .player
                .as_ref()
                .is_some_and(JoinHandle::is_finished)
            {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert!(
            connection
                .player
                .as_ref()
                .is_some_and(JoinHandle::is_finished)
        );
        let replay = handle(
            &request("PLAY", "rtsp://host/a", &SESSION),
            &tx,
            &mut connection,
            &mounts,
            &options,
        );
        assert_eq!(status(&replay), 455);
    }

    #[test]
    fn sender_report_mapping_uses_the_delivery_clock_across_a_stall() {
        let base = RtpTimestamp::new(100);
        let initial = replay_clock_timestamp(base, Duration::ZERO, 0);
        let normal = replay_clock_timestamp(base, Duration::from_secs(5), 0);
        let delayed = replay_clock_timestamp(base, Duration::from_secs(15), 0);
        assert_eq!(initial.get(), 100);
        assert_eq!(normal.get(), 450_100);
        assert_eq!(delayed.get(), 1_350_100);
        assert_eq!(delayed.get().wrapping_sub(normal.get()), 900_000);
    }

    /// A clock 1,000 ppm fast reads a thousandth more, and reaches a reading
    /// a thousandth sooner; frames keep their timestamps and are due sooner.
    #[test]
    fn a_fast_clock_reads_more_and_arrives_sooner() {
        let second = Duration::from_secs(1);
        assert_eq!(fast_clock(second, 1_000), Duration::from_millis(1_001));
        assert_eq!(fast_clock(second, -1_000), Duration::from_millis(999));
        assert_eq!(slow_clock(Duration::from_millis(1_001), 1_000), second);
        assert_eq!(
            (fast_clock(second, 0), slow_clock(second, 0)),
            (second, second)
        );
        let fast = replay_clock_timestamp(RtpTimestamp::new(0), Duration::from_secs(10), 1_000);
        assert_eq!(fast.get(), 900_900);
        let start = Instant::now();
        let exact = frame_due(start, Ticks::new(900_900), Ticks::ZERO, 0).unwrap();
        let sooner = frame_due(start, Ticks::new(900_900), Ticks::ZERO, 1_000).unwrap();
        assert_eq!(exact.0, sooner.0);
        assert_eq!(sooner.1.duration_since(start), Duration::from_secs(10));
        assert_eq!(exact.1.duration_since(start), Duration::from_millis(10_010));
    }

    /// The two reports of a late pair are behind their clocks together by
    /// every sixteenth of a video frame's period in turn; the video's is
    /// behind the audio's by 0, 2, 4, or 6 ms, by 60 ms in one pair of
    /// five, and 20 ms ahead of it in one of fifteen; one pair in three has
    /// its wall time 50 ms behind. The session's reports are numbered from
    /// its start, where the first is made.
    #[test]
    fn late_reports_are_behind_their_clocks_together_and_the_videos_by_more() {
        let elapsed = Duration::from_secs(10);
        let wall = WallUs::new(1_000_000);
        let period = Duration::from_millis(32);
        let pair = |index| LatePair::new(elapsed, wall, index, period);
        // Sixteen pairs, one of each sixteenth.
        let mut behind: Vec<Duration> = (0..16)
            .map(|index| elapsed.checked_sub(pair(index).audio).expect("behind"))
            .collect();
        behind.sort_unstable();
        let sixteenths: Vec<Duration> = (0..16).map(|n| Duration::from_millis(2 * n)).collect();
        assert_eq!(behind, sixteenths);
        // How far the video's report is behind the audio's, in
        // milliseconds, over thirty pairs.
        let video_behind: Vec<i128> = (0..30)
            .map(|index| {
                let pair = pair(index);
                pair.audio.as_millis() as i128 - pair.video.as_millis() as i128
            })
            .collect();
        assert_eq!(
            video_behind,
            [
                0, 2, 4, 60, 0, 2, 4, 6, 60, 2, 4, 6, 0, 60, -20, //
                6, 0, 2, 60, 6, 0, 2, 4, 60, 0, 2, 4, 6, 60, -20
            ]
        );
        let walls: Vec<i64> = (0..6).map(|index| pair(index).wall.get()).collect();
        assert_eq!(
            walls,
            [1_000_000, 950_000, 1_000_000, 1_000_000, 950_000, 1_000_000]
        );
        // A clock that has not read a frame yet is not read before zero.
        let early = LatePair::new(Duration::from_millis(3), wall, 3, period);
        assert_eq!((early.video, early.audio), (Duration::ZERO, Duration::ZERO));

        let mut reports = Reports::default();
        let start = (Instant::now(), WallUs::new(1_000_000));
        let first = reports
            .due(Duration::from_secs(60), start, period)
            .expect("the first report is due at once");
        assert_eq!(
            (first.elapsed, first.wall, first.index, first.video_period),
            (Duration::ZERO, start.1, 0, period)
        );
        assert!(
            reports
                .due(Duration::from_secs(60), start, period)
                .is_none()
        );
        let second = reports
            .due(Duration::ZERO, start, period)
            .expect("the interval has passed");
        assert_eq!(second.index, 1);
        assert!(second.wall.get() > start.1.get());
    }

    /// An audio stream of 64 kHz with `stamps` and a sample clock
    /// `fast_ppm` fast.
    fn player(stamps: AudioStamps, fast_ppm: i32) -> AudioPlayer {
        AudioPlayer {
            sample_rate: 64_000,
            channels: InterleavedChannels { rtp: 2, rtcp: 3 },
            ssrc: Ssrc::new(7),
            wander: false,
            lose: None,
            fast_ppm,
            stamps,
            leave_out: None,
            left_out: 0,
            sent: 0,
        }
    }

    /// The sequence number and the timestamp of every packet `audio` has
    /// due in its first ten seconds. A frame is 16 ms: 10 s hold 625 starts
    /// and the frame at 10 s itself.
    fn packets_of(mut audio: AudioPlayer) -> Vec<(u16, u32)> {
        let bytes = audio.frames_due(Duration::from_secs(10)).unwrap();
        let mut packets = Vec::new();
        let mut at = 0;
        while at + 4 <= bytes.len() {
            let len = usize::from(u16::from_be_bytes([bytes[at + 2], bytes[at + 3]]));
            let packet = &bytes[at + 4..at + 4 + len];
            packets.push((
                u16::from_be_bytes([packet[2], packet[3]]),
                u32::from_be_bytes([packet[4], packet[5], packet[6], packet[7]]),
            ));
            at += 4 + len;
        }
        packets
    }

    /// Seven frames left out from the hundredth: sequence numbers run on.
    /// The camera's clock stamps the frames after them with their own
    /// instants, and the sample clock with the count of what was sent.
    #[test]
    fn sound_left_out_skips_no_sequence_number_and_stamps_follow_the_chosen_clock() {
        for (stamps, after) in [
            (AudioStamps::CameraClock, 107 * 1_024),
            (AudioStamps::SampleClock, 100 * 1_024),
        ] {
            let packets = packets_of(AudioPlayer {
                leave_out: Some(LeftOut { at: 100, frames: 7 }),
                ..player(stamps, 0)
            });
            assert_eq!(packets.len(), 626 - 7);
            assert_eq!(packets[99], (99, 99 * 1_024));
            assert_eq!(packets[100], (100, after), "{stamps:?}");
            assert_eq!(packets[101], (101, after + 1_024), "{stamps:?}");
        }
    }

    /// With the wander, a frame whose number leaves 5 by 31 is stamped two
    /// frames early, another whose number leaves 3 by 7 a frame and 384
    /// units early, backwards from the frame before it, and every other on
    /// its count; without it every frame is on its count.
    #[test]
    fn wandering_stamps_stand_early_of_the_count_and_step_backwards() {
        let wandering = packets_of(AudioPlayer {
            wander: true,
            ..player(AudioStamps::SampleClock, 0)
        });
        let steady = packets_of(player(AudioStamps::SampleClock, 0));
        for (frame, (wandered, on_count)) in wandering.iter().zip(&steady).enumerate() {
            let early = match (frame % 31, frame % 7) {
                (5, _) => 2_048,
                (_, 3) => 1_408,
                _ => 0,
            };
            assert_eq!(on_count.1, frame as u32 * 1_024, "frame {frame}");
            assert_eq!(wandered.1, on_count.1.wrapping_sub(early), "frame {frame}");
            assert_eq!(wandered.0, on_count.0, "frame {frame}");
        }
        assert!(wandering[3].1 < wandering[2].1, "a step backwards");
    }

    /// The audio's sample clock, 10,000 ppm fast, has 1 % more frames due;
    /// stamped by the sample clock a frame's stamp is its count, and stamped
    /// by the camera's clock it is the instant the frame starts. A sender
    /// report reads whichever clock stamps the frames.
    #[test]
    fn a_fast_sample_clock_makes_more_frames_and_stamps_follow_the_chosen_clock() {
        let stamps_of = |audio: AudioPlayer| -> Vec<u32> {
            packets_of(audio)
                .into_iter()
                .map(|packet| packet.1)
                .collect()
        };
        let exact = stamps_of(player(AudioStamps::SampleClock, 0));
        assert_eq!((exact.len(), exact[625]), (626, 640_000));
        let counted = stamps_of(player(AudioStamps::SampleClock, 10_000));
        assert_eq!((counted.len(), counted[631]), (632, 631 * 1_024));
        let clocked = stamps_of(player(AudioStamps::CameraClock, 10_000));
        assert_eq!(clocked.len(), 632);
        // Frame 631 starts 631 × 16 ms / 1.01 after PLAY: 9.99604 s.
        assert_eq!(clocked[631], 639_746);
        let report = |stamps| player(stamps, 10_000).report_timestamp(Duration::from_secs(10));
        assert_eq!(report(AudioStamps::SampleClock).get(), 646_400);
        assert_eq!(report(AudioStamps::CameraClock).get(), 640_000);
    }

    #[test]
    fn tick_durations_stop_at_the_microsecond_range() {
        let largest_convertible = u64::MAX / 100;
        assert!(Ticks::new(largest_convertible).to_duration().is_some());
        assert!(Ticks::new(largest_convertible + 1).to_duration().is_none());
        assert!(frame_due(Instant::now(), Ticks::new(u64::MAX), Ticks::new(1), 0).is_none());
    }

    #[test]
    fn repeat_offsets_stop_when_exhausted_or_stalled() {
        assert_eq!(
            next_loop_offset(Ticks::new(u64::MAX - 1), Ticks::new(1)),
            Some(Ticks::new(u64::MAX))
        );
        assert_eq!(next_loop_offset(Ticks::new(u64::MAX), Ticks::new(1)), None);
        assert_eq!(next_loop_offset(Ticks::new(7), Ticks::ZERO), None);
    }

    #[test]
    fn paced_packets_stay_inside_their_frame() {
        let start = Instant::now();
        let count = usize::MAX;
        let frame = Ticks::new(u64::from(u32::MAX));
        let last = packet_due(start, frame, count - 1, count).expect("paced deadline");
        let frame_end = start
            .checked_add(frame.to_duration().unwrap())
            .expect("frame end");
        assert!(last < frame_end);
        assert!(packet_due(start, Ticks::new(3_000), count, count).is_none());
    }

    #[tokio::test]
    async fn bind_rejects_an_empty_mount_set() {
        let addr = "127.0.0.1:0".parse().unwrap();
        assert!(matches!(
            Server::bind(addr, Mounts::new(), Options::default()).await,
            Err(ServerError::NoMounts)
        ));
    }

    #[tokio::test]
    async fn bind_accepts_the_exact_mtu_boundaries() {
        use crate::rtp::{MAX_MTU, MIN_MTU};

        let addr = "127.0.0.1:0".parse().unwrap();
        for mtu in [MIN_MTU, MAX_MTU] {
            let options = Options {
                mtu,
                ..Options::default()
            };
            Server::bind(addr, Mounts::from([(mount("a"), test_media(0))]), options)
                .await
                .unwrap();
        }
    }

    #[tokio::test]
    async fn bind_rejects_an_mtu_outside_the_boundaries() {
        use crate::rtp::{MAX_MTU, MIN_MTU};

        let addr = "127.0.0.1:0".parse().unwrap();
        for mtu in [MIN_MTU - 1, MAX_MTU + 1] {
            let options = Options {
                mtu,
                ..Options::default()
            };
            let result =
                Server::bind(addr, Mounts::from([(mount("a"), test_media(0))]), options).await;
            assert!(
                matches!(result, Err(ServerError::Mtu(RtpError::InvalidMtu { .. }))),
                "{mtu}"
            );
        }
    }

    #[tokio::test]
    async fn bind_rejects_a_zero_cap_and_zero_deadlines() {
        for options in [
            Options {
                max_connections: 0,
                ..Options::default()
            },
            Options {
                request_timeout: Duration::ZERO,
                ..Options::default()
            },
            Options {
                idle_timeout: Duration::ZERO,
                ..Options::default()
            },
        ] {
            let result = Server::bind(
                "127.0.0.1:0".parse().unwrap(),
                Mounts::from([(mount("a"), test_media(0))]),
                options.clone(),
            )
            .await;
            assert!(
                matches!(
                    result,
                    Err(ServerError::ConnectionCap(0) | ServerError::Deadline)
                ),
                "{options:?}"
            );
        }
    }

    #[tokio::test]
    async fn the_server_debug_output_names_its_mounts() {
        let server = Server::bind("127.0.0.1:0".parse().unwrap(), mounts(), Options::default())
            .await
            .unwrap();
        let shown = format!("{server:?}");
        assert!(shown.contains("[\"a\", \"b\"]"), "{shown}");
    }

    fn find_bytes(haystack: &[u8], needle: &[u8]) -> bool {
        find_offset(haystack, needle).is_some()
    }

    fn find_offset(haystack: &[u8], needle: &[u8]) -> Option<usize> {
        haystack
            .windows(needle.len())
            .position(|window| window == needle)
    }

    async fn setup_raw_session(stream: &mut TcpStream) -> String {
        stream
            .write_all(
                concat!(
                    "SETUP rtsp://host/a/trackID=0 RTSP/1.0\r\nCSeq: 1\r\n",
                    "Transport: RTP/AVP/TCP;unicast;interleaved=0-1\r\n\r\n"
                )
                .as_bytes(),
            )
            .await
            .unwrap();
        let mut setup = vec![0; 1024];
        let setup_len = tokio::time::timeout(Duration::from_secs(1), stream.read(&mut setup))
            .await
            .expect("SETUP response")
            .unwrap();
        let setup = std::str::from_utf8(&setup[..setup_len]).unwrap();
        setup
            .lines()
            .find_map(|line| line.strip_prefix("Session: "))
            .expect("Session header")
            .to_owned()
    }

    async fn fill_queue_until_socket_writer_blocks(tx: &mpsc::Sender<Outgoing>) {
        let media = vec![0x55; 64 * 1024];
        let mut writer_made_progress = false;
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                match tx.try_send(Outgoing::Media(media.clone())) {
                    Ok(()) => {}
                    Err(mpsc::error::TrySendError::Closed(_)) => {
                        panic!("socket writer closed while filling its queue");
                    }
                    Err(mpsc::error::TrySendError::Full(_)) => {
                        tokio::task::yield_now().await;
                        if tx.capacity() > 0 {
                            writer_made_progress = true;
                            continue;
                        }
                        if writer_made_progress
                            && tokio::time::timeout(Duration::from_millis(25), async {
                                while tx.capacity() == 0 {
                                    tokio::task::yield_now().await;
                                }
                            })
                            .await
                            .is_err()
                        {
                            break;
                        }
                    }
                }
            }
        })
        .await
        .expect("loopback socket writer reaches backpressure");
        assert!(writer_made_progress, "writer consumed queued media first");
        assert_eq!(tx.capacity(), 0, "media queue is full");
    }

    /// A connected loopback pair: the client and the server's read and write halves.
    async fn socket_pair() -> (TcpStream, OwnedReadHalf, OwnedWriteHalf) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (client, accepted) = tokio::join!(TcpStream::connect(address), listener.accept());
        let (server, _) = accepted.unwrap();
        server.set_nodelay(true).unwrap();
        let (reader, writer) = server.into_split();
        (client.unwrap(), reader, writer)
    }

    #[tokio::test]
    async fn teardown_enqueue_is_bounded_when_media_queue_and_socket_are_blocked() {
        let (mut client, mut reader, writer) = socket_pair().await;
        let (tx, rx) = mpsc::channel(OUTGOING_QUEUE);
        let (terminal_tx, terminal_rx) = mpsc::channel(1);
        let sender = tokio::spawn(write_queued(writer, rx, terminal_rx));

        fill_queue_until_socket_writer_blocks(&tx).await;
        let expected = rtsp::response(3, 200, "OK", &[("Session", "session-1")], b"");
        client
            .write_all(b"TEARDOWN rtsp://host/a RTSP/1.0\r\nCSeq: 3\r\nSession: session-1\r\n\r\n")
            .await
            .unwrap();
        let mut connection = connection();
        connection.channels = Some(InterleavedChannels { rtp: 0, rtcp: 1 });
        let options = Arc::new(Options::default());
        let configured_mounts = mounts();
        let terminal_observer = terminal_tx.clone();
        let request_tx = tx.clone();
        let request_task = tokio::spawn(async move {
            request_loop(
                &mut reader,
                &request_tx,
                &terminal_tx,
                &mut connection,
                &configured_mounts,
                &options,
            )
            .await
        });
        tokio::time::timeout(Duration::from_millis(100), async {
            while terminal_observer.capacity() != 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("terminal response bypasses the full media queue");
        assert!(
            !request_task.is_finished(),
            "terminal delivery cannot complete while the socket is blocked"
        );

        let mut received = Vec::new();
        tokio::time::timeout(Duration::from_secs(2), client.read_to_end(&mut received))
            .await
            .expect("unblocked socket reaches terminal response and EOF")
            .unwrap();
        assert_eq!(
            request_task.await.unwrap().unwrap(),
            RequestLoopEnd::Terminal
        );
        sender.await.unwrap();
        assert!(
            received.ends_with(&expected),
            "terminal response is last on the wire"
        );
    }

    #[tokio::test]
    async fn a_reply_to_a_peer_that_stops_reading_ends_at_the_deadline() {
        let (mut client, mut reader, _writer) = socket_pair().await;
        // Nothing drains the queue, as when the socket writer waits on a peer
        // that stopped reading.
        let (tx, _rx) = mpsc::channel(1);
        tx.try_send(Outgoing::Media(vec![0])).unwrap();
        let (terminal_tx, _terminal_rx) = mpsc::channel(1);
        client
            .write_all(b"OPTIONS rtsp://host/a RTSP/1.0\r\nCSeq: 1\r\n\r\n")
            .await
            .unwrap();
        let options = Arc::new(Options {
            request_timeout: Duration::from_millis(200),
            ..Options::default()
        });
        let mut connection = connection();
        let result = tokio::time::timeout(
            Duration::from_secs(2),
            request_loop(
                &mut reader,
                &tx,
                &terminal_tx,
                &mut connection,
                &mounts(),
                &options,
            ),
        )
        .await
        .expect("the blocked reply ends at the deadline");
        assert!(
            matches!(result, Err(ConnectionError::ReplyDeadline { .. })),
            "{result:?}"
        );
    }

    async fn serve_with(options: Options) -> (SocketAddr, JoinHandle<Result<(), ServerError>>) {
        let server = Server::bind(
            "127.0.0.1:0".parse().unwrap(),
            Mounts::from([(mount("a"), test_media(0))]),
            options,
        )
        .await
        .unwrap();
        let addr = server.local_addr().unwrap();
        (addr, tokio::spawn(server.run()))
    }

    /// Sends OPTIONS and waits for its response; `None` when the server closed.
    async fn options_round_trip(stream: &mut TcpStream, cseq: u32) -> Option<Vec<u8>> {
        stream
            .write_all(format!("OPTIONS rtsp://host/a RTSP/1.0\r\nCSeq: {cseq}\r\n\r\n").as_bytes())
            .await
            .ok()?;
        let mut reply = vec![0; 1024];
        let read = tokio::time::timeout(Duration::from_secs(2), stream.read(&mut reply))
            .await
            .expect("the server answers or closes before the deadline")
            .ok()?;
        (read > 0).then(|| reply[..read].to_vec())
    }

    #[tokio::test]
    async fn connections_beyond_the_cap_are_closed_at_accept() {
        let (addr, server) = serve_with(Options {
            max_connections: 2,
            ..Options::default()
        })
        .await;
        let mut first = TcpStream::connect(addr).await.unwrap();
        let mut second = TcpStream::connect(addr).await.unwrap();
        assert!(options_round_trip(&mut first, 1).await.is_some());
        assert!(options_round_trip(&mut second, 1).await.is_some());
        let mut excess = TcpStream::connect(addr).await.unwrap();
        let mut received = Vec::new();
        tokio::time::timeout(Duration::from_secs(2), excess.read_to_end(&mut received))
            .await
            .expect("the excess connection is closed before the deadline")
            .unwrap();
        assert!(received.is_empty(), "the excess connection is never served");

        drop(first);
        let reused = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let mut retry = TcpStream::connect(addr).await.unwrap();
                if options_round_trip(&mut retry, 2).await.is_some() {
                    return retry;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await;
        assert!(reused.is_ok(), "a released slot serves the next connection");
        server.abort();
    }

    #[tokio::test]
    async fn a_partial_request_is_closed_at_the_deadline() {
        let (addr, server) = serve_with(Options {
            request_timeout: Duration::from_millis(200),
            ..Options::default()
        })
        .await;
        let mut partial = TcpStream::connect(addr).await.unwrap();
        partial
            .write_all(b"OPTIONS rtsp://host/a RTSP/1.0\r\nCSeq: 1\r\n")
            .await
            .unwrap();
        let mut received = Vec::new();
        tokio::time::timeout(Duration::from_secs(3), partial.read_to_end(&mut received))
            .await
            .expect("a partial request is closed at the deadline")
            .unwrap();
        assert!(received.is_empty(), "a partial request gets no response");
        server.abort();
    }

    #[tokio::test]
    async fn trickled_bytes_do_not_extend_the_request_deadline() {
        let (addr, server) = serve_with(Options {
            request_timeout: Duration::from_millis(200),
            ..Options::default()
        })
        .await;
        let mut trickle = TcpStream::connect(addr).await.unwrap();
        let started = Instant::now();
        let closed = tokio::time::timeout(Duration::from_secs(3), async {
            for byte in b"OPTIONS rtsp://host/a RTSP/1.0\r\nCSeq: 2\r\n"
                .iter()
                .cycle()
            {
                if trickle.write_all(&[*byte]).await.is_err() {
                    return;
                }
                let mut probe = [0u8; 64];
                if let Ok(Ok(0) | Err(_)) =
                    tokio::time::timeout(Duration::from_millis(50), trickle.read(&mut probe)).await
                {
                    return;
                }
            }
        })
        .await;
        assert!(closed.is_ok(), "trickled bytes do not extend the deadline");
        assert!(started.elapsed() < Duration::from_secs(2));
        server.abort();
    }

    #[tokio::test]
    async fn interleaved_frames_before_play_do_not_extend_the_request_deadline() {
        let (addr, server) = serve_with(Options {
            request_timeout: Duration::from_millis(200),
            ..Options::default()
        })
        .await;
        let mut stream = TcpStream::connect(addr).await.unwrap();
        let started = Instant::now();
        let closed = tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                // A four-byte interleaved frame every 50 ms, and no request.
                if stream.write_all(b"$\x01\x00\x00").await.is_err() {
                    return;
                }
                let mut probe = [0u8; 64];
                if let Ok(Ok(0) | Err(_)) =
                    tokio::time::timeout(Duration::from_millis(50), stream.read(&mut probe)).await
                {
                    return;
                }
            }
        })
        .await;
        assert!(
            closed.is_ok(),
            "interleaved frames do not extend the deadline"
        );
        assert!(started.elapsed() < Duration::from_secs(2));
        server.abort();
    }

    #[tokio::test]
    async fn a_playing_connection_closes_after_the_idle_deadline() {
        let (addr, server) = serve_with(Options {
            idle_timeout: Duration::from_millis(300),
            ..Options::default()
        })
        .await;
        let mut stream = TcpStream::connect(addr).await.unwrap();
        let session = setup_raw_session(&mut stream).await;
        let (mut reader, mut writer) = stream.into_split();
        let received = tokio::spawn(async move {
            let mut received = Vec::new();
            tokio::time::timeout(Duration::from_secs(5), reader.read_to_end(&mut received))
                .await
                .map(|read| read.map(|_| received))
        });
        writer
            .write_all(
                format!("PLAY rtsp://host/a RTSP/1.0\r\nCSeq: 2\r\nSession: {session}\r\n\r\n")
                    .as_bytes(),
            )
            .await
            .unwrap();
        // Keepalives every 100 ms keep the session for twice the idle deadline.
        for cseq in 3..9 {
            tokio::time::sleep(Duration::from_millis(100)).await;
            let keepalive = format!(
                "GET_PARAMETER rtsp://host/a RTSP/1.0\r\nCSeq: {cseq}\r\n\
                 Session: {session}\r\n\r\n"
            );
            writer.write_all(keepalive.as_bytes()).await.unwrap();
        }
        let received = received
            .await
            .unwrap()
            .expect("the silent session closes within the deadline")
            .unwrap();
        for cseq in 2..9 {
            assert!(
                find_bytes(&received, format!("CSeq: {cseq}\r\n").as_bytes()),
                "response {cseq} arrives while the client keeps the session alive"
            );
        }
        server.abort();
    }

    #[tokio::test]
    async fn ordinary_teardown_response_arrives_before_eof() {
        let (addr, server) = serve_with(Options::default()).await;
        let mut stream = TcpStream::connect(addr).await.unwrap();
        let session = setup_raw_session(&mut stream).await;
        stream
            .write_all(
                format!("TEARDOWN rtsp://host/a RTSP/1.0\r\nCSeq: 2\r\nSession: {session}\r\n\r\n")
                    .as_bytes(),
            )
            .await
            .unwrap();
        let mut received = Vec::new();
        tokio::time::timeout(Duration::from_secs(2), stream.read_to_end(&mut received))
            .await
            .expect("connection drains and closes")
            .unwrap();
        assert!(find_bytes(&received, b"CSeq: 2\r\n"));
        server.abort();
    }

    #[tokio::test]
    async fn teardown_response_drains_after_queued_playback() {
        let (addr, server) = serve_with(Options::default()).await;
        let mut stream = TcpStream::connect(addr).await.unwrap();
        let session = setup_raw_session(&mut stream).await;
        let pipelined = format!(
            "PLAY rtsp://host/a RTSP/1.0\r\nCSeq: 2\r\nSession: {session}\r\n\r\n\
             TEARDOWN rtsp://host/a RTSP/1.0\r\nCSeq: 3\r\nSession: {session}\r\n\r\n"
        );
        stream.write_all(pipelined.as_bytes()).await.unwrap();
        let mut received = Vec::new();
        tokio::time::timeout(Duration::from_secs(2), stream.read_to_end(&mut received))
            .await
            .expect("connection drains and closes")
            .unwrap();
        assert!(find_bytes(&received, b"CSeq: 2\r\n"), "PLAY response");
        assert!(find_bytes(&received, b"CSeq: 3\r\n"), "TEARDOWN response");
        let play = find_offset(&received, b"CSeq: 2\r\n").unwrap();
        let teardown = find_offset(&received, b"CSeq: 3\r\n").unwrap();
        assert!(play < teardown, "PLAY response precedes terminal response");
        if let Some(media) = find_offset(&received, b"$") {
            assert!(play < media, "PLAY response precedes media");
        }
        server.abort();
    }

    /// The writes one pass of `test_media` (one sync frame: three parameter
    /// sets and a slice, four RTP packets) produces under `options`, with the
    /// instant each arrived.
    async fn one_pass(options: Options) -> Vec<(Instant, Vec<u8>)> {
        let (tx, mut rx) = mpsc::channel(64);
        let options = Arc::new(Options {
            repeat: false,
            ..options
        });
        let player = tokio::spawn(play(
            test_media(0),
            options,
            tx,
            Packetizer::new(Ssrc::new(1), 100),
            RtpTimestamp::new(0),
            InterleavedChannels { rtp: 0, rtcp: 1 },
            None,
        ));
        let mut writes = Vec::new();
        while let Some(frame) = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("the player finishes one pass")
        {
            let Outgoing::Media(bytes) = frame else {
                panic!("the player sends media only");
            };
            writes.push((Instant::now(), bytes));
        }
        player.await.unwrap();
        writes
    }

    /// The RTP sequence numbers in interleaved `bytes` on channel 0.
    fn rtp_sequences(bytes: &[u8]) -> Vec<u16> {
        let mut sequences = Vec::new();
        let mut rest = bytes;
        while let [b'$', channel, high, low, tail @ ..] = rest {
            let len = usize::from(u16::from_be_bytes([*high, *low]));
            let (packet, after) = tail.split_at(len);
            if *channel == 0 {
                sequences.push(u16::from_be_bytes([packet[2], packet[3]]));
            }
            rest = after;
        }
        sequences
    }

    #[tokio::test]
    async fn per_packet_delivery_writes_each_packet_alone() {
        let writes = one_pass(Options::default()).await;
        assert_eq!(writes.len(), 4, "one write per RTP packet");
        for (_, bytes) in &writes {
            assert_eq!(rtp_sequences(bytes).len(), 1);
        }
    }

    #[tokio::test]
    async fn burst_delivery_writes_the_access_unit_at_once() {
        let writes = one_pass(Options {
            delivery: Delivery::Burst,
            ..Options::default()
        })
        .await;
        assert_eq!(writes.len(), 1, "one write per access unit");
        assert_eq!(rtp_sequences(&writes[0].1), [100, 101, 102, 103]);
    }

    #[tokio::test]
    async fn paced_delivery_spreads_packets_over_the_frame_interval() {
        let started = Instant::now();
        let writes = one_pass(Options {
            delivery: Delivery::Paced,
            ..Options::default()
        })
        .await;
        assert_eq!(writes.len(), 4);
        // Four packets over a 3,000-tick (33 ms) frame: the last is due three
        // quarters of the way through it, 25 ms after the frame.
        let last = writes[3].0.duration_since(started);
        assert!(last >= Duration::from_millis(25), "{last:?}");
    }

    /// A stall holds the stream once, when its moment has come and not before.
    #[tokio::test]
    async fn a_stall_delays_delivery_from_its_moment_on() {
        let started = Instant::now();
        let writes = one_pass(Options {
            stall_after: Some(Duration::ZERO),
            stall_for: Duration::from_millis(200),
            ..Options::default()
        })
        .await;
        let first = writes[0].0.duration_since(started);
        assert!(first >= Duration::from_millis(200), "{first:?}");

        let started = Instant::now();
        let writes = one_pass(Options {
            stall_after: Some(Duration::from_secs(3_600)),
            stall_for: Duration::from_secs(1),
            ..Options::default()
        })
        .await;
        let last = writes.last().unwrap().0.duration_since(started);
        assert!(last < Duration::from_secs(1), "{last:?}");
    }

    #[tokio::test]
    async fn every_nth_packet_is_dropped_with_its_sequence_number() {
        let writes = one_pass(Options {
            drop_every: NonZeroU64::new(3),
            delivery: Delivery::Burst,
            ..Options::default()
        })
        .await;
        assert_eq!(rtp_sequences(&writes[0].1), [100, 101, 103]);
    }
}
