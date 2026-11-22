//! The answers of the irisd API as a viewer decodes them, and the bodies of
//! the requests that add a camera and look up its streams. An answer type
//! holds the fields a viewer uses; the daemon may send more. The two request
//! bodies carry a camera's password, which their `Debug` output redacts. A
//! URL a caller gave shows in any `Debug` here with its userinfo, its query,
//! and its fragment each as `<redacted>` and its path as it stands
//! ([`shown_url`]). It shows as `<redacted>` whole when it has no host, or,
//! for a camera's stream URL, when
//! [`rtsp_url_problem`](crate::api::rtsp_url_problem) refuses it. A host a
//! caller gave shows bare when the daemon's rule takes it, and as
//! `<redacted>` whole otherwise.

use std::fmt;

use iris_core::config;
use iris_core::event::EventSource;
use iris_core::id::{CameraId, StreamKind};
use iris_core::time::WallUs;
use serde::{Deserialize, Serialize};
use url::{Position, Url};

use crate::api::{ApiError, Rule, validate};
use crate::vod::PlaybackFragment;

/// What a `Debug` output shows of `url`: its scheme, host, port, and path as
/// they stand, and its userinfo, its query, and its fragment each as
/// `<redacted>`. A camera may take its password in the query as well as in the
/// userinfo. A URL without a host, whose path may hold anything, shows
/// as `<redacted>` whole. A secret written into the path of a URL with a host
/// shows.
pub(super) fn shown_url(url: &Url) -> String {
    if !url.has_host() {
        return "<redacted>".to_owned();
    }
    let userinfo = if url.username().is_empty() && url.password().is_none() {
        ""
    } else {
        "<redacted>@"
    };
    let query = if url.query().is_some() {
        "?<redacted>"
    } else {
        ""
    };
    let fragment = if url.fragment().is_some() {
        "#<redacted>"
    } else {
        ""
    };
    format!(
        "{}{userinfo}{}{query}{fragment}",
        &url[..Position::BeforeUsername],
        &url[Position::BeforeHost..Position::AfterPath],
    )
}

/// What a `Debug` output shows of a camera's stream URL. It is `<redacted>`
/// whole when [`rtsp_url_problem`](crate::api::rtsp_url_problem) finds a
/// problem with it, as it does with a URL that carries userinfo or a fragment,
/// does not parse, or is not `rtsp://` with a host. Otherwise it is the URL as
/// [`shown_url`] shows it, with its query as `<redacted>`. A secret written
/// into its path shows.
fn shown_stream_url(url: &str) -> String {
    match validate::parse_stream_url(url) {
        Ok(parsed) => shown_url(&parsed),
        Err(_) => "<redacted>".to_owned(),
    }
}

/// What a `Debug` output shows of a device service host: the host when the
/// daemon's rule for it, [`config::is_valid_host`], takes it, and
/// `<redacted>` otherwise. Only a bare host passes that rule: one without
/// userinfo, a port, a path, a query, or a fragment.
fn shown_host(host: &str) -> &str {
    if config::is_valid_host(host) {
        host
    } else {
        "<redacted>"
    }
}

/// One camera as `/api/cameras` lists it. The example of the
/// [`api`](crate::api) module decodes a list of them.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Camera {
    /// The configured camera id.
    pub id: CameraId,
    /// The configured display name.
    pub name: String,
    /// Added through the API, so the API may remove it.
    pub provisioned: bool,
    /// The daemon records the camera's audio and serves it live. A daemon
    /// that does not say is taken to have none.
    #[serde(default)]
    pub has_audio: bool,
}

/// A catalog event id as `/api/events` lists it, which is positive:
/// [`EventId::try_new`] makes one of a positive number alone, and an answer
/// whose event carries another id does not decode.
///
/// # Example
///
/// ```
/// # use std::error::Error;
/// #
/// use iris_client::api::EventId;
///
/// let id = EventId::try_new(7)?;
/// assert_eq!(id.get(), 7);
/// // An answer's id decodes by the same rule.
/// assert_eq!(serde_json::from_str::<EventId>("7")?, id);
/// assert!(EventId::try_new(0).is_err());
/// assert!(serde_json::from_str::<EventId>("0").is_err());
/// # Ok::<(), Box<dyn Error>>(())
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "i64", into = "i64")]
pub struct EventId(i64);

