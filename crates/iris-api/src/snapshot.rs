//! Snapshot proxy with HTTP digest authentication (RFC 7616, MD5).

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use axum::extract::{Path, State};
use axum::http::header;
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use futures_util::StreamExt as _;
use iris_core::id::CameraId;
use iris_core::time::WallUs;

use crate::error::ApiError;
use crate::state::{ApiState, SnapshotSource};

/// Largest snapshot accepted from a camera.
const MAX_SNAPSHOT_LEN: usize = 8 << 20;
/// How far before the end of a camera body the final end-of-image marker is
/// sought; cameras may pad or append trailers after it.
const JPEG_EOI_WINDOW: usize = 64 * 1024;
const SNAPSHOT_PATH: &str = "/cgi-bin/snapshot.cgi?channel=1";
const SNAPSHOT_TIMEOUT: Duration = Duration::from_secs(5);

static CNONCE_COUNTER: AtomicU64 = AtomicU64::new(0);

pub(crate) async fn snapshot(
    State(state): State<Arc<ApiState>>,
    Path(camera): Path<String>,
) -> Result<Response, ApiError> {
    let id = CameraId::try_new(&camera).map_err(|e| ApiError::BadRequest(e.to_string()))?;
    let source = state.snapshot_sources.get(&id).ok_or(ApiError::NotFound)?;
    // One camera request at a time per camera; the guard also releases the
    // slot when the client goes away mid-request.
    let _in_flight = state.stats.admit_snapshot(&id)?;
    let body = fetch_snapshot(source).await?;
    Ok(snapshot_response(body))
}

async fn fetch_snapshot(source: &SnapshotSource) -> Result<Bytes, ApiError> {
    fetch_snapshot_with_timeout(source, SNAPSHOT_TIMEOUT).await
}

async fn fetch_snapshot_with_timeout(
    source: &SnapshotSource,
    timeout: Duration,
) -> Result<Bytes, ApiError> {
    let url = snapshot_url(source);
    let client = reqwest::Client::builder()
        .danger_accept_invalid_certs(!source.tls_verify)
        .redirect(reqwest::redirect::Policy::none())
        .timeout(timeout)
        .build()
        .map_err(|error| redacted_upstream_error(&error))?;
    let first = client
        .get(&url)
        .send()
        .await
        .map_err(|error| redacted_upstream_error(&error))?;
    let response = if first.status().as_u16() == 401 {
        let challenge = first
            .headers()
            .get(header::WWW_AUTHENTICATE)
            .and_then(|v| v.to_str().ok())
            .ok_or_else(|| ApiError::Upstream("camera sent no digest challenge".into()))?;
        let authorization = digest::authorization(
            "GET",
            SNAPSHOT_PATH,
            &source.username,
            &source.password,
            challenge,
            &cnonce(),
            1,
        )
        .ok_or_else(|| ApiError::Upstream("unsupported digest challenge".into()))?;
        client
            .get(&url)
            .header(header::AUTHORIZATION, authorization)
            .send()
            .await
            .map_err(|error| redacted_upstream_error(&error))?
    } else {
        first
    };
    if !response.status().is_success() {
        return Err(ApiError::Upstream(format!(
            "camera returned {}",
            response.status()
        )));
    }
    if response
        .content_length()
        .is_some_and(|n| n > MAX_SNAPSHOT_LEN as u64)
    {
        return Err(ApiError::Upstream("snapshot too large".into()));
    }
    let body = collect_snapshot(response).await?;
    let Some(end) = jpeg_extent(&body) else {
        return Err(ApiError::Upstream("camera returned an invalid JPEG".into()));
    };
    Ok(body.slice(..end))
}

fn snapshot_response(body: Bytes) -> Response {
    (
        [
            (header::CONTENT_TYPE, "image/jpeg"),
            (header::CACHE_CONTROL, "no-store"),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
        ],
        body,
    )
        .into_response()
}

