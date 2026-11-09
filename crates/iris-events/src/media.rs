//! ONVIF Media service lookups for the API and the Add camera flow: where a
//! camera serves snapshots ([`snapshot_url`]) and the RTSP URLs of its
//! profiles ([`stream_urls`]).
//!
//! Both use the SOAP transport, clock handling, and call deadline of the
//! pull-point subscriptions in [`onvif`](crate::onvif). Every URL a camera
//! advertises is moved onto the configured camera before it is used or
//! returned, and only [`MediaUrlError::kind`] reaches logs and API answers.

use std::sync::Arc;
use std::time::Duration;

use oxvif::OnvifClient;
use thiserror::Error;
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;
use tracing::debug;
use url::Url;

use crate::endpoint::CameraEndpoint;
use crate::onvif::{SessionError, bounded, device_client, measure_clock_offset};
use crate::text;
use crate::transport::{MAX_ADVERTISED_URL_LEN, SoapTransport};

/// How many media profiles [`stream_urls`] asks about.
pub const MAX_STREAM_PROFILES: usize = 8;
/// The deadline of one whole lookup, over all of its calls and the retry
/// with host time.
pub const MEDIA_LOOKUP_DEADLINE: Duration = Duration::from_secs(60);

/// Why a camera's ONVIF Media service could not name a snapshot or stream URL.
///
/// `Display` may carry camera text through [`MediaUrlError::Session`]; logs
/// and API answers use [`MediaUrlError::kind`] instead.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum MediaUrlError {
    /// An ONVIF call failed, timed out, or the HTTP client could not be built.
    #[error(transparent)]
    Session(#[from] SessionError),
    /// The camera's capabilities name no Media service.
    #[error("camera advertises no media service")]
    NoMediaService,
    /// The Media service lists no profile.
    #[error("camera has no media profile")]
    NoProfile,
    /// An advertised URL has another scheme than expected, carries userinfo or
    /// a fragment, is longer than [`MAX_ADVERTISED_URL_LEN`], or could not be
    /// rebased onto the configured camera.
    #[error("camera advertises an unusable media URL")]
    AdvertisedUrl,
}

impl MediaUrlError {
    /// The kind of failure, without any camera-supplied text, for logs and
    /// API answers.
    #[must_use]
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Session(_) => "ONVIF call failed",
            Self::NoMediaService => "no media service",
            Self::NoProfile => "no media profile",
            Self::AdvertisedUrl => "unusable advertised URL",
        }
    }
}

/// One media profile's RTSP stream, as [`stream_urls`] resolves it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProfileStream {
    /// The profile token, as the camera names it: at most
    /// [`MAX_TEXT_CHARS`](crate::discovery::MAX_TEXT_CHARS) characters, control
    /// characters removed.
    pub token: String,
    /// The profile name, bounded the same way.
    pub name: String,
    /// The stream URL on the configured host, without userinfo.
    pub url: Url,
}

/// Resolves where `endpoint`'s ONVIF Media service serves snapshots: the
/// `GetSnapshotUri` of its first profile. The advertised URL is rebased onto
/// the configured host, port, and scheme, so credentials reach only the
/// configured camera; each call has the 20 s call deadline, the whole lookup
/// [`MEDIA_LOOKUP_DEADLINE`], and a camera whose clock makes WS-Security fail
/// is retried with host time, as event subscriptions are.
///
/// # Errors
///
/// [`MediaUrlError::Session`] when a call fails or a deadline passes, or when
/// the endpoint's host is not a bare name or address;
/// [`MediaUrlError::NoMediaService`] and [`MediaUrlError::NoProfile`] when the
/// camera names none; [`MediaUrlError::AdvertisedUrl`] when the advertised
/// snapshot URL is unusable.
pub async fn snapshot_url(endpoint: &CameraEndpoint) -> Result<Url, MediaUrlError> {
    let policy = Arc::new(SoapTransport::new(endpoint)?);
    let client = device_client(endpoint, policy.clone());
    bounded_lookup(snapshot_lookup(endpoint, client, &policy)).await
}

