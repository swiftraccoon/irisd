//! Bearer-token authentication.

use std::sync::Arc;

use axum::extract::{Request, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

use crate::state::ApiState;

/// The token the caller presented, kept as a request extension so playlists can
/// propagate it into the URIs they emit.
#[derive(Clone, Debug)]
pub(crate) struct AuthToken(pub(crate) Option<String>);

/// Rejects requests without a valid token unless authentication is disabled.
pub(crate) async fn require_token(
    State(state): State<Arc<ApiState>>,
    mut req: Request,
    next: Next,
) -> Response {
    state.stats.record_request();
    if state.tokens.is_empty() {
        req.extensions_mut().insert(AuthToken(None));
        return next.run(req).await;
    }
    let presented = bearer(req.headers()).or_else(|| query_token(req.uri().query()));
    match presented {
        Some(token)
            if state
                .tokens
                .iter()
                .any(|k| constant_time_eq(k.as_bytes(), token.as_bytes())) =>
        {
            req.extensions_mut().insert(AuthToken(Some(token)));
            next.run(req).await
        }
        _ => (
            StatusCode::UNAUTHORIZED,
            [(header::WWW_AUTHENTICATE, "Bearer realm=\"irisd\"")],
        )
            .into_response(),
    }
}

fn bearer(headers: &HeaderMap) -> Option<String> {
    let value = headers.get(header::AUTHORIZATION)?.to_str().ok()?;
    let token = value.strip_prefix("Bearer ")?.trim();
    (!token.is_empty()).then(|| token.to_owned())
}

/// Extracts `token=` from a raw query string, decoding percent escapes.
pub(crate) fn query_token(query: Option<&str>) -> Option<String> {
    let raw = query?
        .split('&')
        .find_map(|pair| pair.strip_prefix("token="))?;
    percent_decode(raw)
}

fn percent_decode(raw: &str) -> Option<String> {
    let bytes = raw.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' => {
                let hex = bytes.get(i + 1..i + 3)?;
                let value = u8::from_str_radix(std::str::from_utf8(hex).ok()?, 16).ok()?;
                out.push(value);
                i += 3;
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8(out).ok()
}

/// Compares two byte strings without short-circuiting on the first difference.
pub(crate) fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compares_in_constant_time_shape() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"ab"));
    }

    #[test]
    fn extracts_and_decodes_query_tokens() {
        assert_eq!(
            query_token(Some("from=1&token=s3cr%2Bt")).as_deref(),
            Some("s3cr+t")
        );
        assert_eq!(query_token(Some("from=1")), None);
        assert_eq!(query_token(None), None);
        assert_eq!(query_token(Some("token=%zz")), None);
    }
}
