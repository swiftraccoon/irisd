//! The irisd HTTP API calls the Linux viewer makes: the camera list, the
//! segment and event lists, the VOD playlist, keyframes, retain marks, camera
//! provisioning, and the ONVIF lookups, with the export in [`export`].
//! [`iris_client::api`] builds each call's request from the API origin and
//! checks its answer; this module carries the request between the two. It
//! sends every request with the [`Bearer`] it was given, if any, in
//! `Authorization`, and follows no redirect. Once an answer's head is in, it
//! reads the part of the body that [`HttpRequest::body_to_read`] asks for,
//! before the call's check takes it.

use std::collections::TryReserveError;
use std::fmt;
use std::time::Duration;

use iris_client::api::{
    Answer, ApiError, BodyRead, Call, Camera, DiscoveredDevice, EventId, EventRecord, HttpRequest,
    NewCamera, ProfileStream, READ_IDLE, Retention, STREAM_LOOKUP_READ_IDLE, Segment,
    StreamUrlsRequest, VodPlaylist, Window, answer,
};
use iris_client::origin::ApiOrigin;
use iris_core::id::{CameraId, StreamKind};
use iris_core::time::WallUs;
use iris_tls::ClientConfig;
use reqwest::header::{self, HeaderValue};
use reqwest::redirect::Policy;
use reqwest::{Method, RequestBuilder, Response};
use thiserror::Error;
use url::Url;

use crate::export::{self, EXPORT_SPACE_CHECK_BYTES, SpaceProbe};

/// Maximum time spent establishing an API connection.
const HTTP_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// The root bearer token of the irisd API. A [`Client`] sends it with each
/// request, and a [`Feed`](crate::Feed) in each session's WebSocket
/// handshake. A token is one or more characters of visible ASCII (0x21 to
/// 0x7e), the characters the daemon's configuration admits in a token. Its
/// `Debug` redacts the token, and it has no `Display`, so no log line or
/// message that formats a `Bearer` shows the token. The trace of a feed's
/// handshake does show it: see
/// [`is_sensitive_log_target`](crate::is_sensitive_log_target).
///
/// # Example
///
/// ```
/// use iris_transport::Bearer;
///
/// let bearer = Bearer::try_new("t0ken")?;
/// assert_eq!(
///     format!("{bearer:?}"),
///     "Bearer(<redacted>)",
///     "the token is redacted"
/// );
/// // A space and a line break are outside visible ASCII, so this token is
/// // refused here, before any call sends it.
/// let refused = Bearer::try_new("t0ken\r\nHost: elsewhere").unwrap_err();
/// assert_eq!(
///     refused.to_string(),
///     "the bearer token is empty or holds a character outside visible ASCII",
///     "the text holds no part of the token"
/// );
/// # Ok::<(), iris_transport::BearerError>(())
/// ```
#[derive(Clone)]
pub struct Bearer {
    /// `Bearer`, a space, and the token, marked sensitive so that HTTP/2
    /// never indexes it.
    header: HeaderValue,
}

impl Bearer {
    /// Holds `token` as the value of an `Authorization` header: `Bearer`, a
    /// space, and the token. The example of [`Bearer`] makes one and refuses
    /// one.
    ///
    /// # Errors
    ///
    /// [`BearerError`] when `token` is empty or holds a character outside
    /// visible ASCII (0x21 to 0x7e): a space, a control character, or a
    /// character that is not ASCII. Visible ASCII is what the daemon's
    /// configuration admits in a token. The length of a token is not
    /// checked: the daemon's configuration sets the shortest it accepts.
    pub fn try_new(token: impl Into<String>) -> Result<Self, BearerError> {
        let token: String = token.into();
        if token.is_empty() || !token.bytes().all(|byte| byte.is_ascii_graphic()) {
            return Err(BearerError);
        }
        // A header value admits every character of visible ASCII, so
        // `Bearer`, a space, and the token always make one. The `map_err`
        // covers a refusal that cannot happen.
        let mut header =
            HeaderValue::try_from(format!("Bearer {token}")).map_err(|_| BearerError)?;
        header.set_sensitive(true);
        Ok(Self { header })
    }

    /// The value of the `Authorization` header that carries the token.
    pub(crate) fn header(&self) -> &HeaderValue {
        &self.header
    }
}

impl fmt::Debug for Bearer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Bearer(<redacted>)")
    }
}

/// Why [`Bearer::try_new`] refused a token: it is empty, or it holds a
/// character outside visible ASCII (0x21 to 0x7e). The text holds no part of
/// the token. The example of [`Bearer`] refuses one.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Error)]
#[error("the bearer token is empty or holds a character outside visible ASCII")]
pub struct BearerError;

/// Why a call of the [`Client`] failed. The text of each is the message the
/// Linux viewer shows. A refusal has the text of the [`ApiError`] that
/// `iris-client` gives. A failure of the transport names the call, with the
/// error the transport met as its source. The example of [`Client`] tells a
/// refusal from a failure of the transport.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum ClientError {
    /// `iris-client` refused the request before it was sent, or its answer
    /// once it arrived; the text and the source are those of the
    /// [`ApiError`].
    #[error(transparent)]
    Api(#[from] ApiError),
    /// The transport could not send the request or receive its answer's head:
    /// the connection failed, or the read deadline passed. The text names the
    /// call, such as "listing the cameras", and the source, a
    /// `reqwest::Error`, says why.
    #[error("{call}")]
    Transport {
        /// The call that did not travel, which the text names.
        call: Call,
        /// Why the request or its answer did not travel.
        source: reqwest::Error,
    },
    /// The transport could not read the body that holds the call's value;
    /// the message of a refused status is dropped instead, and the refusal
    /// stands without it. The text is "reading the answer to {call}", and the
    /// source says why.
    #[error("reading the answer to {call}")]
    Read {
        /// The call whose body did not arrive, which the text names.
        call: Call,
        /// Why the body did not arrive.
        source: reqwest::Error,
    },
    /// The transport could not reserve the memory to hold an answer's body.
    /// The text is "reserving the answer to {call}".
    #[error("reserving the answer to {call}")]
    Reserve {
        /// The call whose body could not be held, which the text names.
        call: Call,
        /// The allocator's refusal.
        source: TryReserveError,
    },
}

/// A client of the irisd API, which sends every request with the bearer it
/// was given, if any, in `Authorization`. Its `Debug` output redacts the
/// token. A clone shares its connection pools.
///
/// # Panics
///
/// Each call runs on a Tokio runtime with its I/O and time drivers enabled.
/// It panics when polled outside one, or on a runtime that lacks one of the
/// two drivers: the connection needs the I/O driver, and the read deadlines
/// need the timer. [`Client::new`] needs no runtime.
///
/// # Example
///
/// ```no_run
/// # use std::error::Error;
/// use std::env;
/// use std::sync::Arc;
///
/// use iris_client::origin::{ApiOrigin, PlainHttp};
/// use iris_transport::{Bearer, Client, ClientError};
///
/// # async fn refresh() -> Result<(), Box<dyn Error>> {
/// let api = "https://nvr.example:8443";
/// let origin = ApiOrigin::parse(api, PlainHttp::LoopbackOnly)?;
/// let tls = Arc::unwrap_or_clone(iris_tls::client_config(None)?);
/// // The daemon trims a presented token and treats an empty one as none.
/// let variable = env::var("IRISD_TOKEN").unwrap_or_default();
/// let token = Some(variable.trim()).filter(|token| !token.is_empty());
/// let bearer = token.map(Bearer::try_new).transpose()?;
/// let client = Client::new(origin.clone(), bearer, tls);
/// // The live feeds connect to the origin the client sends its requests to.
/// assert_eq!(client.origin(), &origin, "the client keeps its origin");
/// match client.cameras().await {
///     Ok(cameras) => println!("{} cameras", cameras.len()),
///     // The daemon refused the list, or its answer failed the list's check.
///     Err(ClientError::Api(refusal)) => eprintln!("refused: {refusal}"),
///     // The request or its answer did not travel, or its body could not be
///     // held.
///     Err(failure) => eprintln!("{failure}"),
/// }
/// # Ok(())
/// # }
/// ```
#[derive(Clone)]
pub struct Client {
    /// The API origin, whose path is `/`, so the absolute path each request
    /// builder sets on it replaces nothing.
    origin: ApiOrigin,
    token: Option<Bearer>,
    http: reqwest::Client,
    /// The client for requests that wait longer than [`READ_IDLE`]: the stream
    /// lookup, whose answer can take the daemon's whole lookup bound to start
    /// ([`STREAM_LOOKUP_READ_IDLE`]).
    lookup_http: reqwest::Client,
    /// Free-space probe for exports; tests inject a fixed answer.
    pub(super) space_probe: SpaceProbe,
    /// Bytes an export writes between free-space checks.
    pub(super) space_check_bytes: u64,
}

impl fmt::Debug for Client {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Client")
            .field("origin", &self.origin.url().as_str())
            .field("token", &self.token)
            .finish_non_exhaustive()
    }
}

/// Builds a reqwest client of the API that verifies servers with `tls`,
/// gives up connecting after `connect_timeout`, and waits `read_timeout` for
/// each read. It follows no redirect: the daemon never redirects, and
/// following one would let a response name the origin that playlist
/// references are checked against.
///
/// The `build` of reqwest refuses these options in one case alone: a
/// preconfigured TLS value of a type other than the `rustls::ClientConfig`
/// that reqwest takes. Whether `tls` is that type is fixed when the crate is
/// built, not when it runs. It is that type: the lockfile holds one
/// `rustls`, which `iris-tls` re-exports and reqwest takes. The test
/// `the_shared_trust_is_the_tls_configuration_reqwest_takes` fails a build
/// where it is not.
fn http_client(
    tls: ClientConfig,
    connect_timeout: Duration,
    read_timeout: Duration,
) -> reqwest::Client {
    reqwest::Client::builder()
        .connect_timeout(connect_timeout)
        .read_timeout(read_timeout)
        .redirect(Policy::none())
        .tls_backend_preconfigured(tls)
        .build()
        .expect("the TLS configuration of iris-tls is the rustls::ClientConfig of reqwest")
}

