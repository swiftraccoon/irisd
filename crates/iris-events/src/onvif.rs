//! ONVIF pull-point event subscriptions, through `oxvif`.
//!
//! The camera's clock is used only to sign WS-Security headers: its offset is
//! measured with `GetSystemDateAndTime`, and host time is the fallback for
//! cameras that misreport UTC yet check tokens against their true clock. Every
//! event is placed on the NVR clock.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use oxvif::{NotificationMessage, OnvifClient};
use thiserror::Error;
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::backoff::Backoff;
use crate::endpoint::{CameraEndpoint, EventSink};
use crate::stats::EventStats;
use iris_core::event::{Event, EventKind, EventSource, EventState};
use iris_core::id::CameraId;
use iris_core::time::WallUs;

/// Subscription lifetime requested from the camera.
const SUBSCRIPTION_LIFETIME: &str = "PT10M";
/// Renew this long after creating or renewing (Home Assistant's cadence).
const RENEW_EVERY: Duration = Duration::from_mins(8);
/// Long-poll timeout sent in `PullMessages`. The `oxvif` HTTP transport caps every
/// request at 10 s, so a longer value fails on every quiet pull; cameras answer a 5 s
/// pull in about 4 s.
const PULL_TIMEOUT: &str = "PT5S";
/// Messages requested per pull.
const PULL_LIMIT: u32 = 100;
/// Wall-clock bound on one pull (the camera's 5 s, the transport's 10 s, plus slack).
const PULL_DEADLINE: Duration = Duration::from_secs(20);
/// Pause between pulls so a chatty camera cannot spin the loop.
const PULL_COOLDOWN: Duration = Duration::from_millis(750);
/// Bound on every single SOAP call other than the pull.
const CALL_DEADLINE: Duration = Duration::from_secs(20);

