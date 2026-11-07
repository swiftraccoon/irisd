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
/// The tags the daemon's VOD writer emits. Any other `#EXT` tag rejects a
/// playlist, in particular the URI-bearing ones (`EXT-X-KEY`, `EXT-X-MEDIA`,
/// `EXT-X-PART`, `EXT-X-STREAM-INF`, ...) that would make the player fetch
/// from elsewhere. Other comments, such as `#IRIS-FRAGMENT`, pass through.
const VOD_TAGS: [&str; 12] = [
    "#EXTM3U",
    "#EXT-X-VERSION",
    "#EXT-X-TARGETDURATION",
    "#EXT-X-PLAYLIST-TYPE",
    "#EXT-X-MEDIA-SEQUENCE",
    "#EXT-X-DISCONTINUITY-SEQUENCE",
    "#EXT-X-INDEPENDENT-SEGMENTS",
    "#EXT-X-MAP",
    "#EXTINF",
    "#EXT-X-BYTERANGE",
    "#EXT-X-DISCONTINUITY",
    "#EXT-X-ENDLIST",
];
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
    pub id: String,
    /// The configured display name.
    pub name: String,
}

/// Space on the filesystem an export writes to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct FilesystemSpace {
    /// Bytes an unprivileged writer may still use.
    available: u64,
    /// The filesystem's size in bytes.
    total: u64,
}

/// Reads the space of the filesystem holding an open file.
type SpaceProbe = fn(&tokio::fs::File) -> std::io::Result<FilesystemSpace>;

/// `fstatvfs` on the export's own descriptor, so the figure is for the
/// filesystem the bytes land on.
fn filesystem_space(file: &tokio::fs::File) -> std::io::Result<FilesystemSpace> {
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

/// An authenticated API client.
pub struct Client {
    base: Url,
    token: Option<String>,
    http: reqwest::Client,
    /// Free-space probe for exports; tests inject a fixed answer.
    space_probe: SpaceProbe,
    /// Bytes an export writes between free-space checks.
    space_check_bytes: u64,
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
        // The daemon never redirects; following one would let a response name
        // the origin that playlist references are checked against.
        let http = reqwest::Client::builder()
            .connect_timeout(connect_timeout)
            .read_timeout(read_timeout)
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .expect("static HTTP client options are valid");
        Self {
            base,
            token,
            http,
            space_probe: filesystem_space,
            space_check_bytes: EXPORT_SPACE_CHECK_BYTES,
        }
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
    /// The fetched playlist with media and init URIs resolved against its URL,
    /// every one on the API origin.
    pub body: String,
}

/// Whether a playlist line is an HLS tag. The comparison ignores case so that
/// no spelling of a tag escapes [`VOD_TAGS`].
fn is_tag(line: &str) -> bool {
    line.as_bytes()
        .get(..4)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case(b"#EXT"))
}

/// Resolves a playlist reference against `base` and requires the result to be
/// on `origin` (scheme, host, and effective port) without userinfo. Opaque
/// origins such as `file:` and `data:` never match.
fn same_origin(base: &Url, reference: &str, origin: &url::Origin) -> anyhow::Result<Url> {
    let absolute = base.join(reference)?;
    anyhow::ensure!(
        absolute.origin() == *origin
            && absolute.username().is_empty()
            && absolute.password().is_none(),
        "VOD playlist references another origin"
    );
    Ok(absolute)
}

