//! The fixtures that the tests of the client, the export, and the feed share.
//! They are a server on the loopback and a client of it, the answers the
//! fixtures serve, a server whose answer is cut short, and the log the tests
//! capture. Each is called through the module, `fixture::client(origin)`,
//! since its name says nothing of it alone.

use std::io;
use std::sync::{Arc, Mutex, Once};

use axum::Router;
use axum::body::Body;
use axum::http::{StatusCode, header};
use axum::response::Response;
use iris_client::api::{EventId, Window};
use iris_client::origin::{ApiOrigin, PlainHttp};
use iris_core::id::CameraId;
use iris_core::time::WallUs;
use iris_tls::ClientConfig;
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;
use tracing::Level;

use crate::client::{Bearer, Client};

/// The bearer of `token`, which a test gives in visible ASCII.
pub(crate) fn bearer(token: &str) -> Bearer {
    Bearer::try_new(token).unwrap()
}

/// The camera id `id`, which a test gives valid: 1 to 32 characters of
/// `[a-z0-9_-]`.
pub(crate) fn camera(id: &str) -> CameraId {
    CameraId::try_new(id).unwrap()
}

/// The wall-clock time `micros` microseconds after the epoch.
pub(crate) fn time(micros: i64) -> WallUs {
    WallUs::new(micros)
}

/// The window `[from, to)` in microseconds after the epoch.
pub(crate) fn window(from: i64, to: i64) -> Window {
    Window::try_new(time(from), time(to)).unwrap()
}

/// The event id `id`, which a test gives positive.
pub(crate) fn event_id(id: i64) -> EventId {
    EventId::try_new(id).unwrap()
}

/// The trust of the platform's certificate store, as a client takes it.
pub(crate) fn trust() -> ClientConfig {
    Arc::unwrap_or_clone(iris_tls::client_config(None).unwrap())
}

/// A client of `origin` without a bearer, trusting the platform's
/// certificate store.
pub(crate) fn client(origin: ApiOrigin) -> Client {
    Client::new(origin, None, trust())
}

/// Serves `app` on a port of the loopback; gives its origin and the task
/// that serves it.
pub(crate) async fn serve(app: Router) -> (ApiOrigin, JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let origin = ApiOrigin::parse(&format!("http://{address}"), PlainHttp::LoopbackOnly);
    (origin.unwrap(), server)
}

/// What the tests of this binary have logged, as the viewer's log prints
/// it without its time.
static LOG: Mutex<Vec<u8>> = Mutex::new(Vec::new());

/// A writer into [`LOG`].
struct Captured;

impl io::Write for Captured {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        LOG.lock().expect("captured log").extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// The lines logged so far that hold `word` as a whole word, such as
/// `camera=cam1`. The first call installs the subscriber that captures the
/// log, for the whole test binary, so a test calls this before it logs.
/// Every test that reads the log names a word that no other test logs.
pub(crate) fn logged(word: &str) -> Vec<String> {
    static SUBSCRIBER: Once = Once::new();
    SUBSCRIBER.call_once(|| {
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(Level::DEBUG)
            .with_ansi(false)
            .without_time()
            .with_writer(|| Captured)
            .finish();
        tracing::subscriber::set_global_default(subscriber)
            .expect("the test binary installs no other subscriber");
    });
    let text =
        String::from_utf8(LOG.lock().expect("captured log").clone()).expect("the log is UTF-8");
    text.lines()
        .filter(|line| line.split(' ').any(|logged| logged == word))
        .map(str::to_owned)
        .collect()
}

/// An answer of `video/mp4` whose body is `body`.
pub(crate) fn mp4_response(body: Body) -> Response {
    Response::builder()
        .header(header::CONTENT_TYPE, "video/mp4")
        .body(body)
        .unwrap()
}

/// What a server of [`serve_raw`] does with a connection once it has written
/// its answer.
#[derive(Clone, Copy)]
pub(crate) enum Then {
    /// It closes its side of the connection, so the client finds the end of
    /// the stream after the answer's last byte.
    Close,
    /// It keeps the connection open and writes nothing more.
    Hold,
}

/// Serves the bytes `answer` to every request, on a port of the loopback;
/// gives its origin and the task that accepts its connections. No HTTP
/// library writes the answer, so the fixture alone decides its bytes and
/// their order. For each connection it reads a request to the end of its
/// head, writes `answer`, and does what `then` says.
pub(crate) async fn serve_raw(answer: Vec<u8>, then: Then) -> (ApiOrigin, JoinHandle<()>) {
    let answer: Arc<[u8]> = answer.into();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        while let Ok((connection, _)) = listener.accept().await {
            tokio::spawn(answer_raw(connection, Arc::clone(&answer), then));
        }
    });
    let origin = ApiOrigin::parse(&format!("http://{address}"), PlainHttp::LoopbackOnly);
    (origin.unwrap(), server)
}

/// One connection of [`serve_raw`]: reads a request's head, writes `answer`,
/// does what `then` says, and reads on until the client closes.
async fn answer_raw(mut connection: TcpStream, answer: Arc<[u8]>, then: Then) {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    let mut request = Vec::new();
    let mut buffer = [0_u8; 1024];
    while !request.windows(4).any(|end| end == b"\r\n\r\n") {
        match connection.read(&mut buffer).await {
            Ok(read) if read > 0 => request.extend_from_slice(&buffer[..read]),
            _ => return,
        }
    }
    if connection.write_all(&answer).await.is_err() {
        return;
    }
    if matches!(then, Then::Close) && connection.shutdown().await.is_err() {
        return;
    }
    // The server leaves the connection only after the client has: a socket
    // closed with bytes of a request unread resets the connection, and a
    // reset can discard the answer before the client reads it.
    while matches!(connection.read(&mut buffer).await, Ok(read) if read > 0) {}
}

/// Serves to every request an answer whose body is cut short. The answer is
/// the head of `status` and `content_type` and the bytes `first` of a
/// chunked body. The connection then ends, before the chunk that ends the
/// body. The failure meets a read of the body, never the wait for the head.
/// The server writes the head and `first`, in that order, before it closes,
/// and the connection delivers them in the order written.
pub(crate) async fn serve_cut_short(
    status: StatusCode,
    content_type: &str,
    first: &[u8],
) -> (ApiOrigin, JoinHandle<()>) {
    let head = format!(
        "HTTP/1.1 {status}\r\ncontent-type: {content_type}\r\ntransfer-encoding: chunked\r\n\r\n\
         {:x}\r\n",
        first.len()
    );
    let mut answer = head.into_bytes();
    answer.extend_from_slice(first);
    answer.extend_from_slice(b"\r\n");
    serve_raw(answer, Then::Close).await
}