/// Resolves the RTSP URLs of `endpoint`'s media profiles, the first
/// [`MAX_STREAM_PROFILES`] in the camera's order: `GetStreamUri` for RTP over
/// RTSP, each URL moved onto the configured host with its advertised port,
/// path, and query, so a camera cannot point the recorder elsewhere. A profile
/// whose URL cannot be resolved is skipped. Calls and clock handling are as
/// for [`snapshot_url`].
///
/// # Errors
///
/// As [`snapshot_url`]: [`MediaUrlError::NoProfile`] when the camera lists no
/// profile, and when no profile resolves, the first profile's error
/// ([`MediaUrlError::AdvertisedUrl`] or [`MediaUrlError::Session`]).
pub async fn stream_urls(endpoint: &CameraEndpoint) -> Result<Vec<ProfileStream>, MediaUrlError> {
    let policy = Arc::new(SoapTransport::new(endpoint)?);
    let client = device_client(endpoint, policy);
    bounded_lookup(stream_lookup(endpoint, client)).await
}

/// Runs `lookup` under [`MEDIA_LOOKUP_DEADLINE`].
async fn bounded_lookup<T>(
    lookup: impl Future<Output = Result<T, MediaUrlError>>,
) -> Result<T, MediaUrlError> {
    match timeout(MEDIA_LOOKUP_DEADLINE, lookup).await {
        Ok(result) => result,
        Err(_) => Err(MediaUrlError::Session(SessionError::Timeout)),
    }
}

/// The snapshot lookup behind [`snapshot_url`], with `client` speaking to the
/// camera and `policy` rebasing what it advertises.
async fn snapshot_lookup(
    endpoint: &CameraEndpoint,
    client: OnvifClient,
    policy: &SoapTransport,
) -> Result<Url, MediaUrlError> {
    let measured = measure_clock_offset(&CancellationToken::new(), &client).await?;
    with_clock_retry(
        endpoint,
        client,
        measured,
        "snapshot URL",
        move |client| async move { resolve_snapshot(client, policy).await },
    )
    .await
}

/// The stream lookup behind [`stream_urls`], with `client` speaking to the
/// camera.
async fn stream_lookup(
    endpoint: &CameraEndpoint,
    client: OnvifClient,
) -> Result<Vec<ProfileStream>, MediaUrlError> {
    let measured = measure_clock_offset(&CancellationToken::new(), &client).await?;
    let host = endpoint.host.as_str();
    with_clock_retry(
        endpoint,
        client,
        measured,
        "stream URLs",
        move |client| async move { resolve_streams(client, host).await },
    )
    .await
}

/// Runs `resolve` with the camera's clock, then once more with host time when
/// a clock-related WS-Security refusal suggests the camera misreports UTC.
async fn with_clock_retry<T, Fut>(
    endpoint: &CameraEndpoint,
    client: OnvifClient,
    measured: i64,
    lookup: &'static str,
    resolve: impl Fn(OnvifClient) -> Fut,
) -> Result<T, MediaUrlError>
where
    Fut: Future<Output = Result<T, MediaUrlError>>,
{
    match resolve(client.clone().with_utc_offset(measured)).await {
        Err(MediaUrlError::Session(error)) if error.retries_with_host_time(measured) => {
            debug!(
                camera = %endpoint.camera,
                error = %iris_core::text::LogSafe(&error),
                measured,
                lookup,
                "resolving with the camera's clock failed; retrying with host time"
            );
            resolve(client.with_utc_offset(0)).await
        }
        resolved => resolved,
    }
}

async fn media_service_url(client: &OnvifClient) -> Result<String, MediaUrlError> {
    let capabilities = bounded(&CancellationToken::new(), client.get_capabilities()).await?;
    capabilities.media.url.ok_or(MediaUrlError::NoMediaService)
}

