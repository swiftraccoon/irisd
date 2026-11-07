//! A private HTTP endpoint for one immutable VOD playlist. Only the manifest is
//! local; GStreamer still fetches media from the daemon with its original URIs.

use std::fmt::Write as _;

use anyhow::Context;
use axum::Router;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::header;
use axum::routing::get;
use tokio::net::TcpListener;
use tokio::task::JoinHandle;
use url::Url;

/// Serves the same playlist on every fetch until the playback owner drops it.
///
/// The loopback route has a 256-bit OS-random capability because its body can
/// contain authenticated media URLs. Neither the route nor the body is logged.
pub struct Snapshot {
    url: Url,
    server: JoinHandle<()>,
}

impl Snapshot {
    /// Starts a loopback listener on the calling Tokio runtime for this body.
    pub async fn new(body: String) -> anyhow::Result<Self> {
        let mut random = [0; 32];
        getrandom::fill(&mut random).context("creating a private playback route")?;
        let mut capability = String::with_capacity(64);
        for byte in random {
            write!(capability, "{byte:02x}").expect("writing to a String cannot fail");
        }
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .context("binding the playback snapshot listener")?;
        let address = listener
            .local_addr()
            .context("reading the playback address")?;
        let path = format!("/{capability}/vod.m3u8");
        let url = Url::parse(&format!("http://{address}{path}"))
            .context("building the playback snapshot URL")?;
        let app = Router::new()
            .route(&path, get(playlist))
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

async fn playlist(State(body): State<Bytes>) -> ([(header::HeaderName, &'static str); 2], Bytes) {
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
        // Cancellation also drops the listener and active requests; never join
        // a runtime task from GTK's main loop.
        self.server.abort();
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    #[tokio::test]
    async fn snapshot_is_private_immutable_and_released_on_drop() {
        let body = "#EXTM3U\nhttp://192.0.2.1/file.mp4?token=fixture-token\n";
        let snapshot = Snapshot::new(body.to_owned()).await.unwrap();
        let url = snapshot.url().clone();
        let http = reqwest::Client::new();
        for _ in 0..2 {
            let response = http.get(url.clone()).send().await.unwrap();
            assert_eq!(response.status(), reqwest::StatusCode::OK);
            assert_eq!(response.text().await.unwrap(), body);
        }
        let wrong = url.join("/wrong/vod.m3u8").unwrap();
        let response = http.get(wrong).send().await.unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::NOT_FOUND);
        assert!(!response.text().await.unwrap().contains("fixture-token"));
        let response = http.post(url.clone()).send().await.unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::METHOD_NOT_ALLOWED);
        assert!(!response.text().await.unwrap().contains("fixture-token"));
        drop(snapshot);
        tokio::time::timeout(Duration::from_secs(1), async {
            while http.get(url.clone()).send().await.is_ok() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("dropping playback must close its listener");
    }
}
