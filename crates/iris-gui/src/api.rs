//! The irisd HTTP API calls the viewer makes: the camera list, the segment and
//! event lists, the VOD playlist, keyframes, exports, retain marks, camera
//! provisioning, and the ONVIF lookups. Every request carries the root bearer in
//! `Authorization` and follows no redirect, and every answer is read under a
//! byte bound and validated before the viewer uses it.

use std::collections::HashSet;
use std::fmt;
use std::fs::File;
use std::io::{self, ErrorKind};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::Context;
use iris_core::id::{CameraId, StreamKind};
use iris_core::time::WallUs;
use iris_tls::ClientConfig;
use reqwest::redirect::Policy;
use reqwest::{RequestBuilder, Response, StatusCode};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use tracing::warn;
use url::Url;

use crate::vod::{self, MAX_VOD_PLAYLIST_BYTES, PlaybackFragment};

/// Maximum time spent establishing an API connection.
const HTTP_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
/// Maximum wait for a response's headers or for its next body bytes.
const HTTP_READ_IDLE_TIMEOUT: Duration = Duration::from_secs(30);
/// The wait for a stream lookup's answer. The daemon answers when the lookup
/// ends, which its own bound (`iris_events::media::MEDIA_LOOKUP_DEADLINE`, 60 s)
/// lets take a minute; the margin covers the request around it.
const STREAM_LOOKUP_READ_TIMEOUT: Duration = Duration::from_secs(70);
/// Bounds the configured-camera response before it becomes GTK widgets and feeds.
const MAX_CAMERAS_BODY_BYTES: usize = 1 << 20;
/// Bounds the one camera `POST /api/cameras` answers with.
const MAX_CAMERA_BODY_BYTES: usize = 16 << 10;
/// Bounds a discovery answer: at most [`MAX_DISCOVERED_DEVICES`] devices of
/// bounded text.
const MAX_DEVICES_BODY_BYTES: usize = 512 << 10;
/// Bounds a stream lookup answer: at most [`MAX_PROFILE_STREAMS`] profiles.
const MAX_STREAMS_BODY_BYTES: usize = 64 << 10;
/// Bounds an error body read for its message.
const MAX_ERROR_BODY_BYTES: usize = 16 << 10;
/// Longest server message shown in the status bar.
const MAX_ERROR_MESSAGE_CHARS: usize = 200;
/// More cameras than a viewer can usefully render are rejected before widget creation.
const MAX_CAMERAS: usize = 256;
/// Camera names and event kinds stay small enough for labels and diagnostics.
const MAX_DISPLAY_STRING_BYTES: usize = 1024;
/// Most discovered devices accepted from the daemon, its own limit
/// (`iris_events::discovery::MAX_DISCOVERED`).
const MAX_DISCOVERED_DEVICES: usize = 64;
/// Most profile streams accepted from the daemon, its own limit
/// (`iris_events::media::MAX_STREAM_PROFILES`).
const MAX_PROFILE_STREAMS: usize = 8;
/// Longest discovery scope, profile token, or profile name the daemon sends
/// (`iris_events::discovery::MAX_TEXT_CHARS`).
const MAX_LABEL_CHARS: usize = 128;
/// Longest endpoint reference the daemon sends.
const MAX_ENDPOINT_CHARS: usize = 256;
/// Longest host name as text (RFC 1035 §2.3.4 allows 255 octets on the wire).
const MAX_HOST_BYTES: usize = 253;
/// Longest stream URL the daemon takes from a camera
/// (`iris_events::MAX_ADVERTISED_URL_LEN`).
const MAX_STREAM_URL_BYTES: usize = 2_048;
/// Bounds segment/event JSON responses from the API.
const MAX_LIST_BODY_BYTES: usize = 16 << 20;
/// Matches the API's maximum segment rows per range response.
const MAX_SEGMENTS: usize = 4_096;
/// Matches the API's maximum event page.
const MAX_EVENTS: usize = 10_000;
/// Matches the API's maximum time-range request, in microseconds.
const MAX_RANGE_US: i128 = 7 * 86_400 * 1_000_000;
/// A keyframe can contain a 4 MiB init and one sample from a 66 MiB fragment,
/// plus bounded re-encoded boxes.
const MAX_KEYFRAME_BYTES: usize = 71 << 20;
/// A retain response contains one event rather than a page.
const MAX_RETAIN_BODY_BYTES: usize = 64 << 10;
/// Number of exclusive temporary-name attempts before reporting a hostile/full directory.
const EXPORT_TEMP_ATTEMPTS: usize = 16;
/// The least free space an export leaves on its filesystem; 2 % of the
/// filesystem applies when that is larger.
const EXPORT_RESERVE_MIN_BYTES: u64 = 1 << 30;
/// Bytes an export writes between free-space checks.
const EXPORT_SPACE_CHECK_BYTES: u64 = 64 << 20;
/// Lowercase hexadecimal digits for private temporary names.
const HEX_DIGITS: &[u8; 16] = b"0123456789abcdef";

/// One camera as `/api/cameras` lists it.
#[derive(Clone, Debug, Deserialize)]
pub struct Camera {
    /// The configured camera id.
    pub id: CameraId,
    /// The configured display name.
    pub name: String,
    /// Added through the API, so the API may remove it.
    pub provisioned: bool,
}

/// A catalog event id as `/api/events` lists it; the validated answers hold
/// positive ids only.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Deserialize)]
#[serde(transparent)]
pub struct EventId(pub i64);

impl fmt::Display for EventId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// A camera WS-Discovery found, as `GET /api/onvif/devices` lists it.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub struct DiscoveredDevice {
    /// The device's endpoint reference.
    pub endpoint: String,
    /// The host of its device service: a name, or an address without brackets.
    pub host: String,
    /// The port of its device service.
    pub port: u16,
    /// Whether the device service uses HTTPS.
    pub https: bool,
    /// The device's name scope.
    pub name: Option<String>,
    /// The device's hardware scope.
    pub hardware: Option<String>,
    /// The device's location scope.
    pub location: Option<String>,
}

/// One ONVIF media profile's RTSP URL, as `POST /api/onvif/streams` lists it.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub struct ProfileStream {
    /// The profile token.
    pub token: String,
    /// The profile name.
    pub name: String,
    /// The RTSP URL on the camera's host, without credentials.
    pub url: String,
}

/// The body of `POST /api/onvif/streams`. Its `Debug` output redacts the password.
#[derive(Clone, Serialize)]
pub struct StreamUrlsRequest {
    /// Host name or address.
    pub host: String,
    /// Port of the ONVIF device service.
    pub http_port: u16,
    /// Reach the camera over HTTPS.
    pub https: bool,
    /// Verify the camera's certificate when `https` is set.
    pub tls_verify: bool,
    /// HTTP user.
    pub username: String,
    /// HTTP password.
    pub password: String,
}

impl fmt::Debug for StreamUrlsRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("StreamUrlsRequest")
            .field("host", &self.host)
            .field("http_port", &self.http_port)
            .field("https", &self.https)
            .field("tls_verify", &self.tls_verify)
            .field("username", &self.username)
            .field("password", &"<redacted>")
            .finish()
    }
}

/// Space on the filesystem an export writes to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct FilesystemSpace {
    /// Bytes an unprivileged writer may still use.
    available: u64,
    /// The filesystem's size in bytes.
    total: u64,
}

/// Reads the space of the filesystem holding an open file; it may block.
type SpaceProbe = fn(&File) -> io::Result<FilesystemSpace>;

/// `fstatvfs` on the export's own descriptor, so the figure is for the
/// filesystem the bytes land on.
fn filesystem_space(file: &File) -> io::Result<FilesystemSpace> {
    let stat = rustix::fs::fstatvfs(file)?;
    Ok(FilesystemSpace {
        available: stat.f_bavail.saturating_mul(stat.f_frsize),
        total: stat.f_blocks.saturating_mul(stat.f_frsize),
    })
}

/// The free space an export leaves: the larger of 1 GiB and 2 % of the filesystem.
fn export_reserve(total: u64) -> u64 {
    EXPORT_RESERVE_MIN_BYTES.max(total / 50)
}

fn check_export_space(space: FilesystemSpace) -> anyhow::Result<()> {
    let reserve = export_reserve(space.total);
    anyhow::ensure!(
        space.available >= reserve,
        "export stopped: {} MiB free on the destination filesystem is below its {} MiB reserve",
        space.available >> 20,
        reserve >> 20
    );
    Ok(())
}

/// An authenticated API client. Its `Debug` output redacts the token.
pub struct Client {
    /// The API origin; `main::check_api_url` admits no path, so the absolute
    /// paths the client joins replace nothing.
    base: Url,
    token: Option<String>,
    http: reqwest::Client,
    /// The client for stream lookups, whose answer can take the daemon's whole
    /// lookup bound to start ([`STREAM_LOOKUP_READ_TIMEOUT`]).
    lookup_http: reqwest::Client,
    /// Free-space probe for exports; tests inject a fixed answer.
    space_probe: SpaceProbe,
    /// Bytes an export writes between free-space checks.
    space_check_bytes: u64,
}