async fn resolve_snapshot(
    client: OnvifClient,
    policy: &SoapTransport,
) -> Result<Url, MediaUrlError> {
    let never = CancellationToken::new();
    let media_url = media_service_url(&client).await?;
    let profiles = bounded(&never, client.get_profiles(&media_url)).await?;
    let profile = profiles
        .into_iter()
        .next()
        .ok_or(MediaUrlError::NoProfile)?;
    let snapshot = bounded(&never, client.get_snapshot_uri(&media_url, &profile.token)).await?;
    policy
        .rebase_url(&snapshot.uri)
        .map_err(|_| MediaUrlError::AdvertisedUrl)
}

async fn resolve_streams(
    client: OnvifClient,
    host: &str,
) -> Result<Vec<ProfileStream>, MediaUrlError> {
    let never = CancellationToken::new();
    let media_url = media_service_url(&client).await?;
    let profiles = bounded(&never, client.get_profiles(&media_url)).await?;
    if profiles.is_empty() {
        return Err(MediaUrlError::NoProfile);
    }
    let mut streams = Vec::new();
    let mut first_failure = None;
    for profile in profiles.into_iter().take(MAX_STREAM_PROFILES) {
        let resolved = bounded(&never, client.get_stream_uri(&media_url, &profile.token))
            .await
            .map_err(MediaUrlError::from)
            .and_then(|stream| rebase_rtsp(host, &stream.uri).ok_or(MediaUrlError::AdvertisedUrl));
        match resolved {
            Ok(url) => streams.push(ProfileStream {
                token: text::bounded_text(&profile.token),
                name: text::bounded_text(&profile.name),
                url,
            }),
            Err(error) => {
                first_failure.get_or_insert(error);
            }
        }
    }
    if streams.is_empty() {
        return Err(first_failure.unwrap_or(MediaUrlError::NoProfile));
    }
    Ok(streams)
}

