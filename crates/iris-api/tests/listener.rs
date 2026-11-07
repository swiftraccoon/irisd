//! Synthetic loopback checks for bind-before-readiness orchestration and the
//! serving loop's connection admission and request-head deadline.

use std::net::SocketAddr;
use std::time::{Duration, Instant};

use axum::Router;
use axum::routing::get;
use iris_api::ServeLimits;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::TcpStream;
use tokio_util::sync::CancellationToken;

const REQUEST: &[u8] = b"GET / HTTP/1.1\r\nHost: localhost\r\n\r\n";
/// The complete response to [`REQUEST`]: a two-byte body.
const RESPONSE_BODY: &[u8] = b"ok";

#[tokio::test]
async fn occupied_bind_fails_before_a_server_task_exists() {
    let occupied = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("reserve loopback address");
    let address = occupied.local_addr().expect("loopback address");
    let error = iris_api::bind(address)
        .await
        .expect_err("address is occupied");
    assert_eq!(error.kind(), std::io::ErrorKind::AddrInUse);
}

#[tokio::test]
async fn an_already_bound_listener_accepts_when_serving_starts() {
    let listener = iris_api::bind("127.0.0.1:0".parse().expect("literal address"))
        .await
        .expect("bind API listener");
    let address = listener.local_addr().expect("loopback address");
    let shutdown = CancellationToken::new();
    let task = tokio::spawn(iris_api::serve_listener(
        listener,
        Router::new().route("/", get(|| async { "ok" })),
        ServeLimits::default(),
        shutdown.clone(),
    ));
    let mut client = tokio::time::timeout(Duration::from_secs(1), TcpStream::connect(address))
        .await
        .expect("connect deadline")
        .expect("connect to bound API");
    client
        .write_all(b"GET / HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
        .await
        .expect("write request");
    let mut response = Vec::new();
    tokio::time::timeout(Duration::from_secs(1), client.read_to_end(&mut response))
        .await
        .expect("response deadline")
        .expect("read response");
    assert!(response.starts_with(b"HTTP/1.1 200"));
    shutdown.cancel();
    task.await.expect("server task").expect("server shutdown");
}

async fn serve(limits: ServeLimits) -> (SocketAddr, CancellationToken) {
    let listener = iris_api::bind("127.0.0.1:0".parse().expect("literal address"))
        .await
        .expect("bind API listener");
    let address = listener.local_addr().expect("loopback address");
    let shutdown = CancellationToken::new();
    tokio::spawn(iris_api::serve_listener(
        listener,
        Router::new().route("/", get(|| async { "ok" })),
        limits,
        shutdown.clone(),
    ));
    (address, shutdown)
}

/// Reads until the server closes the connection; returns what arrived first.
async fn read_until_closed(stream: &mut TcpStream, within: Duration) -> Vec<u8> {
    let mut received = Vec::new();
    let mut chunk = [0_u8; 1024];
    tokio::time::timeout(within, async {
        loop {
            match stream.read(&mut chunk).await {
                Ok(0) | Err(_) => break,
                Ok(read) => received.extend_from_slice(&chunk[..read]),
            }
        }
    })
    .await
    .expect("the server closed the connection before the deadline");
    received
}

/// Sends [`REQUEST`] and reads exactly its keep-alive response.
async fn exchange(stream: &mut TcpStream) {
    stream.write_all(REQUEST).await.expect("write request");
    let mut response = Vec::new();
    let mut chunk = [0_u8; 1024];
    tokio::time::timeout(Duration::from_secs(2), async {
        while !(response.starts_with(b"HTTP/1.1 200") && response.ends_with(RESPONSE_BODY)) {
            let read = stream.read(&mut chunk).await.expect("read response");
            assert_ne!(
                read, 0,
                "the connection stays open after a keep-alive response"
            );
            response.extend_from_slice(&chunk[..read]);
        }
    })
    .await
    .expect("response deadline");
}

fn short_header_deadline() -> ServeLimits {
    ServeLimits {
        header_read_timeout: Duration::from_millis(300),
        ..ServeLimits::default()
    }
}

#[tokio::test]
async fn a_silent_connection_is_closed_at_the_request_head_deadline() {
    let (address, shutdown) = serve(short_header_deadline()).await;
    let mut silent = TcpStream::connect(address).await.expect("connect");
    let started = Instant::now();
    read_until_closed(&mut silent, Duration::from_secs(3)).await;
    assert!(
        started.elapsed() >= Duration::from_millis(250),
        "{:?}",
        started.elapsed()
    );
    shutdown.cancel();
}

#[tokio::test]
async fn a_partial_request_head_is_closed_at_the_request_head_deadline() {
    let (address, shutdown) = serve(short_header_deadline()).await;
    let mut partial = TcpStream::connect(address).await.expect("connect");
    partial
        .write_all(b"GET / HTTP/1.1\r\nHost: localhost\r\n")
        .await
        .expect("partial head");
    let received = read_until_closed(&mut partial, Duration::from_secs(3)).await;
    assert!(
        !received.starts_with(b"HTTP/1.1 200"),
        "an incomplete head is never served"
    );
    shutdown.cancel();
}

#[tokio::test]
async fn an_idle_kept_alive_connection_is_closed_at_the_request_head_deadline() {
    let (address, shutdown) = serve(short_header_deadline()).await;
    let mut idle = TcpStream::connect(address).await.expect("connect");
    exchange(&mut idle).await;
    let started = Instant::now();
    let after = read_until_closed(&mut idle, Duration::from_secs(3)).await;
    assert!(after.is_empty(), "nothing follows the completed response");
    assert!(started.elapsed() >= Duration::from_millis(250));
    shutdown.cancel();
}

#[tokio::test]
async fn the_connection_cap_admits_exactly_its_limit() {
    let (address, shutdown) = serve(ServeLimits {
        max_connections: 2,
        ..ServeLimits::default()
    })
    .await;
    let mut first = TcpStream::connect(address).await.expect("first connection");
    exchange(&mut first).await;
    let mut second = TcpStream::connect(address)
        .await
        .expect("second connection");
    exchange(&mut second).await;

    // The kernel completes the third handshake, but the server does not accept
    // it while two connections hold the slots.
    let mut third = TcpStream::connect(address).await.expect("third connection");
    third.write_all(REQUEST).await.expect("queued request");
    let mut chunk = [0_u8; 64];
    assert!(
        tokio::time::timeout(Duration::from_millis(300), third.read(&mut chunk))
            .await
            .is_err(),
        "a connection beyond the cap waits in the backlog"
    );

    drop(first);
    let mut response = Vec::new();
    tokio::time::timeout(Duration::from_secs(2), async {
        while !response.ends_with(RESPONSE_BODY) {
            let read = third.read(&mut chunk).await.expect("read response");
            assert_ne!(read, 0, "the admitted connection answers");
            response.extend_from_slice(&chunk[..read]);
        }
    })
    .await
    .expect("the third connection is served once a slot frees");
    assert!(response.starts_with(b"HTTP/1.1 200"));
    exchange(&mut second).await;
    shutdown.cancel();
}

#[tokio::test]
async fn shutdown_drains_an_in_flight_response_and_stops_accepting() {
    let listener = iris_api::bind("127.0.0.1:0".parse().expect("literal address"))
        .await
        .expect("bind API listener");
    let address = listener.local_addr().expect("loopback address");
    let shutdown = CancellationToken::new();
    let (entered_tx, entered_rx) = tokio::sync::oneshot::channel::<()>();
    let entered_tx = std::sync::Arc::new(std::sync::Mutex::new(Some(entered_tx)));
    let slow = Router::new().route(
        "/",
        get(move || {
            let entered = entered_tx.lock().expect("signal lock").take();
            async move {
                if let Some(entered) = entered {
                    let _ = entered.send(());
                }
                tokio::time::sleep(Duration::from_millis(300)).await;
                "ok"
            }
        }),
    );
    let task = tokio::spawn(iris_api::serve_listener(
        listener,
        slow,
        ServeLimits::default(),
        shutdown.clone(),
    ));
    let mut client = TcpStream::connect(address).await.expect("connect");
    client.write_all(REQUEST).await.expect("request");
    entered_rx.await.expect("handler entered");
    shutdown.cancel();
    let response = read_until_closed(&mut client, Duration::from_secs(3)).await;
    assert!(
        response.starts_with(b"HTTP/1.1 200") && response.ends_with(RESPONSE_BODY),
        "the in-flight response completes before the connection closes"
    );
    tokio::time::timeout(Duration::from_secs(3), task)
        .await
        .expect("serving ends after draining")
        .expect("server task")
        .expect("server shutdown");
    assert!(
        TcpStream::connect(address).await.is_err(),
        "the listener is closed after shutdown"
    );
}
