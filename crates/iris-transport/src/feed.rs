//! The live feed of one view: a WebSocket client that pushes the daemon's
//! fragmented MP4 byte stream into the view's sink, and connects again as the
//! feed's [`Reconnect`] says until it is cancelled. The framing of the
//! messages, the policy, and the events it sends the view are those of
//! [`iris_client::live`]; this module carries them.

use std::fmt;
use std::sync::Arc;

use async_channel::Sender;
use iris_client::live::{
    FeedEvent, LIVE_MESSAGE_BYTES, LiveError, LiveMessage, LiveSink, Next, Reconnect, SessionEnd,
    SinkError, live_message,
};
use iris_client::origin::ApiOrigin;
use iris_core::id::{CameraId, StreamKind};
use iris_tls::ClientConfig;
use thiserror::Error;
use tokio_tungstenite::Connector;
use tokio_tungstenite::tungstenite::http::{Request, header};
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tokio_tungstenite::tungstenite::protocol::{CloseFrame, WebSocketConfig};
use tokio_tungstenite::tungstenite::{Error as WebSocketError, Message};
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};
use url::Url;

use crate::client::Bearer;

/// Where a live feed connects. Its `Debug` output redacts the token.
///
/// The token travels in each session's WebSocket handshake. The tungstenite
/// crate writes the whole request at trace level under the log target that
/// [`is_sensitive_log_target`] names. An application that installs a log
/// subscriber must drop that target whatever its filter says.
///
/// # Example
///
/// ```no_run
/// # use std::error::Error;
/// use std::env;
/// use std::sync::{Arc, Mutex};
///
/// use iris_client::live::{FeedEvent, LiveMessage, LiveSink, SinkError};
/// use iris_client::origin::{ApiOrigin, PlainHttp};
/// use iris_core::id::{CameraId, StreamKind};
/// use iris_transport::{Bearer, Feed};
/// use tokio_util::sync::CancellationToken;
///
/// /// A sink that counts the payload bytes of a session.
/// #[derive(Default)]
/// struct Counted(Mutex<usize>);
///
/// impl LiveSink for Counted {
///     fn reset(&self) {
///         *self.0.lock().expect("counted bytes") = 0;
///     }
///
///     fn push(&self, message: LiveMessage<'_>) -> Result<(), SinkError> {
///         let payload = match message {
///             LiveMessage::Init(init) => init,
///             LiveMessage::Fragment(fragment) => fragment,
///             // The sink refuses a kind of message it does not know.
///             _ => {
///                 let unknown = "live message of an unknown kind";
///                 return Err(SinkError::new(unknown));
///             }
///         };
///         *self.0.lock().expect("counted bytes") += payload.len();
///         Ok(())
///     }
/// }
///
/// # async fn watch() -> Result<(), Box<dyn Error>> {
/// let api = "https://nvr.example:8443";
/// // The daemon trims a presented token and treats an empty one as none.
/// let variable = env::var("IRISD_TOKEN").unwrap_or_default();
/// let token = Some(variable.trim()).filter(|token| !token.is_empty());
/// let feed = Feed {
///     origin: ApiOrigin::parse(api, PlainHttp::LoopbackOnly)?,
///     token: token.map(Bearer::try_new).transpose()?,
///     camera: CameraId::try_new("cam1")?,
///     kind: StreamKind::Sub,
///     tls: iris_tls::client_config(None)?,
/// };
/// assert_eq!(
///     feed.url().as_str(),
///     "wss://nvr.example:8443/api/streams/cam1/sub/live",
///     "an https origin gives a wss feed"
/// );
/// let (events, received) = async_channel::unbounded();
/// let cancel = CancellationToken::new();
/// let sink = Arc::new(Counted::default());
/// let running = tokio::spawn(feed.run(sink, events, cancel.clone()));
/// // A view restarts its decoder on each restart, and ends the feed when it
/// // stops: here, at the first.
/// while let Ok(event) = received.recv().await {
///     if event == FeedEvent::Restart {
///         break;
///     }
/// }
/// cancel.cancel();
/// running.await?;
/// # Ok(())
/// # }
/// ```
#[derive(Clone)]
pub struct Feed {
    /// The API origin, whose host and port the feed connects to under `ws`
    /// or `wss` ([`Feed::url`]).
    pub origin: ApiOrigin,
    /// The bearer, sent in each session's WebSocket handshake, in its
    /// `Authorization` header.
    pub token: Option<Bearer>,
    /// The camera whose stream the feed carries, which its log lines name.
    pub camera: CameraId,
    /// The camera's stream the feed carries: a main or sub stream's video,
    /// or the audio, whose 404 ends the feed.
    pub kind: StreamKind,
    /// The trust a `wss` feed verifies the server with; the Linux viewer
    /// passes the one its [`Client`](crate::Client) verifies with, so that its
    /// reads and its feeds accept the same certificates.
    pub tls: Arc<ClientConfig>,
}

impl fmt::Debug for Feed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Feed")
            .field("origin", &self.origin.url().as_str())
            .field("token", &self.token)
            .field("camera", &self.camera)
            .field("kind", &self.kind)
            .finish_non_exhaustive()
    }
}

impl Feed {
    /// The WebSocket URL of the live endpoint: `wss` for an `https` API,
    /// `ws` for an `http` one. The example of [`Feed`] reads it.
    ///
    /// # Panics
    ///
    /// Never: it calls [`ApiOrigin::live_url`], which switches the scheme
    /// with `Url::set_scheme` of the `url` crate. That refuses `ws` or `wss`
    /// only for a URL without a host or whose scheme is not special, and an
    /// origin is `http` or `https` with a host.
    #[must_use]
    pub fn url(&self) -> Url {
        self.origin.live_url(&self.camera, self.kind)
    }

    /// Runs the feed until `cancel` fires: connects and pushes every message
    /// into `sink`, and after every session does what the feed's [`Reconnect`]
    /// says. Unless it says to stop, the feed sends `events` a
    /// [`FeedEvent::Restart`], for the view to restart its decoder, and
    /// connects again after the pause it gives. A frame above the message
    /// ceiling sends [`FeedEvent::Oversized`] first, and the pause doubles, up
    /// to a minute, until the sink takes a fragment again, which sends
    /// [`FeedEvent::Live`]. An audio feed the daemon answers with 404 ends:
    /// the camera records no audio.
    ///
    /// The feed waits for room in `events` for every event, so a bounded
    /// channel loses none, [`FeedEvent::Live`] among them; while it waits for
    /// room for that one, the session reads no further. A send that finds the
    /// receiver closed ends the feed at once, since no view is left to act on
    /// what it sends, and `cancel` ends it while it waits for room too.
    ///
    /// Each ending is logged with the feed's `camera`:
    ///
    /// - a failed session as a warning, "live feed failed", whose `error` is
    ///   the failure and each of its sources after a colon;
    /// - a frame above the ceiling as a warning, "live frame exceeds the
    ///   message ceiling", with `limit_bytes` and `retry_s`;
    /// - a closed session as "live feed ended", at debug level;
    /// - an audio feed that ends as "the camera has no live audio", at debug
    ///   level.
    ///
    /// A feed whose receiver closed during a session ends without a log line.
    ///
    /// Each session's handshake carries the feed's token. The tungstenite
    /// crate writes the whole request, the token in it, at trace level under
    /// the log target that [`is_sensitive_log_target`] names. An application
    /// that installs a log subscriber must drop that target whatever its
    /// filter says.
    ///
    /// The example of [`Feed`] runs a feed into a counting sink and cancels
    /// it.
    ///
    /// # Panics
    ///
    /// When polled outside a Tokio runtime with its I/O and time drivers
    /// enabled: each session's connection needs the I/O driver, and the pause
    /// between sessions needs the timer. [`Feed::url`], which it calls,
    /// never panics.
    pub async fn run<S: LiveSink + ?Sized>(
        self,
        sink: Arc<S>,
        events: Sender<FeedEvent>,
        cancel: CancellationToken,
    ) {
        let url = self.url();
        let mut reconnect = Reconnect::new(self.kind);
        loop {
            let announce = reconnect.is_backing_off().then_some(&events);
            let Session { outcome, delivered } = tokio::select! {
                () = cancel.cancelled() => return,
                session = session(
                    &url,
                    self.token.as_ref(),
                    Some(Connector::Rustls(Arc::clone(&self.tls))),
                    &*sink,
                    announce,
                ) => session,
            };
            // A session whose receiver closed has no ending to weigh: the
            // feed ends with it.
            let Some(end) = session_end(&outcome) else {
                return;
            };
            // A session that delivered video ends the back-off before the
            // session's ending is weighed.
            if delivered {
                reconnect.delivered();
            }
            let delay = match reconnect.after(end) {
                Next::Retry(delay) => delay,
                Next::Stop => {
                    // The daemon has no live audio for this camera: it records
                    // none. There is nothing to reconnect to.
                    debug!(camera = %self.camera, "the camera has no live audio");
                    return;
                }
            };
            if end == SessionEnd::Oversized {
                warn!(
                    camera = %self.camera,
                    limit_bytes = LIVE_MESSAGE_BYTES,
                    retry_s = delay.as_secs(),
                    "live frame exceeds the message ceiling"
                );
                if !deliver(&events, FeedEvent::Oversized { retry_in: delay }, &cancel).await {
                    return;
                }
            } else {
                match &outcome {
                    Ok(_) => debug!(camera = %self.camera, "live feed ended"),
                    Err(error) => {
                        let text = failure_text(error);
                        warn!(camera = %self.camera, error = %text, "live feed failed");
                    }
                }
            }
            // A closed receiver means no view restarts its decoder for the
            // next session, nor shows anything of it: the feed ends.
            if !deliver(&events, FeedEvent::Restart, &cancel).await {
                return;
            }
            tokio::select! {
                () = cancel.cancelled() => return,
                () = tokio::time::sleep(delay) => {}
            }
        }
    }
}

