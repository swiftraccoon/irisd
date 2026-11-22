//! The API origin a viewer reaches the daemon at, and where plain HTTP to it
//! is accepted. The request builders and the live URL set absolute paths
//! (`/api/...`) on the origin. Such a path would replace a path prefix and
//! send the bearer elsewhere on the host. So an origin is a scheme, a host,
//! and a port, without a path, a query, a fragment, or userinfo. A plain-HTTP
//! origin carries the bearer and the video unencrypted, so a [`PlainHttp`]
//! policy names the hosts it may be at.
//!
//! # Example
//!
//! ```
//! # use std::error::Error;
//! #
//! use iris_client::origin::{ApiOrigin, OriginError, PlainHttp};
//! use iris_core::id::{CameraId, StreamKind};
//!
//! let api = "https://nvr.example:8443";
//! let origin = ApiOrigin::parse(api, PlainHttp::LoopbackOnly)?;
//! assert_eq!(origin.url().as_str(), "https://nvr.example:8443/");
//! let camera = CameraId::try_new("cam1")?;
//! assert_eq!(
//!     origin.live_url(&camera, StreamKind::Sub).as_str(),
//!     "wss://nvr.example:8443/api/streams/cam1/sub/live"
//! );
//! let plain = "http://nvr.example:8554";
//! let refused = ApiOrigin::parse(plain, PlainHttp::LoopbackOnly);
//! assert!(matches!(refused, Err(OriginError::PlainHttp { .. })));
//! # Ok::<(), Box<dyn Error>>(())
//! ```

use std::fmt;
use std::net::Ipv4Addr;

use iris_core::id::{CameraId, StreamKind};
use thiserror::Error;
use url::{Host, ParseError, Url};

/// The hosts an `http` origin may name. An `https` origin is accepted at any
/// host under each policy. The example of the [module](self) refuses plain
/// HTTP under [`PlainHttp::LoopbackOnly`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum PlainHttp {
    /// The loopback alone: an IPv4 address in `127/8`, the IPv6 address
    /// `::1`, or the name `localhost`.
    LoopbackOnly,
    /// The loopback, and an IPv4 address in a private range of RFC 1918
    /// (`10/8`, `172.16/12`, `192.168/16`) or in the shared range of RFC 6598
    /// (`100.64/10`). Every other name and address is refused: any other IPv6
    /// address, a link-local one, and a documentation one among them.
    PrivateAddresses,
    /// Any name or address, so the bearer and the video may cross any network
    /// unencrypted; the Linux viewer's `--allow-insecure-http`.
    Anywhere,
}

impl PlainHttp {
    /// Whether an `http` origin may name `host`.
    fn admits(self, host: &Host<&str>) -> bool {
        match self {
            Self::LoopbackOnly => is_loopback(host),
            Self::PrivateAddresses => {
                is_loopback(host)
                    || matches!(host, Host::Ipv4(address) if is_private_or_shared(*address))
            }
            Self::Anywhere => true,
        }
    }
}

/// Whether `host` is on the loopback: an IPv4 or IPv6 loopback address, or
/// the name `localhost` in any letter case.
fn is_loopback(host: &Host<&str>) -> bool {
    match host {
        Host::Ipv4(address) => address.is_loopback(),
        Host::Ipv6(address) => address.is_loopback(),
        Host::Domain(name) => name.eq_ignore_ascii_case("localhost"),
    }
}

/// Whether `address` lies in a private range of RFC 1918 or in the shared
/// range of RFC 6598, `100.64/10`.
fn is_private_or_shared(address: Ipv4Addr) -> bool {
    let [first, second, ..] = address.octets();
    address.is_private() || (first == 100 && (64..128).contains(&second))
}