impl Client {
    /// Creates a client for `origin` that sends `token`, when there is one,
    /// with every request. An `https` connection verifies the server with
    /// `tls`, the trust the live feeds use too. The client keeps `tls`: a
    /// viewer that shares it with its feeds in an [`Arc`](std::sync::Arc)
    /// hands the client a clone of the configuration. It gives up connecting
    /// after 5 s, and waits [`READ_IDLE`] for an answer's head or its next
    /// body bytes, [`STREAM_LOOKUP_READ_IDLE`] for a stream lookup's. It
    /// needs no runtime. The example of [`Client`] makes one.
    ///
    /// # Panics
    ///
    /// Only in a build whose `iris-tls` and reqwest take two versions of
    /// `rustls`, which the crate's tests refuse. The builder of reqwest
    /// refuses these options only when the TLS configuration it is handed is
    /// not the `rustls::ClientConfig` of reqwest. The type of `tls`, which
    /// `iris-tls` re-exports, is that type while the lockfile holds one
    /// `rustls`. Nothing a caller passes or the system holds at run time
    /// changes it.
    #[must_use]
    pub fn new(origin: ApiOrigin, token: Option<Bearer>, tls: ClientConfig) -> Self {
        Self::new_with_timeouts(origin, token, tls, HTTP_CONNECT_TIMEOUT, READ_IDLE)
    }

    /// A client as [`Client::new`] makes one, giving up connecting after
    /// `connect_timeout` and waiting `read_timeout` in place of 5 s and
    /// [`READ_IDLE`]; the tests shorten both.
    fn new_with_timeouts(
        origin: ApiOrigin,
        token: Option<Bearer>,
        tls: ClientConfig,
        connect_timeout: Duration,
        read_timeout: Duration,
    ) -> Self {
        Self {
            origin,
            token,
            http: http_client(tls.clone(), connect_timeout, read_timeout),
            lookup_http: http_client(
                tls,
                connect_timeout,
                read_timeout.max(STREAM_LOOKUP_READ_IDLE),
            ),
            space_probe: export::filesystem_space,
            space_check_bytes: EXPORT_SPACE_CHECK_BYTES,
        }
    }

    /// The API origin every request goes to, which the live feeds share. The
    /// example of [`Client`] reads it.
    #[must_use]
    pub fn origin(&self) -> &ApiOrigin {
        &self.origin
    }

    /// `request` with the client's bearer, if it has one: every request the
    /// client sends is built through here.
    fn authorized(&self, request: RequestBuilder) -> RequestBuilder {
        match &self.token {
            Some(token) => request.header(header::AUTHORIZATION, token.header().clone()),
            None => request,
        }
    }

    /// Sends `request` with the client's bearer, if it has one, and its JSON
    /// body as `application/json`, through the client whose read deadline
    /// covers the request's. A failure to send names the call ("listing the
    /// cameras").
    async fn send(&self, request: &HttpRequest) -> Result<Response, ClientError> {
        let http = if request.read_idle() > READ_IDLE {
            &self.lookup_http
        } else {
            &self.http
        };
        let method: Method = request.method().into();
        let url = request.url().clone();
        let mut builder = self.authorized(http.request(method, url));
        if let Some(body) = request.body() {
            builder = builder
                .header(header::CONTENT_TYPE, "application/json")
                .body(body.to_vec());
        }
        builder
            .send()
            .await
            .map_err(|source| ClientError::Transport {
                call: request.call(),
                source,
            })
    }

    /// Sends `request` and reads the part of its answer's body that
    /// [`HttpRequest::body_to_read`] names from the head:
    ///
    /// - nothing;
    /// - the daemon's message up to its limit, dropped when it fails to
    ///   arrive;
    /// - the value up to the call's limit, which fails the call when it
    ///   fails to arrive.
    ///
    /// Every call takes this path. A body of which nothing is read stays in
    /// the response, which is how an export streams its file after its
    /// check.
    pub(super) async fn fetch(&self, request: &HttpRequest) -> Result<Received, ClientError> {
        let mut response = self.send(request).await?;
        let status = response.status().as_u16();
        let content_type = content_type(&response).map(str::to_owned);
        let final_url = response.url().clone();
        let body = match request.body_to_read(status, content_type.as_deref()) {
            BodyRead::Nothing => None,
            BodyRead::Message { max_bytes } => {
                read_body_limited(&mut response, bytes(max_bytes), request.call())
                    .await
                    .ok()
            }
            BodyRead::Value { max_bytes } => {
                match read_body_limited(&mut response, bytes(max_bytes), request.call()).await {
                    Ok(body) => Some(body),
                    Err(BodyError::Oversized) => None,
                    Err(BodyError::Failed(error)) => return Err(error),
                }
            }
        };
        Ok(Received {
            status,
            content_type,
            final_url,
            body,
            response,
        })
    }

    /// Lists the cameras the daemon records, through `GET /api/cameras`, each
    /// with its id, its name, whether the API added it, and whether it has
    /// audio. The example of [`Client`] lists the cameras.
    ///
    /// # Panics
    ///
    /// When polled outside a Tokio runtime with its I/O and time drivers
    /// enabled: see [`Client`](Client#panics).
    ///
    /// # Errors
    ///
    /// A [`ClientError`]: [`ClientError::Transport`] when the request or its
    /// answer's head did not travel, [`ClientError::Read`] or
    /// [`ClientError::Reserve`] when the answer's body could not be read or
    /// held, and [`ClientError::Api`] with the refusal of
    /// [`answer::cameras`].
    pub async fn cameras(&self) -> Result<Vec<Camera>, ClientError> {
        let received = self.fetch(&self.origin.cameras()).await?;
        answer::cameras(&self.origin, &received.answer()).map_err(ClientError::Api)
    }

    /// Adds a camera through `POST /api/cameras`; the daemon starts recording
    /// it and answers with the camera as the list shows it. The example of
    /// [`Client`] makes a call.
    ///
    /// # Panics
    ///
    /// When polled outside a Tokio runtime with its I/O and time drivers
    /// enabled: see [`Client`](Client#panics).
    ///
    /// # Errors
    ///
    /// A [`ClientError`]:
    ///
    /// - [`ClientError::Api`] when the request builder refuses the camera,
    ///   for a stream URL in which
    ///   [`rtsp_url_problem`](iris_client::api::rtsp_url_problem) finds a
    ///   problem;
    /// - [`ClientError::Api`] when [`answer::add_camera`] refuses the answer,
    ///   as it refuses the daemon's conflict;
    /// - [`ClientError::Transport`], [`ClientError::Read`], or
    ///   [`ClientError::Reserve`] as for [`Client::cameras`].
    pub async fn add_camera(&self, camera: &NewCamera) -> Result<Camera, ClientError> {
        let received = self.fetch(&self.origin.add_camera(camera)?).await?;
        answer::add_camera(&self.origin, &received.answer(), &camera.id).map_err(ClientError::Api)
    }

    /// Removes a camera the API added, through `DELETE /api/cameras/{camera}`.
    /// The example of [`Client`] makes a call.
    ///
    /// # Panics
    ///
    /// When polled outside a Tokio runtime with its I/O and time drivers
    /// enabled: see [`Client`](Client#panics).
    ///
    /// # Errors
    ///
    /// A [`ClientError`]: [`ClientError::Transport`] when the request or its
    /// answer's head did not travel, and [`ClientError::Api`] with the
    /// refusal of [`answer::remove_camera`], such as the daemon's conflict for
    /// a camera of the configuration file. The answer's body is read for the
    /// daemon's message alone, whose failure drops the message, so this call
    /// fails with neither [`ClientError::Read`] nor [`ClientError::Reserve`].
    pub async fn remove_camera(&self, camera: &CameraId) -> Result<(), ClientError> {
        let received = self.fetch(&self.origin.remove_camera(camera)).await?;
        answer::remove_camera(&self.origin, &received.answer()).map_err(ClientError::Api)
    }

    /// Lists the segments of one stream that hold recordings inside `window`.
    /// Open segments that have recorded nothing there, such as a stale file
    /// of an offline camera or a file before its first sync, are omitted. The
    /// example of [`Client`] makes a call.
    ///
    /// # Panics
    ///
    /// When polled outside a Tokio runtime with its I/O and time drivers
    /// enabled: see [`Client`](Client#panics).
    ///
    /// # Errors
    ///
    /// A [`ClientError`]: [`ClientError::Api`] with the refusal of
    /// [`answer::segments`], and [`ClientError::Transport`],
    /// [`ClientError::Read`], or [`ClientError::Reserve`] as for
    /// [`Client::cameras`].
    pub async fn segments(
        &self,
        camera: &CameraId,
        kind: StreamKind,
        window: Window,
    ) -> Result<Vec<Segment>, ClientError> {
        let received = self
            .fetch(&self.origin.segments(camera, kind, window))
            .await?;
        answer::segments(&self.origin, &received.answer(), window).map_err(ClientError::Api)
    }

    /// Lists one camera's events in `window`, oldest first as the API orders
    /// them, at most `limit` (1 to 10,000). The example of [`Client`] makes a
    /// call.
    ///
    /// # Panics
    ///
    /// When polled outside a Tokio runtime with its I/O and time drivers
    /// enabled: see [`Client`](Client#panics).
    ///
    /// # Errors
    ///
    /// A [`ClientError`]: [`ClientError::Api`] with a page limit the builder
    /// refuses or the refusal of [`answer::events`], and
    /// [`ClientError::Transport`], [`ClientError::Read`], or
    /// [`ClientError::Reserve`] as for [`Client::cameras`].
    pub async fn events(
        &self,
        camera: &CameraId,
        window: Window,
        limit: usize,
    ) -> Result<Vec<EventRecord>, ClientError> {
        let received = self
            .fetch(&self.origin.events(camera, window, limit)?)
            .await?;
        answer::events(&self.origin, &received.answer(), window, limit).map_err(ClientError::Api)
    }

