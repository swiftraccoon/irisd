//! Bearer-token authentication and scoped media capabilities.

mod token;

use std::fmt;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::extract::{Request, State};
use axum::http::{HeaderMap, Method, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use sha2::{Digest as _, Sha256};

use crate::auth::token::{CAPABILITY_TAG_BYTES, CAPABILITY_VERSION, parse_capability, query_token};
use crate::query::MAX_RANGE_US;
use crate::state::ApiState;

/// Rolling media capabilities expire quickly enough to limit URL disclosure.
const ROLLING_CAPABILITY_TTL: Duration = Duration::from_mins(15);
/// An immutable VOD playlist cannot renew child capabilities. Its longest
/// capability therefore matches the API's maximum selectable playback span.
pub(crate) const MAX_VOD_CAPABILITY_TTL: Duration = Duration::from_micros(MAX_RANGE_US as u64);
/// Rolling capability expiries are rounded down to this step, so every
/// playlist reload within one step mints identical child URIs.
const ROLLING_EXPIRY_STEP_SECS: u64 = 60;
/// Length of a [`CapabilitySecret`].
pub const CAPABILITY_SECRET_BYTES: usize = 32;

/// The secret that signs media capabilities.
///
/// The daemon draws it from the kernel's random source once, keeps it in its
/// state directory with mode 0600, and reuses it across restarts, so a
/// capability outlives a restart. The root tokens take no part in a
/// capability: a captured capability URL reveals nothing about any token, and
/// every token issues interchangeable capabilities. The `Debug` output is
/// redacted.
///
/// ```
/// use iris_api::CapabilitySecret;
///
/// let secret = CapabilitySecret::new([7; 32]);
/// assert_eq!(format!("{secret:?}"), "CapabilitySecret([redacted])");
/// ```
#[derive(Clone)]
pub struct CapabilitySecret([u8; CAPABILITY_SECRET_BYTES]);

impl CapabilitySecret {
    /// Wraps secret bytes drawn from a cryptographic random source.
    #[must_use]
    pub const fn new(bytes: [u8; CAPABILITY_SECRET_BYTES]) -> Self {
        Self(bytes)
    }

    /// The secret bytes, for persisting them.
    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; CAPABILITY_SECRET_BYTES] {
        &self.0
    }
}

impl fmt::Debug for CapabilitySecret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("CapabilitySecret([redacted])")
    }
}

/// A fixed secret for tests of capability issuance.
#[cfg(test)]
pub(crate) fn test_secret() -> CapabilitySecret {
    CapabilitySecret::new([7; CAPABILITY_SECRET_BYTES])
}

/// Authentication context kept as a request extension so playlists can issue
/// path-specific media capabilities without retaining or emitting a root token.
#[derive(Clone, Debug)]
pub(crate) struct AuthContext {
    issuer: Option<CapabilityIssuer>,
}

impl AuthContext {
    pub(crate) fn disabled() -> Self {
        Self { issuer: None }
    }

    /// Issues a capability for one exact request path. Authentication-disabled
    /// deployments need no query credential and return `None`.
    pub(crate) fn capability(&self, path: &str) -> Option<String> {
        self.issuer.as_ref().map(|issuer| issuer.issue(path))
    }

    /// Issues a capability for one exact path with a caller-selected bounded
    /// lifetime. The route's own ceiling and inherited authority still apply.
    pub(crate) fn capability_for(&self, path: &str, lifetime: Duration) -> Option<String> {
        self.issuer
            .as_ref()
            .map(|issuer| issuer.issue_for(path, lifetime))
    }

    #[cfg(test)]
    pub(crate) fn for_test(secret: &CapabilitySecret, expires_at: u64) -> Self {
        Self {
            issuer: Some(CapabilityIssuer {
                key: CapabilityKey::from_secret(secret),
                issued_at: expires_at.saturating_sub(ROLLING_CAPABILITY_TTL.as_secs()),
                default_expires_at: expires_at,
                max_expires_at: expires_at,
            }),
        }
    }

    #[cfg(test)]
    pub(crate) fn for_test_at(secret: &CapabilitySecret, issued_at: u64) -> Self {
        Self {
            issuer: Some(CapabilityIssuer {
                key: CapabilityKey::from_secret(secret),
                issued_at,
                default_expires_at: issued_at.saturating_add(ROLLING_CAPABILITY_TTL.as_secs()),
                max_expires_at: issued_at.saturating_add(MAX_VOD_CAPABILITY_TTL.as_secs()),
            }),
        }
    }
}

#[derive(Clone)]
struct CapabilityKey([u8; CAPABILITY_TAG_BYTES]);

