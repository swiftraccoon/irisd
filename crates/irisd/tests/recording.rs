//! The daemon end to end: it records a loopback camera served by the replay
//! server, lists the open recording through the API, and seals it on SIGTERM
//! with a file that scans clean and a catalog row that matches it.

#![expect(
    clippy::missing_assert_message,
    clippy::unwrap_used,
    reason = "a helper of a test fails as the test does, by panicking"
)]

use std::fs::{self, File};
use std::io::{Read as _, Write as _};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::os::unix::net::UnixDatagram;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use iris_catalog::CatalogHandle;
use iris_core::time::WallUs;
use iris_mp4::index::{
    FLAG_DISCONTINUITY_BEFORE, MAX_PERSISTED_INDEX_RECORDS, read_index_file_bound_to_media,
};
use iris_mp4::reader::{self, ScanOptions};
use iris_replay::media::Media;
use iris_replay::rtsp::MountName;
use iris_replay::server::{Mounts, Options, Server};
use iris_replay::synth::{self, Plan};

const TOKEN: &str = "recording-test-token-0123456789abcdef";
const DEADLINE: Duration = Duration::from_secs(30);

/// A synthetic camera on loopback: 30 fps in GOPs of ten frames, repeated
/// for as long as a client plays it.
fn start_camera(runtime: &tokio::runtime::Runtime) -> SocketAddr {
    start_camera_at(runtime, "127.0.0.1:0".parse().unwrap()).expect("a free loopback port")
}

/// The synthetic camera on `listen`; `None` while the address is still taken.
fn start_camera_at(runtime: &tokio::runtime::Runtime, listen: SocketAddr) -> Option<SocketAddr> {
    start_camera_with(runtime, listen, &Options::default())
}

/// The synthetic camera on `listen` with `options`, replayed without end.
fn start_camera_with(
    runtime: &tokio::runtime::Runtime,
    listen: SocketAddr,
    options: &Options,
) -> Option<SocketAddr> {
    let plan = Plan {
        gops: 30,
        frames_per_gop: 10,
        idr_len: 4_000,
        trail_len: 400,
        ..Plan::default()
    };
    let media = Arc::new(Media::from_bytes(synth::recording(plan).unwrap()).unwrap());
    let mounts = Mounts::from([(MountName::try_new("cam").unwrap(), media)]);
    let options = Options {
        repeat: true,
        ..options.clone()
    };
    runtime.block_on(async {
        let server = Server::bind(listen, mounts, options).await.ok()?;
        let address = server.local_addr().unwrap();
        tokio::spawn(server.run());
        Some(address)
    })
}

fn write_config(dir: &Path, listen: SocketAddr, camera: SocketAddr) -> PathBuf {
    write_config_recording(dir, listen, camera, "\"main\"")
}

/// A configuration that records the kinds in `record` (a TOML list body).
fn write_config_recording(
    dir: &Path,
    listen: SocketAddr,
    camera: SocketAddr,
    record: &str,
) -> PathBuf {
    let config = format!(
        "[storage]\nvideo_roots=[\"{root}\"]\nstate_dir=\"{state}\"\nsync_interval_ms=200\n\
         [api]\nlisten=\"{listen}\"\ntokens=[\"{TOKEN}\"]\n\
         [[camera]]\nid=\"test\"\nname=\"test\"\nrtsp_url=\"rtsp://{camera}/cam\"\n\
         username=\"\"\npassword=\"\"\nrecord=[{record}]\n",
        root = dir.join("video").display(),
        state = dir.join("state").display(),
    );
    let path = dir.join("config.toml");
    fs::write(&path, config).unwrap();
    path
}

fn spawn_daemon(config: &Path, notify_socket: &Path, log: &Path) -> Child {
    Command::new(env!("CARGO_BIN_EXE_irisd"))
        .args(["run", "--no-sandbox", "--config"])
        .arg(config)
        .env("NOTIFY_SOCKET", notify_socket)
        .stdout(File::create(log).unwrap())
        .stderr(Stdio::from(
            fs::OpenOptions::new().append(true).open(log).unwrap(),
        ))
        .spawn()
        .unwrap()
}

