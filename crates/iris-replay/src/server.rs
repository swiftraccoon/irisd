//! The TCP listener, per-connection RTSP state machine, and the paced sender.

use std::collections::HashMap;
use std::error::Error;
use std::fmt;
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

use crate::media::{Media, Ticks};
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
        ssrc: Ssrc::new((random >> 32) as u32),
        seq: (random & 0xffff) as u16,
        base_rtp: RtpTimestamp::new((next_random(counter) & 0xffff_ffff) as u32),
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
        "DESCRIBE" => describe(request, connection, mounts),
        "SETUP" => setup(request, connection, mounts, session),
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
    session: (&str, &str),
) -> Vec<u8> {
    let cseq = request.cseq;
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

fn describe(request: &Request, connection: &mut Connection, mounts: &Mounts) -> Vec<u8> {
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
    let body = sdp::describe(media.parameter_sets(), frame_rate);
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
    ));
    connection.player = Some(player);
    info!(session = %connection.session_id, "playing");
    let rtp_info = format!(
        "url={}/trackID=0;seq={};rtptime={}",
        request.url.trim_end_matches('/'),
        connection.seq,
        connection.base_rtp.get()
    );
    rtsp::response(
        cseq,
        200,
        "OK",
        &[("Range", "npt=0.000-"), ("RTP-Info", &rtp_info), session],
        b"",
    )
}

/// The absolute replay timestamp and pacing deadline for one frame.
fn frame_due(start: Instant, decode_time: Ticks, loop_offset: Ticks) -> Option<(Ticks, Instant)> {
    let ticks = decode_time.checked_add(loop_offset)?;
    let due = start.checked_add(ticks.to_duration()?)?;
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

/// RTP clock value sampled at the same monotonic instant as an RTCP wall time.
fn replay_clock_timestamp(
    base_rtp: RtpTimestamp,
    start: Instant,
    sampled_at: Instant,
) -> RtpTimestamp {
    let ticks = sampled_at.duration_since(start).as_nanos() * u128::from(TIMESCALE) / 1_000_000_000;
    base_rtp.after(Ticks::new(ticks as u64))
}

/// Sends the media paced by its decode times until the connection goes away.
async fn play(
    media: Arc<Media>,
    options: Arc<Options>,
    tx: mpsc::Sender<Outgoing>,
    mut packetizer: Packetizer,
    base_rtp: RtpTimestamp,
    channels: InterleavedChannels,
) {
    let start = Instant::now();
    let start_wall = WallUs::now();
    let mut loop_offset = Ticks::ZERO;
    let mut last_report: Option<Instant> = None;
    let mut stalled = false;
    let mut packet_index = 0u64;
    loop {
        for frame in media.frames() {
            let Some((ticks, due)) = frame_due(start, frame.decode_time, loop_offset) else {
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
            let timestamp = base_rtp.after(ticks);
            let mut out = Vec::new();
            if last_report.is_none_or(|t| t.elapsed() >= SENDER_REPORT_INTERVAL) {
                // Sample both sides of the SR mapping on the delivery clock. The
                // initial report precedes the first RTP packet, so its mapping
                // uses the PLAY-start correlation; later reports use the current
                // correlation. A fault-injection stall therefore does not pair
                // delayed wall time with an earlier frame timestamp.
                let (sampled_at, wall) =
                    last_report.map_or((start, start_wall), |_| (Instant::now(), WallUs::now()));
                let report_timestamp = replay_clock_timestamp(base_rtp, start, sampled_at);
                let report = packetizer.sender_report(wall, report_timestamp);
                if let Err(error) = rtp::interleave(&mut out, channels.rtcp, &report) {
                    warn!(%error, "sender report framing failed");
                    return;
                }
                last_report = Some(sampled_at);
            }
            let Some(sample) = media.sample(frame) else {
                warn!("a frame lies outside its recording");
                return;
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
                        return;
                    }
                }
            }
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
        let start = Instant::now();
        let base = RtpTimestamp::new(100);
        let initial = replay_clock_timestamp(base, start, start);
        let normal = replay_clock_timestamp(base, start, start + Duration::from_secs(5));
        let delayed = replay_clock_timestamp(base, start, start + Duration::from_secs(15));
        assert_eq!(initial.get(), 100);
        assert_eq!(normal.get(), 450_100);
        assert_eq!(delayed.get(), 1_350_100);
        assert_eq!(delayed.get().wrapping_sub(normal.get()), 900_000);
    }

    #[test]
    fn tick_durations_stop_at_the_microsecond_range() {
        let largest_convertible = u64::MAX / 100;
        assert!(Ticks::new(largest_convertible).to_duration().is_some());
        assert!(Ticks::new(largest_convertible + 1).to_duration().is_none());
        assert!(frame_due(Instant::now(), Ticks::new(u64::MAX), Ticks::new(1)).is_none());
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
