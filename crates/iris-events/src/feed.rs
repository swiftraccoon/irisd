//! The Dahua CGI push feed: `eventManager.cgi?action=attach`.

use std::sync::Arc;
use std::time::Duration;

use futures_util::StreamExt as _;
use reqwest::StatusCode;
use thiserror::Error;
use tokio::time::{Instant, timeout, timeout_at};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::backoff::Backoff;
use crate::dahua;
use crate::digest::{DigestAuth, parse_challenge};
use crate::endpoint::{CameraEndpoint, EventSink};
use crate::multipart::{
    DEFAULT_BOUNDARY, MultipartError, MultipartReader, boundary_from_content_type,
};
use crate::stats::EventStats;
use iris_core::time::WallUs;

/// Heartbeat interval requested from the camera, in seconds.
const HEARTBEAT_SECS: u64 = 5;
/// No part for this long means the feed stalled (2 × heartbeat + 5 s).
const STALL: Duration = Duration::from_secs(2 * HEARTBEAT_SECS + 5);
/// Extra pause after an authentication failure so a wrong password cannot
/// lock the account out.
const AUTH_FAILURE_PAUSE: Duration = Duration::from_secs(30);
/// Bounds the complete attach exchange, including digest authentication.
const ATTACH_DEADLINE: Duration = Duration::from_secs(10);

struct PartDeadline {
    deadline: Instant,
}

impl PartDeadline {
    fn new(now: Instant) -> Self {
        Self {
            deadline: now + STALL,
        }
    }

    fn deadline(&self) -> Instant {
        self.deadline
    }

    fn observe(&mut self, now: Instant, completed_parts: usize) {
        if completed_parts != 0 {
            self.deadline = now + STALL;
        }
    }
}