#[derive(Debug, Error)]
enum SessionError {
    #[error("ONVIF call failed: {0}")]
    Onvif(#[from] oxvif::OnvifError),
    #[error("camera advertises no events service")]
    NoEventsService,
    #[error("ONVIF call timed out")]
    Timeout,
}

/// Runs a pull-point subscription for one camera until `shutdown` is cancelled.
pub async fn run_onvif_pullpoint(
    endpoint: CameraEndpoint,
    sink: Arc<dyn EventSink>,
    stats: Arc<EventStats>,
    shutdown: CancellationToken,
) {
    let mut backoff = Backoff::new();
    loop {
        backoff.session_started();
        let outcome = tokio::select! {
            () = shutdown.cancelled() => break,
            outcome = session(&endpoint, sink.as_ref(), &stats, &shutdown) => outcome,
        };
        stats.set_connected(false);
        if shutdown.is_cancelled() {
            break;
        }
        match outcome {
            Ok(()) => info!(camera = %endpoint.camera, "ONVIF subscription ended; resubscribing"),
            Err(error) => {
                warn!(camera = %endpoint.camera, %error, "ONVIF subscription failed; resubscribing");
            }
        }
        stats.record_reconnect();
        let delay = backoff.delay();
        tokio::select! {
            () = shutdown.cancelled() => break,
            () = tokio::time::sleep(delay) => {}
        }
    }
    debug!(camera = %endpoint.camera, "ONVIF pull-point stopped");
}

async fn bounded<T>(
    fut: impl Future<Output = Result<T, oxvif::OnvifError>>,
) -> Result<T, SessionError> {
    match timeout(CALL_DEADLINE, fut).await {
        Ok(result) => Ok(result?),
        Err(_) => Err(SessionError::Timeout),
    }
}

/// Finds the events service and creates a pull-point subscription signed by `client`.
async fn subscribe(client: OnvifClient) -> Result<(OnvifClient, String), SessionError> {
    let capabilities = bounded(client.get_capabilities()).await?;
    let events_url = capabilities
        .events
        .url
        .ok_or(SessionError::NoEventsService)?;
    let subscription = bounded(client.create_pull_point_subscription(
        &events_url,
        None,
        Some(SUBSCRIPTION_LIFETIME),
    ))
    .await?;
    Ok((client, subscription.reference_url))
}

async fn session(
    endpoint: &CameraEndpoint,
    sink: &dyn EventSink,
    stats: &EventStats,
    shutdown: &CancellationToken,
) -> Result<(), SessionError> {
    let device_url = format!("{}/onvif/device_service", endpoint.base_url());
    let client =
        OnvifClient::new(device_url).with_credentials(&endpoint.username, &endpoint.password);
    // The camera's clock may be hours off (DST bugs); WS-Security needs its offset.
    let date_time = bounded(client.get_system_date_and_time()).await?;
    let measured = date_time.utc_offset_secs();
    let (client, reference) = match subscribe(client.clone().with_utc_offset(measured)).await {
        Ok(ok) => ok,
        Err(SessionError::NoEventsService) => return Err(SessionError::NoEventsService),
        Err(error) if measured != 0 => {
            // Some cameras misreport UTC by their DST hour yet check the token
            // against their true clock; host time then works.
            debug!(
                camera = %endpoint.camera,
                %error,
                measured,
                "subscribing with the camera's clock failed; retrying with host time"
            );
            let ok = subscribe(client.with_utc_offset(0)).await?;
            info!(
                camera = %endpoint.camera,
                camera_utc_offset_secs = measured,
                "ONVIF tokens signed with host time"
            );
            ok
        }
        Err(error) => return Err(error),
    };
    stats.set_connected(true);
    info!(camera = %endpoint.camera, "ONVIF pull-point subscribed");
    let mut next_renew = Instant::now() + RENEW_EVERY;

    let result = pull_loop(
        &client,
        &reference,
        endpoint,
        sink,
        stats,
        shutdown,
        &mut next_renew,
    )
    .await;
    // Best effort: release the subscription so the camera does not keep it alive.
    if let Err(error) = timeout(Duration::from_secs(5), client.unsubscribe(&reference))
        .await
        .map_err(|_| SessionError::Timeout)
        .and_then(|r| r.map_err(SessionError::from))
    {
        debug!(camera = %endpoint.camera, %error, "unsubscribe failed");
    }
    result
}

async fn pull_loop(
    client: &OnvifClient,
    reference: &str,
    endpoint: &CameraEndpoint,
    sink: &dyn EventSink,
    stats: &EventStats,
    shutdown: &CancellationToken,
    next_renew: &mut Instant,
) -> Result<(), SessionError> {
    loop {
        let started = Instant::now();
        let pulled = tokio::select! {
            () = shutdown.cancelled() => return Ok(()),
            pulled = timeout(PULL_DEADLINE, client.pull_messages(reference, PULL_TIMEOUT, PULL_LIMIT)) => pulled,
        };
        stats.pull_latency().record(started.elapsed());
        let messages = match pulled {
            Ok(Ok(messages)) => messages,
            Ok(Err(error)) => {
                stats.record_error();
                return Err(error.into());
            }
            Err(_) => {
                stats.record_error();
                return Err(SessionError::Timeout);
            }
        };
        let now = WallUs::now();
        // A successful pull proves the subscription is alive even when it is empty.
        stats.record_message(now);
        for message in &messages {
            if let Some(event) = normalize_notification(&endpoint.camera, message, now) {
                debug!(camera = %endpoint.camera, topic = %message.topic, "ONVIF event");
                stats.record_event(now);
                sink.publish(event);
            }
        }
        if Instant::now() >= *next_renew {
            if let Err(error) =
                bounded(client.renew_subscription(reference, SUBSCRIPTION_LIFETIME)).await
            {
                stats.record_error();
                return Err(error);
            }
            stats.record_renewal();
            *next_renew = Instant::now() + RENEW_EVERY;
        }
        tokio::select! {
            () = shutdown.cancelled() => return Ok(()),
            () = tokio::time::sleep(PULL_COOLDOWN) => {}
        }
    }
}

/// Maps an ONVIF topic (with or without a namespace prefix) to a kind.
#[must_use]
pub fn kind_for_topic(topic: &str) -> EventKind {
    let path = topic.rsplit_once(':').map_or(topic, |(_, path)| path);
    if path.ends_with("VideoSource/MotionAlarm")
        || path.ends_with("CellMotionDetector/Motion")
        || path.ends_with("MotionRegionDetector/Motion")
    {
        EventKind::Motion
    } else if path.ends_with("FieldDetector/ObjectsInside") {
        EventKind::Intrusion
    } else if path.ends_with("LineDetector/Crossed") {
        EventKind::LineCross
    } else if path.ends_with("TamperDetector/Tamper") {
        EventKind::Tamper
    } else if path.ends_with("AudioAnalytics/Audio/DetectedSound") {
        EventKind::Audio
    } else if path.ends_with("ObjectDetector/Face") {
        EventKind::Face
    } else {
        EventKind::Other(path.to_owned())
    }
}

/// Derives the state from the message's boolean data item, if any.
fn state_for_data(data: &HashMap<String, String>) -> Option<EventState> {
    const KEYS: [&str; 6] = [
        "State",
        "IsMotion",
        "IsInside",
        "IsTamper",
        "IsSoundDetected",
        "IsDetected",
    ];
    for key in KEYS {
        if let Some(value) = data.get(key) {
            return match value.to_ascii_lowercase().as_str() {
                "true" | "1" => Some(EventState::Start),
                "false" | "0" => Some(EventState::Stop),
                _ => None,
            };
        }
    }
    None
}

/// Normalises a pulled notification. Property snapshots sent at subscription
/// time (`Initialized`) are reported only when the condition is active.
#[must_use]
pub fn normalize_notification(
    camera: &CameraId,
    message: &NotificationMessage,
    now: WallUs,
) -> Option<Event> {
    let kind = kind_for_topic(&message.topic);
    let state = match state_for_data(&message.data) {
        Some(EventState::Stop)
            if message
                .property_operation
                .eq_ignore_ascii_case("Initialized") =>
        {
            return None;
        }
        Some(state) => state,
        None => EventState::Pulse,
    };
    let payload = serde_json::json!({
        "topic": message.topic,
        "operation": message.property_operation,
        "source": message.source,
        "data": message.data,
    });
    Some(
        Event {
            camera: camera.clone(),
            source: EventSource::OnvifPullPoint,
            kind,
            state,
            nvr_time: now,
            camera_time: parse_utc(&message.utc_time),
            data: Some(payload.to_string()),
        }
        .bounded(),
    )
}

/// Parses `YYYY-MM-DDTHH:MM:SS[.fff](Z|±HH:MM)` into wall time.
#[must_use]
pub fn parse_utc(text: &str) -> Option<WallUs> {
    let text = text.trim();
    // Positions below are byte offsets; a timestamp is ASCII, and a non-ASCII
    // string would make them fall inside a character.
    if text.len() < 19 || !text.is_ascii() {
        return None;
    }
    let (date, rest) = text.split_at(10);
    let rest = rest.strip_prefix('T').or_else(|| rest.strip_prefix(' '))?;
    let mut parts = date.split('-');
    let year: i64 = parts.next()?.parse().ok()?;
    let month: u32 = parts.next()?.parse().ok()?;
    let day: u32 = parts.next()?.parse().ok()?;
    let (hms, tail) = rest.split_at(8);
    let mut hms_parts = hms.split(':');
    let hour: i64 = hms_parts.next()?.parse().ok()?;
    let minute: i64 = hms_parts.next()?.parse().ok()?;
    let second: i64 = hms_parts.next()?.parse().ok()?;
    let (frac_micros, zone) = match tail.strip_prefix('.') {
        Some(frac) => {
            let digits: String = frac.chars().take_while(char::is_ascii_digit).collect();
            let zone = &frac[digits.len()..];
            let mut micros = 0i64;
            for (i, c) in digits.chars().take(6).enumerate() {
                micros += i64::from(c.to_digit(10)?) * 10i64.pow(5 - i as u32);
            }
            (micros, zone)
        }
        None => (0, tail),
    };
    let offset_secs = match zone {
        "" | "Z" | "z" => 0,
        z => {
            let sign = match z.chars().next()? {
                '+' => 1,
                '-' => -1,
                _ => return None,
            };
            let (h, m) = z[1..].split_once(':')?;
            let (h, m) = (h.parse::<i64>().ok()?, m.parse::<i64>().ok()?);
            if !(0..=23).contains(&h) || !(0..=59).contains(&m) {
                return None;
            }
            sign * (h * 3600 + m * 60)
        }
    };
    // Bounded fields keep the arithmetic below in range for any input.
    if !(1970..=9999).contains(&year)
        || !(1..=12).contains(&month)
        || !(1..=31).contains(&day)
        || !(0..=23).contains(&hour)
        || !(0..=59).contains(&minute)
        || !(0..=60).contains(&second)
    {
        return None;
    }
    let days = days_from_civil(year, month, day);
    let secs = days * 86_400 + hour * 3600 + minute * 60 + second - offset_secs;
    Some(WallUs::new(secs * 1_000_000 + frac_micros))
}

/// Days since 1970-01-01 for a proleptic Gregorian date (Howard Hinnant).
fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let m = i64::from(month);
    let d = i64::from(day);
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

#[cfg(test)]
mod tests {
    use super::*;

