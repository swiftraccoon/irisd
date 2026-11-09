//! Start-up and shutdown of the binary: readiness only once the API listener
//! accepts, failures before roots, workers, or readiness, `irisd record`
//! argument checks before its output root, and ordered shutdown on SIGTERM.

use std::fs;
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::os::unix::net::UnixDatagram;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

const STARTUP_TIMEOUT: Duration = Duration::from_secs(10);

fn write_config(dir: &Path, listen: SocketAddr, rtsp: SocketAddr) -> PathBuf {
    let config = format!(
        "[storage]\nvideo_roots=[\"{root}\"]\nstate_dir=\"{state}\"\n\
         [api]\nlisten=\"{listen}\"\ntokens=[\"startup-test-token-0123456789abcdef\"]\n\
         [[camera]]\nid=\"test\"\nname=\"test\"\nrtsp_url=\"rtsp://{rtsp}/main\"\n\
         username=\"\"\npassword=\"\"\nrecord=[\"main\"]\n",
        root = dir.join("video").display(),
        state = dir.join("state").display(),
    );
    let path = dir.join("config.toml");
    fs::write(&path, config).unwrap();
    path
}

fn notification_socket(dir: &Path) -> (UnixDatagram, PathBuf) {
    let path = dir.join("notify.sock");
    let socket = UnixDatagram::bind(&path).unwrap();
    socket
        .set_read_timeout(Some(Duration::from_millis(50)))
        .unwrap();
    (socket, path)
}

fn spawn_daemon(config: &Path, notify_socket: &Path, log: &Path) -> Child {
    spawn_daemon_with(config, notify_socket, log, &[], &[])
}

/// Starts the daemon with `extra_args` and `extra_env` beside the standard
/// test arguments, logging both output streams to `log`.
fn spawn_daemon_with(
    config: &Path,
    notify_socket: &Path,
    log: &Path,
    extra_args: &[&str],
    extra_env: &[(&str, &str)],
) -> Child {
    Command::new(env!("CARGO_BIN_EXE_irisd"))
        .args(["run", "--no-sandbox", "--config"])
        .arg(config)
        .args(extra_args)
        .env("NOTIFY_SOCKET", notify_socket)
        .envs(extra_env.iter().copied())
        .stdout(fs::File::create(log).unwrap())
        .stderr(Stdio::from(
            fs::OpenOptions::new().append(true).open(log).unwrap(),
        ))
        .spawn()
        .unwrap()
}

fn send_sigterm(child: &Child) {
    let pid = rustix::process::Pid::from_raw(i32::try_from(child.id()).unwrap()).unwrap();
    rustix::process::kill_process(pid, rustix::process::Signal::TERM).unwrap();
}

fn wait_for_exit(child: &mut Child) -> std::process::ExitStatus {
    let deadline = Instant::now() + STARTUP_TIMEOUT;
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            return status;
        }
        if Instant::now() >= deadline {
            child.kill().unwrap();
            child.wait().unwrap();
            panic!("daemon did not stop after startup failed");
        }
        thread::sleep(Duration::from_millis(20));
    }
}

/// When `receive_until` stops collecting notifications.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Until {
    /// The first `READY=1`, or the daemon's exit, which fails the test.
    Ready,
    /// The deadline or the daemon's exit.
    Deadline,
}

fn receive_until(
    socket: &UnixDatagram,
    child: &mut Child,
    deadline: Instant,
    until: Until,
) -> Vec<String> {
    let mut messages = Vec::new();
    let mut buffer = [0_u8; 512];
    loop {
        match socket.recv(&mut buffer) {
            Ok(len) => {
                let message = String::from_utf8_lossy(&buffer[..len]).into_owned();
                let ready = message.lines().any(|line| line == "READY=1");
                messages.push(message);
                if ready && until == Until::Ready {
                    return messages;
                }
            }
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) => {}
            Err(error) => panic!("receiving readiness notification: {error}"),
        }
        if let Some(status) = child.try_wait().unwrap() {
            assert!(
                until == Until::Deadline,
                "daemon exited before readiness with {status}"
            );
            return messages;
        }
        if Instant::now() >= deadline {
            return messages;
        }
    }
}

fn ready_count(messages: &[String]) -> usize {
    messages
        .iter()
        .flat_map(|message| message.lines())
        .filter(|line| *line == "READY=1")
        .count()
}

fn drain_notifications(socket: &UnixDatagram) -> Vec<String> {
    let mut messages = Vec::new();
    let mut buffer = [0_u8; 512];
    loop {
        match socket.recv(&mut buffer) {
            Ok(len) => messages.push(String::from_utf8_lossy(&buffer[..len]).into_owned()),
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                return messages;
            }
            Err(error) => panic!("draining readiness notifications: {error}"),
        }
    }
}

