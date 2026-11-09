//! `irisd serve`: a static file server with range support for playback tests.

use std::future::IntoFuture;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::Context;
use axum::Router;
use axum::extract::Request;
use axum::http::{HeaderValue, header};
use axum::middleware::{self, Next};
use axum::response::Response;
use clap::Args as ClapArgs;
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;
use tower_http::services::ServeDir;
use tracing::info;

/// Grace allowed for established responses before their runtime is stopped.
const DRAIN_GRACE: Duration = Duration::from_secs(3);

/// Arguments for `serve`.
#[derive(Debug, ClapArgs)]
pub struct Args {
    /// Directory to serve (for example the output of `irisd hls`).
    #[arg(long, value_name = "DIR")]
    dir: PathBuf,
    /// Address to listen on; the default accepts connections on every interface.
    #[arg(long, default_value = "0.0.0.0:8080")]
    listen: SocketAddr,
}

/// Runs the subcommand until SIGINT (Ctrl-C) or SIGTERM.
pub async fn run(args: Args) -> anyhow::Result<()> {
    let mut sigint = signal(SignalKind::interrupt()).context("installing the SIGINT handler")?;
    let mut sigterm = signal(SignalKind::terminate()).context("installing the SIGTERM handler")?;
    let dir = args
        .dir
        .canonicalize()
        .with_context(|| format!("resolving {}", args.dir.display()))?;
    let app = Router::new()
        .fallback_service(ServeDir::new(&dir))
        .layer(middleware::from_fn(log_and_fix_types));
    let listener = tokio::net::TcpListener::bind(args.listen)
        .await
        .with_context(|| format!("binding {}", args.listen))?;
    let addr = listener.local_addr().context("reading the bound address")?;
    info!(
        dir = %dir.display(),
        %addr,
        "serving; open http://<this-host>:{}/master.m3u8",
        addr.port()
    );
    let listener = listener
        .into_std()
        .context("detaching the listener from the runtime")?;
    let shutdown = CancellationToken::new();
    let mut server = spawn_server(listener, app, shutdown.clone(), DRAIN_GRACE)?;
    tokio::select! {
        result = &mut server => return receive_server_result(result),
        _ = sigint.recv() => shutdown.cancel(),
        _ = sigterm.recv() => shutdown.cancel(),
    }
    match tokio::time::timeout(DRAIN_GRACE + Duration::from_secs(1), server).await {
        Ok(result) => receive_server_result(result),
        Err(_) => anyhow::bail!("static server did not stop after its forced-drain deadline"),
    }
}

fn receive_server_result(
    result: Result<std::io::Result<()>, oneshot::error::RecvError>,
) -> anyhow::Result<()> {
    result.context("static server thread stopped without a result")??;
    Ok(())
}

fn spawn_server(
    listener: std::net::TcpListener,
    app: Router,
    shutdown: CancellationToken,
    drain_grace: Duration,
) -> std::io::Result<oneshot::Receiver<std::io::Result<()>>> {
    listener.set_nonblocking(true)?;
    let (result_tx, result_rx) = oneshot::channel();
    std::thread::Builder::new()
        .name("irisd-static-server".into())
        .spawn(move || {
            let result = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .and_then(|runtime| {
                    runtime.block_on(async move {
                        let listener = tokio::net::TcpListener::from_std(listener)?;
                        let graceful = shutdown.clone();
                        let server = axum::serve(listener, app)
                            .with_graceful_shutdown(graceful.cancelled_owned())
                            .into_future();
                        tokio::pin!(server);
                        tokio::select! {
                            result = &mut server => result,
                            () = shutdown.cancelled() => {
                                match tokio::time::timeout(drain_grace, &mut server).await {
                                    Ok(result) => result,
                                    Err(_) => {
                                        // Returning drops this runtime after the server
                                        // task, which aborts every connection task and
                                        // closes its socket.
                                        Ok(())
                                    }
                                }
                            }
                        }
                    })
                });
            let _ = result_tx.send(result);
        })?;
    Ok(result_rx)
}

async fn log_and_fix_types(req: Request, next: Next) -> Response {
    let method = req.method().clone();
    let path = req.uri().path().to_owned();
    let range = req
        .headers()
        .get(header::RANGE)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    let mut res = next.run(req).await;
    let extension = std::path::Path::new(&path)
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase);
    let content_type = match extension.as_deref() {
        Some("m3u8") => Some("application/vnd.apple.mpegurl"),
        Some("mp4") => Some("video/mp4"),
        _ => None,
    };
    if let Some(content_type) = content_type
        && res.status().is_success()
    {
        res.headers_mut()
            .insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
    }
    info!(
        %method,
        %path,
        status = res.status().as_u16(),
        range = range.as_deref().unwrap_or("-"),
        "request"
    );
    res
}

#[cfg(test)]
mod tests {
    use axum::routing::get;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    use super::*;

    async fn pending_response() -> &'static str {
        std::future::pending().await
    }

    #[tokio::test]
    async fn forced_drain_closes_a_stalled_connection() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let shutdown = CancellationToken::new();
        let server = spawn_server(
            listener,
            Router::new().route("/", get(pending_response)),
            shutdown.clone(),
            Duration::from_millis(20),
        )
        .unwrap();
        let mut client = tokio::net::TcpStream::connect(address).await.unwrap();
        client
            .write_all(b"GET / HTTP/1.1\r\nhost: localhost\r\n\r\n")
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(10)).await;
        shutdown.cancel();
        let result = tokio::time::timeout(Duration::from_secs(1), server)
            .await
            .expect("server must honor its forced drain")
            .expect("server thread result");
        result.unwrap();
        let mut byte = [0];
        let read = tokio::time::timeout(Duration::from_secs(1), client.read(&mut byte))
            .await
            .expect("stalled socket must close");
        assert!(
            matches!(read, Ok(0) | Err(_)),
            "socket stayed readable: {read:?}"
        );
    }

    #[tokio::test]
    async fn completed_response_drains_normally() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let shutdown = CancellationToken::new();
        let server = spawn_server(
            listener,
            Router::new().route("/", get(|| async { "ok" })),
            shutdown.clone(),
            Duration::from_millis(100),
        )
        .unwrap();
        let mut client = tokio::net::TcpStream::connect(address).await.unwrap();
        client
            .write_all(b"GET / HTTP/1.1\r\nhost: localhost\r\nconnection: close\r\n\r\n")
            .await
            .unwrap();
        let mut response = Vec::new();
        client.read_to_end(&mut response).await.unwrap();
        assert!(response.ends_with(b"ok"));
        shutdown.cancel();
        tokio::time::timeout(Duration::from_secs(1), server)
            .await
            .expect("server stops")
            .expect("server thread result")
            .unwrap();
    }
}