impl fmt::Debug for Client {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Client")
            .field("base", &self.base.as_str())
            .field("token", &self.token.as_ref().map(|_| "<redacted>"))
            .finish_non_exhaustive()
    }
}

/// One of the client's HTTP clients. The daemon never redirects; following one
/// would let a response name the origin that playlist references are checked
/// against.
fn http_client(
    tls: &ClientConfig,
    connect_timeout: Duration,
    read_timeout: Duration,
) -> reqwest::Client {
    reqwest::Client::builder()
        .connect_timeout(connect_timeout)
        .read_timeout(read_timeout)
        .redirect(Policy::none())
        .tls_backend_preconfigured(tls.clone())
        .build()
        .expect("static HTTP client options are valid")
}

impl Client {
    /// Creates a client for `base` with an optional bearer token; `https`
    /// connections verify the server with `tls`, the trust the live feeds use too.
    pub fn new(base: Url, token: Option<String>, tls: &ClientConfig) -> Self {
        Self::with_timeouts(
            base,
            token,
            tls,
            HTTP_CONNECT_TIMEOUT,
            HTTP_READ_IDLE_TIMEOUT,
        )
    }

    /// A client trusting the platform's certificate store, for tests.
    #[cfg(test)]
    pub(crate) fn for_tests(base: Url, token: Option<String>) -> Self {
        Self::new(
            base,
            token,
            &iris_tls::client_config(None).expect("platform trust store"),
        )
    }

    fn with_timeouts(
        base: Url,
        token: Option<String>,
        tls: &ClientConfig,
        connect_timeout: Duration,
        read_timeout: Duration,
    ) -> Self {
        Self {
            base,
            token,
            http: http_client(tls, connect_timeout, read_timeout),
            lookup_http: http_client(
                tls,
                connect_timeout,
                read_timeout.max(STREAM_LOOKUP_READ_TIMEOUT),
            ),
            space_probe: filesystem_space,
            space_check_bytes: EXPORT_SPACE_CHECK_BYTES,
        }
    }

    /// `path`, an absolute path, on the API origin.
    fn url(&self, path: &str) -> anyhow::Result<Url> {
        self.base
            .join(path)
            .with_context(|| format!("building the request URL {path}"))
    }

    /// `request` with the root bearer: every request the client sends is built
    /// through here.
    fn authorized(&self, request: RequestBuilder) -> RequestBuilder {
        match &self.token {
            Some(token) => request.bearer_auth(token),
            None => request,
        }
    }

    fn get(&self, url: Url) -> RequestBuilder {
        self.authorized(self.http.get(url))
    }

    /// A JSON `GET` whose answer is a success status and at most `maximum`
    /// bytes; `what` names the request in errors ("listing the cameras").
    async fn fetch<T: DeserializeOwned>(
        &self,
        url: Url,
        what: &str,
        maximum: usize,
    ) -> anyhow::Result<T> {
        let response = self
            .get(url)
            .send()
            .await
            .with_context(|| what.to_owned())?;
        if !response.status().is_success() {
            return Err(refusal(what, response).await);
        }
        let body = read_body_limited(response, maximum, what).await?;
        serde_json::from_slice(&body).with_context(|| format!("decoding the answer to {what}"))
    }

    /// Lists the cameras.
    pub async fn cameras(&self) -> anyhow::Result<Vec<Camera>> {
        let cameras: Vec<Camera> = self
            .fetch(
                self.url("/api/cameras")?,
                "listing the cameras",
                MAX_CAMERAS_BODY_BYTES,
            )
            .await?;
        validate_cameras(&cameras)?;
        Ok(cameras)
    }

    /// Adds a camera through `POST /api/cameras`; the daemon starts recording
    /// it and answers with the camera as the list shows it.
    pub async fn add_camera(&self, camera: &NewCamera) -> anyhow::Result<Camera> {
        const WHAT: &str = "adding the camera";
        let body = serde_json::to_vec(camera).context("encoding the camera")?;
        let response = self
            .authorized(self.http.post(self.url("/api/cameras")?))
            .header("content-type", "application/json")
            .body(body)
            .send()
            .await
            .context(WHAT)?;
        if response.status() != StatusCode::CREATED {
            return Err(refusal(WHAT, response).await);
        }
        let bytes = read_body_limited(response, MAX_CAMERA_BODY_BYTES, WHAT).await?;
        let added: Camera = serde_json::from_slice(&bytes).context("decoding the camera")?;
        validate_cameras(std::slice::from_ref(&added))?;
        anyhow::ensure!(
            added.id == camera.id,
            "the daemon answered with another camera"
        );
        Ok(added)
    }

    /// Removes a camera the API added, through `DELETE /api/cameras/{camera}`.
    pub async fn remove_camera(&self, id: &CameraId) -> anyhow::Result<()> {
        const WHAT: &str = "removing the camera";
        let response = self
            .authorized(self.http.delete(self.url(&format!("/api/cameras/{id}"))?))
            .send()
            .await
            .context(WHAT)?;
        if response.status() != StatusCode::NO_CONTENT {
            return Err(refusal(WHAT, response).await);
        }
        Ok(())
    }

    /// Lists the segments of one stream that hold recordings inside `[from, to)`.
    /// Open segments that have recorded nothing there, such as a stale file of an
    /// offline camera or a file before its first sync, are omitted.
    pub async fn segments(
        &self,
        camera: &CameraId,
        kind: StreamKind,
        from: WallUs,
        to: WallUs,
    ) -> anyhow::Result<Vec<Segment>> {
        validate_range(from, to)?;
        let mut url = self.url(&format!("/api/streams/{camera}/{kind}/segments"))?;
        url.query_pairs_mut()
            .append_pair("from", &from.get().to_string())
            .append_pair("to", &to.get().to_string());
        let segments: Vec<Segment> = self
            .fetch(url, "listing the segments", MAX_LIST_BODY_BYTES)
            .await?;
        validate_segments(&segments, from, to)?;
        Ok(recorded_in_window(segments, from))
    }

    /// Lists one camera's events in `[from, to)`, newest first as the API orders
    /// them, at most `limit` (1 to 10,000).
    pub async fn events(
        &self,
        camera: &CameraId,
        from: WallUs,
        to: WallUs,
        limit: usize,
    ) -> anyhow::Result<Vec<EventRecord>> {
        validate_range(from, to)?;
        anyhow::ensure!(
            (1..=MAX_EVENTS).contains(&limit),
            "invalid event page limit"
        );
        let mut url = self.url("/api/events")?;
        url.query_pairs_mut()
            .append_pair("camera", camera.as_str())
            .append_pair("from", &from.get().to_string())
            .append_pair("to", &to.get().to_string())
            .append_pair("limit", &limit.to_string());
        let events: Vec<EventRecord> = self
            .fetch(url, "listing the events", MAX_LIST_BODY_BYTES)
            .await?;
        validate_events(&events, from, to, limit)?;
        Ok(events)
    }

    /// The destination filesystem's space, read on the blocking pool: a stalled
    /// mount then holds a blocking thread instead of a runtime worker the live
    /// feeds share.
    async fn destination_space(&self, file: &tokio::fs::File) -> anyhow::Result<FilesystemSpace> {
        let probe = self.space_probe;
        let file = file
            .try_clone()
            .await
            .context("duplicating the export descriptor")?
            .into_std()
            .await;
        tokio::task::spawn_blocking(move || probe(&file))
            .await
            .context("running the free-space probe")?
            .context("reading the export filesystem's free space")
    }