impl EventId {
    /// The event id `id`, as the catalog numbers its events. The example on
    /// [`EventId`] makes one and refuses zero.
    ///
    /// # Errors
    ///
    /// [`ApiError::Invalid`] with [`Rule::EventId`], "invalid event id", when
    /// `id` is not positive.
    pub fn try_new(id: i64) -> Result<Self, ApiError> {
        validate::require(id > 0, Rule::EventId)?;
        Ok(Self(id))
    }

    /// The id as the catalog numbers it, which is positive. The example on
    /// [`EventId`] reads it.
    #[must_use]
    pub const fn get(self) -> i64 {
        self.0
    }
}

/// The id `id` as [`EventId::try_new`] makes it, the rule an answer's id is
/// decoded by.
impl TryFrom<i64> for EventId {
    type Error = ApiError;

    fn try_from(id: i64) -> Result<Self, ApiError> {
        Self::try_new(id)
    }
}

/// The number the catalog gives the event, as an answer's JSON carries it.
impl From<EventId> for i64 {
    fn from(id: EventId) -> Self {
        id.0
    }
}

impl fmt::Display for EventId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// What a retain mark does to the footage around an event
/// ([`ApiOrigin::retain_event`](crate::origin::ApiOrigin::retain_event)):
/// keep it from retention until a time, or release it. The example on
/// [`HttpRequest`](crate::api::HttpRequest) releases a mark.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Retention {
    /// Keep the footage from retention until this time.
    Until(WallUs),
    /// Clear the mark, so that retention removes the footage as it removes
    /// any other.
    Release,
}

/// A camera WS-Discovery found, as `GET /api/onvif/devices` lists it. The
/// example on [`validate_devices`](crate::api::validate_devices) checks one.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
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
/// Its `Debug` output shows the URL by the rule a [`NewCamera`] shows its
/// stream URLs by. A URL that
/// [`rtsp_url_problem`](crate::api::rtsp_url_problem) finds a problem with
/// shows as `<redacted>` whole, and any other with its query as `<redacted>`,
/// since a camera may take its password there. The example on
/// [`validate_streams`](crate::api::validate_streams) checks one.
#[derive(Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ProfileStream {
    /// The token the camera's media service names the profile by.
    pub token: String,
    /// The profile's name as the camera labels it, which a viewer shows.
    pub name: String,
    /// The RTSP URL on the camera's host. The daemon refuses an advertised
    /// URL that carries userinfo, and
    /// [`validate_streams`](crate::api::validate_streams) refuses a URL that
    /// [`rtsp_url_problem`](crate::api::rtsp_url_problem) finds a problem
    /// with.
    pub url: String,
}

impl fmt::Debug for ProfileStream {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ProfileStream")
            .field("token", &self.token)
            .field("name", &self.name)
            .field("url", &shown_stream_url(&self.url))
            .finish()
    }
}

/// The body of `POST /api/onvif/streams`. Its `Debug` output redacts the
/// password. It shows the host as `<redacted>` unless the daemon's rule for it
/// ([`iris_core::config::is_valid_host`]) takes it, which only a bare host
/// does. A host with userinfo, a port, a path, a query, or a fragment is
/// redacted. The example on [`ApiOrigin`](crate::origin::ApiOrigin) builds a
/// request with one.
#[derive(Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
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
            .field("host", &shown_host(&self.host))
            .field("http_port", &self.http_port)
            .field("https", &self.https)
            .field("tls_verify", &self.tls_verify)
            .field("username", &self.username)
            .field("password", &"<redacted>")
            .finish()
    }
}

/// A recorded segment as `/api/streams/{camera}/{kind}/segments` lists it;
/// only the fields the timeline draws. The example on
/// [`validate_segments`](crate::api::validate_segments) checks two.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Segment {
    /// The wall time of the file's first fragment.
    #[serde(rename = "wall_start_us")]
    pub start: WallUs,
    /// Wall-clock end; for an open file, its last durable progress.
    #[serde(rename = "wall_end_us")]
    pub end: WallUs,
    /// Whether the file is sealed (false for the one being written).
    pub sealed: bool,
}

