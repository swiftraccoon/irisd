//! Normalised camera events shared by the event sources, the catalog, and the API.

use serde::{Deserialize, Serialize};

use crate::id::CameraId;
use crate::time::WallUs;

/// Largest vendor payload kept with an event, in bytes.
pub const MAX_EVENT_DATA_LEN: usize = 64 * 1024;

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
    /// Any other vendor code, kept verbatim.
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
    /// Vendor payload as JSON text, truncated to [`MAX_EVENT_DATA_LEN`].
    pub data: Option<String>,
}

impl Event {
    /// Truncates `data` to the maximum kept with an event.
    #[must_use]
    pub fn bounded(mut self) -> Self {
        if let Some(data) = self.data.as_mut()
            && data.len() > MAX_EVENT_DATA_LEN
        {
            let mut cut = MAX_EVENT_DATA_LEN;
            while !data.is_char_boundary(cut) {
                cut -= 1;
            }
            data.truncate(cut);
        }
        self
    }
}