    /// Downloads the MP4 export of `[from, to)` to `path`, calling `progress`
    /// with the bytes written so far after every chunk; returns the total.
    /// Before the first byte and after every further 64 MiB it checks the
    /// destination filesystem with `fstatvfs` and stops, removing the partial
    /// file, when free space would fall below the larger of 1 GiB and 2 % of
    /// the filesystem.
    pub async fn export(
        &self,
        camera: &CameraId,
        kind: StreamKind,
        from: WallUs,
        to: WallUs,
        path: &Path,
        progress: impl Fn(u64) + Send,
    ) -> anyhow::Result<u64> {
        use futures::StreamExt as _;
        use tokio::io::AsyncWriteExt as _;

        const WHAT: &str = "requesting the export";
        validate_range(from, to)?;
        let mut url = self.url(&format!("/api/streams/{camera}/{kind}/export.mp4"))?;
        url.query_pairs_mut()
            .append_pair("from", &from.get().to_string())
            .append_pair("to", &to.get().to_string());
        let response = self.get(url).send().await.context(WHAT)?;
        if response.status() != StatusCode::OK {
            return Err(refusal(WHAT, response).await);
        }
        require_content_type(&response, "video/mp4")?;
        let mut temporary = ExportTemp::create(path).await?;
        check_export_space(self.destination_space(temporary.file()?).await?)?;
        let mut next_space_check = self.space_check_bytes;
        let mut stream = response.bytes_stream();
        let mut written = 0u64;
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.context("reading the export")?;
            temporary
                .file()?
                .write_all(&chunk)
                .await
                .context("writing the export")?;
            written = written
                .checked_add(chunk.len() as u64)
                .context("export byte count overflows")?;
            if written >= next_space_check {
                check_export_space(self.destination_space(temporary.file()?).await?)?;
                next_space_check = written.saturating_add(self.space_check_bytes);
            }
            progress(written);
        }
        anyhow::ensure!(written > 0, "export response is empty");
        temporary.commit(path).await?;
        Ok(written)
    }

    /// Keeps the footage around event `id` from retention until `until`, or
    /// clears the mark with `None`; returns the event as the API lists it after
    /// the change.
    pub async fn retain_event(
        &self,
        id: EventId,
        until: Option<WallUs>,
    ) -> anyhow::Result<EventRecord> {
        const WHAT: &str = "retaining the event";
        anyhow::ensure!(id.0 > 0, "invalid event id");
        let body = serde_json::json!({ "until": until.map(|until| until.get().to_string()) });
        let response = self
            .authorized(
                self.http
                    .post(self.url(&format!("/api/events/{id}/retain"))?),
            )
            .header("content-type", "application/json")
            .body(body.to_string())
            .send()
            .await
            .context(WHAT)?;
        if !response.status().is_success() {
            return Err(refusal(WHAT, response).await);
        }
        let bytes = read_body_limited(response, MAX_RETAIN_BODY_BYTES, WHAT).await?;
        let event: EventRecord = serde_json::from_slice(&bytes).context("decoding the event")?;
        validate_event(&event)?;
        anyhow::ensure!(event.id == id, "retain response changed the event id");
        Ok(event)
    }

    /// Fetches the keyframe resource at or before `t`: an init segment plus
    /// one single-sample fragment, or `None` when nothing is recorded there.
    pub async fn keyframe(
        &self,
        camera: &CameraId,
        kind: StreamKind,
        t: WallUs,
    ) -> anyhow::Result<Option<Vec<u8>>> {
        const WHAT: &str = "requesting a keyframe";
        let mut url = self.url(&format!("/api/streams/{camera}/{kind}/keyframe"))?;
        url.query_pairs_mut().append_pair("t", &t.get().to_string());
        let response = self.get(url).send().await.context(WHAT)?;
        if response.status() == StatusCode::NOT_FOUND {
            return Ok(None);
        }
        if !response.status().is_success() {
            return Err(refusal(WHAT, response).await);
        }
        require_content_type(&response, "video/mp4")?;
        let body = read_body_limited(response, MAX_KEYFRAME_BYTES, WHAT).await?;
        anyhow::ensure!(!body.is_empty(), "keyframe response is empty");
        Ok(Some(body))
    }

    /// The HLS VOD playlist URL for `[from, to)`. [`Client::vod_playlist`]
    /// fetches it with the bearer header; the returned body carries scoped
    /// media capabilities for the player to follow.
    pub fn vod_url(
        &self,
        camera: &CameraId,
        kind: StreamKind,
        from: WallUs,
        to: WallUs,
    ) -> anyhow::Result<Url> {
        let mut url = self.url(&format!("/api/streams/{camera}/{kind}/hls/vod.m3u8"))?;
        url.query_pairs_mut()
            .append_pair("from", &from.get().to_string())
            .append_pair("to", &to.get().to_string());
        Ok(url)
    }

    /// Fetches one VOD body for both timing and playback, including boundary GOPs
    /// and omitting unavailable files. Init and media URIs resolve to absolute
    /// URLs on the API origin and keep their authentication queries, so they
    /// still reach the daemon when the body is served locally. A reference to
    /// another origin or scheme, userinfo, or a tag the daemon does not emit
    /// rejects the playlist.
    pub async fn vod_playlist(
        &self,
        camera: &CameraId,
        kind: StreamKind,
        from: WallUs,
        to: WallUs,
    ) -> anyhow::Result<VodPlaylist> {
        const WHAT: &str = "fetching the VOD playlist";
        validate_range(from, to)?;
        let response = self
            .get(self.vod_url(camera, kind, from, to)?)
            .send()
            .await
            .context(WHAT)?;
        if !response.status().is_success() {
            return Err(refusal(WHAT, response).await);
        }
        let origin = self.base.origin();
        anyhow::ensure!(
            response.url().origin() == origin,
            "VOD playlist response came from another origin"
        );
        let base = response.url().clone();
        let bytes = read_body_limited(response, MAX_VOD_PLAYLIST_BYTES, WHAT).await?;
        let text = std::str::from_utf8(&bytes).context("VOD playlist is not UTF-8")?;
        let fragments = vod::playlist_fragments(text)?;
        validate_playback_fragments(&fragments, from, to)?;
        Ok(VodPlaylist {
            fragments,
            body: vod::absolute_playlist(text, &base, &origin)?,
        })
    }

    /// Lists the cameras a WS-Discovery probe finds within `wait`, through
    /// `GET /api/onvif/devices`.
    pub async fn discover(&self, wait: Duration) -> anyhow::Result<Vec<DiscoveredDevice>> {
        let mut url = self.url("/api/onvif/devices")?;
        url.query_pairs_mut()
            .append_pair("wait_ms", &wait.as_millis().to_string());
        let devices: Vec<DiscoveredDevice> = self
            .fetch(url, "discovering cameras", MAX_DEVICES_BODY_BYTES)
            .await?;
        validate_devices(&devices)?;
        Ok(devices)
    }

    /// Resolves the RTSP URLs of one camera's ONVIF media profiles, through
    /// `POST /api/onvif/streams`; the answer may take up to the daemon's lookup
    /// bound, so this request alone waits [`STREAM_LOOKUP_READ_TIMEOUT`].
    pub async fn stream_urls(
        &self,
        request: &StreamUrlsRequest,
    ) -> anyhow::Result<Vec<ProfileStream>> {
        const WHAT: &str = "asking the camera for its streams";
        let body = serde_json::to_vec(request).context("encoding the lookup")?;
        let response = self
            .authorized(self.lookup_http.post(self.url("/api/onvif/streams")?))
            .header("content-type", "application/json")
            .body(body)
            .send()
            .await
            .context(WHAT)?;
        if !response.status().is_success() {
            return Err(refusal(WHAT, response).await);
        }
        let bytes = read_body_limited(response, MAX_STREAMS_BODY_BYTES, WHAT).await?;
        let streams: Vec<ProfileStream> =
            serde_json::from_slice(&bytes).context("decoding the streams")?;
        validate_streams(&streams)?;
        Ok(streams)
    }
}

/// The error for a refused request: the status and the daemon's message.
async fn refusal(what: &str, response: Response) -> anyhow::Error {
    let status = response.status();
    let body = read_body_limited(response, MAX_ERROR_BODY_BYTES, what)
        .await
        .ok();
    anyhow::anyhow!("{}", failure_text(what, status, body.as_deref()))
}

/// The daemon's refusal as the status bar shows it: the status and the
/// `error` field of its JSON body, without control characters and bounded.
fn failure_text(what: &str, status: StatusCode, body: Option<&[u8]>) -> String {
    let detail = body
        .and_then(|body| serde_json::from_slice::<serde_json::Value>(body).ok())
        .and_then(|json| {
            json.get("error")
                .and_then(|e| e.as_str())
                .map(str::to_owned)
        })
        .map(|message| {
            message
                .chars()
                .map(|c| if c.is_control() { ' ' } else { c })
                .take(MAX_ERROR_MESSAGE_CHARS)
                .collect::<String>()
        })
        .filter(|message| !message.trim().is_empty());
    match detail {
        Some(detail) => format!("{what} failed with {status}: {detail}"),
        None => format!("{what} failed with {status}"),
    }
}

/// The body of `response`, refused beyond `maximum` bytes before it is held;
/// `what` names the request in errors.
async fn read_body_limited(
    mut response: Response,
    maximum: usize,
    what: &str,
) -> anyhow::Result<Vec<u8>> {
    if let Some(length) = response.content_length() {
        anyhow::ensure!(
            length <= maximum as u64,
            "the answer to {what} exceeds its byte limit"
        );
    }
    let capacity = response
        .content_length()
        .and_then(|length| usize::try_from(length).ok())
        .unwrap_or(0);
    let mut body = Vec::new();
    body.try_reserve_exact(capacity)
        .with_context(|| format!("reserving the answer to {what}"))?;
    while let Some(chunk) = response
        .chunk()
        .await
        .with_context(|| format!("reading the answer to {what}"))?
    {
        anyhow::ensure!(
            chunk.len() <= maximum.saturating_sub(body.len()),
            "the answer to {what} exceeds its byte limit"
        );
        body.try_reserve(chunk.len())
            .with_context(|| format!("reserving the answer to {what}"))?;
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

/// Text for a label: nonblank, at most [`MAX_DISPLAY_STRING_BYTES`], no
/// control characters.
fn valid_display_string(text: &str) -> bool {
    !text.trim().is_empty()
        && text.len() <= MAX_DISPLAY_STRING_BYTES
        && !text.chars().any(char::is_control)
}

/// Text the daemon cleaned from a camera's answer (`iris_events::text`): at
/// most `max_chars` characters and no control characters; it may be empty.
fn valid_camera_text(text: &str, max_chars: usize) -> bool {
    text.chars().count() <= max_chars && !text.chars().any(char::is_control)
}

/// A device service host as the daemon lists it: a name or a bare address of at
/// most [`MAX_HOST_BYTES`], without whitespace or control characters.
fn valid_host(host: &str) -> bool {
    (1..=MAX_HOST_BYTES).contains(&host.len())
        && !host.chars().any(|c| c.is_whitespace() || c.is_control())
}

/// What is wrong with `url` as a camera stream URL, or `None` for a usable one:
/// `rtsp://` with a host, without userinfo or a fragment, and at most
/// [`MAX_STREAM_URL_BYTES`]. The daemon applies the same rule to the URLs a
/// camera advertises and refuses credentials in configured URLs.
pub fn rtsp_url_problem(url: &str) -> Option<&'static str> {
    if url.len() > MAX_STREAM_URL_BYTES {
        return Some("is longer than 2048 bytes");
    }
    let Ok(parsed) = Url::parse(url) else {
        return Some("is not a URL");
    };
    if parsed.scheme() != "rtsp" {
        return Some("must start with rtsp://");
    }
    if parsed.host_str().is_none_or(str::is_empty) {
        return Some("names no host");
    }
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Some("cannot carry credentials; enter them as the username and password");
    }
    if parsed.fragment().is_some() {
        return Some("cannot carry a fragment");
    }
    None
}