/// The origin of the irisd API: an `http` or `https` URL with a host and a
/// path of `/` alone, accepted under a [`PlainHttp`] policy. A viewer builds
/// its requests and its live feeds from it.
///
/// # Example
///
/// A viewer builds each request of the API on its origin:
///
/// ```
/// # use std::error::Error;
/// #
/// use iris_client::api::Window;
/// use iris_client::origin::{ApiOrigin, PlainHttp};
/// use iris_core::id::{CameraId, StreamKind};
/// use iris_core::time::WallUs;
/// use url::Url;
///
/// let url = Url::parse("https://nvr.example:8443")?;
/// let origin = ApiOrigin::try_new(url, PlainHttp::LoopbackOnly)?;
/// assert!(origin.is_encrypted());
/// assert_eq!(origin.cameras().url().path(), "/api/cameras");
///
/// let camera = CameraId::try_new("cam1")?;
/// let minute = Window::try_new(WallUs::new(0), WallUs::new(60_000_000))?;
/// let listed = origin.segments(&camera, StreamKind::Main, minute);
/// assert_eq!(listed.url().query(), Some("from=0&to=60000000"));
/// let playlist = origin.vod_playlist(&camera, StreamKind::Sub, minute);
/// let vod = "/api/streams/cam1/sub/hls/vod.m3u8";
/// assert_eq!(playlist.url().path(), vod);
/// // A main stream is exported with the camera's recorded audio.
/// let main = origin.export(&camera, StreamKind::Main, minute);
/// assert_eq!(main.url().path(), "/api/cameras/cam1/export.mp4");
/// let sub = origin.export(&camera, StreamKind::Sub, minute);
/// assert_eq!(sub.url().path(), "/api/streams/cam1/sub/export.mp4");
/// let removal = origin.remove_camera(&camera);
/// assert_eq!(removal.url().path(), "/api/cameras/cam1");
/// # Ok::<(), Box<dyn Error>>(())
/// ```
///
/// A builder refuses a request that breaks a rule before it is sent, and
/// encodes the body of one that sends a body:
///
/// ```
/// # use std::error::Error;
/// # use std::time::Duration;
/// #
/// use iris_client::api::{self, NewCamera, StreamUrlsRequest, Window};
/// use iris_client::origin::{ApiOrigin, PlainHttp};
/// use iris_core::id::{CameraId, StreamKind};
/// use iris_core::time::WallUs;
///
/// let address = "https://nvr.example:8443";
/// let origin = ApiOrigin::parse(address, PlainHttp::LoopbackOnly)?;
/// let camera = CameraId::try_new("cam1")?;
/// let minute = Window::try_new(WallUs::new(0), WallUs::new(60_000_000))?;
/// let Err(empty) = origin.events(&camera, minute, 0) else {
///     return Err("an empty page is refused".into());
/// };
/// assert_eq!(empty.to_string(), "invalid event page limit");
/// // A discovery names its wait in whole milliseconds.
/// let discovery = origin.discover(Duration::from_micros(3_000_999))?;
/// assert_eq!(discovery.url().query(), Some("wait_ms=3000"));
///
/// let to_add = NewCamera {
///     id: CameraId::try_new("cam2")?,
///     name: "Camera 2".to_owned(),
///     rtsp_url: "rtsp://viewer@192.0.2.10/stream".to_owned(),
///     sub_url: None,
///     username: "viewer".to_owned(),
///     password: String::new(),
///     record: vec![StreamKind::Main],
///     events: Vec::new(),
/// };
/// let Err(userinfo) = origin.add_camera(&to_add) else {
///     return Err("a URL with userinfo is refused".into());
/// };
/// let credentials = "the RTSP URL cannot carry credentials; \
///                    enter them as the username and password";
/// assert_eq!(userinfo.to_string(), credentials);
///
/// let lookup = StreamUrlsRequest {
///     host: "192.0.2.10".to_owned(),
///     http_port: 80,
///     https: false,
///     tls_verify: true,
///     username: "viewer".to_owned(),
///     password: String::new(),
/// };
/// let streams = origin.stream_urls(&lookup);
/// assert_eq!(streams.read_idle(), api::STREAM_LOOKUP_READ_IDLE);
/// # Ok::<(), Box<dyn Error>>(())
/// ```
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ApiOrigin(Url);

impl ApiOrigin {
    /// Parses `text` as an origin and checks it under `plain_http`, as
    /// [`ApiOrigin::try_new`] does. The example of the [module](self) parses
    /// one.
    ///
    /// # Errors
    ///
    /// [`OriginError::Parse`] when `text` is not a URL, and each refusal of
    /// [`ApiOrigin::try_new`].
    pub fn parse(text: &str, plain_http: PlainHttp) -> Result<Self, OriginError> {
        Self::try_new(Url::parse(text)?, plain_http)
    }