impl fmt::Debug for CapabilityKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("CapabilityKey([redacted])")
    }
}

impl CapabilityKey {
    fn from_secret(secret: &CapabilitySecret) -> Self {
        let mut digest = Sha256::new();
        digest.update(b"irisd media capability key v2\0");
        digest.update(secret.as_bytes());
        Self(digest.finalize().into())
    }

    fn tag(&self, path: &str, expires_at: u64) -> [u8; CAPABILITY_TAG_BYTES] {
        hmac_sha256(
            &self.0,
            &[
                b"irisd media capability v1\0",
                b"GET|HEAD\0",
                &expires_at.to_be_bytes(),
                b"\0",
                path.as_bytes(),
            ],
        )
    }
}

#[derive(Clone, Debug)]
struct CapabilityIssuer {
    key: CapabilityKey,
    issued_at: u64,
    default_expires_at: u64,
    max_expires_at: u64,
}

impl CapabilityIssuer {
    fn issue(&self, path: &str) -> String {
        let route_limit = capability_ttl_limit(path).unwrap_or(ROLLING_CAPABILITY_TTL);
        let route_expires_at = self.issued_at.saturating_add(route_limit.as_secs());
        self.issue_at(
            path,
            self.default_expires_at
                .min(self.max_expires_at)
                .min(route_expires_at),
        )
    }

    fn issue_for(&self, path: &str, lifetime: Duration) -> String {
        let route_limit = capability_ttl_limit(path).unwrap_or(ROLLING_CAPABILITY_TTL);
        let bounded_lifetime = lifetime.min(route_limit);
        let expires_at = self
            .issued_at
            .saturating_add(bounded_lifetime.as_secs())
            .min(self.max_expires_at);
        self.issue_at(path, expires_at)
    }

    fn issue_at(&self, path: &str, expires_at: u64) -> String {
        let tag = self.key.tag(path, expires_at);
        format!("{CAPABILITY_VERSION}.{}.{}", expires_at, hex::encode(tag))
    }
}

/// Rejects requests without a valid root bearer or path-specific capability
/// unless authentication is explicitly disabled with an empty token list.
pub(crate) async fn require_token(
    State(state): State<Arc<ApiState>>,
    mut req: Request,
    next: Next,
) -> Response {
    if state.tokens.is_empty() {
        req.extensions_mut().insert(AuthContext::disabled());
        state.stats.record_request();
        return next.run(req).await;
    }
    // Configuration loading rejects these entries. Keep this boundary fail-closed
    // for callers that construct ApiState directly.
    if state.tokens.iter().any(|token| token.trim().is_empty()) {
        return unauthorized();
    }

    let now = unix_seconds();
    let issuer = bearer(req.headers())
        .and_then(|presented| root_issuer(&state.tokens, &state.capability_secret, presented, now))
        .or_else(|| capability_issuer(&state.capability_secret, &req, now));
    let Some(issuer) = issuer else {
        return unauthorized();
    };
    req.extensions_mut().insert(AuthContext {
        issuer: Some(issuer),
    });
    state.stats.record_request();
    next.run(req).await
}

fn unauthorized() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        [(header::WWW_AUTHENTICATE, "Bearer realm=\"irisd\"")],
    )
        .into_response()
}

fn root_issuer(
    tokens: &[String],
    secret: &CapabilitySecret,
    presented: &str,
    now: Option<u64>,
) -> Option<CapabilityIssuer> {
    if presented.trim().is_empty() {
        return None;
    }
    // Every configured token is compared so the timing does not reveal which
    // one matched or how many there are.
    let mut matched = false;
    for token in tokens {
        matched |= constant_time_eq(token.as_bytes(), presented.as_bytes());
    }
    if !matched {
        return None;
    }
    let now = now?;
    // Rounded down to the minute so every playlist reload within it mints the
    // same child URIs: a preload hint then matches the part listed next.
    let default_expires_at = now.checked_add(ROLLING_CAPABILITY_TTL.as_secs())?;
    let default_expires_at = default_expires_at - default_expires_at % ROLLING_EXPIRY_STEP_SECS;
    let max_expires_at = now.checked_add(MAX_VOD_CAPABILITY_TTL.as_secs())?;
    Some(CapabilityIssuer {
        key: CapabilityKey::from_secret(secret),
        issued_at: now,
        default_expires_at,
        max_expires_at,
    })
}