    /// Keeps the footage around event `id` from retention until a time
    /// ([`Retention::Until`]), or clears the mark ([`Retention::Release`]);
    /// returns the event as the API lists it after the change. The example of
    /// [`Client`] makes a call.
    ///
    /// # Panics
    ///
    /// When polled outside a Tokio runtime with its I/O and time drivers
    /// enabled: see [`Client`](Client#panics).
    ///
    /// # Errors
    ///
    /// A [`ClientError`]: [`ClientError::Api`] with the refusal of
    /// [`answer::retain_event`], and [`ClientError::Transport`],
    /// [`ClientError::Read`], or [`ClientError::Reserve`] as for
    /// [`Client::cameras`].
    pub async fn retain_event(
        &self,
        id: EventId,
        retention: Retention,
    ) -> Result<EventRecord, ClientError> {
        let received = self.fetch(&self.origin.retain_event(id, retention)).await?;
        answer::retain_event(&self.origin, &received.answer(), id).map_err(ClientError::Api)
    }

    /// Fetches the keyframe resource at or before `t`: an init segment plus
    /// one single-sample fragment, or `None` when nothing is recorded there.
    /// The example of [`Client`] makes a call.
    ///
    /// # Panics
    ///
    /// When polled outside a Tokio runtime with its I/O and time drivers
    /// enabled: see [`Client`](Client#panics).
    ///
    /// # Errors
    ///
    /// A [`ClientError`]: [`ClientError::Api`] with the refusal of
    /// [`answer::keyframe`], such as another media type, and
    /// [`ClientError::Transport`], [`ClientError::Read`], or
    /// [`ClientError::Reserve`] as for [`Client::cameras`].
    pub async fn keyframe(
        &self,
        camera: &CameraId,
        kind: StreamKind,
        t: WallUs,
    ) -> Result<Option<Vec<u8>>, ClientError> {
        let received = self.fetch(&self.origin.keyframe(camera, kind, t)).await?;
        let recorded = answer::keyframe(&self.origin, &received.answer())?.is_some();
        // A keyframe the check passes is the whole body it was handed
        // (`answer::keyframe` documents it and its tests pin it), so the
        // client keeps the body it holds rather than copy it.
        Ok(received.body.filter(|_| recorded))
    }

    /// Fetches the VOD playlist of `window`, which a view uses for both timing
    /// and playback: it includes the boundary GOPs and omits unavailable
    /// files. Init and media URIs resolve to absolute URLs on the API origin
    /// and keep their authentication queries, so they still reach the daemon
    /// when the playlist is served locally. A reference to another origin or
    /// scheme, userinfo, or a tag the daemon does not emit rejects the
    /// playlist. The example of [`Client`] makes a call.
    ///
    /// # Panics
    ///
    /// When polled outside a Tokio runtime with its I/O and time drivers
    /// enabled: see [`Client`](Client#panics).
    ///
    /// # Errors
    ///
    /// A [`ClientError`]: [`ClientError::Api`] with the refusal of
    /// [`answer::vod_playlist`], and [`ClientError::Transport`],
    /// [`ClientError::Read`], or [`ClientError::Reserve`] as for
    /// [`Client::cameras`].
    pub async fn vod_playlist(
        &self,
        camera: &CameraId,
        kind: StreamKind,
        window: Window,
    ) -> Result<VodPlaylist, ClientError> {
        let received = self
            .fetch(&self.origin.vod_playlist(camera, kind, window))
            .await?;
        answer::vod_playlist(&self.origin, &received.answer(), window).map_err(ClientError::Api)
    }

    /// Lists the cameras a WS-Discovery probe finds within `wait`, through
    /// `GET /api/onvif/devices`. The example of [`Client`] makes a call.
    ///
    /// # Panics
    ///
    /// When polled outside a Tokio runtime with its I/O and time drivers
    /// enabled: see [`Client`](Client#panics).
    ///
    /// # Errors
    ///
    /// A [`ClientError`]: [`ClientError::Api`] with a wait the builder
    /// refuses, outside [`DISCOVERY_WAIT_MS`](iris_client::api::DISCOVERY_WAIT_MS),
    /// or the refusal of [`answer::discover`], and
    /// [`ClientError::Transport`], [`ClientError::Read`], or
    /// [`ClientError::Reserve`] as for [`Client::cameras`].
    pub async fn discover(&self, wait: Duration) -> Result<Vec<DiscoveredDevice>, ClientError> {
        let received = self.fetch(&self.origin.discover(wait)?).await?;
        answer::discover(&self.origin, &received.answer()).map_err(ClientError::Api)
    }

    /// Resolves the RTSP URLs of one camera's ONVIF media profiles, through
    /// `POST /api/onvif/streams`; the answer may take up to the daemon's lookup
    /// bound, so this request alone waits [`STREAM_LOOKUP_READ_IDLE`]. The
    /// example of [`Client`] makes a call.
    ///
    /// # Panics
    ///
    /// When polled outside a Tokio runtime with its I/O and time drivers
    /// enabled: see [`Client`](Client#panics).
    ///
    /// # Errors
    ///
    /// A [`ClientError`]:
    ///
    /// - [`ClientError::Api`] with the refusal of [`answer::stream_urls`],
    ///   such as the daemon's report that the camera could not be queried;
    /// - [`ClientError::Transport`], [`ClientError::Read`], or
    ///   [`ClientError::Reserve`] as for [`Client::cameras`].
    pub async fn stream_urls(
        &self,
        request: &StreamUrlsRequest,
    ) -> Result<Vec<ProfileStream>, ClientError> {
        let received = self.fetch(&self.origin.stream_urls(request)).await?;
        answer::stream_urls(&self.origin, &received.answer()).map_err(ClientError::Api)
    }
}

/// An answer as the client received it, for the check of its call: the head,
/// the part of the body that [`HttpRequest::body_to_read`] asked for, and the
/// response.
pub(super) struct Received {
    status: u16,
    content_type: Option<String>,
    final_url: Url,
    /// `None` when `body_to_read` asked for none of the body, when the body is
    /// longer than the limit it gave, and when a refusal's message could not
    /// be read.
    body: Option<Vec<u8>>,
    /// The response, whose body is unread when `body_to_read` asked for none
    /// of it: an export streams it to its file once the check passes.
    pub(super) response: Response,
}

impl Received {
    /// The answer the checks of [`iris_client::api::answer`] take.
    pub(super) fn answer(&self) -> Answer<'_> {
        Answer {
            status: self.status,
            content_type: self.content_type.as_deref(),
            final_url: &self.final_url,
            body: self.body.as_deref(),
        }
    }
}

/// `max_bytes`, a limit that [`HttpRequest::body_to_read`] gives, as a
/// length in memory. A limit above `usize::MAX` becomes `usize::MAX`, since
/// memory holds no body longer than that.
fn bytes(max_bytes: u64) -> usize {
    usize::try_from(max_bytes).unwrap_or(usize::MAX)
}

/// Why [`read_body_limited`] holds no body.
enum BodyError {
    /// The body is longer than the limit it was read under.
    Oversized,
    /// Reserving room for the body or reading it failed:
    /// [`ClientError::Reserve`] or [`ClientError::Read`].
    Failed(ClientError),
}