    fn message(topic: &str, data: &[(&str, &str)], operation: &str) -> NotificationMessage {
        NotificationMessage {
            topic: topic.to_owned(),
            utc_time: "2026-10-31T16:35:59Z".to_owned(),
            property_operation: operation.to_owned(),
            source: HashMap::from([("VideoSourceConfigurationToken".to_owned(), "0".to_owned())]),
            data: data
                .iter()
                .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                .collect(),
        }
    }

    #[test]
    fn maps_topics_with_and_without_prefix() {
        assert_eq!(
            kind_for_topic("tns1:VideoSource/MotionAlarm"),
            EventKind::Motion
        );
        assert_eq!(
            kind_for_topic("tns1:RuleEngine/CellMotionDetector/Motion"),
            EventKind::Motion
        );
        assert_eq!(
            kind_for_topic("RuleEngine/FieldDetector/ObjectsInside"),
            EventKind::Intrusion
        );
        assert_eq!(
            kind_for_topic("tns1:RuleEngine/LineDetector/Crossed"),
            EventKind::LineCross
        );
        assert_eq!(
            kind_for_topic("tns1:RuleEngine/TamperDetector/Tamper"),
            EventKind::Tamper
        );
        assert_eq!(
            kind_for_topic("tns1:AudioAnalytics/Audio/DetectedSound"),
            EventKind::Audio
        );
        assert_eq!(
            kind_for_topic("tns1:RuleEngine/ObjectDetector/Face"),
            EventKind::Face
        );
        assert_eq!(
            kind_for_topic("tns1:Monitoring/IPConflict"),
            EventKind::Other("Monitoring/IPConflict".into())
        );
    }

