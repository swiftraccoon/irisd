//! The TCP listener, per-connection RTSP state machine, and the paced sender.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use anyhow::Context;
use iris_core::time::WallUs;
use iris_h265::LengthPrefixedNals;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::tcp::OwnedReadHalf;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tracing::{debug, info, warn};

use crate::media::Media;
use crate::rtp::{Packetizer, interleave};
use crate::rtsp::{Parsed, Request, mount_name, parse, response};
use crate::sdp;

/// Interval between RTCP sender reports.
const SENDER_REPORT_INTERVAL: Duration = Duration::from_secs(5);
/// Outgoing frames buffered per connection before the sender waits.
const OUTGOING_QUEUE: usize = 256;

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
}

impl Server {
    /// Binds `addr`.
    pub async fn bind(addr: SocketAddr, mounts: Mounts, options: Options) -> anyhow::Result<Self> {
        let listener = TcpListener::bind(addr)
            .await
            .with_context(|| format!("binding {addr}"))?;
        Ok(Self {
            listener,
            mounts: Arc::new(mounts),
            options: Arc::new(options),
            counter: Arc::new(AtomicU64::new(seed())),
        })
    }

    /// The bound address.
    pub fn local_addr(&self) -> anyhow::Result<SocketAddr> {
        self.listener.local_addr().context("local address")
    }

