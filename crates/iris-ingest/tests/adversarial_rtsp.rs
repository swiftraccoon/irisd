//! Bounded loopback regressions for ingest terminal paths, network limits, and
//! hostile cameras.

use std::io::{self, Read as _, Write as _};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use iris_core::id::{StreamKey, StreamKind};
use iris_core::time::{MediaTicks, WallUs};
use iris_ingest::{
    FragmentSink, IngestOptions, ReadTicker, SinkError, StreamSource, StreamStats, run_stream,
};
use iris_mp4::fragment::Fragment;
use iris_replay::media::Media;
use iris_replay::rtp::{self, Packetizer, RtpTimestamp, Ssrc};
use iris_replay::rtsp::{self, MountName, Parsed, Request};
use iris_replay::sdp;
use iris_replay::server::{Mounts, Options as ReplayOptions, Server as ReplayServer};
use iris_replay::synth::{self, Plan, SliceKind};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

/// Retina's production limit on one RTSP control message, head and body
/// together; the crate keeps the constant private, so it is mirrored here.
const CONTROL_MESSAGE_LIMIT: usize = 1024 * 1024;
/// How much of an oversized access unit a peer may have sent before the
/// depacketizer's 64 MiB bound ends the session: the bound plus 2 MiB of
/// packets in flight.
const OVERSIZED_AU_SEND_LIMIT: usize = 66 * 1024 * 1024;

#[derive(Default)]
struct Collect {
    fragments: Mutex<Vec<Fragment>>,
}

impl Collect {
    fn fragment_count(&self) -> usize {
        self.fragments.lock().expect("collector mutex").len()
    }

    fn sample_count(&self) -> usize {
        self.fragments
            .lock()
            .expect("collector mutex")
            .iter()
            .map(|fragment| fragment.samples.len())
            .sum()
    }
}

impl FragmentSink for Collect {
    fn send(&self, _key: &StreamKey, fragment: Fragment) -> Result<(), SinkError> {
        self.fragments
            .lock()
            .expect("collector mutex")
            .push(fragment);
        Ok(())
    }
}

#[derive(Clone, Copy, Debug)]
enum FirstConnection {
    Stall,
    Eof,
    OversizedPlayingHeader,
    OversizedAccessUnit,
    OversizedDescribeBody,
}

#[derive(Default)]
struct PeerState {
    connections: AtomicUsize,
    fault_wire_bytes: AtomicUsize,
    normal_frames: AtomicUsize,
}

async fn spawn_fault_peer(first: FirstConnection) -> (SocketAddr, Arc<PeerState>, JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind fault peer");
    let address = listener.local_addr().expect("fault peer address");
    let state = Arc::new(PeerState::default());
    let task_state = Arc::clone(&state);
    let task = tokio::spawn(async move {
        loop {
            let Ok((socket, _peer)) = listener.accept().await else {
                return;
            };
            let connection = task_state.connections.fetch_add(1, Ordering::Relaxed);
            let action = (connection == 0).then_some(first);
            let _ = serve_connection(socket, action, &task_state).await;
        }
    });
    (address, state, task)
}

async fn serve_connection(
    mut socket: TcpStream,
    action: Option<FirstConnection>,
    state: &PeerState,
) -> io::Result<()> {
    socket.set_nodelay(true)?;
    let mut input = Vec::with_capacity(4096);
    let mut played = false;
    loop {
        let Some(request) = next_request(&mut socket, &mut input).await? else {
            return Ok(());
        };
        if matches!(action, Some(FirstConnection::OversizedDescribeBody))
            && request.method == "DESCRIBE"
        {
            let head = format!(
                "RTSP/1.0 200 OK\r\nCSeq: {}\r\nContent-Type: application/sdp\r\n\
                 Content-Length: {}\r\n\r\n",
                request.cseq,
                CONTROL_MESSAGE_LIMIT + 1
            );
            socket.write_all(head.as_bytes()).await?;
            state.fault_wire_bytes.store(head.len(), Ordering::Relaxed);
            continue;
        }

        let response = response_for(&request);
        socket.write_all(&response).await?;
        if request.method == "TEARDOWN" {
            return Ok(());
        }
        if request.method != "PLAY" || played {
            continue;
        }
        played = true;
        match action {
            Some(FirstConnection::Stall) => {
                send_frames(&mut socket, 8, None).await?;
            }
            Some(FirstConnection::Eof) => {
                send_frames(&mut socket, 8, None).await?;
                socket.shutdown().await?;
                return Ok(());
            }
            Some(FirstConnection::OversizedPlayingHeader) => {
                send_frames(&mut socket, 8, None).await?;
                let sent = send_oversized_server_header(&mut socket).await?;
                state.fault_wire_bytes.store(sent, Ordering::Relaxed);
            }
            Some(FirstConnection::OversizedAccessUnit) => {
                let sent = send_oversized_access_unit(&mut socket).await?;
                state.fault_wire_bytes.store(sent, Ordering::Relaxed);
            }
            Some(FirstConnection::OversizedDescribeBody) => {}
            None => {
                send_frames(&mut socket, 9, Some(8)).await?;
                state.normal_frames.store(9, Ordering::Relaxed);
            }
        }
    }
}