/// One fetched VOD playlist, with timing and media references from the same
/// body, as the check of its answer gives it
/// ([`answer::vod_playlist`](crate::api::answer::vod_playlist)). That check is
/// the one place a playlist is made: its timing passed the window's rules and
/// its references are absolute on the API origin. Its `Debug` output leaves
/// out the body, whose references carry media capabilities. The example of
/// the [`playback`](crate::playback) module checks two.
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct VodPlaylist {
    /// Complete fragments in the order this playlist presents them.
    fragments: Vec<PlaybackFragment>,
    /// The fetched playlist with media and init URIs resolved against its URL,
    /// every one on the API origin.
    body: String,
}

impl VodPlaylist {
    /// The playlist of `fragments`, read from `body`, which the check of an
    /// answer has passed.
    pub(crate) fn new(fragments: Vec<PlaybackFragment>, body: String) -> Self {
        Self { fragments, body }
    }

    /// The complete fragments in the order the playlist presents them, at
    /// least one of them in the window it was asked for. The example of the
    /// [`playback`](crate::playback) module reads them through a selection.
    #[must_use]
    pub fn fragments(&self) -> &[PlaybackFragment] {
        &self.fragments
    }

    /// The fetched playlist with its media and init URIs resolved against the
    /// URL it came from, every one an absolute URL on the API origin. The
    /// example of the [`playback`](crate::playback) module reads it through a
    /// snapshot.
    #[must_use]
    pub fn body(&self) -> &str {
        &self.body
    }

    /// The fragments and the body, for the selection that holds them.
    pub(crate) fn into_parts(self) -> (Vec<PlaybackFragment>, String) {
        (self.fragments, self.body)
    }
}

impl fmt::Debug for VodPlaylist {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("VodPlaylist")
            .field("fragments", &self.fragments.len())
            .field("body_bytes", &self.body.len())
            .finish()
    }
}

/// An event as `/api/events` lists it; only the fields the timeline marks.
/// The example on [`validate_events`](crate::api::validate_events) checks
/// one.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct EventRecord {
    /// The id the daemon's catalog gives the event, which a retain mark
    /// names.
    pub id: EventId,
    /// What happened: a kind the recorder names, or a vendor's code.
    pub kind: EventKind,
    /// Recorder time of the event.
    pub nvr_time: WallUs,
    /// Until when the footage around the event is kept, when the event is
    /// retained.
    #[serde(rename = "retain_until_us")]
    pub retain_until: Option<WallUs>,
}

/// An event kind as the API serialises [`iris_core::event::EventKind`]: a
/// bare string for the kinds the recorder names (`motion`, `line_cross`,
/// ...) and `{"other": "<vendor code>"}` for the rest.
///
/// # Example
///
/// ```
/// # use std::error::Error;
/// #
/// use iris_client::api::EventKind;
///
/// let named: EventKind = serde_json::from_str(r#""motion""#)?;
/// assert_eq!(named.label(), "motion");
/// let vendor: EventKind = serde_json::from_str(r#"{"other":"NewFile"}"#)?;
/// assert_eq!(vendor.to_string(), "NewFile");
/// # Ok::<(), Box<dyn Error>>(())
/// ```
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
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
    /// The text a viewer shows for the kind: the name the recorder gives it,
    /// or the vendor's code as the camera sent it. The example on
    /// [`EventKind`] reads both.
    #[must_use]
    pub fn label(&self) -> &str {
        match self {
            Self::Named(name) | Self::Other { other: name } => name,
        }
    }
}

/// The kind as its [`EventKind::label`] gives it: `motion`, or a vendor's
/// code.
impl fmt::Display for EventKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.label())
    }
}

