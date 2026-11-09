//! Camera-supplied text reaches the log only as short, single, escaped lines.
//!
//! A test binary of its own: it installs the process's log subscriber,
//! configured like the daemon's (`info`, no target) with retina's `log`
//! records bridged into it, but with ANSI colours off, so the byte checks see
//! only what a camera contributed and not the formatter's escape sequences.

use std::io::{self, Read as _, Write as _};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use iris_core::id::{StreamKey, StreamKind};
use iris_ingest::{FragmentSink, IngestOptions, SinkError, StreamSource, StreamStats, run_stream};
use iris_mp4::fragment::Fragment;
use iris_replay::{rtsp, sdp, synth};
use tokio_util::sync::CancellationToken;

/// Longest log line a session may write about a hostile `DESCRIBE`: the
/// timestamp, level, and fields around at most 512 characters of error.
const MAX_LINE_BYTES: usize = 1024;

/// The log output, shared with the subscriber's writer.
#[derive(Clone, Default)]
struct Captured(Arc<Mutex<Vec<u8>>>);

impl io::Write for Captured {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.lock().expect("log buffer").extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Captured {
    fn take(&self) -> Vec<u8> {
        std::mem::take(&mut *self.0.lock().expect("log buffer"))
    }
}

struct Discard;

impl FragmentSink for Discard {
    fn send(&self, _key: &StreamKey, _fragment: Fragment) -> Result<(), SinkError> {
        Ok(())
    }
}

/// A valid SDP for one H.265 stream.
fn video_sdp() -> String {
    let sets = [
        synth::TEST_VPS.to_vec(),
        synth::TEST_SPS.to_vec(),
        synth::TEST_PPS.to_vec(),
    ];
    sdp::describe(&sets, 30.0)
}

/// The `DESCRIBE` bodies under test.
#[derive(Clone, Copy, Debug)]
enum Body {
    /// 1 MiB that is no SDP: a version line carrying terminal controls, then
    /// line breaks and forged journal priority prefixes.
    Invalid,
    /// 256 KiB of 18,000 unusable media sections after the video: a warning
    /// per section would log the whole SDP 18,000 times.
    ManySections,
    /// The video, then the most unusable sections accepted, one with controls.
    UnusableSections,
}

impl Body {
    fn bytes(self) -> Vec<u8> {
        match self {
            Body::Invalid => {
                let mut body = b"v=\x1b]0;title\x07\x1b[2J\r\n".to_vec();
                body.extend_from_slice(b"<0>irisd: forged emergency line\n");
                while body.len() < 1024 * 1024 - 4096 {
                    body.extend_from_slice(b"<3>pad\n");
                }
                body
            }
            Body::ManySections => {
                let mut body = video_sdp().into_bytes();
                for _ in 0..18_000 {
                    body.extend_from_slice(b"m=a 0 X 0\n");
                }
                while body.len() < 256 * 1024 {
                    body.extend_from_slice(
                        b"a=pad:xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx\n",
                    );
                }
                body
            }
            Body::UnusableSections => {
                let mut body = video_sdp().into_bytes();
                body.extend_from_slice(b"m=audio 0 RTP/AVP 97\r\na=fmtp:97\x1b[31m\n<0>x\r\n");
                for _ in 0..62 {
                    body.extend_from_slice(b"m=a 0 X 0\r\n");
                }
                body
            }
        }
    }
}

/// A camera that answers every `DESCRIBE` with `body` and then closes the
/// connection, counting the answers.
fn spawn_camera(body: Body, stop: &Arc<AtomicBool>) -> (SocketAddr, Arc<AtomicUsize>) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind camera");
    let address = listener.local_addr().expect("camera address");
    listener
        .set_nonblocking(true)
        .expect("nonblocking listener");
    let answered = Arc::new(AtomicUsize::new(0));
    let counted = Arc::clone(&answered);
    let stop = Arc::clone(stop);
    let body = body.bytes();
    thread::spawn(move || {
        while !stop.load(Ordering::SeqCst) {
            let mut stream = match listener.accept() {
                Ok((stream, _)) => stream,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(5));
                    continue;
                }
                Err(_) => return,
            };
            if stream.set_nonblocking(false).is_err()
                || stream
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .is_err()
            {
                continue;
            }
            let mut input = Vec::new();
            let mut chunk = [0; 4096];
            let head_end = loop {
                if let Some(at) = input.windows(4).position(|w| w == b"\r\n\r\n") {
                    break Some(at);
                }
                match stream.read(&mut chunk) {
                    Ok(0) | Err(_) => break None,
                    Ok(read) => input.extend_from_slice(&chunk[..read]),
                }
            };
            let Some(head_end) = head_end else {
                continue;
            };
            let head = String::from_utf8_lossy(&input[..head_end]);
            let cseq = head
                .split("\r\n")
                .find_map(|line| line.strip_prefix("CSeq: "))
                .and_then(|value| value.trim().parse().ok())
                .unwrap_or(1);
            let reply = rtsp::response(
                cseq,
                200,
                "OK",
                &[("Content-Type", "application/sdp")],
                &body,
            );
            if stream.write_all(&reply).is_ok() {
                counted.fetch_add(1, Ordering::SeqCst);
            }
        }
    });
    (address, answered)
}