/// Rewrites the daemon's VOD body for the player: init and media references
/// become absolute URLs on `origin`, and the body may hold only the tags in
/// [`VOD_TAGS`].
fn absolute_playlist(text: &str, base: &Url, origin: &url::Origin) -> anyhow::Result<String> {
    let mut body = String::new();
    for line in text.lines() {
        if is_tag(line) {
            let name = line.split_once(':').map_or(line, |(name, _)| name);
            anyhow::ensure!(
                VOD_TAGS.contains(&name),
                "VOD playlist has a tag the daemon does not emit"
            );
        }
        if let Some(attributes) = line.strip_prefix("#EXT-X-MAP:") {
            let value = attributes
                .strip_prefix("URI=\"")
                .context("VOD init map must start with its URI")?;
            let (uri, suffix) = value.split_once('"').context("invalid VOD init URI")?;
            anyhow::ensure!(!suffix.contains("URI="), "VOD init map has a second URI");
            let absolute = same_origin(base, uri, origin).context("invalid VOD init URI")?;
            body.push_str("#EXT-X-MAP:URI=\"");
            body.push_str(absolute.as_str());
            body.push('"');
            body.push_str(suffix);
        } else if line.is_empty() || line.starts_with('#') {
            body.push_str(line);
        } else {
            let absolute = same_origin(base, line, origin).context("invalid VOD media URI")?;
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

/// Checks a segment listing against the API's overlap rule. A sealed row must
/// span a positive interval that overlaps `[from, to)`. The API treats an open
/// row as extending to the present, and its `wall_end_us` is its last durable
/// progress, so an open row needs only `wall_start_us <= wall_end_us` and
/// `wall_start_us < to`: its recorded end can precede `from` (a camera offline
/// for longer than the window) or equal its start (a file opened after a
/// rotation or reconnect, before its first sync).
fn validate_segments(segments: &[Segment], from: i64, to: i64) -> anyhow::Result<()> {
    validate_range(from, to)?;
    anyhow::ensure!(
        segments.len() <= MAX_SEGMENTS,
        "too many segments in response"
    );
    for segment in segments {
        if segment.sealed {
            anyhow::ensure!(
                segment.wall_start_us < segment.wall_end_us,
                "invalid segment time range in response"
            );
            anyhow::ensure!(
                segment.wall_end_us > from && segment.wall_start_us < to,
                "segment is outside the requested window"
            );
        } else {
            anyhow::ensure!(
                segment.wall_start_us <= segment.wall_end_us,
                "invalid open segment time range in response"
            );
            anyhow::ensure!(
                segment.wall_start_us < to,
                "segment is outside the requested window"
            );
        }
    }
    Ok(())
}

/// Drops open rows that have recorded nothing inside `[from, to)`, so a window
/// holding only such a row reads as having nothing recorded. Rows that pass
/// [`validate_segments`] and remain cover part of the window.
fn recorded_in_window(mut segments: Vec<Segment>, from: i64) -> Vec<Segment> {
    segments.retain(|segment| {
        segment.sealed
            || (segment.wall_end_us > from && segment.wall_start_us < segment.wall_end_us)
    });
    segments
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

    /// Lists the segments of one stream that hold recordings inside `[from, to)`
    /// (Unix microseconds). Open segments that have recorded nothing there, such
    /// as a stale file of an offline camera or a file before its first sync, are
    /// omitted.
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
        Ok(recorded_in_window(segments, from))
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
    /// Before the first byte and after every further 64 MiB it checks the
    /// destination filesystem with `fstatvfs` and stops, removing the partial
    /// file, when free space would fall below the larger of 1 GiB and 2 % of
    /// the filesystem.
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
        let check_space = |file: &tokio::fs::File| {
            check_export_space(
                (self.space_probe)(file).context("reading the export filesystem's free space")?,
            )
        };
        check_space(temporary.file()?)?;
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
                check_space(temporary.file()?)?;
                next_space_check = written.saturating_add(self.space_check_bytes);
            }
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
    /// and omitting unavailable files. Init and media URIs resolve to absolute
    /// URLs on the API origin and keep their authentication queries, so they
    /// still reach the daemon when the body is served locally. A reference to
    /// another origin or scheme, userinfo, or a tag the daemon does not emit
    /// rejects the playlist.
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
        let origin = self.base.origin();
        anyhow::ensure!(
            response.url().origin() == origin,
            "VOD playlist response came from another origin"
        );
        Ok(VodPlaylist {
            fragments,
            body: absolute_playlist(text, response.url(), &origin)?,
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

    #[test]
    fn open_rows_follow_the_api_overlap_rule_and_sealed_rows_stay_strict() {
        let row = |wall_start_us, wall_end_us, sealed| Segment {
            wall_start_us,
            wall_end_us,
            sealed,
        };
        let (from, to) = (100, 200);
        // An open file of a camera offline since before the window.
        let stale = row(10, 50, false);
        // A file opened after a rotation, before its first sync.
        let fresh = row(150, 150, false);
        let recording = row(90, 150, false);
        assert!(validate_segments(&[stale, fresh, recording], from, to).is_ok());
        for invalid in [
            row(60, 50, false),
            row(200, 200, false),
            row(250, 260, false),
        ] {
            assert!(
                validate_segments(&[invalid], from, to).is_err(),
                "{invalid:?}"
            );
        }
        for invalid in [row(10, 50, true), row(150, 150, true), row(200, 260, true)] {
            assert!(
                validate_segments(&[invalid], from, to).is_err(),
                "{invalid:?}"
            );
        }
        assert_eq!(
            recorded_in_window(vec![stale, fresh, recording, row(100, 120, true)], from),
            [recording, row(100, 120, true)]
        );
    }

    #[tokio::test]
    async fn a_window_with_only_a_stale_open_row_lists_nothing_recorded() {
        // The API's rows for a one-hour window ending at TO: a file opened 3 h
        // before TO whose last durable progress was 2 h before it, and a file
        // opened before its first sync.
        const FROM: i64 = 1_794_076_005_736_367;
        const TO: i64 = 1_794_079_605_736_367;
        const STALE: &str = r#"[{"id":1,"camera":"cam1","kind":"main","run_seq":1,"discontinuity":true,"wall_start_us":1794068805736367,"wall_end_us":1794072405736367,"fragments":10,"bytes":4096,"sealed":false}]"#;
        const FRESH: &str = r#"[{"id":2,"camera":"cam1","kind":"main","run_seq":1,"discontinuity":true,"wall_start_us":1794079604736367,"wall_end_us":1794079604736367,"fragments":0,"bytes":768,"sealed":false}]"#;
        for body in [STALE, FRESH] {
            let (base, server) = serve_fixture(Router::new().route(
                "/api/streams/{camera}/{kind}/segments",
                get(move || async move { body }),
            ))
            .await;
            let client = Client::new(base, None);
            assert_eq!(
                client.segments("cam1", "main", FROM, TO).await.unwrap(),
                [],
                "{body}"
            );
            server.abort();
        }
    }

    #[tokio::test]
    async fn the_viewer_does_not_follow_redirects() {
        let other_requests = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let (other, other_server) = serve_fixture(Router::new().fallback({
            let other_requests = Arc::clone(&other_requests);
            move || async move {
                other_requests.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                "#EXTM3U\n"
            }
        }))
        .await;
        let target = other.join("/vod.m3u8").unwrap().to_string();
        let (base, server) = serve_fixture(Router::new().route(
            "/api/streams/{camera}/{kind}/hls/vod.m3u8",
            get(move || async move {
                Response::builder()
                    .status(StatusCode::FOUND)
                    .header(header::LOCATION, target)
                    .body(Body::empty())
                    .unwrap()
            }),
        ))
        .await;
        let client = Client::new(base, None);
        let error = client
            .vod_playlist("cam1", "main", 1, 2)
            .await
            .err()
            .expect("a redirected playlist is refused");
        assert!(format!("{error:#}").contains("302"), "{error:#}");
        assert_eq!(other_requests.load(std::sync::atomic::Ordering::SeqCst), 0);
        server.abort();
        other_server.abort();
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

    #[test]
    fn the_export_reserve_is_the_larger_of_one_gib_and_two_percent() {
        assert_eq!(export_reserve(10 << 30), 1 << 30);
        assert_eq!(export_reserve(1 << 40), (1 << 40) / 50);
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
        let file = tempfile::tempfile().unwrap();
        let space = filesystem_space(&tokio::fs::File::from_std(file)).unwrap();
        assert!(
            space.total > 0 && space.available <= space.total,
            "{space:?}"
        );
    }

    /// Calls of the plenty-then-low probe in [`exports_stop_before_the_destination_fills`].
    static SPACE_PROBES: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

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
    async fn exports_stop_before_the_destination_fills() {
        let app = Router::new().route(
            "/api/streams/{camera}/{kind}/export.mp4",
            get(export_fixture),
        );
        let (base, server) = serve_fixture(app).await;
        let directory = tempfile::tempdir().unwrap();
        let destination = directory.path().join("recording.mp4");
        let sentinel = b"pre-existing recording";
        std::fs::write(&destination, sentinel).unwrap();

        let mut client = Client::new(base.clone(), None);
        client.space_probe = |_| Ok(LOW_SPACE);
        let written = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let observed = Arc::clone(&written);
        let error = client
            .export("cam1", "main", 10, 11, &destination, move |bytes| {
                observed.store(bytes, std::sync::atomic::Ordering::SeqCst);
            })
            .await
            .expect_err("too little space refuses the export before writing");
        assert!(format!("{error:#}").contains("reserve"), "{error:#}");
        assert_eq!(written.load(std::sync::atomic::Ordering::SeqCst), 0);
        assert_eq!(std::fs::read(&destination).unwrap(), sentinel);
        assert_eq!(export_temporaries(directory.path()), Vec::<PathBuf>::new());

        let mut client = Client::new(base, None);
        client.space_probe = |_| {
            if SPACE_PROBES.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 {
                Ok(PLENTY_OF_SPACE)
            } else {
                Ok(LOW_SPACE)
            }
        };
        client.space_check_bytes = 2048;
        let error = client
            .export("cam1", "main", 10, 11, &destination, |_| {})
            .await
            .expect_err("space running low stops the export at the next check");
        assert!(format!("{error:#}").contains("reserve"), "{error:#}");
        assert_eq!(SPACE_PROBES.load(std::sync::atomic::Ordering::SeqCst), 2);
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

    /// The response URL of a fixture VOD request, and the API origin it belongs to.
    fn vod_base() -> (Url, url::Origin) {
        let base =
            Url::parse("https://192.0.2.10/api/streams/fixture/main/hls/vod.m3u8?token=unused")
                .unwrap();
        let origin = Url::parse("https://192.0.2.10:443/").unwrap().origin();
        (base, origin)
    }

    #[test]
    fn snapshot_uris_keep_the_response_origin_queries_and_byte_ranges() {
        let (base, origin) = vod_base();
        let playlist = concat!(
            "#EXTM3U\n#EXT-X-VERSION:7\n#EXT-X-TARGETDURATION:1\n#EXT-X-MEDIA-SEQUENCE:0\n",
            "#EXT-X-PLAYLIST-TYPE:VOD\n#EXT-X-INDEPENDENT-SEGMENTS\n",
            "#EXT-X-MAP:URI=\"/api/segments/1/file.mp4?token=encoded%26token\",BYTERANGE=\"100@0\"\n",
            "#IRIS-FRAGMENT:WALL-US=10,DURATION-TICKS=90000\n",
            "#EXTINF:1.00000,\n#EXT-X-BYTERANGE:200@100\n",
            "../media/file.mp4?token=encoded%26token\n",
            "#EXT-X-DISCONTINUITY\n#EXTINF:1.00000,\n",
            "https://192.0.2.10:443/api/segments/2/file.mp4?token=same\n#EXT-X-ENDLIST\n"
        );
        let body = absolute_playlist(playlist, &base, &origin).unwrap();
        assert!(body.contains("#EXT-X-MAP:URI=\"https://192.0.2.10/api/segments/1/file.mp4?token=encoded%26token\",BYTERANGE=\"100@0\"\n"));
        assert!(body.contains("#EXT-X-BYTERANGE:200@100\nhttps://192.0.2.10/api/streams/fixture/main/media/file.mp4?token=encoded%26token\n"));
        assert!(body.contains("\nhttps://192.0.2.10/api/segments/2/file.mp4?token=same\n"));
        assert_eq!(
            playlist_fragments(playlist).unwrap(),
            playlist_fragments(&body).unwrap()
        );
    }

    #[test]
    fn playlist_references_must_stay_on_the_api_origin() {
        let (base, origin) = vod_base();
        let with_media = |uri: &str| {
            format!(
                "#EXTM3U\n#EXT-X-MAP:URI=\"/api/segments/1/file.mp4\"\n#EXTINF:1.0,\n{uri}\n#EXT-X-ENDLIST\n"
            )
        };
        let with_map = |uri: &str| {
            format!("#EXTM3U\n#EXT-X-MAP:URI=\"{uri}\"\n#EXTINF:1.0,\n/api/segments/1/file.mp4\n")
        };
        for foreign in [
            "https://192.0.2.20/file.mp4?token=other",
            "//192.0.2.20/file.mp4",
            "https://192.0.2.10:8443/file.mp4",
            "http://192.0.2.10/file.mp4",
            "file:///etc/hostname",
            "data:video/mp4;base64,AAAA",
            "https://user@192.0.2.10/file.mp4",
            "https://user:secret@192.0.2.10/file.mp4",
        ] {
            assert!(
                absolute_playlist(&with_media(foreign), &base, &origin).is_err(),
                "media {foreign}"
            );
            assert!(
                absolute_playlist(&with_map(foreign), &base, &origin).is_err(),
                "map {foreign}"
            );
        }
        assert!(absolute_playlist(&with_media("/api/segments/2/file.mp4"), &base, &origin).is_ok());
        for map in [
            "#EXT-X-MAP:BYTERANGE=\"1@0\",URI=\"/api/segments/1/file.mp4\"",
            "#EXT-X-MAP:URI=\"/api/segments/1/file.mp4\",URI=\"https://192.0.2.20/x\"",
            "#EXT-X-MAP:URI=/api/segments/1/file.mp4",
        ] {
            assert!(
                absolute_playlist(&format!("#EXTM3U\n{map}\n"), &base, &origin).is_err(),
                "{map}"
            );
        }
    }

    #[test]
    fn playlists_carry_only_the_tags_the_daemon_emits() {
        let (base, origin) = vod_base();
        for tag in [
            "#EXT-X-KEY:METHOD=AES-128,URI=\"https://192.0.2.20/key.bin\"",
            "#EXT-X-KEY:METHOD=NONE",
            "#EXT-X-SESSION-KEY:METHOD=AES-128,URI=\"/key.bin\"",
            "#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"a\",NAME=\"a\",URI=\"https://192.0.2.20/a.m3u8\"",
            "#EXT-X-PART:DURATION=0.5,URI=\"https://192.0.2.20/part.m4s\"",
            "#EXT-X-PRELOAD-HINT:TYPE=PART,URI=\"https://192.0.2.20/part.m4s\"",
            "#EXT-X-RENDITION-REPORT:URI=\"https://192.0.2.20/other.m3u8\"",
            "#EXT-X-I-FRAME-STREAM-INF:BANDWIDTH=1,URI=\"https://192.0.2.20/i.m3u8\"",
            "#EXT-X-SESSION-DATA:DATA-ID=\"x\",URI=\"https://192.0.2.20/data.json\"",
            "#EXT-X-STREAM-INF:BANDWIDTH=1",
            "#EXT-X-PROGRAM-DATE-TIME:2026-11-01T00:00:00Z",
            "#ext-x-key:METHOD=AES-128,URI=\"https://192.0.2.20/key.bin\"",
            "#EXTM3U8",
        ] {
            let playlist =
                format!("#EXTM3U\n{tag}\n#EXTINF:1.0,\n/api/segments/1/file.mp4\n#EXT-X-ENDLIST\n");
            assert!(
                absolute_playlist(&playlist, &base, &origin).is_err(),
                "{tag}"
            );
        }
        let commented = "#EXTM3U\n# a comment\n#IRIS-FRAGMENT:WALL-US=1,DURATION-TICKS=9\n";
        assert_eq!(
            absolute_playlist(commented, &base, &origin).unwrap(),
            commented
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