/// Waits for the daemon's `READY=1`, failing if it exits first.
fn wait_until_ready(socket: &UnixDatagram, child: &mut Child, log: &Path) {
    let deadline = Instant::now() + DEADLINE;
    let mut buffer = [0_u8; 512];
    loop {
        if let Ok(len) = socket.recv(&mut buffer)
            && String::from_utf8_lossy(&buffer[..len])
                .lines()
                .any(|line| line == "READY=1")
        {
            return;
        }
        if let Some(status) = child.try_wait().unwrap() {
            panic!(
                "the daemon exited before readiness with {status}:\n{}",
                fs::read_to_string(log).unwrap_or_default()
            );
        }
        assert!(
            Instant::now() < deadline,
            "no readiness within the deadline"
        );
    }
}

/// One authenticated GET; returns the status code and the body.
fn get(listen: SocketAddr, path_and_query: &str) -> (u16, String) {
    let mut stream = TcpStream::connect_timeout(&listen, Duration::from_secs(2)).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    write!(
        stream,
        "GET {path_and_query} HTTP/1.1\r\nHost: {listen}\r\nAuthorization: Bearer {TOKEN}\r\n\
         Connection: close\r\n\r\n"
    )
    .unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).unwrap();
    let status = response
        .split(' ')
        .nth(1)
        .and_then(|code| code.parse().ok())
        .unwrap_or_else(|| panic!("no status line in {response:?}"));
    let body = response
        .split_once("\r\n\r\n")
        .map_or_else(String::new, |(_, body)| body.to_owned());
    (status, body)
}

/// Opens the live WebSocket at `path` and returns the payloads of its first
/// `count` binary messages. The handshake and the frame headers are read by
/// hand: a server's frames are not masked, and their length is seven bits,
/// or sixteen or sixty-four after a marker.
fn live_messages(listen: SocketAddr, path: &str, count: usize) -> Vec<Vec<u8>> {
    let mut stream = TcpStream::connect_timeout(&listen, Duration::from_secs(2)).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(20)))
        .unwrap();
    write!(
        stream,
        "GET {path} HTTP/1.1\r\nHost: {listen}\r\nAuthorization: Bearer {TOKEN}\r\n\
         Connection: Upgrade\r\nUpgrade: websocket\r\nSec-WebSocket-Version: 13\r\n\
         Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\r\n"
    )
    .unwrap();
    let mut head = Vec::new();
    let mut byte = [0_u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        stream.read_exact(&mut byte).unwrap();
        head.push(byte[0]);
    }
    let head = String::from_utf8_lossy(&head);
    assert!(head.starts_with("HTTP/1.1 101"), "{head}");
    let mut messages = Vec::new();
    while messages.len() < count {
        let mut header = [0_u8; 2];
        stream.read_exact(&mut header).unwrap();
        let len = match header[1] & 0x7f {
            126 => {
                let mut len = [0_u8; 2];
                stream.read_exact(&mut len).unwrap();
                u64::from(u16::from_be_bytes(len))
            }
            127 => {
                let mut len = [0_u8; 8];
                stream.read_exact(&mut len).unwrap();
                u64::from_be_bytes(len)
            }
            short => u64::from(short),
        };
        let mut payload = vec![0_u8; usize::try_from(len).unwrap()];
        stream.read_exact(&mut payload).unwrap();
        // Opcode 2 is a binary message; pings and the rest are passed over.
        if header[0] & 0x0f == 2 {
            messages.push(payload);
        }
    }
    messages
}

/// The largest `"fragments":N` among the listed segments.
fn listed_fragments(body: &str) -> u64 {
    body.split("\"fragments\":")
        .skip(1)
        .filter_map(|tail| {
            let digits: String = tail.chars().take_while(char::is_ascii_digit).collect();
            digits.parse().ok()
        })
        .max()
        .unwrap_or(0)
}