    /// Takes `url` as an origin when it passes every check, under
    /// `plain_http` for an `http` URL. The example on [`ApiOrigin`] makes
    /// one.
    ///
    /// # Errors
    ///
    /// The first check `url` fails, in this order. [`OriginError::Scheme`] is
    /// for a scheme other than `http` and `https`, and
    /// [`OriginError::Userinfo`] for a user name or a password.
    /// [`OriginError::NotAnOrigin`] is for a path other than `/`, a query, or a
    /// fragment. [`OriginError::PlainHttp`] is for an `http` URL whose host
    /// `plain_http` does not admit.
    ///
    /// # Panics
    ///
    /// Never: the `url` crate gives every `http` and `https` URL a host.
    pub fn try_new(url: Url, plain_http: PlainHttp) -> Result<Self, OriginError> {
        if !matches!(url.scheme(), "http" | "https") {
            return Err(OriginError::Scheme);
        }
        if !url.username().is_empty() || url.password().is_some() {
            return Err(OriginError::Userinfo);
        }
        if url.path() != "/" || url.query().is_some() || url.fragment().is_some() {
            return Err(OriginError::NotAnOrigin);
        }
        let host = url
            .host()
            .expect("the url crate gives every http and https URL a host");
        if url.scheme() == "http" && !plain_http.admits(&host) {
            let host = url.host_str().unwrap_or_default().to_owned();
            return Err(OriginError::PlainHttp { host });
        }
        Ok(Self(url))
    }

    /// The origin as a URL, whose path is `/`: an absolute path joined to it
    /// replaces nothing. The example of the [module](self) reads it.
    #[must_use]
    pub fn url(&self) -> &Url {
        &self.0
    }

    /// Whether the scheme is `https`, so that requests and live feeds travel
    /// encrypted; an `http` origin carries the bearer and the video in the
    /// clear. The example on [`ApiOrigin`] reads it.
    #[must_use]
    pub fn is_encrypted(&self) -> bool {
        self.0.scheme() == "https"
    }

    /// The URL of the live WebSocket feed of the `kind` stream of `camera`:
    /// `/api/streams/{camera}/{kind}/live` at the origin's host and port,
    /// under `wss` for an `https` origin and `ws` for an `http` one. The
    /// example of the [module](self) gives one.
    ///
    /// # Panics
    ///
    /// Never: `Url::set_scheme` of the `url` crate refuses `ws` or `wss` only
    /// for a URL without a host or whose scheme is not special. An origin is
    /// `http` or `https`, both special, with a host.
    #[must_use]
    pub fn live_url(&self, camera: &CameraId, kind: StreamKind) -> Url {
        let mut url = self.0.clone();
        url.set_path(&format!("/api/streams/{camera}/{kind}/live"));
        let scheme = if self.is_encrypted() { "wss" } else { "ws" };
        url.set_scheme(scheme)
            .expect("http and https switch to ws and wss, all four special schemes");
        url
    }
}

/// The origin's URL, `https://nvr.example:8443/`, which holds no userinfo,
/// query, or fragment.
impl fmt::Display for ApiOrigin {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.0, f)
    }
}

/// Why a URL is refused as the API origin. The text of each is the one a
/// viewer shows, which may add its advice after it; a URL that does not
/// parse carries the reason as its source. The example of the
/// [module](self) refuses plain HTTP.
#[derive(Clone, Debug, PartialEq, Eq, Error)]
#[non_exhaustive]
pub enum OriginError {
    /// The text is not a URL; the value says why, in the words of the `url`
    /// crate.
    #[error("the API URL does not parse")]
    Parse(#[from] ParseError),
    /// The scheme is neither `http` nor `https`.
    #[error("the API URL must use http or https")]
    Scheme,
    /// The URL carries a user name or a password.
    #[error("the API URL cannot carry userinfo")]
    Userinfo,
    /// The URL has a path other than `/`, a query, or a fragment.
    #[error(
        "the API URL must name only the daemon's origin, such as https://nvr.example:8443; \
         a path, query, or fragment is not supported"
    )]
    NotAnOrigin,
    /// An `http` URL names a host that the [`PlainHttp`] policy does not
    /// admit.
    #[error(
        "refusing plain HTTP to {host}: the bearer token and video would cross the network \
         unencrypted"
    )]
    PlainHttp {
        /// The host as the URL names it: a name, an IPv4 address, or an IPv6
        /// address in brackets.
        host: String,
    },
}

