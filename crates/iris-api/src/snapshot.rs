//! Snapshot proxy with HTTP digest authentication (RFC 7616, MD5): the Dahua
//! CGI endpoint, or the URL a camera's ONVIF Media service names for its
//! first profile, resolved on first use and kept for [`ONVIF_URL_TTL`].

use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::extract::{Path, State};
use axum::http::header;
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use futures_util::StreamExt as _;
use iris_core::config::ResolvedSnapshot;
use iris_core::id::CameraId;
use iris_events::CameraEndpoint;
use iris_events::digest::{self, DigestAuth};
use url::Url;

use crate::error::ApiError;
use crate::state::{ApiState, SnapshotSource};

/// Largest snapshot accepted from a camera.
const MAX_SNAPSHOT_LEN: usize = 8 << 20;
/// How far before the end of a camera body the final end-of-image marker is
/// sought; cameras may pad or append trailers after it.
const JPEG_EOI_WINDOW: usize = 64 * 1024;
const SNAPSHOT_PATH: &str = "/cgi-bin/snapshot.cgi?channel=1";
const SNAPSHOT_TIMEOUT: Duration = Duration::from_secs(5);
/// How long a resolved ONVIF snapshot URL is reused before the Media service is
/// asked again; a failed fetch forgets it at once.
const ONVIF_URL_TTL: Duration = Duration::from_secs(3600);

pub(crate) async fn snapshot(
    State(state): State<Arc<ApiState>>,
    Path(camera): Path<String>,
) -> Result<Response, ApiError> {
    let id = CameraId::try_new(&camera).map_err(|e| ApiError::BadRequest(e.to_string()))?;
    let source = state
        .snapshot_sources
        .read()
        .expect("snapshot registry")
        .get(&id)
        .cloned()
        .ok_or(ApiError::NotFound)?;
    // One camera request at a time per camera; the guard also releases the
    // slot when the client goes away mid-request.
    let _in_flight = state.stats.admit_snapshot(&id)?;
    let body = fetch_by_method(&id, &source, ONVIF_URL_TTL).await?;
    Ok(snapshot_response(body))
}

/// Fetches by the camera's method: the Dahua CGI endpoint, or the URL the ONVIF
/// Media service names, reused for `url_lifetime` and forgotten by a failed
/// fetch so the next request resolves it again.
async fn fetch_by_method(
    id: &CameraId,
    source: &SnapshotSource,
    url_lifetime: Duration,
) -> Result<Bytes, ApiError> {
    match source.method {
        ResolvedSnapshot::DahuaCgi => fetch_snapshot(id, source).await,
        ResolvedSnapshot::Onvif => {
            let url = onvif_snapshot_url(id, source, url_lifetime).await?;
            let digest_uri = match url.query() {
                Some(query) => format!("{}?{query}", url.path()),
                None => url.path().to_owned(),
            };
            let fetched = fetch_snapshot_with_timeout(
                id,
                source,
                url.as_str(),
                &digest_uri,
                SNAPSHOT_TIMEOUT,
            )
            .await;
            if fetched.is_err() {
                *source.resolved.lock().expect("snapshot url cache") = None;
            }
            fetched
        }
    }
}

/// The ONVIF snapshot URL of `source`: the cached one while it is younger than
/// `lifetime` ([`ONVIF_URL_TTL`] in service), else resolved through the
/// camera's Media service, with the failure's kind, never camera text, in the
/// log.
async fn onvif_snapshot_url(
    id: &CameraId,
    source: &SnapshotSource,
    lifetime: Duration,
) -> Result<Url, ApiError> {
    let cached = source.resolved.lock().expect("snapshot url cache").clone();
    if let Some((url, resolved_at)) = cached
        && resolved_at.elapsed() < lifetime
    {
        return Ok(url);
    }
    let endpoint = CameraEndpoint {
        camera: id.clone(),
        host: source.host.clone(),
        http_port: source.port,
        https: source.https,
        tls_verify: source.tls_verify,
        username: source.username.clone(),
        password: source.password.clone(),
    };
    let url = iris_events::snapshot_url(&endpoint)
        .await
        .map_err(|error| {
            tracing::debug!(
                camera = %id,
                failure = error.kind(),
                "ONVIF snapshot URL could not be resolved"
            );
            ApiError::Upstream("snapshot URL could not be resolved".into())
        })?;
    *source.resolved.lock().expect("snapshot url cache") = Some((url.clone(), Instant::now()));
    Ok(url)
}