/// Records one session against a camera answering with `body` and returns
/// the log lines it produced, the sessions that ended, the answers the camera
/// sent, and how long the first session took to end.
async fn record_once(body: Body, captured: &Captured) -> (Vec<Vec<u8>>, u64, usize, Duration) {
    let stop = Arc::new(AtomicBool::new(false));
    let (address, answered) = spawn_camera(body, &stop);
    let _ = captured.take();
    let stats = Arc::new(StreamStats::new());
    let shutdown = CancellationToken::new();
    let started = Instant::now();
    let task = tokio::spawn(run_stream(
        StreamSource {
            key: StreamKey {
                camera: "synthetic".parse().expect("camera id"),
                kind: StreamKind::Main,
            },
            url: format!("rtsp://{address}/cam").parse().expect("RTSP URL"),
            username: String::new(),
            password: String::new(),
        },
        IngestOptions::default(),
        Arc::new(Discard),
        Arc::clone(&stats),
        shutdown.clone(),
    ));
    tokio::time::timeout(Duration::from_secs(10), async {
        while stats.reconnects() == 0 {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("the session against the hostile camera ended");
    let ended_after = started.elapsed();
    shutdown.cancel();
    tokio::time::timeout(Duration::from_secs(5), task)
        .await
        .expect("ingest stops within the deadline")
        .expect("ingest task");
    stop.store(true, Ordering::SeqCst);
    let log = captured.take();
    let lines = log
        .split(|&b| b == b'\n')
        .filter(|line| !line.is_empty())
        .map(<[u8]>::to_vec)
        .collect();
    (
        lines,
        stats.reconnects(),
        answered.load(Ordering::SeqCst),
        ended_after,
    )
}

fn count(lines: &[Vec<u8>], needle: &str) -> usize {
    lines
        .iter()
        .filter(|line| String::from_utf8_lossy(line).contains(needle))
        .count()
}

#[test]
fn hostile_describe_responses_log_short_single_lines() {
    let captured = Captured::default();
    let writer = captured.clone();
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::new("info"))
        .with_target(false)
        .with_ansi(false)
        .with_writer(move || writer.clone())
        .init();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");

    for body in [Body::Invalid, Body::ManySections, Body::UnusableSections] {
        let (lines, ended, answered, ended_after) = runtime.block_on(record_once(body, &captured));
        assert!(answered >= 1, "{body:?}: the camera never answered");
        assert!(
            ended_after < Duration::from_secs(2),
            "{body:?}: the first session took {ended_after:?} to end"
        );
        for line in &lines {
            let text = String::from_utf8_lossy(line);
            assert!(
                line.len() <= MAX_LINE_BYTES,
                "{body:?}: {} bytes: {text:.200}",
                line.len()
            );
            assert!(
                !line.iter().any(|&b| b.is_ascii_control() || !b.is_ascii()),
                "{body:?}: raw control or non-ASCII byte in {text:?}"
            );
            assert!(
                !line.starts_with(b"<"),
                "{body:?}: a priority prefix starts {text:?}"
            );
        }
        // One line per ended session, plus retina's one summary per DESCRIBE
        // answer for the SDP whose video was still usable.
        let summaries = count(&lines, "SDP problem(s)");
        let expected_summaries = match body {
            Body::UnusableSections => answered,
            Body::Invalid | Body::ManySections => 0,
        };
        assert_eq!(
            count(&lines, "session ended"),
            ended as usize,
            "{body:?}: {lines:?}"
        );
        assert_eq!(summaries, expected_summaries, "{body:?}");
        assert_eq!(lines.len(), ended as usize + summaries, "{body:?}");
    }
}