#[cfg(test)]
mod tests {
    use std::net::Ipv6Addr;

    use super::*;

    /// The three policies.
    const POLICIES: [PlainHttp; 3] = [
        PlainHttp::LoopbackOnly,
        PlainHttp::PrivateAddresses,
        PlainHttp::Anywhere,
    ];

    /// The first and the last address of each private range of RFC 1918
    /// and of the shared range of RFC 6598: the addresses these tests name
    /// in the ranges, never a host inside one
    /// (`crates/iris-client/AGENTS.md`).
    const RANGE_EDGES: [[u8; 4]; 8] = [
        [10, 0, 0, 0],
        [10, 255, 255, 255],
        [172, 16, 0, 0],
        [172, 31, 255, 255],
        [192, 168, 0, 0],
        [192, 168, 255, 255],
        [100, 64, 0, 0],
        [100, 127, 255, 255],
    ];

    /// A plain-HTTP origin at the IPv4 address `octets`, under `plain_http`.
    fn http(octets: [u8; 4], plain_http: PlainHttp) -> Result<ApiOrigin, OriginError> {
        ApiOrigin::parse(
            &format!("http://{}:8554", Ipv4Addr::from(octets)),
            plain_http,
        )
    }

    /// Whether `origin` is a refusal of plain HTTP.
    fn is_plain_http_refusal(origin: &Result<ApiOrigin, OriginError>) -> bool {
        matches!(origin, Err(OriginError::PlainHttp { .. }))
    }

    #[test]
    fn an_origin_shows_as_its_url() {
        for (text, shown) in [
            ("https://nvr.example:8443", "https://nvr.example:8443/"),
            ("http://127.0.0.1:8554/", "http://127.0.0.1:8554/"),
        ] {
            let origin = ApiOrigin::parse(text, PlainHttp::LoopbackOnly).unwrap();
            assert_eq!(origin.to_string(), shown, "{text}");
            assert_eq!(origin.to_string(), origin.url().as_str(), "{text}");
        }
    }

    #[test]
    fn https_is_accepted_under_every_policy() {
        for plain_http in POLICIES {
            assert!(
                ApiOrigin::parse("https://nvr.example:8443", plain_http).is_ok(),
                "{plain_http:?}"
            );
        }
    }

    #[test]
    fn an_https_origin_is_encrypted() {
        let origin = ApiOrigin::parse("https://nvr.example:8443", PlainHttp::LoopbackOnly);
        assert!(origin.unwrap().is_encrypted());
    }

    #[test]
    fn a_plain_http_origin_is_not_encrypted() {
        let origin = ApiOrigin::parse("http://127.0.0.1:8554", PlainHttp::LoopbackOnly);
        assert!(!origin.unwrap().is_encrypted());
    }

    /// The first and the last address of `127/8`, its usual one, the IPv6
    /// loopback, and `localhost`.
    #[test]
    fn loopback_http_is_accepted_under_loopback_only() {
        for text in [
            "http://127.0.0.0:8554",
            "http://127.0.0.1:8554",
            "http://127.255.255.255:8554",
            "http://[::1]:8554",
            "http://localhost:8554",
        ] {
            let origin = ApiOrigin::parse(text, PlainHttp::LoopbackOnly);
            assert!(origin.is_ok(), "{text}: {origin:?}");
        }
    }

    #[test]
    fn private_addresses_admits_each_of_its_ranges_to_its_edges() {
        for edge in RANGE_EDGES {
            assert!(http(edge, PlainHttp::PrivateAddresses).is_ok(), "{edge:?}");
        }
    }

    /// The address just before and just after each range, and the shared
    /// range's first address with its first octet one below and one above.
    #[test]
    fn private_addresses_refuses_the_addresses_beside_each_range() {
        for beside in [
            [9, 255, 255, 255],
            [11, 0, 0, 0],
            [172, 15, 255, 255],
            [172, 32, 0, 0],
            [192, 167, 255, 255],
            [192, 169, 0, 0],
            [100, 63, 255, 255],
            [100, 128, 0, 0],
            [99, 64, 0, 0],
            [101, 64, 0, 0],
        ] {
            assert!(
                is_plain_http_refusal(&http(beside, PlainHttp::PrivateAddresses)),
                "{beside:?}"
            );
        }
    }

