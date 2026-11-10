//! The Dahua CGI push feed: `eventManager.cgi?action=attach`.

use std::sync::Arc;
use std::time::Duration;

use futures_util::StreamExt as _;
use iris_core::text::LogSafe;
use iris_core::time::WallUs;
use reqwest::StatusCode;
use thiserror::Error;
use tokio::time::{Instant, timeout, timeout_at};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::backoff::Backoff;
use crate::dahua;
use crate::digest::{self, DigestAuth};
use crate::endpoint::CameraEndpoint;
use crate::multipart::{self, DEFAULT_BOUNDARY, MultipartError, MultipartReader};
use crate::sink::EventSink;
use crate::stats::EventStats;

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

/// Why a feed session ended. `Display` may carry camera text through the
/// HTTP error; a log line takes it through [`LogSafe`].
#[derive(Debug, Error)]
enum SessionError {
    #[error("HTTP request failed: {0}")]
    Http(#[from] reqwest::Error),
    /// A second fresh challenge, or a third after a stale nonce: the camera
    /// rejected the credentials.
    #[error("camera rejected the credentials")]
    Unauthorized,
    /// A 401 without a digest challenge irisd can answer (RFC 7616, MD5).
    #[error("camera sent no supported digest challenge")]
    NoDigestChallenge,
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
/// Credentials are sent only as HTTP digest responses and never logged. A
/// redirect is never followed: it fails the attachment with its status, so a
/// camera that redirects HTTP to HTTPS needs `https = true` in its
/// configuration.
pub async fn run_dahua_cgi(
    endpoint: CameraEndpoint,
    sink: Arc<dyn EventSink>,
    stats: Arc<EventStats>,
    shutdown: CancellationToken,
) {
    let client = match cgi_client(&endpoint) {
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
                warn!(
                    camera = %endpoint.camera,
                    error = %LogSafe(error),
                    "event feed session failed; reconnecting"
                );
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

/// The HTTP client of the CGI feed: the camera's certificate policy, a connect
/// bound, no redirects, and no proxy, so a camera or the environment cannot
/// steer requests and credentials elsewhere.
fn cgi_client(endpoint: &CameraEndpoint) -> Result<reqwest::Client, reqwest::Error> {
    reqwest::Client::builder()
        .danger_accept_invalid_certs(!endpoint.tls_verify)
        .connect_timeout(Duration::from_secs(10))
        .redirect(reqwest::redirect::Policy::none())
        .no_proxy()
        .build()
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
        .and_then(multipart::boundary_from_content_type)
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
                continue; // a heartbeat or a part without an event
            };
            if let Some(event) = dahua::normalize(&endpoint.camera, &raw, now) {
                debug!(
                    camera = %endpoint.camera,
                    code = %LogSafe(&raw.code),
                    action = %LogSafe(&raw.action),
                    "event"
                );
                stats.record_event(now);
                sink.publish(event);
            }
        }
    }
}

/// Performs a GET with HTTP digest authentication: reuses a known challenge,
/// answers one 401 with a fresh challenge, answers one more when that fresh
/// challenge only reports a stale nonce, and gives up on the 401 after that.
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
    let mut challenged = false;
    let mut stale_retried = false;
    loop {
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
                    .filter_map(|value| value.to_str().ok())
                    .find_map(digest::parse_challenge)
                    .ok_or(SessionError::NoDigestChallenge)?;
                if !challenged {
                    challenged = true;
                } else if challenge.stale && !stale_retried {
                    stale_retried = true;
                } else {
                    return Err(SessionError::Unauthorized);
                }
                auth.set_challenge(challenge);
            }
            status => return Err(SessionError::Status(status)),
        }
    }
}

#[cfg(test)]
mod tests {
    use iris_core::event::{Event, EventKind, EventSource, EventState};
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

    struct Collect(std::sync::Mutex<Vec<Event>>);

    impl EventSink for Collect {
        fn publish(&self, event: Event) {
            self.0.lock().unwrap().push(event);
        }
    }