async fn next_request(socket: &mut TcpStream, input: &mut Vec<u8>) -> io::Result<Option<Request>> {
    loop {
        match rtsp::parse(input) {
            Parsed::Request(request, consumed) => {
                input.drain(..consumed);
                return Ok(Some(request));
            }
            Parsed::Interleaved(consumed) => {
                input.drain(..consumed);
            }
            Parsed::Incomplete => {
                if socket.read_buf(input).await? == 0 {
                    return Ok(None);
                }
            }
            Parsed::Invalid => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "invalid RTSP request from ingest",
                ));
            }
        }
    }
}

fn response_for(request: &Request) -> Vec<u8> {
    let session = ("Session", "regression;timeout=60");
    match request.method.as_str() {
        "DESCRIBE" => {
            let sets = [
                synth::TEST_VPS.to_vec(),
                synth::TEST_SPS.to_vec(),
                synth::TEST_PPS.to_vec(),
            ];
            let body = sdp::describe(&sets, 30.0);
            rtsp::response(
                request.cseq,
                200,
                "OK",
                &[
                    ("Content-Type", "application/sdp"),
                    (
                        "Content-Base",
                        &format!("{}/", request.url.trim_end_matches('/')),
                    ),
                ],
                body.as_bytes(),
            )
        }
        "SETUP" => rtsp::response(
            request.cseq,
            200,
            "OK",
            &[
                session,
                (
                    "Transport",
                    "RTP/AVP/TCP;unicast;interleaved=0-1;ssrc=12345678",
                ),
            ],
            b"",
        ),
        "PLAY" => rtsp::response(
            request.cseq,
            200,
            "OK",
            &[
                session,
                (
                    "RTP-Info",
                    &format!("url={}/trackID=0;seq=1;rtptime=0", request.url),
                ),
            ],
            b"",
        ),
        _ => rtsp::response(request.cseq, 200, "OK", &[session], b""),
    }
}

async fn send_frames(
    socket: &mut TcpStream,
    count: usize,
    second_sync: Option<usize>,
) -> io::Result<()> {
    let mut packetizer = Packetizer::new(Ssrc::new(0x1234_5678), 1);
    let mut wire = Vec::new();
    let report = packetizer.sender_report(WallUs::now(), RtpTimestamp::new(0));
    rtp::interleave(&mut wire, 1, &report).expect("sender report fits interleaving");
    socket.write_all(&wire).await?;
    for index in 0..count {
        let is_sync = index == 0 || second_sync == Some(index);
        let kind = if is_sync {
            SliceKind::Idr
        } else {
            SliceKind::Trail
        };
        let nal = synth::slice(index as u64, kind, 200).expect("valid synthetic slice");
        let nals = [nal.as_slice()];
        let packets = packetizer
            .access_unit(&nals, RtpTimestamp::new((index as u32) * 3_000), 1_400)
            .expect("valid packetization");
        wire.clear();
        for packet in packets {
            rtp::interleave(&mut wire, 0, &packet).expect("RTP packet fits interleaving");
        }
        socket.write_all(&wire).await?;
    }
    Ok(())
}

async fn send_oversized_server_header(socket: &mut TcpStream) -> io::Result<usize> {
    let prefix = b"SET_PARAMETER rtsp://127.0.0.1/cam RTSP/1.0\r\nCSeq: 99\r\nX-Fill: ";
    socket.write_all(prefix).await?;
    let fill = vec![b'x'; 16 * 1024];
    let mut sent = prefix.len();
    while sent < CONTROL_MESSAGE_LIMIT {
        let length = (CONTROL_MESSAGE_LIMIT - sent).min(fill.len());
        socket.write_all(&fill[..length]).await?;
        sent += length;
    }
    Ok(sent)
}

async fn send_oversized_access_unit(socket: &mut TcpStream) -> io::Result<usize> {
    let mut packetizer = Packetizer::new(Ssrc::new(0x1234_5678), 1);
    let mut nal = vec![0x55; 60_000];
    nal[0] = 0x26;
    nal[1] = 0x01;
    let mut wire_bytes = 0usize;
    let mut packets_sent = 0usize;
    'send: while wire_bytes < OVERSIZED_AU_SEND_LIMIT {
        if packets_sent.is_multiple_of(128) {
            let mut wire = Vec::new();
            let report = packetizer.sender_report(WallUs::now(), RtpTimestamp::new(0));
            rtp::interleave(&mut wire, 1, &report).expect("sender report fits interleaving");
            if wire_bytes + wire.len() > OVERSIZED_AU_SEND_LIMIT {
                break 'send;
            }
            if socket.write_all(&wire).await.is_err() {
                break;
            }
            wire_bytes += wire.len();
        }
        let nals = [nal.as_slice()];
        let packet = packetizer
            .access_unit(&nals, RtpTimestamp::new(0), 65_000)
            .expect("valid bounded packetization")
            .next()
            .expect("one packet for one bounded NAL");
        let mut packet = packet;
        packet[1] &= 0x7f;
        let mut wire = Vec::with_capacity(packet.len() + 4);
        rtp::interleave(&mut wire, 0, &packet).expect("RTP packet fits interleaving");
        if wire_bytes + wire.len() > OVERSIZED_AU_SEND_LIMIT {
            break;
        }
        if socket.write_all(&wire).await.is_err() {
            break;
        }
        wire_bytes += wire.len();
        packets_sent += 1;
    }
    Ok(wire_bytes)
}

