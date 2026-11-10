//! ONVIF pull-point event subscriptions, through `oxvif`.
//!
//! The camera's clock is used only to sign WS-Security headers: its offset is
//! measured with `GetSystemDateAndTime`, and host time is the fallback for
//! cameras that misreport UTC yet check tokens against their true clock. Every
//! event is placed on the NVR clock.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use iris_core::event::{Event, EventKind, EventSource, EventState};
use iris_core::id::CameraId;
use iris_core::text::LogSafe;
use iris_core::time::{self, WallUs};
use oxvif::soap::SoapError;
use oxvif::transport::{Transport, TransportError};
use oxvif::{NotificationMessage, OnvifClient, OnvifError, PullPointSubscription};
use thiserror::Error;
use tokio::time::{Instant, timeout};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::backoff::Backoff;
use crate::endpoint::CameraEndpoint;
use crate::sink::EventSink;
use crate::stats::EventStats;
use crate::transport::SoapTransport;

/// Subscription lifetime requested from the camera.
const SUBSCRIPTION_LIFETIME: &str = "PT10M";
/// Longest renewal delay, and the delay when the granted lease times are
/// unusable (Home Assistant's cadence).
const RENEW_EVERY: Duration = Duration::from_mins(8);
/// Long-poll timeout sent in `PullMessages`. The SOAP transport caps every
/// request at 10 s, so a longer value fails on every quiet pull; cameras answer a 5 s
/// pull in about 4 s.
const PULL_TIMEOUT: &str = "PT5S";
/// Messages requested per pull.
const PULL_LIMIT: u32 = 100;
/// Wall-clock bound on one pull (the camera's 5 s, the transport's 10 s, plus slack).
const PULL_DEADLINE: Duration = Duration::from_secs(20);
/// Pause between pulls so a chatty camera cannot spin the loop.
const PULL_COOLDOWN: Duration = Duration::from_millis(750);
/// Minimum spacing between successful renewal attempts, including unusably tiny leases.
const MIN_RENEW_DELAY: Duration = PULL_COOLDOWN;
/// Bound on every single SOAP call other than the pull and the unsubscribe.
const CALL_DEADLINE: Duration = Duration::from_secs(20);
/// Bound on the unsubscribe that releases a subscription when a session ends.
const UNSUBSCRIBE_DEADLINE: Duration = Duration::from_secs(5);
/// Minimum retry pause after rejected ONVIF credentials.
const AUTH_FAILURE_PAUSE: Duration = Duration::from_secs(30);