/// A camera to add through `POST /api/cameras`: the fields of a `[[camera]]`
/// table that a viewer's form collects. Its `Debug` output redacts the
/// password. It shows a stream URL that
/// [`rtsp_url_problem`](crate::api::rtsp_url_problem) finds a problem with,
/// one with userinfo or a fragment among them, as `<redacted>` whole, and any
/// other with its query as `<redacted>`. A secret written into a URL's path
/// shows. The example on [`ApiOrigin`](crate::origin::ApiOrigin) builds one.
#[derive(Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct NewCamera {
    /// The id the daemon names the camera's recordings and routes by.
    pub id: CameraId,
    /// The name a viewer shows for the camera.
    pub name: String,
    /// Main stream URL.
    /// [`ApiOrigin::add_camera`](crate::origin::ApiOrigin::add_camera) refuses
    /// a URL that [`rtsp_url_problem`](crate::api::rtsp_url_problem) finds a
    /// problem with, one with userinfo among them, and sends any other as it
    /// stands, its query included.
    pub rtsp_url: String,
    /// Sub stream URL, which
    /// [`add_camera`](crate::origin::ApiOrigin::add_camera) checks by the rule
    /// of [`rtsp_url`](NewCamera::rtsp_url). When it is absent, the daemon
    /// derives one from a main URL that holds `subtype=0`, with `subtype=1` in
    /// its place.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sub_url: Option<String>,
    /// RTSP and HTTP user.
    pub username: String,
    /// RTSP and HTTP password.
    pub password: String,
    /// The streams the daemon records, which the body names as a
    /// configuration does: `main`, `sub`, and `audio`.
    pub record: Vec<StreamKind>,
    /// The sources of the camera's events, `dahua_cgi` and
    /// `onvif_pull_point` in the body; left out of it when there is none.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub events: Vec<EventSource>,
}

impl fmt::Debug for NewCamera {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("NewCamera")
            .field("id", &self.id)
            .field("name", &self.name)
            .field("rtsp_url", &shown_stream_url(&self.rtsp_url))
            .field("sub_url", &self.sub_url.as_deref().map(shown_stream_url))
            .field("username", &self.username)
            .field("password", &"<redacted>")
            .field("record", &self.record)
            .field("events", &self.events)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use iris_core::time::MediaTicks;
    use serde::de::DeserializeOwned;

    use super::*;

    fn time(micros: i64) -> WallUs {
        WallUs::new(micros)
    }

    /// Each answer and request body decodes from the JSON it encodes to, as a
    /// data-transfer type does, whichever form its optional fields take.
    #[test]
    fn each_transfer_type_decodes_from_the_json_it_encodes_to() {
        fn round_trip<T: Serialize + DeserializeOwned + PartialEq + fmt::Debug>(value: &T) {
            let json = serde_json::to_string(value).unwrap();
            assert_eq!(&serde_json::from_str::<T>(&json).unwrap(), value, "{json}");
        }
        round_trip(&Camera {
            id: CameraId::try_new("cam1").unwrap(),
            name: "Front".into(),
            provisioned: true,
            has_audio: true,
        });
        for (kind, retain_until) in [
            (EventKind::Named("motion".into()), None),
            (
                EventKind::Other {
                    other: "NewFile".into(),
                },
                Some(time(9)),
            ),
        ] {
            round_trip(&EventRecord {
                id: EventId::try_new(7).unwrap(),
                kind,
                nvr_time: time(5),
                retain_until,
            });
        }
        round_trip(&DiscoveredDevice {
            endpoint: "urn:uuid:1".into(),
            host: "192.0.2.20".into(),
            port: 80,
            https: false,
            name: Some("Garage".into()),
            hardware: None,
            location: None,
        });
        round_trip(&ProfileStream {
            token: "profile-1".into(),
            name: "Main".into(),
            url: "rtsp://192.0.2.20/1".into(),
        });
        round_trip(&Segment {
            start: time(10),
            end: time(20),
            sealed: false,
        });
        round_trip(&lookup("192.0.2.20"));
        round_trip(&new_camera());
        round_trip(&NewCamera {
            sub_url: Some("rtsp://192.0.2.10/y".into()),
            events: vec![EventSource::OnvifPullPoint],
            ..new_camera()
        });
    }