fn source(address: SocketAddr) -> StreamSource {
    StreamSource {
        key: StreamKey {
            camera: "synthetic".parse().expect("camera id"),
            kind: StreamKind::Main,
        },
        url: format!("rtsp://{address}/cam").parse().expect("RTSP URL"),
        username: String::new(),
        password: String::new(),
    }
}

fn options(read_interval: Option<Duration>, inactivity_timeout: Duration) -> IngestOptions {
    IngestOptions {
        read_interval,
        ticker: read_interval.map(ReadTicker::spawn),
        inactivity_timeout,
        ..IngestOptions::default()
    }
}

async fn wait_until(timeout: Duration, mut predicate: impl FnMut() -> bool) {
    tokio::time::timeout(timeout, async {
        while !predicate() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("bounded loopback predicate");
}

fn start_ingest(
    address: SocketAddr,
    options: IngestOptions,
) -> (
    Arc<Collect>,
    Arc<StreamStats>,
    CancellationToken,
    JoinHandle<()>,
) {
    start_ingest_of(source(address), options)
}

fn start_ingest_of(
    source: StreamSource,
    options: IngestOptions,
) -> (
    Arc<Collect>,
    Arc<StreamStats>,
    CancellationToken,
    JoinHandle<()>,
) {
    let sink = Arc::new(Collect::default());
    let stats = Arc::new(StreamStats::new());
    let shutdown = CancellationToken::new();
    let task = tokio::spawn(run_stream(
        source,
        options,
        Arc::clone(&sink) as Arc<dyn FragmentSink>,
        Arc::clone(&stats),
        shutdown.clone(),
    ));
    (sink, stats, shutdown, task)
}

async fn stop_ingest(shutdown: CancellationToken, task: JoinHandle<()>) {
    shutdown.cancel();
    tokio::time::timeout(Duration::from_secs(2), task)
        .await
        .expect("ingest stops within the deadline")
        .expect("ingest task");
}

#[tokio::test]
async fn inactivity_and_eof_flush_pending_frames_once_in_both_read_modes() {
    tokio::time::timeout(Duration::from_secs(15), async {
        for action in [FirstConnection::Stall, FirstConnection::Eof] {
            for read_interval in [None, Some(Duration::from_millis(20))] {
                let (address, _peer_state, peer) = spawn_fault_peer(action).await;
                let (sink, stats, shutdown, task) =
                    start_ingest(address, options(read_interval, Duration::from_millis(150)));
                wait_until(Duration::from_secs(3), || stats.reconnects() == 1).await;
                stop_ingest(shutdown, task).await;
                peer.abort();

                assert_eq!(stats.frames(), 8, "{action:?}, {read_interval:?}");
                assert_eq!(sink.sample_count(), 8, "{action:?}, {read_interval:?}");
                assert_eq!(sink.fragment_count(), 1, "{action:?}, {read_interval:?}");
                assert_eq!(stats.fragments(), 1, "{action:?}, {read_interval:?}");
            }
        }
    })
    .await
    .expect("terminal-path regression stayed bounded");
}

#[tokio::test]
async fn wall_placement_ignores_pause_and_catch_up_in_both_read_modes() {
    tokio::time::timeout(Duration::from_secs(15), async {
        for read_interval in [None, Some(Duration::from_millis(20))] {
            let media = Arc::new(
                Media::from_bytes(
                    synth::recording(Plan {
                        gops: 12,
                        frames_per_gop: 10,
                        idr_len: 2_000,
                        trail_len: 200,
                        ..Plan::default()
                    })
                    .expect("valid synthetic recording"),
                )
                .expect("synthetic media"),
            );
            let mounts = Mounts::from([(MountName::try_new("cam").expect("mount name"), media)]);
            let server = ReplayServer::bind(
                "127.0.0.1:0".parse().expect("loopback address"),
                mounts,
                ReplayOptions {
                    stall_after: Some(Duration::from_millis(450)),
                    stall_for: Duration::from_secs(1),
                    ..ReplayOptions::default()
                },
            )
            .await
            .expect("bind replay server");
            let address = server.local_addr().expect("replay address");
            let peer = tokio::spawn(server.run());
            let (sink, stats, shutdown, task) =
                start_ingest(address, options(read_interval, Duration::from_secs(3)));
            wait_until(Duration::from_secs(6), || sink.fragment_count() >= 8).await;
            stop_ingest(shutdown, task).await;
            peer.abort();

            assert_eq!(stats.reconnects(), 0, "{read_interval:?}");
            let fragments = sink.fragments.lock().expect("collector mutex");
            for pair in fragments[..8].windows(2) {
                let media_us = i64::try_from(pair[0].duration()).expect("short fragment") * 100 / 9;
                let wall_us = pair[1].wall.micros_since(pair[0].wall);
                assert!(
                    (wall_us - media_us).abs() <= 5_000,
                    "{read_interval:?}: media delta {media_us}, wall delta {wall_us}"
                );
            }
        }
    })
    .await
    .expect("wall-placement regression stayed bounded");
}

#[tokio::test]
async fn oversized_access_unit_reconnects_and_records_a_normal_session() {
    tokio::time::timeout(Duration::from_secs(20), async {
        for read_interval in [None, Some(Duration::from_millis(20))] {
            let (address, peer_state, peer) =
                spawn_fault_peer(FirstConnection::OversizedAccessUnit).await;
            let (sink, stats, shutdown, task) =
                start_ingest(address, options(read_interval, Duration::from_secs(30)));
            wait_until(Duration::from_secs(8), || {
                stats.reconnects() == 1
                    && peer_state.normal_frames.load(Ordering::Relaxed) == 9
                    && sink.fragment_count() >= 1
            })
            .await;
            stop_ingest(shutdown, task).await;
            peer.abort();

            let sent = peer_state.fault_wire_bytes.load(Ordering::Relaxed);
            assert!(sent >= 64 * 1024 * 1024, "only {sent} bytes sent");
            assert!(
                sent <= OVERSIZED_AU_SEND_LIMIT,
                "{sent} bytes sent past the depacketizer bound"
            );
            assert!(
                stats.sender_reports() >= 1,
                "the normal session delivered no sender report"
            );
            assert_eq!(stats.reconnects(), 1);
            assert_eq!(stats.frames(), 9);
            assert_eq!(sink.sample_count(), 9);
        }
    })
    .await
    .expect("oversized-AU regression stayed bounded");
}

#[tokio::test]
async fn oversized_control_messages_reconnect_and_preserve_pending_frames() {
    tokio::time::timeout(Duration::from_secs(20), async {
        for action in [
            FirstConnection::OversizedDescribeBody,
            FirstConnection::OversizedPlayingHeader,
        ] {
            for read_interval in [None, Some(Duration::from_millis(20))] {
                let expected_frames = match action {
                    FirstConnection::OversizedDescribeBody => 9,
                    FirstConnection::OversizedPlayingHeader => 17,
                    _ => unreachable!(),
                };
                // The normal session's ninth frame is its next sync boundary: receiving it
                // closes eight samples and leaves that sync pending for the cancellation flush.
                let expected_closed_samples = expected_frames as usize - 1;
                let (address, peer_state, peer) = spawn_fault_peer(action).await;
                let (sink, stats, shutdown, task) =
                    start_ingest(address, options(read_interval, Duration::from_secs(10)));
                wait_until(Duration::from_secs(5), || {
                    stats.reconnects() == 1
                        && peer_state.normal_frames.load(Ordering::Relaxed) == 9
                        && stats.frames() >= expected_frames
                        && sink.sample_count() >= expected_closed_samples
                })
                .await;
                stop_ingest(shutdown, task).await;
                peer.abort();

                assert_eq!(stats.reconnects(), 1, "{action:?}, {read_interval:?}");
                assert_eq!(
                    sink.sample_count() as u64,
                    stats.frames(),
                    "{action:?}, {read_interval:?}: every frame reached the sink"
                );
                match action {
                    FirstConnection::OversizedDescribeBody => {
                        assert_eq!(stats.frames(), 9, "{read_interval:?}");
                        assert!(
                            peer_state.fault_wire_bytes.load(Ordering::Relaxed) < 1024,
                            "{read_interval:?}: the declared oversized body should not be sent"
                        );
                    }
                    FirstConnection::OversizedPlayingHeader => {
                        assert_eq!(stats.frames(), 17, "{read_interval:?}");
                        assert_eq!(
                            peer_state.fault_wire_bytes.load(Ordering::Relaxed),
                            CONTROL_MESSAGE_LIMIT,
                            "{read_interval:?}"
                        );
                    }
                    _ => unreachable!(),
                }
            }
        }
    })
    .await
    .expect("control-message regression stayed bounded");
}

#[derive(Clone, Copy, Debug)]
struct Attempt {
    accepted: bool,
    starts_sync: bool,
    new_run: bool,
    decode_time: u64,
}

struct RejectCall {
    reject: usize,
    attempts: Mutex<Vec<Attempt>>,
}

impl FragmentSink for RejectCall {
    fn send(&self, _key: &StreamKey, fragment: Fragment) -> Result<(), SinkError> {
        let mut attempts = self.attempts.lock().expect("attempt mutex");
        let accepted = attempts.len() + 1 != self.reject;
        attempts.push(Attempt {
            accepted,
            starts_sync: fragment.starts_with_sync(),
            new_run: fragment.new_run,
            decode_time: fragment.decode_time,
        });
        if accepted { Ok(()) } else { Err(SinkError) }
    }
}

async fn replay_address(plan: Plan) -> (SocketAddr, JoinHandle<()>) {
    let media = Arc::new(
        Media::from_bytes(synth::recording(plan).expect("valid synthetic recording"))
            .expect("synthetic rejection media"),
    );
    let mounts = Mounts::from([(MountName::try_new("cam").expect("mount name"), media)]);
    let server = ReplayServer::bind(
        "127.0.0.1:0".parse().expect("loopback address"),
        mounts,
        ReplayOptions::default(),
    )
    .await
    .expect("bind rejection replay");
    let address = server.local_addr().expect("rejection replay address");
    (
        address,
        tokio::spawn(async move {
            let _ = server.run().await;
        }),
    )
}

async fn rejection_case(
    plan: Plan,
    max_fragment: MediaTicks,
    reject: usize,
    read_interval: Option<Duration>,
) -> (Vec<Attempt>, Arc<StreamStats>) {
    let (address, peer) = replay_address(plan).await;
    let sink = Arc::new(RejectCall {
        reject,
        attempts: Mutex::new(Vec::new()),
    });
    let stats = Arc::new(StreamStats::new());
    let shutdown = CancellationToken::new();
    let task = tokio::spawn(run_stream(
        source(address),
        IngestOptions {
            max_fragment,
            ..options(read_interval, Duration::from_secs(10))
        },
        Arc::clone(&sink) as Arc<dyn FragmentSink>,
        Arc::clone(&stats),
        shutdown.clone(),
    ));
    wait_until(Duration::from_secs(5), || {
        sink.attempts.lock().expect("attempt mutex").len() >= 3
    })
    .await;
    stop_ingest(shutdown, task).await;
    peer.abort();
    let attempts = sink.attempts.lock().expect("attempt mutex").clone();
    (attempts, stats)
}

#[tokio::test]
async fn sink_rejection_resumes_only_at_an_independent_sync_run() {
    tokio::time::timeout(Duration::from_secs(20), async {
        for read_interval in [None, Some(Duration::from_millis(20))] {
            let (attempts, stats) = rejection_case(
                Plan {
                    gops: 3,
                    frames_per_gop: 30,
                    idr_len: 2_000,
                    trail_len: 200,
                    ..Plan::default()
                },
                MediaTicks::new(18_000),
                2,
                read_interval,
            )
            .await;
            assert!(
                attempts[0].accepted,
                "{read_interval:?}: the first fragment is accepted"
            );
            assert!(
                attempts[0].starts_sync,
                "{read_interval:?}: it starts at sync"
            );
            assert!(
                !attempts[1].accepted && !attempts[1].starts_sync,
                "{read_interval:?}: the rejected fragment is a continuation"
            );
            assert!(attempts[2].accepted, "{read_interval:?}");
            assert!(attempts[2].starts_sync, "{read_interval:?}");
            assert!(attempts[2].new_run, "{read_interval:?}");
            assert_eq!(attempts[2].decode_time, 0, "{read_interval:?}");
            assert_eq!(stats.dropped_sink(), 1, "{read_interval:?}");
            assert_eq!(stats.gaps(), 1, "{read_interval:?}");

            let (attempts, stats) = rejection_case(
                Plan {
                    gops: 4,
                    frames_per_gop: 10,
                    idr_len: 2_000,
                    trail_len: 200,
                    ..Plan::default()
                },
                MediaTicks::from_secs(2),
                1,
                read_interval,
            )
            .await;
            assert!(
                !attempts[0].accepted && attempts[0].starts_sync,
                "{read_interval:?}: the first, sync fragment is rejected"
            );
            assert!(
                attempts[1].accepted && attempts[1].starts_sync,
                "{read_interval:?}: recording resumes at the next sync fragment"
            );
            assert!(attempts[1].new_run, "{read_interval:?}");
            assert_eq!(attempts[1].decode_time, 0, "{read_interval:?}");
            assert_eq!(stats.dropped_sink(), 1, "{read_interval:?}");
            assert_eq!(stats.gaps(), 1, "{read_interval:?}");
        }
    })
    .await
    .expect("sink-rejection regression stayed bounded");
}

// Hostile cameras on OS threads with blocking sockets: they keep going even if
// the runtime under test blocks, so a regression fails instead of hanging.

/// User-Agent length for the stalled-camera cases: every request is larger
/// than any send buffer the kernel grants, so once the camera stops reading,
/// a request cannot be written whole.
const HUGE_USER_AGENT_BYTES: usize = 1024 * 1024;
/// How long a stalled camera keeps its connection before closing it. Closing
/// also releases a writer blocked on that socket.
const STALL_HOLD: Duration = Duration::from_secs(5);
/// Longest pause allowed between ticks of a task sharing the ingest's runtime.
const MAX_TICK_GAP_MS: u64 = 500;

/// Polls `predicate` every 5 ms until it holds or `timeout` passes.
async fn wait_for(timeout: Duration, mut predicate: impl FnMut() -> bool) -> bool {
    tokio::time::timeout(timeout, async {
        while !predicate() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .is_ok()
}

/// A loopback listener whose connections have a small receive window and
/// segment size, so a camera that stops reading stops the flow at once.
fn small_window_listener() -> std::net::TcpListener {
    let socket = socket2::Socket::new(socket2::Domain::IPV4, socket2::Type::STREAM, None)
        .expect("camera socket");
    socket.set_recv_buffer_size(4096).expect("SO_RCVBUF");
    socket.set_tcp_mss(536).expect("TCP_MAXSEG");
    let address: SocketAddr = "127.0.0.1:0".parse().expect("loopback address");
    socket.bind(&address.into()).expect("bind camera");
    socket.listen(16).expect("listen");
    socket.into()
}

/// Accepts connections without blocking until `stop` is set, handing each to
/// `serve` on its own thread.
fn accept_until(
    listener: std::net::TcpListener,
    stop: Arc<AtomicBool>,
    serve: impl Fn(std::net::TcpStream) + Clone + Send + 'static,
) {
    listener
        .set_nonblocking(true)
        .expect("nonblocking listener");
    thread::spawn(move || {
        while !stop.load(Ordering::SeqCst) {
            match listener.accept() {
                Ok((stream, _)) => {
                    let serve = serve.clone();
                    thread::spawn(move || {
                        if stream.set_nonblocking(false).is_ok() {
                            serve(stream);
                        }
                    });
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(5));
                }
                Err(_) => return,
            }
        }
    });
}

/// One request head read from a blocking camera socket.
struct Head {
    method: String,
    uri: String,
    cseq: u32,
    authorization: Option<String>,
}

/// Reads one request head of any size; `None` at EOF.
fn read_head(stream: &mut std::net::TcpStream, input: &mut Vec<u8>) -> io::Result<Option<Head>> {
    let mut searched = 0;
    let mut chunk = vec![0; 64 * 1024];
    loop {
        if let Some(at) = input[searched..].windows(4).position(|w| w == b"\r\n\r\n") {
            let end = searched + at;
            let head = String::from_utf8_lossy(&input[..end]).into_owned();
            input.drain(..end + 4);
            let mut lines = head.split("\r\n");
            let mut request_line = lines.next().unwrap_or_default().split(' ');
            let method = request_line.next().unwrap_or_default().to_owned();
            let uri = request_line.next().unwrap_or_default().to_owned();
            let mut cseq = 0;
            let mut authorization = None;
            for line in lines {
                let Some((name, value)) = line.split_once(':') else {
                    continue;
                };
                if name.eq_ignore_ascii_case("cseq") {
                    cseq = value.trim().parse().unwrap_or(0);
                } else if name.eq_ignore_ascii_case("authorization") {
                    authorization = Some(value.trim().to_owned());
                }
            }
            return Ok(Some(Head {
                method,
                uri,
                cseq,
                authorization,
            }));
        }
        searched = input.len().saturating_sub(3);
        let read = stream.read(&mut chunk)?;
        if read == 0 {
            return Ok(None);
        }
        input.extend_from_slice(&chunk[..read]);
    }
}

fn describe_reply(cseq: u32, content_base: &str) -> Vec<u8> {
    let sets = [
        synth::TEST_VPS.to_vec(),
        synth::TEST_SPS.to_vec(),
        synth::TEST_PPS.to_vec(),
    ];
    let body = sdp::describe(&sets, 30.0);
    rtsp::response(
        cseq,
        200,
        "OK",
        &[
            ("Content-Type", "application/sdp"),
            ("Content-Base", content_base),
        ],
        body.as_bytes(),
    )
}

fn setup_reply(cseq: u32, session: &str) -> Vec<u8> {
    rtsp::response(
        cseq,
        200,
        "OK",
        &[
            ("Session", session),
            (
                "Transport",
                "RTP/AVP/TCP;unicast;interleaved=0-1;ssrc=12345678",
            ),
        ],
        b"",
    )
}

/// Eight frames of interleaved RTP after a sender report.
fn frames_wire() -> Vec<u8> {
    let mut packetizer = Packetizer::new(Ssrc::new(0x1234_5678), 1);
    let mut wire = Vec::new();
    let report = packetizer.sender_report(WallUs::now(), RtpTimestamp::new(0));
    rtp::interleave(&mut wire, 1, &report).expect("sender report fits interleaving");
    for index in 0..8u32 {
        let kind = if index == 0 {
            SliceKind::Idr
        } else {
            SliceKind::Trail
        };
        let nal = synth::slice(u64::from(index), kind, 200).expect("valid synthetic slice");
        let nals = [nal.as_slice()];
        for packet in packetizer
            .access_unit(&nals, RtpTimestamp::new(index * 3_000), 1_400)
            .expect("valid packetization")
        {
            rtp::interleave(&mut wire, 0, &packet).expect("RTP packet fits interleaving");
        }
    }
    wire
}

/// What the camera does once it has answered `PLAY` and sent eight frames.
#[derive(Clone, Copy, Debug)]
enum Stall {
    /// Stops reading: the session's next keepalive cannot be written.
    Keepalive,
    /// Stops reading and answers a request never made, which ends the
    /// session at once: its `TEARDOWN` on this connection cannot be written.
    Teardown,
}

#[derive(Default)]
struct StallReport {
    played: AtomicBool,
    client_closed: AtomicBool,
    done: AtomicBool,
}

/// Plays one session with a one-second session timeout, stops reading, holds
/// the connection for [`STALL_HOLD`], and reports whether the client closed
/// it by then. Later connections are closed unanswered.
fn spawn_stalling_camera(stall: Stall) -> (SocketAddr, Arc<StallReport>) {
    let listener = small_window_listener();
    let address = listener.local_addr().expect("camera address");
    let report = Arc::new(StallReport::default());
    let camera = Arc::clone(&report);
    thread::spawn(move || {
        let Ok((mut stream, _)) = listener.accept() else {
            return;
        };
        let stop = Arc::new(AtomicBool::new(false));
        accept_until(listener, Arc::clone(&stop), drop);
        let played = play_then_stall(&mut stream, address, stall);
        camera.played.store(played.is_ok(), Ordering::SeqCst);
        if played.is_ok() {
            thread::sleep(STALL_HOLD);
            camera
                .client_closed
                .store(closed_by_peer(&mut stream), Ordering::SeqCst);
        }
        drop(stream);
        stop.store(true, Ordering::SeqCst);
        camera.done.store(true, Ordering::SeqCst);
    });
    (address, report)
}

fn play_then_stall(
    stream: &mut std::net::TcpStream,
    address: SocketAddr,
    stall: Stall,
) -> io::Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(10)))?;
    let session = "stall;timeout=1";
    let mut input = Vec::new();
    while let Some(head) = read_head(stream, &mut input)? {
        let reply = match head.method.as_str() {
            "DESCRIBE" => describe_reply(head.cseq, &format!("rtsp://{address}/cam/")),
            "SETUP" => setup_reply(head.cseq, session),
            _ => rtsp::response(head.cseq, 200, "OK", &[("Session", session)], b""),
        };
        stream.write_all(&reply)?;
        if head.method == "PLAY" {
            stream.write_all(&frames_wire())?;
            if matches!(stall, Stall::Teardown) {
                stream.write_all(b"RTSP/1.0 200 OK\r\nCSeq: 999\r\n\r\n")?;
            }
            return Ok(());
        }
    }
    Err(io::Error::new(io::ErrorKind::UnexpectedEof, "no PLAY"))
}

