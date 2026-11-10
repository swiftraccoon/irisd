//! Dahua CGI event parts: parsing and normalisation.
//!
//! A part of the `eventManager.cgi?action=attach` feed looks like
//! `Code=VideoMotion;action=Start;index=0` optionally followed by
//! `;data={ … }` whose JSON may span several lines and runs to the end of
//! the part. A part whose body is `Heartbeat` carries no event.

use iris_core::event::{Event, EventKind, EventSource, EventState};
use iris_core::id::CameraId;
use iris_core::time::WallUs;

/// One event part before normalisation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RawEvent {
    /// Vendor event code, for example `VideoMotion`.
    pub code: String,
    /// `Start`, `Stop`, `Pulse`, or `State` from well-behaved firmware; any
    /// text the camera sent is kept, and a missing field reads `Pulse`.
    pub action: String,
    /// Channel or rule index: 0 when missing or not a number. An index of 0
    /// is left out of the normalised payload.
    pub index: u32,
    /// The `data=` payload, usually JSON, if present.
    pub data: Option<String>,
}

/// Parses one part body. Returns `None` for heartbeats, empty bodies, and
/// bodies that do not start with `Code=`; see [`RawEvent`] for the defaults
/// of the other fields.
#[must_use]
pub fn parse_part(body: &str) -> Option<RawEvent> {
    let body = body.trim();
    if body.is_empty() || !body.starts_with("Code=") {
        return None;
    }
    let (fields, data) = match body.find(";data=") {
        Some(at) => (
            &body[..at],
            Some(body[at + ";data=".len()..].trim().to_owned()),
        ),
        None => (body, None),
    };
    let mut code = None;
    let mut action = String::from("Pulse");
    let mut index = 0u32;
    for field in fields.split(';') {
        let Some((key, value)) = field.split_once('=') else {
            continue;
        };
        match key.trim() {
            "Code" => code = Some(value.trim().to_owned()),
            "action" => value.trim().clone_into(&mut action),
            "index" => index = value.trim().parse().unwrap_or(0),
            _ => {}
        }
    }
    let code = code.filter(|c| !c.is_empty())?;
    Some(RawEvent {
        code,
        action,
        index,
        data: data.filter(|d| !d.is_empty()),
    })
}

/// Maps a Dahua code to the normalised kind.
#[must_use]
pub fn kind_for_code(code: &str) -> EventKind {
    match code {
        "TimeChange" | "NTPAdjustTime" => EventKind::TimeChange,
        "VideoMotion" => EventKind::Motion,
        "SmartMotionHuman" => EventKind::Human,
        "SmartMotionVehicle" => EventKind::Vehicle,
        "CrossLineDetection" => EventKind::LineCross,
        "CrossRegionDetection" => EventKind::Intrusion,
        "VideoBlind" => EventKind::Tamper,
        "AudioMutation" | "AudioAnomaly" => EventKind::Audio,
        "FaceDetection" => EventKind::Face,
        "VideoLoss" => EventKind::VideoLoss,
        "RtspSessionDisconnect" => EventKind::Disconnect,
        other => EventKind::Other(other.to_owned()),
    }
}

/// Maps a Dahua `action` to the normalised state; `State` reports are pulses.
#[must_use]
pub fn state_for_action(action: &str) -> EventState {
    match action {
        "Start" => EventState::Start,
        "Stop" => EventState::Stop,
        _ => EventState::Pulse,
    }
}