/// Whether the log events of `target` may carry the bearer. The tungstenite
/// crate writes a WebSocket client's whole handshake request at trace level,
/// the `Authorization` header that carries the token of a [`Feed`] included.
/// It writes the request under the target `tungstenite::handshake::client`,
/// which this names. An application that installs a log subscriber must
/// drop every event whose target this names, whatever its filter says.
/// Otherwise a filter as broad as `RUST_LOG=trace` enables the line, and the
/// log then holds the root bearer.
///
/// # Example
///
/// ```
/// use tracing_subscriber::layer::SubscriberExt as _;
/// use tracing_subscriber::{EnvFilter, Layer as _, filter};
///
/// let sensitive = iris_transport::is_sensitive_log_target;
/// // An output layer that writes whatever the filter enables, trace level
/// // included, except a handshake request with the bearer in it.
/// let kept = filter::filter_fn(move |metadata| !sensitive(metadata.target()));
/// let output = tracing_subscriber::fmt::layer().with_filter(kept);
/// let _subscriber = tracing_subscriber::registry()
///     .with(EnvFilter::new("trace"))
///     .with(output);
/// assert!(
///     sensitive("tungstenite::handshake::client"),
///     "the handshake request with the bearer is dropped"
/// );
/// assert!(
///     !sensitive("tungstenite::handshake::machine"),
///     "the handshake's other lines are kept"
/// );
/// ```
#[must_use]
pub fn is_sensitive_log_target(target: &str) -> bool {
    target == "tungstenite::handshake::client"
}

/// How a session ended without a transport or protocol failure.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Ending {
    /// The server closed the stream, or the connection ended.
    Closed,
    /// A message exceeded [`LIVE_MESSAGE_BYTES`]: the daemon closed with code
    /// 1009, or the local limit refused the message.
    Oversized,
    /// The receiver of the feed's events closed as the session announced
    /// video's return: no view is left for the feed.
    Unheard,
}