    #[test]
    fn camera_ids_outside_the_wire_contract_fail_to_decode() {
        for id in ["../cam", "Cam1", "", "a/b"] {
            let body = format!(r#"[{{"id":"{id}","name":"Front","provisioned":false}}]"#);
            assert!(serde_json::from_str::<Vec<Camera>>(&body).is_err(), "{id}");
        }
    }

    #[test]
    fn an_event_id_is_made_of_a_positive_number_alone() {
        for id in [1, 7, i64::MAX] {
            assert_eq!(EventId::try_new(id).map(EventId::get).ok(), Some(id));
        }
        for id in [0, -1, i64::MIN] {
            assert_eq!(
                EventId::try_new(id).map_err(|refused| refused.to_string()),
                Err("invalid event id".to_owned()),
                "{id}"
            );
        }
    }

    /// An answer's id decodes by the rule `try_new` checks a number by.
    #[test]
    fn an_event_id_decodes_by_the_rule_it_is_made_by() {
        assert_eq!(
            serde_json::from_str::<EventId>("7").ok(),
            EventId::try_new(7).ok()
        );
        for id in ["0", "-1"] {
            let refused = serde_json::from_str::<EventId>(id).unwrap_err();
            assert!(
                refused.to_string().starts_with("invalid event id"),
                "{id}: {refused}"
            );
        }
    }

    /// Two events as the daemon lists them: one of a named kind and not
    /// retained, and one of a vendor's code, retained until 5 µs.
    fn listed_events() -> Vec<EventRecord> {
        serde_json::from_str(concat!(
            r#"[{"id":1,"kind":"motion","nvr_time":1,"retain_until_us":null},"#,
            r#"{"id":2,"kind":{"other":"NewFile"},"nvr_time":2,"retain_until_us":5}]"#
        ))
        .unwrap()
    }

    #[test]
    fn an_event_kind_decodes_from_a_name_or_a_vendor_code() {
        let events = listed_events();
        assert_eq!(events[0].kind.label(), "motion");
        assert_eq!(events[1].kind.label(), "NewFile");
    }

    #[test]
    fn an_event_decodes_its_nvr_time() {
        let events = listed_events();
        assert_eq!((events[0].nvr_time, events[1].nvr_time), (time(1), time(2)));
    }

    /// A `retain_until_us` of null decodes as no retention.
    #[test]
    fn an_event_decodes_its_retention_or_its_absence() {
        let events = listed_events();
        assert_eq!(
            (events[0].retain_until, events[1].retain_until),
            (None, Some(time(5)))
        );
    }

    #[test]
    fn an_event_kind_shows_as_its_label() {
        let named = EventKind::Named("motion".into());
        let other = EventKind::Other {
            other: "NewFile".into(),
        };
        assert_eq!(
            (named.to_string(), other.to_string()),
            ("motion".into(), "NewFile".into())
        );
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

    #[test]
    fn a_stream_lookup_request_redacts_its_password() {
        let request = lookup("192.0.2.20");
        assert!(!format!("{request:?}").contains("secret"));
    }

    /// A host shows as it stands only when the daemon's rule takes it, so
    /// userinfo, a query, or anything else a caller put after a host never
    /// prints.
    #[test]
    fn a_stream_lookup_shows_a_host_only_as_the_daemon_takes_one() {
        for host in ["192.0.2.20", "2001:db8::7", "camera.example"] {
            let shown = format!("{:?}", lookup(host));
            assert!(shown.contains(&format!("host: {host:?}")), "{shown}");
        }
        for host in [
            "viewer:hidden@192.0.2.20",
            "viewer@192.0.2.20",
            "[viewer:hidden@192.0.2.20]",
            "192.0.2.20?password=hidden",
            "192.0.2.20/hidden",
            "192.0.2.20#hidden",
            "192.0.2.20:80",
            "",
        ] {
            let shown = format!("{:?}", lookup(host));
            assert!(shown.contains(r#"host: "<redacted>""#), "{shown}");
            assert!(!shown.contains("hidden") && !shown.contains('@'), "{shown}");
        }
    }

    fn new_camera() -> NewCamera {
        NewCamera {
            id: CameraId::try_new("cam8").unwrap(),
            name: "Garage".into(),
            rtsp_url: "rtsp://192.0.2.10/x".into(),
            sub_url: None,
            username: "viewer".into(),
            password: "secret".into(),
            record: vec![StreamKind::Main],
            events: Vec::new(),
        }
    }

    /// The body names the streams to record as a camera's configuration
    /// does, the daemon's `[[camera]]` table.
    #[test]
    fn a_camera_to_add_names_its_streams_as_a_configuration_does() {
        let camera = NewCamera {
            record: vec![StreamKind::Main, StreamKind::Sub],
            ..new_camera()
        };
        let json = serde_json::to_value(&camera).unwrap();
        assert_eq!(json["record"], serde_json::json!(["main", "sub"]));
    }

    /// The body names the event sources as a camera's configuration does,
    /// the daemon's `[[camera]]` table.
    #[test]
    fn a_camera_to_add_names_its_event_sources_as_a_configuration_does() {
        let camera = NewCamera {
            events: vec![EventSource::DahuaCgi, EventSource::OnvifPullPoint],
            ..new_camera()
        };
        let json = serde_json::to_value(&camera).unwrap();
        assert_eq!(
            json["events"],
            serde_json::json!(["dahua_cgi", "onvif_pull_point"])
        );
    }

    /// The body of a camera to add without a sub stream URL has no `sub_url` key.
    #[test]
    fn a_camera_to_add_without_a_sub_stream_url_leaves_it_out_of_the_body() {
        let json = serde_json::to_value(new_camera()).unwrap();
        assert!(json.get("sub_url").is_none(), "{json}");
    }

    #[test]
    fn a_camera_to_add_redacts_its_password() {
        let shown = format!("{:?}", new_camera());
        assert!(
            !shown.contains("secret") && shown.contains("<redacted>"),
            "{shown}"
        );
    }

    /// The `Debug` output of a camera to add whose main and sub URL are both
    /// `url`.
    fn shown_with_streams(url: &str) -> String {
        let camera = NewCamera {
            rtsp_url: url.into(),
            sub_url: Some(url.into()),
            ..new_camera()
        };
        format!("{camera:?}")
    }

    /// A stream URL that `rtsp_url_problem` passes shows with its query as
    /// redacted, so a query secret in either URL never prints.
    #[test]
    fn a_camera_to_add_shows_a_stream_urls_query_redacted() {
        for (url, expected) in [
            ("rtsp://192.0.2.10/x", "rtsp://192.0.2.10/x"),
            (
                "rtsp://192.0.2.10/x?password=hidden",
                "rtsp://192.0.2.10/x?<redacted>",
            ),
            (
                "rtsp://192.0.2.10:554/cam/realmonitor?channel=1&subtype=0",
                "rtsp://192.0.2.10:554/cam/realmonitor?<redacted>",
            ),
        ] {
            let shown = shown_with_streams(url);
            assert!(
                shown.contains(&format!("rtsp_url: {expected:?}"))
                    && shown.contains(&format!("sub_url: Some({expected:?})")),
                "{shown}"
            );
            assert!(!shown.contains("hidden"), "{shown}");
        }
    }

    /// A secret a caller writes into a stream URL's path prints.
    #[test]
    fn a_camera_to_add_shows_a_stream_urls_path_as_it_stands() {
        let path = "rtsp://192.0.2.10/user=viewer_password=shown";
        let shown = shown_with_streams(path);
        assert!(
            shown.contains(&format!("rtsp_url: {path:?}"))
                && shown.contains(&format!("sub_url: Some({path:?})")),
            "{shown}"
        );
    }

    /// A stream URL that `rtsp_url_problem` refuses shows as redacted whole,
    /// so neither userinfo nor a query in either URL prints, whether the URL
    /// parses or not.
    #[test]
    fn a_camera_to_add_shows_a_stream_url_the_rule_refuses_redacted_whole() {
        for url in [
            "rtsp://viewer:hidden@192.0.2.10/x",
            "rtsp://viewer@192.0.2.10/x",
            "http://viewer:hidden@192.0.2.10/x",
            "rtsp://viewer:hidden@[192.0.2.10/x",
            "rtsp://[192.0.2.10/x?password=hidden",
            "rtsp://192.0.2.10/x#hidden",
            "viewer:hidden@192.0.2.10/x",
        ] {
            let shown = shown_with_streams(url);
            assert!(
                shown.contains(r#"rtsp_url: "<redacted>""#)
                    && shown.contains(r#"sub_url: Some("<redacted>")"#),
                "{shown}"
            );
            assert!(!shown.contains("hidden") && !shown.contains('@'), "{shown}");
        }
    }

    #[test]
    fn a_camera_to_add_without_a_sub_url_shows_none() {
        let shown = format!("{:?}", new_camera());
        assert!(shown.contains("sub_url: None"), "{shown}");
    }

    /// The `Debug` output of a profile stream whose URL is `url`.
    fn shown_profile(url: &str) -> String {
        let stream = ProfileStream {
            token: "profile-1".into(),
            name: "Main".into(),
            url: url.into(),
        };
        format!("{stream:?}")
    }

    /// The `Debug` output of a profile stream whose URL shows as `shown`.
    fn profile_showing(shown: &str) -> String {
        format!(r#"ProfileStream {{ token: "profile-1", name: "Main", url: {shown:?} }}"#)
    }

    /// A profile's URL shows as the stream URL of a camera to add does: with
    /// its query as redacted when `rtsp_url_problem` passes it.
    #[test]
    fn a_profile_stream_shows_its_urls_query_redacted() {
        for (url, expected) in [
            ("rtsp://192.0.2.20/1", "rtsp://192.0.2.20/1"),
            (
                "rtsp://192.0.2.20/1?password=hidden",
                "rtsp://192.0.2.20/1?<redacted>",
            ),
        ] {
            assert_eq!(shown_profile(url), profile_showing(expected), "{url}");
        }
    }

    /// A profile's URL that `rtsp_url_problem` refuses shows as redacted
    /// whole, so neither userinfo nor a query secret prints.
    #[test]
    fn a_profile_stream_shows_a_url_the_rule_refuses_redacted_whole() {
        for url in [
            "rtsp://viewer:hidden@192.0.2.20/1",
            "rtsp://192.0.2.20/1#hidden",
            "rtsp://[192.0.2.20/1?password=hidden",
        ] {
            assert_eq!(shown_profile(url), profile_showing("<redacted>"), "{url}");
        }
    }

    /// A URL shows its scheme, host, port, and path as they stand, and its
    /// userinfo, query, and fragment each as redacted, whichever it holds.
    #[test]
    fn a_url_shows_without_its_userinfo_query_or_fragment() {
        for (url, shown) in [
            (
                "https://nvr.example:8443/api/cameras",
                "https://nvr.example:8443/api/cameras",
            ),
            (
                "https://viewer:hidden@nvr.example:8443/api/cameras",
                "https://<redacted>@nvr.example:8443/api/cameras",
            ),
            (
                "https://viewer@nvr.example/api",
                "https://<redacted>@nvr.example/api",
            ),
            (
                "https://:hidden@nvr.example/api",
                "https://<redacted>@nvr.example/api",
            ),
            (
                "https://nvr.example/api/segments/7/file.mp4?token=hidden",
                "https://nvr.example/api/segments/7/file.mp4?<redacted>",
            ),
            (
                "https://nvr.example/api?",
                "https://nvr.example/api?<redacted>",
            ),
            (
                "https://nvr.example/api#hidden",
                "https://nvr.example/api#<redacted>",
            ),
            (
                "https://viewer:hidden@[2001:db8::1]:8443/api?token=hidden#hidden",
                "https://<redacted>@[2001:db8::1]:8443/api?<redacted>#<redacted>",
            ),
            ("https://nvr.example/shown", "https://nvr.example/shown"),
        ] {
            assert_eq!(shown_url(&Url::parse(url).unwrap()), shown, "{url}");
        }
    }

    /// A URL without a host, whose path may hold anything, shows as redacted
    /// whole.
    #[test]
    fn a_url_without_a_host_shows_redacted_whole() {
        for url in ["data:text/plain,hidden", "viewer:hidden@nvr.example/api"] {
            assert_eq!(shown_url(&Url::parse(url).unwrap()), "<redacted>", "{url}");
        }
    }

    #[test]
    fn a_vod_playlist_debug_shows_counts_in_place_of_its_body() {
        let playlist = VodPlaylist {
            fragments: vec![PlaybackFragment {
                wall_start: time(1_000_000),
                duration: MediaTicks::new(90_000),
            }],
            body: concat!(
                "#EXTM3U\n",
                "#EXT-X-MAP:URI=\"https://nvr.example/api/segments/7/file.mp4?token=c4p\"\n",
                "#IRIS-FRAGMENT:WALL-US=1000000,DURATION-TICKS=90000\n#EXTINF:1.00000,\n",
                "https://nvr.example/api/segments/7/file.mp4?token=c4p\n#EXT-X-ENDLIST\n"
            )
            .to_owned(),
        };
        let shown = format!("{playlist:?}");
        assert!(
            !shown.contains("token=") && !shown.contains("/api/segments/"),
            "{shown}"
        );
        // The counts stand in for the body, whole.
        assert_eq!(
            shown,
            format!(
                "VodPlaylist {{ fragments: 1, body_bytes: {} }}",
                playlist.body.len()
            )
        );
    }
}