    /// The whole feed task against a loopback camera that sends a heartbeat
    /// and two events and closes: the events reach the sink normalised, the
    /// counters follow, the feed is attached again after the backoff, and
    /// shutdown ends the task.
    #[tokio::test]
    async fn the_feed_task_delivers_events_reconnects_and_stops_at_shutdown() {
        const FEED: &str = "--myboundary\r\nContent-Type: text/plain\r\nContent-Length: 9\r\n\r\n\
            Heartbeat\r\n--myboundary\r\nContent-Type: text/plain\r\nContent-Length: 39\r\n\r\n\
            Code=VideoMotion;action=Start;index=0\r\n\r\n--myboundary\r\n\
            Content-Type: text/plain\r\nContent-Length: 36\r\n\r\n\
            Code=VideoLoss;action=Stop;index=0\r\n\r\n--myboundary\r\n";
        let camera = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = camera.local_addr().unwrap();
        let attaches = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let server = tokio::spawn({
            let attaches = Arc::clone(&attaches);
            async move {
                loop {
                    let (mut socket, _) = camera.accept().await.unwrap();
                    let mut request = [0; 1_024];
                    let read = socket.read(&mut request).await.unwrap();
                    let request = String::from_utf8_lossy(&request[..read]).into_owned();
                    assert!(
                        request.starts_with(
                            "GET /cgi-bin/eventManager.cgi?action=attach&codes=[All]&heartbeat=5 "
                        ),
                        "{request}"
                    );
                    attaches.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    let response = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: multipart/x-mixed-replace; \
                         boundary=myboundary\r\nConnection: close\r\n\r\n{FEED}"
                    );
                    tokio::io::AsyncWriteExt::write_all(&mut socket, response.as_bytes())
                        .await
                        .unwrap();
                }
            }
        });
        let endpoint = CameraEndpoint {
            camera: "cam1".parse().unwrap(),
            host: address.ip().to_string(),
            http_port: address.port(),
            https: false,
            tls_verify: true,
            username: String::new(),
            password: String::new(),
        };
        let sink = Arc::new(Collect(std::sync::Mutex::new(Vec::new())));
        let stats = Arc::new(EventStats::new());
        let shutdown = CancellationToken::new();
        let task = tokio::spawn(run_dahua_cgi(
            endpoint,
            Arc::clone(&sink) as Arc<dyn EventSink>,
            Arc::clone(&stats),
            shutdown.clone(),
        ));

        // The second attach follows the first backoff of about a second.
        timeout(Duration::from_secs(10), async {
            while attaches.load(std::sync::atomic::Ordering::SeqCst) < 2
                || sink.0.lock().unwrap().len() < 4
            {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("the feed attached twice and delivered both rounds");
        {
            let events = sink.0.lock().unwrap();
            let seen: Vec<_> = events
                .iter()
                .take(2)
                .map(|event| (event.kind.clone(), event.state, event.source))
                .collect();
            assert_eq!(
                seen,
                [
                    (EventKind::Motion, EventState::Start, EventSource::DahuaCgi),
                    (
                        EventKind::VideoLoss,
                        EventState::Stop,
                        EventSource::DahuaCgi
                    ),
                ]
            );
            assert!(events.iter().all(|event| event.camera.as_str() == "cam1"));
        }
        assert!(stats.events() >= 4);
        assert!(stats.messages() >= 6, "heartbeats count as messages");
        assert!(stats.reconnects() >= 1);
        assert!(stats.errors() >= 1, "a feed that ends is counted");

        shutdown.cancel();
        timeout(Duration::from_secs(2), task)
            .await
            .expect("the task ends at shutdown")
            .unwrap();
        assert!(!stats.connected());
        server.abort();
    }

    /// A loopback HTTPS camera with a self-signed certificate that answers
    /// one request with an empty 200; the task returns whether a request reached
    /// HTTP.
    async fn self_signed_camera(
        tls_verify: bool,
    ) -> (CameraEndpoint, tokio::task::JoinHandle<bool>) {
        use tokio_rustls::TlsAcceptor;
        use tokio_rustls::rustls::ServerConfig;
        use tokio_rustls::rustls::pki_types::PrivatePkcs8KeyDer;

        let certified = rcgen::generate_simple_self_signed(vec!["127.0.0.1".to_owned()]).unwrap();
        let key = PrivatePkcs8KeyDer::from(certified.signing_key.serialize_der());
        let config = ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(vec![certified.cert.der().clone()], key.into())
            .unwrap();
        let acceptor = TlsAcceptor::from(Arc::new(config));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            let Ok(mut socket) = acceptor.accept(socket).await else {
                return false;
            };
            let mut request = [0; 2_048];
            if !matches!(socket.read(&mut request).await, Ok(length) if length > 0) {
                return false;
            }
            tokio::io::AsyncWriteExt::write_all(
                &mut socket,
                b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            )
            .await
            .is_ok()
        });
        let endpoint = CameraEndpoint {
            camera: "cam1".parse().unwrap(),
            host: "127.0.0.1".to_owned(),
            http_port: address.port(),
            https: true,
            tls_verify,
            username: String::new(),
            password: String::new(),
        };
        (endpoint, server)
    }