/// Moves an advertised `rtsp://` URL onto `host`, keeping its port, path, and
/// query; refuses other schemes, userinfo, fragments, and URLs longer than
/// [`MAX_ADVERTISED_URL_LEN`].
fn rebase_rtsp(host: &str, candidate: &str) -> Option<Url> {
    if candidate.len() > MAX_ADVERTISED_URL_LEN {
        return None;
    }
    let mut url = Url::parse(candidate).ok()?;
    if url.scheme() != "rtsp"
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
        || url.host_str().is_none_or(str::is_empty)
    {
        return None;
    }
    let host = if host.contains(':') {
        format!("[{host}]")
    } else {
        host.to_owned()
    };
    url.set_host(Some(&host)).ok()?;
    Some(url)
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use async_trait::async_trait;
    use oxvif::transport::{Transport, TransportError};
    use tokio::net::TcpListener;
    use tokio::sync::mpsc;
    use tokio::time::Instant;

    use super::*;
    use crate::loopback;

    /// How a mocked Media service misbehaves.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum MediaMode {
        /// The capabilities name no Media service.
        NoMediaService,
        /// `GetProfiles` lists nothing.
        NoProfiles,
        /// Every stream URL has the wrong scheme.
        AllUnusable,
        /// The first profile's URL has the wrong scheme, the second is usable.
        FirstUnusable,
        /// Twelve profiles.
        ManyProfiles,
        /// The first `GetProfiles` answers a clock fault, later ones succeed.
        ClockFaultOnce,
        /// The snapshot URL carries userinfo.
        SnapshotUserinfo,
        /// Every call takes 19 s, within the per-call deadline.
        Slow,
    }

    struct MediaTransport {
        mode: MediaMode,
        profile_calls: AtomicUsize,
        stream_calls: AtomicUsize,
    }

    impl MediaTransport {
        fn new(mode: MediaMode) -> Arc<Self> {
            Arc::new(Self {
                mode,
                profile_calls: AtomicUsize::new(0),
                stream_calls: AtomicUsize::new(0),
            })
        }
    }

    #[async_trait]
    impl Transport for MediaTransport {
        async fn soap_post(
            &self,
            _url: &str,
            action: &str,
            body: String,
        ) -> Result<String, TransportError> {
            let action = action.to_ascii_lowercase();
            let body = body.to_ascii_lowercase();
            if self.mode == MediaMode::Slow {
                tokio::time::sleep(Duration::from_secs(19)).await;
            }
            if action.contains("getcapabilities") && self.mode == MediaMode::NoMediaService {
                return Ok(r#"<s:Envelope xmlns:s="http://www.w3.org/2003/05/soap-envelope" xmlns:tds="http://www.onvif.org/ver10/device/wsdl" xmlns:tt="http://www.onvif.org/ver10/schema"><s:Body><tds:GetCapabilitiesResponse><tds:Capabilities><tt:Events><tt:XAddr>http://203.0.113.9/onvif/events</tt:XAddr></tt:Events></tds:Capabilities></tds:GetCapabilitiesResponse></s:Body></s:Envelope>"#.to_owned());
            }
            if action.contains("getprofiles") {
                let call = self.profile_calls.fetch_add(1, Ordering::SeqCst) + 1;
                return Ok(match self.mode {
                    MediaMode::NoProfiles => loopback::profiles_reply(&[]),
                    MediaMode::ManyProfiles => {
                        let names: Vec<String> = (1..=12).map(|n| format!("profile-{n}")).collect();
                        let profiles: Vec<(&str, &str)> =
                            names.iter().map(|name| (name.as_str(), "P")).collect();
                        loopback::profiles_reply(&profiles)
                    }
                    MediaMode::ClockFaultOnce if call == 1 => loopback::clock_fault_reply(),
                    _ => loopback::reply(&action, &body, "http://203.0.113.9"),
                });
            }
            if action.contains("getstreamuri") {
                self.stream_calls.fetch_add(1, Ordering::SeqCst);
                let unusable = loopback::stream_uri_reply("http://203.0.113.9/not-rtsp");
                return Ok(match self.mode {
                    MediaMode::AllUnusable => unusable,
                    MediaMode::FirstUnusable if body.contains("profile-1<") => unusable,
                    _ => loopback::reply(&action, &body, "http://203.0.113.9"),
                });
            }
            if action.contains("getsnapshoturi") && self.mode == MediaMode::SnapshotUserinfo {
                return Ok(loopback::snapshot_uri_reply(
                    "http://user:secret@203.0.113.9/snap.jpg",
                ));
            }
            Ok(loopback::reply(&action, &body, "http://203.0.113.9"))
        }
    }

    fn mocked(mode: MediaMode) -> (CameraEndpoint, OnvifClient, Arc<MediaTransport>) {
        let endpoint = loopback::endpoint("127.0.0.1", 8080);
        let transport = MediaTransport::new(mode);
        let client = device_client(&endpoint, transport.clone());
        (endpoint, client, transport)
    }

    fn urls(streams: &[ProfileStream]) -> Vec<&str> {
        streams.iter().map(|stream| stream.url.as_str()).collect()
    }

    /// Stream URLs come from every profile's `GetStreamUri`, moved onto the
    /// configured host with the advertised port and path.
    #[tokio::test]
    async fn stream_urls_cover_every_profile_on_the_configured_host() {
        let camera = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = camera.local_addr().unwrap().port();
        let (requests_tx, mut requests) = mpsc::unbounded_channel();
        let advertised = "http://203.0.113.9:8080".to_owned();
        let camera_task = tokio::spawn(loopback::camera(camera, advertised, requests_tx));
        let endpoint = loopback::endpoint("127.0.0.1", port);
        let streams = timeout(Duration::from_secs(10), stream_urls(&endpoint))
            .await
            .expect("the lookup ends before the deadline")
            .expect("the fixture answers every call");
        camera_task.abort();
        let summary: Vec<(&str, &str, &str)> = streams
            .iter()
            .map(|stream| {
                (
                    stream.token.as_str(),
                    stream.name.as_str(),
                    stream.url.as_str(),
                )
            })
            .collect();
        assert_eq!(
            summary,
            vec![
                ("profile-1", "Main", "rtsp://127.0.0.1:8554/live/profile-1"),
                ("profile-2", "Sub", "rtsp://127.0.0.1:8554/live/profile-2"),
            ]
        );
        let mut stream_calls = 0;
        while let Ok((_, action)) = requests.try_recv() {
            stream_calls += usize::from(action.contains("getstreamuri"));
        }
        assert_eq!(stream_calls, 2, "one GetStreamUri per profile");
    }

    #[test]
    fn rtsp_urls_are_rebased_onto_the_configured_host_only() {
        assert_eq!(
            rebase_rtsp("192.0.2.7", "rtsp://203.0.113.9:8554/live/1?x=1")
                .unwrap()
                .as_str(),
            "rtsp://192.0.2.7:8554/live/1?x=1"
        );
        assert_eq!(
            rebase_rtsp("2001:db8::7", "rtsp://203.0.113.9/live")
                .unwrap()
                .as_str(),
            "rtsp://[2001:db8::7]/live"
        );
        for bad in [
            "http://203.0.113.9/live",
            "rtsp://user:pw@203.0.113.9/live",
            "rtsp://203.0.113.9/live#f",
            "rtsp:live",
            "not a url",
        ] {
            assert!(rebase_rtsp("192.0.2.7", bad).is_none(), "{bad}");
        }
    }

    #[test]
    fn an_advertised_stream_url_is_bounded_in_length() {
        let prefix = "rtsp://203.0.113.9/";
        let exact = format!(
            "{prefix}{}",
            "p".repeat(MAX_ADVERTISED_URL_LEN - prefix.len())
        );
        assert_eq!(exact.len(), MAX_ADVERTISED_URL_LEN);
        assert!(rebase_rtsp("192.0.2.7", &exact).is_some());
        assert!(rebase_rtsp("192.0.2.7", &format!("{exact}p")).is_none());
    }

    /// The snapshot URL comes from the Media service's first profile, and
    /// every advertised URL is rebased onto the configured camera.
    #[tokio::test]
    async fn snapshot_url_follows_the_media_service_and_rebases_advertised_urls() {
        let camera = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = camera.local_addr().unwrap().port();
        let (requests_tx, mut requests) = mpsc::unbounded_channel();
        let advertised = "http://203.0.113.9:8080".to_owned();
        let camera_task = tokio::spawn(loopback::camera(camera, advertised, requests_tx));
        let endpoint = loopback::endpoint("127.0.0.1", port);
        let url = timeout(Duration::from_secs(10), snapshot_url(&endpoint))
            .await
            .expect("the resolution ends before the deadline")
            .expect("the fixture answers every call");
        assert_eq!(
            url.as_str(),
            format!("http://127.0.0.1:{port}/snap.jpg?channel=1")
        );
        camera_task.abort();
        let mut served = Vec::new();
        while let Ok(request) = requests.try_recv() {
            served.push(request);
        }
        let actions: Vec<&str> = served
            .iter()
            .map(|(_, action)| {
                [
                    "getsystemdateandtime",
                    "getcapabilities",
                    "getprofiles",
                    "getsnapshoturi",
                ]
                .into_iter()
                .find(|name| action.contains(name))
                .unwrap_or("other")
            })
            .collect();
        assert_eq!(
            actions,
            [
                "getsystemdateandtime",
                "getcapabilities",
                "getprofiles",
                "getsnapshoturi"
            ]
        );
        assert!(
            served
                .iter()
                .skip(2)
                .all(|(path, _)| path.starts_with("/onvif/media")),
            "media calls reach the configured camera at the advertised path: {served:?}"
        );
    }

    #[tokio::test]
    async fn media_lookups_report_a_missing_media_service() {
        let (endpoint, client, _) = mocked(MediaMode::NoMediaService);
        let error = stream_lookup(&endpoint, client).await.unwrap_err();
        assert!(matches!(error, MediaUrlError::NoMediaService), "{error:?}");
        assert_eq!(error.kind(), "no media service");
    }

    #[tokio::test]
    async fn stream_urls_report_no_profile_for_an_empty_list() {
        let (endpoint, client, _) = mocked(MediaMode::NoProfiles);
        let error = stream_lookup(&endpoint, client).await.unwrap_err();
        assert!(matches!(error, MediaUrlError::NoProfile), "{error:?}");
    }

    #[tokio::test]
    async fn stream_urls_skip_a_profile_whose_url_is_unusable() {
        let (endpoint, client, transport) = mocked(MediaMode::FirstUnusable);
        let streams = stream_lookup(&endpoint, client).await.unwrap();
        assert_eq!(urls(&streams), ["rtsp://127.0.0.1:8554/live/profile-2"]);
        assert_eq!(transport.stream_calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn stream_urls_fail_with_the_first_error_when_no_profile_resolves() {
        let (endpoint, client, _) = mocked(MediaMode::AllUnusable);
        let error = stream_lookup(&endpoint, client).await.unwrap_err();
        assert!(matches!(error, MediaUrlError::AdvertisedUrl), "{error:?}");
        assert_eq!(error.kind(), "unusable advertised URL");
    }

    #[tokio::test]
    async fn stream_urls_ask_about_at_most_eight_profiles() {
        let (endpoint, client, transport) = mocked(MediaMode::ManyProfiles);
        let streams = stream_lookup(&endpoint, client).await.unwrap();
        assert_eq!(streams.len(), MAX_STREAM_PROFILES);
        assert_eq!(
            transport.stream_calls.load(Ordering::SeqCst),
            MAX_STREAM_PROFILES
        );
        assert_eq!(streams[7].token, "profile-8");
    }

    #[tokio::test]
    async fn media_lookups_retry_once_with_host_time_after_a_clock_fault() {
        let (endpoint, client, transport) = mocked(MediaMode::ClockFaultOnce);
        let streams = stream_lookup(&endpoint, client).await.unwrap();
        assert_eq!(streams.len(), 2);
        assert_eq!(transport.profile_calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn snapshot_url_rejects_an_advertised_url_with_userinfo() {
        let (endpoint, client, _) = mocked(MediaMode::SnapshotUserinfo);
        let policy = SoapTransport::new(&endpoint).unwrap();
        let error = snapshot_lookup(&endpoint, client, &policy)
            .await
            .unwrap_err();
        assert!(matches!(error, MediaUrlError::AdvertisedUrl), "{error:?}");
    }

    #[tokio::test(start_paused = true)]
    async fn media_lookups_end_at_the_lookup_deadline() {
        let (endpoint, client, _) = mocked(MediaMode::Slow);
        let started = Instant::now();
        let error = bounded_lookup(stream_lookup(&endpoint, client))
            .await
            .unwrap_err();
        assert!(
            matches!(error, MediaUrlError::Session(SessionError::Timeout)),
            "{error:?}"
        );
        assert_eq!(started.elapsed(), MEDIA_LOOKUP_DEADLINE);

        let (endpoint, client, _) = mocked(MediaMode::Slow);
        let policy = SoapTransport::new(&endpoint).unwrap();
        let started = Instant::now();
        let error = bounded_lookup(snapshot_lookup(&endpoint, client, &policy))
            .await
            .unwrap_err();
        assert!(
            matches!(error, MediaUrlError::Session(SessionError::Timeout)),
            "{error:?}"
        );
        assert_eq!(started.elapsed(), MEDIA_LOOKUP_DEADLINE);
    }

    #[tokio::test]
    async fn an_unusable_host_fails_the_lookup_before_any_call() {
        let endpoint = loopback::endpoint("cam host", 8080);
        let error = stream_urls(&endpoint).await.unwrap_err();
        assert!(
            matches!(error, MediaUrlError::Session(SessionError::InvalidEndpoint)),
            "{error:?}"
        );
    }
}
