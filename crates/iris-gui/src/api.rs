//! The small part of the irisd HTTP API the viewer reads.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::Context;
use serde::Deserialize;
use url::Url;

/// Maximum time spent establishing an API connection.
const HTTP_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
/// Maximum time without body progress on an established API response.
const HTTP_READ_IDLE_TIMEOUT: Duration = Duration::from_secs(30);
/// Bounds the configured-camera response before it becomes GTK widgets and feeds.
const MAX_CAMERAS_BODY_BYTES: usize = 1 << 20;
/// More cameras than a viewer can usefully render are rejected before widget creation.
const MAX_CAMERAS: usize = 256;
/// Camera IDs follow `iris_core::id::CameraId`'s wire contract.
const MAX_CAMERA_ID_BYTES: usize = 32;
/// Human-readable network strings remain small enough for labels and diagnostics.
const MAX_DISPLAY_STRING_BYTES: usize = 1024;
/// Bounds segment/event JSON responses from the API.
const MAX_LIST_BODY_BYTES: usize = 16 << 20;
/// Matches the API's maximum segment rows per range response.
const MAX_SEGMENTS: usize = 4_096;
/// Matches the API's maximum event page.
const MAX_EVENTS: usize = 10_000;
/// Matches the API's maximum time-range request.
const MAX_RANGE_US: i128 = 7 * 86_400 * 1_000_000;
/// A keyframe can contain a 4 MiB init and one sample from a 66 MiB fragment,
/// plus bounded re-encoded boxes.
const MAX_KEYFRAME_BYTES: usize = 71 << 20;
/// A retain response contains one event rather than a page.
const MAX_RETAIN_BODY_BYTES: usize = 64 << 10;
/// Bounds playlist metadata received before playback starts.
const MAX_VOD_PLAYLIST_BYTES: usize = 64 << 20;
/// Matches the API's selected-fragment budget.
const MAX_PLAYBACK_FRAGMENTS: usize = 100_000;
/// Number of exclusive temporary-name attempts before reporting a hostile/full directory.
const EXPORT_TEMP_ATTEMPTS: usize = 16;
/// Lowercase hexadecimal digits for private temporary names.
const HEX_DIGITS: &[u8; 16] = b"0123456789abcdef";

/// One camera as `/api/cameras` lists it.
#[derive(Clone, Debug, Deserialize)]
pub struct Camera {
    /// The configured camera id.
    pub id: String,
    /// The configured display name.
    pub name: String,
}

/// An authenticated API client.
pub struct Client {
    base: Url,
    token: Option<String>,
    http: reqwest::Client,
}

impl Client {
    /// Creates a client for `base` with an optional bearer token.
    pub fn new(base: Url, token: Option<String>) -> Self {
        Self::with_timeouts(base, token, HTTP_CONNECT_TIMEOUT, HTTP_READ_IDLE_TIMEOUT)
    }

    fn with_timeouts(
        base: Url,
        token: Option<String>,
        connect_timeout: Duration,
        read_timeout: Duration,
    ) -> Self {
        let http = reqwest::Client::builder()
            .connect_timeout(connect_timeout)
            .read_timeout(read_timeout)
            .build()
            .expect("static HTTP client options are valid");
        Self { base, token, http }
    }

    /// Lists the cameras.
    pub async fn cameras(&self) -> anyhow::Result<Vec<Camera>> {
        let url = self
            .base
            .join("/api/cameras")
            .context("building the cameras URL")?;
        let mut request = self.http.get(url);
        if let Some(token) = &self.token {
            request = request.bearer_auth(token);
        }
        let response = request.send().await.context("requesting the cameras")?;
        let status = response.status();
        if !status.is_success() {
            anyhow::bail!("cameras request failed with {status}");
        }
        let body = read_body_limited(response, MAX_CAMERAS_BODY_BYTES, "cameras").await?;
        let cameras: Vec<Camera> = serde_json::from_slice(&body).context("decoding the cameras")?;
        validate_cameras(&cameras)?;
        Ok(cameras)
    }
}

