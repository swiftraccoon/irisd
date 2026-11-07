//! The live feed of one tile: a WebSocket client that pushes the daemon's
//! fragmented MP4 byte stream into the tile's `appsrc`, reconnecting until
//! it is cancelled.

use std::time::Duration;

use futures::StreamExt as _;
use tokio_tungstenite::Connector;
use tokio_tungstenite::tungstenite::Error as WebSocketError;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest as _;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::tungstenite::http::Request;
use tokio_tungstenite::tungstenite::http::header;
use tokio_tungstenite::tungstenite::protocol::CloseFrame;
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};
use url::Url;

use crate::tile::{SOURCE_QUEUE_BYTES, Source, TileEvent};

/// Message type byte of an init segment (the API's `MSG_INIT`).
const MSG_INIT: u8 = 0x01;
/// Message type byte of a media fragment (the API's `MSG_FRAGMENT`).
const MSG_FRAGMENT: u8 = 0x02;
/// Pause before reconnecting after a session ends or fails.
const RECONNECT_DELAY: Duration = Duration::from_secs(2);
/// Longest pause between attempts while the stream's frames exceed the message ceiling.
const MAX_OVERSIZED_DELAY: Duration = Duration::from_secs(60);
/// The live protocol's message ceiling: one type byte plus one full source
/// payload, 16 MiB + 1 bytes. The daemon honours the same ceiling: it skips an
/// oversized non-sync frame and closes the socket with code 1009 (Message Too
/// Big) when a sync frame exceeds it.
pub(crate) const LIVE_MESSAGE_BYTES: usize = SOURCE_QUEUE_BYTES as usize + 1;

/// Where a feed connects.
#[derive(Clone, Debug)]
pub struct Feed {
    /// Base URL of the API.
    pub api: Url,
    /// Bearer token, sent in the WebSocket handshake's Authorization header.
    pub token: Option<String>,
    /// Camera id.
    pub camera: String,
    /// Stream kind.
    pub kind: String,
}

impl Feed {
    /// The WebSocket URL of the live endpoint: `wss` for an `https` API,
    /// `ws` otherwise.
    pub fn url(&self) -> anyhow::Result<Url> {
        let mut url = self
            .api
            .join(&format!("/api/streams/{}/{}/live", self.camera, self.kind))?;
        let scheme = match url.scheme() {
            "https" => "wss",
            _ => "ws",
        };
        url.set_scheme(scheme)
            .map_err(|()| anyhow::anyhow!("cannot set the WebSocket scheme"))?;
        Ok(url)
    }
}

/// How a session ended without a transport or protocol failure.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Ending {
    /// The server closed the stream, or the connection ended.
    Closed,
    /// A message exceeded [`LIVE_MESSAGE_BYTES`]: the daemon closed with code
    /// 1009, or the local limit refused the message.
    Oversized,
}

/// The pause before the next attempt after another oversized frame: 2 s,
/// doubling up to [`MAX_OVERSIZED_DELAY`].
fn next_oversized_delay(previous: Option<Duration>) -> Duration {
    previous.map_or(RECONNECT_DELAY, |delay| {
        delay.saturating_mul(2).min(MAX_OVERSIZED_DELAY)
    })
}

/// Runs the feed until `cancel` fires: connects, pushes every message's
/// payload into `source`, and on any failure tells the tile to restart its
/// pipeline and reconnects after a pause. A frame above the message ceiling
/// puts the tile in its oversized state and backs off exponentially until the
/// feed delivers video again.
pub async fn run(
    feed: Feed,
    source: Source,
    tile_events: async_channel::Sender<TileEvent>,
    cancel: CancellationToken,
) {
    let url = match feed.url() {
        Ok(url) => url,
        Err(error) => {
            warn!(camera = %feed.camera, %error, "live feed URL");
            return;
        }
    };
    let mut oversized_delay: Option<Duration> = None;
    loop {
        let mut streamed = false;
        let announce = oversized_delay.is_some().then_some(&tile_events);
        let outcome = tokio::select! {
            () = cancel.cancelled() => return,
            // `None` verifies a `wss` server against the platform's native roots.
            outcome = session(&url, feed.token.as_deref(), None, &source, announce, &mut streamed) => outcome,
        };
        if streamed {
            oversized_delay = None;
        }
        let delay = match outcome {
            Ok(Ending::Oversized) => {
                let delay = next_oversized_delay(oversized_delay);
                oversized_delay = Some(delay);
                warn!(
                    camera = %feed.camera,
                    limit_bytes = LIVE_MESSAGE_BYTES,
                    retry_s = delay.as_secs(),
                    "live frame exceeds the message ceiling"
                );
                let _ = tile_events
                    .send(TileEvent::Oversized { retry_in: delay })
                    .await;
                delay
            }
            Ok(Ending::Closed) => {
                debug!(camera = %feed.camera, "live feed ended");
                RECONNECT_DELAY
            }
            Err(error) => {
                warn!(camera = %feed.camera, error = %format!("{error:#}"), "live feed failed");
                RECONNECT_DELAY
            }
        };
        let _ = tile_events.send(TileEvent::Restart).await;
        tokio::select! {
            () = cancel.cancelled() => return,
            () = tokio::time::sleep(delay) => {}
        }
    }
}