/// The body of `response`, refused beyond `maximum` bytes before it is held:
/// a `Content-Length` above `maximum`, or a chunk past it, stops the read.
/// `call` names the request in errors. It borrows the response, so that the
/// client reads a body and keeps the response it came in.
async fn read_body_limited(
    response: &mut Response,
    maximum: usize,
    call: Call,
) -> Result<Vec<u8>, BodyError> {
    let reserving = |source| BodyError::Failed(ClientError::Reserve { call, source });
    if response
        .content_length()
        .is_some_and(|length| length > maximum as u64)
    {
        return Err(BodyError::Oversized);
    }
    let capacity = response
        .content_length()
        .and_then(|length| usize::try_from(length).ok())
        .unwrap_or(0);
    let mut body = Vec::new();
    body.try_reserve_exact(capacity).map_err(reserving)?;
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|source| BodyError::Failed(ClientError::Read { call, source }))?
    {
        if chunk.len() > maximum.saturating_sub(body.len()) {
            return Err(BodyError::Oversized);
        }
        body.try_reserve(chunk.len()).map_err(reserving)?;
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

/// The value of the `Content-Type` header of `response`, when it has one
/// that is text.
fn content_type(response: &Response) -> Option<&str> {
    response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
}

#[cfg(test)]
mod tests {
    use std::error::Error as _;
    use std::io;
    use std::net::{SocketAddr, TcpStream};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    use axum::Router;
    use axum::body::{Body, Bytes};
    use axum::extract::{Path, RawQuery};
    use axum::http::{HeaderMap, HeaderName, Method, StatusCode, Uri, header};
    use axum::response::Response;
    use axum::routing::{self, MethodRouter};
    use futures::stream;
    use iris_client::api::{EventKind, MAX_CAMERA_BODY_BYTES, MAX_ERROR_BODY_BYTES};
    use iris_client::origin::PlainHttp;
    use tokio::net::{TcpListener, TcpSocket};
    use tokio::task::JoinHandle;
    use tokio::time::Instant;

    use super::*;
    use crate::fixture::{self, Then};

    /// One request the provisioning fixture saw.
    #[derive(Clone)]
    struct SeenRequest {
        method: String,
        path: String,
        token: Option<String>,
        body: String,
    }

    type Seen = Arc<Mutex<Vec<SeenRequest>>>;

    fn record(seen: &Seen, method: &str, path: String, headers: &HeaderMap, body: String) {
        let token = headers
            .get(header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        seen.lock().unwrap().push(SeenRequest {
            method: method.to_owned(),
            path,
            token,
            body,
        });
    }

    /// The header of a JSON answer.
    const JSON: [(HeaderName, &str); 1] = [(header::CONTENT_TYPE, "application/json")];

    /// A daemon stand-in for the provisioning routes: the id `taken` is in
    /// use, the camera `static` comes from the configuration file, and a
    /// lookup of the host 192.0.2.99 fails.
    fn provisioning_fixture(seen: &Seen) -> Router {
        Router::new()
            .route(
                "/api/cameras",
                routing::post({
                    let seen = Arc::clone(seen);
                    move |headers: HeaderMap, body: String| {
                        let seen = Arc::clone(&seen);
                        async move { add_route(&seen, &headers, &body) }
                    }
                }),
            )
            .route(
                "/api/cameras/{camera}",
                routing::delete({
                    let seen = Arc::clone(seen);
                    move |Path(camera): Path<String>, headers: HeaderMap| {
                        let seen = Arc::clone(&seen);
                        async move { remove_route(&seen, &camera, &headers) }
                    }
                }),
            )
            .route(
                "/api/onvif/devices",
                routing::get({
                    let seen = Arc::clone(seen);
                    move |headers: HeaderMap, RawQuery(query): RawQuery| {
                        let seen = Arc::clone(&seen);
                        async move {
                            let path = format!("/api/onvif/devices?{}", query.unwrap_or_default());
                            record(&seen, "GET", path, &headers, String::new());
                            let devices = concat!(
                                r#"[{"endpoint":"urn:uuid:1","host":"192.0.2.20","#,
                                r#""port":80,"https":false,"name":"Garage","#,
                                r#""hardware":"IPC","location":null}]"#
                            );
                            (StatusCode::OK, JSON, devices.to_owned())
                        }
                    }
                }),
            )
            .route(
                "/api/onvif/streams",
                routing::post({
                    let seen = Arc::clone(seen);
                    move |headers: HeaderMap, body: String| {
                        let seen = Arc::clone(&seen);
                        async move { streams_route(&seen, &headers, &body) }
                    }
                }),
            )
    }

    fn add_route(
        seen: &Seen,
        headers: &HeaderMap,
        body: &str,
    ) -> (StatusCode, [(HeaderName, &'static str); 1], String) {
        record(
            seen,
            "POST",
            "/api/cameras".into(),
            headers,
            body.to_owned(),
        );
        let json: serde_json::Value = serde_json::from_str(body).unwrap();
        if json["id"] == "taken" {
            let error = r#"{"error":"conflict: camera taken already exists"}"#;
            return (StatusCode::CONFLICT, JSON, error.to_owned());
        }
        let camera = format!(
            r#"{{"id":"{}","name":"{}","vendor":null,"has_snapshot":true,"provisioned":true}}"#,
            json["id"].as_str().unwrap(),
            json["name"].as_str().unwrap()
        );
        (StatusCode::CREATED, JSON, camera)
    }

    fn remove_route(
        seen: &Seen,
        camera: &str,
        headers: &HeaderMap,
    ) -> (StatusCode, [(HeaderName, &'static str); 1], String) {
        record(
            seen,
            "DELETE",
            format!("/api/cameras/{camera}"),
            headers,
            String::new(),
        );
        if camera == "static" {
            let error = r#"{"error":"conflict: camera static is in the configuration file"}"#;
            (StatusCode::CONFLICT, JSON, error.to_owned())
        } else {
            (StatusCode::NO_CONTENT, JSON, String::new())
        }
    }

    fn streams_route(
        seen: &Seen,
        headers: &HeaderMap,
        body: &str,
    ) -> (StatusCode, [(HeaderName, &'static str); 1], String) {
        record(
            seen,
            "POST",
            "/api/onvif/streams".into(),
            headers,
            body.to_owned(),
        );
        let json: serde_json::Value = serde_json::from_str(body).unwrap();
        if json["host"] == "192.0.2.99" {
            let error =
                r#"{"error":"upstream: the camera could not be queried: ONVIF call failed"}"#;
            return (StatusCode::BAD_GATEWAY, JSON, error.to_owned());
        }
        let streams = concat!(
            r#"[{"token":"profile-1","name":"Main","url":"rtsp://192.0.2.20:554/live/1"},"#,
            r#"{"token":"profile-2","name":"Sub","url":"rtsp://192.0.2.20:554/live/2"}]"#
        );
        (StatusCode::OK, JSON, streams.to_owned())
    }

    fn lookup(host: &str) -> StreamUrlsRequest {
        StreamUrlsRequest {
            host: host.into(),
            http_port: 80,
            https: false,
            tls_verify: true,
            username: "viewer".into(),
            password: "secret".into(),
        }
    }

    fn new_camera(id: &str) -> NewCamera {
        NewCamera {
            id: fixture::camera(id),
            name: "Garage".into(),
            rtsp_url: "rtsp://192.0.2.10/x".into(),
            sub_url: None,
            username: "viewer".into(),
            password: "secret".into(),
            record: vec![StreamKind::Main],
            events: Vec::new(),
        }
    }

    async fn provisioning_client() -> (Client, Seen, JoinHandle<()>) {
        let seen: Seen = Arc::new(Mutex::new(Vec::new()));
        let (base, server) = fixture::serve(provisioning_fixture(&seen)).await;
        let client = Client::new(base, Some(fixture::bearer("t0ken")), fixture::trust());
        (client, seen, server)
    }

    /// A discovery asks for the devices that answer within the wait it was
    /// given. The test [`every_call_sends_the_bearer`] checks the bearer it
    /// sends.
    #[tokio::test]
    async fn discovery_asks_for_the_devices_within_the_wait() {
        let (client, seen, server) = provisioning_client().await;
        client.discover(Duration::from_secs(3)).await.unwrap();
        let asked: Vec<(String, String)> = requests(&seen)
            .into_iter()
            .map(|(method, path, _)| (method, path))
            .collect();
        assert_eq!(
            asked,
            [(
                "GET".to_owned(),
                "/api/onvif/devices?wait_ms=3000".to_owned()
            )]
        );
        server.abort();
    }

    #[tokio::test]
    async fn discovery_decodes_the_devices() {
        let (client, _seen, server) = provisioning_client().await;
        let devices = client.discover(Duration::from_secs(3)).await.unwrap();
        assert_eq!(
            devices
                .iter()
                .map(|device| (device.host.as_str(), device.port, device.name.as_deref()))
                .collect::<Vec<_>>(),
            [("192.0.2.20", 80, Some("Garage"))]
        );
        server.abort();
    }

    #[tokio::test]
    async fn a_stream_lookup_decodes_the_profile_urls() {
        let (client, _seen, server) = provisioning_client().await;
        let streams = client.stream_urls(&lookup("192.0.2.20")).await.unwrap();
        assert_eq!(
            streams
                .iter()
                .map(|stream| stream.url.as_str())
                .collect::<Vec<_>>(),
            [
                "rtsp://192.0.2.20:554/live/1",
                "rtsp://192.0.2.20:554/live/2"
            ]
        );
        server.abort();
    }

    /// A stream lookup posts the camera's host, ports, and credentials. The
    /// test [`every_call_sends_the_bearer`] checks the bearer it sends.
    #[tokio::test]
    async fn a_stream_lookup_posts_the_request() {
        let (client, seen, server) = provisioning_client().await;
        client.stream_urls(&lookup("192.0.2.20")).await.unwrap();
        let seen = seen.lock().unwrap().clone();
        let posted: serde_json::Value = serde_json::from_str(&seen[0].body).unwrap();
        assert_eq!(
            posted,
            serde_json::json!({
                "host": "192.0.2.20",
                "http_port": 80,
                "https": false,
                "tls_verify": true,
                "username": "viewer",
                "password": "secret",
            })
        );
        server.abort();
    }

    #[tokio::test]
    async fn a_failed_stream_lookup_reports_the_daemon_message() {
        let (client, _seen, server) = provisioning_client().await;
        let error = client
            .stream_urls(&lookup("192.0.2.99"))
            .await
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("502") && error.contains("could not be queried"),
            "{error}"
        );
        server.abort();
    }

    #[test]
    fn the_client_redacts_its_token() {
        let client = Client::new(
            ApiOrigin::parse("http://192.0.2.10:8554", PlainHttp::Anywhere).unwrap(),
            Some(fixture::bearer("t0ken")),
            fixture::trust(),
        );
        let shown = format!("{client:?}");
        assert!(
            shown.contains("<redacted>") && !shown.contains("t0ken"),
            "{shown}"
        );
    }

    /// The text with which `Bearer::try_new` refuses `token`.
    fn refusal(token: &str) -> String {
        Bearer::try_new(token)
            .expect_err("the token is refused")
            .to_string()
    }

    /// The text of every refused token, which holds no part of the token.
    const REFUSED: &str = "the bearer token is empty or holds a character outside visible ASCII";

    #[test]
    fn an_empty_token_is_refused() {
        assert_eq!(refusal(""), REFUSED);
    }

    #[test]
    fn a_token_with_a_space_inside_is_refused() {
        assert_eq!(refusal("t0 ken"), REFUSED);
    }

    /// A tab is the one character below 0x20 that a header value takes, so
    /// the check of visible ASCII alone refuses it.
    #[test]
    fn a_token_with_a_tab_inside_is_refused() {
        assert_eq!(refusal("t0\tken"), REFUSED);
    }

    #[test]
    fn a_token_with_a_trailing_line_feed_is_refused() {
        assert_eq!(refusal("t0ken\n"), REFUSED);
    }

    #[test]
    fn a_token_with_a_character_outside_ascii_is_refused() {
        assert_eq!(refusal("t0k\u{e9}n"), REFUSED);
    }

    /// Every character of visible ASCII, from `!` to `~`, in one token.
    #[test]
    fn a_token_of_visible_ascii_is_accepted() {
        let token: String = ('!'..='~').collect();
        assert!(Bearer::try_new(token).is_ok(), "visible ASCII is a token");
    }

    /// The header value is marked sensitive, so HTTP/2 never indexes it and
    /// its `Debug` output hides it.
    #[test]
    fn a_bearer_marks_its_header_value_sensitive() {
        assert!(
            fixture::bearer("t0ken").header().is_sensitive(),
            "the header value is sensitive"
        );
    }

    #[test]
    fn a_bearer_redacts_its_token() {
        assert_eq!(
            format!("{:?}", fixture::bearer("t0ken")),
            "Bearer(<redacted>)"
        );
    }

    /// The `Authorization` header a client sends is `Bearer`, a space, and
    /// the token it was given, byte for byte.
    #[tokio::test]
    async fn a_client_sends_the_bearer_it_was_given() {
        let seen: Seen = Arc::new(Mutex::new(Vec::new()));
        let route = {
            let seen = Arc::clone(&seen);
            routing::get(move |headers: HeaderMap| {
                let seen = Arc::clone(&seen);
                async move {
                    record(&seen, "GET", "/api/cameras".into(), &headers, String::new());
                    (StatusCode::OK, JSON, "[]")
                }
            })
        };
        let (base, server) = fixture::serve(Router::new().route("/api/cameras", route)).await;
        let token = Some(fixture::bearer("Mixed-Case+/=~!"));
        Client::new(base, token, fixture::trust())
            .cameras()
            .await
            .unwrap();
        let sent: Vec<Option<String>> = requests(&seen)
            .into_iter()
            .map(|(_, _, token)| token)
            .collect();
        assert_eq!(sent, [Some("Bearer Mixed-Case+/=~!".to_owned())]);
        server.abort();
    }

    /// The one case in which `Client::new` panics does not arise: reqwest
    /// builds a client with the options of `http_client` from the trust of
    /// `iris-tls`, which is the `rustls::ClientConfig` that reqwest takes.
    #[test]
    fn the_shared_trust_is_the_tls_configuration_reqwest_takes() {
        let tls = iris_tls::client_config(None).unwrap();
        // `http_client` panics where reqwest refuses its trust.
        let _built = http_client(ClientConfig::clone(&tls), HTTP_CONNECT_TIMEOUT, READ_IDLE);
    }

    /// The reqwest crate refuses a TLS value of another type than its
    /// `rustls::ClientConfig`, here the `Arc` that holds one: the case in
    /// which `Client::new` would panic.
    #[test]
    fn a_tls_value_of_another_type_is_refused() {
        let other = reqwest::Client::builder()
            .redirect(Policy::none())
            .tls_backend_preconfigured(iris_tls::client_config(None).unwrap())
            .build();
        assert!(other.is_err(), "a TLS value of another type is refused");
    }

    /// A client whose ordinary read deadline is 50 ms, of a server that
    /// answers the camera list and the stream lookup after 200 ms. Each
    /// answer is one its call accepts, so the read deadline alone can fail a
    /// call.
    async fn late_answers_client() -> (Client, JoinHandle<()>) {
        let cameras = || async {
            tokio::time::sleep(Duration::from_millis(200)).await;
            (
                StatusCode::OK,
                JSON,
                r#"[{"id":"cam1","name":"Front","provisioned":false}]"#.to_owned(),
            )
        };
        let streams = || async {
            tokio::time::sleep(Duration::from_millis(200)).await;
            (
                StatusCode::OK,
                JSON,
                r#"[{"token":"p","name":"Main","url":"rtsp://192.0.2.20/1"}]"#.to_owned(),
            )
        };
        let (base, server) = fixture::serve(
            Router::new()
                .route("/api/onvif/streams", routing::post(streams))
                .route("/api/cameras", routing::get(cameras)),
        )
        .await;
        let client = Client::new_with_timeouts(
            base,
            None,
            fixture::trust(),
            Duration::from_secs(1),
            Duration::from_millis(50),
        );
        (client, server)
    }

    #[tokio::test]
    async fn an_ordinary_call_ends_at_the_read_idle_deadline() {
        let (client, server) = late_answers_client().await;
        match client.cameras().await {
            Err(ClientError::Transport {
                call: Call::Cameras,
                source,
            }) => assert!(
                source.is_timeout(),
                "an ordinary request ends at the read idle deadline: {source:?}"
            ),
            other => panic!("an ordinary request ends at the read idle deadline: {other:?}"),
        }
        server.abort();
    }

    #[tokio::test]
    async fn a_stream_lookup_outlasts_the_read_idle_deadline() {
        let (client, server) = late_answers_client().await;
        let streams = client.stream_urls(&lookup("192.0.2.20")).await.unwrap();
        assert_eq!(streams.len(), 1);
        server.abort();
    }

    /// A listener on the loopback whose queue of connections is full. The
    /// system drops the first packet of each further connection, so a
    /// connect to the listener waits. Gives the listener's origin, the
    /// listener, and the connections that fill its queue.
    fn full_listener() -> (ApiOrigin, TcpListener, Vec<TcpStream>) {
        let socket = TcpSocket::new_v4().unwrap();
        socket.bind(SocketAddr::from(([127, 0, 0, 1], 0))).unwrap();
        let listener = socket.listen(1).unwrap();
        let address = listener.local_addr().unwrap();
        let mut queued = Vec::new();
        // Each connect the queue takes is held; the first it drops times out.
        while let Ok(connection) = TcpStream::connect_timeout(&address, Duration::from_millis(100))
        {
            queued.push(connection);
            assert!(queued.len() < 64, "the queue of the listener fills");
        }
        let origin = ApiOrigin::parse(&format!("http://{address}"), PlainHttp::LoopbackOnly);
        (origin.unwrap(), listener, queued)
    }

    /// A client that `Client::new` makes gives up connecting after 5 s. The
    /// listener never completes the connection. The clock is paused, so the
    /// runtime moves it to the connect deadline, the one timer left.
    #[tokio::test]
    async fn a_client_gives_up_connecting_after_five_seconds() {
        let (origin, _listener, _queued) = full_listener();
        let client = fixture::client(origin);
        tokio::time::pause();
        let start = Instant::now();
        // A client without a deadline would wait for ever: the bound ends
        // the test instead.
        let error = tokio::time::timeout(Duration::from_secs(60), client.cameras())
            .await
            .expect("the client gives up before the bound of the test")
            .expect_err("the connection never completes");
        let waited = start.elapsed();
        let timed_out = matches!(
            &error,
            ClientError::Transport { source, .. } if source.is_connect() && source.is_timeout()
        );
        let five_seconds = Duration::from_secs(5);
        assert!(
            timed_out && (five_seconds..five_seconds * 2).contains(&waited),
            "the client gave up connecting after {waited:?}: {error:?}"
        );
    }

    /// An answer of `[]` to the camera list, with its length.
    const EMPTY_LIST: &[u8] =
        b"HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: 2\r\n\r\n[]";

    /// A client that `Client::new` makes waits [`READ_IDLE`] for the head
    /// of an answer. The server answers the first request of a connection
    /// and never the next, and the first call leaves the connection in the
    /// pool. The clock is then paused, so the runtime moves it to the read
    /// deadline, the one timer left.
    #[tokio::test]
    async fn a_client_waits_read_idle_for_the_head_of_an_answer() {
        let (origin, server) = fixture::serve_raw(EMPTY_LIST.to_vec(), Then::Hold).await;
        let client = fixture::client(origin);
        client
            .cameras()
            .await
            .expect("the first request is answered");
        // The connection goes back to the pool after its answer is read.
        tokio::time::sleep(Duration::from_millis(100)).await;
        tokio::time::pause();
        let start = Instant::now();
        // A client without a deadline would wait for ever: the bound ends
        // the test instead.
        let error = tokio::time::timeout(READ_IDLE * 4, client.cameras())
            .await
            .expect("the client gives up before the bound of the test")
            .expect_err("the next request is not answered");
        let waited = start.elapsed();
        let timed_out = matches!(
            &error,
            ClientError::Transport { source, .. } if !source.is_connect() && source.is_timeout()
        );
        assert!(
            timed_out && (READ_IDLE..READ_IDLE * 2).contains(&waited),
            "the client waited {waited:?} for the head: {error:?}"
        );
        server.abort();
    }

    /// An added camera is posted with the fields the request builder
    /// encodes, and none it leaves out. The test
    /// [`every_call_sends_the_bearer`] checks the bearer it sends.
    #[tokio::test]
    async fn add_camera_posts_the_camera() {
        let (client, seen, server) = provisioning_client().await;
        client.add_camera(&new_camera("cam8")).await.unwrap();
        let seen = seen.lock().unwrap().clone();
        let posted: serde_json::Value = serde_json::from_str(&seen[0].body).unwrap();
        assert_eq!(
            (&posted["id"], &posted["password"], &posted["record"]),
            (
                &serde_json::json!("cam8"),
                &serde_json::json!("secret"),
                &serde_json::json!(["main"])
            )
        );
        assert!(posted.get("sub_url").is_none() && posted.get("events").is_none());
        server.abort();
    }

    #[tokio::test]
    async fn add_camera_answers_the_added_camera() {
        let (client, _seen, server) = provisioning_client().await;
        let added = client.add_camera(&new_camera("cam8")).await.unwrap();
        assert_eq!(
            (added.id.as_str(), added.name.as_str(), added.provisioned),
            ("cam8", "Garage", true)
        );
        server.abort();
    }

    #[tokio::test]
    async fn add_camera_reports_the_daemon_conflict() {
        let (client, _seen, server) = provisioning_client().await;
        let error = client
            .add_camera(&new_camera("taken"))
            .await
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("409") && error.contains("already exists"),
            "{error}"
        );
        server.abort();
    }

    /// A removal deletes the camera's route. The test
    /// [`every_call_sends_the_bearer`] checks the bearer it sends.
    #[tokio::test]
    async fn remove_camera_deletes_the_camera() {
        let (client, seen, server) = provisioning_client().await;
        client
            .remove_camera(&fixture::camera("cam8"))
            .await
            .unwrap();
        let asked: Vec<(String, String)> = requests(&seen)
            .into_iter()
            .map(|(method, path, _)| (method, path))
            .collect();
        assert_eq!(
            asked,
            [("DELETE".to_owned(), "/api/cameras/cam8".to_owned())]
        );
        server.abort();
    }

    #[tokio::test]
    async fn remove_camera_reports_the_daemon_conflict() {
        let (client, _seen, server) = provisioning_client().await;
        let error = client
            .remove_camera(&fixture::camera("static"))
            .await
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("409") && error.contains("configuration file"),
            "{error}"
        );
        server.abort();
    }

    /// A client of `origin` with the bearer `t0ken`.
    fn bearing_client(origin: ApiOrigin) -> Client {
        Client::new(origin, Some(fixture::bearer("t0ken")), fixture::trust())
    }

    /// The method, the path, and the bearer of each request `seen` holds.
    fn requests(seen: &Seen) -> Vec<(String, String, Option<String>)> {
        seen.lock()
            .unwrap()
            .iter()
            .map(|request| {
                (
                    request.method.clone(),
                    request.path.clone(),
                    request.token.clone(),
                )
            })
            .collect()
    }

    /// The call answers with the cameras the daemon lists, once their check
    /// has passed them. The test
    /// [`every_call_asks_for_the_route_of_its_request`] checks the route it
    /// asks for.
    #[tokio::test]
    async fn the_camera_list_is_the_cameras_the_daemon_lists() {
        let cameras = || async {
            let cameras = concat!(
                r#"[{"id":"cam1","name":"Front","provisioned":false},"#,
                r#"{"id":"cam2","name":"Back","provisioned":true}]"#
            );
            (StatusCode::OK, JSON, cameras)
        };
        let (base, server) =
            fixture::serve(Router::new().route("/api/cameras", routing::get(cameras))).await;
        let cameras = fixture::client(base).cameras().await.unwrap();
        assert_eq!(
            cameras
                .iter()
                .map(|camera| (camera.id.as_str(), camera.name.as_str(), camera.provisioned))
                .collect::<Vec<_>>(),
            [("cam1", "Front", false), ("cam2", "Back", true)]
        );
        server.abort();
    }

    /// A server whose retain route records each request in `seen` and
    /// answers with event 7, marked until the time the request names.
    async fn retain_server(seen: &Seen) -> (ApiOrigin, JoinHandle<()>) {
        let route = {
            let seen = Arc::clone(seen);
            routing::post(move |uri: Uri, headers: HeaderMap, body: String| {
                let seen = Arc::clone(&seen);
                async move {
                    let posted: serde_json::Value = serde_json::from_str(&body).unwrap();
                    let until = posted["until"].as_str().unwrap_or("null").to_owned();
                    record(&seen, "POST", uri.path().to_owned(), &headers, body);
                    let event = format!(
                        r#"{{"id":7,"kind":"motion","nvr_time":15,"retain_until_us":{until}}}"#
                    );
                    (StatusCode::OK, JSON, event)
                }
            })
        };
        fixture::serve(Router::new().route("/api/events/{id}/retain", route)).await
    }

    /// The body each request `seen` holds, as JSON.
    fn posted_bodies(seen: &Seen) -> Vec<serde_json::Value> {
        seen.lock()
            .unwrap()
            .iter()
            .map(|request| serde_json::from_str(&request.body).unwrap())
            .collect()
    }

    /// A retain mark posts the time the footage is kept until. The test
    /// [`a_retain_mark_is_posted_to_the_retain_route_of_its_event`] checks
    /// the route it is posted to.
    #[tokio::test]
    async fn a_retain_mark_posts_the_time_it_keeps_the_footage_until() {
        let seen: Seen = Arc::new(Mutex::new(Vec::new()));
        let (base, server) = retain_server(&seen).await;
        fixture::client(base)
            .retain_event(fixture::event_id(7), Retention::Until(fixture::time(2_000)))
            .await
            .unwrap();
        assert_eq!(
            posted_bodies(&seen),
            [serde_json::json!({ "until": "2000" })]
        );
        server.abort();
    }

    #[tokio::test]
    async fn a_released_mark_posts_no_time() {
        let seen: Seen = Arc::new(Mutex::new(Vec::new()));
        let (base, server) = retain_server(&seen).await;
        fixture::client(base)
            .retain_event(fixture::event_id(7), Retention::Release)
            .await
            .unwrap();
        assert_eq!(posted_bodies(&seen), [serde_json::json!({ "until": null })]);
        server.abort();
    }

    #[tokio::test]
    async fn a_retain_mark_answers_the_event_as_the_daemon_lists_it() {
        let seen: Seen = Arc::new(Mutex::new(Vec::new()));
        let (base, server) = retain_server(&seen).await;
        let kept = fixture::client(base)
            .retain_event(fixture::event_id(7), Retention::Until(fixture::time(2_000)))
            .await
            .unwrap();
        assert_eq!(
            kept,
            EventRecord {
                id: fixture::event_id(7),
                kind: EventKind::Named("motion".to_owned()),
                nvr_time: fixture::time(15),
                retain_until: Some(fixture::time(2_000)),
            }
        );
        server.abort();
    }

    /// A released mark answers with the event as the daemon lists it after
    /// the change, without a time it is kept until.
    #[tokio::test]
    async fn a_released_mark_answers_the_event_without_a_mark() {
        let seen: Seen = Arc::new(Mutex::new(Vec::new()));
        let (base, server) = retain_server(&seen).await;
        let cleared = fixture::client(base)
            .retain_event(fixture::event_id(7), Retention::Release)
            .await
            .unwrap();
        assert_eq!(cleared.retain_until, None);
        server.abort();
    }

    /// A mark that keeps the footage until a time is posted to the retain
    /// route of its event. The test
    /// [`every_call_asks_for_the_route_of_its_request`] checks the route of a
    /// released mark.
    #[tokio::test]
    async fn a_retain_mark_is_posted_to_the_retain_route_of_its_event() {
        let seen: Seen = Arc::new(Mutex::new(Vec::new()));
        let (base, server) = retain_server(&seen).await;
        fixture::client(base)
            .retain_event(fixture::event_id(7), Retention::Until(fixture::time(2_000)))
            .await
            .unwrap();
        let asked: Vec<(String, String)> = requests(&seen)
            .into_iter()
            .map(|(method, path, _)| (method, path))
            .collect();
        assert_eq!(
            asked,
            [("POST".to_owned(), "/api/events/7/retain".to_owned())]
        );
        server.abort();
    }

    /// A request with a body says the body is JSON: the request that adds a
    /// camera carries `application/json` in `Content-Type`.
    #[tokio::test]
    async fn a_request_with_a_body_says_the_body_is_json() {
        let content_type = Arc::new(Mutex::new(None));
        let route = {
            let content_type = Arc::clone(&content_type);
            routing::post(move |headers: HeaderMap| {
                let content_type = Arc::clone(&content_type);
                async move {
                    *content_type.lock().unwrap() = headers
                        .get(header::CONTENT_TYPE)
                        .and_then(|value| value.to_str().ok())
                        .map(str::to_owned);
                    StatusCode::NOT_FOUND
                }
            })
        };
        let (base, server) = fixture::serve(Router::new().route("/api/cameras", route)).await;
        let _ = fixture::client(base).add_camera(&new_camera("cam8")).await;
        assert_eq!(
            content_type.lock().unwrap().as_deref(),
            Some("application/json")
        );
        server.abort();
    }

    /// Makes every call of a client with the bearer `t0ken`, the export
    /// among them; gives the method, the path, and the bearer of each
    /// request, in the order of the calls. A fallback records each request
    /// and answers it with 404, which each call refuses or, for a keyframe,
    /// takes as nothing recorded: only the requests matter here.
    async fn requests_of_every_call() -> Vec<(String, String, Option<String>)> {
        let seen: Seen = Arc::new(Mutex::new(Vec::new()));
        let app = Router::new().fallback({
            let seen = Arc::clone(&seen);
            move |method: Method, uri: Uri, headers: HeaderMap| {
                let seen = Arc::clone(&seen);
                async move {
                    let path = uri.path().to_owned();
                    record(&seen, method.as_str(), path, &headers, String::new());
                    StatusCode::NOT_FOUND
                }
            }
        });
        let (base, server) = fixture::serve(app).await;
        let client = bearing_client(base);
        let camera = fixture::camera("cam1");
        let window = fixture::window(10, 30);
        let destination = tempfile::tempdir().unwrap();
        let _ = client.cameras().await;
        let _ = client.add_camera(&new_camera("cam8")).await;
        let _ = client.remove_camera(&fixture::camera("cam8")).await;
        let _ = client.segments(&camera, StreamKind::Main, window).await;
        let _ = client.events(&camera, window, 50).await;
        let _ = client
            .retain_event(fixture::event_id(7), Retention::Release)
            .await;
        let _ = client
            .keyframe(&camera, StreamKind::Main, fixture::time(20))
            .await;
        let _ = client.vod_playlist(&camera, StreamKind::Main, window).await;
        let _ = client.discover(Duration::from_secs(3)).await;
        let _ = client.stream_urls(&lookup("192.0.2.20")).await;
        let export = destination.path().join("export.mp4");
        let _ = client
            .export(&camera, StreamKind::Main, window, &export, |_| {})
            .await;
        server.abort();
        requests(&seen)
    }

    /// Every call of the client sends the bearer in `Authorization`, the
    /// export's among them: each of the eleven requests carries it.
    #[tokio::test]
    async fn every_call_sends_the_bearer() {
        let sent: Vec<Option<String>> = requests_of_every_call()
            .await
            .into_iter()
            .map(|(_, _, token)| token)
            .collect();
        assert_eq!(sent, vec![Some("Bearer t0ken".to_owned()); 11]);
    }

    /// Every call of the client asks for the route of its request, with the
    /// request's method. The export is among the calls.
    #[tokio::test]
    async fn every_call_asks_for_the_route_of_its_request() {
        let asked: Vec<(String, String)> = requests_of_every_call()
            .await
            .into_iter()
            .map(|(method, path, _)| (method, path))
            .collect();
        assert_eq!(
            asked,
            [
                ("GET", "/api/cameras"),
                ("POST", "/api/cameras"),
                ("DELETE", "/api/cameras/cam8"),
                ("GET", "/api/streams/cam1/main/segments"),
                ("GET", "/api/events"),
                ("POST", "/api/events/7/retain"),
                ("GET", "/api/streams/cam1/main/keyframe"),
                ("GET", "/api/streams/cam1/main/hls/vod.m3u8"),
                ("GET", "/api/onvif/devices"),
                ("POST", "/api/onvif/streams"),
                ("GET", "/api/cameras/cam1/export.mp4"),
            ]
            .map(|(method, path)| (method.to_owned(), path.to_owned()))
        );
    }

    /// The start of a one-hour window, in microseconds after the epoch.
    const WINDOW_FROM: i64 = 1_794_076_005_736_367;
    /// The end of the window that [`WINDOW_FROM`] starts.
    const WINDOW_TO: i64 = 1_794_079_605_736_367;

    /// The segments the client lists for the window from [`WINDOW_FROM`] to
    /// [`WINDOW_TO`] when the daemon answers with the rows `body`.
    async fn listed_segments(body: &'static str) -> Vec<Segment> {
        let (base, server) = fixture::serve(Router::new().route(
            "/api/streams/{camera}/{kind}/segments",
            routing::get(move || async move { body }),
        ))
        .await;
        let segments = fixture::client(base)
            .segments(
                &fixture::camera("cam1"),
                StreamKind::Main,
                fixture::window(WINDOW_FROM, WINDOW_TO),
            )
            .await
            .unwrap();
        server.abort();
        segments
    }

    /// A file opened 3 h before the window's end whose last durable
    /// progress was 2 h before it recorded nothing in the window.
    #[tokio::test]
    async fn a_window_with_only_a_stale_open_row_lists_nothing_recorded() {
        let stale = concat!(
            r#"[{"id":1,"camera":"cam1","kind":"main","run_seq":1,"discontinuity":true,"#,
            r#""wall_start_us":1794068805736367,"wall_end_us":1794072405736367,"#,
            r#""fragments":10,"bytes":4096,"sealed":false}]"#
        );
        assert_eq!(listed_segments(stale).await, []);
    }

    /// The open row of a file before its first sync holds no fragment and
    /// records nothing in the window.
    #[tokio::test]
    async fn a_window_with_only_an_open_row_before_its_first_sync_lists_nothing_recorded() {
        let unsynced = concat!(
            r#"[{"id":2,"camera":"cam1","kind":"main","run_seq":1,"discontinuity":true,"#,
            r#""wall_start_us":1794079604736367,"wall_end_us":1794079604736367,"#,
            r#""fragments":0,"bytes":768,"sealed":false}]"#
        );
        assert_eq!(listed_segments(unsynced).await, []);
    }

    /// A route that answers `body` as JSON and keeps in `asked` the target
    /// it was asked for.
    fn listing(asked: &Arc<Mutex<Vec<String>>>, body: &'static str) -> MethodRouter {
        let asked = Arc::clone(asked);
        routing::get(move |uri: Uri| {
            let asked = Arc::clone(&asked);
            async move {
                asked.lock().unwrap().push(uri.to_string());
                (StatusCode::OK, JSON, body)
            }
        })
    }

    /// A server that lists one segment and one event, with a client of it
    /// and the targets it was asked for.
    async fn listing_server() -> (Client, Arc<Mutex<Vec<String>>>, JoinHandle<()>) {
        let asked = Arc::new(Mutex::new(Vec::new()));
        let segment = r#"[{"wall_start_us":12,"wall_end_us":20,"sealed":true}]"#;
        let event = r#"[{"id":7,"kind":"motion","nvr_time":15,"retain_until_us":null}]"#;
        let (base, server) = fixture::serve(
            Router::new()
                .route(
                    "/api/streams/{camera}/{kind}/segments",
                    listing(&asked, segment),
                )
                .route("/api/events", listing(&asked, event)),
        )
        .await;
        (fixture::client(base), asked, server)
    }

    #[tokio::test]
    async fn segments_are_the_rows_the_daemon_lists() {
        let (client, _asked, server) = listing_server().await;
        let segments = client
            .segments(
                &fixture::camera("cam1"),
                StreamKind::Main,
                fixture::window(10, 30),
            )
            .await
            .unwrap();
        assert_eq!(
            segments,
            [Segment {
                start: fixture::time(12),
                end: fixture::time(20),
                sealed: true
            }]
        );
        server.abort();
    }

    #[tokio::test]
    async fn a_segment_list_asks_for_the_window() {
        let (client, asked, server) = listing_server().await;
        client
            .segments(
                &fixture::camera("cam1"),
                StreamKind::Main,
                fixture::window(10, 30),
            )
            .await
            .unwrap();
        assert_eq!(
            *asked.lock().unwrap(),
            ["/api/streams/cam1/main/segments?from=10&to=30"]
        );
        server.abort();
    }

    #[tokio::test]
    async fn events_are_the_rows_the_daemon_lists() {
        let (client, _asked, server) = listing_server().await;
        let events = client
            .events(&fixture::camera("cam1"), fixture::window(10, 30), 50)
            .await
            .unwrap();
        assert_eq!(
            events,
            [EventRecord {
                id: fixture::event_id(7),
                kind: EventKind::Named("motion".to_owned()),
                nvr_time: fixture::time(15),
                retain_until: None
            }]
        );
        server.abort();
    }

    #[tokio::test]
    async fn an_event_list_asks_for_a_page_of_its_window() {
        let (client, asked, server) = listing_server().await;
        client
            .events(&fixture::camera("cam1"), fixture::window(10, 30), 50)
            .await
            .unwrap();
        assert_eq!(
            *asked.lock().unwrap(),
            ["/api/events?camera=cam1&from=10&to=30&limit=50"]
        );
        server.abort();
    }

    /// A client of a server whose playlist route redirects to a second
    /// server, with the count of the requests the second server receives.
    async fn redirecting_server() -> (Client, Arc<AtomicUsize>, [JoinHandle<()>; 2]) {
        let other_requests = Arc::new(AtomicUsize::new(0));
        let (other, other_server) = fixture::serve(Router::new().fallback({
            let other_requests = Arc::clone(&other_requests);
            move || async move {
                other_requests.fetch_add(1, Ordering::SeqCst);
                "#EXTM3U\n"
            }
        }))
        .await;
        let target = other.url().join("/vod.m3u8").unwrap().to_string();
        let (base, server) = fixture::serve(Router::new().route(
            "/api/streams/{camera}/{kind}/hls/vod.m3u8",
            routing::get(move || async move {
                Response::builder()
                    .status(StatusCode::FOUND)
                    .header(header::LOCATION, target)
                    .body(Body::empty())
                    .unwrap()
            }),
        ))
        .await;
        (
            fixture::client(base),
            other_requests,
            [server, other_server],
        )
    }

    /// The playlist of the window `[1, 2)`, which the redirecting server
    /// answers with 302.
    async fn redirected_playlist(client: &Client) -> Result<VodPlaylist, ClientError> {
        client
            .vod_playlist(
                &fixture::camera("cam1"),
                StreamKind::Main,
                fixture::window(1, 2),
            )
            .await
    }

    #[tokio::test]
    async fn the_client_does_not_follow_a_redirect() {
        let (client, other_requests, servers) = redirecting_server().await;
        let _ = redirected_playlist(&client).await;
        assert_eq!(other_requests.load(Ordering::SeqCst), 0);
        for server in servers {
            server.abort();
        }
    }

    #[tokio::test]
    async fn a_redirected_playlist_is_refused_with_its_status() {
        let (client, _other_requests, servers) = redirecting_server().await;
        let error = redirected_playlist(&client)
            .await
            .expect_err("a redirected playlist is refused");
        assert!(error.to_string().contains("302"), "{error}");
        for server in servers {
            server.abort();
        }
    }

    #[tokio::test]
    async fn a_body_beyond_its_byte_limit_is_refused() {
        let chunked = || async {
            Body::from_stream(stream::iter([
                Ok::<Bytes, io::Error>(Bytes::from_static(b"ab")),
                Ok(Bytes::from_static(b"cd")),
            ]))
        };
        let (base, server) =
            fixture::serve(Router::new().route("/chunked", routing::get(chunked))).await;
        let client = fixture::client(base.clone());
        let mut response = client
            .http
            .get(base.url().join("/chunked").unwrap())
            .send()
            .await
            .unwrap();
        assert!(
            matches!(
                read_body_limited(&mut response, 3, Call::Cameras).await,
                Err(BodyError::Oversized)
            ),
            "the body is refused as oversized"
        );
        server.abort();
    }

    #[tokio::test]
    async fn a_stalled_body_ends_at_the_read_idle_deadline() {
        use futures::StreamExt as _;

        let stalled = || async {
            let body = stream::once(async { Ok::<Bytes, io::Error>(Bytes::from_static(b"[")) })
                .chain(stream::pending::<Result<Bytes, io::Error>>());
            Response::builder()
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from_stream(body))
                .unwrap()
        };
        let (base, server) =
            fixture::serve(Router::new().route("/api/cameras", routing::get(stalled))).await;
        let client = Client::new_with_timeouts(
            base,
            None,
            fixture::trust(),
            Duration::from_secs(1),
            Duration::from_millis(30),
        );
        assert!(
            tokio::time::timeout(Duration::from_secs(1), client.cameras())
                .await
                .expect("client read deadline must fire")
                .is_err()
        );
        server.abort();
    }

    /// A body that never ends, behind a head of `status` and `content_type`.
    fn endless_answer(status: StatusCode, content_type: &str) -> Response {
        Response::builder()
            .status(status)
            .header(header::CONTENT_TYPE, content_type)
            .body(Body::from_stream(
                stream::pending::<Result<Bytes, io::Error>>(),
            ))
            .unwrap()
    }

    /// A server whose keyframe route answers with `answer`.
    async fn keyframe_server(answer: fn() -> Response) -> (ApiOrigin, JoinHandle<()>) {
        fixture::serve(Router::new().route(
            "/api/streams/{camera}/{kind}/keyframe",
            routing::get(move || async move { answer() }),
        ))
        .await
    }

    /// The refusal of a keyframe answered with another media type, `text/html`,
    /// from a head whose body never ends.
    async fn other_media_type_refusal() -> ClientError {
        let (base, server) = keyframe_server(|| endless_answer(StatusCode::OK, "text/html")).await;
        let client = fixture::client(base);
        let error = tokio::time::timeout(
            Duration::from_secs(5),
            client.keyframe(&fixture::camera("cam1"), StreamKind::Main, fixture::time(5)),
        )
        .await
        .expect("the head alone refuses the keyframe")
        .unwrap_err();
        server.abort();
        error
    }

    /// A keyframe answered with another media type is refused from its head:
    /// the client reads none of its body, which here never ends, and shows
    /// the refusal of the content type.
    #[tokio::test]
    async fn a_keyframe_of_another_media_type_is_refused_from_its_head() {
        let error = other_media_type_refusal().await;
        assert_eq!(error.to_string(), "response is not video/mp4");
    }

    #[tokio::test]
    async fn a_keyframe_of_another_media_type_is_refused_without_a_source() {
        let error = other_media_type_refusal().await;
        assert!(error.source().is_none(), "{error:?}");
    }

    /// A keyframe's 404 says nothing is recorded from its head alone; the
    /// client reads none of its body.
    #[tokio::test]
    async fn a_keyframe_404_is_answered_from_its_head() {
        let (base, server) =
            keyframe_server(|| endless_answer(StatusCode::NOT_FOUND, "application/json")).await;
        let client = fixture::client(base);
        let keyframe = tokio::time::timeout(
            Duration::from_secs(5),
            client.keyframe(&fixture::camera("cam1"), StreamKind::Main, fixture::time(5)),
        )
        .await
        .expect("the head alone answers a 404");
        assert_eq!(keyframe.unwrap(), None);
        server.abort();
    }

    /// A keyframe longer than the 16 KiB a refusal's message may take is read
    /// whole: a value's limit is its call's, not a refusal's.
    #[tokio::test]
    async fn a_keyframe_longer_than_a_refusal_message_is_read_whole() {
        let (base, server) = keyframe_server(|| {
            fixture::mp4_response(Body::from(
                (0..(32 << 10))
                    .map(|i| (i % 251) as u8)
                    .collect::<Vec<u8>>(),
            ))
        })
        .await;
        let client = fixture::client(base);
        let keyframe = client
            .keyframe(&fixture::camera("cam1"), StreamKind::Main, fixture::time(5))
            .await
            .unwrap()
            .expect("a keyframe");
        assert_eq!(keyframe.len(), 32 << 10);
        assert!(
            keyframe
                .iter()
                .enumerate()
                .all(|(i, byte)| usize::from(*byte) == i % 251),
            "the keyframe is the body served"
        );
        server.abort();
    }

    /// The refusal of adding a camera when the daemon answers with
    /// `answer`.
    async fn added_camera_refusal(answer: fn() -> Response) -> ClientError {
        let (base, server) = fixture::serve(Router::new().route(
            "/api/cameras",
            routing::post(move || async move { answer() }),
        ))
        .await;
        let error = fixture::client(base)
            .add_camera(&new_camera("cam8"))
            .await
            .unwrap_err();
        server.abort();
        error
    }

    /// The refusal of adding a camera when the head of the answer declares a
    /// length one byte past the call's limit, 16 KiB. The server sends the
    /// head and no byte of the body, and keeps the connection open, so only
    /// the declared length can refuse the answer.
    async fn declared_past_limit_refusal() -> ClientError {
        let head = format!(
            "HTTP/1.1 201 Created\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n",
            MAX_CAMERA_BODY_BYTES + 1
        );
        let (base, server) = fixture::serve_raw(head.into_bytes(), Then::Hold).await;
        let client = fixture::client(base);
        let camera = new_camera("cam8");
        let error = tokio::time::timeout(Duration::from_secs(5), client.add_camera(&camera))
            .await
            .expect("the head alone refuses the answer")
            .unwrap_err();
        server.abort();
        error
    }

    /// An answer whose head declares a length past its call's limit, 16 KiB
    /// for an added camera, is refused before any of its body is read.
    #[tokio::test]
    async fn an_answer_declared_past_its_calls_byte_limit_is_refused_as_too_large() {
        let error = declared_past_limit_refusal().await;
        assert_eq!(
            error.to_string(),
            "the answer to adding the camera exceeds its byte limit"
        );
    }

    #[tokio::test]
    async fn an_answer_declared_past_its_calls_byte_limit_is_refused_without_a_source() {
        let error = declared_past_limit_refusal().await;
        assert!(error.source().is_none(), "{error:?}");
    }

    /// The refusal of adding a camera when the answer streams 20 KiB, past
    /// the call's limit of 16 KiB, with no length declared.
    async fn streamed_past_limit_refusal() -> ClientError {
        added_camera_refusal(|| {
            let chunks = (0..=4).map(|_| Ok::<Bytes, io::Error>(Bytes::from(vec![b'x'; 4 << 10])));
            Response::builder()
                .status(StatusCode::CREATED)
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from_stream(stream::iter(chunks)))
                .unwrap()
        })
        .await
    }

    /// An answer past its call's limit, 16 KiB for an added camera, is
    /// refused as it streams past the limit, with no length declared.
    #[tokio::test]
    async fn an_answer_streamed_past_its_calls_byte_limit_is_refused_as_too_large() {
        let error = streamed_past_limit_refusal().await;
        assert_eq!(
            error.to_string(),
            "the answer to adding the camera exceeds its byte limit"
        );
    }

    #[tokio::test]
    async fn an_answer_streamed_past_its_calls_byte_limit_is_refused_without_a_source() {
        let error = streamed_past_limit_refusal().await;
        assert!(error.source().is_none(), "{error:?}");
    }

    /// A conflict whose JSON body, padded, is `length` bytes long.
    fn conflict(length: usize) -> Response {
        let head = r#"{"error":"in use","pad":""#;
        let tail = r#""}"#;
        let body = format!(
            "{head}{}{tail}",
            "p".repeat(length - head.len() - tail.len())
        );
        Response::builder()
            .status(StatusCode::CONFLICT)
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body))
            .unwrap()
    }

    /// A refusal's body is read for the daemon's message up to 16 KiB: a
    /// message in a body of that length shows.
    #[tokio::test]
    async fn a_refusal_shows_a_message_within_its_limit() {
        let error = added_camera_refusal(|| conflict(MAX_ERROR_BODY_BYTES)).await;
        assert_eq!(
            error.to_string(),
            "adding the camera failed with 409 Conflict: in use"
        );
    }

    #[tokio::test]
    async fn a_refusal_with_a_message_within_its_limit_has_no_source() {
        let error = added_camera_refusal(|| conflict(MAX_ERROR_BODY_BYTES)).await;
        assert!(error.source().is_none(), "{error:?}");
    }

    /// A message in a refusal's body longer than 16 KiB is dropped, and the
    /// refusal stands without it.
    #[tokio::test]
    async fn a_refusal_drops_a_message_past_its_limit() {
        let error = added_camera_refusal(|| conflict(MAX_ERROR_BODY_BYTES + 1)).await;
        assert_eq!(
            error.to_string(),
            "adding the camera failed with 409 Conflict"
        );
    }

    #[tokio::test]
    async fn a_refusal_whose_message_is_past_its_limit_has_no_source() {
        let error = added_camera_refusal(|| conflict(MAX_ERROR_BODY_BYTES + 1)).await;
        assert!(error.source().is_none(), "{error:?}");
    }

    /// A client of a server that answers every request with `status` and a
    /// JSON body that fails to arrive: the bytes `body`, and then the end of
    /// the connection before the body's end.
    async fn cut_short_client(status: StatusCode, body: &str) -> (Client, JoinHandle<()>) {
        let (base, server) =
            fixture::serve_cut_short(status, "application/json", body.as_bytes()).await;
        (fixture::client(base), server)
    }

    /// A client whose camera list arrives cut short after `[]`.
    async fn cut_short_list_client() -> (Client, JoinHandle<()>) {
        cut_short_client(StatusCode::OK, "[]").await
    }

    /// The bytes of a 503 refusal that arrive before its body fails hold
    /// the daemon's whole message, and the refusal still stands without it.
    #[tokio::test]
    async fn a_refusal_whose_body_fails_to_arrive_stands_without_its_message() {
        let busy = r#"{"error":"busy"}"#;
        let (client, server) = cut_short_client(StatusCode::SERVICE_UNAVAILABLE, busy).await;
        let error = client.discover(Duration::from_secs(3)).await.unwrap_err();
        assert!(
            matches!(
                error,
                ClientError::Api(ApiError::Refused { detail: None, .. })
            ),
            "{error:?}"
        );
        assert_eq!(
            error.to_string(),
            "discovering cameras failed with 503 Service Unavailable"
        );
        server.abort();
    }

    #[tokio::test]
    async fn a_value_whose_body_fails_to_arrive_fails_the_call() {
        let (client, server) = cut_short_list_client().await;
        let error = client.cameras().await.unwrap_err();
        assert!(
            matches!(
                error,
                ClientError::Read {
                    call: Call::Cameras,
                    ..
                }
            ),
            "{error:?}"
        );
        assert_eq!(
            error.to_string(),
            "reading the answer to listing the cameras"
        );
        server.abort();
    }

    #[tokio::test]
    async fn a_value_whose_body_fails_to_arrive_keeps_the_transport_error_as_its_source() {
        let (client, server) = cut_short_list_client().await;
        let error = client.cameras().await.unwrap_err();
        let source = error.source().expect("the transport's error is the source");
        assert!(source.is::<reqwest::Error>(), "{source:?}");
        server.abort();
    }

    /// A listener on the loopback that closes each connection it accepts
    /// before it reads a byte, so a request to it fails in the transport.
    /// Gives the listener's origin and the task that accepts. The task holds
    /// the port for as long as it runs.
    async fn closing_server() -> (ApiOrigin, JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            while let Ok((connection, _)) = listener.accept().await {
                drop(connection);
            }
        });
        let origin = ApiOrigin::parse(&format!("http://{address}"), PlainHttp::LoopbackOnly);
        (origin.unwrap(), server)
    }

    /// A failure of the transport names the call. The connection closes
    /// before an answer, on a port the fixture holds throughout.
    #[tokio::test]
    async fn a_transport_failure_names_the_call() {
        let (origin, server) = closing_server().await;
        let error = fixture::client(origin).cameras().await.unwrap_err();
        assert!(
            matches!(
                error,
                ClientError::Transport {
                    call: Call::Cameras,
                    ..
                }
            ),
            "{error:?}"
        );
        assert_eq!(error.to_string(), "listing the cameras");
        server.abort();
    }

    #[tokio::test]
    async fn a_transport_failure_keeps_the_error_it_met_as_its_source() {
        let (origin, server) = closing_server().await;
        let error = fixture::client(origin).cameras().await.unwrap_err();
        let source = error.source().expect("the transport's error is the source");
        assert!(source.is::<reqwest::Error>(), "{source:?}");
        server.abort();
    }

    /// A reservation the allocator refuses for the VOD playlist's answer.
    fn refused_reservation() -> ClientError {
        ClientError::Reserve {
            call: Call::VodPlaylist,
            source: Vec::<u8>::new().try_reserve(usize::MAX).unwrap_err(),
        }
    }

    #[test]
    fn a_refused_reservation_names_the_answer_it_was_to_hold() {
        assert_eq!(
            refused_reservation().to_string(),
            "reserving the answer to fetching the VOD playlist"
        );
    }

    #[test]
    fn a_refused_reservation_keeps_the_allocators_refusal_as_its_source() {
        let reserving = refused_reservation();
        let source = reserving
            .source()
            .expect("the allocator's refusal is the source");
        assert!(source.is::<TryReserveError>(), "{source:?}");
    }
}