async fn collect_snapshot(response: reqwest::Response) -> Result<Bytes, ApiError> {
    let initial_capacity = response
        .content_length()
        .and_then(|len| usize::try_from(len).ok())
        .unwrap_or(0)
        .min(MAX_SNAPSHOT_LEN);
    let mut body = Vec::new();
    body.try_reserve_exact(initial_capacity)
        .map_err(|_| ApiError::Upstream("snapshot allocation failed".into()))?;
    let mut chunks = response.bytes_stream();
    while let Some(chunk) = chunks.next().await {
        let chunk = chunk.map_err(|error| redacted_upstream_error(&error))?;
        let Some(next_len) = body.len().checked_add(chunk.len()) else {
            return Err(ApiError::Upstream("snapshot too large".into()));
        };
        if next_len > MAX_SNAPSHOT_LEN {
            return Err(ApiError::Upstream("snapshot too large".into()));
        }
        body.try_reserve(chunk.len())
            .map_err(|_| ApiError::Upstream("snapshot allocation failed".into()))?;
        body.extend_from_slice(&chunk);
    }
    Ok(Bytes::from(body))
}

/// The length of the JPEG at the start of `body`: through its final
/// end-of-image marker (`FF D9`) within the last [`JPEG_EOI_WINDOW`] bytes.
/// `None` without a start-of-image marker and first segment marker
/// (`FF D8 FF`) or without that end marker. Bytes after the marker are
/// padding or vendor trailers and are not served.
fn jpeg_extent(body: &[u8]) -> Option<usize> {
    if !body.starts_with(&[0xff, 0xd8, 0xff]) {
        return None;
    }
    let window_start = body.len().saturating_sub(JPEG_EOI_WINDOW).max(3);
    body.get(window_start..)?
        .windows(2)
        .rposition(|pair| pair == [0xff, 0xd9])
        .map(|position| window_start + position + 2)
}

fn redacted_upstream_error(error: &reqwest::Error) -> ApiError {
    tracing::warn!(
        timeout = error.is_timeout(),
        connect = error.is_connect(),
        status = ?error.status(),
        "camera snapshot request failed"
    );
    ApiError::Upstream("snapshot request failed".into())
}

fn snapshot_url(source: &SnapshotSource) -> String {
    let scheme = if source.https { "https" } else { "http" };
    // Config supplies bare hosts; IPv6 needs brackets in an HTTP authority.
    if source.host.contains(':') {
        format!(
            "{scheme}://[{}]:{}{SNAPSHOT_PATH}",
            source.host, source.port
        )
    } else {
        format!("{scheme}://{}:{}{SNAPSHOT_PATH}", source.host, source.port)
    }
}

#[cfg(test)]
mod tests {
    use std::future::Future;
    use std::net::SocketAddr;

    use tokio::io::AsyncReadExt as _;
    use tokio::io::AsyncWriteExt as _;
    use tokio::net::TcpListener;
    use tokio::net::TcpStream;

    use super::*;

    fn source(address: SocketAddr) -> SnapshotSource {
        SnapshotSource {
            host: address.ip().to_string(),
            port: address.port(),
            https: false,
            tls_verify: true,
            username: String::new(),
            password: String::new(),
        }
    }

    async fn read_request(socket: &mut TcpStream) {
        let mut request = Vec::new();
        let mut chunk = [0_u8; 1024];
        while request.len() < 16 * 1024 {
            let read = socket.read(&mut chunk).await.expect("read request");
            if read == 0 {
                break;
            }
            request.extend_from_slice(&chunk[..read]);
            if request.windows(4).any(|window| window == b"\r\n\r\n") {
                break;
            }
        }
        assert!(
            request.windows(4).any(|window| window == b"\r\n\r\n"),
            "complete request headers"
        );
    }

