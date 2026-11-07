//! The TCP listener, per-connection RTSP state machine, and the paced sender.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, ensure};
use iris_core::time::WallUs;
use iris_h265::LengthPrefixedNals;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Semaphore, mpsc, oneshot};
use tokio::task::JoinHandle;
use tracing::{debug, info, warn};

use crate::media::Media;
use crate::rtp::{Packetizer, interleave, validate_mtu};
use crate::rtsp::{Parsed, Request, mount_name, parse, response, valid_mount_name};
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

/// Replay behaviour shared by all sessions.
#[derive(Clone, Debug)]
pub struct Options {
    /// Largest RTP packet, header included.
    pub mtu: usize,
    /// Replay the file again when it ends, with continuous timestamps.
    pub repeat: bool,
    /// Drop every n-th RTP packet.
    pub drop_every: Option<u64>,
    /// Pause sending once, this long after PLAY, for `stall_for`; the paused
    /// frames are then sent in a burst with their original timestamps, as a
    /// network stall would deliver them.
    pub stall_after: Option<Duration>,
    /// Length of the stall.
    pub stall_for: Duration,
    /// Write each access unit with one system call instead of one per packet.
    /// Cameras send one TCP segment per RTP packet, so the per-packet default
    /// reproduces the receiver's wake-up pattern; bursts are cheaper to send.
    pub burst: bool,
    /// Spread a frame's packets over the frame interval instead of sending
    /// them back to back, as a camera on a slow link delivers them.
    pub pace: bool,
    /// Connections served at once; the listener closes further connections
    /// as it accepts them.
    pub max_connections: usize,
    /// Before `PLAY`, the time a connection has to complete each request.
    /// Trickled bytes do not extend it.
    pub request_timeout: Duration,
    /// After `PLAY`, the longest silence between client requests or
    /// interleaved frames. It stays above the recorder's keepalive, which
    /// retina sends every 30 s for the default 60 s session timeout.
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
            burst: false,
            pace: false,
            max_connections: DEFAULT_MAX_CONNECTIONS,
            request_timeout: Duration::from_secs(10),
            idle_timeout: Duration::from_secs(120),
        }
    }
}

/// Mount names to recordings.
pub type Mounts = HashMap<String, Arc<Media>>;

/// A bound listener.
pub struct Server {
    listener: TcpListener,
    mounts: Arc<Mounts>,
    options: Arc<Options>,
    counter: Arc<AtomicU64>,
    connections: Arc<Semaphore>,
}

impl Server {
    /// Binds `addr`. The connection cap must be positive and both deadlines in
    /// `options` between a millisecond and a day.
    pub async fn bind(addr: SocketAddr, mounts: Mounts, options: Options) -> anyhow::Result<Self> {
        validate_mtu(options.mtu)?;
        ensure!(
            (1..=Semaphore::MAX_PERMITS).contains(&options.max_connections),
            "the replay connection cap must be positive"
        );
        let deadline_range = Duration::from_millis(1)..=MAX_DEADLINE;
        ensure!(
            deadline_range.contains(&options.request_timeout)
                && deadline_range.contains(&options.idle_timeout),
            "replay request and idle deadlines must be positive and at most one day"
        );
        ensure!(!mounts.is_empty(), "at least one replay mount is required");
        for name in mounts.keys() {
            ensure!(valid_mount_name(name), "invalid replay mount name {name:?}");
        }
        let listener = TcpListener::bind(addr)
            .await
            .with_context(|| format!("binding {addr}"))?;
        Ok(Self {
            listener,
            mounts: Arc::new(mounts),
            connections: Arc::new(Semaphore::new(options.max_connections)),
            options: Arc::new(options),
            counter: Arc::new(AtomicU64::new(seed())),
        })
    }

    /// The bound address.
    pub fn local_addr(&self) -> anyhow::Result<SocketAddr> {
        self.listener.local_addr().context("local address")
    }