/// The sum of `"fragments":N` over the listed segments.
fn listed_fragments_in_all(body: &str) -> u64 {
    body.split("\"fragments\":")
        .skip(1)
        .filter_map(|tail| {
            let digits: String = tail.chars().take_while(char::is_ascii_digit).collect();
            digits.parse::<u64>().ok()
        })
        .sum()
}

/// Polls the segment listing until it holds at least `at_least` fragments.
fn wait_for_fragments(listen: SocketAddr, at_least: u64, log: &Path) -> u64 {
    let now = WallUs::now().get();
    let listing = format!(
        "/api/streams/test/main/segments?from={}&to={}",
        now - 60_000_000,
        now + 600_000_000
    );
    let deadline = Instant::now() + DEADLINE;
    loop {
        let (status, body) = get(listen, &listing);
        assert_eq!(status, 200, "{body}");
        let fragments = listed_fragments_in_all(&body);
        if fragments >= at_least {
            return fragments;
        }
        assert!(
            Instant::now() < deadline,
            "fewer than {at_least} durable fragments within the deadline: {body}\n{}",
            fs::read_to_string(log).unwrap_or_default()
        );
        thread::sleep(Duration::from_millis(100));
    }
}

fn recordings(root: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(dir) = pending.pop() {
        for entry in fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                pending.push(path);
            } else if path.extension().is_some_and(|x| x == "mp4") {
                found.push(path);
            }
        }
    }
    found.sort();
    found
}

#[test]
fn the_daemon_records_a_camera_lists_it_and_seals_it_at_shutdown() {
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let camera = start_camera(&runtime);
    let dir = tempfile::tempdir().unwrap();
    let reserved = TcpListener::bind("127.0.0.1:0").unwrap();
    let listen = reserved.local_addr().unwrap();
    drop(reserved);
    let config = write_config(dir.path(), listen, camera);
    let notify_path = dir.path().join("notify.sock");
    let notify = UnixDatagram::bind(&notify_path).unwrap();
    notify
        .set_read_timeout(Some(Duration::from_millis(50)))
        .unwrap();
    let log = dir.path().join("daemon.log");
    let mut child = spawn_daemon(&config, &notify_path, &log);
    wait_until_ready(&notify, &mut child, &log);

    let (status, cameras) = get(listen, "/api/cameras");
    assert_eq!(status, 200, "{cameras}");
    assert!(cameras.contains("\"id\":\"test\""), "{cameras}");
    // An unauthenticated client learns nothing.
    let mut bare = TcpStream::connect_timeout(&listen, Duration::from_secs(2)).unwrap();
    write!(
        bare,
        "GET /api/cameras HTTP/1.1\r\nHost: {listen}\r\nConnection: close\r\n\r\n"
    )
    .unwrap();
    let mut refused = String::new();
    bare.read_to_string(&mut refused).unwrap();
    assert!(refused.starts_with("HTTP/1.1 401"), "{refused}");

    // The open recording is listed once fragments are durable.
    let now = WallUs::now().get();
    let listing = format!(
        "/api/streams/test/main/segments?from={}&to={}",
        now - 60_000_000,
        now + 600_000_000
    );
    let deadline = Instant::now() + DEADLINE;
    let durable = loop {
        let (status, body) = get(listen, &listing);
        assert_eq!(status, 200, "{body}");
        let fragments = listed_fragments(&body);
        if fragments >= 3 {
            break fragments;
        }
        assert!(
            Instant::now() < deadline,
            "no durable fragments within the deadline: {body}\n{}",
            fs::read_to_string(&log).unwrap_or_default()
        );
        thread::sleep(Duration::from_millis(100));
    };

    stop_with_sigterm(&mut child, &log);
    check_what_was_left(dir.path(), durable, &runtime);
}

/// SIGTERM stops the daemon in order, with success.
fn stop_with_sigterm(child: &mut Child, log: &Path) {
    let pid = rustix::process::Pid::from_raw(i32::try_from(child.id()).unwrap()).unwrap();
    rustix::process::kill_process(pid, rustix::process::Signal::TERM).unwrap();
    let deadline = Instant::now() + DEADLINE;
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if Instant::now() >= deadline {
            child.kill().unwrap();
            child.wait().unwrap();
            panic!("the daemon did not stop on SIGTERM");
        }
        thread::sleep(Duration::from_millis(20));
    };
    assert!(
        status.success(),
        "{status}:\n{}",
        fs::read_to_string(log).unwrap_or_default()
    );
}

