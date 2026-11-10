//! Normalised camera events shared by the event sources, the catalog, and the API.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::id::CameraId;
use crate::time::WallUs;

/// Largest vendor payload kept with an event, in bytes.
pub const MAX_EVENT_DATA_LEN: usize = 64 * 1024;
/// Largest unmapped vendor event code kept in [`EventKind::Other`], in bytes.
pub const MAX_EVENT_KIND_LEN: usize = 1024;

/// Where an event came from. The names (`dahua_cgi`, `onvif_pull_point`,
/// `onvif_metadata`, `manual`) are shared by serde, [`fmt::Display`], and
/// [`FromStr`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventSource {
    /// The Dahua HTTP CGI push feed.
    DahuaCgi,
    /// An ONVIF `PullPoint` subscription.
    OnvifPullPoint,
    /// The ONVIF metadata RTP track. Reserved: no event source produces it,
    /// and configuration rejects subscribing to it.
    OnvifMetadata,
    /// An event entered by a person. Reserved: no API route creates one, and
    /// configuration rejects subscribing to it.
    Manual,
}

/// Error returned when an event source name is unknown.
#[derive(Clone, Debug, PartialEq, Eq, Error)]
#[error(
    "unknown event source {0:?}; expected dahua_cgi, onvif_pull_point, onvif_metadata, or manual"
)]
pub struct UnknownEventSource(String);

impl EventSource {
    /// The name serde, logs, and the catalog use.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::DahuaCgi => "dahua_cgi",
            Self::OnvifPullPoint => "onvif_pull_point",
            Self::OnvifMetadata => "onvif_metadata",
            Self::Manual => "manual",
        }
    }
}

impl fmt::Display for EventSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for EventSource {
    type Err = UnknownEventSource;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "dahua_cgi" => Ok(Self::DahuaCgi),
            "onvif_pull_point" => Ok(Self::OnvifPullPoint),
            "onvif_metadata" => Ok(Self::OnvifMetadata),
            "manual" => Ok(Self::Manual),
            other => Err(UnknownEventSource(other.to_owned())),
        }
    }
}

/// What an event reports. Vendor codes without a mapping keep their name.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventKind {
    /// Pixel-change motion detection.
    Motion,
    /// A person was detected.
    Human,
    /// A vehicle was detected.
    Vehicle,
    /// A tripwire rule fired.
    LineCross,
    /// An intrusion-region rule fired.
    Intrusion,
    /// Camera tampering or video blind.
    Tamper,
    /// Audio-level anomaly.
    Audio,
    /// A face was detected.
    Face,
    /// The camera lost its video input.
    VideoLoss,
    /// The camera's clock was set or adjusted.
    TimeChange,
    /// A client session ended abnormally.
    Disconnect,
    /// Any other vendor code; [`Event::bounded`] cuts it to
    /// [`MAX_EVENT_KIND_LEN`] bytes on a UTF-8 boundary.
    Other(String),
}

/// Error returned when a name is not a built-in event kind.
#[derive(Clone, Debug, PartialEq, Eq, Error)]
#[error("unknown event kind {0:?}; vendor codes travel as EventKind::Other")]
pub struct UnknownEventKind(String);

impl EventKind {
    /// The built-in kinds, in the order of their definition; `Other` carries
    /// everything else.
    pub const BUILT_IN: [Self; 11] = [
        Self::Motion,
        Self::Human,
        Self::Vehicle,
        Self::LineCross,
        Self::Intrusion,
        Self::Tamper,
        Self::Audio,
        Self::Face,
        Self::VideoLoss,
        Self::TimeChange,
        Self::Disconnect,
    ];

    /// The serde name of a built-in kind (`motion`, `line_cross`, …), or the
    /// vendor code of [`EventKind::Other`].
    #[must_use]
    pub fn name(&self) -> &str {
        match self {
            Self::Motion => "motion",
            Self::Human => "human",
            Self::Vehicle => "vehicle",
            Self::LineCross => "line_cross",
            Self::Intrusion => "intrusion",
            Self::Tamper => "tamper",
            Self::Audio => "audio",
            Self::Face => "face",
            Self::VideoLoss => "video_loss",
            Self::TimeChange => "time_change",
            Self::Disconnect => "disconnect",
            Self::Other(code) => code,
        }
    }
}

impl fmt::Display for EventKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// Parses a built-in kind by its serde name; a vendor code is not one and
/// belongs in [`EventKind::Other`].
impl FromStr for EventKind {
    type Err = UnknownEventKind;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::BUILT_IN
            .iter()
            .find(|kind| kind.name() == s)
            .cloned()
            .ok_or_else(|| UnknownEventKind(s.to_owned()))
    }
}

/// The phase of an event. The names (`start`, `stop`, `pulse`) are shared by
/// serde, [`fmt::Display`], and [`FromStr`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventState {
    /// The condition began.
    Start,
    /// The condition ended.
    Stop,
    /// A momentary occurrence or a state report.
    Pulse,
}