async fn read_body_limited(
    mut response: reqwest::Response,
    maximum: usize,
    what: &str,
) -> anyhow::Result<Vec<u8>> {
    if let Some(length) = response.content_length() {
        anyhow::ensure!(
            length <= maximum as u64,
            "{what} response exceeds its byte limit"
        );
    }
    let capacity = response
        .content_length()
        .and_then(|length| usize::try_from(length).ok())
        .unwrap_or(0);
    let mut body = Vec::new();
    body.try_reserve_exact(capacity)
        .with_context(|| format!("reserving the {what} response"))?;
    while let Some(chunk) = response
        .chunk()
        .await
        .with_context(|| format!("reading {what}"))?
    {
        anyhow::ensure!(
            chunk.len() <= maximum.saturating_sub(body.len()),
            "{what} response exceeds its byte limit"
        );
        body.try_reserve(chunk.len())
            .with_context(|| format!("reserving the {what} response"))?;
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

fn valid_camera_id(id: &str) -> bool {
    (1..=MAX_CAMERA_ID_BYTES).contains(&id.len())
        && id.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'_' | b'-')
        })
}

fn valid_display_string(text: &str) -> bool {
    !text.trim().is_empty()
        && text.len() <= MAX_DISPLAY_STRING_BYTES
        && !text.chars().any(char::is_control)
}

fn validate_cameras(cameras: &[Camera]) -> anyhow::Result<()> {
    anyhow::ensure!(cameras.len() <= MAX_CAMERAS, "too many cameras in response");
    let mut ids = HashSet::with_capacity(cameras.len());
    for camera in cameras {
        anyhow::ensure!(valid_camera_id(&camera.id), "invalid camera id in response");
        anyhow::ensure!(
            valid_display_string(&camera.name),
            "invalid camera name in response"
        );
        anyhow::ensure!(ids.insert(&camera.id), "duplicate camera id in response");
    }
    Ok(())
}

/// A recorded segment as `/api/streams/{camera}/{kind}/segments` lists it;
/// only the fields the timeline draws.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
pub struct Segment {
    /// Wall-clock start, Unix microseconds.
    pub wall_start_us: i64,
    /// Wall-clock end, Unix microseconds.
    pub wall_end_us: i64,
    /// Whether the file is sealed (false for the one being written).
    pub sealed: bool,
}

/// One complete fragment selected by the VOD playlist, in served order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PlaybackFragment {
    /// Recorder time of the fragment's first sample, Unix microseconds.
    pub wall_start_us: i64,
    /// Fragment duration in the API's 90 kHz media timescale.
    pub duration_ticks: u32,
}

/// One fetched VOD selection, with timing and media references from the same body.
pub struct VodPlaylist {
    /// Complete fragments in the order this playlist presents them.
    pub fragments: Vec<PlaybackFragment>,
    /// The fetched playlist with media and init URIs resolved against its origin.
    pub body: String,
}

fn absolute_playlist(text: &str, base: &Url) -> anyhow::Result<String> {
    let mut body = String::new();
    for line in text.lines() {
        if line.starts_with("#EXT-X-MAP:") {
            let (prefix, value) = line
                .split_once("URI=\"")
                .context("VOD init map has no URI")?;
            let (uri, suffix) = value.split_once('"').context("invalid VOD init URI")?;
            let absolute = base.join(uri).context("invalid VOD init URI")?;
            body.push_str(prefix);
            body.push_str("URI=\"");
            body.push_str(absolute.as_str());
            body.push('"');
            body.push_str(suffix);
        } else if line.is_empty() || line.starts_with('#') {
            body.push_str(line);
        } else {
            let absolute = base.join(line).context("invalid VOD media URI")?;
            body.push_str(absolute.as_str());
        }
        body.push('\n');
        anyhow::ensure!(
            body.len() <= MAX_VOD_PLAYLIST_BYTES,
            "resolved VOD playlist exceeds its byte limit"
        );
    }
    Ok(body)
}