    /// The feed's HTTP client verifies the camera's certificate unless the
    /// camera is configured not to be verified: a self-signed certificate
    /// stops a verified camera before any request, and an unverified one is
    /// attached to.
    #[tokio::test]
    async fn the_feed_client_verifies_certificates_unless_told_not_to() {
        for (tls_verify, reaches_http) in [(true, false), (false, true)] {
            let (endpoint, server) = self_signed_camera(tls_verify).await;
            let path = "/cgi-bin/eventManager.cgi?action=attach";
            let url = format!("{}{path}", endpoint.base_url());
            let result = get_with_digest(
                &cgi_client(&endpoint).unwrap(),
                &url,
                path,
                &mut DigestAuth::new("", ""),
            )
            .await;
            assert_eq!(result.is_ok(), reaches_http, "tls_verify={tls_verify}");
            let reached = timeout(Duration::from_secs(5), server)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(reached, reaches_http, "tls_verify={tls_verify}");
        }
    }

    #[tokio::test]
    async fn camera_redirects_are_never_followed() {
        let sink = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let sink_address = sink.local_addr().unwrap();
        let camera = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let camera_address = camera.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = camera.accept().await.unwrap();
            let mut request = [0; 1_024];
            assert!(socket.read(&mut request).await.unwrap() > 0);
            tokio::io::AsyncWriteExt::write_all(
                &mut socket,
                format!(
                    "HTTP/1.1 302 Found\r\nLocation: http://{sink_address}/internal?x=1\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                )
                .as_bytes(),
            )
            .await
            .unwrap();
        });
        let endpoint = CameraEndpoint {
            camera: "cam1".parse().unwrap(),
            host: camera_address.ip().to_string(),
            http_port: camera_address.port(),
            https: false,
            tls_verify: true,
            username: "user".to_owned(),
            password: "password".to_owned(),
        };
        let url = format!(
            "{}/cgi-bin/eventManager.cgi?action=attach",
            endpoint.base_url()
        );
        let result = get_with_digest(
            &cgi_client(&endpoint).unwrap(),
            &url,
            "/cgi-bin/eventManager.cgi?action=attach",
            &mut DigestAuth::new("user", "password"),
        )
        .await;
        assert!(
            matches!(result, Err(SessionError::Status(StatusCode::FOUND))),
            "{:?}",
            result.map(|response| response.status())
        );
        assert!(
            timeout(Duration::from_millis(200), sink.accept())
                .await
                .is_err(),
            "the redirect target received a request"
        );
        server.await.unwrap();
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
        let result = result.expect("a connected server cannot hold attach open indefinitely");
        assert!(
            matches!(result, Err(SessionError::AttachTimeout)),
            "{:?}",
            result.map(|response| response.status())
        );
    }

    /// Serves `responses` on successive connections, each as the status line
    /// and headers given, and reports the `Authorization` header of every
    /// request (`None` when absent).
    async fn scripted_camera(
        responses: Vec<&'static str>,
    ) -> (
        std::net::SocketAddr,
        tokio::task::JoinHandle<Vec<Option<String>>>,
    ) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let mut authorizations = Vec::new();
            for response in responses {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut request = vec![0; 8_192];
                let length = socket.read(&mut request).await.unwrap();
                let head = String::from_utf8_lossy(&request[..length]).into_owned();
                authorizations.push(
                    head.lines()
                        .find_map(|line| line.strip_prefix("authorization: "))
                        .map(str::to_owned),
                );
                tokio::io::AsyncWriteExt::write_all(
                    &mut socket,
                    format!("{response}Content-Length: 0\r\nConnection: close\r\n\r\n").as_bytes(),
                )
                .await
                .unwrap();
            }
            authorizations
        });
        (address, server)
    }

    const DIGEST_CHALLENGE: &str = "HTTP/1.1 401 Unauthorized\r\nWWW-Authenticate: Digest realm=\"camera\", nonce=\"first-nonce\", qop=\"auth\"\r\n";
    const STALE_CHALLENGE: &str = "HTTP/1.1 401 Unauthorized\r\nWWW-Authenticate: Digest realm=\"camera\", nonce=\"second-nonce\", qop=\"auth\", stale=true\r\n";
    const BASIC_CHALLENGE: &str =
        "HTTP/1.1 401 Unauthorized\r\nWWW-Authenticate: Basic realm=\"camera\"\r\n";
    const OK: &str = "HTTP/1.1 200 OK\r\nContent-Type: multipart/x-mixed-replace; boundary=b\r\n";

    async fn attach(address: std::net::SocketAddr) -> Result<reqwest::Response, SessionError> {
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap();
        get_with_digest(
            &client,
            &format!("http://{address}/feed"),
            "/feed",
            &mut DigestAuth::new("user", "password"),
        )
        .await
    }

    #[tokio::test]
    async fn a_fresh_challenge_is_answered_once() {
        let (address, server) = scripted_camera(vec![DIGEST_CHALLENGE, OK]).await;
        attach(address)
            .await
            .expect("the second request is authorized");
        let authorizations = server.await.unwrap();
        assert_eq!(authorizations[0], None);
        let answer = authorizations[1]
            .as_deref()
            .expect("an Authorization header");
        assert!(answer.starts_with("Digest username=\"user\""), "{answer}");
        assert!(
            answer.contains("nonce=\"first-nonce\"") && answer.contains("nc=00000001"),
            "{answer}"
        );
    }

    #[tokio::test]
    async fn a_stale_nonce_gets_one_more_attempt() {
        let (address, server) = scripted_camera(vec![DIGEST_CHALLENGE, STALE_CHALLENGE, OK]).await;
        attach(address)
            .await
            .expect("the third request is authorized");
        let authorizations = server.await.unwrap();
        let nonces: Vec<Option<&str>> = authorizations
            .iter()
            .map(|answer| {
                answer.as_deref().map(|answer| {
                    if answer.contains("second-nonce") {
                        "second"
                    } else {
                        "first"
                    }
                })
            })
            .collect();
        assert_eq!(nonces, [None, Some("first"), Some("second")]);
    }

    #[tokio::test]
    async fn a_second_fresh_challenge_means_rejected_credentials() {
        let (address, server) = scripted_camera(vec![DIGEST_CHALLENGE, DIGEST_CHALLENGE]).await;
        let error = attach(address).await.unwrap_err();
        assert!(matches!(error, SessionError::Unauthorized), "{error:?}");
        assert_eq!(server.await.unwrap().len(), 2, "no third request");
    }

    #[tokio::test]
    async fn a_camera_without_a_digest_challenge_is_not_reported_as_rejecting_credentials() {
        let (address, server) = scripted_camera(vec![BASIC_CHALLENGE]).await;
        let error = attach(address).await.unwrap_err();
        assert!(
            matches!(error, SessionError::NoDigestChallenge),
            "{error:?}"
        );
        assert_eq!(server.await.unwrap(), [None]);
    }
}