/// What the daemon left in `dir`: one recording that scans clean to its last
/// byte with at least the `durable` fragments the API listed, opens at a sync
/// fragment, has a sidecar bound to it, and a sealed catalog row with the
/// same facts.
fn check_what_was_left(dir: &Path, durable: u64, runtime: &tokio::runtime::Runtime) {
    let files = recordings(&dir.join("video"));
    assert_eq!(files.len(), 1, "{files:?}");
    let mut media = File::open(&files[0]).unwrap();
    let len = media.metadata().unwrap().len();
    let report = reader::scan(
        &mut media,
        ScanOptions {
            verify_crc: true,
            collect_samples: false,
        },
    )
    .unwrap();
    assert_eq!(report.stop, None);
    assert_eq!(report.valid_len, len);
    assert!(
        report.fragments.len() as u64 >= durable,
        "{}",
        report.fragments.len()
    );
    assert!(report.fragments[0].starts_with_sync);
    // Ten frames a fragment, as the camera's GOPs; the last holds what had
    // arrived of its GOP at shutdown.
    let (last, whole) = report.fragments.split_last().unwrap();
    assert!(whole.iter().all(|f| f.sample_count == 10));
    assert!((1..=10).contains(&last.sample_count));
    let sidecar = File::open(files[0].with_extension("idx")).unwrap();
    let (_, records) =
        read_index_file_bound_to_media(sidecar, MAX_PERSISTED_INDEX_RECORDS, &report, len)
            .unwrap()
            .expect("the sidecar is bound to the recording");
    assert_eq!(records.len(), report.fragments.len());

    let fragments = u32::try_from(report.fragments.len()).unwrap();
    runtime.block_on(async {
        let catalog = CatalogHandle::open(&dir.join("state").join("irisd.db")).unwrap();
        let row = catalog
            .segment_by_path(&files[0])
            .await
            .unwrap()
            .expect("the recording has a catalog row");
        assert!(row.sealed);
        assert_eq!((row.bytes, row.fragments), (len, fragments));
        assert_eq!(row.key.camera.as_str(), "test");
        assert_eq!(catalog.writing_segments().await.unwrap(), Vec::new());
        catalog.shutdown();
    });
}