fn playlist_fragments(text: &str) -> anyhow::Result<Vec<PlaybackFragment>> {
    anyhow::ensure!(
        text.len() <= MAX_VOD_PLAYLIST_BYTES,
        "VOD playlist exceeds its byte limit"
    );
    let mut fragments = Vec::new();
    for line in text.lines() {
        let Some(fields) = line.strip_prefix("#IRIS-FRAGMENT:WALL-US=") else {
            continue;
        };
        let (wall, ticks) = fields
            .split_once(",DURATION-TICKS=")
            .context("invalid VOD fragment timing")?;
        let wall_start_us: i64 = wall.parse().context("invalid VOD fragment wall time")?;
        let duration_ticks: u32 = ticks.parse().context("invalid VOD fragment duration")?;
        anyhow::ensure!(duration_ticks > 0, "zero VOD fragment duration");
        wall_start_us
            .checked_add(i64::from(duration_ticks) * 1_000_000 / 90_000)
            .context("VOD fragment wall time overflows")?;
        anyhow::ensure!(
            fragments.len() < MAX_PLAYBACK_FRAGMENTS,
            "VOD playlist has too many fragments"
        );
        fragments.push(PlaybackFragment {
            wall_start_us,
            duration_ticks,
        });
    }
    anyhow::ensure!(
        !fragments.is_empty(),
        "VOD playlist has no fragment timing metadata"
    );
    Ok(fragments)
}

fn validate_playback_fragments(
    fragments: &[PlaybackFragment],
    from: i64,
    to: i64,
) -> anyhow::Result<()> {
    validate_range(from, to)?;
    let mut previous = None;
    let mut overlaps_window = false;
    for fragment in fragments {
        if let Some(previous) = previous {
            anyhow::ensure!(
                fragment.wall_start_us >= previous,
                "VOD fragment times are not ordered"
            );
        }
        let start = i128::from(fragment.wall_start_us);
        let duration = i128::from(fragment.duration_ticks) * 1_000_000 / 90_000;
        let end = start + duration;
        overlaps_window |= end > i128::from(from) && start < i128::from(to);
        previous = Some(fragment.wall_start_us);
    }
    anyhow::ensure!(
        overlaps_window,
        "VOD playlist has no fragment in the requested window"
    );
    Ok(())
}

fn validate_range(from: i64, to: i64) -> anyhow::Result<()> {
    let span = i128::from(to) - i128::from(from);
    anyhow::ensure!(span > 0, "invalid time range");
    anyhow::ensure!(span <= MAX_RANGE_US, "time range exceeds seven days");
    Ok(())
}

fn validate_segments(segments: &[Segment], from: i64, to: i64) -> anyhow::Result<()> {
    validate_range(from, to)?;
    anyhow::ensure!(
        segments.len() <= MAX_SEGMENTS,
        "too many segments in response"
    );
    for segment in segments {
        anyhow::ensure!(
            segment.wall_start_us < segment.wall_end_us,
            "invalid segment time range in response"
        );
        anyhow::ensure!(
            i128::from(segment.wall_end_us) > i128::from(from)
                && i128::from(segment.wall_start_us) < i128::from(to),
            "segment is outside the requested window"
        );
    }
    Ok(())
}

/// An event as `/api/events` lists it; only the fields the timeline marks.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub struct EventRecord {
    /// The catalog id.
    pub id: i64,
    /// The event kind.
    pub kind: EventKind,
    /// Recorder time of the event, Unix microseconds.
    pub nvr_time: i64,
    /// Until when the footage around the event is kept, when the event is retained.
    #[serde(default)]
    pub retain_until_us: Option<i64>,
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
    anyhow::ensure!(event.id > 0, "invalid event id in response");
    anyhow::ensure!(
        valid_display_string(event.kind.label()),
        "invalid event kind in response"
    );
    Ok(())
}

