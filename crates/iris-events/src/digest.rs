//! HTTP Digest authentication (RFC 7616, MD5 and MD5-sess, `qop=auth`).

use std::fmt;
use std::fmt::Write as _;
use std::time::{SystemTime, UNIX_EPOCH};

use md5::{Digest as _, Md5};

/// A parsed `WWW-Authenticate: Digest …` challenge.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Challenge {
    /// Protection realm.
    pub realm: String,
    /// Server nonce.
    pub nonce: String,
    /// `auth` when the server offers it; `None` for RFC 2069 style.
    pub qop: Option<String>,
    /// Opaque value echoed back.
    pub opaque: Option<String>,
    /// `MD5` (default) or `MD5-sess`.
    pub algorithm: String,
    /// Whether the previous nonce merely expired.
    pub stale: bool,
}

/// Parses the value of a `WWW-Authenticate` header. Returns `None` unless it
/// is a Digest challenge with a realm and a nonce.
#[must_use]
pub fn parse_challenge(value: &str) -> Option<Challenge> {
    let rest = value.trim().strip_prefix("Digest")?.trim_start();
    let mut realm = None;
    let mut nonce = None;
    let mut qop = None;
    let mut opaque = None;
    let mut algorithm = String::from("MD5");
    let mut stale = false;
    for (key, val) in split_params(rest) {
        match key.to_ascii_lowercase().as_str() {
            "realm" => realm = Some(val),
            "nonce" => nonce = Some(val),
            "qop" => {
                qop = val
                    .split(',')
                    .map(str::trim)
                    .find(|q| *q == "auth")
                    .map(str::to_owned);
            }
            "opaque" => opaque = Some(val),
            "algorithm" => algorithm = val,
            "stale" => stale = val.eq_ignore_ascii_case("true"),
            _ => {}
        }
    }
    Some(Challenge {
        realm: realm?,
        nonce: nonce?,
        qop,
        opaque,
        algorithm,
        stale,
    })
}

/// Splits `k=v, k2="v, with comma"` pairs, honouring quotes.
fn split_params(s: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut key = String::new();
    let mut val = String::new();
    let mut in_key = true;
    let mut quoted = false;
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        match (in_key, quoted, c) {
            (true, _, '=') => in_key = false,
            (true, _, ',') => {
                key.clear();
            }
            (true, _, c) if !c.is_whitespace() => key.push(c),
            (true, _, _) => {}
            (false, false, '"') => quoted = true,
            (false, true, '"') => quoted = false,
            (false, true, '\\') => {
                if let Some(next) = chars.next() {
                    val.push(next);
                }
            }
            (false, false, ',') => {
                out.push((key.trim().to_owned(), val.trim().to_owned()));
                key.clear();
                val.clear();
                in_key = true;
            }
            (false, _, c) => val.push(c),
        }
    }
    if !key.trim().is_empty() {
        out.push((key.trim().to_owned(), val.trim().to_owned()));
    }
    out
}

fn md5_hex(input: &str) -> String {
    hex::encode(Md5::digest(input.as_bytes()))
}

/// Computes the `response` value.
///
/// `ha1` is `MD5(user:realm:password)` (already session-derived for
/// `MD5-sess`), `ha2` is `MD5(method:uri)`.
#[must_use]
pub fn compute_response(
    ha1: &str,
    nonce: &str,
    qop: Option<(&str, u32, &str)>,
    ha2: &str,
) -> String {
    match qop {
        Some((qop, nc, cnonce)) => md5_hex(&format!("{ha1}:{nonce}:{nc:08x}:{cnonce}:{qop}:{ha2}")),
        None => md5_hex(&format!("{ha1}:{nonce}:{ha2}")),
    }
}

/// Digest state for one camera: credentials, the current challenge, and the
/// nonce count.
#[derive(Clone)]
pub struct DigestAuth {
    username: String,
    password: String,
    challenge: Option<Challenge>,
    nonce_count: u32,
}

impl fmt::Debug for DigestAuth {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("DigestAuth")
            .field("username", &"<redacted>")
            .field("password", &"<redacted>")
            .field("has_challenge", &self.challenge.is_some())
            .field("nonce_count", &self.nonce_count)
            .finish()
    }
}

impl DigestAuth {
    /// Creates state without a challenge.
    #[must_use]
    pub fn new(username: &str, password: &str) -> Self {
        Self {
            username: username.to_owned(),
            password: password.to_owned(),
            challenge: None,
            nonce_count: 0,
        }
    }

    /// Whether a challenge has been seen.
    #[must_use]
    pub fn has_challenge(&self) -> bool {
        self.challenge.is_some()
    }

    /// Adopts a challenge from a 401 response.
    pub fn set_challenge(&mut self, challenge: Challenge) {
        self.challenge = Some(challenge);
        self.nonce_count = 0;
    }

    /// Builds an `Authorization` header value for `method` and `uri`, or
    /// `None` without a challenge.
    pub fn authorization(&mut self, method: &str, uri: &str) -> Option<String> {
        let cnonce = clock_cnonce();
        self.authorization_with(method, uri, &cnonce)
    }