    async fn test_server<Handler, HandlerFuture>(
        handler: Handler,
    ) -> (SnapshotSource, tokio::task::JoinHandle<()>)
    where
        Handler: FnOnce(TcpStream) -> HandlerFuture + Send + 'static,
        HandlerFuture: Future<Output = ()> + Send + 'static,
    {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind loopback server");
        let address = listener.local_addr().expect("loopback address");
        let task = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("accept request");
            read_request(&mut socket).await;
            handler(socket).await;
        });
        (source(address), task)
    }

    async fn serve_body(body: Vec<u8>, content_type: &str) -> SnapshotSource {
        let content_type = content_type.to_owned();
        let (source, _task) = test_server(move |mut socket| async move {
            let headers = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            socket
                .write_all(headers.as_bytes())
                .await
                .expect("write response headers");
            let _ = socket.write_all(&body).await;
        })
        .await;
        source
    }

    fn jpeg_with_len(len: usize) -> Vec<u8> {
        assert!(len >= 5);
        let mut body = vec![0_u8; len];
        body[..3].copy_from_slice(&[0xff, 0xd8, 0xff]);
        body[len - 2..].copy_from_slice(&[0xff, 0xd9]);
        body
    }

    #[test]
    fn the_jpeg_extent_ends_at_the_final_end_marker_in_the_tail_window() {
        let jpeg = jpeg_with_len(64);
        assert_eq!(jpeg_extent(&jpeg), Some(64));
        for tail in [
            &b"\r\n"[..],
            &[0_u8][..],
            &[0_u8; 3][..],
            &[0x5a_u8; 1024][..],
        ] {
            let mut padded = jpeg.clone();
            padded.extend_from_slice(tail);
            assert_eq!(
                jpeg_extent(&padded),
                Some(64),
                "{} trailing bytes",
                tail.len()
            );
        }
        // An embedded thumbnail's end marker precedes the image's own.
        let mut nested = jpeg_with_len(32);
        nested.extend_from_slice(&jpeg_with_len(32));
        nested.extend_from_slice(b"\0\0");
        assert_eq!(jpeg_extent(&nested), Some(64));
    }

    #[test]
    fn bodies_without_both_jpeg_markers_in_bounds_are_rejected() {
        assert_eq!(jpeg_extent(b"<html>not a jpeg</html>"), None);
        assert_eq!(jpeg_extent(&[]), None);
        assert_eq!(jpeg_extent(&[0xff, 0xd8]), None);
        assert_eq!(
            jpeg_extent(&[0xff, 0xd8, 0xff, 0xd9]),
            None,
            "the end marker cannot overlap the start marker"
        );
        let mut missing_start = jpeg_with_len(64);
        missing_start[2] = 0x00;
        assert_eq!(
            jpeg_extent(&missing_start),
            None,
            "missing first segment marker"
        );
        let mut missing_end = jpeg_with_len(64);
        missing_end[62] = 0x00;
        assert_eq!(jpeg_extent(&missing_end), None);
        // The marker and everything after it fill exactly the tail window.
        let mut distant = jpeg_with_len(64);
        distant.extend(std::iter::repeat_n(0_u8, JPEG_EOI_WINDOW - 2));
        assert_eq!(
            jpeg_extent(&distant),
            Some(64),
            "an end marker just inside the window is found"
        );
        distant.push(0);
        assert_eq!(
            jpeg_extent(&distant),
            None,
            "an end marker more than 64 KiB before the end is rejected"
        );
    }

    #[tokio::test]
    async fn padded_snapshots_are_served_through_their_end_marker() {
        let mut body = jpeg_with_len(64);
        body.extend_from_slice(b"\r\n\0\0");
        let source = serve_body(body, "image/jpeg").await;
        let served = fetch_snapshot(&source).await.expect("padding is accepted");
        assert_eq!(served.len(), 64);
        assert!(served.ends_with(&[0xff, 0xd9]));
    }

    fn upstream_text(error: ApiError) -> String {
        match error {
            ApiError::Upstream(text) => text,
            other => panic!("expected upstream error, got {other}"),
        }
    }

    #[test]
    fn snapshot_urls_bracket_ipv6_and_preserve_other_hosts() {
        for host in ["2001:db8::1", "192.0.2.7", "camera.example"] {
            for (https, scheme, port) in [(false, "http", 8080), (true, "https", 8443)] {
                let source = SnapshotSource {
                    host: host.into(),
                    port,
                    https,
                    tls_verify: true,
                    username: String::new(),
                    password: String::new(),
                };
                let authority = if host.contains(':') {
                    format!("[{host}]")
                } else {
                    host.into()
                };
                let url = snapshot_url(&source);
                assert_eq!(url, format!("{scheme}://{authority}:{port}{SNAPSHOT_PATH}"));
                let request = reqwest::Client::new().get(url).build().unwrap();
                assert_eq!(request.url().scheme(), scheme);
                assert_eq!(request.url().port(), Some(port));
                assert_eq!(request.url().path(), "/cgi-bin/snapshot.cgi");
                assert_eq!(request.url().query(), Some("channel=1"));
            }
        }
    }

    #[tokio::test]
    async fn accepts_exact_limit_and_sets_fixed_safe_headers() {
        let source = serve_body(jpeg_with_len(MAX_SNAPSHOT_LEN), "text/html").await;
        let body = fetch_snapshot(&source).await.expect("exact limit succeeds");
        assert_eq!(body.len(), MAX_SNAPSHOT_LEN);

        let response = snapshot_response(body);
        assert_eq!(response.headers()[header::CONTENT_TYPE], "image/jpeg");
        assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
        assert_eq!(
            response.headers()[header::X_CONTENT_TYPE_OPTIONS],
            "nosniff"
        );
    }

    #[tokio::test]
    async fn rejects_declared_limit_plus_one_before_reading_the_body() {
        let (source, server) = test_server(|mut socket| async move {
            let headers = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                MAX_SNAPSHOT_LEN + 1
            );
            socket
                .write_all(headers.as_bytes())
                .await
                .expect("write oversized headers");
        })
        .await;
        assert_eq!(
            upstream_text(fetch_snapshot(&source).await.expect_err("must reject")),
            "snapshot too large"
        );
        server.await.expect("server task");
    }

    #[tokio::test]
    async fn rejects_limit_plus_one_from_a_chunked_body_without_a_length() {
        let body = jpeg_with_len(MAX_SNAPSHOT_LEN + 1);
        let (source, server) = test_server(move |mut socket| async move {
            socket
                .write_all(
                    b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n",
                )
                .await
                .expect("write chunked headers");
            for chunk in body.chunks(64 * 1024) {
                let header = format!("{:x}\r\n", chunk.len());
                if socket.write_all(header.as_bytes()).await.is_err()
                    || socket.write_all(chunk).await.is_err()
                    || socket.write_all(b"\r\n").await.is_err()
                {
                    return;
                }
            }
            let _ = socket.write_all(b"0\r\n\r\n").await;
        })
        .await;
        assert_eq!(
            upstream_text(fetch_snapshot(&source).await.expect_err("must reject")),
            "snapshot too large"
        );
        server.await.expect("server task");
    }

    #[tokio::test]
    async fn concurrent_oversized_snapshots_are_each_rejected_at_the_header() {
        const REQUESTS: usize = 4;
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind loopback server");
        let source = source(listener.local_addr().expect("loopback address"));
        let server = tokio::spawn(async move {
            let mut tasks = Vec::new();
            for _ in 0..REQUESTS {
                let (mut socket, _) = listener.accept().await.expect("accept request");
                tasks.push(tokio::spawn(async move {
                    read_request(&mut socket).await;
                    let headers = format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        MAX_SNAPSHOT_LEN + 1
                    );
                    socket
                        .write_all(headers.as_bytes())
                        .await
                        .expect("write oversized headers");
                }));
            }
            for task in tasks {
                task.await.expect("connection task");
            }
        });
        let results =
            futures_util::future::join_all((0..REQUESTS).map(|_| fetch_snapshot(&source))).await;
        for result in results {
            assert_eq!(
                upstream_text(result.expect_err("must reject")),
                "snapshot too large"
            );
        }
        server.await.expect("server task");
    }

    #[tokio::test]
    async fn rejects_non_jpeg_bodies_regardless_of_upstream_media_type() {
        let source = serve_body(
            b"<script>document.body.textContent='active'</script>".to_vec(),
            "text/html",
        )
        .await;
        assert_eq!(
            upstream_text(fetch_snapshot(&source).await.expect_err("must reject")),
            "camera returned an invalid JPEG"
        );
    }

    #[tokio::test]
    async fn truncated_body_failure_is_redacted() {
        let (source, server) = test_server(|mut socket| async move {
            socket
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Length: 10\r\nConnection: close\r\n\r\n\xff\xd8\xff\xd9",
                )
                .await
                .expect("write truncated response");
        })
        .await;
        let text = upstream_text(fetch_snapshot(&source).await.expect_err("must reject"));
        assert_eq!(text, "snapshot request failed");
        assert!(!text.contains(&source.host));
        assert!(!text.contains(&source.port.to_string()));
        assert!(!text.contains("snapshot.cgi"));
        server.await.expect("server task");
    }

    #[tokio::test]
    async fn slow_body_hits_the_request_deadline_with_a_redacted_error() {
        let (source, server) = test_server(|mut socket| async move {
            socket
                .write_all(
                    b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n2\r\n\xff\xd8\r\n",
                )
                .await
                .expect("write first chunk");
            tokio::time::sleep(Duration::from_secs(1)).await;
            let _ = socket.write_all(b"2\r\n\xff\xd9\r\n0\r\n\r\n").await;
        })
        .await;
        let text = upstream_text(
            fetch_snapshot_with_timeout(&source, Duration::from_millis(50))
                .await
                .expect_err("slow body must time out"),
        );
        assert_eq!(text, "snapshot request failed");
        server.abort();
    }

    #[tokio::test]
    async fn a_slow_snapshot_can_be_cancelled_without_waiting_for_the_camera_timeout() {
        let (source, server) = test_server(|mut socket| async move {
            socket
                .write_all(
                    b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n2\r\n\xff\xd8\r\n",
                )
                .await
                .expect("write first chunk");
            tokio::time::sleep(Duration::from_secs(1)).await;
            let _ = socket.write_all(b"2\r\n\xff\xd9\r\n0\r\n\r\n").await;
        })
        .await;
        let cancelled =
            tokio::time::timeout(Duration::from_millis(50), fetch_snapshot(&source)).await;
        assert!(cancelled.is_err(), "caller deadline cancels the body read");
        server.abort();
    }

    #[tokio::test]
    async fn redirects_are_never_followed() {
        for kind in [
            "same-origin",
            "cross-host",
            "cross-port",
            "cross-scheme",
            "loop",
        ] {
            let target = TcpListener::bind("127.0.0.1:0")
                .await
                .expect("bind redirect target");
            let target_address = target.local_addr().expect("target address");
            let source_listener = TcpListener::bind("127.0.0.1:0")
                .await
                .expect("bind redirect source");
            let source_address = source_listener.local_addr().expect("source address");
            let location = match kind {
                "same-origin" => "/another-snapshot".to_owned(),
                "cross-host" => format!("http://localhost:{}/target", source_address.port()),
                "cross-port" => format!("http://127.0.0.1:{}/target", target_address.port()),
                "cross-scheme" => {
                    format!("https://127.0.0.1:{}/target", source_address.port())
                }
                "loop" => format!("http://{source_address}{SNAPSHOT_PATH}"),
                _ => unreachable!(),
            };
            let source_task = tokio::spawn(async move {
                let (mut socket, _) = source_listener.accept().await.expect("accept redirect");
                read_request(&mut socket).await;
                let response = format!(
                    "HTTP/1.1 302 Found\r\nLocation: {location}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                );
                socket
                    .write_all(response.as_bytes())
                    .await
                    .expect("write redirect");
                drop(socket);
                tokio::time::timeout(Duration::from_millis(150), source_listener.accept())
                    .await
                    .is_ok()
            });
            let target_task = tokio::spawn(async move {
                tokio::time::timeout(Duration::from_millis(150), target.accept())
                    .await
                    .is_ok()
            });

            let error = fetch_snapshot(&source(source_address))
                .await
                .expect_err("redirect is not a snapshot response");
            assert_eq!(upstream_text(error), "camera returned 302 Found");
            assert!(!source_task.await.expect("redirect source task"), "{kind}");
            assert!(!target_task.await.expect("redirect target task"), "{kind}");
        }
    }
}