/// Error returned when an event state name is unknown.
#[derive(Clone, Debug, PartialEq, Eq, Error)]
#[error("unknown event state {0:?}; expected start, stop, or pulse")]
pub struct UnknownEventState(String);

impl EventState {
    /// The name serde, logs, and the catalog use.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Start => "start",
            Self::Stop => "stop",
            Self::Pulse => "pulse",
        }
    }
}

impl fmt::Display for EventState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for EventState {
    type Err = UnknownEventState;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "start" => Ok(Self::Start),
            "stop" => Ok(Self::Stop),
            "pulse" => Ok(Self::Pulse),
            other => Err(UnknownEventState(other.to_owned())),
        }
    }
}

/// One normalised event.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Event {
    /// The camera that reported it.
    pub camera: CameraId,
    /// The source it was received from.
    pub source: EventSource,
    /// What it reports.
    pub kind: EventKind,
    /// Start, stop, or pulse.
    pub state: EventState,
    /// Receipt time on the NVR clock; the authoritative placement.
    pub nvr_time: WallUs,
    /// The camera's own timestamp when it supplied one (diagnostic only).
    pub camera_time: Option<WallUs>,
    /// Vendor payload. [`Event::bounded`] makes it valid JSON text of at most
    /// [`MAX_EVENT_DATA_LEN`] bytes: invalid input becomes a JSON string, and
    /// an encoding beyond the limit becomes `{"truncated":true}` rather than
    /// a slice.
    pub data: Option<String>,
}

impl Event {
    /// Bounds camera-controlled strings to their shared storage limits.
    ///
    /// Vendor data is canonicalized as valid JSON. Invalid text becomes a JSON
    /// string, and over-limit encodings become a small explicit truncation marker.
    #[must_use]
    pub fn bounded(mut self) -> Self {
        if let EventKind::Other(code) = &mut self.kind {
            truncate_utf8(code, MAX_EVENT_KIND_LEN);
        }
        if let Some(data) = self.data.take() {
            self.data = Some(bound_json(&data));
        }
        self
    }
}