    /// The first and the last address of the link-local range.
    #[test]
    fn private_addresses_refuses_the_link_local_range() {
        for refused in [[169, 254, 0, 0], [169, 254, 255, 255]] {
            assert!(
                is_plain_http_refusal(&http(refused, PlainHttp::PrivateAddresses)),
                "{refused:?}"
            );
        }
    }

    /// The first and the last address of the first documentation range.
    #[test]
    fn private_addresses_refuses_the_first_documentation_range() {
        for refused in [[192, 0, 2, 0], [192, 0, 2, 255]] {
            assert!(
                is_plain_http_refusal(&http(refused, PlainHttp::PrivateAddresses)),
                "{refused:?}"
            );
        }
    }

    #[test]
    fn private_addresses_admits_the_loopback() {
        for loopback in [
            "http://127.0.0.0:8554",
            "http://127.255.255.255:8554",
            "http://[::1]:8554",
            "http://localhost:8554",
        ] {
            assert!(
                ApiOrigin::parse(loopback, PlainHttp::PrivateAddresses).is_ok(),
                "{loopback}"
            );
        }
    }

    #[test]
    fn private_addresses_refuses_a_name_other_than_localhost() {
        assert!(matches!(
            ApiOrigin::parse("http://nvr.example:8554", PlainHttp::PrivateAddresses),
            Err(OriginError::PlainHttp { host }) if host == "nvr.example"
        ));
    }

    /// The first and the last address of the unique local range, and the
    /// first and the last address of `10/8` mapped into IPv6.
    #[test]
    fn private_addresses_refuses_every_ipv6_address_but_the_loopback() {
        for v6 in [
            Ipv6Addr::new(0xfc00, 0, 0, 0, 0, 0, 0, 0),
            Ipv6Addr::new(
                0xfdff, 0xffff, 0xffff, 0xffff, 0xffff, 0xffff, 0xffff, 0xffff,
            ),
            Ipv4Addr::new(10, 0, 0, 0).to_ipv6_mapped(),
            Ipv4Addr::new(10, 255, 255, 255).to_ipv6_mapped(),
        ] {
            let text = format!("http://[{v6}]:8554");
            assert!(
                is_plain_http_refusal(&ApiOrigin::parse(&text, PlainHttp::PrivateAddresses)),
                "{text}"
            );
        }
    }

    /// `LoopbackOnly`, the Linux viewer's default, admits the loopback alone.
    #[test]
    fn loopback_only_refuses_every_address_private_addresses_admits() {
        for edge in RANGE_EDGES {
            assert!(
                is_plain_http_refusal(&http(edge, PlainHttp::LoopbackOnly)),
                "{edge:?}"
            );
        }
    }

    #[test]
    fn anywhere_admits_a_public_address() {
        assert!(http([192, 0, 2, 7], PlainHttp::Anywhere).is_ok());
    }

    #[test]
    fn a_public_address_is_refused_under_the_other_policies() {
        for refusing in [PlainHttp::LoopbackOnly, PlainHttp::PrivateAddresses] {
            assert!(
                is_plain_http_refusal(&http([192, 0, 2, 7], refusing)),
                "{refusing:?}"
            );
        }
    }

    #[test]
    fn a_path_query_or_fragment_is_refused() {
        for text in [
            "https://nvr.example/nvr/",
            "https://nvr.example/?token=x",
            "https://nvr.example/#top",
        ] {
            assert_eq!(
                ApiOrigin::parse(text, PlainHttp::Anywhere),
                Err(OriginError::NotAnOrigin),
                "{text}"
            );
        }
    }

    #[test]
    fn userinfo_is_refused() {
        for text in [
            "https://user@nvr.example/",
            "https://user:secret@nvr.example/",
        ] {
            assert_eq!(
                ApiOrigin::parse(text, PlainHttp::Anywhere),
                Err(OriginError::Userinfo),
                "{text}"
            );
        }
    }

