//! A private HTTP endpoint for one immutable VOD playlist. Only the manifest is
//! local: the body's init and media URIs are absolute URLs on the API origin
//! ([`crate::api::VodPlaylist::body`]), so GStreamer fetches media from the
//! daemon.

use std::fmt;
use std::fmt::Write as _;
use std::net::Ipv4Addr;

use anyhow::Context;
use axum::Router;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::HeaderName;
use axum::http::header;
use axum::routing;
use tokio::net::TcpListener;
use tokio::task::JoinHandle;
use url::Url;

/// Serves the same playlist on every fetch until the playback owner drops it.
///
/// The loopback route has a 256-bit OS-random capability because its body can
/// contain authenticated media URLs. Neither the route nor the body is logged,
/// and the `Debug` output names the port alone.
pub struct Snapshot {
    url: Url,
    server: JoinHandle<()>,
}

impl fmt::Debug for Snapshot {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Snapshot")
            .field("port", &self.url.port())
            .finish_non_exhaustive()
    }
}

impl Snapshot {
    /// Starts a loopback listener on the calling Tokio runtime for `body`. Every
    /// URI in `body` must be absolute: a relative one would resolve against the
    /// loopback endpoint, which serves nothing but the playlist.
    pub async fn new(body: String) -> anyhow::Result<Self> {
        let mut random = [0; 32];
        getrandom::fill(&mut random).context("creating a private playback route")?;
        let mut capability = String::with_capacity(64);
        for byte in random {
            write!(capability, "{byte:02x}").expect("writing to a String cannot fail");
        }
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .context("binding the playback snapshot listener")?;
        let address = listener
            .local_addr()
            .context("reading the playback address")?;
        let path = format!("/{capability}/vod.m3u8");
        let url = Url::parse(&format!("http://{address}{path}"))
            .context("building the playback snapshot URL")?;
        let app = Router::new()
            .route(&path, routing::get(playlist))
            .with_state(Bytes::from(body));
        let server = tokio::spawn(async move {
            // A listener failure reaches GStreamer as a failed playlist request.
            let _ = axum::serve(listener, app).await;
        });
        Ok(Self { url, server })
    }

    /// The capability URL given only to the media player.
    pub fn url(&self) -> &Url {
        &self.url
    }
}

async fn playlist(State(body): State<Bytes>) -> ([(HeaderName, &'static str); 2], Bytes) {
    (
        [
            (header::CONTENT_TYPE, "application/vnd.apple.mpegurl"),
            (header::CACHE_CONTROL, "no-store"),
        ],
        body,
    )
}

impl Drop for Snapshot {
    fn drop(&mut self) {
        // Aborting the server drops the listener, so no further connection is
        // accepted; an open connection closes after its current request. A
        // runtime task is never joined from GTK's main loop.
        self.server.abort();
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use reqwest::StatusCode;

    use super::*;

    const BODY: &str = "#EXTM3U\nhttp://192.0.2.1/file.mp4?token=fixture-token\n";

    /// A client that never consults the proxy environment, so the loopback
    /// requests reach the snapshot.
    fn http() -> reqwest::Client {
        reqwest::Client::builder().no_proxy().build().unwrap()
    }

    #[tokio::test]
    async fn the_snapshot_serves_the_same_body_on_every_fetch() {
        let snapshot = Snapshot::new(BODY.to_owned()).await.unwrap();
        for _ in 0..2 {
            let response = http().get(snapshot.url().clone()).send().await.unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            assert_eq!(response.text().await.unwrap(), BODY);
        }
    }

    #[tokio::test]
    async fn the_snapshot_answers_only_a_get_of_its_capability_path() {
        let snapshot = Snapshot::new(BODY.to_owned()).await.unwrap();
        let wrong = snapshot.url().join("/wrong/vod.m3u8").unwrap();
        let response = http().get(wrong).send().await.unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        assert!(!response.text().await.unwrap().contains("fixture-token"));
        let response = http().post(snapshot.url().clone()).send().await.unwrap();
        assert_eq!(response.status(), StatusCode::METHOD_NOT_ALLOWED);
        assert!(!response.text().await.unwrap().contains("fixture-token"));
    }

    #[tokio::test]
    async fn dropping_the_snapshot_closes_its_listener() {
        let snapshot = Snapshot::new(BODY.to_owned()).await.unwrap();
        let url = snapshot.url().clone();
        drop(snapshot);
        let http = http();
        tokio::time::timeout(Duration::from_secs(1), async {
            while http.get(url.clone()).send().await.is_ok() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("dropping playback must close its listener");
    }

    #[tokio::test]
    async fn the_debug_output_leaves_out_the_capability() {
        let snapshot = Snapshot::new(BODY.to_owned()).await.unwrap();
        let shown = format!("{snapshot:?}");
        let capability = snapshot
            .url()
            .path_segments()
            .unwrap()
            .next()
            .unwrap()
            .to_owned();
        assert!(!shown.contains(&capability), "{shown}");
    }
}