fn bound_json(data: &str) -> String {
    let encoded = match serde_json::from_str::<serde_json::Value>(data) {
        Ok(value) => serde_json::to_string(&value),
        Err(_) => serde_json::to_string(data),
    };
    let Ok(encoded) = encoded else {
        return String::from(r#"{"truncated":true}"#);
    };
    if encoded.len() > MAX_EVENT_DATA_LEN {
        String::from(r#"{"truncated":true}"#)
    } else if encoded.capacity() > MAX_EVENT_DATA_LEN {
        encoded.as_str().to_owned()
    } else {
        encoded
    }
}

fn truncate_utf8(value: &mut String, limit: usize) {
    if value.len() <= limit && value.capacity() <= limit {
        return;
    }
    let mut cut = value.len().min(limit);
    while !value.is_char_boundary(cut) {
        cut -= 1;
    }
    // `String::truncate` keeps the allocation. Rebuild the bounded slice so a
    // small value cannot retain an attacker-sized backing buffer.
    *value = value[..cut].to_owned();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounds_payload_and_other_code_without_invalid_json() {
        let event = Event {
            camera: CameraId::try_new("cam1").unwrap(),
            source: EventSource::DahuaCgi,
            kind: EventKind::Other("温".repeat(MAX_EVENT_KIND_LEN)),
            state: EventState::Pulse,
            nvr_time: WallUs::new(1),
            camera_time: None,
            data: Some(format!(
                r#"{{"value":"{}"}}"#,
                "度".repeat(MAX_EVENT_DATA_LEN)
            )),
        }
        .bounded();
        let EventKind::Other(code) = event.kind else {
            panic!("other kind changed variant");
        };
        // 1,024 bytes is not a boundary of three-byte characters: 341 fit.
        assert_eq!(code, "温".repeat(341));
        let data = event.data.unwrap();
        assert_eq!(data, r#"{"truncated":true}"#);
        assert!(serde_json::from_str::<serde_json::Value>(&data).is_ok());
    }

    #[test]
    fn bounding_releases_oversized_string_capacity() {
        let mut code = String::with_capacity(MAX_EVENT_KIND_LEN * 128);
        code.push_str("vendor");
        let mut data = String::with_capacity(MAX_EVENT_DATA_LEN * 16);
        data.push_str("{}");
        let event = Event {
            camera: CameraId::try_new("cam1").unwrap(),
            source: EventSource::DahuaCgi,
            kind: EventKind::Other(code),
            state: EventState::Pulse,
            nvr_time: WallUs::new(1),
            camera_time: None,
            data: Some(data),
        }
        .bounded();
        let EventKind::Other(code) = event.kind else {
            panic!("other kind changed variant");
        };
        assert!(code.capacity() <= MAX_EVENT_KIND_LEN);
        assert!(event.data.unwrap().capacity() <= MAX_EVENT_DATA_LEN);
    }

    #[test]
    fn source_and_state_names_round_trip_and_match_serde() {
        for source in [
            EventSource::DahuaCgi,
            EventSource::OnvifPullPoint,
            EventSource::OnvifMetadata,
            EventSource::Manual,
        ] {
            assert_eq!(source.to_string().parse::<EventSource>().unwrap(), source);
            assert_eq!(
                serde_json::to_string(&source).unwrap(),
                format!("\"{source}\"")
            );
        }
        for state in [EventState::Start, EventState::Stop, EventState::Pulse] {
            assert_eq!(state.to_string().parse::<EventState>().unwrap(), state);
            assert_eq!(
                serde_json::to_string(&state).unwrap(),
                format!("\"{state}\"")
            );
        }
        assert!("cgi".parse::<EventSource>().is_err());
        assert!("begin".parse::<EventState>().is_err());
    }

    #[test]
    fn kind_names_match_serde_and_other_shows_its_code() {
        assert_eq!(EventKind::LineCross.to_string(), "line_cross");
        assert_eq!(
            serde_json::to_string(&EventKind::LineCross).unwrap(),
            "\"line_cross\""
        );
        assert_eq!(EventKind::Other("Vendor/Code".into()).name(), "Vendor/Code");
    }

    #[test]
    fn built_in_kinds_round_trip_through_their_names_and_vendor_codes_do_not() {
        for kind in &EventKind::BUILT_IN {
            assert_eq!(kind.name().parse::<EventKind>().as_ref(), Ok(kind));
            assert_eq!(
                serde_json::to_string(kind).unwrap(),
                format!("\"{}\"", kind.name())
            );
        }
        assert_eq!(EventKind::BUILT_IN.len(), 11);
        for code in ["other", "Vendor/Code", ""] {
            assert!(code.parse::<EventKind>().is_err(), "{code:?}");
        }
    }

    #[test]
    fn truncation_cuts_at_a_character_boundary_and_drops_spare_capacity() {
        // "ab" and a three-byte character: a limit inside it cuts before it.
        for (limit, kept) in [
            (5, "ab度"),
            (4, "ab"),
            (3, "ab"),
            (2, "ab"),
            (1, "a"),
            (0, ""),
        ] {
            let mut value = String::from("ab度");
            truncate_utf8(&mut value, limit);
            assert_eq!(value, kept, "limit {limit}");
        }
        let mut roomy = String::with_capacity(4096);
        roomy.push_str("ab");
        truncate_utf8(&mut roomy, 16);
        assert_eq!(roomy, "ab");
        assert!(roomy.capacity() <= 16);
    }

    #[test]
    fn invalid_payload_is_json_quoted() {
        let event = Event {
            camera: CameraId::try_new("cam1").unwrap(),
            source: EventSource::DahuaCgi,
            kind: EventKind::Motion,
            state: EventState::Pulse,
            nvr_time: WallUs::new(1),
            camera_time: None,
            data: Some("plain camera text".to_owned()),
        }
        .bounded();
        let data = event.data.unwrap();
        assert_eq!(data, r#""plain camera text""#);
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&data).unwrap(),
            serde_json::Value::String("plain camera text".to_owned())
        );
    }

    #[test]
    fn compact_json_at_the_limit_remains_valid() {
        let overhead = r#"{"value":""#.len() + r#""}"#.len();
        let payload = format!(
            r#"{{"value":"{}"}}"#,
            "x".repeat(MAX_EVENT_DATA_LEN - overhead)
        );
        assert_eq!(payload.len(), MAX_EVENT_DATA_LEN);
        let event = Event {
            camera: CameraId::try_new("cam1").unwrap(),
            source: EventSource::DahuaCgi,
            kind: EventKind::Motion,
            state: EventState::Pulse,
            nvr_time: WallUs::new(1),
            camera_time: None,
            data: Some(payload.clone()),
        }
        .bounded();
        assert_eq!(event.data.as_deref(), Some(payload.as_str()));
        assert!(serde_json::from_str::<serde_json::Value>(event.data.as_deref().unwrap()).is_ok());
    }

    #[test]
    fn plain_text_encoding_obeys_the_json_encoded_limit() {
        let exact_plain = "x".repeat(MAX_EVENT_DATA_LEN - 2);
        let exact = bound_json(&exact_plain);
        assert_eq!(exact.len(), MAX_EVENT_DATA_LEN);
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&exact).unwrap(),
            serde_json::Value::String(exact_plain)
        );

        let oversized_plain = "x".repeat(MAX_EVENT_DATA_LEN - 1);
        assert_eq!(bound_json(&oversized_plain), r#"{"truncated":true}"#);

        let escaping_expansion = "\0".repeat(MAX_EVENT_DATA_LEN / 2);
        assert_eq!(bound_json(&escaping_expansion), r#"{"truncated":true}"#);
    }
}