/// Why a session failed. Each has the text of the error it holds, or of the
/// rule the message broke. The feed's "live feed failed" line, which the
/// Linux viewer logs, therefore names the failure in the words of what
/// refused it.
#[derive(Debug, Error)]
enum SessionError {
    /// The URL is not one tungstenite takes, or the connection, its
    /// handshake, or a read failed: a handshake the daemon answered with a
    /// status in place of an upgrade among them.
    #[error(transparent)]
    WebSocket(#[from] WebSocketError),
    /// A message broke the fixed framing.
    #[error(transparent)]
    Message(#[from] LiveError),
    /// The sink refused a message.
    #[error(transparent)]
    Refused(#[from] SinkError),
    /// A text message arrived on the binary feed.
    #[error("text message on the binary live feed")]
    Text,
}

/// Sends `event` unless `cancel` fires first, waiting for room in a full
/// channel. Gives `true` once the receiver holds the event. Gives `false`
/// when the receiver is closed or the feed is cancelled, either of which
/// ends the feed.
async fn deliver(events: &Sender<FeedEvent>, event: FeedEvent, cancel: &CancellationToken) -> bool {
    tokio::select! {
        () = cancel.cancelled() => false,
        sent = events.send(event) => sent.is_ok(),
    }
}

/// How one session went, for the loop that runs the feed: how it ended, and
/// whether its sink took a fragment, which ends the feed's back-off before the
/// ending is weighed.
struct Session {
    /// How the session ended.
    outcome: Result<Ending, SessionError>,
    /// Whether the sink took a fragment.
    delivered: bool,
}

/// How a session ended, for the feed's [`Reconnect`]: a handshake the daemon
/// answered with a status in place of an upgrade ends `Refused` with that
/// status, and any other failure ends `Failed`. A session whose receiver
/// closed has no ending: the feed ends without weighing it.
fn session_end(outcome: &Result<Ending, SessionError>) -> Option<SessionEnd> {
    Some(match outcome {
        Ok(Ending::Closed) => SessionEnd::Closed,
        Ok(Ending::Oversized) => SessionEnd::Oversized,
        Ok(Ending::Unheard) => return None,
        Err(SessionError::WebSocket(WebSocketError::Http(response))) => SessionEnd::Refused {
            status: response.status().as_u16(),
        },
        Err(_) => SessionEnd::Failed,
    })
}

/// The text of a failed session as the "live feed failed" line logs it: the
/// error and then each of its sources, after a colon apiece.
fn failure_text(error: &SessionError) -> String {
    use std::error::Error as _;

    let mut texts = vec![error.to_string()];
    let mut source = error.source();
    while let Some(cause) = source {
        texts.push(cause.to_string());
        source = cause.source();
    }
    texts.join(": ")
}

fn connect_request(url: &Url, token: Option<&Bearer>) -> Result<Request<()>, SessionError> {
    use tokio_tungstenite::tungstenite::client::IntoClientRequest as _;

    let mut request = url.as_str().into_client_request()?;
    if let Some(token) = token {
        let value = token.header().clone();
        request.headers_mut().insert(header::AUTHORIZATION, value);
    }
    Ok(request)
}

/// One connection: pushes every message into `sink` until the server closes,
/// and gives how the session ended and whether the sink took a fragment. A
/// text message, an empty message, or an unknown type byte breaks the fixed
/// protocol and fails the session, as does a message the sink refuses.
/// `connector` selects the TLS trust for `wss`; the feed passes its shared
/// configuration. `announce` receives [`FeedEvent::Live`] once the sink has
/// taken the first fragment: the session waits for room for it, and ends
/// [`Ending::Unheard`] when its receiver is closed. The handshake sends
/// `token` in `Authorization`, and tungstenite logs the whole request under
/// the target [`is_sensitive_log_target`] names.
async fn session<S: LiveSink + ?Sized>(
    url: &Url,
    token: Option<&Bearer>,
    connector: Option<Connector>,
    sink: &S,
    announce: Option<&Sender<FeedEvent>>,
) -> Session {
    use futures::StreamExt as _;

    let mut delivered = false;
    let outcome: Result<Ending, SessionError> = async {
        let request = connect_request(url, token)?;
        let (stream, _) = tokio_tungstenite::connect_async_tls_with_config(
            request,
            Some(websocket_config()),
            /* disable_nagle */ false,
            connector,
        )
        .await?;
        let (_, mut read) = stream.split();
        sink.reset();
        while let Some(message) = read.next().await {
            match message {
                Ok(Message::Binary(bytes)) => {
                    let live = live_message(&bytes)?;
                    sink.push(live)?;
                    if matches!(live, LiveMessage::Fragment(_)) && !delivered {
                        delivered = true;
                        // The view clears its oversized state on this event
                        // alone, so it waits for room rather than be lost.
                        if let Some(events) = announce
                            && events.send(FeedEvent::Live).await.is_err()
                        {
                            return Ok(Ending::Unheard);
                        }
                    }
                }
                Ok(Message::Close(frame)) => return Ok(close_ending(frame.as_ref())),
                Ok(Message::Text(_)) => return Err(SessionError::Text),
                // Pings, pongs, and raw frames: tungstenite answers pings itself.
                Ok(_) => {}
                Err(WebSocketError::Capacity(_)) => return Ok(Ending::Oversized),
                Err(error) => return Err(error.into()),
            }
        }
        Ok(Ending::Closed)
    }
    .await;
    Session { outcome, delivered }
}

/// Close code 1009 (Message Too Big) is the daemon refusing a frame above
/// the ceiling.
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

#[cfg(test)]
mod tests {
    use std::io;
    use std::net::SocketAddr;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    use async_channel::Receiver;
    use axum::extract::WebSocketUpgrade;
    use axum::extract::ws::{CloseFrame as AxumCloseFrame, Message as AxumMessage};
    use axum::{Router, routing};
    use iris_client::live::{MSG_FRAGMENT, MSG_INIT};
    use iris_client::origin::PlainHttp;
    use rcgen::{CertifiedKey, KeyPair};
    use tokio::net::TcpListener;
    use tokio::task::JoinHandle;
    use tokio::time::Instant;
    use tokio_rustls::TlsAcceptor;
    use tokio_rustls::rustls::pki_types::PrivatePkcs8KeyDer;
    use tokio_rustls::rustls::{self, ServerConfig};
    use tracing::field::{Field, Visit};
    use tracing::{Event, Subscriber};
    use tracing_subscriber::Layer;
    use tracing_subscriber::filter::{self, LevelFilter};
    use tracing_subscriber::layer::Context;

    use super::*;
    use crate::fixture;

    /// Bounds every network step of a test.
    const TEST_DEADLINE: Duration = Duration::from_secs(10);

    /// A sink that keeps the byte stream of its session, and refuses an empty
    /// payload as the Linux viewer's source does.
    #[derive(Debug, Default)]
    struct RecordingSink {
        bytes: Mutex<Vec<u8>>,
    }

    impl RecordingSink {
        /// The bytes taken since the session opened.
        fn queued_bytes(&self) -> u64 {
            self.bytes.lock().expect("recorded bytes").len() as u64
        }
    }

    impl LiveSink for RecordingSink {
        fn reset(&self) {
            self.bytes.lock().expect("recorded bytes").clear();
        }

        fn push(&self, message: LiveMessage<'_>) -> Result<(), SinkError> {
            let (LiveMessage::Init(payload) | LiveMessage::Fragment(payload)) = message else {
                // The sink refuses a kind of message it does not know.
                return Err(SinkError::new("live message of an unknown kind"));
            };
            if payload.is_empty() {
                return Err(SinkError::new("live input payload is empty"));
            }
            self.bytes
                .lock()
                .expect("recorded bytes")
                .extend_from_slice(payload);
            Ok(())
        }
    }

    fn feed(api: &str, token: Option<&str>) -> Feed {
        Feed {
            origin: ApiOrigin::parse(api, PlainHttp::Anywhere).unwrap(),
            token: token.map(fixture::bearer),
            camera: CameraId::try_new("cam1").unwrap(),
            kind: StreamKind::Sub,
            tls: iris_tls::client_config(None).unwrap(),
        }
    }

    #[test]
    fn an_http_origin_gives_a_ws_feed() {
        assert_eq!(
            feed("http://192.0.2.10:8554", None).url().as_str(),
            "ws://192.0.2.10:8554/api/streams/cam1/sub/live"
        );
    }

    #[test]
    fn an_https_origin_gives_a_wss_feed() {
        assert_eq!(
            feed("https://example.invalid/", None).url().as_str(),
            "wss://example.invalid/api/streams/cam1/sub/live"
        );
    }

    #[test]
    fn the_bearer_travels_in_the_handshake_header() {
        let feed = feed("http://192.0.2.10:8554", Some("t0ken"));
        let request = connect_request(&feed.url(), feed.token.as_ref()).unwrap();
        assert_eq!(request.headers()[header::AUTHORIZATION], "Bearer t0ken");
    }

    #[test]
    fn the_bearer_stays_out_of_the_feed_url() {
        let url = feed("http://192.0.2.10:8554", Some("t0ken")).url();
        assert!(
            !url.as_str().contains("t0ken") && url.query().is_none(),
            "{url}"
        );
    }

    #[test]
    fn the_feed_redacts_its_token() {
        let shown = format!("{:?}", feed("http://192.0.2.10:8554", Some("t0ken")));
        assert!(
            shown.contains("<redacted>") && !shown.contains("t0ken"),
            "{shown}"
        );
    }

    /// A failure that wraps another, as a broken handshake does.
    #[derive(Debug, Error)]
    #[error("handshake refused")]
    struct Wrapping(#[source] io::Error);

    /// A failed session without a source has the text of its failure alone.
    #[test]
    fn a_failure_without_a_source_has_its_text_alone() {
        let refused = SessionError::from(SinkError::new("live input payload is empty"));
        assert_eq!(failure_text(&refused), "live input payload is empty");
        assert_eq!(
            failure_text(&SessionError::Text),
            "text message on the binary live feed"
        );
    }

    /// A failed session has the text that the `{:#}` of `anyhow` writes of an
    /// error's chain: the failure and then each source after a colon, down
    /// the whole chain.
    #[test]
    fn a_failure_has_the_text_of_each_of_its_sources() {
        let reset = io::Error::other("connection reset");
        let broken = WebSocketError::Io(io::Error::other(Wrapping(reset)));
        // The text of the tungstenite error holds that of the I/O error,
        // whose source is the one it wraps: the chain repeats a text, as the
        // `{:#}` of `anyhow` does.
        assert_eq!(
            failure_text(&SessionError::from(broken)),
            "IO error: handshake refused: handshake refused: connection reset"
        );
    }

    #[test]
    fn the_websocket_admits_messages_of_at_most_the_live_message_ceiling() {
        let config = websocket_config();
        assert_eq!(
            (config.max_message_size, config.max_frame_size),
            (Some(LIVE_MESSAGE_BYTES), Some(LIVE_MESSAGE_BYTES))
        );
    }

    /// A TLS WebSocket listener on loopback with a self-signed certificate for
    /// 127.0.0.1. Each accepted session receives one init and one fragment.
    async fn tls_websocket_fixture() -> (Url, CertifiedKey<KeyPair>) {
        use futures::SinkExt as _;

        let certified = rcgen::generate_simple_self_signed(vec!["127.0.0.1".to_owned()]).unwrap();
        let key = PrivatePkcs8KeyDer::from(certified.signing_key.serialize_der());
        let config = ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(vec![certified.cert.der().clone()], key.into())
            .unwrap();
        let acceptor = TlsAcceptor::from(Arc::new(config));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
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

    /// Runs one `wss` session with the TLS fixture under the platform's
    /// trust alone, which does not hold the fixture's self-signed
    /// certificate; gives the session and its sink.
    async fn untrusted_wss_session() -> (Session, RecordingSink) {
        let (url, _certified) = tls_websocket_fixture().await;
        let sink = RecordingSink::default();
        let platform_only = iris_tls::client_config(None).unwrap();
        let session = tokio::time::timeout(
            TEST_DEADLINE,
            session(
                &url,
                Some(&fixture::bearer("fixture")),
                Some(Connector::Rustls(platform_only)),
                &sink,
                None,
            ),
        )
        .await
        .expect("the platform trust store decides before the deadline");
        (session, sink)
    }

    /// Runs one `wss` session with the TLS fixture under a trust that holds
    /// the fixture's certificate in its CA file; gives the session and its
    /// sink. An operator takes the same path with the proxy's authority in
    /// `--ca-file`.
    async fn trusted_wss_session() -> (Session, RecordingSink) {
        let (url, certified) = tls_websocket_fixture().await;
        let directory = tempfile::tempdir().unwrap();
        let ca_file = directory.path().join("proxy-ca.pem");
        std::fs::write(&ca_file, certified.cert.pem()).unwrap();
        let with_ca_file = iris_tls::client_config(Some(&ca_file)).unwrap();
        let sink = RecordingSink::default();
        let session = tokio::time::timeout(
            TEST_DEADLINE,
            session(
                &url,
                Some(&fixture::bearer("fixture")),
                Some(Connector::Rustls(with_ca_file)),
                &sink,
                None,
            ),
        )
        .await
        .expect("the trusted session ends before the deadline");
        (session, sink)
    }

    #[tokio::test]
    async fn a_wss_session_refuses_a_server_the_platform_does_not_trust() {
        let (refused_session, _sink) = untrusted_wss_session().await;
        let untrusted = refused_session
            .outcome
            .expect_err("a self-signed server is not in the platform trust store");
        let rustls_error = match &untrusted {
            SessionError::WebSocket(WebSocketError::Io(io)) => io
                .get_ref()
                .and_then(|inner| inner.downcast_ref::<rustls::Error>()),
            _ => None,
        };
        // Linux's verifier names an unknown issuer; macOS's says the system
        // does not trust the certificate, with another reason.
        #[cfg(target_os = "linux")]
        let refused = {
            use tokio_rustls::rustls::CertificateError;

            matches!(
                rustls_error,
                Some(rustls::Error::InvalidCertificate(
                    CertificateError::UnknownIssuer
                ))
            )
        };
        #[cfg(not(target_os = "linux"))]
        let refused = matches!(rustls_error, Some(rustls::Error::InvalidCertificate(_)));
        assert!(refused, "{}", failure_text(&untrusted));
    }

    #[tokio::test]
    async fn a_wss_session_with_an_untrusted_server_pushes_nothing_into_its_sink() {
        let (_refused_session, sink) = untrusted_wss_session().await;
        assert_eq!(sink.queued_bytes(), 0);
    }

    #[tokio::test]
    async fn a_wss_session_with_an_untrusted_server_reports_no_fragment_taken() {
        let (refused_session, _sink) = untrusted_wss_session().await;
        assert!(!refused_session.delivered, "no fragment was taken");
    }

    /// A `wss` session trusts a server whose authority is in the CA file:
    /// the session's two payloads reach the sink.
    #[tokio::test]
    async fn a_wss_session_trusts_a_server_whose_authority_is_in_the_ca_file() {
        let (trusted_session, sink) = trusted_wss_session().await;
        trusted_session
            .outcome
            .expect("WSS connects when the CA file names the fixture root");
        assert_eq!(sink.queued_bytes(), 5, "both payloads reach the sink");
    }

    #[tokio::test]
    async fn a_trusted_wss_session_reports_the_fragment_its_sink_took() {
        let (trusted_session, _sink) = trusted_wss_session().await;
        assert!(trusted_session.delivered, "the sink took the fragment");
    }

    /// The fixture closes each session after its fragment, and the trusted
    /// session ends as closed.
    #[tokio::test]
    async fn a_trusted_wss_session_ends_as_closed_when_the_server_closes_it() {
        let (trusted_session, _sink) = trusted_wss_session().await;
        let ending = trusted_session
            .outcome
            .expect("WSS connects when the CA file names the fixture root");
        assert_eq!(ending, Ending::Closed);
    }

    /// How the plain-WebSocket fixture ends each session after its init message.
    #[derive(Clone, Copy)]
    enum Script {
        /// One fragment with this many payload bytes, then a normal close.
        Fragment(usize),
        /// One fragment of four payload bytes, and then nothing: the session
        /// stays open until the client leaves it.
        Hold,
        /// One fragment of four payload bytes, then a close with code 1009.
        FragmentThenOversized,
        /// A close with code 1009, as the daemon sends for an oversized sync frame.
        Oversized,
        /// A close with code 1001 (Going Away), as a server that stops sends.
        Away,
        /// A binary message whose type byte the protocol does not define.
        UnknownType,
        /// A text message.
        Text,
    }

    async fn plain_websocket_fixture(script: Script) -> SocketAddr {
        scripted_websocket_fixture(&[script]).await
    }

    /// The close with code 1009 that the daemon sends for a frame above the
    /// ceiling.
    fn oversized_close() -> AxumMessage {
        AxumMessage::Close(Some(AxumCloseFrame {
            code: 1009,
            reason: "frame exceeds the live message ceiling".into(),
        }))
    }

    /// A plain-WebSocket fixture whose sessions follow `scripts` in turn,
    /// the last for every session after it.
    async fn scripted_websocket_fixture(scripts: &[Script]) -> SocketAddr {
        let scripts: Arc<[Script]> = scripts.into();
        let sessions = Arc::new(AtomicUsize::new(0));
        let app = Router::new().route(
            "/api/streams/{camera}/{kind}/live",
            routing::get(move |upgrade: WebSocketUpgrade| async move {
                let turn = sessions.fetch_add(1, Ordering::SeqCst);
                let script = scripts[turn.min(scripts.len() - 1)];
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
                        Script::Hold => {
                            let fragment = vec![MSG_FRAGMENT, 0, 0, 0, 0];
                            let _ = socket.send(AxumMessage::Binary(fragment.into())).await;
                            std::future::pending::<()>().await;
                        }
                        Script::FragmentThenOversized => {
                            let fragment = vec![MSG_FRAGMENT, 0, 0, 0, 0];
                            let _ = socket.send(AxumMessage::Binary(fragment.into())).await;
                            let _ = socket.send(oversized_close()).await;
                        }
                        Script::Oversized => {
                            let _ = socket.send(oversized_close()).await;
                        }
                        Script::Away => {
                            let frame = AxumCloseFrame {
                                code: 1001,
                                reason: "going away".into(),
                            };
                            let _ = socket.send(AxumMessage::Close(Some(frame))).await;
                        }
                        Script::UnknownType => {
                            let message = AxumMessage::Binary(vec![0x03, 1, 2].into());
                            let _ = socket.send(message).await;
                            let _ = socket.send(AxumMessage::Close(None)).await;
                        }
                        Script::Text => {
                            let _ = socket.send(AxumMessage::Text("hello".into())).await;
                            let _ = socket.send(AxumMessage::Close(None)).await;
                        }
                    }
                })
            }),
        );
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        address
    }

    async fn plain_session(address: SocketAddr) -> Result<Ending, SessionError> {
        let url = Url::parse(&format!("ws://{address}/api/streams/cam1/main/live")).unwrap();
        let sink = RecordingSink::default();
        tokio::time::timeout(TEST_DEADLINE, session(&url, None, None, &sink, None))
            .await
            .expect("the session ends before the deadline")
            .outcome
    }

    /// A close with a code other than 1009 ends the session as closed: a
    /// session the server closes with 1001, a close with each of several
    /// other codes, and a close without a frame.
    #[tokio::test]
    async fn a_close_other_than_1009_ends_the_session_as_closed() {
        let away = plain_websocket_fixture(Script::Away).await;
        assert_eq!(plain_session(away).await.unwrap(), Ending::Closed);
        for code in [1000, 1001, 1002, 1008, 1010, 1011, 4000] {
            let frame = CloseFrame {
                code: CloseCode::from(code),
                reason: "".into(),
            };
            assert_eq!(close_ending(Some(&frame)), Ending::Closed, "{code}");
        }
        assert_eq!(close_ending(None), Ending::Closed);
    }

    /// A close with 1009, the daemon's refusal of a frame above the ceiling,
    /// ends the session as oversized.
    #[tokio::test]
    async fn a_close_with_1009_ends_the_session_as_oversized() {
        let refused = plain_websocket_fixture(Script::Oversized).await;
        assert_eq!(plain_session(refused).await.unwrap(), Ending::Oversized);
    }

    /// The sink takes a frame of 15 MiB, below the ceiling, whole: it holds
    /// the four payload bytes of the init segment and the frame's 15 MiB.
    #[tokio::test]
    async fn a_frame_below_the_ceiling_is_taken() {
        let below = plain_websocket_fixture(Script::Fragment(15 << 20)).await;
        let url = Url::parse(&format!("ws://{below}/api/streams/cam1/main/live")).unwrap();
        let sink = RecordingSink::default();
        tokio::time::timeout(TEST_DEADLINE, session(&url, None, None, &sink, None))
            .await
            .expect("the session ends before the deadline");
        assert_eq!(sink.queued_bytes(), 4 + (15 << 20));
    }

    /// A session whose one frame is 15 MiB, below the ceiling, ends as
    /// closed when the server closes it, not as oversized.
    #[tokio::test]
    async fn a_session_with_a_frame_below_the_ceiling_ends_as_closed() {
        let below = plain_websocket_fixture(Script::Fragment(15 << 20)).await;
        assert_eq!(plain_session(below).await.unwrap(), Ending::Closed);
    }

    #[tokio::test]
    async fn frames_above_the_ceiling_end_the_session_as_oversized() {
        for payload in [(16 << 20) + 4096, 20 << 20] {
            let above = plain_websocket_fixture(Script::Fragment(payload)).await;
            assert_eq!(
                plain_session(above).await.unwrap(),
                Ending::Oversized,
                "{payload} bytes"
            );
        }
    }

    /// The sink takes a message of [`LIVE_MESSAGE_BYTES`], the ceiling,
    /// whole: it holds the four payload bytes of the init segment and the
    /// payload of the fragment, the message less its type byte.
    #[tokio::test]
    async fn a_message_at_the_ceiling_is_taken() {
        let payload = LIVE_MESSAGE_BYTES - 1;
        let at = plain_websocket_fixture(Script::Fragment(payload)).await;
        let url = Url::parse(&format!("ws://{at}/api/streams/cam1/main/live")).unwrap();
        let sink = RecordingSink::default();
        tokio::time::timeout(TEST_DEADLINE, session(&url, None, None, &sink, None))
            .await
            .expect("the session ends before the deadline");
        assert_eq!(sink.queued_bytes(), 4 + payload as u64);
    }

    /// A message one byte longer than [`LIVE_MESSAGE_BYTES`] ends the
    /// session as oversized.
    #[tokio::test]
    async fn a_message_one_byte_above_the_ceiling_ends_the_session_as_oversized() {
        let above = plain_websocket_fixture(Script::Fragment(LIVE_MESSAGE_BYTES)).await;
        assert_eq!(plain_session(above).await.unwrap(), Ending::Oversized);
    }

    #[tokio::test]
    async fn an_unknown_message_type_fails_the_session() {
        let address = plain_websocket_fixture(Script::UnknownType).await;
        let error = plain_session(address).await.unwrap_err();
        assert!(
            failure_text(&error).contains("unknown live message type"),
            "{}",
            failure_text(&error)
        );
    }

    #[tokio::test]
    async fn a_text_message_fails_the_session() {
        let address = plain_websocket_fixture(Script::Text).await;
        let error = plain_session(address).await.unwrap_err();
        assert!(
            failure_text(&error).contains("text message"),
            "{}",
            failure_text(&error)
        );
    }

    #[tokio::test]
    async fn an_oversized_stream_backs_off_instead_of_reconnecting_every_two_seconds() {
        // The test waits out the real 2 s pause: a paused clock would jump to
        // the deadline whenever the runtime waited on the loopback socket.
        let address = plain_websocket_fixture(Script::Oversized).await;
        let feed = Feed {
            kind: StreamKind::Main,
            ..feed(&format!("http://{address}"), None)
        };
        let (events_tx, events_rx) = async_channel::unbounded();
        let cancel = CancellationToken::new();
        let task = tokio::spawn(feed.run(
            Arc::new(RecordingSink::default()),
            events_tx,
            cancel.clone(),
        ));
        let mut oversized = Vec::new();
        let deadline = Instant::now() + TEST_DEADLINE;
        while oversized.len() < 2 {
            let event = tokio::time::timeout_at(deadline, events_rx.recv())
                .await
                .expect("the feed reports each attempt before the deadline")
                .expect("the feed keeps its event channel open");
            if let FeedEvent::Oversized { retry_in } = event {
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

    /// The origin of a daemon with no route, which answers every upgrade
    /// with 404, as a daemon that records no audio for a camera answers its
    /// audio feed.
    async fn refusing_daemon() -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, Router::new()).await });
        format!("http://{address}")
    }

    /// Runs the audio feed of the camera `camera` against a daemon that
    /// answers it with 404, until it ends; gives the events it sent.
    async fn refused_audio_feed(camera: &str) -> Receiver<FeedEvent> {
        let api = refusing_daemon().await;
        let (events_tx, events_rx) = async_channel::unbounded();
        let sound = Feed {
            camera: CameraId::try_new(camera).unwrap(),
            kind: StreamKind::Audio,
            ..feed(&api, None)
        };
        tokio::time::timeout(
            TEST_DEADLINE,
            sound.run(
                Arc::new(RecordingSink::default()),
                events_tx,
                CancellationToken::new(),
            ),
        )
        .await
        .expect("the audio feed ends");
        events_rx
    }

    /// An audio feed that the daemon answers with 404 ends, with nothing to
    /// cancel it: the camera records no audio.
    #[tokio::test]
    async fn an_audio_feed_the_daemon_does_not_have_ends() {
        refused_audio_feed("cam1").await;
    }

    /// An audio feed that ends sends the view no restart, since no session
    /// follows it.
    #[tokio::test]
    async fn an_audio_feed_the_daemon_does_not_have_asks_for_no_restart() {
        let events = refused_audio_feed("cam1").await;
        assert!(events.try_recv().is_err(), "no restart was asked");
    }

    /// A video feed that the daemon answers with 404 keeps trying.
    #[tokio::test]
    async fn a_video_feed_the_daemon_refuses_keeps_trying() {
        let api = refusing_daemon().await;
        let (events_tx, events_rx) = async_channel::unbounded();
        let cancel = CancellationToken::new();
        let picture = tokio::spawn(feed(&api, None).run(
            Arc::new(RecordingSink::default()),
            events_tx,
            cancel.clone(),
        ));
        let event = next_event(&events_rx).await;
        assert!(
            event == FeedEvent::Restart && !picture.is_finished(),
            "the video feed restarts the view and keeps trying: {event:?}"
        );
        cancel.cancel();
        tokio::time::timeout(TEST_DEADLINE, picture)
            .await
            .expect("the cancelled feed ends")
            .unwrap();
    }

    /// The next event a feed sends, within the test's deadline.
    async fn next_event(events: &Receiver<FeedEvent>) -> FeedEvent {
        tokio::time::timeout(TEST_DEADLINE, events.recv())
            .await
            .expect("the feed sends an event before the deadline")
            .expect("the feed keeps its event channel open")
    }

    /// Waits, within the test's deadline, until `sink` holds `bytes` payload
    /// bytes of the session that pushes into it.
    async fn until_holding(sink: &RecordingSink, bytes: u64) {
        tokio::time::timeout(TEST_DEADLINE, async {
            while sink.queued_bytes() < bytes {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("the session delivers before the deadline");
    }

    /// A feed that backs off after a frame above the ceiling announces
    /// video's return once the next session's sink takes a fragment. On an
    /// unbounded channel, as the Linux viewer's is, the events come in order:
    /// the pause, the restart before the next session, video's return, and
    /// the restart after it. The tile leaves its oversized state on video's
    /// return.
    #[tokio::test]
    async fn the_session_after_a_back_off_announces_the_return_of_video() {
        // The test waits out the real 2 s pause: a paused clock would jump to
        // the deadline whenever the runtime waited on the loopback socket.
        let address = scripted_websocket_fixture(&[Script::Oversized, Script::Fragment(4)]).await;
        let (events_tx, events_rx) = async_channel::unbounded();
        let cancel = CancellationToken::new();
        let running = tokio::spawn(feed(&format!("http://{address}"), None).run(
            Arc::new(RecordingSink::default()),
            events_tx,
            cancel.clone(),
        ));
        let mut events = Vec::new();
        for _ in 0..4 {
            events.push(next_event(&events_rx).await);
        }
        cancel.cancel();
        tokio::time::timeout(TEST_DEADLINE, running)
            .await
            .expect("cancellation stops the feed")
            .unwrap();
        assert_eq!(
            events,
            [
                FeedEvent::Oversized {
                    retry_in: Duration::from_secs(2)
                },
                FeedEvent::Restart,
                FeedEvent::Live,
                FeedEvent::Restart
            ]
        );
    }

    /// Cancelling a feed ends it while a session is open, not only while it
    /// waits between sessions: a tile that stops ends its feed at once.
    #[tokio::test]
    async fn a_feed_cancelled_while_its_session_is_open_ends() {
        let address = plain_websocket_fixture(Script::Hold).await;
        let sink = Arc::new(RecordingSink::default());
        let (events_tx, events_rx) = async_channel::unbounded();
        let cancel = CancellationToken::new();
        let running = tokio::spawn(feed(&format!("http://{address}"), None).run(
            Arc::clone(&sink),
            events_tx,
            cancel.clone(),
        ));
        // The session is open once its sink holds the init segment and the
        // fragment; the fixture sends nothing more and keeps it open.
        until_holding(&sink, 8).await;
        cancel.cancel();
        tokio::time::timeout(TEST_DEADLINE, running)
            .await
            .expect("cancellation ends the feed in its open session")
            .unwrap();
        assert!(
            events_rx.is_empty(),
            "the feed sent no event, so it ended inside its open session"
        );
    }

    /// Runs a feed whose receiver is closed against a fixture that ends its
    /// session as `script` says, until the feed ends with nothing to cancel
    /// it.
    async fn feed_with_its_receiver_closed(script: Script) {
        let address = plain_websocket_fixture(script).await;
        let (events_tx, events_rx) = async_channel::unbounded();
        drop(events_rx);
        tokio::time::timeout(
            TEST_DEADLINE,
            feed(&format!("http://{address}"), None).run(
                Arc::new(RecordingSink::default()),
                events_tx,
                CancellationToken::new(),
            ),
        )
        .await
        .expect("the feed ends with nothing to cancel it");
    }

    /// A feed whose receiver is closed ends after a session that closed: no
    /// view is left to restart its decoder.
    #[tokio::test]
    async fn a_feed_whose_receiver_closed_ends_after_a_closed_session() {
        feed_with_its_receiver_closed(Script::Away).await;
    }

    /// A feed whose receiver is closed ends after a frame above the ceiling:
    /// no view is left to show the pause.
    #[tokio::test]
    async fn a_feed_whose_receiver_closed_ends_after_a_frame_above_the_ceiling() {
        feed_with_its_receiver_closed(Script::Oversized).await;
    }

    /// A feed whose receiver closed during a session ends without a log
    /// line for that session. The one line it logs is the first session's:
    /// the frame above the ceiling that starts the back-off, without which
    /// no session announces video's return.
    #[tokio::test]
    async fn a_feed_whose_receiver_closed_during_a_session_logs_no_line_for_it() {
        // The test waits out the real 2 s pause: a paused clock would jump to
        // the deadline whenever the runtime waited on the loopback socket.
        assert_eq!(fixture::logged("camera=unheard"), Vec::<String>::new());
        let address = scripted_websocket_fixture(&[Script::Oversized, Script::Hold]).await;
        let (events_tx, events_rx) = async_channel::unbounded();
        let running = tokio::spawn(feed_of(address, "unheard", StreamKind::Main).run(
            Arc::new(RecordingSink::default()),
            events_tx,
            CancellationToken::new(),
        ));
        // The first session's pause and restart; then no view is left.
        next_event(&events_rx).await;
        next_event(&events_rx).await;
        drop(events_rx);
        tokio::time::timeout(TEST_DEADLINE, running)
            .await
            .expect("the feed ends in its held session")
            .unwrap();
        assert_eq!(
            fixture::logged("camera=unheard"),
            [concat!(
                " WARN iris_transport::feed: live frame exceeds the message ceiling ",
                "camera=unheard limit_bytes=16777217 retry_s=2"
            )]
        );
    }

    /// A session whose sink took a fragment ends the back-off before the
    /// session's ending is weighed. The second session here delivers a
    /// fragment and then ends with a frame above the ceiling, so its pause
    /// starts again at 2 s and does not double to 4 s.
    #[tokio::test]
    async fn a_session_that_delivered_video_starts_the_back_off_afresh() {
        // The test waits out the real 2 s pause: a paused clock would jump to
        // the deadline whenever the runtime waited on the loopback socket.
        let scripts = [Script::Oversized, Script::FragmentThenOversized];
        let address = scripted_websocket_fixture(&scripts).await;
        let (events_tx, events_rx) = async_channel::unbounded();
        let cancel = CancellationToken::new();
        let running = tokio::spawn(feed(&format!("http://{address}"), None).run(
            Arc::new(RecordingSink::default()),
            events_tx,
            cancel.clone(),
        ));
        let mut pauses = Vec::new();
        // The restarts between the pauses keep arriving, so the deadline is
        // on the two pauses together, not on each event.
        tokio::time::timeout(TEST_DEADLINE, async {
            while pauses.len() < 2 {
                if let FeedEvent::Oversized { retry_in } = next_event(&events_rx).await {
                    pauses.push(retry_in);
                }
            }
        })
        .await
        .expect("the feed reports two pauses before the deadline");
        cancel.cancel();
        tokio::time::timeout(TEST_DEADLINE, running)
            .await
            .expect("cancellation stops the feed")
            .unwrap();
        assert_eq!(pauses, [Duration::from_secs(2), Duration::from_secs(2)]);
    }

    /// A fragment that arrives but that the sink refuses does not end the
    /// back-off. The second session's one fragment has an empty payload,
    /// which the sink refuses, so the pause after the third session doubles
    /// to 4 s.
    #[tokio::test]
    async fn a_fragment_the_sink_refuses_does_not_end_the_back_off() {
        // The test waits out the real pauses of 2 s: a paused clock would
        // jump to the deadline whenever the runtime waited on the loopback
        // socket.
        let scripts = [Script::Oversized, Script::Fragment(0), Script::Oversized];
        let address = scripted_websocket_fixture(&scripts).await;
        let (events_tx, events_rx) = async_channel::unbounded();
        let cancel = CancellationToken::new();
        let running = tokio::spawn(feed(&format!("http://{address}"), None).run(
            Arc::new(RecordingSink::default()),
            events_tx,
            cancel.clone(),
        ));
        let mut pauses = Vec::new();
        // The restarts between the pauses keep arriving, so the deadline is
        // on the two pauses together, not on each event.
        tokio::time::timeout(TEST_DEADLINE, async {
            while pauses.len() < 2 {
                if let FeedEvent::Oversized { retry_in } = next_event(&events_rx).await {
                    pauses.push(retry_in);
                }
            }
        })
        .await
        .expect("the feed reports two pauses before the deadline");
        cancel.cancel();
        tokio::time::timeout(TEST_DEADLINE, running)
            .await
            .expect("cancellation stops the feed")
            .unwrap();
        assert_eq!(pauses, [Duration::from_secs(2), Duration::from_secs(4)]);
    }

    /// A feed whose receiver closes while it backs off ends as its next
    /// session announces video's return, though that session stays open.
    #[tokio::test]
    async fn a_feed_whose_receiver_closed_ends_as_it_announces_video() {
        // The test waits out the real 2 s pause: a paused clock would jump to
        // the deadline whenever the runtime waited on the loopback socket.
        let address = scripted_websocket_fixture(&[Script::Oversized, Script::Hold]).await;
        let (events_tx, events_rx) = async_channel::unbounded();
        let running = tokio::spawn(feed(&format!("http://{address}"), None).run(
            Arc::new(RecordingSink::default()),
            events_tx,
            CancellationToken::new(),
        ));
        let pause = Duration::from_secs(2);
        assert_eq!(
            next_event(&events_rx).await,
            FeedEvent::Oversized { retry_in: pause }
        );
        assert_eq!(next_event(&events_rx).await, FeedEvent::Restart);
        // The next session opens after the pause, with no view left.
        drop(events_rx);
        tokio::time::timeout(TEST_DEADLINE, running)
            .await
            .expect("the feed ends in its held session")
            .unwrap();
    }

    /// With a channel of one place, the feed waits for room for each event.
    /// The second session's announcement of video's return finds the place
    /// taken by the restart before it. The announcement arrives once the
    /// restart is read, and is not lost.
    #[tokio::test]
    async fn the_return_of_video_waits_for_room_in_a_full_channel() {
        // The test waits out the real 2 s pause: a paused clock would jump to
        // the deadline whenever the runtime waited on the loopback socket.
        let address = scripted_websocket_fixture(&[Script::Oversized, Script::Fragment(4)]).await;
        let sink = Arc::new(RecordingSink::default());
        let (events_tx, events_rx) = async_channel::bounded(1);
        let cancel = CancellationToken::new();
        let running = tokio::spawn(feed(&format!("http://{address}"), None).run(
            Arc::clone(&sink),
            events_tx,
            cancel.clone(),
        ));
        assert_eq!(
            next_event(&events_rx).await,
            FeedEvent::Oversized {
                retry_in: Duration::from_secs(2)
            }
        );
        // The restart takes the one place. The second session's sink then
        // holds its init segment and its fragment, four payload bytes each,
        // and the feed waits to announce video's return.
        until_holding(&sink, 8).await;
        assert_eq!(next_event(&events_rx).await, FeedEvent::Restart);
        assert_eq!(next_event(&events_rx).await, FeedEvent::Live);
        assert_eq!(next_event(&events_rx).await, FeedEvent::Restart);
        cancel.cancel();
        tokio::time::timeout(TEST_DEADLINE, running)
            .await
            .expect("cancellation stops the feed")
            .unwrap();
    }

    /// A feed cancelled while it waits for room in a full channel ends: the
    /// oversized session's announcement takes the one place, nothing reads
    /// it, and the restart after it waits until the cancellation.
    #[tokio::test]
    async fn a_feed_cancelled_while_it_waits_for_room_ends() {
        let address = plain_websocket_fixture(Script::Oversized).await;
        let (events_tx, events_rx) = async_channel::bounded(1);
        let cancel = CancellationToken::new();
        let running = tokio::spawn(feed(&format!("http://{address}"), None).run(
            Arc::new(RecordingSink::default()),
            events_tx,
            cancel.clone(),
        ));
        tokio::time::timeout(TEST_DEADLINE, async {
            while !events_rx.is_full() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("the oversized session fills the channel before the deadline");
        cancel.cancel();
        tokio::time::timeout(TEST_DEADLINE, running)
            .await
            .expect("cancellation ends the feed that waits for room")
            .unwrap();
        assert_eq!(events_rx.len(), 1, "the restart never found room");
    }

    /// A feed cancelled while it waits for room to report a frame above the
    /// ceiling ends. The channel's one place is taken before the feed runs.
    /// The feed logs the frame, and then waits to send `Oversized` until the
    /// cancellation.
    #[tokio::test]
    async fn a_feed_cancelled_while_it_waits_to_report_an_oversized_frame_ends() {
        assert_eq!(fixture::logged("camera=crowded"), Vec::<String>::new());
        let address = plain_websocket_fixture(Script::Oversized).await;
        let (events_tx, events_rx) = async_channel::bounded(1);
        events_tx
            .try_send(FeedEvent::Restart)
            .expect("the empty channel takes the event that fills it");
        let cancel = CancellationToken::new();
        let running = tokio::spawn(feed_of(address, "crowded", StreamKind::Main).run(
            Arc::new(RecordingSink::default()),
            events_tx,
            cancel.clone(),
        ));
        let reported = || {
            fixture::logged("camera=crowded")
                .iter()
                .any(|line| line.contains("live frame exceeds the message ceiling"))
        };
        tokio::time::timeout(TEST_DEADLINE, async {
            while !reported() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("the feed logs the frame above the ceiling before the deadline");
        cancel.cancel();
        tokio::time::timeout(TEST_DEADLINE, running)
            .await
            .expect("cancellation ends the feed that waits to report the frame")
            .unwrap();
        assert_eq!(
            events_rx.try_recv(),
            Ok(FeedEvent::Restart),
            "the report never found room"
        );
    }

    /// A feed of the `kind` stream of `camera` on the fixture at `address`.
    fn feed_of(address: SocketAddr, camera: &str, kind: StreamKind) -> Feed {
        Feed {
            camera: CameraId::try_new(camera).unwrap(),
            kind,
            ..feed(&format!("http://{address}"), None)
        }
    }

    /// Runs `feed` into a recording sink until it asks for its first
    /// restart, and returns the events it sent until then.
    async fn first_session(feed: Feed) -> Vec<FeedEvent> {
        let (events_tx, events_rx) = async_channel::unbounded();
        let cancel = CancellationToken::new();
        let task = tokio::spawn(feed.run(
            Arc::new(RecordingSink::default()),
            events_tx,
            cancel.clone(),
        ));
        let mut events = Vec::new();
        while events.last() != Some(&FeedEvent::Restart) {
            events.push(next_event(&events_rx).await);
        }
        cancel.cancel();
        tokio::time::timeout(TEST_DEADLINE, task)
            .await
            .expect("cancellation stops the feed")
            .unwrap();
        events
    }

    /// A payload the sink refuses fails the session, whose line names the
    /// sink's reason. A fragment of the type byte alone has an empty payload.
    #[tokio::test]
    async fn a_refused_payload_fails_the_session_with_the_sinks_reason() {
        assert_eq!(fixture::logged("camera=refuser"), Vec::<String>::new());
        let address = plain_websocket_fixture(Script::Fragment(0)).await;
        first_session(feed_of(address, "refuser", StreamKind::Main)).await;
        assert_eq!(
            fixture::logged("camera=refuser"),
            [concat!(
                " WARN iris_transport::feed: live feed failed camera=refuser ",
                "error=live input payload is empty"
            )]
        );
    }

    /// After a session that failed on a refused payload, the feed restarts
    /// the view to connect again, and sends nothing before.
    #[tokio::test]
    async fn a_refused_payload_restarts_the_view() {
        let address = plain_websocket_fixture(Script::Fragment(0)).await;
        let events = first_session(feed_of(address, "restarter", StreamKind::Main)).await;
        assert_eq!(events, [FeedEvent::Restart]);
    }

    /// After a session that the server closed, the feed restarts the view
    /// to connect again, and sends nothing before.
    #[tokio::test]
    async fn a_closed_session_restarts_the_view() {
        let closing = plain_websocket_fixture(Script::Fragment(4)).await;
        let events = first_session(feed_of(closing, "reopener", StreamKind::Main)).await;
        assert_eq!(events, [FeedEvent::Restart]);
    }

    /// After a frame above the ceiling, the feed reports the pause to the
    /// view and then restarts it.
    #[tokio::test]
    async fn a_frame_above_the_ceiling_reports_the_pause_before_the_restart() {
        let oversized = plain_websocket_fixture(Script::Oversized).await;
        let events = first_session(feed_of(oversized, "pauser", StreamKind::Sub)).await;
        assert_eq!(
            events,
            [
                FeedEvent::Oversized {
                    retry_in: Duration::from_secs(2)
                },
                FeedEvent::Restart
            ]
        );
    }

    /// A closed session logs its line at debug level, with the feed's camera.
    /// The test [`a_refused_payload_fails_the_session_with_the_sinks_reason`]
    /// checks the line of a failed session.
    #[tokio::test]
    async fn a_closed_session_logs_its_line() {
        assert_eq!(fixture::logged("camera=closer"), Vec::<String>::new());
        let closing = plain_websocket_fixture(Script::Fragment(4)).await;
        first_session(feed_of(closing, "closer", StreamKind::Main)).await;
        assert_eq!(
            fixture::logged("camera=closer"),
            ["DEBUG iris_transport::feed: live feed ended camera=closer"]
        );
    }

    /// A frame above the ceiling logs a warning with the feed's camera, the
    /// limit, and the pause.
    #[tokio::test]
    async fn a_frame_above_the_ceiling_logs_its_line() {
        assert_eq!(fixture::logged("camera=oversized"), Vec::<String>::new());
        let oversized = plain_websocket_fixture(Script::Oversized).await;
        first_session(feed_of(oversized, "oversized", StreamKind::Sub)).await;
        assert_eq!(
            fixture::logged("camera=oversized"),
            [concat!(
                " WARN iris_transport::feed: live frame exceeds the message ceiling ",
                "camera=oversized limit_bytes=16777217 retry_s=2"
            )]
        );
    }

    /// An audio feed that ends logs its line at debug level, with the feed's
    /// camera.
    #[tokio::test]
    async fn an_audio_feed_that_ends_logs_its_line() {
        assert_eq!(fixture::logged("camera=silent"), Vec::<String>::new());
        refused_audio_feed("silent").await;
        assert_eq!(
            fixture::logged("camera=silent"),
            ["DEBUG iris_transport::feed: the camera has no live audio camera=silent"]
        );
    }

    /// Every event a subscriber hands it, as the target a filter reads and
    /// the text of its fields. The tungstenite crate writes through `log`.
    /// Its records reach a subscriber as events of the target `log`, with
    /// the record's target in the field `log.target`. That is the target a
    /// filter reads, and the one the recorder keeps.
    #[derive(Clone, Debug, Default)]
    struct Recorder(Arc<Mutex<Vec<(String, String)>>>);

    impl Recorder {
        /// The events recorded so far, each as its target and its text.
        fn events(&self) -> Vec<(String, String)> {
            self.0.lock().expect("recorded events").clone()
        }
    }

    /// The fields of one event: a `log` record's target, and the text of
    /// every other field.
    #[derive(Default)]
    struct Fields {
        target: Option<String>,
        text: String,
    }

    impl Visit for Fields {
        fn record_str(&mut self, field: &Field, value: &str) {
            if field.name() == "log.target" {
                self.target = Some(value.to_owned());
            } else {
                self.record_debug(field, &value);
            }
        }

        fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
            use std::fmt::Write as _;

            write!(self.text, " {field}={value:?}").expect("a String takes any text");
        }
    }

    impl<S: Subscriber> Layer<S> for Recorder {
        fn on_event(&self, event: &Event<'_>, _: Context<'_, S>) {
            let mut fields = Fields::default();
            event.record(&mut fields);
            let target = fields
                .target
                .unwrap_or_else(|| event.metadata().target().to_owned());
            self.0
                .lock()
                .expect("recorded events")
                .push((target, fields.text));
        }
    }

    /// A listener on the loopback that reads one handshake request and
    /// answers it with 400 in place of an upgrade. Gives the listener's
    /// address and the task that returns the request it read.
    async fn refusing_fixture() -> (SocketAddr, JoinHandle<Vec<u8>>) {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            let mut buffer = [0_u8; 1024];
            while !request.windows(4).any(|end| end == b"\r\n\r\n") {
                let read = socket.read(&mut buffer).await.unwrap();
                if read == 0 {
                    break;
                }
                request.extend_from_slice(&buffer[..read]);
            }
            socket
                .write_all(b"HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\n\r\n")
                .await
                .unwrap();
            request
        });
        (address, server)
    }

    /// The handshake request that the server of [`refusing_fixture`] read,
    /// within the test's deadline, so that a handshake that never arrives
    /// fails the test.
    async fn handshake_request(server: JoinHandle<Vec<u8>>) -> String {
        let request = tokio::time::timeout(TEST_DEADLINE, server)
            .await
            .expect("the fixture reads a handshake before the deadline")
            .unwrap();
        String::from_utf8_lossy(&request).into_owned()
    }

    /// The URL of the main stream's live feed on the fixture at `address`.
    fn live_url(address: SocketAddr) -> Url {
        Url::parse(&format!("ws://{address}/api/streams/cam1/main/live")).unwrap()
    }

    /// A handshake that the daemon answers with a status in place of an
    /// upgrade ends the session as refused with that status, here 400.
    #[tokio::test]
    async fn a_handshake_answered_with_a_status_ends_the_session_as_refused_with_it() {
        let (address, server) = refusing_fixture().await;
        let sink = RecordingSink::default();
        let refused = tokio::time::timeout(
            TEST_DEADLINE,
            session(&live_url(address), None, None, &sink, None),
        )
        .await
        .expect("the handshake ends before the deadline");
        server.abort();
        assert_eq!(
            session_end(&refused.outcome),
            Some(SessionEnd::Refused { status: 400 })
        );
    }

    /// The value of the `Authorization` header in the handshake `request`.
    fn authorization(request: &str) -> Option<&str> {
        request.lines().find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("authorization")
                .then_some(value.trim())
        })
    }

    /// A feed that runs sends its bearer in the handshake of its session:
    /// the request the server reads holds `Bearer`, a space, and the token
    /// in `Authorization`.
    #[tokio::test]
    async fn a_feed_sends_its_bearer_in_the_handshake() {
        let (address, server) = refusing_fixture().await;
        let (events_tx, _events_rx) = async_channel::unbounded();
        let cancel = CancellationToken::new();
        let running = tokio::spawn(feed(&format!("http://{address}"), Some("feed-t0ken")).run(
            Arc::new(RecordingSink::default()),
            events_tx,
            cancel.clone(),
        ));
        let request = handshake_request(server).await;
        cancel.cancel();
        tokio::time::timeout(TEST_DEADLINE, running)
            .await
            .expect("cancellation stops the feed")
            .unwrap();
        assert_eq!(authorization(&request), Some("Bearer feed-t0ken"));
    }

    /// The bearer of the traced handshakes, which no other line holds.
    const SENTINEL: &str = "client-v01-handshake-sentinel";

    /// The events of one real handshake with the bearer [`SENTINEL`], which
    /// the server refuses with 400, as two recorders see them. The first
    /// records every target at trace level. The second is a layer whose
    /// filter drops the targets that [`is_sensitive_log_target`] names.
    async fn traced_handshake() -> (Vec<(String, String)>, Vec<(String, String)>) {
        use tracing_subscriber::layer::SubscriberExt as _;
        use tracing_subscriber::util::SubscriberInitExt as _;

        let everything = Recorder::default();
        let guarded = Recorder::default();
        let subscriber = tracing_subscriber::registry()
            .with(everything.clone().with_filter(LevelFilter::TRACE))
            .with(guarded.clone().with_filter(filter::filter_fn(|metadata| {
                !is_sensitive_log_target(metadata.target())
            })));
        // The default of this thread alone, which runs the whole handshake.
        // `set_default` also forwards the `log` records tungstenite writes to
        // the thread's subscriber, at every level.
        let default = subscriber.set_default();
        let (address, server) = refusing_fixture().await;
        let url = live_url(address);
        let sink = RecordingSink::default();
        tokio::time::timeout(
            TEST_DEADLINE,
            session(&url, Some(&fixture::bearer(SENTINEL)), None, &sink, None),
        )
        .await
        .expect("the handshake ends before the deadline");
        drop(default);
        handshake_request(server).await;
        (everything.events(), guarded.events())
    }

    /// The tungstenite crate writes the whole handshake request at trace
    /// level, the `Authorization` that carries the bearer included, and only
    /// under the target [`is_sensitive_log_target`] names. A tungstenite that
    /// wrote the request under another target fails this test, where a
    /// viewer would leak the bearer.
    #[tokio::test]
    async fn tungstenite_traces_the_bearer_under_the_sensitive_target_alone() {
        let (everything, _) = traced_handshake().await;
        let holding: Vec<String> = everything
            .into_iter()
            .filter(|(_, text)| text.contains(SENTINEL))
            .map(|(target, _)| target)
            .collect();
        assert!(
            !holding.is_empty() && holding.iter().all(|target| is_sensitive_log_target(target)),
            "the bearer is written under {holding:?}"
        );
    }

    #[tokio::test]
    async fn a_layer_filtered_by_the_guard_holds_no_bearer() {
        let (_, kept) = traced_handshake().await;
        assert!(
            kept.iter().all(|(_, text)| !text.contains(SENTINEL)),
            "the guard keeps {kept:?}"
        );
    }

    #[tokio::test]
    async fn a_layer_filtered_by_the_guard_keeps_the_handshakes_other_lines() {
        let (_, kept) = traced_handshake().await;
        assert!(
            kept.iter()
                .any(|(target, _)| target.starts_with("tungstenite::")),
            "the guard keeps {kept:?}"
        );
    }
}
