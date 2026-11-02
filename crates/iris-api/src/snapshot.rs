//! Snapshot proxy with HTTP digest authentication (RFC 7616, MD5).

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use axum::extract::{Path, State};
use axum::http::header;
use axum::response::{IntoResponse, Response};
use iris_core::id::CameraId;
use iris_core::time::WallUs;

use crate::error::ApiError;
use crate::state::ApiState;

/// Largest snapshot accepted from a camera.
const MAX_SNAPSHOT_LEN: usize = 8 << 20;
const SNAPSHOT_PATH: &str = "/cgi-bin/snapshot.cgi?channel=1";

static CNONCE_COUNTER: AtomicU64 = AtomicU64::new(0);

pub(crate) async fn snapshot(
    State(state): State<Arc<ApiState>>,
    Path(camera): Path<String>,
) -> Result<Response, ApiError> {
    let id = CameraId::try_new(&camera).map_err(|e| ApiError::BadRequest(e.to_string()))?;
    let source = state.snapshot_sources.get(&id).ok_or(ApiError::NotFound)?;
    let scheme = if source.https { "https" } else { "http" };
    let url = format!("{scheme}://{}:{}{SNAPSHOT_PATH}", source.host, source.port);
    let client = reqwest::Client::builder()
        .danger_accept_invalid_certs(!source.tls_verify)
        .timeout(Duration::from_secs(5))
        .build()
        .map_err(|e| ApiError::Upstream(e.to_string()))?;
    let upstream = |e: reqwest::Error| ApiError::Upstream(e.to_string());
    let first = client.get(&url).send().await.map_err(upstream)?;
    let response = if first.status().as_u16() == 401 {
        let challenge = first
            .headers()
            .get(header::WWW_AUTHENTICATE)
            .and_then(|v| v.to_str().ok())
            .ok_or_else(|| ApiError::Upstream("camera sent no digest challenge".into()))?;
        let authorization = digest::authorization(
            "GET",
            SNAPSHOT_PATH,
            &source.username,
            &source.password,
            challenge,
            &cnonce(),
            1,
        )
        .ok_or_else(|| ApiError::Upstream("unsupported digest challenge".into()))?;
        client
            .get(&url)
            .header(header::AUTHORIZATION, authorization)
            .send()
            .await
            .map_err(upstream)?
    } else {
        first
    };
    if !response.status().is_success() {
        return Err(ApiError::Upstream(format!(
            "camera returned {}",
            response.status()
        )));
    }
    if response
        .content_length()
        .is_some_and(|n| n > MAX_SNAPSHOT_LEN as u64)
    {
        return Err(ApiError::Upstream("snapshot too large".into()));
    }
    let content_type = response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("image/jpeg")
        .to_owned();
    let body = response.bytes().await.map_err(upstream)?;
    if body.len() > MAX_SNAPSHOT_LEN {
        return Err(ApiError::Upstream("snapshot too large".into()));
    }
    Ok((
        [
            (header::CONTENT_TYPE, content_type),
            (header::CACHE_CONTROL, "no-store".to_owned()),
        ],
        body,
    )
        .into_response())
}

fn cnonce() -> String {
    let n = CNONCE_COUNTER.fetch_add(1, Ordering::Relaxed);
    let micros = WallUs::now().get() as u64;
    hex::encode((micros ^ n.rotate_left(32)).to_be_bytes())
}

/// HTTP digest (RFC 7616) with the MD5 algorithm, as IP cameras implement it.
pub(crate) mod digest {
    use std::fmt::Write as _;

    use md5::{Digest as _, Md5};

    /// The parts of a `WWW-Authenticate: Digest …` header irisd uses.
    #[derive(Clone, Debug, PartialEq, Eq)]
    pub(crate) struct Challenge {
        pub(crate) realm: String,
        pub(crate) nonce: String,
        pub(crate) qop_auth: bool,
        pub(crate) opaque: Option<String>,
        pub(crate) algorithm: Option<String>,
    }

    /// Parses a digest challenge; `None` for other schemes or missing fields.
    pub(crate) fn parse_challenge(header: &str) -> Option<Challenge> {
        let rest = header.trim().strip_prefix("Digest")?.trim_start();
        let mut realm = None;
        let mut nonce = None;
        let mut qop_auth = false;
        let mut opaque = None;
        let mut algorithm = None;
        for (key, value) in parameters(rest) {
            match key.to_ascii_lowercase().as_str() {
                "realm" => realm = Some(value),
                "nonce" => nonce = Some(value),
                "qop" => qop_auth = value.split(',').any(|q| q.trim() == "auth"),
                "opaque" => opaque = Some(value),
                "algorithm" => algorithm = Some(value),
                _ => {}
            }
        }
        Some(Challenge {
            realm: realm?,
            nonce: nonce?,
            qop_auth,
            opaque,
            algorithm,
        })
    }