    /// Accepts connections until the listener fails. A connection beyond
    /// [`Options::max_connections`] is closed when it is accepted.
    pub async fn run(self) -> anyhow::Result<()> {
        loop {
            let (stream, peer) = self.listener.accept().await.context("accept")?;
            let Ok(permit) = Arc::clone(&self.connections).try_acquire_owned() else {
                warn!(%peer, limit = self.options.max_connections, "connection limit reached; closing");
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
                    Err(error) => warn!(%peer, %error, "connection ended"),
                }
            });
        }
    }
}

/// A pseudo-random seed from the clock, for SSRCs, sequence numbers, and session ids.
fn seed() -> u64 {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
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
    ssrc: u32,
    seq: u16,
    base_rtp: u32,
    player: Option<JoinHandle<()>>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct InterleavedChannels {
    rtp: u8,
    rtcp: u8,
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
) -> anyhow::Result<()> {
    stream.set_nodelay(true).context("nodelay")?;
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
        ssrc: (random >> 32) as u32,
        seq: (random & 0xffff) as u16,
        base_rtp: (next_random(counter) & 0xffff_ffff) as u32,
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
                        && writer.write_all(&bytes).await.is_err()
                    {
                        return;
                    }
                }
                if writer.write_all(&terminal.bytes).await.is_ok() {
                    let _ = terminal.delivered.send(());
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
                if writer.write_all(&bytes).await.is_err() {
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
) -> anyhow::Result<RequestLoopEnd> {
    let mut buf = Vec::with_capacity(4096);
    let mut deadline = next_request_deadline(connection, options);
    loop {
        match parse(&buf) {
            Parsed::Incomplete => {
                let Ok(read) = tokio::time::timeout_at(deadline, reader.read_buf(&mut buf)).await
                else {
                    let (state, limit) = if connection.player.is_some() {
                        ("after PLAY", options.idle_timeout)
                    } else {
                        ("before PLAY", options.request_timeout)
                    };
                    anyhow::bail!(
                        "no complete request within {} s {state}",
                        limit.as_secs_f64()
                    );
                };
                if read.context("read")? == 0 {
                    return Ok(RequestLoopEnd::InputClosed);
                }
            }
            Parsed::Interleaved(len) => {
                buf.drain(..len);
                deadline = next_request_deadline(connection, options);
            }
            Parsed::Invalid => anyhow::bail!("not an RTSP request"),
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
                if tx.send(Outgoing::Control(reply)).await.is_err() {
                    return Ok(RequestLoopEnd::InputClosed);
                }
                deadline = next_request_deadline(connection, options);
            }
        }
    }
}

/// When the next complete request or interleaved frame must have arrived: the
/// per-request deadline before `PLAY`, the idle deadline once playing.
fn next_request_deadline(connection: &Connection, options: &Options) -> tokio::time::Instant {
    let limit = if connection.player.is_some() {
        options.idle_timeout
    } else {
        options.request_timeout
    };
    tokio::time::Instant::now() + limit
}

fn handle(
    request: &Request,
    tx: &mpsc::Sender<Outgoing>,
    connection: &mut Connection,
    mounts: &Mounts,
    options: &Arc<Options>,
) -> Vec<u8> {
    let cseq = request.cseq;
    let session = ("Session", connection.session_id.clone());
    match request.method.as_str() {
        "OPTIONS" => response(
            cseq,
            200,
            "OK",
            &[(
                "Public",
                "OPTIONS, DESCRIBE, SETUP, PLAY, TEARDOWN, GET_PARAMETER, SET_PARAMETER".into(),
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
            response(cseq, 200, "OK", &[session], b"")
        }
        "GET_PARAMETER" | "SET_PARAMETER" => {
            if let Some(reply) = session_error(request, connection) {
                return reply;
            }
            response(cseq, 200, "OK", &[session], b"")
        }
        _ => response(cseq, 405, "Method Not Allowed", &[], b""),
    }
}

fn setup(
    request: &Request,
    connection: &mut Connection,
    mounts: &Mounts,
    session: (&str, String),
) -> Vec<u8> {
    let cseq = request.cseq;
    if connection.channels.is_some() {
        return response(cseq, 455, "Method Not Valid in This State", &[], b"");
    }
    let Some(name) = mount_name(&request.url) else {
        return response(cseq, 404, "Not Found", &[], b"");
    };
    let Some(media) = mounts.get(name) else {
        return response(cseq, 404, "Not Found", &[], b"");
    };
    if connection
        .mount
        .as_deref()
        .is_some_and(|selected| selected != name)
    {
        return response(cseq, 455, "Method Not Valid in This State", &[], b"");
    }
    let Some(channels) = request
        .header("transport")
        .and_then(parse_interleaved_transport)
    else {
        return response(cseq, 461, "Unsupported Transport", &[], b"");
    };
    connection.mount = Some(name.to_owned());
    connection.media = Some(Arc::clone(media));
    connection.channels = Some(channels);
    response(
        cseq,
        200,
        "OK",
        &[
            (
                "Transport",
                format!(
                    "RTP/AVP/TCP;unicast;interleaved={}-{}",
                    channels.rtp, channels.rtcp
                ),
            ),
            session,
        ],
        b"",
    )
}

fn parse_interleaved_transport(transport: &str) -> Option<InterleavedChannels> {
    let mut parts = transport.split(';').map(str::trim);
    if !parts.next()?.eq_ignore_ascii_case("RTP/AVP/TCP") {
        return None;
    }
    let mut selected = None;
    for part in parts {
        let Some((name, value)) = part.split_once('=') else {
            continue;
        };
        if !name.trim().eq_ignore_ascii_case("interleaved") {
            continue;
        }
        if selected.is_some() {
            return None;
        }
        let (media_text, control_text) = value.trim().split_once('-')?;
        let channels = InterleavedChannels {
            rtp: media_text.parse().ok()?,
            rtcp: control_text.parse().ok()?,
        };
        if channels.rtp == channels.rtcp {
            return None;
        }
        selected = Some(channels);
    }
    Some(selected.unwrap_or(InterleavedChannels { rtp: 0, rtcp: 1 }))
}

fn session_error(request: &Request, connection: &Connection) -> Option<Vec<u8>> {
    if connection.channels.is_none() {
        return Some(response(
            request.cseq,
            455,
            "Method Not Valid in This State",
            &[],
            b"",
        ));
    }
    (!session_matches(request, connection))
        .then(|| response(request.cseq, 454, "Session Not Found", &[], b""))
}

fn session_matches(request: &Request, connection: &Connection) -> bool {
    let session = request
        .header("session")
        .and_then(|value| value.split(';').next())
        .map(str::trim);
    session == Some(connection.session_id.as_str())
}

fn describe(request: &Request, connection: &mut Connection, mounts: &Mounts) -> Vec<u8> {
    let cseq = request.cseq;
    let Some(name) = mount_name(&request.url) else {
        return response(cseq, 404, "Not Found", &[], b"");
    };
    let Some(media) = mounts.get(name) else {
        return response(cseq, 404, "Not Found", &[], b"");
    };
    if connection.channels.is_some()
        && connection
            .mount
            .as_deref()
            .is_some_and(|selected| selected != name)
    {
        return response(cseq, 455, "Method Not Valid in This State", &[], b"");
    }
    connection.mount = Some(name.to_owned());
    connection.media = Some(Arc::clone(media));
    let frame_rate = media
        .frames
        .first()
        .map_or(30.0, |f| 90_000.0 / f64::from(f.duration.max(1)));
    let body = sdp::describe(&media.parameter_sets, frame_rate);
    let base = format!(
        "{}/",
        request
            .url
            .split('?')
            .next()
            .unwrap_or(&request.url)
            .trim_end_matches('/')
    );
    response(
        cseq,
        200,
        "OK",
        &[
            ("Content-Type", "application/sdp".into()),
            ("Content-Base", base),
        ],
        body.as_bytes(),
    )
}

fn start_play(
    request: &Request,
    tx: &mpsc::Sender<Outgoing>,
    connection: &mut Connection,
    mounts: &Mounts,
    options: &Arc<Options>,
    session: (&str, String),
) -> Vec<u8> {
    let cseq = request.cseq;
    let Some(name) = mount_name(&request.url) else {
        return response(cseq, 404, "Not Found", &[], b"");
    };
    if !mounts.contains_key(name) {
        return response(cseq, 404, "Not Found", &[], b"");
    }
    if connection.mount.as_deref() != Some(name) {
        return response(cseq, 455, "Method Not Valid in This State", &[], b"");
    }
    let Some(media) = connection.media.clone() else {
        return response(cseq, 455, "Method Not Valid in This State", &[], b"");
    };
    let Some(channels) = connection.channels else {
        return response(cseq, 455, "Method Not Valid in This State", &[], b"");
    };
    if connection.player.is_some() {
        return response(cseq, 455, "Method Not Valid in This State", &[], b"");
    }
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
        connection.base_rtp
    );
    response(
        cseq,
        200,
        "OK",
        &[
            ("Range", "npt=0.000-".into()),
            ("RTP-Info", rtp_info),
            session,
        ],
        b"",
    )
}

/// Ticks of the 90 kHz clock as a duration, when the microsecond conversion fits.
fn ticks_to_duration(ticks: u64) -> Option<Duration> {
    ticks
        .checked_mul(100)
        .map(|micros| Duration::from_micros(micros / 9))
}

/// The absolute replay timestamp and pacing deadline for one frame.
fn frame_due(start: Instant, decode_time: u64, loop_offset: u64) -> Option<(u64, Instant)> {
    let ticks = decode_time.checked_add(loop_offset)?;
    let due = start.checked_add(ticks_to_duration(ticks)?)?;
    Some((ticks, due))
}

/// The deadline of one packet distributed across its frame interval.
fn packet_due(
    frame_due: Instant,
    frame_ticks: u32,
    packet_index: usize,
    packet_count: usize,
) -> Option<Instant> {
    if packet_index >= packet_count {
        return None;
    }
    let offset_ticks =
        u128::from(frame_ticks).checked_mul(packet_index as u128)? / packet_count as u128;
    let offset_ticks = u64::try_from(offset_ticks).ok()?;
    frame_due.checked_add(ticks_to_duration(offset_ticks)?)
}

/// Advances the continuous RTP timeline for a repeated file.
fn next_loop_offset(loop_offset: u64, total_ticks: u64) -> Option<u64> {
    let next = loop_offset.checked_add(total_ticks)?;
    (next > loop_offset).then_some(next)
}

/// RTP clock value sampled at the same monotonic instant as an RTCP wall time.
fn replay_clock_timestamp(base_rtp: u32, start: Instant, sampled_at: Instant) -> u32 {
    let ticks = sampled_at.duration_since(start).as_nanos() * 90_000 / 1_000_000_000;
    base_rtp.wrapping_add(ticks as u32)
}

/// Sends the media paced by its decode times until the connection goes away.
async fn play(
    media: Arc<Media>,
    options: Arc<Options>,
    tx: mpsc::Sender<Outgoing>,
    mut packetizer: Packetizer,
    base_rtp: u32,
    channels: InterleavedChannels,
) {
    let start = Instant::now();
    let start_ntp = WallUs::now().to_ntp64();
    let mut loop_offset = 0u64;
    let mut last_report: Option<Instant> = None;
    let mut stalled = false;
    let mut packet_index = 0u64;
    loop {
        for frame in &media.frames {
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
            let timestamp = base_rtp.wrapping_add(ticks as u32);
            let mut out = Vec::new();
            if last_report.is_none_or(|t| t.elapsed() >= SENDER_REPORT_INTERVAL) {
                // Sample both sides of the SR mapping on the delivery clock. The
                // initial report precedes the first RTP packet, so its mapping
                // uses the PLAY-start correlation; later reports use the current
                // correlation. A fault-injection stall therefore does not pair
                // delayed wall time with an earlier frame timestamp.
                let (sampled_at, ntp) = last_report.map_or((start, start_ntp), |_| {
                    (Instant::now(), WallUs::now().to_ntp64())
                });
                let report_timestamp = replay_clock_timestamp(base_rtp, start, sampled_at);
                let report = packetizer.sender_report(ntp, report_timestamp);
                if let Err(error) = interleave(&mut out, channels.rtcp, &report) {
                    warn!(%error, "sender report framing failed");
                    return;
                }
                last_report = Some(sampled_at);
            }
            let sample = media.sample(frame);
            let mut nals: Vec<&[u8]> = Vec::with_capacity(4);
            if frame.is_sync {
                nals.extend(media.parameter_sets.iter().map(Vec::as_slice));
            }
            for nal in LengthPrefixedNals::new(sample) {
                match nal {
                    Ok(nal) => nals.push(nal),
                    Err(_) => break,
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
                    .is_some_and(|n| packet_index.is_multiple_of(n))
                {
                    continue;
                }
                if options.pace && i > 0 {
                    let Some(packet_due) = packet_due(due, frame.duration, i, count) else {
                        warn!("paced packet deadline exceeds the pacing clock");
                        return;
                    };
                    tokio::time::sleep_until(packet_due.into()).await;
                }
                if let Err(error) = interleave(&mut out, channels.rtp, &packet) {
                    warn!(%error, "RTP framing failed");
                    return;
                }
                if !options.burst
                    && tx
                        .send(Outgoing::Media(std::mem::take(&mut out)))
                        .await
                        .is_err()
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
        let Some(next_offset) = next_loop_offset(loop_offset, media.total_ticks) else {
            warn!("repeat timeline is exhausted or does not advance");
            return;
        };
        loop_offset = next_offset;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use tokio::io::AsyncReadExt;

    use crate::media::Frame;

    fn test_media(fill: u8) -> Arc<Media> {
        Arc::new(Media {
            data: Bytes::from(vec![0, 0, 0, 3, 0x26, 0x01, fill]),
            parameter_sets: vec![vec![0x40, 0x01], vec![0x42, 0x01], vec![0x44, 0x01]],
            frames: vec![Frame {
                offset: 0,
                len: 7,
                duration: 3_000,
                decode_time: 0,
                is_sync: true,
            }],
            total_ticks: 3_000,
        })
    }

    fn mounts() -> Mounts {
        Mounts::from([
            ("a".to_owned(), test_media(0xaa)),
            ("b".to_owned(), test_media(0xbb)),
        ])
    }

    fn connection() -> Connection {
        Connection {
            session_id: "session-1".to_owned(),
            mount: None,
            media: None,
            channels: None,
            ssrc: 7,
            seq: 11,
            base_rtp: 13,
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

    #[test]
    fn transport_channels_are_parsed_or_rejected_before_setup() {
        assert_eq!(
            parse_interleaved_transport("RTP/AVP/TCP;unicast;interleaved=4-5"),
            Some(InterleavedChannels { rtp: 4, rtcp: 5 })
        );
        assert_eq!(
            parse_interleaved_transport("rtp/avp/tcp;unicast"),
            Some(InterleavedChannels { rtp: 0, rtcp: 1 })
        );
        for invalid in [
            "RTP/AVP;unicast;interleaved=0-1",
            "RTP/AVP/TCP;interleaved=4",
            "RTP/AVP/TCP;interleaved=4-4",
            "RTP/AVP/TCP;interleaved=x-5",
            "RTP/AVP/TCP;interleaved=256-1",
            "RTP/AVP/TCP;interleaved=0-1;interleaved=2-3",
        ] {
            assert_eq!(parse_interleaved_transport(invalid), None, "{invalid}");
        }
    }

    #[tokio::test]
    async fn setup_binds_mount_transport_and_session_before_play() {
        let mounts = mounts();
        let options = Arc::new(Options::default());
        let (tx, _rx) = mpsc::channel(16);
        let mut connection = connection();

        let options_reply = handle(
            &request("OPTIONS", "rtsp://host/a", &[]),
            &tx,
            &mut connection,
            &mounts,
            &options,
        );
        assert!(!String::from_utf8(options_reply).unwrap().contains("PAUSE"));

        let described = handle(
            &request("DESCRIBE", "rtsp://host/a", &[]),
            &tx,
            &mut connection,
            &mounts,
            &options,
        );
        assert_eq!(status(&described), 200);
        assert_eq!(connection.mount.as_deref(), Some("a"));

        let missing = handle(
            &request(
                "SETUP",
                "rtsp://host/missing/trackID=0",
                &[("transport", "RTP/AVP/TCP;interleaved=0-1")],
            ),
            &tx,
            &mut connection,
            &mounts,
            &options,
        );
        assert_eq!(status(&missing), 404);
        assert!(connection.channels.is_none());

        let crossed = handle(
            &request(
                "SETUP",
                "rtsp://host/b/trackID=0",
                &[("transport", "RTP/AVP/TCP;interleaved=0-1")],
            ),
            &tx,
            &mut connection,
            &mounts,
            &options,
        );
        assert_eq!(status(&crossed), 455);
        assert!(connection.channels.is_none());

        let rejected = handle(
            &request(
                "SETUP",
                "rtsp://host/a/trackID=0",
                &[("transport", "RTP/AVP;unicast")],
            ),
            &tx,
            &mut connection,
            &mounts,
            &options,
        );
        assert_eq!(status(&rejected), 461);
        let premature = handle(
            &request("PLAY", "rtsp://host/a", &[]),
            &tx,
            &mut connection,
            &mounts,
            &options,
        );
        assert_eq!(status(&premature), 455);
    }

    #[tokio::test]
    async fn session_channels_and_play_state_are_enforced() {
        let mounts = mounts();
        let options = Arc::new(Options::default());
        let (tx, mut rx) = mpsc::channel(16);
        let mut connection = connection();

        let setup = handle(
            &request(
                "SETUP",
                "rtsp://host/a/trackID=0",
                &[("transport", "RTP/AVP/TCP;unicast;interleaved=4-5")],
            ),
            &tx,
            &mut connection,
            &mounts,
            &options,
        );
        assert_eq!(status(&setup), 200);
        let setup_text = String::from_utf8(setup).unwrap();
        assert!(setup_text.contains("interleaved=4-5\r\n"));
        assert!(setup_text.contains("Session: session-1\r\n"));
        assert!(!setup_text.contains("timeout="));

        let wrong_session = handle(
            &request("PLAY", "rtsp://host/a", &[("session", "wrong")]),
            &tx,
            &mut connection,
            &mounts,
            &options,
        );
        assert_eq!(status(&wrong_session), 454);
        let wrong_mount = handle(
            &request("PLAY", "rtsp://host/b", &[("session", "session-1")]),
            &tx,
            &mut connection,
            &mounts,
            &options,
        );
        assert_eq!(status(&wrong_mount), 455);

        let playing = handle(
            &request("PLAY", "rtsp://host/a", &[("session", "session-1")]),
            &tx,
            &mut connection,
            &mounts,
            &options,
        );
        assert_eq!(status(&playing), 200);
        let first_frames = tokio::time::timeout(Duration::from_secs(1), rx.recv())
            .await
            .expect("media arrives")
            .expect("sender open");
        let Outgoing::Media(first_frames) = first_frames else {
            panic!("player emitted a control response");
        };
        assert_eq!(first_frames[1], 5, "RTCP uses selected channel");
        let rtcp_len = usize::from(u16::from_be_bytes([first_frames[2], first_frames[3]]));
        assert_eq!(
            first_frames[4 + rtcp_len + 1],
            4,
            "RTP uses selected channel"
        );

        let repeated = handle(
            &request("PLAY", "rtsp://host/a", &[("session", "session-1")]),
            &tx,
            &mut connection,
            &mounts,
            &options,
        );
        assert_eq!(status(&repeated), 455);
        let pause = handle(
            &request("PAUSE", "rtsp://host/a", &[("session", "session-1")]),
            &tx,
            &mut connection,
            &mounts,
            &options,
        );
        assert_eq!(status(&pause), 405);

        let teardown = handle(
            &request("TEARDOWN", "rtsp://host/a", &[("session", "session-1")]),
            &tx,
            &mut connection,
            &mounts,
            &options,
        );
        assert_eq!(status(&teardown), 200);
    }

    #[tokio::test]
    async fn play_after_natural_completion_returns_a_state_error() {
        let mounts = mounts();
        let options = Arc::new(Options {
            repeat: false,
            ..Options::default()
        });
        let (tx, mut rx) = mpsc::channel(16);
        let mut connection = connection();
        let setup = handle(
            &request(
                "SETUP",
                "rtsp://host/a/trackID=0",
                &[("transport", "RTP/AVP/TCP;interleaved=0-1")],
            ),
            &tx,
            &mut connection,
            &mounts,
            &options,
        );
        assert_eq!(status(&setup), 200);
        let first = handle(
            &request("PLAY", "rtsp://host/a", &[("session", "session-1")]),
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
            &request("PLAY", "rtsp://host/a", &[("session", "session-1")]),
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
        let initial = replay_clock_timestamp(100, start, start);
        let normal = replay_clock_timestamp(100, start, start + Duration::from_secs(5));
        let delayed = replay_clock_timestamp(100, start, start + Duration::from_secs(15));
        assert_eq!(initial, 100);
        assert_eq!(normal, 450_100);
        assert_eq!(delayed, 1_350_100);
        assert_eq!(delayed.wrapping_sub(normal), 900_000);
    }

    #[test]
    fn replay_deadlines_fail_closed_at_timeline_boundaries() {
        let start = Instant::now();
        let largest_convertible = u64::MAX / 100;
        assert!(ticks_to_duration(largest_convertible).is_some());
        assert!(ticks_to_duration(largest_convertible + 1).is_none());

        assert!(frame_due(start, u64::MAX, 1).is_none());
        assert_eq!(next_loop_offset(u64::MAX - 1, 1), Some(u64::MAX));
        assert_eq!(next_loop_offset(u64::MAX, 1), None);
        assert_eq!(next_loop_offset(7, 0), None);

        let count = usize::MAX;
        let last = packet_due(start, u32::MAX, count - 1, count).expect("paced deadline");
        let frame_end = start
            .checked_add(ticks_to_duration(u64::from(u32::MAX)).unwrap())
            .expect("frame end");
        assert!(last < frame_end);
        assert!(packet_due(start, 3_000, count, count).is_none());
    }

    #[tokio::test]
    async fn bind_rejects_empty_and_unreachable_mount_sets() {
        let addr = "127.0.0.1:0".parse().unwrap();
        let Err(empty) = Server::bind(addr, Mounts::new(), Options::default()).await else {
            panic!("empty mounts must fail");
        };
        assert!(empty.to_string().contains("at least one replay mount"));

        let mut invalid = Mounts::new();
        invalid.insert("a/b".to_owned(), test_media(0));
        let Err(error) = Server::bind(addr, invalid, Options::default()).await else {
            panic!("unreachable mount must fail");
        };
        assert!(error.to_string().contains("invalid replay mount name"));
    }

    #[tokio::test]
    async fn bind_validates_exact_mtu_boundaries() {
        use crate::rtp::{MAX_MTU, MIN_MTU};

        let addr = "127.0.0.1:0".parse().unwrap();
        for mtu in [MIN_MTU, MAX_MTU] {
            Server::bind(
                addr,
                Mounts::from([("a".to_owned(), test_media(0))]),
                Options {
                    mtu,
                    ..Options::default()
                },
            )
            .await
            .unwrap();
        }
        for mtu in [MIN_MTU - 1, MAX_MTU + 1] {
            let error = Server::bind(
                addr,
                Mounts::from([("a".to_owned(), test_media(0))]),
                Options {
                    mtu,
                    ..Options::default()
                },
            )
            .await
            .err()
            .expect("out-of-range MTU must fail");
            assert!(
                error.to_string().contains("outside the supported"),
                "{error}"
            );
        }
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
            .write_all(b"SETUP rtsp://host/a/trackID=0 RTSP/1.0\r\nCSeq: 1\r\nTransport: RTP/AVP/TCP;unicast;interleaved=0-1\r\n\r\n")
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

    #[tokio::test]
    async fn teardown_enqueue_is_bounded_when_media_queue_and_socket_are_blocked() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (client, accepted) = tokio::join!(TcpStream::connect(address), listener.accept());
        let mut client = client.unwrap();
        let (server, _) = accepted.unwrap();
        server.set_nodelay(true).unwrap();
        let (mut reader, writer) = server.into_split();
        let (tx, rx) = mpsc::channel(OUTGOING_QUEUE);
        let (terminal_tx, terminal_rx) = mpsc::channel(1);
        let sender = tokio::spawn(write_queued(writer, rx, terminal_rx));

        fill_queue_until_socket_writer_blocks(&tx).await;
        let expected = response(3, 200, "OK", &[("Session", "session-1".into())], b"");
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

    async fn serve_with(options: Options) -> (SocketAddr, JoinHandle<anyhow::Result<()>>) {
        let server = Server::bind(
            "127.0.0.1:0".parse().unwrap(),
            Mounts::from([("a".to_owned(), test_media(0))]),
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
    async fn requests_before_play_must_complete_within_the_deadline() {
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
            writer
                .write_all(
                    format!(
                        "GET_PARAMETER rtsp://host/a RTSP/1.0\r\nCSeq: {cseq}\r\nSession: {session}\r\n\r\n"
                    )
                    .as_bytes(),
                )
                .await
                .unwrap();
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
            assert!(
                Server::bind(
                    "127.0.0.1:0".parse().unwrap(),
                    Mounts::from([("a".to_owned(), test_media(0))]),
                    options,
                )
                .await
                .is_err()
            );
        }
    }

    #[tokio::test]
    async fn ordinary_teardown_response_arrives_before_eof() {
        let server = Server::bind(
            "127.0.0.1:0".parse().unwrap(),
            Mounts::from([("a".to_owned(), test_media(0))]),
            Options::default(),
        )
        .await
        .unwrap();
        let addr = server.local_addr().unwrap();
        let server_task = tokio::spawn(server.run());
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
        server_task.abort();
    }

    #[tokio::test]
    async fn teardown_response_drains_after_queued_playback() {
        let server = Server::bind(
            "127.0.0.1:0".parse().unwrap(),
            Mounts::from([("a".to_owned(), test_media(0))]),
            Options::default(),
        )
        .await
        .unwrap();
        let addr = server.local_addr().unwrap();
        let server_task = tokio::spawn(server.run());
        let mut stream = TcpStream::connect(addr).await.unwrap();
        let session = setup_raw_session(&mut stream).await;
        let pipelined = format!(
            "PLAY rtsp://host/a RTSP/1.0\r\nCSeq: 2\r\nSession: {session}\r\n\r\nTEARDOWN rtsp://host/a RTSP/1.0\r\nCSeq: 3\r\nSession: {session}\r\n\r\n"
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
        server_task.abort();
    }
}