fn connect_request(url: &Url, token: Option<&str>) -> anyhow::Result<Request<()>> {
    let mut request = url.as_str().into_client_request()?;
    if let Some(token) = token {
        let value = HeaderValue::from_str(&format!("Bearer {token}"))?;
        request.headers_mut().insert(header::AUTHORIZATION, value);
    }
    Ok(request)
}

/// One connection: pushes every payload into `source` until the server closes.
/// `connector` selects the TLS trust for `wss` (`None` uses the platform's
/// native roots). `announce` receives [`TileEvent::Live`] at the first fragment,
/// and `streamed` records that a fragment arrived.
async fn session(
    url: &Url,
    token: Option<&str>,
    connector: Option<Connector>,
    source: &Source,
    announce: Option<&async_channel::Sender<TileEvent>>,
    streamed: &mut bool,
) -> anyhow::Result<Ending> {
    let request = connect_request(url, token)?;
    let (stream, _) = tokio_tungstenite::connect_async_tls_with_config(
        request,
        Some(websocket_config()),
        false,
        connector,
    )
    .await?;
    let (_, mut read) = stream.split();
    source.reset();
    while let Some(message) = read.next().await {
        match message {
            Ok(Message::Binary(bytes)) => {
                source.push(live_payload(&bytes)?)?;
                if bytes.first() == Some(&MSG_FRAGMENT) && !*streamed {
                    *streamed = true;
                    if let Some(events) = announce {
                        let _ = events.try_send(TileEvent::Live);
                    }
                }
            }
            Ok(Message::Close(frame)) => return Ok(close_ending(frame.as_ref())),
            Ok(_) => {}
            Err(WebSocketError::Capacity(_)) => return Ok(Ending::Oversized),
            Err(error) => return Err(error.into()),
        }
    }
    Ok(Ending::Closed)
}

/// Close code 1009 (Message Too Big) is the daemon refusing a frame above the ceiling.
fn close_ending(frame: Option<&CloseFrame>) -> Ending {
    match frame {
        Some(frame) if frame.code == CloseCode::Size => Ending::Oversized,
        _ => Ending::Closed,
    }
}

fn websocket_config() -> WebSocketConfig {
    WebSocketConfig::default()
        .max_message_size(Some(LIVE_MESSAGE_BYTES))
        .max_frame_size(Some(LIVE_MESSAGE_BYTES))
}

fn live_payload(message: &[u8]) -> anyhow::Result<&[u8]> {
    let Some((&kind, payload)) = message.split_first() else {
        anyhow::bail!("empty live message");
    };
    match kind {
        MSG_INIT | MSG_FRAGMENT => Ok(payload),
        other => anyhow::bail!("unknown live message type {other:#04x}"),
    }
}

#[cfg(test)]
mod tests {
    use std::net::SocketAddr;
    use std::sync::Arc;

    use axum::Router;
    use axum::extract::WebSocketUpgrade;
    use axum::extract::ws::CloseFrame as AxumCloseFrame;
    use axum::extract::ws::Message as AxumMessage;
    use axum::routing::get;
    use futures::SinkExt as _;
    use tokio_rustls::TlsAcceptor;
    use tokio_rustls::rustls::pki_types::PrivatePkcs8KeyDer;
    use tokio_rustls::rustls::{CertificateError, ClientConfig, RootCertStore, ServerConfig};

    use super::*;

    /// Bounds every network step of a test.
    const TEST_DEADLINE: Duration = Duration::from_secs(10);