fn cnonce() -> String {
    let n = CNONCE_COUNTER.fetch_add(1, Ordering::Relaxed);
    let micros = WallUs::now().get() as u64;
    hex::encode((micros ^ n.rotate_left(32)).to_be_bytes())
}

/// HTTP digest (RFC 7616) with the MD5 algorithm, as IP cameras implement it.
pub(crate) mod digest {
    use std::fmt::Write as _;

    use md5::{Digest as _, Md5};

    /// The parts of a `WWW-Authenticate: Digest …` header irisd uses.
    #[derive(Clone, Debug, PartialEq, Eq)]
    pub(crate) struct Challenge {
        pub(crate) realm: String,
        pub(crate) nonce: String,
        pub(crate) qop_auth: bool,
        pub(crate) opaque: Option<String>,
        pub(crate) algorithm: Option<String>,
    }

    /// Parses a digest challenge; `None` for other schemes or missing fields.
    pub(crate) fn parse_challenge(header: &str) -> Option<Challenge> {
        let rest = header.trim().strip_prefix("Digest")?.trim_start();
        let mut realm = None;
        let mut nonce = None;
        let mut qop_auth = false;
        let mut opaque = None;
        let mut algorithm = None;
        for (key, value) in parameters(rest) {
            match key.to_ascii_lowercase().as_str() {
                "realm" => realm = Some(value),
                "nonce" => nonce = Some(value),
                "qop" => qop_auth = value.split(',').any(|q| q.trim() == "auth"),
                "opaque" => opaque = Some(value),
                "algorithm" => algorithm = Some(value),
                _ => {}
            }
        }
        Some(Challenge {
            realm: realm?,
            nonce: nonce?,
            qop_auth,
            opaque,
            algorithm,
        })
    }