/// Why a feed session ended.
#[derive(Debug, Error)]
enum SessionError {
    #[error("HTTP request failed: {0}")]
    Http(#[from] reqwest::Error),
    #[error("camera rejected the credentials")]
    Unauthorized,
    #[error("unexpected HTTP status {0}")]
    Status(StatusCode),
    #[error("no part received for {STALL:?}")]
    Stalled,
    #[error("feed attach did not complete within {ATTACH_DEADLINE:?}")]
    AttachTimeout,
    #[error("feed ended")]
    Ended,
    #[error(transparent)]
    Multipart(#[from] MultipartError),
}

/// Runs the Dahua CGI event feed for one camera until `shutdown` is cancelled.
///
/// Reconnects with exponential backoff on errors, stalls, and end of stream.
/// Credentials are sent only as HTTP digest responses and never logged.
pub async fn run_dahua_cgi(
    endpoint: CameraEndpoint,
    sink: Arc<dyn EventSink>,
    stats: Arc<EventStats>,
    shutdown: CancellationToken,
) {
    let client = match reqwest::Client::builder()
        .danger_accept_invalid_certs(!endpoint.tls_verify)
        .connect_timeout(Duration::from_secs(10))
        .build()
    {
        Ok(client) => client,
        Err(error) => {
            warn!(camera = %endpoint.camera, %error, "cannot build HTTP client; event feed disabled");
            shutdown.cancelled().await;
            return;
        }
    };
    let mut auth = DigestAuth::new(&endpoint.username, &endpoint.password);
    let mut backoff = Backoff::new(&format!("{}/dahua", endpoint.camera));
    loop {
        let outcome = tokio::select! {
            () = shutdown.cancelled() => break,
            outcome = session(
                &client,
                &endpoint,
                &mut auth,
                sink.as_ref(),
                &stats,
                &mut backoff,
            ) => outcome,
        };
        stats.set_connected(false);
        if shutdown.is_cancelled() {
            break;
        }
        match &outcome {
            Ok(()) | Err(SessionError::Ended) => {
                info!(camera = %endpoint.camera, "event feed ended; reconnecting");
            }
            Err(error) => {
                warn!(camera = %endpoint.camera, %error, "event feed session failed; reconnecting");
            }
        }
        stats.record_reconnect();
        let mut delay = backoff.delay();
        if matches!(outcome, Err(SessionError::Unauthorized)) {
            delay = delay.max(AUTH_FAILURE_PAUSE);
        }
        tokio::select! {
            () = shutdown.cancelled() => break,
            () = tokio::time::sleep(delay) => {}
        }
    }
    debug!(camera = %endpoint.camera, "event feed stopped");
}

/// Runs one feed session, counting the error that ends it.
async fn session(
    client: &reqwest::Client,
    endpoint: &CameraEndpoint,
    auth: &mut DigestAuth,
    sink: &dyn EventSink,
    stats: &EventStats,
    backoff: &mut Backoff,
) -> Result<(), SessionError> {
    let result = attached(client, endpoint, auth, sink, stats, backoff).await;
    if result.is_err() {
        stats.record_error();
    }
    result
}

/// Attaches to the feed and forwards its parts until it stalls, ends, or fails.
async fn attached(
    client: &reqwest::Client,
    endpoint: &CameraEndpoint,
    auth: &mut DigestAuth,
    sink: &dyn EventSink,
    stats: &EventStats,
    backoff: &mut Backoff,
) -> Result<(), SessionError> {
    let path =
        format!("/cgi-bin/eventManager.cgi?action=attach&codes=[All]&heartbeat={HEARTBEAT_SECS}");
    let url = format!("{}{path}", endpoint.base_url());
    let response = get_with_digest(client, &url, &path, auth).await?;
    let boundary = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .and_then(boundary_from_content_type)
        .unwrap_or_else(|| DEFAULT_BOUNDARY.to_owned());
    let mut reader = MultipartReader::new(&boundary);
    let mut stream = response.bytes_stream();
    backoff.session_connected();
    stats.set_connected(true);
    info!(camera = %endpoint.camera, "event feed connected");
    let mut part_deadline = PartDeadline::new(Instant::now());
    loop {
        let chunk = match timeout_at(part_deadline.deadline(), stream.next()).await {
            Err(_) => return Err(SessionError::Stalled),
            Ok(None) => return Err(SessionError::Ended),
            Ok(Some(chunk)) => chunk?,
        };
        let parts = reader.push(&chunk)?;
        part_deadline.observe(Instant::now(), parts.len());
        for part in parts {
            let now = WallUs::now();
            stats.record_message(now);
            let text = String::from_utf8_lossy(&part);
            let Some(raw) = dahua::parse_part(&text) else {
                continue; // heartbeat
            };
            if let Some(event) = dahua::normalize(&endpoint.camera, &raw, now) {
                debug!(camera = %endpoint.camera, code = %raw.code, action = %raw.action, "event");
                stats.record_event(now);
                sink.publish(event);
            }
        }
    }
}

/// Performs a GET with HTTP digest authentication: reuses a known challenge,
/// answers one 401 with a fresh challenge, and gives up on a second 401.
async fn get_with_digest(
    client: &reqwest::Client,
    url: &str,
    uri: &str,
    auth: &mut DigestAuth,
) -> Result<reqwest::Response, SessionError> {
    timeout(
        ATTACH_DEADLINE,
        get_with_digest_inner(client, url, uri, auth),
    )
    .await
    .map_err(|_| SessionError::AttachTimeout)?
}

async fn get_with_digest_inner(
    client: &reqwest::Client,
    url: &str,
    uri: &str,
    auth: &mut DigestAuth,
) -> Result<reqwest::Response, SessionError> {
    for attempt in 0..2 {
        let mut request = client.get(url);
        if let Some(header) = auth.authorization("GET", uri) {
            request = request.header(reqwest::header::AUTHORIZATION, header);
        }
        let response = request.send().await?;
        match response.status() {
            StatusCode::OK => return Ok(response),
            StatusCode::UNAUTHORIZED => {
                let challenge = response
                    .headers()
                    .get_all(reqwest::header::WWW_AUTHENTICATE)
                    .iter()
                    .filter_map(|v| v.to_str().ok())
                    .find_map(parse_challenge);
                match challenge {
                    Some(challenge) if attempt == 0 || challenge.stale => {
                        auth.set_challenge(challenge);
                    }
                    _ => return Err(SessionError::Unauthorized),
                }
            }
            status => return Err(SessionError::Status(status)),
        }
    }
    Err(SessionError::Unauthorized)
}

#[cfg(test)]
mod tests {
    use tokio::io::AsyncReadExt as _;
    use tokio::net::TcpListener;

    use super::*;

    #[test]
    fn incomplete_chunks_do_not_move_the_part_deadline() {
        let started = Instant::now();
        let mut deadline = PartDeadline::new(started);
        let incomplete_arrival = started + STALL - Duration::from_secs(1);
        deadline.observe(incomplete_arrival, 0);
        assert_eq!(deadline.deadline(), started + STALL);

        let completed_arrival = incomplete_arrival;
        deadline.observe(completed_arrival, 1);
        assert_eq!(
            deadline.deadline(),
            started + STALL * 2 - Duration::from_secs(1)
        );
    }

    #[test]
    fn fragmented_heartbeat_just_before_deadline_moves_it_once() {
        let started = Instant::now();
        let mut deadline = PartDeadline::new(started);
        let mut reader = MultipartReader::new("b");
        let near_deadline = started + STALL - Duration::from_millis(1);

        let parts = reader
            .push(b"--b\r\nContent-Type:text/plain\r\n\r\nHeart")
            .unwrap();
        assert_eq!(parts, Vec::<Vec<u8>>::new());
        deadline.observe(near_deadline, parts.len());
        assert_eq!(deadline.deadline(), started + STALL);

        let parts = reader.push(b"beat\r\n--b\r\n").unwrap();
        assert_eq!(parts, [b"Heartbeat".to_vec()]);
        deadline.observe(near_deadline, parts.len());
        assert_eq!(deadline.deadline(), near_deadline + STALL);
    }

    #[tokio::test]
    async fn a_server_that_never_sends_headers_hits_the_attach_deadline() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = [0; 1_024];
            assert!(socket.read(&mut request).await.unwrap() > 0);
            std::future::pending::<()>().await;
        });
        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .build()
            .unwrap();
        let result = timeout(
            Duration::from_secs(11),
            get_with_digest(
                &client,
                &format!("http://{address}/feed"),
                "/feed",
                &mut DigestAuth::new("", ""),
            ),
        )
        .await;
        server.abort();
        assert!(
            result.is_ok(),
            "a connected server cannot hold attach open indefinitely"
        );
        assert!(matches!(result.unwrap(), Err(SessionError::AttachTimeout)));
    }
}