/// Reads what is queued: EOF or a reset within half a second means the peer
/// has closed its end.
fn closed_by_peer(stream: &mut std::net::TcpStream) -> bool {
    if stream.set_nonblocking(true).is_err() {
        return false;
    }
    let deadline = Instant::now() + Duration::from_millis(500);
    let mut chunk = vec![0; 256 * 1024];
    loop {
        match stream.read(&mut chunk) {
            Ok(0) => return true,
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                if Instant::now() >= deadline {
                    return false;
                }
                thread::sleep(Duration::from_millis(10));
            }
            Err(error) => return error.kind() == io::ErrorKind::ConnectionReset,
        }
    }
}

/// Records the longest pause between the ticks of a task on this runtime. The
/// monitor shares the session's only runtime thread, so a blocked thread
/// shows as a gap; on a multi-thread runtime another worker would keep
/// ticking, which is why the tests that use it run on `current_thread`.
fn spawn_tick_gap_monitor() -> (Arc<AtomicU64>, JoinHandle<()>) {
    let longest = Arc::new(AtomicU64::new(0));
    let recorded = Arc::clone(&longest);
    let task = tokio::spawn(async move {
        let mut ticks = tokio::time::interval(Duration::from_millis(50));
        let mut previous = Instant::now();
        loop {
            ticks.tick().await;
            let now = Instant::now();
            let gap = u64::try_from(now.duration_since(previous).as_millis()).unwrap_or(u64::MAX);
            recorded.fetch_max(gap, Ordering::SeqCst);
            previous = now;
        }
    });
    (longest, task)
}