    #[test]
    fn live_url_switches_scheme_and_keeps_the_bearer_in_a_header() {
        let feed = Feed {
            api: Url::parse("http://192.0.2.10:8554").unwrap(),
            token: Some("a b".into()),
            camera: "cam1".into(),
            kind: "sub".into(),
        };
        assert_eq!(
            feed.url().unwrap().as_str(),
            "ws://192.0.2.10:8554/api/streams/cam1/sub/live"
        );
        let request = connect_request(&feed.url().unwrap(), feed.token.as_deref()).unwrap();
        assert_eq!(request.headers()[header::AUTHORIZATION], "Bearer a b");
        let secure = Feed {
            api: Url::parse("https://example.invalid/").unwrap(),
            token: None,
            ..feed
        };
        assert_eq!(
            secure.url().unwrap().as_str(),
            "wss://example.invalid/api/streams/cam1/sub/live"
        );
    }

    #[test]
    fn websocket_admission_matches_the_source_payload_limit() {
        let config = websocket_config();
        assert_eq!(config.max_message_size, Some(LIVE_MESSAGE_BYTES));
        assert_eq!(config.max_frame_size, Some(LIVE_MESSAGE_BYTES));
        assert_eq!(LIVE_MESSAGE_BYTES, SOURCE_QUEUE_BYTES as usize + 1);
        assert_eq!(
            LIVE_MESSAGE_BYTES,
            (16 << 20) + 1,
            "documented protocol ceiling"
        );
    }

    #[test]
    fn malformed_fixed_protocol_messages_end_the_session() {
        assert!(live_payload(&[]).is_err());
        assert!(live_payload(&[0xff]).is_err());
        assert_eq!(live_payload(&[MSG_INIT, 7]).unwrap(), &[7]);
        assert_eq!(live_payload(&[MSG_FRAGMENT, 8]).unwrap(), &[8]);
    }

    #[test]
    fn oversized_backoff_doubles_from_two_seconds_to_a_minute() {
        let mut delay = None;
        let mut sequence = Vec::new();
        for _ in 0..8 {
            let next = next_oversized_delay(delay);
            sequence.push(next.as_secs());
            delay = Some(next);
        }
        assert_eq!(sequence, [2, 4, 8, 16, 32, 60, 60, 60]);
    }

