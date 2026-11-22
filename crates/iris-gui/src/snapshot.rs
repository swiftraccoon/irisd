//! A private HTTP endpoint for the immutable playlists of one playback
//! selection: the bodies of an [`iris_client::playback::PlaybackSnapshot`],
//! each under its name. They are a VOD playlist alone, or a video and an
//! audio playlist under a multivariant one. Only the manifests are local: a
//! body's init and media URIs are absolute URLs on the API origin
//! ([`iris_client::api::VodPlaylist::body`]), so GStreamer fetches media from
//! the daemon.

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
use iris_client::playback::PlaybackSnapshot;
use tokio::net::TcpListener;
use tokio::task::JoinHandle;
use url::Url;

/// Serves the same playlists on every fetch until the playback owner drops it.
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
    /// Starts a loopback listener on the calling Tokio runtime. It serves
    /// each body of `playlists`, moved out of it, under its name in one
    /// private directory, and nothing else. The snapshot's URL is that of
    /// the entry, [`PlaybackSnapshot::ENTRY`]. Every init and media URI in a
    /// media playlist must be absolute: a relative one would resolve against
    /// the loopback endpoint, which serves nothing but the playlists.
    ///
    /// # Errors
    ///
    /// - The OS random source cannot give the capability's 32 bytes.
    /// - No port of IPv4 loopback can be bound.
    /// - The listener cannot tell the address it is bound to.
    /// - The address and the capability make no URL.
    ///
    /// # Panics
    ///
    /// When it runs outside a Tokio runtime with its I/O driver enabled,
    /// which the listener and its server task need.
    pub async fn serve(playlists: PlaybackSnapshot) -> anyhow::Result<Self> {
        let audio = playlists.has_audio();
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
        let url = Url::parse(&format!(
            "http://{address}/{capability}/{}",
            PlaybackSnapshot::ENTRY
        ))
        .context("building the playback snapshot URL")?;
        let mut app = Router::new();
        for (name, body) in playlists {
            app = app.route(
                &format!("/{capability}/{name}"),
                routing::get(playlist).with_state(Bytes::from(body)),
            );
        }
        let server = tokio::spawn(async move {
            // A listener failure reaches GStreamer as a failed playlist request.
            let _ = axum::serve(listener, app).await;
        });
        Ok(Self { url, server, audio })
    }

    /// Whether the snapshot plays with audio ([`PlaybackSnapshot::has_audio`]).
    #[must_use]
    pub fn has_audio(&self) -> bool {
        self.audio
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

    use iris_client::api::{Answer, VodPlaylist, Window, answer};
    use iris_client::origin::{ApiOrigin, PlainHttp};
    use iris_client::playback::PlaybackSelection;
    use iris_core::time::WallUs;
    use reqwest::StatusCode;

    use super::*;

    impl Snapshot {
        /// Serves the VOD playlist `video` alone, as a window without audio
        /// is served. Its references are absolute URLs on `origin`.
        pub(crate) async fn serve_video(origin: &ApiOrigin, video: &str) -> anyhow::Result<Self> {
            Self::serve(test_snapshot(origin, video, None)).await
        }

        /// Serves `video` with `audio` under a multivariant playlist that
        /// names the two, as a main stream with recorded audio is served.
        /// The references of both are absolute URLs on `origin`.
        pub(crate) async fn serve_video_with_audio(
            origin: &ApiOrigin,
            video: &str,
            audio: &str,
        ) -> anyhow::Result<Self> {
            Self::serve(test_snapshot(origin, video, Some(audio))).await
        }
    }

    /// The snapshot of a selection of `video`, with `audio` when there is
    /// one, for the tests that serve bodies they wrote. Each body passes
    /// the check that `iris-client` makes of a fetched VOD playlist, on
    /// `origin` and for the window of the first second. The check gives it
    /// back unchanged, since its references are absolute URLs on `origin`.
    fn test_snapshot(origin: &ApiOrigin, video: &str, audio: Option<&str>) -> PlaybackSnapshot {
        let window = Window::try_new(WallUs::new(0), WallUs::new(1_000_000)).unwrap();
        let url = origin
            .url()
            .join("/api/streams/fixture/main/hls/vod.m3u8")
            .unwrap();
        let checked = |body: &str| -> VodPlaylist {
            let received = Answer {
                status: 200,
                content_type: Some("application/vnd.apple.mpegurl"),
                final_url: &url,
                body: Some(body.as_bytes()),
            };
            answer::vod_playlist(origin, &received, window).unwrap()
        };
        PlaybackSelection::new(checked(video), audio.map(checked))
            .into_parts()
            .1
    }

    const BODY: &str = "#EXTM3U\n#IRIS-FRAGMENT:WALL-US=0,DURATION-TICKS=180000\n#EXTINF:2.00000,\n\
                        http://192.0.2.1/file.mp4?token=fixture-token\n";

    const VIDEO: &str = "#EXTM3U\n#IRIS-FRAGMENT:WALL-US=0,DURATION-TICKS=180000\n\
                         #EXTINF:2.00000,\nhttp://192.0.2.1/v.mp4?token=video-token\n";

    const AUDIO: &str = "#EXTM3U\n#IRIS-FRAGMENT:WALL-US=0,DURATION-TICKS=180000\n\
                         #EXTINF:2.00000,\nhttp://192.0.2.1/a.mp4?token=audio-token\n";

    /// The origin the bodies' references are on.
    fn origin() -> ApiOrigin {
        ApiOrigin::parse("http://192.0.2.1", PlainHttp::Anywhere).unwrap()
    }

    /// A client that never consults the proxy environment, so the loopback
    /// requests reach the snapshot.
    fn http() -> reqwest::Client {
        reqwest::Client::builder().no_proxy().build().unwrap()
    }

    /// The status and the body of a GET of `url`.
    async fn fetch(url: Url) -> (StatusCode, String) {
        let response = http().get(url).send().await.unwrap();
        (response.status(), response.text().await.unwrap())
    }

    #[tokio::test]
    async fn the_snapshot_serves_the_same_body_on_every_fetch() {
        let snapshot = Snapshot::serve_video(&origin(), BODY).await.unwrap();
        for _ in 0..2 {
            let response = http().get(snapshot.url().clone()).send().await.unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            assert_eq!(response.text().await.unwrap(), BODY);
        }
    }

    /// The status and the body of the answer to a GET of another path than
    /// the snapshot's.
    async fn answer_to_another_path() -> (StatusCode, String) {
        let snapshot = Snapshot::serve_video(&origin(), BODY).await.unwrap();
        fetch(snapshot.url().join("/wrong/vod.m3u8").unwrap()).await
    }

    /// The status and the body of the answer to a POST of the snapshot's
    /// URL.
    async fn answer_to_a_post() -> (StatusCode, String) {
        let snapshot = Snapshot::serve_video(&origin(), BODY).await.unwrap();
        let response = http().post(snapshot.url().clone()).send().await.unwrap();
        (response.status(), response.text().await.unwrap())
    }

    #[tokio::test]
    async fn the_snapshot_answers_another_path_with_not_found() {
        let (status, _) = answer_to_another_path().await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn the_answer_to_another_path_carries_no_token() {
        let (_, body) = answer_to_another_path().await;
        assert!(!body.contains("fixture-token"), "{body}");
    }

    #[tokio::test]
    async fn the_snapshot_answers_a_post_with_method_not_allowed() {
        let (status, _) = answer_to_a_post().await;
        assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED);
    }

    #[tokio::test]
    async fn the_answer_to_a_post_carries_no_token() {
        let (_, body) = answer_to_a_post().await;
        assert!(!body.contains("fixture-token"), "{body}");
    }

    #[tokio::test]
    async fn dropping_the_snapshot_closes_its_listener() {
        let snapshot = Snapshot::serve_video(&origin(), BODY).await.unwrap();
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
        let snapshot = Snapshot::serve_video(&origin(), BODY).await.unwrap();
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

    /// With audio, the snapshot's URL is a multivariant playlist that names
    /// the audio playlist and then the video playlist.
    #[tokio::test]
    async fn a_snapshot_with_audio_serves_a_multivariant_playlist_of_both() {
        let snapshot = Snapshot::serve_video_with_audio(&origin(), VIDEO, AUDIO)
            .await
            .unwrap();
        let (status, multivariant) = fetch(snapshot.url().clone()).await;
        assert_eq!(status, StatusCode::OK);
        assert!(
            multivariant.contains("URI=\"audio.m3u8\""),
            "{multivariant}"
        );
        assert!(multivariant.ends_with("video.m3u8\n"), "{multivariant}");
    }

    #[tokio::test]
    async fn the_multivariant_playlist_carries_no_token() {
        let snapshot = Snapshot::serve_video_with_audio(&origin(), VIDEO, AUDIO)
            .await
            .unwrap();
        let (_, multivariant) = fetch(snapshot.url().clone()).await;
        assert!(!multivariant.contains("token"), "{multivariant}");
    }

    /// With audio, the two media playlists the multivariant one names are
    /// served beside it, each as written.
    #[tokio::test]
    async fn a_snapshot_with_audio_serves_both_media_playlists() {
        let snapshot = Snapshot::serve_video_with_audio(&origin(), VIDEO, AUDIO)
            .await
            .unwrap();
        for (name, body) in [("video.m3u8", VIDEO), ("audio.m3u8", AUDIO)] {
            let (status, text) = fetch(snapshot.url().join(name).unwrap()).await;
            assert_eq!((status, text.as_str()), (StatusCode::OK, body), "{name}");
        }
    }

    /// The status and the body of the answer to a name the selection does not
    /// hold, from a snapshot with audio.
    async fn answer_to_another_name() -> (StatusCode, String) {
        let snapshot = Snapshot::serve_video_with_audio(&origin(), VIDEO, AUDIO)
            .await
            .unwrap();
        fetch(snapshot.url().join("other.m3u8").unwrap()).await
    }

    #[tokio::test]
    async fn a_snapshot_with_audio_serves_no_other_name() {
        let (status, _) = answer_to_another_name().await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn the_answer_to_another_name_carries_no_token() {
        let (_, body) = answer_to_another_name().await;
        assert!(!body.contains("token"), "{body}");
    }

    #[tokio::test]
    async fn a_video_only_snapshot_serves_no_audio_playlist() {
        let alone = Snapshot::serve_video(&origin(), VIDEO).await.unwrap();
        let (status, _) = fetch(alone.url().join("audio.m3u8").unwrap()).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }
}