/// A camera that goes away and comes back, as one that reboots: the daemon
/// reconnects on its own, records the returned stream as a second run, and
/// everything it wrote on either side of the outage scans clean.
#[test]
fn the_daemon_resumes_recording_when_the_camera_returns() {
    let first_camera = tokio::runtime::Runtime::new().unwrap();
    let camera = start_camera(&first_camera);
    let dir = tempfile::tempdir().unwrap();
    let reserved = TcpListener::bind("127.0.0.1:0").unwrap();
    let listen = reserved.local_addr().unwrap();
    drop(reserved);
    let config = write_config(dir.path(), listen, camera);
    let notify_path = dir.path().join("notify.sock");
    let notify = UnixDatagram::bind(&notify_path).unwrap();
    notify
        .set_read_timeout(Some(Duration::from_millis(50)))
        .unwrap();
    let log = dir.path().join("daemon.log");
    let mut child = spawn_daemon(&config, &notify_path, &log);
    wait_until_ready(&notify, &mut child, &log);
    let before = wait_for_fragments(listen, 3, &log);

    // The camera vanishes with its connections, stays away, and returns on
    // the same address.
    first_camera.shutdown_background();
    thread::sleep(Duration::from_millis(1_500));
    let second_camera = tokio::runtime::Runtime::new().unwrap();
    let deadline = Instant::now() + DEADLINE;
    while start_camera_at(&second_camera, camera).is_none() {
        assert!(
            Instant::now() < deadline,
            "the camera's address stayed taken"
        );
        thread::sleep(Duration::from_millis(100));
    }
    let returned = WallUs::now();
    wait_for_fragments(listen, before + 6, &log);
    stop_with_sigterm(&mut child, &log);

    let files = recordings(&dir.path().join("video"));
    let mut runs = 0;
    let mut last_wall = WallUs::default();
    for file in &files {
        let mut media = File::open(file).unwrap();
        let len = media.metadata().unwrap().len();
        let report = reader::scan(
            &mut media,
            ScanOptions {
                verify_crc: true,
                collect_samples: false,
            },
        )
        .unwrap();
        assert_eq!(report.stop, None, "{file:?}");
        assert_eq!(report.valid_len, len, "{file:?}");
        assert!(report.fragments[0].starts_with_sync, "{file:?}");
        let sidecar = File::open(file.with_extension("idx")).unwrap();
        let (_, records) =
            read_index_file_bound_to_media(sidecar, MAX_PERSISTED_INDEX_RECORDS, &report, len)
                .unwrap()
                .expect("the sidecar is bound to the recording");
        runs += records
            .iter()
            .filter(|record| record.flags & FLAG_DISCONTINUITY_BEFORE != 0)
            .count();
        last_wall = last_wall.max(records.last().unwrap().wall);
    }
    assert_eq!(runs, 2, "one run on each side of the outage: {files:?}");
    assert!(
        last_wall > returned,
        "frames were recorded after the return"
    );
    second_camera.block_on(async {
        let catalog = CatalogHandle::open(&dir.path().join("state").join("irisd.db")).unwrap();
        assert_eq!(catalog.writing_segments().await.unwrap(), Vec::new());
        for file in &files {
            let row = catalog.segment_by_path(file).await.unwrap();
            assert!(row.is_some_and(|row| row.sealed), "{file:?}");
        }
        catalog.shutdown();
    });
}

/// The first forty frames of the test camera's live audio. The audio is
/// live in the protocol of the video streams: the sound track's init
/// segment, then a fragment of one frame for each, contiguous in time.
fn heard_live_audio(listen: SocketAddr) -> Vec<reader::FragmentInfo> {
    let live = live_messages(listen, "/api/streams/test/audio/live", 41);
    assert_eq!(live[0][0], 0x01, "the init segment comes first");
    assert!(live[0].windows(4).any(|window| window == b"mp4a"));
    let heard: Vec<reader::FragmentInfo> = live[1..]
        .iter()
        .map(|message| {
            assert_eq!(message[0], 0x02);
            reader::parse_fragment(&message[1..]).expect("a live fragment parses")
        })
        .collect();
    assert!(
        heard
            .iter()
            .all(|frame| frame.sample_count == 1 && frame.duration == 1_440 && frame.all_sync),
        "{heard:?}"
    );
    assert!(
        heard
            .windows(2)
            .all(|pair| pair[1].decode_time == pair[0].decode_time + 1_440),
        "live audio is not contiguous"
    );
    heard
}

/// A live frame has the decode time the recording gives it: every frame
/// heard that the file reaches is a sample of one of its fragments.
fn check_heard_frames_are_recorded(
    heard: &[reader::FragmentInfo],
    recorded: &[reader::FragmentInfo],
) {
    let recorded_end = recorded
        .last()
        .map_or(0, |f| f.decode_time + u64::from(f.duration));
    let in_file = |decode_time: u64| {
        recorded.iter().any(|f| {
            decode_time >= f.decode_time
                && decode_time < f.decode_time + u64::from(f.duration)
                && (decode_time - f.decode_time).is_multiple_of(1_440)
        })
    };
    let reached: Vec<u64> = heard
        .iter()
        .map(|frame| frame.decode_time)
        .filter(|&decode_time| decode_time < recorded_end)
        .collect();
    assert!(reached.len() >= 20, "{} of the frames heard", reached.len());
    assert!(reached.iter().all(|&decode_time| in_file(decode_time)));
}

