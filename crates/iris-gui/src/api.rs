//! The small part of the irisd HTTP API the viewer reads.

use anyhow::Context;
use serde::Deserialize;
use url::Url;

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
        Self {
            base,
            token,
            http: reqwest::Client::new(),
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
        let body = response.bytes().await.context("reading the cameras")?;
        serde_json::from_slice(&body).context("decoding the cameras")
    }
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
        let body = response
            .bytes()
            .await
            .with_context(|| format!("reading {what}"))?;
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
        let mut url = self
            .base
            .join(&format!("/api/streams/{camera}/{kind}/segments"))
            .context("building the segments URL")?;
        url.query_pairs_mut()
            .append_pair("from", &from.to_string())
            .append_pair("to", &to.to_string());
        self.fetch(url, "segments").await
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
        let mut url = self
            .base
            .join("/api/events")
            .context("building the events URL")?;
        url.query_pairs_mut()
            .append_pair("camera", camera)
            .append_pair("from", &from.to_string())
            .append_pair("to", &to.to_string())
            .append_pair("limit", &limit.to_string());
        self.fetch(url, "events").await
    }

    /// Downloads the MP4 export of `[from, to)` to `path`, calling `progress`
    /// with the bytes written so far after every chunk; returns the total.
    pub async fn export(
        &self,
        camera: &str,
        kind: &str,
        from: i64,
        to: i64,
        path: &std::path::Path,
        progress: impl Fn(u64) + Send,
    ) -> anyhow::Result<u64> {
        use futures::StreamExt as _;
        use tokio::io::AsyncWriteExt as _;
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
        if !status.is_success() {
            anyhow::bail!("export request failed with {status}");
        }
        let mut file = tokio::fs::File::create(path)
            .await
            .with_context(|| format!("creating {}", path.display()))?;
        let mut stream = response.bytes_stream();
        let mut written = 0u64;
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.context("reading the export")?;
            file.write_all(&chunk).await.context("writing the export")?;
            written += chunk.len() as u64;
            progress(written);
        }
        file.flush().await.context("flushing the export")?;
        Ok(written)
    }

    /// Keeps the footage around event `id` from retention until `until` (Unix
    /// microseconds), or clears the mark with `None`; returns the event as the
    /// API now lists it.
    pub async fn retain_event(&self, id: i64, until: Option<i64>) -> anyhow::Result<EventRecord> {
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
        let bytes = response.bytes().await.context("reading the event")?;
        serde_json::from_slice(&bytes).context("decoding the event")
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
        let body = response.bytes().await.context("reading a keyframe")?;
        Ok(Some(body.to_vec()))
    }

    /// The HLS VOD playlist URL for `[from, to)`, with the token as a query
    /// parameter because the media player fetches it without headers.
    pub fn vod_url(&self, camera: &str, kind: &str, from: i64, to: i64) -> anyhow::Result<Url> {
        let mut url = self
            .base
            .join(&format!("/api/streams/{camera}/{kind}/hls/vod.m3u8"))
            .context("building the playlist URL")?;
        {
            let mut query = url.query_pairs_mut();
            query
                .append_pair("from", &from.to_string())
                .append_pair("to", &to.to_string());
            if let Some(token) = &self.token {
                query.append_pair("token", token);
            }
        }
        Ok(url)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn the_playlist_url_carries_range_and_token() {
        let client = Client::new(
            Url::parse("http://192.0.2.10:8554").unwrap(),
            Some("t".into()),
        );
        assert_eq!(
            client.vod_url("cam1", "sub", 10, 20).unwrap().as_str(),
            "http://192.0.2.10:8554/api/streams/cam1/sub/hls/vod.m3u8?from=10&to=20&token=t"
        );
    }
}
