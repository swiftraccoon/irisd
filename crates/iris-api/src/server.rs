//! The connection loop: HTTP/1.1 with bounded connection admission, a
//! request-head deadline that also closes idle kept-alive connections, and a
//! write-progress deadline for peers that stop reading.
//!
//! The loop serves HTTP/1.1 only. Every client in this repository (the viewer,
//! the tools, and the HLS players the API is tested with) speaks HTTP/1.1 to
//! the plain-TCP listener, and cleartext HTTP/2 would add 200 streams per
//! connection and a protocol sniff that no request-head deadline covers.

use std::future::Future as _;
use std::io;
use std::pin::{Pin, pin};
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use axum::Router;
use hyper::server::conn::http1;
use hyper_util::rt::{TokioIo, TokioTimer};
use hyper_util::service::TowerToHyperService;
use iris_core::config::DEFAULT_API_CONNECTIONS;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::TcpListener;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio::time::Sleep;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

/// Pause after an accept failure that does not concern one connection, such as
/// descriptor exhaustion, so the loop does not spin.
const ACCEPT_ERROR_PAUSE: Duration = Duration::from_secs(1);

/// Connection-level limits applied by [`serve_listener`].
///
/// ```
/// use std::time::Duration;
///
/// let mut limits = iris_api::ServeLimits::default();
/// assert_eq!(limits.max_connections, 256);
/// limits.write_stall_timeout = Duration::from_secs(10);
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ServeLimits {
    /// Connections served at once, upgraded WebSocket connections included. A slot is
    /// taken before each `accept`, so further clients wait in the kernel's
    /// listen backlog instead of holding descriptors. Values below one are
    /// raised to one and values above `tokio::sync::Semaphore::MAX_PERMITS`
    /// lowered to it.
    pub max_connections: usize,
    /// Longest wait for a complete request head, counted from the moment the
    /// connection is ready for one; an idle kept-alive connection closes after
    /// it too.
    pub header_read_timeout: Duration,
    /// Longest a pending response write may make no progress before the
    /// connection is closed. Any accepted byte restarts it, so a slow reader
    /// is kept; a dropped response releases its media admission.
    pub write_stall_timeout: Duration,
}

impl Default for ServeLimits {
    /// [`DEFAULT_API_CONNECTIONS`] connections, a 10 s request-head deadline,
    /// and a 30 s write-stall deadline.
    fn default() -> Self {
        Self {
            max_connections: DEFAULT_API_CONNECTIONS,
            header_read_timeout: Duration::from_secs(10),
            write_stall_timeout: Duration::from_secs(30),
        }
    }
}

/// Serves `router` on an already-bound listener until `shutdown` is cancelled.
///
/// Cancellation stops accepting, lets every connection finish its in-flight
/// response before closing, and returns once all connections have closed.
/// Upgraded WebSocket sessions end through the shutdown token they observe.
///
/// # Errors
///
/// Only the error of reading the listener's local address. Accept errors are
/// logged and retried: per-connection failures at once, resource exhaustion
/// after a one-second pause.
#[expect(
    clippy::missing_panics_doc,
    reason = "the expect names a semaphore this function owns and never closes"
)]
pub async fn serve_listener(
    listener: TcpListener,
    router: Router,
    limits: ServeLimits,
    shutdown: CancellationToken,
) -> io::Result<()> {
    let listen = listener.local_addr()?;
    tracing::info!(
        %listen,
        max_connections = limits.max_connections,
        "API listening"
    );
    let admission = Arc::new(Semaphore::new(
        limits.max_connections.clamp(1, Semaphore::MAX_PERMITS),
    ));
    let connections = TaskTracker::new();
    let mut http = http1::Builder::new();
    http.timer(TokioTimer::new())
        .header_read_timeout(limits.header_read_timeout);
    loop {
        let slot = tokio::select! {
            () = shutdown.cancelled() => break,
            slot = Arc::clone(&admission).acquire_owned() => {
                slot.expect("the connection admission semaphore is never closed")
            }
        };
        let stream = tokio::select! {
            () = shutdown.cancelled() => break,
            accepted = listener.accept() => match accepted {
                Ok((stream, _)) => stream,
                Err(error) => {
                    if !is_connection_error(&error) {
                        tracing::error!(%error, "API accept failed; pausing");
                        tokio::select! {
                            () = shutdown.cancelled() => break,
                            () = tokio::time::sleep(ACCEPT_ERROR_PAUSE) => {}
                        }
                    }
                    continue;
                }
            },
        };
        let io = TokioIo::new(AdmittedStream::new(
            stream,
            slot,
            limits.write_stall_timeout,
        ));
        let connection = http
            .serve_connection(io, TowerToHyperService::new(router.clone()))
            .with_upgrades();
        let shutdown = shutdown.clone();
        connections.spawn(async move {
            let mut connection = pin!(connection);
            let mut draining = false;
            loop {
                tokio::select! {
                    result = connection.as_mut() => {
                        if let Err(error) = result {
                            tracing::debug!(%error, "API connection ended with an error");
                        }
                        break;
                    }
                    () = shutdown.cancelled(), if !draining => {
                        draining = true;
                        connection.as_mut().graceful_shutdown();
                    }
                }
            }
        });
    }
    drop(listener);
    connections.close();
    connections.wait().await;
    Ok(())
}