    /// Splits `key=value, key="quoted, value"` pairs.
    fn parameters(input: &str) -> Vec<(String, String)> {
        let mut out = Vec::new();
        let mut rest = input;
        while !rest.is_empty() {
            let rest_trimmed = rest.trim_start_matches([' ', ',']);
            let Some(eq) = rest_trimmed.find('=') else {
                break;
            };
            let key = rest_trimmed[..eq].trim().to_owned();
            let after = &rest_trimmed[eq + 1..];
            let (value, remainder) = if let Some(quoted) = after.strip_prefix('"') {
                match quoted.find('"') {
                    Some(end) => (quoted[..end].to_owned(), &quoted[end + 1..]),
                    None => (quoted.to_owned(), ""),
                }
            } else {
                let end = after.find(',').unwrap_or(after.len());
                (after[..end].trim().to_owned(), &after[end..])
            };
            out.push((key, value));
            rest = remainder;
        }
        out
    }

    fn md5_hex(input: &str) -> String {
        hex::encode(Md5::digest(input.as_bytes()))
    }

    /// Builds the `Authorization` header value for `method` on `uri`.
    pub(crate) fn authorization(
        method: &str,
        uri: &str,
        username: &str,
        password: &str,
        challenge_header: &str,
        cnonce: &str,
        nc: u32,
    ) -> Option<String> {
        let c = parse_challenge(challenge_header)?;
        if c.algorithm
            .as_deref()
            .is_some_and(|a| !a.eq_ignore_ascii_case("MD5"))
        {
            return None;
        }
        let ha1 = md5_hex(&format!("{username}:{}:{password}", c.realm));
        let ha2 = md5_hex(&format!("{method}:{uri}"));
        let nc_hex = format!("{nc:08x}");
        let response = if c.qop_auth {
            md5_hex(&format!("{ha1}:{}:{nc_hex}:{cnonce}:auth:{ha2}", c.nonce))
        } else {
            md5_hex(&format!("{ha1}:{}:{ha2}", c.nonce))
        };
        let mut value = format!(
            "Digest username=\"{username}\", realm=\"{}\", nonce=\"{}\", uri=\"{uri}\", response=\"{response}\"",
            c.realm, c.nonce
        );
        if c.qop_auth {
            let _ = write!(value, ", qop=auth, nc={nc_hex}, cnonce=\"{cnonce}\"");
        }
        if let Some(opaque) = &c.opaque {
            let _ = write!(value, ", opaque=\"{opaque}\"");
        }
        if c.algorithm.is_some() {
            value.push_str(", algorithm=MD5");
        }
        Some(value)
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        const RFC2617: &str = "Digest realm=\"testrealm@host.com\", qop=\"auth,auth-int\", nonce=\"dcd98b7102dd2f0e8b11d0f600bfb0c093\", opaque=\"5ccc069c403ebaf9f0171e9517f40e41\"";

        #[test]
        fn parses_quoted_and_bare_parameters() {
            let c = parse_challenge(
                "Digest realm=\"Login to X\", nonce=abc123, qop=auth, algorithm=MD5",
            )
            .unwrap();
            assert_eq!(c.realm, "Login to X");
            assert_eq!(c.nonce, "abc123");
            assert!(c.qop_auth);
            assert_eq!(c.algorithm.as_deref(), Some("MD5"));
            assert!(parse_challenge("Basic realm=\"x\"").is_none());
        }

        #[test]
        fn reproduces_the_rfc_2617_example() {
            let header = authorization(
                "GET",
                "/dir/index.html",
                "Mufasa",
                "Circle Of Life",
                RFC2617,
                "0a4f113b",
                1,
            )
            .unwrap();
            assert!(
                header.contains("response=\"6629fae49393a05397450978507c4ef1\""),
                "{header}"
            );
            assert!(header.contains("qop=auth, nc=00000001, cnonce=\"0a4f113b\""));
            assert!(header.contains("opaque=\"5ccc069c403ebaf9f0171e9517f40e41\""));
        }
    }
}