    /// A TLS WebSocket listener on loopback with a self-signed certificate for
    /// 127.0.0.1. Each accepted session receives one init and one fragment.
    async fn tls_websocket_fixture() -> (Url, rcgen::CertifiedKey<rcgen::KeyPair>) {
        let certified = rcgen::generate_simple_self_signed(vec!["127.0.0.1".to_owned()]).unwrap();
        let key = PrivatePkcs8KeyDer::from(certified.signing_key.serialize_der());
        let config = ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(vec![certified.cert.der().clone()], key.into())
            .unwrap();
        let acceptor = TlsAcceptor::from(Arc::new(config));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            while let Ok((socket, _)) = listener.accept().await {
                let acceptor = acceptor.clone();
                tokio::spawn(async move {
                    // A client that rejects the certificate ends the handshake here.
                    let Ok(tls) = acceptor.accept(socket).await else {
                        return;
                    };
                    let Ok(mut socket) = tokio_tungstenite::accept_async(tls).await else {
                        return;
                    };
                    let _ = socket
                        .send(Message::Binary(vec![MSG_INIT, 1, 2, 3].into()))
                        .await;
                    let _ = socket
                        .send(Message::Binary(vec![MSG_FRAGMENT, 4, 5].into()))
                        .await;
                    let _ = socket.close(None).await;
                });
            }
        });
        let url = Url::parse(&format!("wss://127.0.0.1:{port}/api/streams/cam1/sub/live")).unwrap();
        (url, certified)
    }

    #[tokio::test]
    async fn wss_sessions_verify_the_server_certificate() {
        gst::init().unwrap();
        let (url, certified) = tls_websocket_fixture().await;

        let source = Source::for_tests();
        let mut streamed = false;
        let untrusted = tokio::time::timeout(
            TEST_DEADLINE,
            session(&url, Some("fixture"), None, &source, None, &mut streamed),
        )
        .await
        .expect("the default trust store decides before the deadline")
        .expect_err("a self-signed server is not in the platform trust store");
        let rustls_error = untrusted
            .downcast_ref::<WebSocketError>()
            .and_then(|error| match error {
                WebSocketError::Io(io) => io.get_ref(),
                _ => None,
            })
            .and_then(|inner| inner.downcast_ref::<tokio_rustls::rustls::Error>());
        assert!(
            matches!(
                rustls_error,
                Some(tokio_rustls::rustls::Error::InvalidCertificate(
                    CertificateError::UnknownIssuer
                ))
            ),
            "{untrusted:#}"
        );
        assert!(!streamed);
        assert_eq!(source.queued_bytes(), 0);

        let mut roots = RootCertStore::empty();
        roots.add(certified.cert.der().clone()).unwrap();
        let config = ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth();
        let ending = tokio::time::timeout(
            TEST_DEADLINE,
            session(
                &url,
                Some("fixture"),
                Some(Connector::Rustls(Arc::new(config))),
                &source,
                None,
                &mut streamed,
            ),
        )
        .await
        .expect("the trusted session ends before the deadline")
        .expect("WSS connects when the fixture root is trusted");
        assert_eq!(ending, Ending::Closed);
        assert!(streamed);
        assert_eq!(source.queued_bytes(), 5, "both payloads reach the source");
    }

    /// How the plain-WebSocket fixture ends each session after its init message.
    #[derive(Clone, Copy)]
    enum Script {
        /// One fragment with this many payload bytes, then a normal close.
        Fragment(usize),
        /// A close with code 1009, as the daemon sends for an oversized sync frame.
        TooBig,
    }

    async fn plain_websocket_fixture(script: Script) -> SocketAddr {
        let app = Router::new().route(
            "/api/streams/{camera}/{kind}/live",
            get(move |upgrade: WebSocketUpgrade| async move {
                upgrade.on_upgrade(move |mut socket| async move {
                    let init = vec![MSG_INIT, 0, 0, 0, 8];
                    let _ = socket.send(AxumMessage::Binary(init.into())).await;
                    match script {
                        Script::Fragment(payload) => {
                            let mut fragment = vec![0u8; payload + 1];
                            fragment[0] = MSG_FRAGMENT;
                            let _ = socket.send(AxumMessage::Binary(fragment.into())).await;
                            let _ = socket.send(AxumMessage::Close(None)).await;
                        }
                        Script::TooBig => {
                            let frame = AxumCloseFrame {
                                code: 1009,
                                reason: "frame exceeds the live message ceiling".into(),
                            };
                            let _ = socket.send(AxumMessage::Close(Some(frame))).await;
                        }
                    }
                })
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        address
    }

    async fn plain_session(address: SocketAddr) -> anyhow::Result<Ending> {
        let url = Url::parse(&format!("ws://{address}/api/streams/cam1/main/live")).unwrap();
        let source = Source::for_tests();
        let mut streamed = false;
        tokio::time::timeout(
            TEST_DEADLINE,
            session(&url, None, None, &source, None, &mut streamed),
        )
        .await
        .expect("the session ends before the deadline")
    }

    #[tokio::test]
    async fn frames_above_the_ceiling_are_a_distinct_ending() {
        gst::init().unwrap();
        let below = plain_websocket_fixture(Script::Fragment(15 << 20)).await;
        assert_eq!(plain_session(below).await.unwrap(), Ending::Closed);
        for payload in [(16 << 20) + 4096, 20 << 20] {
            let above = plain_websocket_fixture(Script::Fragment(payload)).await;
            assert_eq!(
                plain_session(above).await.unwrap(),
                Ending::Oversized,
                "{payload} bytes"
            );
        }
        let refused = plain_websocket_fixture(Script::TooBig).await;
        assert_eq!(plain_session(refused).await.unwrap(), Ending::Oversized);
    }

    #[tokio::test]
    async fn an_oversized_stream_backs_off_instead_of_reconnecting_every_two_seconds() {
        gst::init().unwrap();
        let address = plain_websocket_fixture(Script::TooBig).await;
        let feed = Feed {
            api: Url::parse(&format!("http://{address}")).unwrap(),
            token: None,
            camera: "cam1".into(),
            kind: "main".into(),
        };
        let (events_tx, events_rx) = async_channel::unbounded();
        let cancel = CancellationToken::new();
        let task = tokio::spawn(run(feed, Source::for_tests(), events_tx, cancel.clone()));
        let mut oversized = Vec::new();
        let deadline = tokio::time::Instant::now() + TEST_DEADLINE;
        while oversized.len() < 2 {
            let event = tokio::time::timeout_at(deadline, events_rx.recv())
                .await
                .expect("the feed reports each attempt before the deadline")
                .expect("the feed keeps its event channel open");
            if let TileEvent::Oversized { retry_in } = event {
                oversized.push(retry_in);
            }
        }
        cancel.cancel();
        tokio::time::timeout(TEST_DEADLINE, task)
            .await
            .expect("cancellation stops the feed")
            .unwrap();
        assert_eq!(
            oversized,
            [Duration::from_secs(2), Duration::from_secs(4)],
            "the tile shows the oversized state with a growing retry delay"
        );
    }
}