    /// Accepts connections until the listener fails.
    pub async fn run(self) -> anyhow::Result<()> {
        loop {
            let (stream, peer) = self.listener.accept().await.context("accept")?;
            let mounts = Arc::clone(&self.mounts);
            let options = Arc::clone(&self.options);
            let counter = Arc::clone(&self.counter);
            tokio::spawn(async move {
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
    media: Option<Arc<Media>>,
    ssrc: u32,
    seq: u16,
    base_rtp: u32,
    player: Option<JoinHandle<()>>,
}

async fn serve(
    stream: TcpStream,
    mounts: &Mounts,
    options: &Arc<Options>,
    counter: &AtomicU64,
) -> anyhow::Result<()> {
    stream.set_nodelay(true).context("nodelay")?;
    let (mut reader, mut writer) = stream.into_split();
    let (tx, mut rx) = mpsc::channel::<Vec<u8>>(OUTGOING_QUEUE);
    let sender = tokio::spawn(async move {
        while let Some(frame) = rx.recv().await {
            if writer.write_all(&frame).await.is_err() {
                break;
            }
        }
    });
    let random = next_random(counter);
    let mut connection = Connection {
        session_id: format!("{:016x}", next_random(counter)),
        media: None,
        ssrc: (random >> 32) as u32,
        seq: (random & 0xffff) as u16,
        base_rtp: (next_random(counter) & 0xffff_ffff) as u32,
        player: None,
    };
    let result = request_loop(&mut reader, &tx, &mut connection, mounts, options).await;
    if let Some(player) = connection.player.take() {
        player.abort();
    }
    drop(tx);
    sender.abort();
    result
}

async fn request_loop(
    reader: &mut OwnedReadHalf,
    tx: &mpsc::Sender<Vec<u8>>,
    connection: &mut Connection,
    mounts: &Mounts,
    options: &Arc<Options>,
) -> anyhow::Result<()> {
    let mut buf = Vec::with_capacity(4096);
    loop {
        match parse(&buf) {
            Parsed::Incomplete => {
                let n = reader.read_buf(&mut buf).await.context("read")?;
                if n == 0 {
                    return Ok(());
                }
            }
            Parsed::Interleaved(len) => {
                buf.drain(..len);
            }
            Parsed::Invalid => anyhow::bail!("not an RTSP request"),
            Parsed::Request(request, consumed) => {
                buf.drain(..consumed);
                let reply = handle(&request, tx, connection, mounts, options);
                let teardown = request.method == "TEARDOWN";
                if tx.send(reply).await.is_err() {
                    return Ok(());
                }
                if teardown {
                    return Ok(());
                }
            }
        }
    }
}

fn handle(
    request: &Request,
    tx: &mpsc::Sender<Vec<u8>>,
    connection: &mut Connection,
    mounts: &Mounts,
    options: &Arc<Options>,
) -> Vec<u8> {
    let cseq = request.cseq;
    let session = ("Session", format!("{};timeout=60", connection.session_id));
    match request.method.as_str() {
        "OPTIONS" => response(
            cseq,
            200,
            "OK",
            &[(
                "Public",
                "OPTIONS, DESCRIBE, SETUP, PLAY, PAUSE, TEARDOWN, GET_PARAMETER, SET_PARAMETER"
                    .into(),
            )],
            b"",
        ),
        "DESCRIBE" => describe(request, connection, mounts),
        "SETUP" => {
            if connection.media.is_none()
                && let Some(media) = mount_name(&request.url).and_then(|m| mounts.get(m))
            {
                connection.media = Some(Arc::clone(media));
            }
            if connection.media.is_none() {
                return response(cseq, 404, "Not Found", &[], b"");
            }
            let transport = request.header("transport").unwrap_or("");
            if !transport.contains("RTP/AVP/TCP") {
                return response(cseq, 461, "Unsupported Transport", &[], b"");
            }
            let channels = transport
                .split(';')
                .find_map(|p| p.strip_prefix("interleaved="))
                .unwrap_or("0-1");
            response(
                cseq,
                200,
                "OK",
                &[
                    (
                        "Transport",
                        format!("RTP/AVP/TCP;unicast;interleaved={channels}"),
                    ),
                    session,
                ],
                b"",
            )
        }
        "PLAY" => start_play(request, tx, connection, options, session),
        "PAUSE" | "TEARDOWN" => {
            if let Some(player) = connection.player.take() {
                player.abort();
            }
            response(cseq, 200, "OK", &[session], b"")
        }
        "GET_PARAMETER" | "SET_PARAMETER" => response(cseq, 200, "OK", &[session], b""),
        _ => response(cseq, 405, "Method Not Allowed", &[], b""),
    }
}

fn describe(request: &Request, connection: &mut Connection, mounts: &Mounts) -> Vec<u8> {
    let cseq = request.cseq;
    let Some(media) = mount_name(&request.url).and_then(|m| mounts.get(m)) else {
        return response(cseq, 404, "Not Found", &[], b"");
    };
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
    tx: &mpsc::Sender<Vec<u8>>,
    connection: &mut Connection,
    options: &Arc<Options>,
    session: (&str, String),
) -> Vec<u8> {
    let cseq = request.cseq;
    let Some(media) = connection.media.clone() else {
        return response(cseq, 455, "Method Not Valid in This State", &[], b"");
    };
    if connection.player.is_none() {
        let player = tokio::spawn(play(
            media,
            Arc::clone(options),
            tx.clone(),
            Packetizer::new(connection.ssrc, connection.seq),
            connection.base_rtp,
        ));
        connection.player = Some(player);
        info!(session = %connection.session_id, "playing");
    }
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

/// Ticks of the 90 kHz clock as a duration.
fn ticks_to_duration(ticks: u64) -> Duration {
    Duration::from_micros(ticks * 100 / 9)
}

/// Sends the media paced by its decode times until the connection goes away.
async fn play(
    media: Arc<Media>,
    options: Arc<Options>,
    tx: mpsc::Sender<Vec<u8>>,
    mut packetizer: Packetizer,
    base_rtp: u32,
) {
    let start = Instant::now();
    let mut loop_offset = 0u64;
    let mut last_report: Option<Instant> = None;
    let mut stalled = false;
    let mut packet_index = 0u64;
    loop {
        for frame in &media.frames {
            let ticks = frame.decode_time + loop_offset;
            let due = start + ticks_to_duration(ticks);
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
                let report = packetizer.sender_report(WallUs::now().to_ntp64(), timestamp);
                interleave(&mut out, 1, &report);
                last_report = Some(Instant::now());
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
            let packets = packetizer.access_unit(&nals, timestamp, options.mtu);
            let count = u32::try_from(packets.len()).unwrap_or(u32::MAX).max(1);
            let span = ticks_to_duration(u64::from(frame.duration));
            for (i, packet) in packets.into_iter().enumerate() {
                packet_index += 1;
                if options
                    .drop_every
                    .is_some_and(|n| packet_index.is_multiple_of(n))
                {
                    continue;
                }
                if options.pace && i > 0 {
                    let offset = span * u32::try_from(i).unwrap_or(u32::MAX) / count;
                    tokio::time::sleep_until((due + offset).into()).await;
                }
                interleave(&mut out, 0, &packet);
                if !options.burst && tx.send(std::mem::take(&mut out)).await.is_err() {
                    return;
                }
            }
            if !out.is_empty() && tx.send(out).await.is_err() {
                return;
            }
        }
        if !options.repeat {
            info!("file ended");
            return;
        }
        loop_offset += media.total_ticks;
    }
}