#[test]
fn occupied_api_port_emits_no_ready_and_exits_without_recording_tasks() {
    let dir = tempfile::tempdir().unwrap();
    let occupied = TcpListener::bind("127.0.0.1:0").unwrap();
    let unavailable_rtsp = TcpListener::bind("127.0.0.1:0").unwrap();
    let config = write_config(
        dir.path(),
        occupied.local_addr().unwrap(),
        unavailable_rtsp.local_addr().unwrap(),
    );
    let (notify, notify_path) = notification_socket(dir.path());
    let log = dir.path().join("daemon.log");
    let mut child = spawn_daemon(&config, &notify_path, &log);
    let status = wait_for_exit(&mut child);
    let messages = receive_until(
        &notify,
        &mut child,
        Instant::now() + Duration::from_millis(100),
        Until::Deadline,
    );

    assert!(!status.success());
    assert_eq!(ready_count(&messages), 0, "{messages:?}");
    let text = fs::read_to_string(log).unwrap();
    assert!(text.contains("binding API listener"), "{text}");
    assert!(!text.contains("irisd running"), "{text}");
    assert!(!text.contains("panicked"), "{text}");
}

#[test]
fn the_api_accepts_connections_by_the_only_ready_notification() {
    let dir = tempfile::tempdir().unwrap();
    let reserved = TcpListener::bind("127.0.0.1:0").unwrap();
    let listen = reserved.local_addr().unwrap();
    drop(reserved);
    let unavailable_rtsp = TcpListener::bind("127.0.0.1:0").unwrap();
    let config = write_config(dir.path(), listen, unavailable_rtsp.local_addr().unwrap());
    drop(unavailable_rtsp);
    let (notify, notify_path) = notification_socket(dir.path());
    let log = dir.path().join("daemon.log");
    let mut child = spawn_daemon(&config, &notify_path, &log);

    let mut messages = receive_until(
        &notify,
        &mut child,
        Instant::now() + STARTUP_TIMEOUT,
        Until::Ready,
    );
    assert_eq!(ready_count(&messages), 1, "{messages:?}");
    TcpStream::connect_timeout(&listen, Duration::from_secs(1))
        .expect("API listener must accept connections by READY=1");

    messages.extend(receive_until(
        &notify,
        &mut child,
        Instant::now() + Duration::from_millis(250),
        Until::Deadline,
    ));
    assert_eq!(ready_count(&messages), 1, "{messages:?}");

    // Clean-up only; `signal_immediately_after_ready_enters_ordered_shutdown`
    // asserts the stop itself.
    send_sigterm(&child);
    wait_for_exit(&mut child);
}

#[test]
fn invalid_daemon_periods_fail_before_roots_workers_or_readiness() {
    let cases: [(&[&str], bool); 2] = [
        (&["--stats-interval", "0s"], false),
        (&["--selfcheck-cycle", "1ns"], true),
    ];
    for (index, (arguments, two_streams)) in cases.into_iter().enumerate() {
        let dir = tempfile::tempdir().unwrap();
        let api = TcpListener::bind("127.0.0.1:0").unwrap();
        let rtsp = TcpListener::bind("127.0.0.1:0").unwrap();
        let config = write_config(
            dir.path(),
            api.local_addr().unwrap(),
            rtsp.local_addr().unwrap(),
        );
        drop(api);
        if two_streams {
            let text = fs::read_to_string(&config).unwrap().replace(
                "record=[\"main\"]",
                &format!(
                    "sub_url=\"rtsp://{}/sub\"\nrecord=[\"main\",\"sub\"]",
                    rtsp.local_addr().unwrap()
                ),
            );
            fs::write(&config, text).unwrap();
        }
        let (notify, notify_path) = notification_socket(dir.path());
        let log = dir.path().join(format!("invalid-{index}.log"));
        let mut child = spawn_daemon_with(&config, &notify_path, &log, arguments, &[]);
        let status = wait_for_exit(&mut child);
        let messages = receive_until(
            &notify,
            &mut child,
            Instant::now() + Duration::from_millis(100),
            Until::Deadline,
        );
        assert!(!status.success());
        assert_eq!(ready_count(&messages), 0, "{messages:?}");
        assert!(!dir.path().join("video").exists());
        assert!(!dir.path().join("state").exists());
        let text = fs::read_to_string(log).unwrap();
        assert!(!text.contains("recording"), "{text}");
        assert!(!text.contains("panicked"), "{text}");
    }
}

#[test]
fn daemon_with_no_recorded_sources_fails_before_roots_workers_or_readiness() {
    let dir = tempfile::tempdir().unwrap();
    let api = TcpListener::bind("127.0.0.1:0").unwrap();
    let rtsp = TcpListener::bind("127.0.0.1:0").unwrap();
    let config = write_config(
        dir.path(),
        api.local_addr().unwrap(),
        rtsp.local_addr().unwrap(),
    );
    let text = fs::read_to_string(&config)
        .unwrap()
        .replace("record=[\"main\"]", "record=[]");
    fs::write(&config, text).unwrap();
    drop(api);
    let (notify, notify_path) = notification_socket(dir.path());
    let log = dir.path().join("no-sources.log");
    let mut child = spawn_daemon(&config, &notify_path, &log);
    let status = wait_for_exit(&mut child);
    let messages = receive_until(
        &notify,
        &mut child,
        Instant::now() + Duration::from_millis(100),
        Until::Deadline,
    );
    assert!(!status.success());
    assert_eq!(ready_count(&messages), 0, "{messages:?}");
    assert!(!dir.path().join("video").exists());
    assert!(!dir.path().join("state").exists());
    let text = fs::read_to_string(log).unwrap();
    assert!(text.contains("no streams to record"), "{text}");
    assert!(!text.contains("recording"), "{text}");
    assert!(!text.contains("panicked"), "{text}");
}

