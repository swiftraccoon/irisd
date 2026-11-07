//! Synthetic loopback checks for bind-before-readiness orchestration.

use std::time::Duration;

use axum::Router;
use axum::routing::get;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio_util::sync::CancellationToken;

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
        shutdown.clone(),
    ));
    let mut client = tokio::time::timeout(
        Duration::from_secs(1),
        tokio::net::TcpStream::connect(address),
    )
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
