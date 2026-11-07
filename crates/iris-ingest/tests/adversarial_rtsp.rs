//! Bounded loopback regressions for ingest terminal paths and network limits.

use std::io;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use iris_core::id::{StreamKey, StreamKind};
use iris_core::time::{MediaTicks, WallUs};
use iris_ingest::{
    FragmentSink, IngestOptions, ReadTicker, SinkError, StreamSource, StreamStats, run_stream,
};
use iris_mp4::fragment::Fragment;
use iris_replay::media::Media;
use iris_replay::rtp::{Packetizer, interleave};
use iris_replay::rtsp::{self, Parsed, Request};
use iris_replay::sdp;
use iris_replay::server::{Options as ReplayOptions, Server as ReplayServer};
use iris_replay::synth::{self, Plan};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

const CONTROL_MESSAGE_LIMIT: usize = 1024 * 1024;
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
                "RTSP/1.0 200 OK\r\nCSeq: {}\r\nContent-Type: application/sdp\r\nContent-Length: {}\r\n\r\n",
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
    let session = ("Session", "regression;timeout=60".to_owned());
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
                    ("Content-Type", "application/sdp".to_owned()),
                    (
                        "Content-Base",
                        format!("{}/", request.url.trim_end_matches('/')),
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
                    "RTP/AVP/TCP;unicast;interleaved=0-1;ssrc=12345678".to_owned(),
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
                    format!("url={}/trackID=0;seq=1;rtptime=0", request.url),
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
    let mut packetizer = Packetizer::new(0x1234_5678, 1);
    let mut wire = Vec::new();
    let report = packetizer.sender_report(WallUs::now().to_ntp64(), 0);
    interleave(&mut wire, 1, &report).expect("sender report fits interleaving");
    socket.write_all(&wire).await?;
    for index in 0..count {
        let is_sync = index == 0 || second_sync == Some(index);
        let nal = synth::slice(index as u64, is_sync, 200).expect("valid synthetic slice");
        let nals = [nal.as_slice()];
        let packets = packetizer
            .access_unit(&nals, (index as u32) * 3_000, 1_400)
            .expect("valid packetization");
        wire.clear();
        for packet in packets {
            interleave(&mut wire, 0, &packet).expect("RTP packet fits interleaving");
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
    let mut packetizer = Packetizer::new(0x1234_5678, 1);
    let mut nal = vec![0x55; 60_000];
    nal[0] = 0x26;
    nal[1] = 0x01;
    let mut wire_bytes = 0usize;
    let mut packets_sent = 0usize;
    'send: while wire_bytes < OVERSIZED_AU_SEND_LIMIT {
        if packets_sent.is_multiple_of(128) {
            let mut wire = Vec::new();
            let report = packetizer.sender_report(WallUs::now().to_ntp64(), 0);
            interleave(&mut wire, 1, &report).expect("sender report fits interleaving");
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
            .access_unit(&nals, 0, 65_000)
            .expect("valid bounded packetization")
            .next()
            .expect("one packet for one bounded NAL");
        let mut packet = packet;
        packet[1] &= 0x7f;
        let mut wire = Vec::with_capacity(packet.len() + 4);
        interleave(&mut wire, 0, &packet).expect("RTP packet fits interleaving");
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
    let sink = Arc::new(Collect::default());
    let stats = Arc::new(StreamStats::new());
    let shutdown = CancellationToken::new();
    let task = tokio::spawn(run_stream(
        source(address),
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
            let mounts = [("cam".to_owned(), media)].into_iter().collect();
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
            assert!(sent <= OVERSIZED_AU_SEND_LIMIT);
            assert!(stats.sender_reports() >= 1);
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
                assert_eq!(sink.sample_count() as u64, stats.frames());
                match action {
                    FirstConnection::OversizedDescribeBody => {
                        assert_eq!(stats.frames(), 9);
                        assert!(
                            peer_state.fault_wire_bytes.load(Ordering::Relaxed) < 1024,
                            "the declared oversized body should not be sent"
                        );
                    }
                    FirstConnection::OversizedPlayingHeader => {
                        assert_eq!(stats.frames(), 17);
                        assert_eq!(
                            peer_state.fault_wire_bytes.load(Ordering::Relaxed),
                            CONTROL_MESSAGE_LIMIT
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
    let mounts = [("cam".to_owned(), media)].into_iter().collect();
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
            ..IngestOptions::default()
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
    tokio::time::timeout(Duration::from_secs(10), async {
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
        )
        .await;
        assert!(attempts[0].accepted && attempts[0].starts_sync);
        assert!(!attempts[1].accepted && !attempts[1].starts_sync);
        assert!(attempts[2].accepted);
        assert!(attempts[2].starts_sync);
        assert!(attempts[2].new_run);
        assert_eq!(attempts[2].decode_time, 0);
        assert_eq!(stats.dropped_sink(), 1);
        assert_eq!(stats.gaps(), 1);

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
        )
        .await;
        assert!(!attempts[0].accepted && attempts[0].starts_sync);
        assert!(attempts[1].accepted && attempts[1].starts_sync);
        assert!(attempts[1].new_run);
        assert_eq!(attempts[1].decode_time, 0);
        assert_eq!(stats.dropped_sink(), 1);
        assert_eq!(stats.gaps(), 1);
    })
    .await
    .expect("sink-rejection regression stayed bounded");
}