fn validate_cameras(cameras: &[Camera]) -> anyhow::Result<()> {
    anyhow::ensure!(cameras.len() <= MAX_CAMERAS, "too many cameras in response");
    let mut ids = HashSet::with_capacity(cameras.len());
    for camera in cameras {
        anyhow::ensure!(
            valid_display_string(&camera.name),
            "invalid camera name in response"
        );
        anyhow::ensure!(ids.insert(&camera.id), "duplicate camera id in response");
    }
    Ok(())
}

fn validate_devices(devices: &[DiscoveredDevice]) -> anyhow::Result<()> {
    anyhow::ensure!(
        devices.len() <= MAX_DISCOVERED_DEVICES,
        "the daemon listed more devices than one probe reports"
    );
    for device in devices {
        anyhow::ensure!(
            valid_host(&device.host),
            "the daemon listed a device with an invalid host"
        );
        anyhow::ensure!(
            valid_camera_text(&device.endpoint, MAX_ENDPOINT_CHARS),
            "the daemon listed a device with an invalid endpoint reference"
        );
        anyhow::ensure!(
            [&device.name, &device.hardware, &device.location]
                .into_iter()
                .flatten()
                .all(|scope| !scope.is_empty() && valid_camera_text(scope, MAX_LABEL_CHARS)),
            "the daemon listed a device with an invalid scope"
        );
    }
    Ok(())
}

fn validate_streams(streams: &[ProfileStream]) -> anyhow::Result<()> {
    anyhow::ensure!(
        !streams.is_empty() && streams.len() <= MAX_PROFILE_STREAMS,
        "the daemon listed no usable stream"
    );
    for stream in streams {
        anyhow::ensure!(
            valid_camera_text(&stream.token, MAX_LABEL_CHARS)
                && valid_camera_text(&stream.name, MAX_LABEL_CHARS),
            "the daemon listed a profile with an invalid token or name"
        );
        if let Some(problem) = rtsp_url_problem(&stream.url) {
            anyhow::bail!("the daemon listed a stream URL that {problem}");
        }
    }
    Ok(())
}

/// A recorded segment as `/api/streams/{camera}/{kind}/segments` lists it;
/// only the fields the timeline draws.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
pub struct Segment {
    /// Wall-clock start.
    #[serde(rename = "wall_start_us")]
    pub start: WallUs,
    /// Wall-clock end; for an open file, its last durable progress.
    #[serde(rename = "wall_end_us")]
    pub end: WallUs,
    /// Whether the file is sealed (false for the one being written).
    pub sealed: bool,
}

/// One fetched VOD selection, with timing and media references from the same
/// body. Its `Debug` output leaves out the body, whose references carry media
/// capabilities.
pub struct VodPlaylist {
    /// Complete fragments in the order this playlist presents them.
    pub fragments: Vec<PlaybackFragment>,
    /// The fetched playlist with media and init URIs resolved against its URL,
    /// every one on the API origin.
    pub body: String,
}

impl fmt::Debug for VodPlaylist {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("VodPlaylist")
            .field("fragments", &self.fragments.len())
            .field("body_bytes", &self.body.len())
            .finish()
    }
}

fn validate_playback_fragments(
    fragments: &[PlaybackFragment],
    from: WallUs,
    to: WallUs,
) -> anyhow::Result<()> {
    validate_range(from, to)?;
    let mut previous = None;
    let mut overlaps_window = false;
    for fragment in fragments {
        if let Some(previous) = previous {
            anyhow::ensure!(
                fragment.wall_start >= previous,
                "VOD fragment times are not ordered"
            );
        }
        let start = i128::from(fragment.wall_start.get());
        let end = start + i128::from(fragment.duration.as_micros());
        overlaps_window |= end > i128::from(from.get()) && start < i128::from(to.get());
        previous = Some(fragment.wall_start);
    }
    anyhow::ensure!(
        overlaps_window,
        "VOD playlist has no fragment in the requested window"
    );
    Ok(())
}

fn validate_range(from: WallUs, to: WallUs) -> anyhow::Result<()> {
    let span = i128::from(to.get()) - i128::from(from.get());
    anyhow::ensure!(span > 0, "invalid time range");
    anyhow::ensure!(span <= MAX_RANGE_US, "time range exceeds seven days");
    Ok(())
}

/// Checks a segment listing against the API's overlap rule. A sealed row must
/// span a positive interval that overlaps `[from, to)`. The API treats an open
/// row as extending to the present, and its end is its last durable progress,
/// so an open row needs only `start <= end` and `start < to`: its recorded end
/// can precede `from` (a camera offline for longer than the window) or equal its
/// start (a file opened after a rotation or reconnect, before its first sync).
fn validate_segments(segments: &[Segment], from: WallUs, to: WallUs) -> anyhow::Result<()> {
    validate_range(from, to)?;
    anyhow::ensure!(
        segments.len() <= MAX_SEGMENTS,
        "too many segments in response"
    );
    for segment in segments {
        if segment.sealed {
            anyhow::ensure!(
                segment.start < segment.end,
                "invalid segment time range in response"
            );
            anyhow::ensure!(
                segment.end > from && segment.start < to,
                "segment is outside the requested window"
            );
        } else {
            anyhow::ensure!(
                segment.start <= segment.end,
                "invalid open segment time range in response"
            );
            anyhow::ensure!(
                segment.start < to,
                "segment is outside the requested window"
            );
        }
    }
    Ok(())
}

/// Drops open rows that have recorded nothing inside `[from, to)`, so a window
/// holding only such a row reads as having nothing recorded. Rows that pass
/// [`validate_segments`] and remain cover part of the window.
fn recorded_in_window(mut segments: Vec<Segment>, from: WallUs) -> Vec<Segment> {
    segments
        .retain(|segment| segment.sealed || (segment.end > from && segment.start < segment.end));
    segments
}

/// An event as `/api/events` lists it; only the fields the timeline marks.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub struct EventRecord {
    /// The catalog id.
    pub id: EventId,
    /// The event kind.
    pub kind: EventKind,
    /// Recorder time of the event.
    pub nvr_time: WallUs,
    /// Until when the footage around the event is kept, when the event is retained.
    #[serde(rename = "retain_until_us")]
    pub retain_until: Option<WallUs>,
}

/// An event kind as the API serialises `iris_core::event::EventKind`: a
/// bare string for the kinds the recorder names (`motion`, `line_crossing`,
/// ...) and `{"other": "<vendor code>"}` for the rest.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(untagged)]
pub enum EventKind {
    /// A kind the recorder names.
    Named(String),
    /// A vendor-specific code passed through.
    Other {
        /// The vendor's code.
        other: String,
    },
}

impl EventKind {
    /// The kind as a label.
    pub fn label(&self) -> &str {
        match self {
            Self::Named(name) | Self::Other { other: name } => name,
        }
    }
}

fn validate_event(event: &EventRecord) -> anyhow::Result<()> {
    anyhow::ensure!(event.id.0 > 0, "invalid event id in response");
    anyhow::ensure!(
        valid_display_string(event.kind.label()),
        "invalid event kind in response"
    );
    Ok(())
}

fn validate_events(
    events: &[EventRecord],
    from: WallUs,
    to: WallUs,
    limit: usize,
) -> anyhow::Result<()> {
    validate_range(from, to)?;
    anyhow::ensure!(
        (1..=MAX_EVENTS).contains(&limit),
        "invalid event page limit"
    );
    anyhow::ensure!(events.len() <= limit, "too many events in response");
    let mut ids = HashSet::with_capacity(events.len());
    for event in events {
        validate_event(event)?;
        anyhow::ensure!(
            event.nvr_time >= from && event.nvr_time < to,
            "event is outside the requested window"
        );
        anyhow::ensure!(ids.insert(event.id), "duplicate event id in response");
    }
    Ok(())
}