fn validate_events(events: &[EventRecord], from: i64, to: i64, limit: usize) -> anyhow::Result<()> {
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

fn require_content_type(response: &reqwest::Response, expected: &str) -> anyhow::Result<()> {
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
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
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
        if let Some(path) = self.path.take() {
            let _ = std::fs::remove_file(path);
        }
    }
}

impl Client {
    fn get(&self, url: Url) -> reqwest::RequestBuilder {
        let request = self.http.get(url);
        match &self.token {
            Some(token) => request.bearer_auth(token),
            None => request,
        }
    }

    async fn fetch<T: serde::de::DeserializeOwned>(
        &self,
        url: Url,
        what: &str,
        maximum: usize,
    ) -> anyhow::Result<T> {
        let response = self
            .get(url)
            .send()
            .await
            .with_context(|| format!("requesting {what}"))?;
        let status = response.status();
        if !status.is_success() {
            anyhow::bail!("{what} request failed with {status}");
        }
        let body = read_body_limited(response, maximum, what).await?;
        serde_json::from_slice(&body).with_context(|| format!("decoding {what}"))
    }

    /// Lists the segments of one stream overlapping `[from, to)` (Unix microseconds).
    pub async fn segments(
        &self,
        camera: &str,
        kind: &str,
        from: i64,
        to: i64,
    ) -> anyhow::Result<Vec<Segment>> {
        validate_range(from, to)?;
        let mut url = self
            .base
            .join(&format!("/api/streams/{camera}/{kind}/segments"))
            .context("building the segments URL")?;
        url.query_pairs_mut()
            .append_pair("from", &from.to_string())
            .append_pair("to", &to.to_string());
        let segments: Vec<Segment> = self.fetch(url, "segments", MAX_LIST_BODY_BYTES).await?;
        validate_segments(&segments, from, to)?;
        Ok(segments)
    }

    /// Lists one camera's events in `[from, to)` (Unix microseconds), newest first
    /// as the API orders them, at most `limit`.
    pub async fn events(
        &self,
        camera: &str,
        from: i64,
        to: i64,
        limit: usize,
    ) -> anyhow::Result<Vec<EventRecord>> {
        validate_range(from, to)?;
        anyhow::ensure!(
            (1..=MAX_EVENTS).contains(&limit),
            "invalid event page limit"
        );
        let mut url = self
            .base
            .join("/api/events")
            .context("building the events URL")?;
        url.query_pairs_mut()
            .append_pair("camera", camera)
            .append_pair("from", &from.to_string())
            .append_pair("to", &to.to_string())
            .append_pair("limit", &limit.to_string());
        let events: Vec<EventRecord> = self.fetch(url, "events", MAX_LIST_BODY_BYTES).await?;
        validate_events(&events, from, to, limit)?;
        Ok(events)
    }

