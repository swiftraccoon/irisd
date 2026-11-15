//! A private HTTP endpoint for one immutable VOD playlist, or for a video and
//! an audio playlist under a multivariant one. Only the manifests are local:
//! a body's init and media URIs are absolute URLs on the API origin
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

use crate::vod;

/// Serves the same playlist on every fetch until the playback owner drops it.
///
/// The loopback route has a 256-bit OS-random capability because its body can
/// contain authenticated media URLs. Neither the route nor the body is logged,
/// and the `Debug` output names the port alone.
pub struct Snapshot {
    url: Url,
    server: JoinHandle<()>,
    /// Whether the snapshot names an audio playlist beside the video's.
    audio: bool,
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
        Self::serve(body, Vec::new()).await
    }

    /// Starts a listener for a video playlist played with an audio playlist:
    /// the snapshot's URL is a multivariant playlist that names the two
    /// bodies beside it ([`vod::multivariant_playlist`]). Every URI in either
    /// body must be absolute, as for [`Snapshot::new`].
    pub async fn with_audio(video: String, audio: String) -> anyhow::Result<Self> {
        let multivariant = vod::multivariant_playlist(&video, &audio);
        let mut snapshot = Self::serve(
            multivariant,
            vec![(vod::VIDEO_PLAYLIST, video), (vod::AUDIO_PLAYLIST, audio)],
        )
        .await?;
        snapshot.audio = true;
        Ok(snapshot)
    }

    /// Whether the snapshot plays with audio ([`Snapshot::with_audio`]).
    pub fn has_audio(&self) -> bool {
        self.audio
    }

    /// Serves `body` at the snapshot's URL and each of `beside` under its
    /// name in the same private directory.
    async fn serve(body: String, beside: Vec<(&'static str, String)>) -> anyhow::Result<Self> {
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
        let mut app =
            Router::new().route(&path, routing::get(playlist).with_state(Bytes::from(body)));
        for (name, body) in beside {
            app = app.route(
                &format!("/{capability}/{name}"),
                routing::get(playlist).with_state(Bytes::from(body)),
            );
        }
        let server = tokio::spawn(async move {
            // A listener failure reaches GStreamer as a failed playlist request.
            let _ = axum::serve(listener, app).await;
        });
        Ok(Self {
            url,
            server,
            audio: false,
        })
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

    /// With audio, the snapshot's URL is a multivariant playlist and the two
    /// media playlists it names are served beside it, and nothing else is.
    #[tokio::test]
    async fn a_snapshot_with_audio_serves_the_multivariant_playlist_and_both_bodies() {
        let video = "#EXTM3U\n#EXTINF:2.00000,\nhttp://192.0.2.1/v.mp4?token=video-token\n";
        let audio = "#EXTM3U\n#EXTINF:2.00000,\nhttp://192.0.2.1/a.mp4?token=audio-token\n";
        let snapshot = Snapshot::with_audio(video.to_owned(), audio.to_owned())
            .await
            .unwrap();
        let fetch = |url: Url| async move {
            let response = http().get(url).send().await.unwrap();
            (response.status(), response.text().await.unwrap())
        };
        let (status, multivariant) = fetch(snapshot.url().clone()).await;
        assert_eq!(status, StatusCode::OK);
        assert!(
            multivariant.contains("URI=\"audio.m3u8\""),
            "{multivariant}"
        );
        assert!(multivariant.ends_with("video.m3u8\n"), "{multivariant}");
        assert!(!multivariant.contains("token"), "{multivariant}");
        for (name, body) in [("video.m3u8", video), ("audio.m3u8", audio)] {
            let (status, text) = fetch(snapshot.url().join(name).unwrap()).await;
            assert_eq!((status, text.as_str()), (StatusCode::OK, body), "{name}");
        }
        let (status, text) = fetch(snapshot.url().join("other.m3u8").unwrap()).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert!(!text.contains("token"));
        // A video-only snapshot serves neither name.
        let alone = Snapshot::new(video.to_owned()).await.unwrap();
        let (status, _) = fetch(alone.url().join("audio.m3u8").unwrap()).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }
}