fn capability_issuer(
    secret: &CapabilitySecret,
    req: &Request,
    now: Option<u64>,
) -> Option<CapabilityIssuer> {
    if !matches!(*req.method(), Method::GET | Method::HEAD) {
        return None;
    }
    let path = req.uri().path();
    let ttl_limit = capability_ttl_limit(path)?;
    let encoded = query_token(req.uri().query())?;
    let (expires_at, supplied_tag) = parse_capability(&encoded)?;
    let now = now?;
    if expires_at < now || expires_at > now.checked_add(ttl_limit.as_secs())? {
        return None;
    }

    let key = CapabilityKey::from_secret(secret);
    let expected = key.tag(path, expires_at);
    constant_time_eq(&expected, &supplied_tag).then_some(CapabilityIssuer {
        key,
        issued_at: now,
        default_expires_at: expires_at,
        max_expires_at: expires_at,
    })
}

#[cfg(test)]
fn is_capability_path(path: &str) -> bool {
    capability_ttl_limit(path).is_some()
}

/// The longest lifetime a capability for `path` may carry, or `None` for a
/// path no capability authorizes. The paths are the ones playlists issue
/// capabilities for, and nothing else: a VOD playlist's recording files
/// (`hls::file_uri`) and the live playlist's init, segment, and part
/// resources (`llhls::uri`). A valid signature for one of them cannot become
/// authority for playlists, the live WebSocket, JSON, snapshots, exports, or
/// mutations.
fn capability_ttl_limit(path: &str) -> Option<Duration> {
    let fields: Vec<&str> = path.split('/').filter(|field| !field.is_empty()).collect();
    match fields.as_slice() {
        ["api", "segments", id, "file.mp4"] if id.parse::<i64>().is_ok() => {
            Some(MAX_VOD_CAPABILITY_TTL)
        }
        ["api", "streams", camera, kind, "hls", "init.mp4"]
            if valid_stream_fields(camera, kind) =>
        {
            Some(ROLLING_CAPABILITY_TTL)
        }
        [
            "api",
            "streams",
            camera,
            kind,
            "hls",
            "seg",
            msn,
            "media.m4s",
        ] if valid_stream_fields(camera, kind) && msn.parse::<u64>().is_ok() => {
            Some(ROLLING_CAPABILITY_TTL)
        }
        [
            "api",
            "streams",
            camera,
            kind,
            "hls",
            "part",
            msn,
            index,
            "part.m4s",
        ] if valid_stream_fields(camera, kind)
            && msn.parse::<u64>().is_ok()
            && index.parse::<usize>().is_ok() =>
        {
            Some(ROLLING_CAPABILITY_TTL)
        }
        _ => None,
    }
}

fn valid_stream_fields(camera: &str, kind: &str) -> bool {
    !camera.is_empty() && matches!(kind, "main" | "sub" | "audio")
}

fn bearer(headers: &HeaderMap) -> Option<&str> {
    let value = headers.get(header::AUTHORIZATION)?.to_str().ok()?;
    let token = value.strip_prefix("Bearer ")?.trim();
    (!token.is_empty()).then_some(token)
}

fn unix_seconds() -> Option<u64> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .map(|duration| duration.as_secs())
}

fn hmac_sha256(key: &[u8; 32], fields: &[&[u8]]) -> [u8; 32] {
    const BLOCK_BYTES: usize = 64;
    let mut inner_pad = [0x36; BLOCK_BYTES];
    let mut outer_pad = [0x5c; BLOCK_BYTES];
    for (index, byte) in key.iter().copied().enumerate() {
        inner_pad[index] ^= byte;
        outer_pad[index] ^= byte;
    }
    let mut inner = Sha256::new();
    inner.update(inner_pad);
    for field in fields {
        inner.update(field);
    }
    let inner_tag = inner.finalize();
    let mut outer = Sha256::new();
    outer.update(outer_pad);
    outer.update(inner_tag);
    outer.finalize().into()
}