    #[test]
    fn derives_state_and_camera_time() {
        let camera = CameraId::try_new("cam1").unwrap();
        let start = normalize_notification(
            &camera,
            &message(
                "tns1:VideoSource/MotionAlarm",
                &[("State", "true")],
                "Changed",
            ),
            WallUs::new(1),
        )
        .unwrap();
        assert_eq!(
            (start.kind.clone(), start.state),
            (EventKind::Motion, EventState::Start)
        );
        assert_eq!(start.camera_time, Some(WallUs::new(1_793_464_559_000_000)));
        assert_eq!(start.source, EventSource::OnvifPullPoint);
        let stop = normalize_notification(
            &camera,
            &message(
                "tns1:RuleEngine/CellMotionDetector/Motion",
                &[("IsMotion", "false")],
                "Changed",
            ),
            WallUs::new(1),
        )
        .unwrap();
        assert_eq!(stop.state, EventState::Stop);
        assert!(
            normalize_notification(
                &camera,
                &message(
                    "tns1:VideoSource/MotionAlarm",
                    &[("State", "false")],
                    "Initialized"
                ),
                WallUs::new(1)
            )
            .is_none()
        );
        let pulse = normalize_notification(
            &camera,
            &message(
                "tns1:RuleEngine/LineDetector/Crossed",
                &[("ObjectId", "7")],
                "Changed",
            ),
            WallUs::new(1),
        )
        .unwrap();
        assert_eq!(pulse.state, EventState::Pulse);
        assert!(
            pulse
                .data
                .as_deref()
                .unwrap()
                .contains("\"ObjectId\":\"7\"")
        );
    }