#[test]
fn invalid_record_period_fails_before_the_output_root() {
    let dir = tempfile::tempdir().unwrap();
    let api = TcpListener::bind("127.0.0.1:0").unwrap();
    let rtsp = TcpListener::bind("127.0.0.1:0").unwrap();
    let config = write_config(
        dir.path(),
        api.local_addr().unwrap(),
        rtsp.local_addr().unwrap(),
    );
    let root = dir.path().join("standalone");
    let output = Command::new(env!("CARGO_BIN_EXE_irisd"))
        .args(["record", "--config"])
        .arg(&config)
        .args(["--root"])
        .arg(&root)
        .args(["--streams", "main", "--stats-interval", "0s"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(!root.exists());
    let text = String::from_utf8_lossy(&output.stderr);
    assert!(text.contains("greater than zero"), "{text}");
    assert!(!text.contains("panicked"), "{text}");
}

#[test]
fn duplicate_record_stream_options_fail_before_the_output_root() {
    for (index, stream_args) in [
        vec!["--streams", "main,main"],
        vec!["--streams", "main", "--streams", "main"],
    ]
    .into_iter()
    .enumerate()
    {
        let dir = tempfile::tempdir().unwrap();
        let api = TcpListener::bind("127.0.0.1:0").unwrap();
        let rtsp = TcpListener::bind("127.0.0.1:0").unwrap();
        let config = write_config(
            dir.path(),
            api.local_addr().unwrap(),
            rtsp.local_addr().unwrap(),
        );
        let root = dir.path().join(format!("standalone-{index}"));
        let output = Command::new(env!("CARGO_BIN_EXE_irisd"))
            .args(["record", "--config"])
            .arg(&config)
            .args(["--root"])
            .arg(&root)
            .args(stream_args)
            .args(["--duration", "1ms"])
            .output()
            .unwrap();
        assert!(!output.status.success());
        assert!(!root.exists());
        let text = String::from_utf8_lossy(&output.stderr);
        assert!(
            text.contains("more than once") || text.contains("cannot be used multiple times"),
            "{text}"
        );
    }
}

#[test]
fn standalone_with_no_resolved_sources_fails_before_the_output_root() {
    let dir = tempfile::tempdir().unwrap();
    let api = TcpListener::bind("127.0.0.1:0").unwrap();
    let rtsp = TcpListener::bind("127.0.0.1:0").unwrap();
    let config = write_config(
        dir.path(),
        api.local_addr().unwrap(),
        rtsp.local_addr().unwrap(),
    );
    let root = dir.path().join("standalone-empty");
    let output = Command::new(env!("CARGO_BIN_EXE_irisd"))
        .args(["record", "--config"])
        .arg(&config)
        .args(["--root"])
        .arg(&root)
        .args(["--streams", "sub", "--duration", "1ms"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(!root.exists());
    let text = String::from_utf8_lossy(&output.stderr);
    assert!(text.contains("no streams to record"), "{text}");
    assert!(!text.contains("panicked"), "{text}");
}

/// `WATCHDOG_USEC=0` disables the watchdog ping, the path the packaged unit
/// does not take, so the test covers the signal handling on its own.
#[test]
fn signal_immediately_after_ready_enters_ordered_shutdown_without_a_watchdog() {
    let dir = tempfile::tempdir().unwrap();
    let reserved = TcpListener::bind("127.0.0.1:0").unwrap();
    let listen = reserved.local_addr().unwrap();
    drop(reserved);
    let unavailable_rtsp = TcpListener::bind("127.0.0.1:0").unwrap();
    let config = write_config(dir.path(), listen, unavailable_rtsp.local_addr().unwrap());
    drop(unavailable_rtsp);
    let (notify, notify_path) = notification_socket(dir.path());
    let log = dir.path().join("signal.log");
    let mut child = spawn_daemon_with(&config, &notify_path, &log, &[], &[("WATCHDOG_USEC", "0")]);
    let messages = receive_until(
        &notify,
        &mut child,
        Instant::now() + STARTUP_TIMEOUT,
        Until::Ready,
    );
    assert_eq!(ready_count(&messages), 1, "{messages:?}");
    send_sigterm(&child);
    let status = wait_for_exit(&mut child);
    let messages = drain_notifications(&notify);
    let text = fs::read_to_string(log).unwrap();
    assert!(status.success(), "{text}");
    assert!(text.contains("SIGTERM; stopping"), "{text}");
    assert!(text.contains("stopped"), "{text}");
    assert!(
        messages
            .iter()
            .any(|message| message.lines().any(|line| line == "STOPPING=1")),
        "{messages:?}"
    );
}