/// Compares two byte strings without short-circuiting on the first difference.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constant_time_eq_matches_only_identical_bytes() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"ab"));
    }

    /// RFC 4231 test case 1: a 20-byte key of `0x0b`, zero-padded to the
    /// 32-byte key this function takes (HMAC pads keys to the block size
    /// with zeros, so the tag is unchanged), over `"Hi There"`.
    #[test]
    fn hmac_sha256_matches_rfc_4231_case_1() {
        let mut key = [0u8; 32];
        key[..20].fill(0x0b);
        assert_eq!(
            hex::encode(hmac_sha256(&key, &[b"Hi There"])),
            "b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7"
        );
        assert_eq!(
            hmac_sha256(&key, &[b"Hi ", b"There"]),
            hmac_sha256(&key, &[b"Hi There"]),
            "fields are concatenated"
        );
    }

    #[test]
    fn rolling_capabilities_share_an_expiry_within_the_minute() {
        let tokens = vec!["root secret".to_owned()];
        let secret = test_secret();
        let path = "/api/streams/cam1/main/hls/init.mp4";
        let at = |now: u64| {
            root_issuer(&tokens, &secret, "root secret", Some(now))
                .unwrap()
                .issue(path)
        };
        // The step applies to the expiry, so issuers whose default expiries
        // fall in the same minute agree.
        assert_eq!(at(10_020), at(10_079));
        assert_ne!(at(10_020), at(10_080));
        let (expires_at, _) = parse_capability(&at(10_020)).unwrap();
        assert_eq!(expires_at % ROLLING_EXPIRY_STEP_SECS, 0);
        assert!(
            expires_at > 10_020 + ROLLING_CAPABILITY_TTL.as_secs() - ROLLING_EXPIRY_STEP_SECS
                && expires_at <= 10_020 + ROLLING_CAPABILITY_TTL.as_secs()
        );
    }

    #[test]
    fn extracts_and_decodes_query_tokens_but_rejects_blanks() {
        assert_eq!(
            query_token(Some("from=1&token=s3cr%2Bt")).as_deref(),
            Some("s3cr+t")
        );
        assert_eq!(query_token(Some("from=1")), None);
        assert_eq!(query_token(None), None);
        assert_eq!(query_token(Some("token=%zz")), None);
        assert_eq!(query_token(Some("token=")), None);
        assert_eq!(query_token(Some("token=%20%20")), None);
    }

    #[test]
    fn capabilities_are_exact_path_and_tamper_resistant() {
        let key = CapabilityKey::from_secret(&test_secret());
        let issuer = CapabilityIssuer {
            key: key.clone(),
            issued_at: 1_000_000,
            default_expires_at: 1_234_567,
            max_expires_at: 1_234_567,
        };
        let encoded = issuer.issue("/api/segments/7/file.mp4");
        let (expires_at, tag) = parse_capability(&encoded).unwrap();
        assert_eq!(expires_at, 1_234_567);
        assert!(constant_time_eq(
            &tag,
            &key.tag("/api/segments/7/file.mp4", expires_at)
        ));
        assert!(!constant_time_eq(
            &tag,
            &key.tag("/api/segments/8/file.mp4", expires_at)
        ));
        let mut tampered = encoded.into_bytes();
        let last = tampered.last_mut().unwrap();
        *last = if *last == b'0' { b'1' } else { b'0' };
        let tampered = String::from_utf8(tampered).unwrap();
        let (_, tag) = parse_capability(&tampered).unwrap();
        assert!(!constant_time_eq(
            &tag,
            &key.tag("/api/segments/7/file.mp4", expires_at)
        ));
    }

    #[test]
    fn only_issued_media_paths_accept_capabilities() {
        for path in [
            "/api/segments/7/file.mp4",
            "/api/streams/cam1/main/hls/init.mp4",
            "/api/streams/cam1/main/hls/seg/1/media.m4s",
            "/api/streams/cam1/main/hls/part/1/2/part.m4s",
        ] {
            assert!(is_capability_path(path), "{path}");
        }
        for path in [
            "/api/cameras",
            "/api/cameras/cam1/snapshot.jpg",
            "/api/streams/cam1/main/segments",
            "/api/streams/cam1/main/export.mp4",
            "/api/streams/cam1/main/keyframe",
            "/api/streams/cam1/main/hls/vod.m3u8",
            "/api/streams/cam1/sub/hls/live.m3u8",
            "/api/streams/cam1/main/live",
            "/api/events/1/retain",
        ] {
            assert!(!is_capability_path(path), "{path}");
        }
    }

    #[test]
    fn capability_verification_enforces_method_path_expiry_and_integrity() {
        let tokens = vec!["root secret".to_owned()];
        let secret = test_secret();
        let now = 10_000;
        let live_path = "/api/streams/cam1/main/hls/seg/1/media.m4s";
        let issuer = root_issuer(&tokens, &secret, "root secret", Some(now)).unwrap();
        let capability = issuer.issue(live_path);
        let request = |method: Method, path: &str, capability: &str| {
            Request::builder()
                .method(method)
                .uri(format!("{path}?token={capability}"))
                .body(axum::body::Body::empty())
                .unwrap()
        };

        for method in [Method::GET, Method::HEAD] {
            assert!(
                capability_issuer(&secret, &request(method, live_path, &capability), Some(now))
                    .is_some()
            );
        }
        assert!(
            capability_issuer(
                &secret,
                &request(Method::POST, live_path, &capability),
                Some(now)
            )
            .is_none()
        );
        for path in [
            "/api/streams/cam1/sub/hls/seg/1/media.m4s",
            "/api/streams/cam1/main/hls/seg/2/media.m4s",
            "/api/streams/cam1/main/live",
            "/api/cameras",
            "/api/cameras/cam1/snapshot.jpg",
            "/api/streams/cam1/main/export.mp4",
            "/api/events/1/retain",
        ] {
            assert!(
                capability_issuer(&secret, &request(Method::GET, path, &capability), Some(now))
                    .is_none(),
                "capability escaped to {path}"
            );
        }

        let expired = CapabilityIssuer {
            key: CapabilityKey::from_secret(&secret),
            issued_at: now - 2,
            default_expires_at: now - 1,
            max_expires_at: now - 1,
        }
        .issue(live_path);
        assert!(
            capability_issuer(
                &secret,
                &request(Method::GET, live_path, &expired),
                Some(now)
            )
            .is_none()
        );

        let mut tampered = capability.into_bytes();
        let last = tampered.last_mut().unwrap();
        *last = if *last == b'0' { b'1' } else { b'0' };
        let tampered = String::from_utf8(tampered).unwrap();
        assert!(
            capability_issuer(
                &secret,
                &request(Method::GET, live_path, &tampered),
                Some(now)
            )
            .is_none()
        );
    }

    #[test]
    fn immutable_vod_capabilities_cover_the_selected_horizon_and_remain_bounded() {
        let tokens = vec!["root secret".to_owned()];
        let secret = test_secret();
        let issued_at = 10_000;
        let path = "/api/segments/7/file.mp4";
        let issuer = root_issuer(&tokens, &secret, "root secret", Some(issued_at)).unwrap();
        let default_playback = Duration::from_hours(1);
        let lifetime = default_playback + ROLLING_CAPABILITY_TTL;
        let capability = issuer.issue_for(path, lifetime);
        let (expires_at, _) = parse_capability(&capability).unwrap();
        assert_eq!(expires_at, issued_at + lifetime.as_secs());

        let request = |path: &str, capability: &str| {
            Request::builder()
                .uri(format!("{path}?token={capability}"))
                .body(axum::body::Body::empty())
                .unwrap()
        };
        assert!(
            capability_issuer(
                &secret,
                &request(path, &capability),
                Some(issued_at + default_playback.as_secs()),
            )
            .is_some(),
            "the final URI in the default one-hour playback must remain authorized"
        );
        assert!(
            capability_issuer(&secret, &request(path, &capability), Some(expires_at + 1),)
                .is_none(),
            "the VOD capability must expire after its transport margin"
        );

        let capped = issuer.issue_for(path, MAX_VOD_CAPABILITY_TTL + ROLLING_CAPABILITY_TTL);
        let (capped_expires_at, _) = parse_capability(&capped).unwrap();
        assert_eq!(
            capped_expires_at,
            issued_at + MAX_VOD_CAPABILITY_TTL.as_secs()
        );

        let live_path = "/api/streams/cam1/main/hls/seg/1/media.m4s";
        let live = issuer.issue_for(live_path, MAX_VOD_CAPABILITY_TTL);
        let (live_expires_at, _) = parse_capability(&live).unwrap();
        assert_eq!(
            live_expires_at,
            issued_at + ROLLING_CAPABILITY_TTL.as_secs(),
            "rolling LL-HLS children retain the short lifetime"
        );
    }

    #[test]
    fn capabilities_depend_on_the_secret_and_not_on_the_issuing_token() {
        let tokens = vec![
            "first root token".to_owned(),
            "second root token".to_owned(),
        ];
        let secret = test_secret();
        let now = 10_000;
        let path = "/api/streams/cam1/main/hls/init.mp4";
        let capability = root_issuer(&tokens, &secret, "second root token", Some(now))
            .unwrap()
            .issue(path);
        assert!(!capability.contains("root token"));
        let request = Request::builder()
            .uri(format!("{path}?token={capability}"))
            .body(axum::body::Body::empty())
            .unwrap();
        assert!(capability_issuer(&secret, &request, Some(now)).is_some());
        let other_secret = CapabilitySecret::new([8; CAPABILITY_SECRET_BYTES]);
        assert!(
            capability_issuer(&other_secret, &request, Some(now)).is_none(),
            "a tag under one secret never verifies under another"
        );
        assert!(root_issuer(&tokens, &secret, "third root token", Some(now)).is_none());
        assert!(root_issuer(&tokens, &secret, " ", Some(now)).is_none());
    }
}
