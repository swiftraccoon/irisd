//! The Dahua CGI push feed: `eventManager.cgi?action=attach`.

use std::sync::Arc;
use std::time::Duration;

use futures_util::StreamExt as _;
use reqwest::StatusCode;
use thiserror::Error;
use tokio::time::timeout;
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
    let mut backoff = Backoff::new();
    loop {
        backoff.session_started();
        let outcome = tokio::select! {
            () = shutdown.cancelled() => break,
            outcome = session(&client, &endpoint, &mut auth, sink.as_ref(), &stats) => outcome,
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
) -> Result<(), SessionError> {
    let result = attached(client, endpoint, auth, sink, stats).await;
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
    stats.set_connected(true);
    info!(camera = %endpoint.camera, "event feed connected");
    loop {
        let chunk = match timeout(STALL, stream.next()).await {
            Err(_) => return Err(SessionError::Stalled),
            Ok(None) => return Err(SessionError::Ended),
            Ok(Some(chunk)) => chunk?,
        };
        for part in reader.push(&chunk)? {
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