    /// As [`DigestAuth::authorization`] with an explicit client nonce (testable).
    pub fn authorization_with(&mut self, method: &str, uri: &str, cnonce: &str) -> Option<String> {
        let challenge = self.challenge.as_ref()?;
        self.nonce_count += 1;
        let nc = self.nonce_count;
        let mut ha1 = md5_hex(&format!(
            "{}:{}:{}",
            self.username, challenge.realm, self.password
        ));
        if challenge.algorithm.eq_ignore_ascii_case("MD5-sess") {
            ha1 = md5_hex(&format!("{ha1}:{}:{cnonce}", challenge.nonce));
        }
        let ha2 = md5_hex(&format!("{method}:{uri}"));
        let qop = challenge.qop.as_deref().map(|q| (q, nc, cnonce));
        let response = compute_response(&ha1, &challenge.nonce, qop, &ha2);
        let mut header = format!(
            "Digest username=\"{}\", realm=\"{}\", nonce=\"{}\", uri=\"{}\", response=\"{response}\", algorithm={}",
            self.username, challenge.realm, challenge.nonce, uri, challenge.algorithm
        );
        if let Some(q) = &challenge.qop {
            let _ = write!(header, ", qop={q}, nc={nc:08x}, cnonce=\"{cnonce}\"");
        }
        if let Some(opaque) = &challenge.opaque {
            let _ = write!(header, ", opaque=\"{opaque}\"");
        }
        Some(header)
    }
}

/// A client nonce from the clock; uniqueness per request is all that matters.
fn clock_cnonce() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    format!(
        "{:016x}",
        (nanos as u64).wrapping_mul(0x9e37_79b9_7f4a_7c15)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_challenge_with_quoted_lists() {
        let c = parse_challenge(
            "Digest realm=\"testrealm@host.com\", qop=\"auth,auth-int\", nonce=\"dcd98b7102dd2f0e8b11d0f600bfb0c093\", opaque=\"5ccc069c403ebaf9f0171e9517f40e41\", stale=FALSE",
        )
        .unwrap();
        assert_eq!(c.realm, "testrealm@host.com");
        assert_eq!(c.nonce, "dcd98b7102dd2f0e8b11d0f600bfb0c093");
        assert_eq!(c.qop.as_deref(), Some("auth"));
        assert_eq!(
            c.opaque.as_deref(),
            Some("5ccc069c403ebaf9f0171e9517f40e41")
        );
        assert_eq!(c.algorithm, "MD5");
        assert!(!c.stale);
        assert!(parse_challenge("Basic realm=\"x\"").is_none());
    }

    #[test]
    fn matches_rfc2617_example_with_qop_auth() {
        let mut auth = DigestAuth::new("Mufasa", "Circle Of Life");
        auth.set_challenge(parse_challenge("Digest realm=\"testrealm@host.com\", qop=\"auth,auth-int\", nonce=\"dcd98b7102dd2f0e8b11d0f600bfb0c093\", opaque=\"5ccc069c403ebaf9f0171e9517f40e41\"").unwrap());
        let header = auth
            .authorization_with("GET", "/dir/index.html", "0a4f113b")
            .unwrap();
        assert!(
            header.contains("response=\"6629fae49393a05397450978507c4ef1\""),
            "{header}"
        );
        assert!(header.contains("nc=00000001"));
        assert!(header.contains("qop=auth"));
        // The nonce count advances per request.
        let second = auth
            .authorization_with("GET", "/dir/index.html", "0a4f113b")
            .unwrap();
        assert!(second.contains("nc=00000002"));
    }

    #[test]
    fn matches_rfc7616_md5_example() {
        let mut auth = DigestAuth::new("Mufasa", "Circle of Life");
        auth.set_challenge(parse_challenge("Digest realm=\"http-auth@example.org\", qop=\"auth, auth-int\", algorithm=MD5, nonce=\"7ypf/xlj9XXwfDPEoM4URrv/xwf94BcCAzFZH4GiTo0v\", opaque=\"FQhe/qaU925kfnzjCev0ciny7QMkPqMAFRtzCUYo5tdS\"").unwrap());
        let header = auth
            .authorization_with(
                "GET",
                "/dir/index.html",
                "f2/wE4q74E6zIJEtWaHKaf5wv/H5QzzpXusqGemxURZJ",
            )
            .unwrap();
        assert!(
            header.contains("response=\"8ca523f5e9506fed4657c9700eebdbec\""),
            "{header}"
        );
    }

    #[test]
    fn computes_legacy_response_without_qop() {
        // Same inputs as the RFC 2617 example but without qop (RFC 2069 form).
        let ha1 = md5_hex("Mufasa:testrealm@host.com:Circle Of Life");
        let ha2 = md5_hex("GET:/dir/index.html");
        assert_eq!(
            compute_response(&ha1, "dcd98b7102dd2f0e8b11d0f600bfb0c093", None, &ha2),
            "670fd8c2df070c60b045671b8b24ff02"
        );
        let mut auth = DigestAuth::new("Mufasa", "Circle Of Life");
        auth.set_challenge(
            parse_challenge(
                "Digest realm=\"testrealm@host.com\", nonce=\"dcd98b7102dd2f0e8b11d0f600bfb0c093\"",
            )
            .unwrap(),
        );
        let header = auth.authorization("GET", "/dir/index.html").unwrap();
        assert!(header.contains("response=\"670fd8c2df070c60b045671b8b24ff02\""));
        assert!(!header.contains("qop="));
    }

    #[test]
    fn debug_redacts_credentials_and_challenge() {
        let mut auth = DigestAuth::new("distinctive-user", "distinctive-password");
        auth.set_challenge(
            parse_challenge("Digest realm=\"distinctive-realm\", nonce=\"distinctive-nonce\"")
                .unwrap(),
        );
        let _ = auth
            .authorization_with("GET", "/", "distinctive-cnonce")
            .unwrap();
        for rendered in [format!("{auth:?}"), format!("{auth:#?}")] {
            assert!(rendered.contains("<redacted>"));
            assert!(rendered.contains("has_challenge: true"));
            assert!(rendered.contains("nonce_count: 1"));
            for marker in [
                "distinctive-user",
                "distinctive-password",
                "distinctive-realm",
                "distinctive-nonce",
                "distinctive-cnonce",
            ] {
                assert!(!rendered.contains(marker), "debug leaked {marker}");
            }
        }
    }
}