/// The Dahua CGI snapshot of `source`.
async fn fetch_snapshot(id: &CameraId, source: &SnapshotSource) -> Result<Bytes, ApiError> {
    fetch_snapshot_with_timeout(
        id,
        source,
        &snapshot_url(source),
        SNAPSHOT_PATH,
        SNAPSHOT_TIMEOUT,
    )
    .await
}

/// Fetches `url` from the camera with `source`'s credentials; `digest_uri` is
/// the request target a digest challenge is answered for. The first `Digest`
/// challenge irisd can answer (RFC 7616, MD5) among the `WWW-Authenticate`
/// headers is used, as the event feed does.
async fn fetch_snapshot_with_timeout(
    id: &CameraId,
    source: &SnapshotSource,
    url: &str,
    digest_uri: &str,
    timeout: Duration,
) -> Result<Bytes, ApiError> {
    let client = reqwest::Client::builder()
        .danger_accept_invalid_certs(!source.tls_verify)
        .redirect(reqwest::redirect::Policy::none())
        .no_proxy()
        .timeout(timeout)
        .build()
        .map_err(|error| redacted_upstream_error(id, error))?;
    let first = client
        .get(url)
        .send()
        .await
        .map_err(|error| redacted_upstream_error(id, error))?;
    let response = if first.status().as_u16() == 401 {
        let challenge = first
            .headers()
            .get_all(header::WWW_AUTHENTICATE)
            .iter()
            .filter_map(|value| value.to_str().ok())
            .find_map(digest::parse_challenge)
            .ok_or_else(|| {
                ApiError::Upstream("camera sent no supported digest challenge".into())
            })?;
        let mut auth = DigestAuth::new(&source.username, &source.password);
        auth.set_challenge(challenge);
        let authorization = auth
            .authorization("GET", digest_uri)
            .expect("a challenge was set");
        client
            .get(url)
            .header(header::AUTHORIZATION, authorization)
            .send()
            .await
            .map_err(|error| redacted_upstream_error(id, error))?
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
    let body = collect_snapshot(id, response).await?;
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

async fn collect_snapshot(id: &CameraId, response: reqwest::Response) -> Result<Bytes, ApiError> {
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
        let chunk = chunk.map_err(|error| redacted_upstream_error(id, error))?;
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

/// Logs a camera request failure with its cause but without the camera URL,
/// which carries the configured host, and answers a fixed body.
fn redacted_upstream_error(id: &CameraId, error: reqwest::Error) -> ApiError {
    tracing::warn!(
        camera = %id,
        error = ?error.without_url(),
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
    use std::sync::atomic::Ordering;

    use tokio::io::AsyncReadExt as _;
    use tokio::io::AsyncWriteExt as _;
    use tokio::net::TcpListener;
    use tokio::net::TcpStream;

    use super::*;

    fn source(address: SocketAddr) -> SnapshotSource {
        SnapshotSource::new(
            address.ip().to_string(),
            address.port(),
            false,
            true,
            String::new(),
            String::new(),
            ResolvedSnapshot::DahuaCgi,
        )
    }

    fn cam1() -> CameraId {
        CameraId::try_new("cam1").unwrap()
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

    /// A loopback ONVIF camera: SOAP answers for the device and media calls,
    /// every advertised URL on another host, and the snapshot at the rebased
    /// path; `fail` makes the snapshot answer 500. Records every request line.
    async fn onvif_fixture() -> (
        SnapshotSource,
        Arc<std::sync::Mutex<Vec<String>>>,
        Arc<std::sync::atomic::AtomicBool>,
    ) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let fail = Arc::new(std::sync::atomic::AtomicBool::new(false));
        tokio::spawn({
            let seen = Arc::clone(&seen);
            let fail = Arc::clone(&fail);
            async move {
                loop {
                    let Ok((mut socket, _)) = listener.accept().await else {
                        return;
                    };
                    let Some(head) = read_head_and_body(&mut socket).await else {
                        continue;
                    };
                    let first_line = head.lines().next().unwrap_or_default().to_owned();
                    seen.lock().unwrap().push(first_line.clone());
                    let (status, content_type, body) = if first_line.starts_with("GET ") {
                        if fail.load(Ordering::Relaxed) {
                            ("500 Internal Server Error", "text/plain", b"busy".to_vec())
                        } else {
                            ("200 OK", "image/jpeg", jpeg_with_len(64))
                        }
                    } else {
                        let reply =
                            soap_reply(&head.to_ascii_lowercase(), "http://203.0.113.9:8080");
                        ("200 OK", "application/soap+xml", reply.into_bytes())
                    };
                    let headers = format!(
                        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    );
                    let _ = socket.write_all(headers.as_bytes()).await;
                    let _ = socket.write_all(&body).await;
                }
            }
        });
        let mut source = source(address);
        source.method = ResolvedSnapshot::Onvif;
        (source, seen, fail)
    }

    /// Reads one request, head and body, and returns the head.
    async fn read_head_and_body(socket: &mut TcpStream) -> Option<String> {
        let mut request = Vec::new();
        let mut chunk = [0_u8; 4096];
        loop {
            match socket.read(&mut chunk).await {
                Ok(0) | Err(_) => return None,
                Ok(read) => request.extend_from_slice(&chunk[..read]),
            }
            let Some(end) = request.windows(4).position(|window| window == b"\r\n\r\n") else {
                continue;
            };
            let head = String::from_utf8_lossy(&request[..end + 4]).to_string();
            let body_len: usize = head
                .lines()
                .find_map(|line| {
                    line.to_ascii_lowercase()
                        .strip_prefix("content-length:")
                        .map(str::to_owned)
                })
                .map_or(0, |value| value.trim().parse().unwrap_or(0));
            if request.len() >= end + 4 + body_len {
                return Some(head);
            }
        }
    }

    fn soap_reply(head_lower: &str, advertised: &str) -> String {
        const ENVELOPE: &str = r#"<s:Envelope xmlns:s="http://www.w3.org/2003/05/soap-envelope" xmlns:tds="http://www.onvif.org/ver10/device/wsdl" xmlns:trt="http://www.onvif.org/ver10/media/wsdl" xmlns:tt="http://www.onvif.org/ver10/schema"><s:Body>"#;
        let body = if head_lower.contains("getsystemdateandtime") {
            "<tds:GetSystemDateAndTimeResponse><tds:SystemDateAndTime><tt:UTCDateTime><tt:Time><tt:Hour>0</tt:Hour><tt:Minute>0</tt:Minute><tt:Second>0</tt:Second></tt:Time><tt:Date><tt:Year>2026</tt:Year><tt:Month>1</tt:Month><tt:Day>1</tt:Day></tt:Date></tt:UTCDateTime></tds:SystemDateAndTime></tds:GetSystemDateAndTimeResponse>".to_owned()
        } else if head_lower.contains("getcapabilities") {
            format!(
                "<tds:GetCapabilitiesResponse><tds:Capabilities><tt:Media><tt:XAddr>{advertised}/onvif/media</tt:XAddr></tt:Media></tds:Capabilities></tds:GetCapabilitiesResponse>"
            )
        } else if head_lower.contains("getprofiles") {
            r#"<trt:GetProfilesResponse><trt:Profiles token="profile-1" fixed="true"><tt:Name>Main</tt:Name></trt:Profiles></trt:GetProfilesResponse>"#.to_owned()
        } else if head_lower.contains("getsnapshoturi") {
            format!(
                "<trt:GetSnapshotUriResponse><trt:MediaUri><tt:Uri>{advertised}/snap.jpg?channel=1</tt:Uri><tt:InvalidAfterConnect>false</tt:InvalidAfterConnect><tt:InvalidAfterReboot>false</tt:InvalidAfterReboot><tt:Timeout>PT0S</tt:Timeout></trt:MediaUri></trt:GetSnapshotUriResponse>"
            )
        } else {
            panic!("unexpected SOAP call: {head_lower}")
        };
        format!("{ENVELOPE}{body}</s:Body></s:Envelope>")
    }

    /// The ONVIF method asks the Media service once, fetches the snapshot at
    /// the advertised path on the configured camera, reuses the URL, and
    /// resolves again after a failed fetch.
    #[tokio::test]
    async fn onvif_snapshots_follow_the_media_service_and_cache_the_url() {
        let (source, seen, fail) = onvif_fixture().await;
        let id = CameraId::try_new("cam1").unwrap();
        let body = tokio::time::timeout(
            Duration::from_secs(10),
            fetch_by_method(&id, &source, ONVIF_URL_TTL),
        )
        .await
        .expect("the first snapshot completes")
        .expect("the snapshot is served through the resolved URL");
        assert_eq!(body.len(), 64);
        {
            let seen = seen.lock().unwrap();
            assert_eq!(seen.len(), 5, "{seen:?}");
            assert!(
                seen[0].starts_with("POST /onvif/device_service "),
                "{seen:?}"
            );
            assert!(seen[2].starts_with("POST /onvif/media "), "{seen:?}");
            assert_eq!(seen[4], "GET /snap.jpg?channel=1 HTTP/1.1", "{seen:?}");
        }
        fetch_by_method(&id, &source, ONVIF_URL_TTL)
            .await
            .expect("the cached URL serves again");
        assert_eq!(
            seen.lock().unwrap().len(),
            6,
            "no media call for a cached URL"
        );

        fail.store(true, Ordering::Relaxed);
        assert!(fetch_by_method(&id, &source, ONVIF_URL_TTL).await.is_err());
        assert!(
            source.resolved.lock().unwrap().is_none(),
            "a failed fetch forgets the URL"
        );
        fail.store(false, Ordering::Relaxed);
        fetch_by_method(&id, &source, ONVIF_URL_TTL)
            .await
            .expect("resolved again after the failure");
        assert_eq!(
            seen.lock().unwrap().len(),
            7 + 5,
            "{:?}",
            seen.lock().unwrap()
        );
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
        let served = fetch_snapshot(&cam1(), &source)
            .await
            .expect("padding is accepted");
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
                let source = SnapshotSource::new(
                    host.into(),
                    port,
                    https,
                    true,
                    String::new(),
                    String::new(),
                    ResolvedSnapshot::DahuaCgi,
                );
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
        let body = fetch_snapshot(&cam1(), &source)
            .await
            .expect("exact limit succeeds");
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
            upstream_text(
                fetch_snapshot(&cam1(), &source)
                    .await
                    .expect_err("must reject")
            ),
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
            upstream_text(
                fetch_snapshot(&cam1(), &source)
                    .await
                    .expect_err("must reject")
            ),
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
        let camera = cam1();
        let results =
            futures_util::future::join_all((0..REQUESTS).map(|_| fetch_snapshot(&camera, &source)))
                .await;
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
            upstream_text(
                fetch_snapshot(&cam1(), &source)
                    .await
                    .expect_err("must reject")
            ),
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
        let text = upstream_text(
            fetch_snapshot(&cam1(), &source)
                .await
                .expect_err("must reject"),
        );
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
            fetch_snapshot_with_timeout(
                &cam1(),
                &source,
                &snapshot_url(&source),
                SNAPSHOT_PATH,
                Duration::from_millis(50),
            )
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
            tokio::time::timeout(Duration::from_millis(50), fetch_snapshot(&cam1(), &source)).await;
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

            let error = fetch_snapshot(&cam1(), &source(source_address))
                .await
                .expect_err("redirect is not a snapshot response");
            assert_eq!(upstream_text(error), "camera returned 302 Found");
            assert!(!source_task.await.expect("redirect source task"), "{kind}");
            assert!(!target_task.await.expect("redirect target task"), "{kind}");
        }
    }
}
