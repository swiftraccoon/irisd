//! Normalised camera events shared by the event sources, the catalog, and the API.

use serde::{Deserialize, Serialize};

use crate::id::CameraId;
use crate::time::WallUs;

/// Largest vendor payload kept with an event, in bytes.
pub const MAX_EVENT_DATA_LEN: usize = 64 * 1024;
/// Largest unmapped vendor event code kept in [`EventKind::Other`], in bytes.
pub const MAX_EVENT_KIND_LEN: usize = 1024;

/// Where an event came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventSource {
    /// The Dahua HTTP CGI push feed.
    DahuaCgi,
    /// An ONVIF `PullPoint` subscription.
    OnvifPullPoint,
    /// The ONVIF metadata RTP track.
    OnvifMetadata,
    /// Entered by a person through the API.
    Manual,
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
    /// Any other vendor code, kept up to [`MAX_EVENT_KIND_LEN`] bytes.
    Other(String),
}

/// The phase of an event.
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
    /// Vendor payload as valid JSON text bounded to [`MAX_EVENT_DATA_LEN`].
    ///
    /// Invalid input is represented as a JSON string. An encoding beyond the
    /// limit is replaced by `{"truncated":true}` rather than sliced.
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
        assert!(code.len() <= MAX_EVENT_KIND_LEN);
        assert!(code.is_char_boundary(code.len()));
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
