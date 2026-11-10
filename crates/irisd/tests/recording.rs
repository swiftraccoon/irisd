//! The daemon end to end: it records a loopback camera served by the replay
//! server, lists the open recording through the API, and seals it on SIGTERM
//! with a file that scans clean and a catalog row that matches it.

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
use iris_mp4::index::{MAX_PERSISTED_INDEX_RECORDS, read_index_file_bound_to_media};
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
        ..Options::default()
    };
    runtime.block_on(async {
        let server = Server::bind("127.0.0.1:0".parse().unwrap(), mounts, options)
            .await
            .unwrap();
        let address = server.local_addr().unwrap();
        tokio::spawn(server.run());
        address
    })
}

fn write_config(dir: &Path, listen: SocketAddr, camera: SocketAddr) -> PathBuf {
    let config = format!(
        "[storage]\nvideo_roots=[\"{root}\"]\nstate_dir=\"{state}\"\nsync_interval_ms=200\n\
         [api]\nlisten=\"{listen}\"\ntokens=[\"{TOKEN}\"]\n\
         [[camera]]\nid=\"test\"\nname=\"test\"\nrtsp_url=\"rtsp://{camera}/cam\"\n\
         username=\"\"\npassword=\"\"\nrecord=[\"main\"]\n",
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