    /// Splits `key=value, key="quoted, value"` pairs.
    fn parameters(input: &str) -> Vec<(String, String)> {
        let mut out = Vec::new();
        let mut rest = input;
        while !rest.is_empty() {
            let rest_trimmed = rest.trim_start_matches([' ', ',']);
            let Some(eq) = rest_trimmed.find('=') else {
                break;
            };
            let key = rest_trimmed[..eq].trim().to_owned();
            let after = &rest_trimmed[eq + 1..];
            let (value, remainder) = if let Some(quoted) = after.strip_prefix('"') {
                match quoted.find('"') {
                    Some(end) => (quoted[..end].to_owned(), &quoted[end + 1..]),
                    None => (quoted.to_owned(), ""),
                }
            } else {
                let end = after.find(',').unwrap_or(after.len());
                (after[..end].trim().to_owned(), &after[end..])
            };
            out.push((key, value));
            rest = remainder;
        }
        out
    }

    fn md5_hex(input: &str) -> String {
        hex::encode(Md5::digest(input.as_bytes()))
    }

    /// Builds the `Authorization` header value for `method` on `uri`.
    pub(crate) fn authorization(
        method: &str,
        uri: &str,
        username: &str,
        password: &str,
        challenge_header: &str,
        cnonce: &str,
        nc: u32,
    ) -> Option<String> {
        let c = parse_challenge(challenge_header)?;
        if c.algorithm
            .as_deref()
            .is_some_and(|a| !a.eq_ignore_ascii_case("MD5"))
        {
            return None;
        }
        let ha1 = md5_hex(&format!("{username}:{}:{password}", c.realm));
        let ha2 = md5_hex(&format!("{method}:{uri}"));
        let nc_hex = format!("{nc:08x}");
        let response = if c.qop_auth {
            md5_hex(&format!("{ha1}:{}:{nc_hex}:{cnonce}:auth:{ha2}", c.nonce))
        } else {
            md5_hex(&format!("{ha1}:{}:{ha2}", c.nonce))
        };
        let mut value = format!(
            "Digest username=\"{username}\", realm=\"{}\", nonce=\"{}\", uri=\"{uri}\", response=\"{response}\"",
            c.realm, c.nonce
        );
        if c.qop_auth {
            let _ = write!(value, ", qop=auth, nc={nc_hex}, cnonce=\"{cnonce}\"");
        }
        if let Some(opaque) = &c.opaque {
            let _ = write!(value, ", opaque=\"{opaque}\"");
        }
        if c.algorithm.is_some() {
            value.push_str(", algorithm=MD5");
        }
        Some(value)
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        const RFC2617: &str = "Digest realm=\"testrealm@host.com\", qop=\"auth,auth-int\", nonce=\"dcd98b7102dd2f0e8b11d0f600bfb0c093\", opaque=\"5ccc069c403ebaf9f0171e9517f40e41\"";

        #[test]
        fn parses_quoted_and_bare_parameters() {
            let c = parse_challenge(
                "Digest realm=\"Login to X\", nonce=abc123, qop=auth, algorithm=MD5",
            )
            .unwrap();
            assert_eq!(c.realm, "Login to X");
            assert_eq!(c.nonce, "abc123");
            assert!(c.qop_auth);
            assert_eq!(c.algorithm.as_deref(), Some("MD5"));
            assert!(parse_challenge("Basic realm=\"x\"").is_none());
        }

        #[test]
        fn reproduces_the_rfc_2617_example() {
            let header = authorization(
                "GET",
                "/dir/index.html",
                "Mufasa",
                "Circle Of Life",
                RFC2617,
                "0a4f113b",
                1,
            )
            .unwrap();
            assert!(
                header.contains("response=\"6629fae49393a05397450978507c4ef1\""),
                "{header}"
            );
            assert!(header.contains("qop=auth, nc=00000001, cnonce=\"0a4f113b\""));
            assert!(header.contains("opaque=\"5ccc069c403ebaf9f0171e9517f40e41\""));
        }
    }
}