    /// Downloads the MP4 export of `[from, to)` to `path`, calling `progress`
    /// with the bytes written so far after every chunk; returns the total.
    pub async fn export(
        &self,
        camera: &str,
        kind: &str,
        from: i64,
        to: i64,
        path: &Path,
        progress: impl Fn(u64) + Send,
    ) -> anyhow::Result<u64> {
        use futures::StreamExt as _;
        use tokio::io::AsyncWriteExt as _;

        validate_range(from, to)?;
        let mut url = self
            .base
            .join(&format!("/api/streams/{camera}/{kind}/export.mp4"))
            .context("building the export URL")?;
        url.query_pairs_mut()
            .append_pair("from", &from.to_string())
            .append_pair("to", &to.to_string());
        let response = self
            .get(url)
            .send()
            .await
            .context("requesting the export")?;
        let status = response.status();
        if status != reqwest::StatusCode::OK {
            anyhow::bail!("export request failed with {status}");
        }
        require_content_type(&response, "video/mp4")?;
        let mut temporary = ExportTemp::create(path).await?;
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
            progress(written);
        }
        anyhow::ensure!(written > 0, "export response is empty");
        temporary.commit(path).await?;
        Ok(written)
    }

    /// Keeps the footage around event `id` from retention until `until` (Unix
    /// microseconds), or clears the mark with `None`; returns the event as the
    /// API now lists it.
    pub async fn retain_event(&self, id: i64, until: Option<i64>) -> anyhow::Result<EventRecord> {
        anyhow::ensure!(id > 0, "invalid event id");
        let url = self
            .base
            .join(&format!("/api/events/{id}/retain"))
            .context("building the retain URL")?;
        let mut request = self.http.post(url);
        if let Some(token) = &self.token {
            request = request.bearer_auth(token);
        }
        let body = match until {
            Some(until) => format!("{{\"until\": \"{until}\"}}"),
            None => "{\"until\": null}".to_owned(),
        };
        let response = request
            .header("content-type", "application/json")
            .body(body)
            .send()
            .await
            .context("retaining the event")?;
        let status = response.status();
        if !status.is_success() {
            anyhow::bail!("retain request failed with {status}");
        }
        let bytes = read_body_limited(response, MAX_RETAIN_BODY_BYTES, "event").await?;
        let event: EventRecord = serde_json::from_slice(&bytes).context("decoding the event")?;
        validate_event(&event)?;
        anyhow::ensure!(event.id == id, "retain response changed the event id");
        Ok(event)
    }

    /// Fetches the keyframe resource at or before `t`: an init segment plus
    /// one single-sample fragment, or `None` when nothing is recorded there.
    pub async fn keyframe(
        &self,
        camera: &str,
        kind: &str,
        t: i64,
    ) -> anyhow::Result<Option<Vec<u8>>> {
        let mut url = self
            .base
            .join(&format!("/api/streams/{camera}/{kind}/keyframe"))
            .context("building the keyframe URL")?;
        url.query_pairs_mut().append_pair("t", &t.to_string());
        let response = self
            .get(url)
            .send()
            .await
            .context("requesting a keyframe")?;
        let status = response.status();
        if status == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        if !status.is_success() {
            anyhow::bail!("keyframe request failed with {status}");
        }
        require_content_type(&response, "video/mp4")?;
        let body = read_body_limited(response, MAX_KEYFRAME_BYTES, "keyframe").await?;
        anyhow::ensure!(!body.is_empty(), "keyframe response is empty");
        Ok(Some(body))
    }

    /// The HLS VOD playlist URL for `[from, to)`. [`Client::vod_playlist`]
    /// fetches it with the bearer header; the returned body carries scoped
    /// media capabilities for the player to follow.
    pub fn vod_url(&self, camera: &str, kind: &str, from: i64, to: i64) -> anyhow::Result<Url> {
        let mut url = self
            .base
            .join(&format!("/api/streams/{camera}/{kind}/hls/vod.m3u8"))
            .context("building the playlist URL")?;
        url.query_pairs_mut()
            .append_pair("from", &from.to_string())
            .append_pair("to", &to.to_string());
        Ok(url)
    }

    /// Fetches one VOD body for both timing and playback, including boundary GOPs
    /// and omitting unavailable files. Relative init and media URIs retain their
    /// original HTTP origin and authentication when the body is served locally.
    pub async fn vod_playlist(
        &self,
        camera: &str,
        kind: &str,
        from: i64,
        to: i64,
    ) -> anyhow::Result<VodPlaylist> {
        validate_range(from, to)?;
        let url = self.vod_url(camera, kind, from, to)?;
        let mut response = self
            .get(url)
            .send()
            .await
            .map_err(reqwest::Error::without_url)
            .context("requesting the VOD playlist")?;
        anyhow::ensure!(
            response.status().is_success(),
            "VOD playlist request failed with {}",
            response.status()
        );
        let mut bytes = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(reqwest::Error::without_url)
            .context("reading the VOD playlist")?
        {
            anyhow::ensure!(
                chunk.len() <= MAX_VOD_PLAYLIST_BYTES.saturating_sub(bytes.len()),
                "VOD playlist exceeds its byte limit"
            );
            bytes.extend_from_slice(&chunk);
        }
        let text = std::str::from_utf8(&bytes).context("VOD playlist is not UTF-8")?;
        let fragments = playlist_fragments(text)?;
        validate_playback_fragments(&fragments, from, to)?;
        Ok(VodPlaylist {
            fragments,
            body: absolute_playlist(text, response.url())?,
        })
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::io;
    use std::sync::Arc;

    use axum::Router;
    use axum::body::{Body, Bytes};
    use axum::extract::Query;
    use axum::http::{StatusCode, header};
    use axum::response::Response;
    use axum::routing::get;
    use futures::StreamExt as _;
    use futures::stream;

    use super::*;

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
    fn response_values_are_validated_before_ui_use() {
        let cameras = vec![Camera {
            id: "cam1".into(),
            name: "Front".into(),
        }];
        assert!(validate_cameras(&cameras).is_ok());
        assert!(
            validate_cameras(&[
                cameras[0].clone(),
                Camera {
                    id: "cam1".into(),
                    name: "Duplicate".into(),
                },
            ])
            .is_err()
        );
        assert!(
            validate_cameras(&[Camera {
                id: "../cam".into(),
                name: "Front".into(),
            }])
            .is_err()
        );
        assert!(
            validate_cameras(&[Camera {
                id: "cam2".into(),
                name: "\n".into(),
            }])
            .is_err()
        );
        assert!(validate_cameras(&vec![cameras[0].clone(); MAX_CAMERAS + 1]).is_err());

        let segment = Segment {
            wall_start_us: 0,
            wall_end_us: 10,
            sealed: true,
        };
        assert!(validate_segments(&[segment], 0, 10).is_ok());
        assert!(validate_segments(&[segment], i64::MIN, i64::MAX).is_err());
        assert!(
            validate_segments(
                &[Segment {
                    wall_start_us: 20,
                    wall_end_us: 30,
                    sealed: true,
                }],
                0,
                10,
            )
            .is_err()
        );

        let event = EventRecord {
            id: 1,
            kind: EventKind::Named("motion".into()),
            nvr_time: 5,
            retain_until_us: None,
        };
        assert!(validate_events(std::slice::from_ref(&event), 0, 10, 1).is_ok());
        let mut invalid = event;
        invalid.nvr_time = i64::MAX;
        assert!(validate_events(&[invalid], 0, 10, 1).is_err());

        let ordered = [
            PlaybackFragment {
                wall_start_us: -10,
                duration_ticks: 900,
            },
            PlaybackFragment {
                wall_start_us: 10,
                duration_ticks: 900,
            },
        ];
        assert!(validate_playback_fragments(&ordered, 0, 20).is_ok());
        let reversed = [ordered[1], ordered[0]];
        assert!(validate_playback_fragments(&reversed, 0, 20).is_err());
        assert!(validate_playback_fragments(&ordered, 20_000, 30_000).is_err());
    }

    #[tokio::test]
    async fn counted_reads_and_idle_timeout_bound_responses() {
        let chunked = || async {
            Body::from_stream(stream::iter([
                Ok::<Bytes, io::Error>(Bytes::from_static(b"ab")),
                Ok(Bytes::from_static(b"cd")),
            ]))
        };
        let stalled = || async {
            let body = stream::once(async { Ok::<Bytes, io::Error>(Bytes::from_static(b"[")) })
                .chain(stream::pending::<Result<Bytes, io::Error>>());
            Response::builder()
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from_stream(body))
                .unwrap()
        };
        let (base, server) = serve_fixture(
            Router::new()
                .route("/chunked", get(chunked))
                .route("/api/cameras", get(stalled)),
        )
        .await;
        let client = Client::with_timeouts(
            base.clone(),
            None,
            Duration::from_secs(1),
            Duration::from_millis(30),
        );
        let response = client
            .http
            .get(base.join("/chunked").unwrap())
            .send()
            .await
            .unwrap();
        assert!(read_body_limited(response, 3, "fixture").await.is_err());
        assert!(
            tokio::time::timeout(Duration::from_secs(1), client.cameras())
                .await
                .expect("client read deadline must fire")
                .is_err()
        );
        server.abort();
    }

    #[tokio::test]
    async fn export_commits_only_complete_owned_temporary_files() {
        let app = Router::new().route(
            "/api/streams/{camera}/{kind}/export.mp4",
            get(export_fixture),
        );
        let (base, server) = serve_fixture(app).await;
        let client = Arc::new(Client::new(base, None));
        let directory = tempfile::tempdir().unwrap();
        let destination = directory.path().join("recording.mp4");
        let sentinel = b"pre-existing recording";

        for from in [1, 2, 3] {
            std::fs::write(&destination, sentinel).unwrap();
            assert!(
                client
                    .export("cam1", "main", from, from + 1, &destination, |_| {})
                    .await
                    .is_err()
            );
            assert_eq!(std::fs::read(&destination).unwrap(), sentinel);
            assert_eq!(export_temporaries(directory.path()), Vec::<PathBuf>::new());
        }

        assert_eq!(
            client
                .export("cam1", "main", 4, 5, &destination, |_| {})
                .await
                .unwrap(),
            12
        );
        assert_eq!(std::fs::read(&destination).unwrap(), b"complete mp4");

        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;

            let linked = directory.path().join("linked.mp4");
            std::fs::write(&linked, sentinel).unwrap();
            std::fs::remove_file(&destination).unwrap();
            symlink(&linked, &destination).unwrap();
            client
                .export("cam1", "main", 4, 5, &destination, |_| {})
                .await
                .unwrap();
            assert_eq!(std::fs::read(&linked).unwrap(), sentinel);
            assert_eq!(std::fs::read(&destination).unwrap(), b"complete mp4");
            assert!(
                std::fs::symlink_metadata(&destination)
                    .unwrap()
                    .file_type()
                    .is_file()
            );
        }

        let first = client.export("cam1", "main", 10, 11, &destination, |_| {});
        let second = client.export("cam1", "main", 20, 21, &destination, |_| {});
        let (first, second) = tokio::join!(first, second);
        assert!(first.is_ok() && second.is_ok());
        let complete_a = vec![b'A'; 8 * 1024];
        let complete_b = vec![b'B'; 8 * 1024];
        let result = std::fs::read(&destination).unwrap();
        assert!(result == complete_a || result == complete_b);
        assert_eq!(export_temporaries(directory.path()), Vec::<PathBuf>::new());

        std::fs::write(&destination, sentinel).unwrap();
        let task = tokio::spawn({
            let client = Arc::clone(&client);
            let destination = destination.clone();
            async move {
                client
                    .export("cam1", "main", 30, 31, &destination, |_| {})
                    .await
            }
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

    #[tokio::test]
    async fn export_rejects_partial_content_before_creating_a_temporary() {
        let app = Router::new().route(
            "/api/streams/{camera}/{kind}/export.mp4",
            get(export_fixture),
        );
        let (base, server) = serve_fixture(app).await;
        let client = Client::new(base, None);
        let directory = tempfile::tempdir().unwrap();
        let destination = directory.path().join("recording.mp4");
        let sentinel = b"pre-existing recording";
        std::fs::write(&destination, sentinel).unwrap();

        assert!(
            client
                .export("cam1", "main", 5, 6, &destination, |_| {})
                .await
                .is_err()
        );
        assert_eq!(std::fs::read(&destination).unwrap(), sentinel);
        assert_eq!(export_temporaries(directory.path()), Vec::<PathBuf>::new());
        server.abort();
    }

    #[cfg(target_os = "linux")]
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
    fn vod_timing_preserves_fragment_order_and_full_boundary_gops() {
        let fragments = playlist_fragments(concat!(
            "#EXTM3U\n",
            "#IRIS-FRAGMENT:WALL-US=100,DURATION-TICKS=180000\n",
            "#IRIS-FRAGMENT:WALL-US=9000000,DURATION-TICKS=3001\n",
        ))
        .unwrap();
        assert_eq!(
            fragments,
            vec![
                PlaybackFragment {
                    wall_start_us: 100,
                    duration_ticks: 180_000
                },
                PlaybackFragment {
                    wall_start_us: 9_000_000,
                    duration_ticks: 3001
                },
            ]
        );
        for invalid in [
            "#EXTM3U\n",
            "#IRIS-FRAGMENT:WALL-US=0,DURATION-TICKS=0",
            "#IRIS-FRAGMENT:WALL-US=9223372036854775807,DURATION-TICKS=90000",
        ] {
            assert!(playlist_fragments(invalid).is_err());
        }
    }

    #[test]
    fn snapshot_uris_keep_the_response_origin_queries_and_byte_ranges() {
        let base =
            Url::parse("https://192.0.2.10/api/streams/fixture/main/hls/vod.m3u8?token=unused")
                .unwrap();
        let playlist = concat!(
            "#EXTM3U\n",
            "#EXT-X-MAP:URI=\"/api/segments/1/file.mp4?token=encoded%26token\",BYTERANGE=\"100@0\"\n",
            "#IRIS-FRAGMENT:WALL-US=10,DURATION-TICKS=90000\n",
            "#EXTINF:1.00000,\n#EXT-X-BYTERANGE:200@100\n",
            "../media/file.mp4?token=encoded%26token\n",
            "https://192.0.2.20/file.mp4?token=other\n#EXT-X-ENDLIST\n"
        );
        let body = absolute_playlist(playlist, &base).unwrap();
        assert!(body.contains("#EXT-X-MAP:URI=\"https://192.0.2.10/api/segments/1/file.mp4?token=encoded%26token\",BYTERANGE=\"100@0\"\n"));
        assert!(body.contains("#EXT-X-BYTERANGE:200@100\nhttps://192.0.2.10/api/streams/fixture/main/media/file.mp4?token=encoded%26token\n"));
        assert!(body.contains("https://192.0.2.20/file.mp4?token=other\n"));
        assert_eq!(
            playlist_fragments(playlist).unwrap(),
            playlist_fragments(&body).unwrap()
        );
    }

    #[test]
    fn event_kinds_decode_named_and_other_forms() {
        let events: Vec<EventRecord> = serde_json::from_str(
            r#"[{"id":1,"kind":"motion","nvr_time":1},{"id":2,"kind":{"other":"NewFile"},"nvr_time":2,"retain_until_us":5}]"#,
        )
        .unwrap();
        assert_eq!(events[0].kind.label(), "motion");
        assert_eq!(events[1].kind.label(), "NewFile");
        assert_eq!(events[1].nvr_time, 2);
        assert_eq!(events[0].retain_until_us, None);
        assert_eq!(events[1].retain_until_us, Some(5));
    }

    #[test]
    fn the_playlist_url_carries_the_range_but_not_the_root_bearer() {
        let client = Client::new(
            Url::parse("http://192.0.2.10:8554").unwrap(),
            Some("t".into()),
        );
        assert_eq!(
            client.vod_url("cam1", "sub", 10, 20).unwrap().as_str(),
            "http://192.0.2.10:8554/api/streams/cam1/sub/hls/vod.m3u8?from=10&to=20"
        );
    }
}