/// Why an ONVIF call or session failed.
///
/// `Display` may carry camera text (a SOAP fault's code and reason, an XML
/// parser message); a log line takes it through
/// [`LogSafe`].
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum SessionError {
    /// The camera refused or garbled a call.
    #[error("ONVIF call failed: {0}")]
    Onvif(#[from] OnvifError),
    /// The camera's capabilities name no events service.
    #[error("camera advertises no events service")]
    NoEventsService,
    /// A call exceeded its deadline.
    #[error("ONVIF call timed out")]
    Timeout,
    /// The HTTP client could not be built.
    #[error("cannot build ONVIF HTTP client: {0}")]
    HttpClient(#[from] reqwest::Error),
    /// The session's shutdown token was cancelled.
    #[error("ONVIF session cancelled")]
    Cancelled,
    /// The endpoint's host is not a bare host name or IP address, so no URL
    /// can be built for it ([`iris_core::config::is_valid_host`]).
    #[error("camera host is not a bare host name or address")]
    InvalidEndpoint,
}

impl SessionError {
    /// Whether a call signed with the camera's measured clock offset should be
    /// repeated with host time: only after a structured clock fault, and only
    /// when `measured` differs from host time.
    pub(crate) fn retries_with_host_time(&self, measured: i64) -> bool {
        measured != 0 && self.is_clock_authentication_failure()
    }

    fn is_authentication_failure(&self) -> bool {
        let Self::Onvif(error) = self else {
            return false;
        };
        match error {
            OnvifError::Transport(TransportError::HttpStatus { status: 401, .. }) => true,
            OnvifError::Soap(SoapError::Fault {
                code,
                reason,
                subcode,
                ..
            }) => {
                let fields = [
                    Some(code.as_str()),
                    subcode.as_deref(),
                    Some(reason.as_str()),
                ];
                fields
                    .into_iter()
                    .flatten()
                    .any(|field| is_auth_fault_marker(field) || is_clock_fault_marker(field))
            }
            _ => false,
        }
    }

    fn is_clock_authentication_failure(&self) -> bool {
        let Self::Onvif(OnvifError::Soap(SoapError::Fault {
            code,
            reason,
            subcode,
            ..
        })) = self
        else {
            return false;
        };
        [
            Some(code.as_str()),
            subcode.as_deref(),
            Some(reason.as_str()),
        ]
        .into_iter()
        .flatten()
        .any(is_clock_fault_marker)
    }
}

fn is_auth_fault_marker(value: &str) -> bool {
    let marker = value.to_ascii_lowercase();
    marker.contains("notauthorized")
        || marker.contains("not authorized")
        || marker.contains("failedauthentication")
        || marker.contains("failed authentication")
        || marker.contains("invalidsecurity")
        || marker.contains("invalid security")
}

fn is_clock_fault_marker(value: &str) -> bool {
    let marker: String = value
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .flat_map(char::to_lowercase)
        .collect();
    // "time check" is the wording of the hardware fault recorded in
    // `docs/research/11-multi-camera-results.md`.
    marker.contains("messageexpired")
        || marker.contains("invalidtimestamp")
        || marker.contains("clockskew")
        || marker.contains("timecheck")
}

/// Runs a pull-point subscription for one camera until `shutdown` is cancelled.
pub async fn run_onvif_pullpoint(
    endpoint: CameraEndpoint,
    sink: Arc<dyn EventSink>,
    stats: Arc<EventStats>,
    shutdown: CancellationToken,
) {
    let mut backoff = Backoff::new(&format!("{}/onvif", endpoint.camera));
    loop {
        if shutdown.is_cancelled() {
            break;
        }
        let outcome = session(&endpoint, sink.as_ref(), &stats, &shutdown, &mut backoff).await;
        stats.set_connected(false);
        if shutdown.is_cancelled() || matches!(outcome, Err(SessionError::Cancelled)) {
            break;
        }
        match &outcome {
            Ok(()) => info!(camera = %endpoint.camera, "ONVIF subscription ended; resubscribing"),
            Err(error) => {
                warn!(
                    camera = %endpoint.camera,
                    error = %LogSafe(error),
                    "ONVIF subscription failed; resubscribing"
                );
            }
        }
        let Some(authentication_failure) = account_retry_cycle(&stats, &outcome) else {
            break;
        };
        let delay = retry_delay(&mut backoff, authentication_failure);
        if !wait_for_retry(&shutdown, delay).await {
            break;
        }
    }
    debug!(camera = %endpoint.camera, "ONVIF pull-point stopped");
}

fn retry_delay(backoff: &mut Backoff, authentication_failure: bool) -> Duration {
    let delay = backoff.delay();
    if authentication_failure {
        delay.max(AUTH_FAILURE_PAUSE)
    } else {
        delay
    }
}

fn account_retry_cycle(stats: &EventStats, outcome: &Result<(), SessionError>) -> Option<bool> {
    if matches!(outcome, Err(SessionError::Cancelled)) {
        return None;
    }
    let authentication_failure = outcome
        .as_ref()
        .is_err_and(SessionError::is_authentication_failure);
    if outcome.is_err() {
        stats.record_error();
    }
    stats.record_reconnect();
    Some(authentication_failure)
}

async fn wait_for_retry(shutdown: &CancellationToken, delay: Duration) -> bool {
    tokio::select! {
        () = shutdown.cancelled() => false,
        () = tokio::time::sleep(delay) => true,
    }
}

/// The device-service client of `endpoint`, speaking through `transport`
/// with the endpoint's credentials.
pub(crate) fn device_client(
    endpoint: &CameraEndpoint,
    transport: Arc<dyn Transport>,
) -> OnvifClient {
    OnvifClient::new(format!("{}/onvif/device_service", endpoint.base_url()))
        .with_transport(transport)
        .with_credentials(&endpoint.username, &endpoint.password)
}

/// The camera's UTC offset in seconds, from `GetSystemDateAndTime`. The
/// camera's clock may be hours off (DST bugs), and WS-Security tokens are
/// signed with it.
pub(crate) async fn measure_clock_offset(
    shutdown: &CancellationToken,
    client: &OnvifClient,
) -> Result<i64, SessionError> {
    let date_time = bounded(shutdown, client.get_system_date_and_time()).await?;
    Ok(date_time.utc_offset_secs())
}

/// Runs one SOAP call under the 20 s call deadline, ending early on
/// `shutdown`.
pub(crate) async fn bounded<T>(
    shutdown: &CancellationToken,
    fut: impl Future<Output = Result<T, OnvifError>>,
) -> Result<T, SessionError> {
    bounded_with_deadline(shutdown, CALL_DEADLINE, fut).await
}

async fn bounded_with_deadline<T>(
    shutdown: &CancellationToken,
    deadline: Duration,
    fut: impl Future<Output = Result<T, OnvifError>>,
) -> Result<T, SessionError> {
    tokio::select! {
        () = shutdown.cancelled() => Err(SessionError::Cancelled),
        result = timeout(deadline, fut) => match result {
            Ok(result) => Ok(result?),
            Err(_) => Err(SessionError::Timeout),
        }
    }
}

/// Finds the events service and creates a pull-point subscription signed by
/// `client`, bounding each call by `deadline`.
///
/// The advertised events service and subscription reference are rebased onto
/// the configured origin ([`SoapTransport::rebase_url`]) before use: the events
/// URL before the subscription exists, so a camera that advertises an
/// unreachable address still subscribes, and the reference before any later
/// call, so pulls, renewals, and the unsubscribe reach the configured camera.
async fn subscribe_with_deadline(
    client: OnvifClient,
    transport: &SoapTransport,
    shutdown: &CancellationToken,
    deadline: Duration,
) -> Result<(OnvifClient, PullPointSubscription), SessionError> {
    let capabilities = bounded_with_deadline(shutdown, deadline, client.get_capabilities()).await?;
    let events_url = capabilities
        .events
        .url
        .ok_or(SessionError::NoEventsService)?;
    let events_url = transport
        .rebase_url(&events_url)
        .map_err(OnvifError::from)?;
    let mut subscription = bounded_with_deadline(
        shutdown,
        deadline,
        client.create_pull_point_subscription(
            events_url.as_str(),
            None,
            Some(SUBSCRIPTION_LIFETIME),
        ),
    )
    .await?;
    subscription.reference_url = transport
        .rebase_url(&subscription.reference_url)
        .map_err(OnvifError::from)?
        .to_string();
    Ok((client, subscription))
}

async fn subscribe_with_clock_fallback(
    client: OnvifClient,
    transport: &SoapTransport,
    shutdown: &CancellationToken,
    measured: i64,
    deadline: Duration,
) -> Result<(OnvifClient, PullPointSubscription, Option<SessionError>), SessionError> {
    match subscribe_with_deadline(
        client.clone().with_utc_offset(measured),
        transport,
        shutdown,
        deadline,
    )
    .await
    {
        Ok((client, subscription)) => Ok((client, subscription, None)),
        Err(error) if error.retries_with_host_time(measured) => {
            let (client, subscription) =
                subscribe_with_deadline(client.with_utc_offset(0), transport, shutdown, deadline)
                    .await?;
            Ok((client, subscription, Some(error)))
        }
        Err(error) => Err(error),
    }
}

async fn session(
    endpoint: &CameraEndpoint,
    sink: &dyn EventSink,
    stats: &EventStats,
    shutdown: &CancellationToken,
    backoff: &mut Backoff,
) -> Result<(), SessionError> {
    let transport = Arc::new(SoapTransport::new(endpoint)?);
    let client = device_client(endpoint, transport.clone());
    let measured = measure_clock_offset(shutdown, &client).await?;
    let (client, subscription, fallback_error) = subscribe_with_clock_fallback(
        client,
        transport.as_ref(),
        shutdown,
        measured,
        CALL_DEADLINE,
    )
    .await?;
    if let Some(error) = fallback_error {
        // Some cameras misreport UTC by their DST hour yet check the token
        // against their true clock; host time then works.
        debug!(
            camera = %endpoint.camera,
            error = %LogSafe(&error),
            measured,
            "subscribing with the camera's clock failed; retried with host time"
        );
        info!(
            camera = %endpoint.camera,
            camera_utc_offset_secs = measured,
            "ONVIF tokens signed with host time"
        );
    }
    let next_renew_delay =
        renewal_delay(&subscription.current_time, &subscription.termination_time);
    let reference = subscription.reference_url;
    backoff.session_connected();
    info!(camera = %endpoint.camera, "ONVIF pull-point subscribed");
    active_subscription(
        &client,
        &reference,
        endpoint,
        sink,
        stats,
        shutdown,
        Instant::now() + next_renew_delay,
    )
    .await
}

async fn active_subscription(
    client: &OnvifClient,
    reference: &str,
    endpoint: &CameraEndpoint,
    sink: &dyn EventSink,
    stats: &EventStats,
    shutdown: &CancellationToken,
    mut next_renew: Instant,
) -> Result<(), SessionError> {
    stats.set_connected(true);
    let result = pull_loop(
        client,
        reference,
        endpoint,
        sink,
        stats,
        shutdown,
        &mut next_renew,
    )
    .await;
    // Best effort: release the subscription so the camera does not keep it alive.
    if let Err(error) = timeout(UNSUBSCRIBE_DEADLINE, client.unsubscribe(reference))
        .await
        .map_err(|_| SessionError::Timeout)
        .and_then(|r| r.map_err(SessionError::from))
    {
        debug!(camera = %endpoint.camera, error = %LogSafe(&error), "unsubscribe failed");
    }
    stats.set_connected(false);
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
        if Instant::now() >= *next_renew {
            renew_active_subscription(client, reference, stats, shutdown, next_renew).await?;
        }
        let started = Instant::now();
        let pulled = timeout(
            PULL_DEADLINE,
            client.pull_messages(reference, PULL_TIMEOUT, PULL_LIMIT),
        );
        tokio::pin!(pulled);
        let messages = loop {
            tokio::select! {
                () = shutdown.cancelled() => return Ok(()),
                result = &mut pulled => {
                    stats.pull_latency().record(started.elapsed());
                    break match result {
                        Ok(Ok(messages)) => messages,
                        Ok(Err(error)) => return Err(error.into()),
                        Err(_) => return Err(SessionError::Timeout),
                    };
                }
                () = tokio::time::sleep_until(*next_renew) => {
                    renew_active_subscription(client, reference, stats, shutdown, next_renew).await?;
                }
            }
        };
        let now = WallUs::now();
        // A successful pull proves the subscription is alive even when it is empty.
        stats.record_message(now);
        for message in &messages {
            if let Some(event) = normalize_notification(&endpoint.camera, message, now) {
                debug!(camera = %endpoint.camera, topic = %LogSafe(&message.topic), "ONVIF event");
                stats.record_event(now);
                sink.publish(event);
            }
        }
        let cooldown_until = Instant::now() + PULL_COOLDOWN;
        loop {
            tokio::select! {
                () = shutdown.cancelled() => return Ok(()),
                () = tokio::time::sleep_until(cooldown_until) => break,
                () = tokio::time::sleep_until(*next_renew) => {
                    renew_active_subscription(client, reference, stats, shutdown, next_renew).await?;
                }
            }
        }
    }
}

async fn renew_active_subscription(
    client: &OnvifClient,
    reference: &str,
    stats: &EventStats,
    shutdown: &CancellationToken,
    next_renew: &mut Instant,
) -> Result<(), SessionError> {
    let lease = bounded(
        shutdown,
        client.renew_subscription(reference, SUBSCRIPTION_LIFETIME),
    )
    .await?;
    stats.record_renewal();
    *next_renew = Instant::now() + renewal_delay(&lease.current_time, &lease.termination_time);
    Ok(())
}

fn renewal_delay(current_time: &str, termination_time: &str) -> Duration {
    let Some(current) = parse_utc(current_time) else {
        return RENEW_EVERY;
    };
    let Some(termination) = parse_utc(termination_time) else {
        return RENEW_EVERY;
    };
    let Some(lease_micros) = termination.get().checked_sub(current.get()) else {
        return RENEW_EVERY;
    };
    let Ok(lease_micros) = u64::try_from(lease_micros) else {
        return RENEW_EVERY;
    };
    let renew_nanos = (u128::from(lease_micros) * 1_000 * 4 / 5)
        .clamp(MIN_RENEW_DELAY.as_nanos(), RENEW_EVERY.as_nanos());
    Duration::from_nanos(
        u64::try_from(renew_nanos).expect("maximum renewal delay fits in nanoseconds"),
    )
}

/// Maps an ONVIF topic (with or without namespace prefixes) to a kind by its
/// last segments; an unknown topic is kept as [`EventKind::Other`].
///
/// ```
/// use iris_core::event::EventKind;
/// use iris_events::onvif::kind_for_topic;
///
/// assert_eq!(kind_for_topic("tns1:VideoSource/MotionAlarm"), EventKind::Motion);
/// assert_eq!(kind_for_topic("tns1:RuleEngine/LineDetector/Crossed"), EventKind::LineCross);
/// assert_eq!(kind_for_topic("tns1:Vendor/Rule"), EventKind::Other("tns1:Vendor/Rule".to_owned()));
/// ```
#[must_use]
pub fn kind_for_topic(topic: &str) -> EventKind {
    let segments: Vec<&str> = topic
        .split('/')
        .map(|segment| segment.split_once(':').map_or(segment, |(_, local)| local))
        .collect();
    if has_topic_suffix(&segments, &["VideoSource", "MotionAlarm"])
        || has_topic_suffix(&segments, &["CellMotionDetector", "Motion"])
        || has_topic_suffix(&segments, &["MotionRegionDetector", "Motion"])
    {
        EventKind::Motion
    } else if has_topic_suffix(&segments, &["FieldDetector", "ObjectsInside"]) {
        EventKind::Intrusion
    } else if has_topic_suffix(&segments, &["LineDetector", "Crossed"]) {
        EventKind::LineCross
    } else if has_topic_suffix(&segments, &["TamperDetector", "Tamper"])
        || has_topic_suffix(
            &segments,
            &["VideoSource", "GlobalSceneChange", "ImagingService"],
        )
        || has_topic_suffix(
            &segments,
            &["VideoSource", "GlobalSceneChange", "AnalyticsService"],
        )
    {
        EventKind::Tamper
    } else if has_topic_suffix(&segments, &["AudioAnalytics", "Audio", "DetectedSound"]) {
        EventKind::Audio
    } else if has_topic_suffix(&segments, &["ObjectDetector", "Face"]) {
        EventKind::Face
    } else {
        EventKind::Other(topic.to_owned())
    }
}

fn has_topic_suffix(segments: &[&str], suffix: &[&str]) -> bool {
    segments.ends_with(suffix)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum StateEvidence {
    Known(EventState),
    Conflicting,
    Missing,
}

fn parse_boolean(value: &str) -> Option<EventState> {
    match value.to_ascii_lowercase().as_str() {
        "true" | "1" => Some(EventState::Start),
        "false" | "0" => Some(EventState::Stop),
        _ => None,
    }
}

fn evidence_for_keys(data: &HashMap<String, String>, keys: &[&str]) -> StateEvidence {
    let mut evidence = None;
    for key in keys {
        let Some(state) = data.get(*key).and_then(|value| parse_boolean(value)) else {
            continue;
        };
        if evidence.is_some_and(|known| known != state) {
            return StateEvidence::Conflicting;
        }
        evidence = Some(state);
    }
    evidence.map_or(StateEvidence::Missing, StateEvidence::Known)
}

/// Derives topic-aware state evidence from valid boolean data items.
fn state_for_data(topic: &str, kind: &EventKind, data: &HashMap<String, String>) -> StateEvidence {
    let segments: Vec<&str> = topic
        .split('/')
        .map(|segment| segment.split_once(':').map_or(segment, |(_, local)| local))
        .collect();
    let specific: &[&str] = match kind {
        EventKind::Motion => &["IsMotion"],
        EventKind::Intrusion => &["IsInside"],
        EventKind::Tamper => &["IsTamper"],
        EventKind::Audio => &["IsSoundDetected"],
        EventKind::Face => &["IsDetected"],
        EventKind::Other(_)
            if has_topic_suffix(&segments, &["Device", "Trigger", "DigitalInput"]) =>
        {
            &["LogicalState"]
        }
        _ => &[],
    };
    let preferred = evidence_for_keys(data, specific);
    if preferred != StateEvidence::Missing {
        return preferred;
    }
    evidence_for_keys(
        data,
        &[
            "State",
            "IsMotion",
            "IsInside",
            "IsTamper",
            "IsSoundDetected",
            "IsDetected",
        ],
    )
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
    let evidence = state_for_data(&message.topic, &kind, &message.data);
    let state = if message.property_operation.eq_ignore_ascii_case("Deleted") {
        EventState::Stop
    } else if message
        .property_operation
        .eq_ignore_ascii_case("Initialized")
    {
        match evidence {
            StateEvidence::Known(EventState::Start) => EventState::Start,
            StateEvidence::Known(EventState::Stop | EventState::Pulse)
            | StateEvidence::Conflicting
            | StateEvidence::Missing => return None,
        }
    } else {
        match evidence {
            StateEvidence::Known(state) => state,
            StateEvidence::Conflicting | StateEvidence::Missing => EventState::Pulse,
        }
    };
    let payload = serde_json::json!({
        "topic": message.topic,
        "operation": message.property_operation,
        "source": message.source,
        "source_elements": message.source_elements.iter().map(|item| serde_json::json!({
            "name": item.name,
            "content": item.content,
            "truncated": item.truncated,
        })).collect::<Vec<_>>(),
        "data": message.data,
        "data_elements": message.data_elements.iter().map(|item| serde_json::json!({
            "name": item.name,
            "content": item.content,
            "truncated": item.truncated,
        })).collect::<Vec<_>>(),
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

/// Parses an ISO 8601 date-time as cameras write it into wall time:
/// `YYYY-MM-DD` and `HH:MM:SS` joined by `T` or a space, an optional fraction
/// of any length (kept to microseconds), and a zone of `Z`, `z`, `±HH:MM` up to
/// 14:00, or none, which is read as UTC. Surrounding whitespace is ignored.
/// Years run from 1970 to 9999; a leap second, a longer text than 64 bytes, or
/// a non-ASCII character is `None`.
///
/// ```
/// use iris_core::time::WallUs;
/// use iris_events::onvif::parse_utc;
///
/// assert_eq!(parse_utc("1970-01-01T00:00:01Z"), Some(WallUs::new(1_000_000)));
/// assert_eq!(parse_utc("1970-01-01 01:00:00.5+01:00"), Some(WallUs::new(500_000)));
/// assert_eq!(parse_utc("1970-01-01T00:00:60Z"), None);
/// ```
#[must_use]
pub fn parse_utc(text: &str) -> Option<WallUs> {
    let text = text.trim();
    // Positions below are byte offsets; a timestamp is ASCII, and a non-ASCII
    // string would make them fall inside a character.
    if !(19..=64).contains(&text.len()) || !text.is_ascii() {
        return None;
    }
    let bytes = text.as_bytes();
    if bytes[4] != b'-'
        || bytes[7] != b'-'
        || !matches!(bytes[10], b'T' | b' ')
        || bytes[13] != b':'
        || bytes[16] != b':'
    {
        return None;
    }
    let year = parse_decimal(&bytes[0..4])?;
    let month = parse_decimal(&bytes[5..7])? as u32;
    let day = parse_decimal(&bytes[8..10])? as u32;
    let hour = parse_decimal(&bytes[11..13])?;
    let minute = parse_decimal(&bytes[14..16])?;
    let second = parse_decimal(&bytes[17..19])?;
    let mut tail = &bytes[19..];
    let mut frac_micros = 0i64;
    if tail.first() == Some(&b'.') {
        tail = &tail[1..];
        let digits = tail.iter().take_while(|byte| byte.is_ascii_digit()).count();
        if digits == 0 {
            return None;
        }
        for (index, digit) in tail[..digits].iter().take(6).enumerate() {
            frac_micros += i64::from(*digit - b'0') * 10i64.pow(5 - index as u32);
        }
        tail = &tail[digits..];
    }
    let offset_secs = match tail {
        b"" | b"Z" | b"z" => 0,
        [sign @ (b'+' | b'-'), h1, h2, b':', m1, m2]
            if h1.is_ascii_digit()
                && h2.is_ascii_digit()
                && m1.is_ascii_digit()
                && m2.is_ascii_digit() =>
        {
            let zone_hour = i64::from((*h1 - b'0') * 10 + (*h2 - b'0'));
            let zone_minute = i64::from((*m1 - b'0') * 10 + (*m2 - b'0'));
            if zone_hour > 14 || zone_minute > 59 || (zone_hour == 14 && zone_minute != 0) {
                return None;
            }
            let direction = if *sign == b'+' { 1 } else { -1 };
            direction * (zone_hour * 3600 + zone_minute * 60)
        }
        _ => return None,
    };
    // Bounded fields keep the arithmetic below in range for any input.
    if !(1970..=9999).contains(&year)
        || !(1..=12).contains(&month)
        || !(1..=time::days_in_month(year, month)).contains(&day)
        || !(0..=23).contains(&hour)
        || !(0..=59).contains(&minute)
        || !(0..=59).contains(&second)
    {
        return None;
    }
    let days = time::days_from_civil(year, month, day);
    let secs = days * 86_400 + hour * 3600 + minute * 60 + second - offset_secs;
    Some(WallUs::new(secs * 1_000_000 + frac_micros))
}

fn parse_decimal(bytes: &[u8]) -> Option<i64> {
    bytes.iter().try_fold(0i64, |value, byte| {
        byte.is_ascii_digit()
            .then(|| value * 10 + i64::from(*byte - b'0'))
    })
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
    use tokio::net::TcpListener;
    use tokio_rustls::TlsAcceptor;
    use tokio_rustls::rustls::ServerConfig;
    use tokio_rustls::rustls::pki_types::PrivatePkcs8KeyDer;

    use super::*;
    use crate::loopback;

    /// Seeded instants from 1970 through 2104, to the microsecond: the
    /// parser reads back what `WallUs` formats, with `Z` and behind an offset.
    #[test]
    fn parse_utc_reads_back_every_generated_instant_wall_us_formats() {
        let mut state = 0x9e37_79b9_7f4a_7c15_u64;
        for _ in 0..20_000 {
            state ^= state >> 12;
            state ^= state << 25;
            state ^= state >> 27;
            let micros = i64::try_from(
                state.wrapping_mul(0x2545_f491_4f6c_dd1d) % (135 * 365 * 86_400 * 1_000_000),
            )
            .unwrap();
            let wall = WallUs::new(micros);
            let text = wall.to_string();
            assert_eq!(parse_utc(&text), Some(wall), "{text}");
            let ahead = format!("{}+05:30", text.trim_end_matches('Z'));
            assert_eq!(
                parse_utc(&ahead),
                Some(wall.offset(-19_800_000_000)),
                "{ahead}"
            );
        }
    }

    struct Ignore;

    impl EventSink for Ignore {
        fn publish(&self, _: Event) {}
    }

    /// Finds the events service and creates a pull-point subscription signed
    /// by `client`, under the production call deadline.
    async fn subscribe(
        client: OnvifClient,
        transport: &SoapTransport,
        shutdown: &CancellationToken,
    ) -> Result<(OnvifClient, PullPointSubscription), SessionError> {
        subscribe_with_deadline(client, transport, shutdown, CALL_DEADLINE).await
    }

    #[derive(Clone, Copy, Debug)]
    enum CancellationPoint {
        Pull,
        Cooldown,
        Renew,
    }

    struct CleanupTransport {
        point: CancellationPoint,
        pulls: AtomicUsize,
        renews: AtomicUsize,
        unsubscribes: AtomicUsize,
    }

    struct TinyLeaseTransport {
        pulls: AtomicUsize,
        completed_pulls: AtomicUsize,
        renews: AtomicUsize,
        unsubscribes: AtomicUsize,
    }

    struct AdvertisedUrlTransport {
        events_url: String,
        reference_url: String,
        creates: AtomicUsize,
        create_urls: std::sync::Mutex<Vec<String>>,
    }

    impl AdvertisedUrlTransport {
        fn new(events_url: &str, reference_url: &str) -> Self {
            Self {
                events_url: events_url.to_owned(),
                reference_url: reference_url.to_owned(),
                creates: AtomicUsize::new(0),
                create_urls: std::sync::Mutex::new(Vec::new()),
            }
        }
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum SetupPoint {
        DateTime,
        Capabilities,
        Create,
    }

    struct SetupCancellationTransport {
        point: SetupPoint,
        entered: AtomicUsize,
        creates: AtomicUsize,
        unsubscribes: AtomicUsize,
    }

    #[derive(Clone, Copy, Debug)]
    enum FallbackMode {
        ClockFault,
        DetailInvalidTimestamp,
        DetailMessageExpired,
        UnrelatedTimestamp,
        Unauthorized,
        Parse,
        Transport,
        LostCreateResponse,
    }

    struct FallbackTransport {
        mode: FallbackMode,
        creates: AtomicUsize,
    }

    #[async_trait::async_trait]
    impl Transport for FallbackTransport {
        async fn soap_post(
            &self,
            _url: &str,
            action: &str,
            _body: String,
        ) -> Result<String, TransportError> {
            if action.contains("GetCapabilities") {
                return Ok(r#"<s:Envelope xmlns:s="http://www.w3.org/2003/05/soap-envelope" xmlns:tds="http://www.onvif.org/ver10/device/wsdl" xmlns:tt="http://www.onvif.org/ver10/schema"><s:Body><tds:GetCapabilitiesResponse><tds:Capabilities><tt:Events><tt:XAddr>http://127.0.0.1:8080/onvif/events</tt:XAddr></tt:Events></tds:Capabilities></tds:GetCapabilitiesResponse></s:Body></s:Envelope>"#.to_owned());
            }
            if action.contains("CreatePullPointSubscriptionRequest") {
                let attempt = self.creates.fetch_add(1, Ordering::SeqCst) + 1;
                if attempt > 1 {
                    return Ok(r#"<s:Envelope xmlns:s="http://www.w3.org/2003/05/soap-envelope" xmlns:tev="http://www.onvif.org/ver10/events/wsdl" xmlns:wsa="http://www.w3.org/2005/08/addressing"><s:Body><tev:CreatePullPointSubscriptionResponse><tev:SubscriptionReference><wsa:Address>http://127.0.0.1:8080/onvif/events/subscription</wsa:Address></tev:SubscriptionReference><tev:CurrentTime>2026-01-01T00:00:00Z</tev:CurrentTime><tev:TerminationTime>2026-01-01T00:10:00Z</tev:TerminationTime></tev:CreatePullPointSubscriptionResponse></s:Body></s:Envelope>"#.to_owned());
                }
                return match self.mode {
                    FallbackMode::ClockFault => Ok(r#"<s:Envelope xmlns:s="http://www.w3.org/2003/05/soap-envelope"><s:Body><s:Fault><s:Code><s:Value>s:Sender</s:Value><s:Subcode><s:Value>wsse:MessageExpired</s:Value></s:Subcode></s:Code><s:Reason><s:Text>Security timestamp expired</s:Text></s:Reason></s:Fault></s:Body></s:Envelope>"#.to_owned()),
                    FallbackMode::DetailInvalidTimestamp => Ok(r#"<s:Envelope xmlns:s="http://www.w3.org/2003/05/soap-envelope"><s:Body><s:Fault><s:Code><s:Value>s:Receiver</s:Value><s:Subcode><s:Value>ter:Action</s:Value></s:Subcode></s:Code><s:Reason><s:Text>Out of resources</s:Text></s:Reason><s:Detail><Text>InvalidTimestamp</Text></s:Detail></s:Fault></s:Body></s:Envelope>"#.to_owned()),
                    FallbackMode::DetailMessageExpired => Ok(r#"<s:Envelope xmlns:s="http://www.w3.org/2003/05/soap-envelope"><s:Body><s:Fault><s:Code><s:Value>s:Receiver</s:Value><s:Subcode><s:Value>ter:Action</s:Value></s:Subcode></s:Code><s:Reason><s:Text>Out of resources</s:Text></s:Reason><s:Detail><Text>MessageExpired</Text></s:Detail></s:Fault></s:Body></s:Envelope>"#.to_owned()),
                    FallbackMode::UnrelatedTimestamp => Ok(r#"<s:Envelope xmlns:s="http://www.w3.org/2003/05/soap-envelope"><s:Body><s:Fault><s:Code><s:Value>s:Receiver</s:Value><s:Subcode><s:Value>ter:Action</s:Value></s:Subcode></s:Code><s:Reason><s:Text>Recording timestamp unavailable</s:Text></s:Reason></s:Fault></s:Body></s:Envelope>"#.to_owned()),
                    FallbackMode::Unauthorized => Err(
                        TransportError::HttpStatus {
                            status: 401,
                            body: String::new(),
                        },
                    ),
                    FallbackMode::Parse => Ok("not XML".to_owned()),
                    FallbackMode::Transport => {
                        Err(TransportError::EndpointPolicy)
                    }
                    FallbackMode::LostCreateResponse => std::future::pending().await,
                };
            }
            panic!("unexpected action {action}");
        }
    }

    #[async_trait::async_trait]
    impl Transport for SetupCancellationTransport {
        async fn soap_post(
            &self,
            _url: &str,
            action: &str,
            _body: String,
        ) -> Result<String, TransportError> {
            let at_point = (matches!(self.point, SetupPoint::DateTime)
                && action.contains("GetSystemDateAndTime"))
                || (matches!(self.point, SetupPoint::Capabilities)
                    && action.contains("GetCapabilities"))
                || (matches!(self.point, SetupPoint::Create)
                    && action.contains("CreatePullPointSubscriptionRequest"));
            if action.contains("CreatePullPointSubscriptionRequest") {
                self.creates.fetch_add(1, Ordering::SeqCst);
            }
            if at_point {
                self.entered.fetch_add(1, Ordering::SeqCst);
                return std::future::pending().await;
            }
            if action.contains("GetSystemDateAndTime") {
                return Ok(r#"<s:Envelope xmlns:s="http://www.w3.org/2003/05/soap-envelope" xmlns:tds="http://www.onvif.org/ver10/device/wsdl" xmlns:tt="http://www.onvif.org/ver10/schema"><s:Body><tds:GetSystemDateAndTimeResponse><tds:SystemDateAndTime><tt:UTCDateTime><tt:Time><tt:Hour>0</tt:Hour><tt:Minute>0</tt:Minute><tt:Second>0</tt:Second></tt:Time><tt:Date><tt:Year>2026</tt:Year><tt:Month>1</tt:Month><tt:Day>1</tt:Day></tt:Date></tt:UTCDateTime></tds:SystemDateAndTime></tds:GetSystemDateAndTimeResponse></s:Body></s:Envelope>"#.to_owned());
            }
            if action.contains("GetCapabilities") {
                return Ok(r#"<s:Envelope xmlns:s="http://www.w3.org/2003/05/soap-envelope" xmlns:tds="http://www.onvif.org/ver10/device/wsdl" xmlns:tt="http://www.onvif.org/ver10/schema"><s:Body><tds:GetCapabilitiesResponse><tds:Capabilities><tt:Events><tt:XAddr>http://127.0.0.1:8080/onvif/events</tt:XAddr></tt:Events></tds:Capabilities></tds:GetCapabilitiesResponse></s:Body></s:Envelope>"#.to_owned());
            }
            if action.contains("CreatePullPointSubscriptionRequest") {
                return Ok(r#"<s:Envelope xmlns:s="http://www.w3.org/2003/05/soap-envelope" xmlns:tev="http://www.onvif.org/ver10/events/wsdl" xmlns:wsa="http://www.w3.org/2005/08/addressing"><s:Body><tev:CreatePullPointSubscriptionResponse><tev:SubscriptionReference><wsa:Address>http://127.0.0.1:8080/onvif/events/subscription</wsa:Address></tev:SubscriptionReference><tev:CurrentTime>2026-01-01T00:00:00Z</tev:CurrentTime><tev:TerminationTime>2026-01-01T00:10:00Z</tev:TerminationTime></tev:CreatePullPointSubscriptionResponse></s:Body></s:Envelope>"#.to_owned());
            }
            if action.contains("UnsubscribeRequest") {
                self.unsubscribes.fetch_add(1, Ordering::SeqCst);
                return Ok(r#"<s:Envelope xmlns:s="http://www.w3.org/2003/05/soap-envelope"><s:Body><UnsubscribeResponse/></s:Body></s:Envelope>"#.to_owned());
            }
            panic!("unexpected action {action}");
        }
    }

    #[async_trait::async_trait]
    impl Transport for AdvertisedUrlTransport {
        async fn soap_post(
            &self,
            url: &str,
            action: &str,
            _body: String,
        ) -> Result<String, TransportError> {
            if action.contains("GetCapabilities") {
                return Ok(format!(
                    r#"<s:Envelope xmlns:s="http://www.w3.org/2003/05/soap-envelope" xmlns:tds="http://www.onvif.org/ver10/device/wsdl" xmlns:tt="http://www.onvif.org/ver10/schema"><s:Body><tds:GetCapabilitiesResponse><tds:Capabilities><tt:Events><tt:XAddr>{}</tt:XAddr></tt:Events></tds:Capabilities></tds:GetCapabilitiesResponse></s:Body></s:Envelope>"#,
                    self.events_url
                ));
            }
            if action.contains("CreatePullPointSubscriptionRequest") {
                self.creates.fetch_add(1, Ordering::SeqCst);
                self.create_urls
                    .lock()
                    .expect("create URL log")
                    .push(url.to_owned());
                return Ok(format!(
                    r#"<s:Envelope xmlns:s="http://www.w3.org/2003/05/soap-envelope" xmlns:tev="http://www.onvif.org/ver10/events/wsdl" xmlns:wsa="http://www.w3.org/2005/08/addressing"><s:Body><tev:CreatePullPointSubscriptionResponse><tev:SubscriptionReference><wsa:Address>{}</wsa:Address></tev:SubscriptionReference><tev:CurrentTime>2026-01-01T00:00:00Z</tev:CurrentTime><tev:TerminationTime>2026-01-01T00:10:00Z</tev:TerminationTime></tev:CreatePullPointSubscriptionResponse></s:Body></s:Envelope>"#,
                    self.reference_url
                ));
            }
            panic!("unexpected action {action}");
        }
    }

    #[async_trait::async_trait]
    impl Transport for CleanupTransport {
        async fn soap_post(
            &self,
            _url: &str,
            action: &str,
            _body: String,
        ) -> Result<String, TransportError> {
            const EMPTY_PULL: &str = r#"<s:Envelope xmlns:s="http://www.w3.org/2003/05/soap-envelope"><s:Body><PullMessagesResponse/></s:Body></s:Envelope>"#;
            const RENEWED: &str = r#"<s:Envelope xmlns:s="http://www.w3.org/2003/05/soap-envelope"><s:Body><RenewResponse><CurrentTime>2026-01-01T00:00:00Z</CurrentTime><TerminationTime>2026-01-01T00:10:00Z</TerminationTime></RenewResponse></s:Body></s:Envelope>"#;
            const UNSUBSCRIBED: &str = r#"<s:Envelope xmlns:s="http://www.w3.org/2003/05/soap-envelope"><s:Body><UnsubscribeResponse/></s:Body></s:Envelope>"#;
            if action.contains("PullMessagesRequest") {
                self.pulls.fetch_add(1, Ordering::SeqCst);
                if matches!(self.point, CancellationPoint::Pull) {
                    return std::future::pending().await;
                }
                return Ok(EMPTY_PULL.to_owned());
            }
            if action.contains("RenewRequest") {
                self.renews.fetch_add(1, Ordering::SeqCst);
                if matches!(self.point, CancellationPoint::Renew) {
                    return std::future::pending().await;
                }
                return Ok(RENEWED.to_owned());
            }
            if action.contains("UnsubscribeRequest") {
                self.unsubscribes.fetch_add(1, Ordering::SeqCst);
                return Ok(UNSUBSCRIBED.to_owned());
            }
            panic!("unexpected action {action}");
        }
    }

    #[async_trait::async_trait]
    impl Transport for TinyLeaseTransport {
        async fn soap_post(
            &self,
            _url: &str,
            action: &str,
            _body: String,
        ) -> Result<String, TransportError> {
            if action.contains("PullMessagesRequest") {
                self.pulls.fetch_add(1, Ordering::SeqCst);
                tokio::time::sleep(Duration::from_secs(2)).await;
                self.completed_pulls.fetch_add(1, Ordering::SeqCst);
                return Ok(r#"<s:Envelope xmlns:s="http://www.w3.org/2003/05/soap-envelope"><s:Body><PullMessagesResponse/></s:Body></s:Envelope>"#.to_owned());
            }
            if action.contains("RenewRequest") {
                let attempt = self.renews.fetch_add(1, Ordering::SeqCst) + 1;
                let termination = if attempt.is_multiple_of(2) {
                    "2026-01-01T00:00:00.000001Z"
                } else {
                    "2026-01-01T00:00:00.000000Z"
                };
                return Ok(format!(
                    r#"<s:Envelope xmlns:s="http://www.w3.org/2003/05/soap-envelope"><s:Body><RenewResponse><CurrentTime>2026-01-01T00:00:00.000000Z</CurrentTime><TerminationTime>{termination}</TerminationTime></RenewResponse></s:Body></s:Envelope>"#
                ));
            }
            if action.contains("UnsubscribeRequest") {
                self.unsubscribes.fetch_add(1, Ordering::SeqCst);
                return Ok(r#"<s:Envelope xmlns:s="http://www.w3.org/2003/05/soap-envelope"><s:Body><UnsubscribeResponse/></s:Body></s:Envelope>"#.to_owned());
            }
            panic!("unexpected action {action}");
        }
    }

    async fn wait_for(predicate: impl Fn() -> bool) {
        timeout(Duration::from_secs(1), async {
            while !predicate() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("test state reached within one second");
    }

    async fn yield_until_paused(predicate: impl Fn() -> bool) {
        for _ in 0..1_000 {
            if predicate() {
                return;
            }
            tokio::task::yield_now().await;
        }
        panic!("paused-time test state was not reached");
    }

    #[tokio::test]
    async fn cancellation_during_each_setup_call_is_prompt_and_does_not_unsubscribe() {
        for point in [
            SetupPoint::DateTime,
            SetupPoint::Capabilities,
            SetupPoint::Create,
        ] {
            let transport = Arc::new(SetupCancellationTransport {
                point,
                entered: AtomicUsize::new(0),
                creates: AtomicUsize::new(0),
                unsubscribes: AtomicUsize::new(0),
            });
            let endpoint = CameraEndpoint {
                camera: "cam1".parse().unwrap(),
                host: "127.0.0.1".to_owned(),
                http_port: 8080,
                https: false,
                tls_verify: true,
                username: String::new(),
                password: String::new(),
            };
            let policy = SoapTransport::new(&endpoint).unwrap();
            let client = OnvifClient::new(endpoint.base_url()).with_transport(transport.clone());
            let shutdown = CancellationToken::new();
            let cancel_transport = transport.clone();
            let cancel_shutdown = shutdown.clone();
            let canceller = tokio::spawn(async move {
                wait_for(|| cancel_transport.entered.load(Ordering::SeqCst) == 1).await;
                cancel_shutdown.cancel();
            });

            let result = match point {
                SetupPoint::DateTime => bounded(&shutdown, client.get_system_date_and_time())
                    .await
                    .map(|_| ()),
                SetupPoint::Capabilities | SetupPoint::Create => {
                    subscribe(client, &policy, &shutdown).await.map(|_| ())
                }
            };
            canceller.await.unwrap();
            assert!(matches!(result, Err(SessionError::Cancelled)), "{point:?}");
            assert_eq!(transport.unsubscribes.load(Ordering::SeqCst), 0);
            assert_eq!(
                transport.creates.load(Ordering::SeqCst),
                usize::from(matches!(point, SetupPoint::Create))
            );
        }
    }

    #[tokio::test]
    async fn cancellation_after_subscription_always_unsubscribes_and_clears_connected() {
        for point in [
            CancellationPoint::Pull,
            CancellationPoint::Cooldown,
            CancellationPoint::Renew,
        ] {
            let transport = Arc::new(CleanupTransport {
                point,
                pulls: AtomicUsize::new(0),
                renews: AtomicUsize::new(0),
                unsubscribes: AtomicUsize::new(0),
            });
            let client =
                OnvifClient::new("http://127.0.0.1/onvif/events").with_transport(transport.clone());
            let endpoint = CameraEndpoint {
                camera: "cam1".parse().unwrap(),
                host: "127.0.0.1".to_owned(),
                http_port: 80,
                https: false,
                tls_verify: true,
                username: String::new(),
                password: String::new(),
            };
            let stats = Arc::new(EventStats::new());
            let shutdown = CancellationToken::new();
            let task_stats = stats.clone();
            let task_shutdown = shutdown.clone();
            let next_renew = if matches!(point, CancellationPoint::Renew) {
                Instant::now()
            } else {
                Instant::now() + RENEW_EVERY
            };
            let task = tokio::spawn(async move {
                active_subscription(
                    &client,
                    "http://127.0.0.1/onvif/events/subscription",
                    &endpoint,
                    &Ignore,
                    task_stats.as_ref(),
                    &task_shutdown,
                    next_renew,
                )
                .await
            });

            match point {
                CancellationPoint::Pull => {
                    wait_for(|| transport.pulls.load(Ordering::SeqCst) == 1).await;
                }
                CancellationPoint::Cooldown => {
                    wait_for(|| stats.messages() == 1).await;
                }
                CancellationPoint::Renew => {
                    wait_for(|| transport.renews.load(Ordering::SeqCst) == 1).await;
                }
            }
            assert!(stats.connected(), "{point:?}");
            shutdown.cancel();
            let result = timeout(Duration::from_secs(1), task)
                .await
                .expect("subscription cleanup completed")
                .unwrap();
            assert!(
                matches!(result, Ok(()) | Err(SessionError::Cancelled)),
                "{point:?}: {result:?}"
            );
            assert!(!stats.connected(), "{point:?}");
            assert_eq!(
                transport.unsubscribes.load(Ordering::SeqCst),
                1,
                "{point:?}"
            );
        }
    }

    #[tokio::test]
    async fn cancellation_with_an_existing_reference_runs_cleanup_before_the_first_pull() {
        let transport = Arc::new(CleanupTransport {
            point: CancellationPoint::Pull,
            pulls: AtomicUsize::new(0),
            renews: AtomicUsize::new(0),
            unsubscribes: AtomicUsize::new(0),
        });
        let client =
            OnvifClient::new("http://127.0.0.1/onvif/events").with_transport(transport.clone());
        let endpoint = CameraEndpoint {
            camera: "cam1".parse().unwrap(),
            host: "127.0.0.1".to_owned(),
            http_port: 80,
            https: false,
            tls_verify: true,
            username: String::new(),
            password: String::new(),
        };
        let stats = EventStats::new();
        let shutdown = CancellationToken::new();
        shutdown.cancel();

        active_subscription(
            &client,
            "http://127.0.0.1/onvif/events/subscription",
            &endpoint,
            &Ignore,
            &stats,
            &shutdown,
            Instant::now() + RENEW_EVERY,
        )
        .await
        .unwrap();

        assert!(!stats.connected());
        assert!(transport.pulls.load(Ordering::SeqCst) <= 1);
        assert_eq!(transport.unsubscribes.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn renewal_deadline_remains_live_while_a_pull_is_pending() {
        let transport = Arc::new(CleanupTransport {
            point: CancellationPoint::Pull,
            pulls: AtomicUsize::new(0),
            renews: AtomicUsize::new(0),
            unsubscribes: AtomicUsize::new(0),
        });
        let client =
            OnvifClient::new("http://127.0.0.1/onvif/events").with_transport(transport.clone());
        let endpoint = CameraEndpoint {
            camera: "cam1".parse().unwrap(),
            host: "127.0.0.1".to_owned(),
            http_port: 80,
            https: false,
            tls_verify: true,
            username: String::new(),
            password: String::new(),
        };
        let stats = Arc::new(EventStats::new());
        let shutdown = CancellationToken::new();
        let task_stats = stats.clone();
        let task_shutdown = shutdown.clone();
        let task = tokio::spawn(async move {
            active_subscription(
                &client,
                "http://127.0.0.1/onvif/events/subscription",
                &endpoint,
                &Ignore,
                task_stats.as_ref(),
                &task_shutdown,
                Instant::now() + Duration::from_millis(10),
            )
            .await
        });

        wait_for(|| transport.pulls.load(Ordering::SeqCst) == 1).await;
        wait_for(|| transport.renews.load(Ordering::SeqCst) == 1).await;
        assert_eq!(stats.renewals(), 1);
        shutdown.cancel();
        timeout(Duration::from_secs(1), task)
            .await
            .expect("subscription cleanup completed")
            .unwrap()
            .unwrap();
        assert!(!stats.connected());
        assert_eq!(transport.unsubscribes.load(Ordering::SeqCst), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn tiny_renewals_are_floor_paced_without_suppressing_pull_or_cancellation() {
        let transport = Arc::new(TinyLeaseTransport {
            pulls: AtomicUsize::new(0),
            completed_pulls: AtomicUsize::new(0),
            renews: AtomicUsize::new(0),
            unsubscribes: AtomicUsize::new(0),
        });
        let client =
            OnvifClient::new("http://127.0.0.1/onvif/events").with_transport(transport.clone());
        let endpoint = CameraEndpoint {
            camera: "cam1".parse().unwrap(),
            host: "127.0.0.1".to_owned(),
            http_port: 80,
            https: false,
            tls_verify: true,
            username: String::new(),
            password: String::new(),
        };
        let stats = Arc::new(EventStats::new());
        let shutdown = CancellationToken::new();
        let task_stats = stats.clone();
        let task_shutdown = shutdown.clone();
        let task = tokio::spawn(async move {
            active_subscription(
                &client,
                "http://127.0.0.1/onvif/events/subscription",
                &endpoint,
                &Ignore,
                task_stats.as_ref(),
                &task_shutdown,
                Instant::now() + MIN_RENEW_DELAY,
            )
            .await
        });

        yield_until_paused(|| transport.pulls.load(Ordering::SeqCst) == 1).await;
        let final_millisecond = Duration::from_millis(1);
        let almost_minimum = MIN_RENEW_DELAY
            .checked_sub(final_millisecond)
            .expect("renewal floor exceeds one millisecond");
        tokio::time::advance(almost_minimum).await;
        assert_eq!(transport.renews.load(Ordering::SeqCst), 0);

        tokio::time::advance(final_millisecond).await;
        yield_until_paused(|| stats.renewals() == 1).await;
        assert_eq!(transport.completed_pulls.load(Ordering::SeqCst), 0);

        tokio::time::advance(almost_minimum).await;
        assert_eq!(transport.renews.load(Ordering::SeqCst), 1);
        tokio::time::advance(final_millisecond).await;
        yield_until_paused(|| stats.renewals() == 2).await;
        assert_eq!(transport.completed_pulls.load(Ordering::SeqCst), 0);

        tokio::time::advance(Duration::from_millis(500)).await;
        yield_until_paused(|| stats.messages() == 1).await;
        assert_eq!(transport.pulls.load(Ordering::SeqCst), 1);
        assert_eq!(transport.completed_pulls.load(Ordering::SeqCst), 1);
        assert_eq!(transport.renews.load(Ordering::SeqCst), 2);

        assert!(stats.connected());
        shutdown.cancel();
        timeout(Duration::from_secs(1), task)
            .await
            .expect("subscription cancellation remained prompt")
            .unwrap()
            .unwrap();
        assert!(!stats.connected());
        assert_eq!(transport.unsubscribes.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn discovered_urls_are_rebased_or_rejected_before_authenticated_followup_calls() {
        let endpoint = CameraEndpoint {
            camera: "cam1".parse().unwrap(),
            host: "127.0.0.1".to_owned(),
            http_port: 8080,
            https: false,
            tls_verify: true,
            username: "marker-user".to_owned(),
            password: "marker-password".to_owned(),
        };
        let policy = SoapTransport::new(&endpoint).unwrap();
        let shutdown = CancellationToken::new();
        let same_events = "http://127.0.0.1:8080/onvif/events";
        let same_reference = "http://127.0.0.1:8080/onvif/events/subscription";

        for rejected in [
            "http://user@127.0.0.1:8080/onvif/events",
            "http://127.0.0.1:8080/onvif/events#fragment",
            "ftp://127.0.0.1:8080/onvif/events",
        ] {
            let foreign_events = Arc::new(AdvertisedUrlTransport::new(rejected, same_reference));
            let client =
                OnvifClient::new(endpoint.base_url()).with_transport(foreign_events.clone());
            assert!(
                subscribe(client, &policy, &shutdown).await.is_err(),
                "{rejected}"
            );
            assert_eq!(
                foreign_events.creates.load(Ordering::SeqCst),
                0,
                "created a subscription at rejected events URL {rejected}"
            );

            let reference = rejected.replace("/onvif/events", "/onvif/events/subscription");
            let foreign_reference = Arc::new(AdvertisedUrlTransport::new(same_events, &reference));
            let client =
                OnvifClient::new(endpoint.base_url()).with_transport(foreign_reference.clone());
            assert!(
                subscribe(client, &policy, &shutdown).await.is_err(),
                "{rejected}"
            );
            assert_eq!(
                foreign_reference.creates.load(Ordering::SeqCst),
                1,
                "{rejected}"
            );
        }

        for foreign in [
            "http://127.0.0.1:8080/onvif/events",
            "http://192.0.2.10:8080/onvif/events",
            "http://127.0.0.1:8081/onvif/events",
            "https://127.0.0.1:8080/onvif/events",
            "http://camera.example/onvif/events",
        ] {
            let reference = foreign.replace("/onvif/events", "/onvif/events/subscription");
            let advertised = Arc::new(AdvertisedUrlTransport::new(foreign, &reference));
            let client = OnvifClient::new(endpoint.base_url()).with_transport(advertised.clone());
            let (_, subscription) = subscribe(client, &policy, &shutdown).await.unwrap();
            assert_eq!(
                advertised.create_urls.lock().unwrap().as_slice(),
                [same_events],
                "{foreign}"
            );
            assert_eq!(subscription.reference_url, same_reference, "{foreign}");
        }
    }

    /// Runs one session against a loopback camera configured as `host` that
    /// advertises `advertised` for its event services; returns every request
    /// it served.
    async fn rebased_session(
        host: &str,
        advertised: impl FnOnce(u16) -> String,
    ) -> Vec<(String, String)> {
        let camera = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = camera.local_addr().unwrap().port();
        let (requests_tx, mut requests) = tokio::sync::mpsc::unbounded_channel();
        let camera_task = tokio::spawn(loopback::camera(camera, advertised(port), requests_tx));
        let endpoint = loopback::endpoint(host, port);
        let shutdown = CancellationToken::new();
        let session_shutdown = shutdown.clone();
        let session = tokio::spawn(async move {
            let mut backoff = Backoff::new("test/onvif");
            session(
                &endpoint,
                &Ignore,
                &EventStats::new(),
                &session_shutdown,
                &mut backoff,
            )
            .await
        });
        let mut served = Vec::new();
        timeout(Duration::from_secs(10), async {
            while let Some(request) = requests.recv().await {
                let pulled = request.1.contains("pullmessagesrequest");
                let unsubscribed = request.1.contains("unsubscriberequest");
                served.push(request);
                if pulled {
                    shutdown.cancel();
                }
                if unsubscribed {
                    break;
                }
            }
        })
        .await
        .expect("the session subscribes, pulls, and unsubscribes");
        timeout(Duration::from_secs(10), session)
            .await
            .expect("the session ends after cancellation")
            .unwrap()
            .expect("a cancelled session ends cleanly");
        camera_task.abort();
        served
    }

    fn served_paths(served: &[(String, String)]) -> Vec<&str> {
        served.iter().map(|(path, _)| path.as_str()).collect()
    }

    #[tokio::test]
    async fn a_camera_advertising_a_foreign_address_is_served_at_the_configured_one() {
        let sink = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let sink_address = sink.local_addr().unwrap();
        let served = rebased_session("127.0.0.1", |_| format!("http://{sink_address}")).await;
        assert_eq!(
            served_paths(&served),
            [
                "/onvif/device_service",
                "/onvif/device_service",
                "/onvif/events",
                "/onvif/events/subscription",
                "/onvif/events/subscription",
            ]
        );
        assert!(
            timeout(Duration::from_millis(200), sink.accept())
                .await
                .is_err(),
            "the advertised address received no request"
        );
    }

    #[tokio::test]
    async fn a_camera_configured_by_host_name_is_served_at_that_host() {
        let sink = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let sink_address = sink.local_addr().unwrap();
        let served = rebased_session("localhost", |_| format!("http://{sink_address}")).await;
        assert!(
            served.iter().any(|(path, action)| path == "/onvif/events"
                && action.contains("createpullpointsubscriptionrequest")),
            "{served:?}"
        );
        assert!(
            served
                .iter()
                .any(|(_, action)| action.contains("unsubscriberequest")),
            "the subscription is released: {served:?}"
        );
        assert!(
            timeout(Duration::from_millis(200), sink.accept())
                .await
                .is_err(),
            "the advertised address received a request"
        );
    }

    #[tokio::test]
    async fn only_a_structured_clock_fault_creates_a_host_time_subscription() {
        let endpoint = CameraEndpoint {
            camera: "cam1".parse().unwrap(),
            host: "127.0.0.1".to_owned(),
            http_port: 8080,
            https: false,
            tls_verify: true,
            username: "marker-user".to_owned(),
            password: "marker-password".to_owned(),
        };
        let policy = SoapTransport::new(&endpoint).unwrap();

        for mode in [
            FallbackMode::ClockFault,
            FallbackMode::DetailInvalidTimestamp,
            FallbackMode::DetailMessageExpired,
            FallbackMode::UnrelatedTimestamp,
            FallbackMode::Unauthorized,
            FallbackMode::Parse,
            FallbackMode::Transport,
            FallbackMode::LostCreateResponse,
        ] {
            let transport = Arc::new(FallbackTransport {
                mode,
                creates: AtomicUsize::new(0),
            });
            let client = OnvifClient::new(endpoint.base_url()).with_transport(transport.clone());
            let result = subscribe_with_clock_fallback(
                client,
                &policy,
                &CancellationToken::new(),
                3_600,
                Duration::from_millis(20),
            )
            .await;
            if matches!(mode, FallbackMode::ClockFault) {
                let (_, _, first_error) = result.unwrap();
                assert!(first_error.is_some());
                assert_eq!(transport.creates.load(Ordering::SeqCst), 2);
            } else {
                assert!(result.is_err(), "{mode:?}");
                assert_eq!(
                    transport.creates.load(Ordering::SeqCst),
                    1,
                    "{mode:?} must not create a second subscription"
                );
            }
        }

        let transport = Arc::new(FallbackTransport {
            mode: FallbackMode::ClockFault,
            creates: AtomicUsize::new(0),
        });
        let client = OnvifClient::new(endpoint.base_url()).with_transport(transport.clone());
        assert!(
            subscribe_with_clock_fallback(
                client,
                &policy,
                &CancellationToken::new(),
                0,
                Duration::from_millis(20),
            )
            .await
            .is_err()
        );
        assert_eq!(transport.creates.load(Ordering::SeqCst), 1);
    }

    async fn self_signed_endpoint(
        tls_verify: bool,
    ) -> (CameraEndpoint, tokio::task::JoinHandle<bool>) {
        let certified = rcgen::generate_simple_self_signed(vec!["127.0.0.1".to_owned()]).unwrap();
        let key = PrivatePkcs8KeyDer::from(certified.signing_key.serialize_der());
        let config = ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(vec![certified.cert.der().clone()], key.into())
            .unwrap();
        let acceptor = TlsAcceptor::from(Arc::new(config));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            let Ok(mut socket) = acceptor.accept(socket).await else {
                return false;
            };
            let mut request = [0; 8_192];
            let Ok(length) = socket.read(&mut request).await else {
                return false;
            };
            assert!(
                std::str::from_utf8(&request[..length])
                    .unwrap()
                    .contains("POST /onvif/device_service")
            );
            // A deliberate HTTP error ends the session after proving TLS and
            // HTTP reached this endpoint; no subscription fixture is needed.
            socket
                .write_all(
                    b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                )
                .await
                .unwrap();
            true
        });
        let endpoint = CameraEndpoint {
            camera: "cam1".parse().unwrap(),
            host: "127.0.0.1".to_owned(),
            http_port: address.port(),
            https: true,
            tls_verify,
            username: String::new(),
            password: String::new(),
        };
        (endpoint, server)
    }

    #[tokio::test]
    async fn an_unverified_onvif_endpoint_accepts_a_self_signed_certificate() {
        let (endpoint, server) = self_signed_endpoint(false).await;
        let mut backoff = Backoff::new("test/onvif");
        let result = timeout(
            Duration::from_secs(5),
            session(
                &endpoint,
                &Ignore,
                &EventStats::new(),
                &CancellationToken::new(),
                &mut backoff,
            ),
        )
        .await;
        assert!(
            result.is_ok(),
            "local TLS exchange completes within the deadline"
        );
        let reached_http = timeout(Duration::from_secs(5), server)
            .await
            .unwrap()
            .unwrap();
        assert!(
            reached_http,
            "tls_verify=false must permit the self-signed ONVIF endpoint"
        );
    }

    #[tokio::test]
    async fn a_verified_onvif_endpoint_rejects_a_self_signed_certificate() {
        let (endpoint, server) = self_signed_endpoint(true).await;
        let mut backoff = Backoff::new("test/onvif");
        let result = timeout(
            Duration::from_secs(5),
            session(
                &endpoint,
                &Ignore,
                &EventStats::new(),
                &CancellationToken::new(),
                &mut backoff,
            ),
        )
        .await;
        assert!(result.unwrap().is_err());
        let reached_http = timeout(Duration::from_secs(5), server)
            .await
            .unwrap()
            .unwrap();
        assert!(
            !reached_http,
            "tls_verify=true must keep certificate verification enabled"
        );
    }

    fn message(topic: &str, data: &[(&str, &str)], operation: &str) -> NotificationMessage {
        NotificationMessage {
            topic: topic.to_owned(),
            utc_time: "2026-10-31T16:35:59Z".to_owned(),
            property_operation: operation.to_owned(),
            source: HashMap::from([("VideoSourceConfigurationToken".to_owned(), "0".to_owned())]),
            source_elements: Vec::new(),
            data: data
                .iter()
                .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                .collect(),
            data_elements: Vec::new(),
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
            EventKind::Other("tns1:Monitoring/IPConflict".into())
        );
    }

    #[test]
    fn topic_matching_is_segment_aware_and_keeps_unknown_identity() {
        for topic in [
            "tns1:RuleEngine/tt:CellMotionDetector/tt:Motion",
            "RuleEngine/CellMotionDetector/Motion",
        ] {
            assert_eq!(kind_for_topic(topic), EventKind::Motion);
        }
        for topic in [
            "tns1:VideoSource/tt:GlobalSceneChange/tt:ImagingService",
            "tns1:VideoSource/tt:GlobalSceneChange/tt:AnalyticsService",
        ] {
            assert_eq!(kind_for_topic(topic), EventKind::Tamper);
        }
        for topic in [
            "tns1:NotVideoSource/MotionAlarm",
            "tns1:RuleEngine/MyTamperDetector/Tamper",
        ] {
            assert_eq!(kind_for_topic(topic), EventKind::Other(topic.to_owned()));
        }
    }

    #[test]
    fn every_mapped_topic_accepts_prefix_variants_without_near_collisions() {
        let cases: &[(&[&str], EventKind)] = &[
            (&["VideoSource", "MotionAlarm"], EventKind::Motion),
            (&["CellMotionDetector", "Motion"], EventKind::Motion),
            (&["MotionRegionDetector", "Motion"], EventKind::Motion),
            (&["FieldDetector", "ObjectsInside"], EventKind::Intrusion),
            (&["LineDetector", "Crossed"], EventKind::LineCross),
            (&["TamperDetector", "Tamper"], EventKind::Tamper),
            (
                &["VideoSource", "GlobalSceneChange", "ImagingService"],
                EventKind::Tamper,
            ),
            (
                &["VideoSource", "GlobalSceneChange", "AnalyticsService"],
                EventKind::Tamper,
            ),
            (
                &["AudioAnalytics", "Audio", "DetectedSound"],
                EventKind::Audio,
            ),
            (&["ObjectDetector", "Face"], EventKind::Face),
        ];

        for (segments, expected) in cases {
            let plain = segments.join("/");
            let root_prefixed = format!("tns1:{plain}");
            let per_segment = segments
                .iter()
                .map(|segment| format!("tt:{segment}"))
                .collect::<Vec<_>>()
                .join("/");
            for topic in [&plain, &root_prefixed, &per_segment] {
                assert_eq!(&kind_for_topic(topic), expected, "topic={topic}");
            }

            let mut collision = segments.iter().map(ToString::to_string).collect::<Vec<_>>();
            collision[0] = format!("Not{}", collision[0]);
            let collision = collision.join("/");
            assert_eq!(
                kind_for_topic(&collision),
                EventKind::Other(collision.clone()),
                "near collision {collision}"
            );
        }

        let first = "vendor:Rule/A";
        let second = "vendor:Rule/B";
        assert_eq!(kind_for_topic(first), EventKind::Other(first.to_owned()));
        assert_eq!(kind_for_topic(second), EventKind::Other(second.to_owned()));
        assert_ne!(kind_for_topic(first), kind_for_topic(second));
    }

    #[test]
    fn a_notification_keeps_the_camera_time_beside_the_receipt_time() {
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
        assert_eq!(start.nvr_time, WallUs::new(1));
        assert_eq!(start.camera_time, Some(WallUs::new(1_793_464_559_000_000)));
        assert_eq!(start.source, EventSource::OnvifPullPoint);
    }

    #[test]
    fn each_fault_marker_is_recognised_in_any_case_and_spacing() {
        for auth in [
            "ter:NotAuthorized",
            "Sender not authorized",
            "wsse:FailedAuthentication",
            "The security token failed authentication",
            "wsse:InvalidSecurity",
            "An invalid security token was provided",
        ] {
            assert!(is_auth_fault_marker(auth), "{auth}");
            assert!(is_auth_fault_marker(&auth.to_ascii_uppercase()), "{auth}");
            assert!(!is_clock_fault_marker(auth), "{auth}");
        }
        for clock in [
            "wsu:MessageExpired",
            "wsse:InvalidTimestamp",
            "Clock skew too great",
            "Wsse authorized time check failed",
        ] {
            assert!(is_clock_fault_marker(clock), "{clock}");
            assert!(
                is_clock_fault_marker(&clock.to_ascii_uppercase()),
                "{clock}"
            );
            assert!(!is_auth_fault_marker(clock), "{clock}");
        }
        for other in ["ter:ActionNotSupported", "Receiver", ""] {
            assert!(!is_auth_fault_marker(other) && !is_clock_fault_marker(other));
        }
    }

    /// The data item that belongs to a topic's kind decides its state even
    /// when a generic item beside it says otherwise; without a kind of its
    /// own the two disagree and the state is not known.
    #[test]
    fn the_item_of_the_topics_kind_outranks_a_generic_state_item() {
        for (kind, key) in [
            (EventKind::Motion, "IsMotion"),
            (EventKind::Intrusion, "IsInside"),
            (EventKind::Tamper, "IsTamper"),
            (EventKind::Audio, "IsSoundDetected"),
            (EventKind::Face, "IsDetected"),
        ] {
            let data = HashMap::from([
                (key.to_owned(), "true".to_owned()),
                ("State".to_owned(), "false".to_owned()),
            ]);
            assert_eq!(
                state_for_data("tns1:RuleEngine/Any", &kind, &data),
                StateEvidence::Known(EventState::Start),
                "{key}"
            );
            assert_eq!(
                state_for_data("tns1:RuleEngine/Any", &EventKind::Other("x".into()), &data),
                StateEvidence::Conflicting,
                "{key}"
            );
        }
    }

    #[test]
    fn derives_state_from_the_topic_and_its_boolean_data() {
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
    }

    #[test]
    fn a_notification_without_boolean_state_is_a_pulse_carrying_its_data() {
        let camera = CameraId::try_new("cam1").unwrap();
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
    fn property_operations_and_topic_specific_state_do_not_create_false_transitions() {
        let camera = CameraId::try_new("cam1").unwrap();
        let motion = "tns1:VideoSource/MotionAlarm";

        for (data, expected) in [
            (vec![("State", "true")], Some(EventState::Start)),
            (vec![("State", "false")], None),
            (vec![("State", "unknown")], None),
            (Vec::new(), None),
        ] {
            let event = normalize_notification(
                &camera,
                &message(motion, &data, "Initialized"),
                WallUs::new(1),
            );
            assert_eq!(event.map(|event| event.state), expected, "data={data:?}");
        }

        for data in [
            vec![("State", "true")],
            vec![("State", "false")],
            Vec::new(),
        ] {
            let event =
                normalize_notification(&camera, &message(motion, &data, "Deleted"), WallUs::new(1))
                    .unwrap();
            assert_eq!(event.state, EventState::Stop, "data={data:?}");
        }

        let preferred = normalize_notification(
            &camera,
            &message(
                motion,
                &[("State", "true"), ("IsMotion", "false")],
                "Changed",
            ),
            WallUs::new(1),
        )
        .unwrap();
        assert_eq!(preferred.state, EventState::Stop);

        let malformed_fallback = normalize_notification(
            &camera,
            &message(
                motion,
                &[("State", "invalid"), ("IsMotion", "true")],
                "Changed",
            ),
            WallUs::new(1),
        )
        .unwrap();
        assert_eq!(malformed_fallback.state, EventState::Start);

        for value in ["true", "1", "false", "0"] {
            let event = normalize_notification(
                &camera,
                &message(
                    "tns1:Device/tt:Trigger/tt:DigitalInput",
                    &[("LogicalState", value)],
                    "Changed",
                ),
                WallUs::new(1),
            )
            .unwrap();
            let expected = if matches!(value, "true" | "1") {
                EventState::Start
            } else {
                EventState::Stop
            };
            assert_eq!(event.state, expected);
        }

        let unrelated_logical_state = normalize_notification(
            &camera,
            &message(
                "tns1:Monitoring/VendorState",
                &[("LogicalState", "true")],
                "Changed",
            ),
            WallUs::new(1),
        )
        .unwrap();
        assert_eq!(unrelated_logical_state.state, EventState::Pulse);

        let conflicting = normalize_notification(
            &camera,
            &message(
                "tns1:Monitoring/VendorState",
                &[("State", "true"), ("IsMotion", "false")],
                "Changed",
            ),
            WallUs::new(1),
        )
        .unwrap();
        assert_eq!(conflicting.state, EventState::Pulse);
    }

    #[test]
    fn every_property_operation_handles_true_false_invalid_and_missing_state() {
        let camera = CameraId::try_new("cam1").unwrap();
        let topic = "tns1:VideoSource/MotionAlarm";
        for (operation, value, expected) in [
            ("Initialized", Some("true"), Some(EventState::Start)),
            ("Initialized", Some("false"), None),
            ("Initialized", Some("invalid"), None),
            ("Initialized", None, None),
            ("Deleted", Some("true"), Some(EventState::Stop)),
            ("Deleted", Some("false"), Some(EventState::Stop)),
            ("Deleted", Some("invalid"), Some(EventState::Stop)),
            ("Deleted", None, Some(EventState::Stop)),
            ("Changed", Some("true"), Some(EventState::Start)),
            ("Changed", Some("false"), Some(EventState::Stop)),
            ("Changed", Some("invalid"), Some(EventState::Pulse)),
            ("Changed", None, Some(EventState::Pulse)),
        ] {
            let data = value.map_or_else(Vec::new, |value| vec![("State", value)]);
            let actual =
                normalize_notification(&camera, &message(topic, &data, operation), WallUs::new(1))
                    .map(|event| event.state);
            assert_eq!(actual, expected, "operation={operation} value={value:?}");
        }
    }

    #[test]
    fn structured_element_items_are_retained_without_interpretation() {
        let camera = CameraId::try_new("cam1").unwrap();
        let mut notification = message("tns1:RuleEngine/VendorAnalytics/Object", &[], "Changed");
        notification.data_elements.push(oxvif::ElementItem {
            name: "Object".to_owned(),
            content: "<Object><Id>7</Id></Object>".to_owned(),
            truncated: false,
        });
        let event = normalize_notification(&camera, &notification, WallUs::new(1)).unwrap();
        assert_eq!(event.state, EventState::Pulse);
        let payload: serde_json::Value =
            serde_json::from_str(event.data.as_deref().unwrap()).unwrap();
        assert_eq!(payload["data_elements"][0]["name"], "Object");
        assert_eq!(
            payload["data_elements"][0]["content"],
            "<Object><Id>7</Id></Object>"
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
        assert_eq!(parse_utc("2026-11-01T23:59:60Z"), None);
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

    #[test]
    fn rejects_impossible_dates_fractions_and_zone_offsets() {
        for (month, last_day, invalid_day) in [
            (1, 31, 32),
            (2, 28, 29),
            (3, 31, 32),
            (4, 30, 31),
            (5, 31, 32),
            (6, 30, 31),
            (7, 31, 32),
            (8, 31, 32),
            (9, 30, 31),
            (10, 31, 32),
            (11, 30, 31),
            (12, 31, 32),
        ] {
            assert!(parse_utc(&format!("2026-{month:02}-{last_day:02}T00:00:00Z")).is_some());
            assert_eq!(
                parse_utc(&format!("2026-{month:02}-{invalid_day:02}T00:00:00Z")),
                None
            );
        }
        for invalid in [
            "2026-02-29T00:00:00Z",
            "2026-02-31T00:00:00Z",
            "2026-04-31T00:00:00Z",
            "2100-02-29T00:00:00Z",
            "2026-01-01T00:00:00.Z",
            "2026-01-01T00:00:60Z",
            "2026-01-01T00:00:00+14:01",
            "2026-01-01T00:00:00-14:01",
            "2026-01-01T00:00:00+15:00",
            "2026-01-01T00:00:00+00:60",
            // One field out of range at a time.
            "1969-12-31T23:59:59Z",
            "2026-00-10T00:00:00Z",
            "2026-01-00T00:00:00Z",
            "2026-01-01T24:00:00Z",
            "2026-01-01T00:60:00Z",
            // One zone digit that is not a digit at a time.
            "2026-01-01T00:00:00+a0:00",
            "2026-01-01T00:00:00+0a:00",
            "2026-01-01T00:00:00+00:a0",
            "2026-01-01T00:00:00+00:0a",
            "2026-01-01T00:00:00+0000",
        ] {
            assert_eq!(parse_utc(invalid), None, "accepted {invalid}");
        }
        // One separator wrong at a time.
        for position in [4, 7, 10, 13, 16] {
            let mut text = b"2026-01-01T00:00:00Z".to_vec();
            text[position] = b'x';
            let text = String::from_utf8(text).unwrap();
            assert_eq!(parse_utc(&text), None, "accepted {text}");
        }
        assert_eq!(
            parse_utc("1970-01-01T13:59:00+13:59"),
            Some(WallUs::new(0)),
            "the largest minute offset"
        );
        for valid in [
            "2000-02-29T00:00:00Z",
            "2024-02-29T00:00:00Z",
            "2026-01-01T00:00:00+14:00",
            "2026-01-01T00:00:00-14:00",
        ] {
            assert!(parse_utc(valid).is_some(), "rejected {valid}");
        }
        assert_eq!(
            parse_utc("1970-01-01T00:00:00.123456789Z"),
            Some(WallUs::new(123_456))
        );
        assert_eq!(
            parse_utc("1970-01-01T00:00:00.000001Z"),
            Some(WallUs::new(1))
        );
        assert_eq!(
            parse_utc(&format!("1970-01-01T00:00:00.{}Z", "1".repeat(64))),
            None
        );
    }

    #[test]
    fn renewal_uses_granted_camera_clock_duration_with_safe_fallbacks() {
        assert!(MIN_RENEW_DELAY >= PULL_COOLDOWN);
        assert_eq!(
            renewal_delay("2026-01-01T00:00:00Z", "2026-01-01T00:01:00Z"),
            Duration::from_secs(48)
        );
        assert_eq!(
            renewal_delay("2026-01-01T10:00:00Z", "2026-01-01T10:20:00Z"),
            RENEW_EVERY
        );
        assert_eq!(
            renewal_delay("2026-01-01T00:00:00Z", "2026-01-01T00:00:04Z"),
            Duration::from_millis(3_200)
        );
        assert_eq!(
            renewal_delay("2026-01-01T00:00:00.000000Z", "2026-01-01T00:00:00.000001Z"),
            MIN_RENEW_DELAY
        );
        assert_eq!(
            renewal_delay("2026-01-01T00:00:00Z", "2026-01-01T00:00:00Z"),
            MIN_RENEW_DELAY
        );
        assert_eq!(
            renewal_delay("2026-01-01T00:00:00.000000Z", "2026-01-01T00:00:00.500000Z"),
            MIN_RENEW_DELAY
        );
        assert_eq!(
            renewal_delay("2026-01-01T00:00:00Z", "2026-01-01T00:00:01Z"),
            Duration::from_millis(800)
        );
        assert_eq!(
            renewal_delay("2026-01-01T00:00:01Z", "2026-01-01T00:00:00Z"),
            RENEW_EVERY
        );
        assert_eq!(renewal_delay("", ""), RENEW_EVERY);
        assert_eq!(renewal_delay("bad", "also-bad"), RENEW_EVERY);
        assert_eq!(
            renewal_delay("2026-01-01T00:00:00Z", "2026-01-01T00:10:00Z"),
            RENEW_EVERY
        );
        assert_eq!(
            renewal_delay("2030-01-01T00:00:00Z", "2030-01-01T00:00:30Z"),
            Duration::from_secs(24)
        );
    }

    #[test]
    fn authentication_and_clock_faults_are_classified_structurally() {
        let http = SessionError::Onvif(OnvifError::Transport(TransportError::HttpStatus {
            status: 401,
            body: String::new(),
        }));
        assert!(http.is_authentication_failure());
        assert!(!http.is_clock_authentication_failure());

        let unauthorized = SessionError::Onvif(OnvifError::Soap(SoapError::Fault {
            code: "s:Sender".to_owned(),
            reason: "Not Authorized".to_owned(),
            subcode: Some("ter:NotAuthorized".to_owned()),
            detail: None,
        }));
        assert!(unauthorized.is_authentication_failure());
        assert!(!unauthorized.is_clock_authentication_failure());

        let expired = SessionError::Onvif(OnvifError::Soap(SoapError::Fault {
            code: "s:Sender".to_owned(),
            reason: "Security timestamp expired".to_owned(),
            subcode: Some("wsse:MessageExpired".to_owned()),
            detail: None,
        }));
        assert!(expired.is_authentication_failure());
        assert!(expired.is_clock_authentication_failure());
        assert!(expired.retries_with_host_time(3_600));
        assert!(!expired.retries_with_host_time(0));

        let time_check = SessionError::Onvif(OnvifError::Soap(SoapError::Fault {
            code: "s:Sender".to_owned(),
            reason: "Wsse authorized time check failed".to_owned(),
            subcode: None,
            detail: None,
        }));
        assert!(
            time_check.is_clock_authentication_failure(),
            "the hardware fault wording of 11 is a clock fault"
        );
        assert!(time_check.retries_with_host_time(3_600));

        let unrelated = SessionError::Onvif(OnvifError::Soap(SoapError::Fault {
            code: "s:Receiver".to_owned(),
            reason: "Out of resources".to_owned(),
            subcode: Some("ter:Action".to_owned()),
            detail: None,
        }));
        assert!(!unrelated.is_authentication_failure());
        assert!(!unrelated.is_clock_authentication_failure());
        assert!(!unrelated.retries_with_host_time(3_600));

        let unrelated_timestamp = SessionError::Onvif(OnvifError::Soap(SoapError::Fault {
            code: "s:Receiver".to_owned(),
            reason: "Recording timestamp unavailable".to_owned(),
            subcode: Some("ter:Action".to_owned()),
            detail: Some("created time field was omitted".to_owned()),
        }));
        assert!(!unrelated_timestamp.is_authentication_failure());
        assert!(!unrelated_timestamp.is_clock_authentication_failure());
        assert!(!unrelated_timestamp.retries_with_host_time(3_600));

        for detail in ["InvalidTimestamp", "MessageExpired", "NotAuthorized"] {
            let detail_only = SessionError::Onvif(OnvifError::Soap(SoapError::Fault {
                code: "s:Receiver".to_owned(),
                reason: "Out of resources".to_owned(),
                subcode: Some("ter:Action".to_owned()),
                detail: Some(detail.to_owned()),
            }));
            assert!(
                !detail_only.is_authentication_failure(),
                "classified Fault Detail marker {detail} as authentication"
            );
            assert!(
                !detail_only.is_clock_authentication_failure(),
                "classified Fault Detail marker {detail} as a clock fault"
            );
            assert!(
                !detail_only.retries_with_host_time(3_600),
                "retried Fault Detail marker {detail} with host time"
            );
        }

        assert!(!SessionError::Timeout.retries_with_host_time(3_600));
        let parse = SessionError::Onvif(OnvifError::Soap(SoapError::XmlParse("marker".to_owned())));
        assert!(!parse.retries_with_host_time(3_600));
    }

    #[test]
    fn authentication_retries_wait_at_least_the_lockout_floor() {
        assert!(
            retry_delay(&mut Backoff::new("auth"), true) >= AUTH_FAILURE_PAUSE,
            "authentication retries must respect the lockout floor"
        );
        assert!(retry_delay(&mut Backoff::new("other"), false) < AUTH_FAILURE_PAUSE);
    }

    #[test]
    fn a_logged_session_error_is_one_bounded_line() {
        let hostile = format!("fault\n<0>forged{}", "x".repeat(2_048));
        let error = SessionError::Onvif(OnvifError::Soap(SoapError::Fault {
            code: "s:Sender".to_owned(),
            reason: hostile,
            subcode: None,
            detail: None,
        }));
        let line = LogSafe(&error).to_string();
        assert!(!line.contains('\n'), "{line}");
        assert!(line.contains("fault\\n<0>forged"), "{line}");
        assert!(
            line.len() <= iris_core::text::MAX_LOGGED_CHARS && line.ends_with("..."),
            "{}",
            line.len()
        );
    }

    /// Against a loopback port nothing listens on, the subscription fails,
    /// is counted, and is tried again after each backoff until shutdown,
    /// which ends the task at once.
    #[tokio::test(start_paused = true)]
    async fn a_camera_that_refuses_connections_is_retried_until_shutdown() {
        let unused = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = unused.local_addr().unwrap().port();
        drop(unused);
        let endpoint = CameraEndpoint {
            camera: "cam1".parse().unwrap(),
            host: "127.0.0.1".to_owned(),
            http_port: port,
            https: false,
            tls_verify: true,
            username: String::new(),
            password: String::new(),
        };
        let stats = Arc::new(EventStats::new());
        let shutdown = CancellationToken::new();
        let task = tokio::spawn(run_onvif_pullpoint(
            endpoint,
            Arc::new(Ignore),
            Arc::clone(&stats),
            shutdown.clone(),
        ));
        // Backoff starts at a second and is capped at thirty.
        tokio::time::sleep(Duration::from_mins(5)).await;
        let attempts = stats.reconnects();
        assert!(attempts >= 3, "{attempts} attempts in five minutes");
        assert_eq!(stats.errors(), attempts);
        assert!(!stats.connected());
        assert_eq!(stats.events(), 0);
        shutdown.cancel();
        timeout(Duration::from_secs(1), task)
            .await
            .expect("the task ends at shutdown")
            .unwrap();
        // At most the attempt that was in flight is counted after that.
        assert!(stats.reconnects() <= attempts + 1);
    }

    #[tokio::test]
    async fn cancellation_interrupts_the_authentication_retry_floor() {
        let shutdown = CancellationToken::new();
        let task_shutdown = shutdown.clone();
        let task =
            tokio::spawn(async move { wait_for_retry(&task_shutdown, AUTH_FAILURE_PAUSE).await });
        tokio::task::yield_now().await;
        shutdown.cancel();
        let slept = timeout(Duration::from_secs(1), task)
            .await
            .expect("retry wait observed cancellation")
            .unwrap();
        assert!(!slept);
    }

    #[test]
    fn setup_and_active_failures_are_counted_once_but_cancellation_is_not() {
        let stats = EventStats::new();
        assert_eq!(
            account_retry_cycle(&stats, &Err(SessionError::NoEventsService)),
            Some(false)
        );
        assert_eq!((stats.errors(), stats.reconnects()), (1, 1));

        assert_eq!(
            account_retry_cycle(&stats, &Err(SessionError::Timeout)),
            Some(false)
        );
        assert_eq!((stats.errors(), stats.reconnects()), (2, 2));

        assert_eq!(
            account_retry_cycle(&stats, &Err(SessionError::Cancelled)),
            None
        );
        assert_eq!((stats.errors(), stats.reconnects()), (2, 2));
    }
}