/// A camera that offers AAC, recorded with `audio`: the daemon keeps the
/// audio in files of its own under the camera's `audio` stream, lists them
/// through the API, serves the frames live with the decode times it records,
/// and seals the files at shutdown like the video.
#[test]
fn the_daemon_records_audio_in_its_own_files() {
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let camera = start_camera_with(
        &runtime,
        "127.0.0.1:0".parse().unwrap(),
        &Options {
            audio_sample_rate: Some(64_000),
            ..Options::default()
        },
    )
    .unwrap();
    let dir = tempfile::tempdir().unwrap();
    let reserved = TcpListener::bind("127.0.0.1:0").unwrap();
    let listen = reserved.local_addr().unwrap();
    drop(reserved);
    let config = write_config_recording(dir.path(), listen, camera, "\"main\", \"audio\"");
    let notify_path = dir.path().join("notify.sock");
    let notify = UnixDatagram::bind(&notify_path).unwrap();
    notify
        .set_read_timeout(Some(Duration::from_millis(50)))
        .unwrap();
    let log = dir.path().join("daemon.log");
    let mut child = spawn_daemon(&config, &notify_path, &log);
    wait_until_ready(&notify, &mut child, &log);

    // The audio is cut with the video: six fragments are two seconds.
    let now = WallUs::now().get();
    let listing = format!(
        "/api/streams/test/audio/segments?from={}&to={}",
        now - 60_000_000,
        now + 600_000_000
    );
    let deadline = Instant::now() + DEADLINE;
    loop {
        let (status, body) = get(listen, &listing);
        assert_eq!(status, 200, "{body}");
        if listed_fragments_in_all(&body) >= 6 {
            assert!(body.contains("\"codecs\":\"mp4a.40.2\""), "{body}");
            break;
        }
        assert!(
            Instant::now() < deadline,
            "no durable audio within the deadline: {body}\n{}",
            fs::read_to_string(&log).unwrap_or_default()
        );
        thread::sleep(Duration::from_millis(100));
    }
    let heard = heard_live_audio(listen);
    // A second for the video fragments over the frames heard to close, so
    // that the file reaches them.
    thread::sleep(Duration::from_secs(1));
    stop_with_sigterm(&mut child, &log);

    let files = recordings(&dir.path().join("video"));
    let audio: Vec<&PathBuf> = files
        .iter()
        .filter(|path| path.components().any(|part| part.as_os_str() == "audio"))
        .collect();
    assert_eq!(audio.len(), 1, "{files:?}");
    assert_eq!(files.len(), 2, "one video and one audio file: {files:?}");
    let mut media = File::open(audio[0]).unwrap();
    let len = media.metadata().unwrap().len();
    let report = reader::scan(
        &mut media,
        ScanOptions {
            verify_crc: true,
            collect_samples: false,
        },
    )
    .unwrap();
    check_heard_frames_are_recorded(&heard, &report.fragments);
    assert_eq!(report.stop, None);
    assert_eq!(report.valid_len, len);
    assert!(report.fragments.len() >= 6);
    // A video fragment of ten frames lasts a third of a second: twenty or
    // twenty-one audio frames of 16 ms, the last fragment perhaps fewer.
    let (last, whole) = report.fragments.split_last().unwrap();
    assert!(
        whole.iter().all(|f| (20..=21).contains(&f.sample_count)
            && f.duration == u32::from(f.sample_count) * 1_440
            && f.starts_with_sync),
        "{:?}",
        whole.iter().map(|f| f.sample_count).collect::<Vec<_>>()
    );
    assert!((1..=21).contains(&last.sample_count));
    let bytes = fs::read(audio[0]).unwrap();
    assert!(bytes.windows(4).any(|window| window == b"mp4a"));
    assert!(bytes.windows(4).any(|window| window == b"soun"));
    runtime.block_on(async {
        let catalog = CatalogHandle::open(&dir.path().join("state").join("irisd.db")).unwrap();
        let row = catalog
            .segment_by_path(audio[0])
            .await
            .unwrap()
            .expect("the audio recording has a catalog row");
        assert!(row.sealed);
        assert_eq!(row.key.kind, iris_core::id::StreamKind::Audio);
        assert_eq!(row.bytes, len);
        assert_eq!(catalog.writing_segments().await.unwrap(), Vec::new());
        catalog.shutdown();
    });
}