    #[test]
    fn rejects_non_ascii_without_panicking() {
        // A fuzz finding: multibyte characters made the byte offsets fall inside a character.
        let text = String::from_utf8_lossy(&[0xff, 0xff, 0xff, 0x8a, 0xff, 0xdd, 0xdd, 0x0a]);
        assert_eq!(parse_utc(&text), None);
        assert_eq!(parse_utc("2026-11-01T00:00:00Ζ"), None);
        assert!(parse_utc("2026-11-01T00:00:00Z").is_some());
    }

    #[test]
    fn out_of_range_fields_are_rejected_without_overflow() {
        // A fuzz finding: a zone hour of many digits overflowed the offset multiplication.
        let text = String::from_utf8_lossy(&[
            0x20, 0x20, 0x20, 0x20, 0x30, 0x2d, 0x30, 0x30, 0x2d, 0x30, 0x2d, 0x30, 0x20, 0x41,
            0x20, 0x2d, 0x36, 0x31, 0x3a, 0x31, 0x3a, 0x38, 0x38, 0x2e, 0x30, 0x2d, 0x2d, 0x30,
            0x30, 0x31, 0x37, 0x37, 0x37, 0x37, 0x37, 0x37, 0x37, 0x37, 0x37, 0x37, 0x37, 0x37,
            0x37, 0x37, 0x37, 0x37, 0x36, 0x31, 0x3a, 0x31, 0x3a, 0x38, 0x38, 0x2e, 0x30, 0x2d,
            0x2d, 0x30, 0x30, 0x31, 0x37, 0x37, 0x37, 0x37, 0x37, 0x37, 0x37, 0x37, 0x37, 0x37,
            0x37, 0x37, 0x37, 0x37, 0x37, 0x37, 0x37, 0x37, 0x37, 0x37, 0x37, 0x37, 0x37, 0x37,
            0x37, 0x37, 0x37, 0x37, 0x37, 0x37, 0x37, 0x37, 0x3f, 0x37, 0x37, 0x37, 0x37, 0x37,
            0x37, 0x37, 0x37, 0x37, 0x37, 0x37, 0x37, 0x37, 0x37, 0x37, 0x37, 0x3f, 0x37, 0x37,
            0x37, 0x37, 0x37, 0x37, 0x37, 0x37, 0x37, 0x31, 0x3a, 0x31, 0x3a, 0x38, 0x1e,
        ]);
        assert_eq!(parse_utc(&text), None);
        assert_eq!(parse_utc("2026-11-01T00:00:00+99999999999999:00"), None);
        assert_eq!(parse_utc("2026-11-01T25:00:00Z"), None);
        assert_eq!(parse_utc("99999999-01-01T00:00:00Z"), None);
        assert!(parse_utc("2026-11-01T23:59:60Z").is_some());
    }

    #[test]
    fn parses_iso8601_variants() {
        assert_eq!(parse_utc("1970-01-01T00:00:00Z"), Some(WallUs::new(0)));
        assert_eq!(
            parse_utc("2026-10-31T16:35:59.250Z"),
            Some(WallUs::new(1_793_464_559_250_000))
        );
        assert_eq!(
            parse_utc("2026-10-31T12:35:59-04:00"),
            Some(WallUs::new(1_793_464_559_000_000))
        );
        assert_eq!(
            parse_utc("2026-10-31 16:35:59"),
            Some(WallUs::new(1_793_464_559_000_000))
        );
        assert_eq!(parse_utc("garbage"), None);
        assert_eq!(parse_utc("2026-13-01T00:00:00Z"), None);
    }
}