/// Runs the timer-driven ingest as the daemon configures it against a camera
/// that stops reading after `PLAY`.
async fn stalled_camera_case(stall: Stall) {
    let (address, report) = spawn_stalling_camera(stall);
    let (longest_gap, monitor) = spawn_tick_gap_monitor();
    let interval = Duration::from_millis(20);
    let (_sink, stats, shutdown, task) = start_ingest(
        address,
        IngestOptions {
            read_interval: Some(interval),
            ticker: Some(ReadTicker::spawn(interval)),
            recv_low_water: Some(32 * 1024),
            user_agent: "x".repeat(HUGE_USER_AGENT_BYTES),
            ..IngestOptions::default()
        },
    );
    let played = wait_for(Duration::from_secs(10), || {
        report.played.load(Ordering::SeqCst)
    })
    .await;
    let stalled_at = Instant::now();
    let ended = wait_for(Duration::from_secs(3), || stats.reconnects() >= 1).await;
    let ended_after = stalled_at.elapsed();
    let finished = wait_for(STALL_HOLD + Duration::from_secs(5), || {
        report.done.load(Ordering::SeqCst)
    })
    .await;
    stop_ingest(shutdown, task).await;
    monitor.abort();

    let longest_gap = longest_gap.load(Ordering::SeqCst);
    assert!(played, "{stall:?}: the camera never played");
    assert!(
        longest_gap <= MAX_TICK_GAP_MS,
        "{stall:?}: the runtime stalled for {longest_gap} ms"
    );
    assert!(ended, "{stall:?}: the session outlived the stall");
    // The keepalive interval is half the one-second session timeout: the
    // unwritable keepalive fails the session at the next one.
    assert!(
        ended_after <= Duration::from_millis(2_500),
        "{stall:?}: the session ended {ended_after:?} after the camera stopped reading"
    );
    assert!(finished, "{stall:?}: the camera did not finish");
    assert!(
        report.client_closed.load(Ordering::SeqCst),
        "{stall:?}: the TEARDOWN still held the stalled connection after {STALL_HOLD:?}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn keepalive_to_a_camera_that_stopped_reading_never_stalls_the_runtime() {
    stalled_camera_case(Stall::Keepalive).await;
}

#[tokio::test(flavor = "current_thread")]
async fn teardown_to_a_camera_that_stopped_reading_never_stalls_the_runtime() {
    stalled_camera_case(Stall::Teardown).await;
}

/// How the camera asks for credentials.
#[derive(Clone, Copy, Debug)]
enum Challenge {
    /// `DESCRIBE` is challenged with Basic, so later requests carry Basic.
    Basic,
    /// `DESCRIBE` is challenged with Digest, so later requests carry Digest.
    Digest,
    /// Nothing is challenged until the fresh-connection `TEARDOWN`.
    Teardown,
}

impl Challenge {
    fn header(self) -> &'static str {
        match self {
            Challenge::Basic | Challenge::Teardown => "Basic realm=\"cam\"",
            Challenge::Digest => "Digest realm=\"cam\", nonce=\"0123456789abcdef\"",
        }
    }
}