/// Errors that end one pending connection rather than the listener: the
/// network errors Linux's `accept(2)` reports for the connection it would have
/// returned, which are retried at once like `EAGAIN`.
fn is_connection_error(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::ConnectionRefused
            | io::ErrorKind::ConnectionAborted
            | io::ErrorKind::ConnectionReset
            | io::ErrorKind::NetworkDown
            | io::ErrorKind::NetworkUnreachable
            | io::ErrorKind::HostUnreachable
    )
}

/// An accepted connection that holds its admission slot for its whole life,
/// including after a WebSocket upgrade, and fails a write that makes no
/// progress for the stall deadline.
struct AdmittedStream<Stream> {
    stream: Stream,
    _slot: OwnedSemaphorePermit,
    write_stall_timeout: Duration,
    stalled: Option<Pin<Box<Sleep>>>,
}

impl<Stream> AdmittedStream<Stream> {
    fn new(stream: Stream, slot: OwnedSemaphorePermit, write_stall_timeout: Duration) -> Self {
        Self {
            stream,
            _slot: slot,
            write_stall_timeout,
            stalled: None,
        }
    }

    /// Restarts the stall deadline when a write operation completes. A pending
    /// one starts the deadline, or fails once it has elapsed without progress.
    fn observe<Output>(
        &mut self,
        cx: &mut Context<'_>,
        poll: Poll<io::Result<Output>>,
    ) -> Poll<io::Result<Output>> {
        if poll.is_ready() {
            self.stalled = None;
            return poll;
        }
        let timeout = self.write_stall_timeout;
        let stalled = self
            .stalled
            .get_or_insert_with(|| Box::pin(tokio::time::sleep(timeout)));
        if stalled.as_mut().poll(cx).is_pending() {
            return Poll::Pending;
        }
        self.stalled = None;
        Poll::Ready(Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "the peer accepted no response bytes within the write deadline",
        )))
    }
}

impl<Stream: AsyncRead + Unpin> AsyncRead for AdmittedStream<Stream> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().stream).poll_read(cx, buf)
    }
}

impl<Stream: AsyncWrite + Unpin> AsyncWrite for AdmittedStream<Stream> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        let poll = Pin::new(&mut this.stream).poll_write(cx, buf);
        this.observe(cx, poll)
    }

    fn poll_write_vectored(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        let poll = Pin::new(&mut this.stream).poll_write_vectored(cx, bufs);
        this.observe(cx, poll)
    }

    fn is_write_vectored(&self) -> bool {
        self.stream.is_write_vectored()
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        let poll = Pin::new(&mut this.stream).poll_flush(cx);
        this.observe(cx, poll)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        let poll = Pin::new(&mut this.stream).poll_shutdown(cx);
        this.observe(cx, poll)
    }
}

#[cfg(test)]
mod tests {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    use super::*;

    fn slot() -> OwnedSemaphorePermit {
        Arc::new(Semaphore::new(1))
            .try_acquire_owned()
            .expect("one free slot")
    }

    #[tokio::test(start_paused = true)]
    async fn a_write_without_progress_fails_after_the_stall_deadline() {
        let (server, _client) = tokio::io::duplex(1024);
        let mut stream = AdmittedStream::new(server, slot(), Duration::from_millis(300));
        let started = tokio::time::Instant::now();
        let error = stream
            .write_all(&[0x55; 4096])
            .await
            .expect_err("a reader that never reads stalls the write");
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        let waited = started.elapsed();
        assert!(
            waited >= Duration::from_millis(300) && waited < Duration::from_millis(400),
            "{waited:?}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_slow_reader_that_keeps_reading_is_never_cut() {
        let (server, mut client) = tokio::io::duplex(1024);
        let mut stream = AdmittedStream::new(server, slot(), Duration::from_millis(300));
        let reader = tokio::spawn(async move {
            let mut received = Vec::new();
            let mut chunk = [0_u8; 512];
            loop {
                // Each pause is shorter than the deadline, but the whole
                // transfer takes many deadlines.
                tokio::time::sleep(Duration::from_millis(200)).await;
                match client.read(&mut chunk).await.expect("read") {
                    0 => break received,
                    read => received.extend_from_slice(&chunk[..read]),
                }
            }
        });
        let payload: Vec<u8> = (0..16_384_u32).map(|index| index as u8).collect();
        let started = tokio::time::Instant::now();
        stream
            .write_all(&payload)
            .await
            .expect("progress restarts the deadline");
        stream.shutdown().await.expect("shutdown");
        drop(stream);
        assert!(started.elapsed() > Duration::from_secs(3));
        assert_eq!(reader.await.expect("reader task"), payload);
    }

    #[tokio::test]
    async fn the_slot_is_held_until_the_stream_drops() {
        let semaphore = Arc::new(Semaphore::new(1));
        let (server, _client) = tokio::io::duplex(64);
        let stream = AdmittedStream::new(
            server,
            Arc::clone(&semaphore)
                .try_acquire_owned()
                .expect("free slot"),
            Duration::from_secs(1),
        );
        assert_eq!(semaphore.available_permits(), 0);
        drop(stream);
        assert_eq!(semaphore.available_permits(), 1);
    }
}