fn require_content_type(response: &Response, expected: &str) -> anyhow::Result<()> {
    let media_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(';').next())
        .map(str::trim);
    anyhow::ensure!(
        media_type.is_some_and(|media_type| media_type.eq_ignore_ascii_case(expected)),
        "response is not {expected}"
    );
    Ok(())
}

/// An export's exclusive sibling temporary, removed when dropped before
/// [`ExportTemp::commit`].
struct ExportTemp {
    path: Option<PathBuf>,
    file: Option<tokio::fs::File>,
}

impl ExportTemp {
    async fn create(destination: &Path) -> anyhow::Result<Self> {
        anyhow::ensure!(
            destination.file_name().is_some(),
            "export destination has no file name"
        );
        let parent = destination
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        for _ in 0..EXPORT_TEMP_ATTEMPTS {
            let mut random = [0u8; 16];
            getrandom::fill(&mut random).context("creating an export temporary name")?;
            let mut name = String::with_capacity(47);
            name.push_str(".irisd-export-");
            for byte in random {
                name.push(char::from(HEX_DIGITS[usize::from(byte >> 4)]));
                name.push(char::from(HEX_DIGITS[usize::from(byte & 0x0f)]));
            }
            let path = parent.join(name).with_extension("tmp");
            match tokio::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
                .await
            {
                Ok(file) => {
                    return Ok(Self {
                        path: Some(path),
                        file: Some(file),
                    });
                }
                Err(error) if error.kind() == ErrorKind::AlreadyExists => {}
                Err(error) => {
                    return Err(error).with_context(|| {
                        format!("creating export beside {}", destination.display())
                    });
                }
            }
        }
        anyhow::bail!("cannot allocate a unique export temporary file")
    }

    fn file(&mut self) -> anyhow::Result<&mut tokio::fs::File> {
        self.file
            .as_mut()
            .context("export temporary file is closed")
    }

    async fn commit(mut self, destination: &Path) -> anyhow::Result<()> {
        use tokio::io::AsyncWriteExt as _;

        let mut file = self
            .file
            .take()
            .context("export temporary file is closed")?;
        file.flush().await.context("flushing the export")?;
        file.sync_all().await.context("syncing the export")?;
        drop(file);
        let path = self
            .path
            .as_ref()
            .context("export temporary path is missing")?;
        tokio::fs::rename(path, destination)
            .await
            .with_context(|| format!("committing export to {}", destination.display()))?;
        self.path.take();
        Ok(())
    }
}

impl Drop for ExportTemp {
    fn drop(&mut self) {
        if let Some(path) = self.path.take()
            && let Err(error) = std::fs::remove_file(&path)
            && error.kind() != ErrorKind::NotFound
        {
            warn!(path = %path.display(), %error, "cannot remove an export temporary");
        }
    }
}

/// A camera to add through `POST /api/cameras`: the fields of a `[[camera]]`
/// table the dialog collects. Its `Debug` output redacts the password.
#[derive(Clone, Serialize, PartialEq, Eq)]
pub struct NewCamera {
    /// Stable identifier.
    pub id: CameraId,
    /// Display name.
    pub name: String,
    /// Main stream URL without credentials.
    pub rtsp_url: String,
    /// Sub stream URL; the daemon derives one from a Dahua-style main URL when absent.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sub_url: Option<String>,
    /// RTSP and HTTP user.
    pub username: String,
    /// RTSP and HTTP password.
    pub password: String,
    /// Recorded kinds, `main` and `sub`.
    pub record: Vec<String>,
    /// Event sources, `dahua_cgi` and `onvif_pull_point`.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub events: Vec<String>,
}

impl fmt::Debug for NewCamera {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("NewCamera")
            .field("id", &self.id)
            .field("name", &self.name)
            .field("rtsp_url", &self.rtsp_url)
            .field("sub_url", &self.sub_url)
            .field("username", &self.username)
            .field("password", &"<redacted>")
            .field("record", &self.record)
            .field("events", &self.events)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::io;
    use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    use axum::Router;
    use axum::body::{Body, Bytes};
    use axum::extract::Query;
    use axum::http::{HeaderMap, StatusCode, header};
    use axum::response::Response;
    use axum::routing;
    use futures::StreamExt as _;
    use futures::stream;

    use super::*;

    fn camera(id: &str) -> CameraId {
        CameraId::try_new(id).unwrap()
    }

    fn time(micros: i64) -> WallUs {
        WallUs::new(micros)
    }

    async fn serve_fixture(app: Router) -> (Url, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        (Url::parse(&format!("http://{address}")).unwrap(), server)
    }

    fn mp4_response(body: Body) -> Response {
        Response::builder()
            .header(header::CONTENT_TYPE, "video/mp4")
            .body(body)
            .unwrap()
    }

    fn delayed_export_body(byte: u8) -> Body {
        Body::from_stream(stream::unfold(0usize, move |index| {
            let byte = byte;
            async move {
                if index == 8 {
                    return None;
                }
                tokio::time::sleep(Duration::from_millis(2)).await;
                Some((
                    Ok::<Bytes, io::Error>(Bytes::from(vec![byte; 1024])),
                    index + 1,
                ))
            }
        }))
    }

    /// The export fixture; the `from` parameter picks the answer.
    async fn export_fixture(Query(query): Query<HashMap<String, String>>) -> Response {
        let from = query["from"].parse::<i64>().unwrap();
        match from {
            1 => mp4_response(Body::from_stream(stream::iter([
                Ok(Bytes::from_static(b"partial")),
                Err(io::Error::other("fixture body failure")),
            ]))),
            2 => mp4_response(Body::empty()),
            3 => Response::builder()
                .header(header::CONTENT_TYPE, "text/plain")
                .body(Body::from("not an mp4"))
                .unwrap(),
            4 => mp4_response(Body::from("complete mp4")),
            5 => Response::builder()
                .status(StatusCode::PARTIAL_CONTENT)
                .header(header::CONTENT_TYPE, "video/mp4")
                .header(header::CONTENT_RANGE, "bytes 0-6/12")
                .body(Body::from("partial"))
                .unwrap(),
            10 => mp4_response(delayed_export_body(b'A')),
            20 => mp4_response(delayed_export_body(b'B')),
            30 => {
                let body =
                    stream::once(async { Ok::<Bytes, io::Error>(Bytes::from_static(b"partial")) })
                        .chain(stream::pending::<Result<Bytes, io::Error>>());
                mp4_response(Body::from_stream(body))
            }
            _ => unreachable!(),
        }
    }

    async fn export_server() -> (Url, tokio::task::JoinHandle<()>) {
        serve_fixture(Router::new().route(
            "/api/streams/{camera}/{kind}/export.mp4",
            routing::get(export_fixture),
        ))
        .await
    }

    /// Exports the fixture answer that `from` picks to `destination`.
    async fn export_case(client: &Client, from: i64, destination: &Path) -> anyhow::Result<u64> {
        client
            .export(
                &camera("cam1"),
                StreamKind::Main,
                time(from),
                time(from + 1),
                destination,
                |_| {},
            )
            .await
    }