/// The `TEARDOWN` requests a camera received on fresh connections.
#[derive(Default)]
struct Teardowns(Mutex<Vec<Head>>);

/// A camera that advertises `elsewhere` as its `Content-Base`, plays eight
/// frames, and closes the connection, so the `TEARDOWN` needs a fresh one.
fn spawn_advertising_camera(
    challenge: Challenge,
    elsewhere: SocketAddr,
    stop: &Arc<AtomicBool>,
) -> (SocketAddr, Arc<Teardowns>) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind camera");
    let address = listener.local_addr().expect("camera address");
    let teardowns = Arc::new(Teardowns::default());
    let recorded = Arc::clone(&teardowns);
    accept_until(listener, Arc::clone(stop), move |mut stream| {
        let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
        let mut input = Vec::new();
        while let Ok(Some(head)) = read_head(&mut stream, &mut input) {
            let challenged =
                head.authorization.is_none() && !matches!(challenge, Challenge::Teardown);
            let played = head.method == "PLAY";
            let reply = match head.method.as_str() {
                "DESCRIBE" if challenged => rtsp::response(
                    head.cseq,
                    401,
                    "Unauthorized",
                    &[("WWW-Authenticate", challenge.header())],
                    b"",
                ),
                "DESCRIBE" => describe_reply(head.cseq, &format!("rtsp://{elsewhere}/elsewhere/")),
                "SETUP" => setup_reply(head.cseq, "advertised;timeout=60"),
                "TEARDOWN" => {
                    let unauthenticated = head.authorization.is_none();
                    let cseq = head.cseq;
                    recorded.0.lock().expect("teardown records").push(head);
                    if unauthenticated {
                        rtsp::response(
                            cseq,
                            401,
                            "Unauthorized",
                            &[("WWW-Authenticate", challenge.header())],
                            b"",
                        )
                    } else {
                        rtsp::response(cseq, 200, "OK", &[], b"")
                    }
                }
                _ => rtsp::response(head.cseq, 200, "OK", &[], b""),
            };
            if stream.write_all(&reply).is_err() {
                return;
            }
            if played {
                let _ = stream.write_all(&frames_wire());
                let _ = stream.shutdown(std::net::Shutdown::Both);
                return;
            }
        }
    });
    (address, teardowns)
}