    /// The checks run in one order, and the first a URL fails is its
    /// refusal: the scheme, then userinfo, then the path before plain HTTP.
    #[test]
    fn the_first_check_a_url_fails_names_its_refusal() {
        let documentation = Ipv4Addr::new(192, 0, 2, 7);
        for (text, refusal) in [
            ("ftp://user@nvr.example/x".to_owned(), OriginError::Scheme),
            (
                format!("http://user@{documentation}/x"),
                OriginError::Userinfo,
            ),
            (
                format!("http://{documentation}/x"),
                OriginError::NotAnOrigin,
            ),
        ] {
            assert_eq!(
                ApiOrigin::parse(&text, PlainHttp::LoopbackOnly),
                Err(refusal),
                "{text}"
            );
        }
    }

    #[test]
    fn text_that_does_not_parse_keeps_the_reason_as_its_source() {
        use std::error::Error;

        let refused =
            ApiOrigin::parse("http://nvr.example:99999", PlainHttp::Anywhere).unwrap_err();
        assert_eq!(refused, OriginError::Parse(ParseError::InvalidPort));
        let reason = refused.source();
        assert_eq!(
            reason.map(ToString::to_string).as_deref(),
            Some("invalid port number"),
            "the url crate's reason is the source"
        );
    }

    /// The text a viewer shows for `refusal`, and the refusal after it in a
    /// walk of every refusal. The `match` names each variant, so a refusal
    /// added to `OriginError` fails to compile here until it has its text
    /// and its place in the walk.
    fn text_and_next(refusal: &OriginError) -> (&'static str, Option<OriginError>) {
        match refusal {
            OriginError::Parse(_) => ("the API URL does not parse", Some(OriginError::Scheme)),
            OriginError::Scheme => (
                "the API URL must use http or https",
                Some(OriginError::Userinfo),
            ),
            OriginError::Userinfo => (
                "the API URL cannot carry userinfo",
                Some(OriginError::NotAnOrigin),
            ),
            OriginError::NotAnOrigin => (
                "the API URL must name only the daemon's origin, such as \
                 https://nvr.example:8443; a path, query, or fragment is not supported",
                Some(OriginError::PlainHttp {
                    host: "nvr.example".to_owned(),
                }),
            ),
            OriginError::PlainHttp { .. } => (
                "refusing plain HTTP to nvr.example: the bearer token and video would cross \
                 the network unencrypted",
                None,
            ),
        }
    }

    /// Each refusal has the text a viewer shows before it adds any advice.
    #[test]
    fn each_origin_refusal_has_the_text_a_viewer_shows() {
        let mut walked: Vec<OriginError> = Vec::new();
        let mut next = Some(OriginError::Parse(ParseError::InvalidPort));
        while let Some(refusal) = next {
            assert!(
                !walked.contains(&refusal),
                "the walk returns to {refusal:?}"
            );
            let (text, after) = text_and_next(&refusal);
            assert_eq!(refusal.to_string(), text, "{refusal:?}");
            walked.push(refusal);
            next = after;
        }
        assert_eq!(walked.len(), 5, "the walk visits every refusal");
    }

    #[test]
    fn the_plain_http_refusal_warns_of_unencrypted_traffic_to_the_host() {
        let refused = ApiOrigin::parse("http://nvr.example:8554", PlainHttp::LoopbackOnly)
            .unwrap_err()
            .to_string();
        assert_eq!(
            refused,
            "refusing plain HTTP to nvr.example: the bearer token and video would cross the \
             network unencrypted",
            "the text ends where a viewer's advice begins"
        );
    }

    #[test]
    fn live_url_maps_each_http_scheme_to_its_websocket_scheme() {
        let camera = CameraId::try_new("cam1").unwrap();
        for (origin, live) in [
            (
                "http://192.0.2.7:8554",
                "ws://192.0.2.7:8554/api/streams/cam1/sub/live",
            ),
            (
                "https://nvr.example:8443",
                "wss://nvr.example:8443/api/streams/cam1/sub/live",
            ),
            (
                "https://nvr.example",
                "wss://nvr.example/api/streams/cam1/sub/live",
            ),
        ] {
            let origin = ApiOrigin::parse(origin, PlainHttp::Anywhere).unwrap();
            assert_eq!(origin.live_url(&camera, StreamKind::Sub).as_str(), live);
        }
    }
}