    fn export_temporaries(directory: &Path) -> Vec<PathBuf> {
        std::fs::read_dir(directory)
            .unwrap()
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with(".irisd-export-"))
            })
            .collect()
    }

    #[test]
    fn failure_text_names_the_status_and_the_bounded_server_message() {
        let status = StatusCode::CONFLICT;
        assert_eq!(
            failure_text(
                "adding the camera",
                status,
                Some(br#"{"error":"conflict: camera cam8 already exists"}"#)
            ),
            "adding the camera failed with 409 Conflict: conflict: camera cam8 already exists"
        );
        assert_eq!(
            failure_text("adding the camera", status, Some(b"<html>not json</html>")),
            "adding the camera failed with 409 Conflict"
        );
        assert_eq!(
            failure_text("removing the camera", StatusCode::FORBIDDEN, None),
            "removing the camera failed with 403 Forbidden"
        );
        let hostile = format!(r#"{{"error":"line\nbreak\u001b[31m{}"}}"#, "x".repeat(500));
        let shown = failure_text("adding the camera", status, Some(hostile.as_bytes()));
        assert!(
            !shown.contains('\n') && !shown.contains('\u{1b}'),
            "{shown}"
        );
        assert!(shown.len() < 260, "{}", shown.len());
    }

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

    const JSON: [(&str, &str); 1] = [("content-type", "application/json")];

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
                    move |axum::extract::Path(camera): axum::extract::Path<String>,
                          headers: HeaderMap| {
                        let seen = Arc::clone(&seen);
                        async move { remove_route(&seen, &camera, &headers) }
                    }
                }),
            )
            .route(
                "/api/onvif/devices",
                routing::get({
                    let seen = Arc::clone(seen);
                    move |headers: HeaderMap,
                          axum::extract::RawQuery(query): axum::extract::RawQuery| {
                        let seen = Arc::clone(&seen);
                        async move {
                            let path =
                                format!("/api/onvif/devices?{}", query.unwrap_or_default());
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
    ) -> (StatusCode, [(&'static str, &'static str); 1], String) {
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
    ) -> (StatusCode, [(&'static str, &'static str); 1], String) {
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
    ) -> (StatusCode, [(&'static str, &'static str); 1], String) {
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
            id: camera(id),
            name: "Garage".into(),
            rtsp_url: "rtsp://192.0.2.10/x".into(),
            sub_url: None,
            username: "viewer".into(),
            password: "secret".into(),
            record: vec!["main".into()],
            events: Vec::new(),
        }
    }

    async fn provisioning_client() -> (Client, Seen, tokio::task::JoinHandle<()>) {
        let seen: Seen = Arc::new(Mutex::new(Vec::new()));
        let (base, server) = serve_fixture(provisioning_fixture(&seen)).await;
        (Client::for_tests(base, Some("t0ken".into())), seen, server)
    }

    #[tokio::test]
    async fn discovery_sends_the_bearer_and_the_wait_and_decodes_the_devices() {
        let (client, seen, server) = provisioning_client().await;
        let devices = client.discover(Duration::from_secs(3)).await.unwrap();
        assert_eq!(
            devices
                .iter()
                .map(|device| (device.host.as_str(), device.port, device.name.as_deref()))
                .collect::<Vec<_>>(),
            [("192.0.2.20", 80, Some("Garage"))]
        );
        let seen = seen.lock().unwrap().clone();
        assert_eq!(
            (
                seen[0].method.as_str(),
                seen[0].path.as_str(),
                seen[0].token.as_deref()
            ),
            (
                "GET",
                "/api/onvif/devices?wait_ms=3000",
                Some("Bearer t0ken")
            )
        );
        server.abort();
    }

    #[tokio::test]
    async fn a_stream_lookup_posts_the_request_with_the_bearer() {
        let (client, seen, server) = provisioning_client().await;
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
        let seen = seen.lock().unwrap().clone();
        assert_eq!(seen[0].token.as_deref(), Some("Bearer t0ken"));
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
    fn a_stream_lookup_request_redacts_its_password() {
        let request = lookup("192.0.2.20");
        assert!(!format!("{request:?}").contains("secret"));
    }

    #[test]
    fn the_client_redacts_its_token() {
        let client = Client::for_tests(
            Url::parse("http://192.0.2.10:8554").unwrap(),
            Some("t0ken".into()),
        );
        let shown = format!("{client:?}");
        assert!(
            shown.contains("<redacted>") && !shown.contains("t0ken"),
            "{shown}"
        );
    }

    #[tokio::test]
    async fn a_stream_lookup_outlasts_the_read_idle_deadline() {
        let slow = || async {
            tokio::time::sleep(Duration::from_millis(200)).await;
            (
                StatusCode::OK,
                JSON,
                r#"[{"token":"p","name":"Main","url":"rtsp://192.0.2.20/1"}]"#.to_owned(),
            )
        };
        let (base, server) = serve_fixture(
            Router::new()
                .route("/api/onvif/streams", routing::post(slow))
                .route("/api/cameras", routing::get(slow)),
        )
        .await;
        let client = Client::with_timeouts(
            base,
            None,
            &iris_tls::client_config(None).unwrap(),
            Duration::from_secs(1),
            Duration::from_millis(50),
        );
        assert!(
            client.cameras().await.is_err(),
            "an ordinary request ends at the read idle deadline"
        );
        assert_eq!(
            client
                .stream_urls(&lookup("192.0.2.20"))
                .await
                .unwrap()
                .len(),
            1
        );
        server.abort();
    }

    #[tokio::test]
    async fn add_camera_posts_the_camera_with_the_bearer() {
        let (client, seen, server) = provisioning_client().await;
        let added = client.add_camera(&new_camera("cam8")).await.unwrap();
        assert_eq!((added.id.as_str(), added.name.as_str()), ("cam8", "Garage"));
        assert!(added.provisioned);
        let seen = seen.lock().unwrap().clone();
        assert_eq!(seen[0].token.as_deref(), Some("Bearer t0ken"));
        let posted: serde_json::Value = serde_json::from_str(&seen[0].body).unwrap();
        assert_eq!(posted["id"], "cam8");
        assert_eq!(posted["password"], "secret");
        assert_eq!(posted["record"], serde_json::json!(["main"]));
        assert!(posted.get("sub_url").is_none() && posted.get("events").is_none());
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

    #[tokio::test]
    async fn remove_camera_deletes_with_the_bearer() {
        let (client, seen, server) = provisioning_client().await;
        client.remove_camera(&camera("cam8")).await.unwrap();
        let seen = seen.lock().unwrap().clone();
        assert_eq!(
            (
                seen[0].method.as_str(),
                seen[0].path.as_str(),
                seen[0].token.as_deref()
            ),
            ("DELETE", "/api/cameras/cam8", Some("Bearer t0ken"))
        );
        server.abort();
    }

    #[tokio::test]
    async fn remove_camera_reports_the_daemon_conflict() {
        let (client, _seen, server) = provisioning_client().await;
        let error = client
            .remove_camera(&camera("static"))
            .await
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("409") && error.contains("configuration file"),
            "{error}"
        );
        server.abort();
    }

    fn listed(id: &str, name: &str) -> Camera {
        Camera {
            id: camera(id),
            name: name.into(),
            provisioned: false,
        }
    }

    #[test]
    fn a_camera_list_with_valid_names_and_distinct_ids_is_accepted() {
        assert!(validate_cameras(&[listed("cam1", "Front"), listed("cam2", "Back")]).is_ok());
    }

    #[test]
    fn duplicate_camera_ids_are_refused() {
        assert!(validate_cameras(&[listed("cam1", "Front"), listed("cam1", "Duplicate")]).is_err());
    }

    #[test]
    fn camera_ids_outside_the_wire_contract_fail_to_decode() {
        for id in ["../cam", "Cam1", "", "a/b"] {
            let body = format!(r#"[{{"id":"{id}","name":"Front","provisioned":false}}]"#);
            assert!(serde_json::from_str::<Vec<Camera>>(&body).is_err(), "{id}");
        }
    }

    #[test]
    fn camera_names_with_control_characters_are_refused() {
        assert!(validate_cameras(&[listed("cam2", "\n")]).is_err());
    }

    #[test]
    fn more_than_max_cameras_are_refused() {
        assert!(validate_cameras(&vec![listed("cam1", "Front"); MAX_CAMERAS + 1]).is_err());
    }

    #[test]
    fn ranges_of_more_than_seven_days_are_refused() {
        let segment = Segment {
            start: time(0),
            end: time(10),
            sealed: true,
        };
        assert!(validate_segments(&[segment], time(0), time(10)).is_ok());
        assert!(validate_segments(&[segment], time(i64::MIN), time(i64::MAX)).is_err());
    }

    #[test]
    fn sealed_segments_outside_the_window_are_refused() {
        let outside = Segment {
            start: time(20),
            end: time(30),
            sealed: true,
        };
        assert!(validate_segments(&[outside], time(0), time(10)).is_err());
    }

    fn event(id: i64, nvr_time: i64) -> EventRecord {
        EventRecord {
            id: EventId(id),
            kind: EventKind::Named("motion".into()),
            nvr_time: time(nvr_time),
            retain_until: None,
        }
    }

    #[test]
    fn events_inside_the_window_are_accepted() {
        assert!(validate_events(&[event(1, 5)], time(0), time(10), 1).is_ok());
    }

    #[test]
    fn events_outside_the_window_are_refused() {
        assert!(validate_events(&[event(1, i64::MAX)], time(0), time(10), 1).is_err());
    }

    #[test]
    fn events_without_a_positive_id_are_refused() {
        assert!(validate_events(&[event(0, 5)], time(0), time(10), 1).is_err());
    }

    fn fragment(wall_start: i64) -> PlaybackFragment {
        PlaybackFragment {
            wall_start: time(wall_start),
            duration: iris_core::time::MediaTicks::new(900),
        }
    }

    #[test]
    fn ordered_playback_fragments_in_the_window_are_accepted() {
        assert!(
            validate_playback_fragments(&[fragment(-10), fragment(10)], time(0), time(20)).is_ok()
        );
    }

    #[test]
    fn unordered_playback_fragments_are_refused() {
        assert!(
            validate_playback_fragments(&[fragment(10), fragment(-10)], time(0), time(20)).is_err()
        );
    }

    #[test]
    fn playlists_without_a_fragment_in_the_window_are_refused() {
        assert!(
            validate_playback_fragments(&[fragment(-10), fragment(10)], time(20_000), time(30_000))
                .is_err()
        );
    }

    fn row(start: i64, end: i64, sealed: bool) -> Segment {
        Segment {
            start: time(start),
            end: time(end),
            sealed,
        }
    }

    #[test]
    fn open_rows_follow_the_api_overlap_rule() {
        let (from, to) = (time(100), time(200));
        // An open file of a camera offline since before the window, a file
        // opened after a rotation before its first sync, and a recording file.
        let accepted = [
            row(10, 50, false),
            row(150, 150, false),
            row(90, 150, false),
        ];
        assert!(validate_segments(&accepted, from, to).is_ok());
        for refused in [
            row(60, 50, false),
            row(200, 200, false),
            row(250, 260, false),
        ] {
            assert!(
                validate_segments(&[refused], from, to).is_err(),
                "{refused:?}"
            );
        }
    }

    #[test]
    fn sealed_rows_need_a_positive_span_overlapping_the_window() {
        for refused in [row(10, 50, true), row(150, 150, true), row(200, 260, true)] {
            assert!(
                validate_segments(&[refused], time(100), time(200)).is_err(),
                "{refused:?}"
            );
        }
    }

    #[test]
    fn open_rows_that_recorded_nothing_in_the_window_are_dropped() {
        let rows = vec![
            row(10, 50, false),
            row(150, 150, false),
            row(90, 150, false),
            row(100, 120, true),
        ];
        assert_eq!(
            recorded_in_window(rows, time(100)),
            [row(90, 150, false), row(100, 120, true)]
        );
    }

    #[tokio::test]
    async fn a_window_with_only_a_stale_open_row_lists_nothing_recorded() {
        // The API's rows for a one-hour window ending at TO: a file opened 3 h
        // before TO whose last durable progress was 2 h before it, and a file
        // opened before its first sync.
        const FROM: i64 = 1_794_076_005_736_367;
        const TO: i64 = 1_794_079_605_736_367;
        const STALE: &str = concat!(
            r#"[{"id":1,"camera":"cam1","kind":"main","run_seq":1,"discontinuity":true,"#,
            r#""wall_start_us":1794068805736367,"wall_end_us":1794072405736367,"#,
            r#""fragments":10,"bytes":4096,"sealed":false}]"#
        );
        const FRESH: &str = concat!(
            r#"[{"id":2,"camera":"cam1","kind":"main","run_seq":1,"discontinuity":true,"#,
            r#""wall_start_us":1794079604736367,"wall_end_us":1794079604736367,"#,
            r#""fragments":0,"bytes":768,"sealed":false}]"#
        );
        for body in [STALE, FRESH] {
            let (base, server) = serve_fixture(Router::new().route(
                "/api/streams/{camera}/{kind}/segments",
                routing::get(move || async move { body }),
            ))
            .await;
            let client = Client::for_tests(base, None);
            assert_eq!(
                client
                    .segments(&camera("cam1"), StreamKind::Main, time(FROM), time(TO))
                    .await
                    .unwrap(),
                [],
                "{body}"
            );
            server.abort();
        }
    }

    #[tokio::test]
    async fn the_viewer_does_not_follow_redirects() {
        let other_requests = Arc::new(AtomicUsize::new(0));
        let (other, other_server) = serve_fixture(Router::new().fallback({
            let other_requests = Arc::clone(&other_requests);
            move || async move {
                other_requests.fetch_add(1, Ordering::SeqCst);
                "#EXTM3U\n"
            }
        }))
        .await;
        let target = other.join("/vod.m3u8").unwrap().to_string();
        let (base, server) = serve_fixture(Router::new().route(
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
        let client = Client::for_tests(base, None);
        let error = client
            .vod_playlist(&camera("cam1"), StreamKind::Main, time(1), time(2))
            .await
            .expect_err("a redirected playlist is refused");
        assert!(format!("{error:#}").contains("302"), "{error:#}");
        assert_eq!(other_requests.load(Ordering::SeqCst), 0);
        server.abort();
        other_server.abort();
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
            serve_fixture(Router::new().route("/chunked", routing::get(chunked))).await;
        let client = Client::for_tests(base.clone(), None);
        let response = client
            .http
            .get(base.join("/chunked").unwrap())
            .send()
            .await
            .unwrap();
        assert!(read_body_limited(response, 3, "fixture").await.is_err());
        server.abort();
    }

    #[tokio::test]
    async fn a_stalled_body_ends_at_the_read_idle_deadline() {
        let stalled = || async {
            let body = stream::once(async { Ok::<Bytes, io::Error>(Bytes::from_static(b"[")) })
                .chain(stream::pending::<Result<Bytes, io::Error>>());
            Response::builder()
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from_stream(body))
                .unwrap()
        };
        let (base, server) =
            serve_fixture(Router::new().route("/api/cameras", routing::get(stalled))).await;
        let client = Client::with_timeouts(
            base,
            None,
            &iris_tls::client_config(None).unwrap(),
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

    #[tokio::test]
    async fn failed_exports_leave_the_destination_and_no_temporary() {
        let (base, server) = export_server().await;
        let client = Client::for_tests(base, None);
        let directory = tempfile::tempdir().unwrap();
        let destination = directory.path().join("recording.mp4");
        let sentinel = b"pre-existing recording";
        // A body failure, an empty body, and a body that is not MP4.
        for from in [1, 2, 3] {
            std::fs::write(&destination, sentinel).unwrap();
            assert!(
                export_case(&client, from, &destination).await.is_err(),
                "from {from}"
            );
            assert_eq!(
                std::fs::read(&destination).unwrap(),
                sentinel,
                "from {from}"
            );
            assert_eq!(
                export_temporaries(directory.path()),
                Vec::<PathBuf>::new(),
                "from {from}"
            );
        }
        server.abort();
    }

    #[tokio::test]
    async fn a_complete_export_replaces_the_destination() {
        let (base, server) = export_server().await;
        let client = Client::for_tests(base, None);
        let directory = tempfile::tempdir().unwrap();
        let destination = directory.path().join("recording.mp4");
        std::fs::write(&destination, b"pre-existing recording").unwrap();
        assert_eq!(export_case(&client, 4, &destination).await.unwrap(), 12);
        assert_eq!(std::fs::read(&destination).unwrap(), b"complete mp4");
        assert_eq!(export_temporaries(directory.path()), Vec::<PathBuf>::new());
        server.abort();
    }

    #[tokio::test]
    async fn an_export_replaces_a_symlink_instead_of_writing_through_it() {
        let (base, server) = export_server().await;
        let client = Client::for_tests(base, None);
        let directory = tempfile::tempdir().unwrap();
        let destination = directory.path().join("recording.mp4");
        let linked = directory.path().join("linked.mp4");
        let sentinel = b"pre-existing recording";
        std::fs::write(&linked, sentinel).unwrap();
        std::os::unix::fs::symlink(&linked, &destination).unwrap();
        export_case(&client, 4, &destination).await.unwrap();
        assert_eq!(std::fs::read(&linked).unwrap(), sentinel);
        assert_eq!(std::fs::read(&destination).unwrap(), b"complete mp4");
        assert!(
            std::fs::symlink_metadata(&destination)
                .unwrap()
                .file_type()
                .is_file()
        );
        server.abort();
    }

    #[tokio::test]
    async fn concurrent_exports_leave_one_complete_file() {
        let (base, server) = export_server().await;
        let client = Client::for_tests(base, None);
        let directory = tempfile::tempdir().unwrap();
        let destination = directory.path().join("recording.mp4");
        let (first, second) = tokio::join!(
            export_case(&client, 10, &destination),
            export_case(&client, 20, &destination)
        );
        assert!(first.is_ok() && second.is_ok());
        let result = std::fs::read(&destination).unwrap();
        assert!(result == vec![b'A'; 8 * 1024] || result == vec![b'B'; 8 * 1024]);
        assert_eq!(export_temporaries(directory.path()), Vec::<PathBuf>::new());
        server.abort();
    }

    #[tokio::test]
    async fn a_cancelled_export_leaves_the_destination_and_no_temporary() {
        let (base, server) = export_server().await;
        let client = Arc::new(Client::for_tests(base, None));
        let directory = tempfile::tempdir().unwrap();
        let destination = directory.path().join("recording.mp4");
        let sentinel = b"pre-existing recording";
        std::fs::write(&destination, sentinel).unwrap();
        let task = tokio::spawn({
            let client = Arc::clone(&client);
            let destination = destination.clone();
            async move { export_case(&client, 30, &destination).await }
        });
        tokio::time::timeout(Duration::from_secs(1), async {
            while export_temporaries(directory.path()).is_empty() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("export must create its private temporary");
        task.abort();
        let _ = task.await;
        assert_eq!(std::fs::read(&destination).unwrap(), sentinel);
        assert_eq!(export_temporaries(directory.path()), Vec::<PathBuf>::new());
        server.abort();
    }

    #[test]
    fn the_export_reserve_is_the_larger_of_one_gib_and_two_percent() {
        assert_eq!(export_reserve(10 << 30), 1 << 30);
        assert_eq!(export_reserve(1 << 40), (1 << 40) / 50);
    }

    #[test]
    fn free_space_below_the_reserve_stops_an_export() {
        let total = 100 << 30;
        assert!(
            check_export_space(FilesystemSpace {
                available: 2 << 30,
                total
            })
            .is_ok()
        );
        assert!(
            check_export_space(FilesystemSpace {
                available: (2 << 30) - 1,
                total
            })
            .is_err()
        );
    }

    #[test]
    fn fstatvfs_reports_the_export_filesystem() {
        let space = filesystem_space(&tempfile::tempfile().unwrap()).unwrap();
        assert!(
            space.total > 0 && space.available <= space.total,
            "{space:?}"
        );
    }

    /// Calls of the plenty-then-low probe in
    /// [`space_running_low_stops_an_export_at_the_next_check`].
    static SPACE_PROBES: AtomicUsize = AtomicUsize::new(0);

    /// 512 MiB free of 100 GiB: below the 2 GiB reserve.
    const LOW_SPACE: FilesystemSpace = FilesystemSpace {
        available: 512 << 20,
        total: 100 << 30,
    };

    /// 50 GiB free of 100 GiB.
    const PLENTY_OF_SPACE: FilesystemSpace = FilesystemSpace {
        available: 50 << 30,
        total: 100 << 30,
    };

    #[tokio::test]
    async fn too_little_space_refuses_an_export_before_its_first_byte() {
        let (base, server) = export_server().await;
        let directory = tempfile::tempdir().unwrap();
        let destination = directory.path().join("recording.mp4");
        let sentinel = b"pre-existing recording";
        std::fs::write(&destination, sentinel).unwrap();
        let mut client = Client::for_tests(base, None);
        client.space_probe = |_| Ok(LOW_SPACE);
        let written = Arc::new(AtomicU64::new(0));
        let observed = Arc::clone(&written);
        let error = client
            .export(
                &camera("cam1"),
                StreamKind::Main,
                time(10),
                time(11),
                &destination,
                move |bytes| observed.store(bytes, Ordering::SeqCst),
            )
            .await
            .expect_err("too little space refuses the export before writing");
        assert!(format!("{error:#}").contains("reserve"), "{error:#}");
        assert_eq!(written.load(Ordering::SeqCst), 0);
        assert_eq!(std::fs::read(&destination).unwrap(), sentinel);
        assert_eq!(export_temporaries(directory.path()), Vec::<PathBuf>::new());
        server.abort();
    }

    #[tokio::test]
    async fn space_running_low_stops_an_export_at_the_next_check() {
        let (base, server) = export_server().await;
        let directory = tempfile::tempdir().unwrap();
        let destination = directory.path().join("recording.mp4");
        let sentinel = b"pre-existing recording";
        std::fs::write(&destination, sentinel).unwrap();
        let mut client = Client::for_tests(base, None);
        client.space_probe = |_| {
            if SPACE_PROBES.fetch_add(1, Ordering::SeqCst) == 0 {
                Ok(PLENTY_OF_SPACE)
            } else {
                Ok(LOW_SPACE)
            }
        };
        client.space_check_bytes = 2048;
        let error = export_case(&client, 10, &destination)
            .await
            .expect_err("space running low stops the export at the next check");
        assert!(format!("{error:#}").contains("reserve"), "{error:#}");
        assert_eq!(SPACE_PROBES.load(Ordering::SeqCst), 2);
        assert_eq!(std::fs::read(&destination).unwrap(), sentinel);
        assert_eq!(export_temporaries(directory.path()), Vec::<PathBuf>::new());
        server.abort();
    }

    #[tokio::test]
    async fn export_rejects_partial_content_before_creating_a_temporary() {
        let (base, server) = export_server().await;
        let client = Client::for_tests(base, None);
        let directory = tempfile::tempdir().unwrap();
        let destination = directory.path().join("recording.mp4");
        let sentinel = b"pre-existing recording";
        std::fs::write(&destination, sentinel).unwrap();
        assert!(export_case(&client, 5, &destination).await.is_err());
        assert_eq!(std::fs::read(&destination).unwrap(), sentinel);
        assert_eq!(export_temporaries(directory.path()), Vec::<PathBuf>::new());
        server.abort();
    }

    #[tokio::test]
    async fn export_temporary_is_removed_after_local_write_failure() {
        use tokio::io::AsyncWriteExt as _;

        let directory = tempfile::tempdir().unwrap();
        let destination = directory.path().join("recording.mp4");
        let sentinel = b"pre-existing recording";
        std::fs::write(&destination, sentinel).unwrap();
        let mut temporary = ExportTemp::create(&destination).await.unwrap();
        let temporary_path = temporary.path.clone().unwrap();
        temporary.file = Some(
            tokio::fs::OpenOptions::new()
                .write(true)
                .open("/dev/full")
                .await
                .unwrap(),
        );
        let file = temporary.file().unwrap();
        let failed = match file.write_all(b"fails").await {
            Ok(()) => file.flush().await.is_err(),
            Err(_) => true,
        };
        assert!(failed, "/dev/full must fail the temporary write or flush");
        drop(temporary);
        assert!(!temporary_path.exists());
        assert_eq!(std::fs::read(&destination).unwrap(), sentinel);
    }

    #[test]
    fn event_kinds_decode_named_and_other_forms() {
        let events: Vec<EventRecord> = serde_json::from_str(concat!(
            r#"[{"id":1,"kind":"motion","nvr_time":1,"retain_until_us":null},"#,
            r#"{"id":2,"kind":{"other":"NewFile"},"nvr_time":2,"retain_until_us":5}]"#
        ))
        .unwrap();
        assert_eq!(events[0].kind.label(), "motion");
        assert_eq!(events[1].kind.label(), "NewFile");
        assert_eq!(events[1].nvr_time, time(2));
        assert_eq!(events[0].retain_until, None);
        assert_eq!(events[1].retain_until, Some(time(5)));
    }

    #[test]
    fn the_playlist_url_carries_the_range_but_not_the_root_bearer() {
        let client = Client::for_tests(
            Url::parse("http://192.0.2.10:8554").unwrap(),
            Some("t".into()),
        );
        assert_eq!(
            client
                .vod_url(&camera("cam1"), StreamKind::Sub, time(10), time(20))
                .unwrap()
                .as_str(),
            "http://192.0.2.10:8554/api/streams/cam1/sub/hls/vod.m3u8?from=10&to=20"
        );
    }

    fn device(host: &str) -> DiscoveredDevice {
        DiscoveredDevice {
            endpoint: "urn:uuid:1".into(),
            host: host.into(),
            port: 80,
            https: false,
            name: Some("Garage".into()),
            hardware: None,
            location: None,
        }
    }

    #[test]
    fn devices_the_daemon_could_list_are_accepted() {
        let unnamed = DiscoveredDevice {
            endpoint: String::new(),
            name: None,
            ..device("2001:db8::7")
        };
        assert!(validate_devices(&[device("192.0.2.5"), unnamed]).is_ok());
    }

    #[test]
    fn devices_with_an_unusable_host_are_refused() {
        for host in ["", " ", "a b", "x\n", &"h".repeat(MAX_HOST_BYTES + 1)] {
            assert!(validate_devices(&[device(host)]).is_err(), "{host:?}");
        }
    }

    #[test]
    fn devices_with_oversized_or_control_text_are_refused() {
        let long = "n".repeat(MAX_LABEL_CHARS + 1);
        for broken in [
            DiscoveredDevice {
                name: Some(long.clone()),
                ..device("192.0.2.5")
            },
            DiscoveredDevice {
                hardware: Some("IPC\u{1b}[31m".into()),
                ..device("192.0.2.5")
            },
            DiscoveredDevice {
                location: Some(String::new()),
                ..device("192.0.2.5")
            },
            DiscoveredDevice {
                endpoint: "e".repeat(MAX_ENDPOINT_CHARS + 1),
                ..device("192.0.2.5")
            },
        ] {
            assert!(
                validate_devices(std::slice::from_ref(&broken)).is_err(),
                "{broken:?}"
            );
        }
    }

    #[test]
    fn more_devices_than_one_probe_reports_are_refused() {
        assert!(validate_devices(&vec![device("192.0.2.5"); MAX_DISCOVERED_DEVICES + 1]).is_err());
    }

    fn profile(url: &str) -> ProfileStream {
        ProfileStream {
            token: "profile-1".into(),
            name: "Main".into(),
            url: url.into(),
        }
    }

    #[test]
    fn stream_urls_must_be_rtsp_with_a_host_and_without_credentials() {
        assert!(validate_streams(&[profile("rtsp://192.0.2.20:554/live/1")]).is_ok());
        for url in [
            "http://192.0.2.20/live",
            "rtsp:live",
            "rtsp://viewer:secret@192.0.2.20/live",
            "rtsp://viewer@192.0.2.20/live",
            "rtsp://192.0.2.20/live#x",
            &format!("rtsp://192.0.2.20/{}", "x".repeat(MAX_STREAM_URL_BYTES)),
        ] {
            assert!(validate_streams(&[profile(url)]).is_err(), "{url}");
        }
    }

    #[test]
    fn a_stream_list_must_hold_one_to_eight_profiles() {
        assert!(validate_streams(&[]).is_err());
        assert!(
            validate_streams(&vec![
                profile("rtsp://192.0.2.20/1");
                MAX_PROFILE_STREAMS + 1
            ])
            .is_err()
        );
    }

    #[test]
    fn profiles_with_control_characters_in_their_names_are_refused() {
        let broken = ProfileStream {
            name: "Main\n".into(),
            ..profile("rtsp://192.0.2.20/1")
        };
        assert!(validate_streams(&[broken]).is_err());
    }
}