/// A listener that counts the connections it gets.
fn spawn_counting_listener(stop: &Arc<AtomicBool>) -> (SocketAddr, Arc<AtomicUsize>) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind listener");
    let address = listener.local_addr().expect("listener address");
    let connections = Arc::new(AtomicUsize::new(0));
    let counted = Arc::clone(&connections);
    accept_until(listener, Arc::clone(stop), move |_stream| {
        counted.fetch_add(1, Ordering::SeqCst);
    });
    (address, connections)
}

/// A camera may advertise any `Content-Base`; the `TEARDOWN` that follows a
/// dropped session still goes to the camera's own address, with credentials
/// only ever sent there.
#[tokio::test]
async fn teardown_goes_to_the_camera_never_to_an_advertised_host() {
    tokio::time::timeout(Duration::from_secs(40), async {
        for challenge in [Challenge::Basic, Challenge::Digest, Challenge::Teardown] {
            let stop = Arc::new(AtomicBool::new(false));
            let (elsewhere, elsewhere_connections) = spawn_counting_listener(&stop);
            let (camera, teardowns) = spawn_advertising_camera(challenge, elsewhere, &stop);
            let interval = Duration::from_millis(20);
            let (_sink, _stats, shutdown, task) = start_ingest_of(
                StreamSource {
                    username: "user".to_owned(),
                    password: "secret".to_owned(),
                    ..source(camera)
                },
                IngestOptions {
                    read_interval: Some(interval),
                    ticker: Some(ReadTicker::spawn(interval)),
                    recv_low_water: Some(32 * 1024),
                    ..IngestOptions::default()
                },
            );
            let authenticated = wait_for(Duration::from_secs(10), || {
                teardowns
                    .0
                    .lock()
                    .expect("teardown records")
                    .iter()
                    .any(|head| head.authorization.is_some())
            })
            .await;
            // Time for any connection to the advertised host to show.
            tokio::time::sleep(Duration::from_millis(300)).await;
            stop_ingest(shutdown, task).await;
            stop.store(true, Ordering::SeqCst);

            assert_eq!(
                elsewhere_connections.load(Ordering::SeqCst),
                0,
                "{challenge:?}: the advertised host was contacted"
            );
            assert!(
                authenticated,
                "{challenge:?}: no TEARDOWN reached the camera"
            );
            let teardowns = teardowns.0.lock().expect("teardown records");
            let teardown = teardowns
                .iter()
                .find(|head| head.authorization.is_some())
                .expect("an authenticated TEARDOWN");
            // The advertised URL survives only as the Request-URI.
            assert_eq!(teardown.uri, format!("rtsp://{elsewhere}/elsewhere/"));
            let authorization = teardown.authorization.as_deref().unwrap_or_default();
            match challenge {
                Challenge::Basic | Challenge::Teardown => {
                    assert_eq!(authorization, "Basic dXNlcjpzZWNyZXQ=");
                }
                Challenge::Digest => {
                    assert!(
                        authorization.starts_with("Digest username=\"user\""),
                        "{authorization}"
                    );
                }
            }
        }
    })
    .await
    .expect("advertised-host regression stayed bounded");
}