/// Normalises a raw part. `now` is the NVR receipt time. Heartbeats never
/// reach this function; a `Heartbeat` code is still rejected for safety.
#[must_use]
pub fn normalize(camera: &CameraId, raw: &RawEvent, now: WallUs) -> Option<Event> {
    if raw.code == "Heartbeat" {
        return None;
    }
    let vendor_data = raw.data.as_ref().map(|data| {
        serde_json::from_str::<serde_json::Value>(data)
            .unwrap_or_else(|_| serde_json::Value::String(data.clone()))
    });
    let payload = match (raw.index, vendor_data) {
        (0, None) => None,
        (0, Some(data)) => Some(data),
        (index, None) => Some(serde_json::json!({ "index": index })),
        (index, Some(data)) => Some(serde_json::json!({ "index": index, "data": data })),
    };
    let data = payload.and_then(|value| serde_json::to_string(&value).ok());
    Some(
        Event {
            camera: camera.clone(),
            source: EventSource::DahuaCgi,
            kind: kind_for_code(&raw.code),
            state: state_for_action(&raw.action),
            nvr_time: now,
            camera_time: None,
            data,
        }
        .bounded(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cam() -> CameraId {
        CameraId::try_new("cam1").unwrap()
    }

    #[test]
    fn heartbeat_and_empty_bodies_are_not_events() {
        assert_eq!(parse_part("Heartbeat"), None);
        assert_eq!(parse_part("\r\nHeartbeat\r\n"), None);
        assert_eq!(parse_part(""), None);
        assert_eq!(parse_part("Code=;action=Start"), None);
    }

    #[test]
    fn parses_a_motion_start() {
        let raw = parse_part("Code=VideoMotion;action=Start;index=0\r\n").unwrap();
        assert_eq!(
            raw,
            RawEvent {
                code: "VideoMotion".into(),
                action: "Start".into(),
                index: 0,
                data: None
            }
        );
    }

    #[test]
    fn a_missing_action_or_index_takes_the_documented_default() {
        let raw = parse_part("Code=VideoMotion;index=notanumber").unwrap();
        assert_eq!((raw.action.as_str(), raw.index), ("Pulse", 0));
        let raw = parse_part("Code=VideoMotion;action=Odd").unwrap();
        assert_eq!(raw.action, "Odd");
        assert_eq!(state_for_action(&raw.action), EventState::Pulse);
    }

    #[test]
    fn state_reports_are_pulses() {
        let raw = parse_part("Code=VideoMotionInfo;action=State;index=0").unwrap();
        assert_eq!(raw.action, "State");
        let event = normalize(&cam(), &raw, WallUs::new(5)).unwrap();
        assert_eq!(event.kind, EventKind::Other("VideoMotionInfo".into()));
        assert_eq!(event.state, EventState::Pulse);
        assert_eq!(event.nvr_time, WallUs::new(5));
    }

    #[test]
    fn keeps_multi_line_json_data() {
        let body = "Code=TimeChange;action=Pulse;index=0;data={\n   \"BeforeModifyTime\" : \"2026-10-31 12:00:00\",\n   \"ModifiedTime\" : \"2026-10-31 12:00:00\"\n}\n\n";
        let raw = parse_part(body).unwrap();
        assert_eq!(raw.code, "TimeChange");
        let data = raw.data.as_deref().unwrap();
        assert!(data.starts_with('{') && data.ends_with('}'));
        assert!(data.contains("ModifiedTime"));
        let event = normalize(&cam(), &raw, WallUs::new(0)).unwrap();
        assert_eq!(event.kind, EventKind::TimeChange);
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(event.data.as_deref().unwrap()).unwrap(),
            serde_json::from_str::<serde_json::Value>(data).unwrap()
        );
    }

    #[test]
    fn maps_codes_and_actions() {
        assert_eq!(kind_for_code("SmartMotionHuman"), EventKind::Human);
        assert_eq!(kind_for_code("SmartMotionVehicle"), EventKind::Vehicle);
        assert_eq!(kind_for_code("CrossLineDetection"), EventKind::LineCross);
        assert_eq!(kind_for_code("CrossRegionDetection"), EventKind::Intrusion);
        assert_eq!(kind_for_code("VideoBlind"), EventKind::Tamper);
        assert_eq!(kind_for_code("AudioAnomaly"), EventKind::Audio);
        assert_eq!(kind_for_code("FaceDetection"), EventKind::Face);
        assert_eq!(kind_for_code("VideoLoss"), EventKind::VideoLoss);
        assert_eq!(
            kind_for_code("RtspSessionDisconnect"),
            EventKind::Disconnect
        );
        assert_eq!(kind_for_code("NTPAdjustTime"), EventKind::TimeChange);
        assert_eq!(kind_for_code("NewFile"), EventKind::Other("NewFile".into()));
        assert_eq!(state_for_action("Start"), EventState::Start);
        assert_eq!(state_for_action("Stop"), EventState::Stop);
        assert_eq!(state_for_action("State"), EventState::Pulse);
        assert_eq!(state_for_action("Pulse"), EventState::Pulse);
        assert_eq!(state_for_action("start"), EventState::Pulse);
    }

    #[test]
    fn index_is_kept_with_the_payload() {
        let raw =
            parse_part("Code=CrossLineDetection;action=Start;index=2;data={\"Name\":\"rule\"}")
                .unwrap();
        let event = normalize(&cam(), &raw, WallUs::new(0)).unwrap();
        let payload: serde_json::Value =
            serde_json::from_str(event.data.as_deref().unwrap()).unwrap();
        assert_eq!(payload["index"], 2);
        assert_eq!(payload["data"]["Name"], "rule");
    }

    #[test]
    fn opaque_data_is_quoted() {
        let raw = parse_part("Code=VideoMotion;action=Start;index=2;data=plain").unwrap();
        let event = normalize(&cam(), &raw, WallUs::new(0)).unwrap();
        let payload: serde_json::Value =
            serde_json::from_str(event.data.as_deref().unwrap()).unwrap();
        assert_eq!(payload, serde_json::json!({"index": 2, "data": "plain"}));
    }

    #[test]
    fn a_nonzero_index_survives_without_data() {
        let raw = parse_part("Code=VideoMotion;action=Start;index=3").unwrap();
        let event = normalize(&cam(), &raw, WallUs::new(0)).unwrap();
        let payload: serde_json::Value =
            serde_json::from_str(event.data.as_deref().unwrap()).unwrap();
        assert_eq!(payload, serde_json::json!({"index": 3}));
    }
}
