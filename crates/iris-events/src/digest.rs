//! HTTP Digest authentication (RFC 7616, MD5 and MD5-sess, `qop=auth`).

use std::fmt;
use std::fmt::Write as _;

use iris_core::nonce::client_nonce;
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
    /// The algorithm token as the server wrote it: `MD5` when absent, else
    /// `MD5` or `MD5-sess` in any letter case ([`parse_challenge`] accepts no
    /// other).
    pub algorithm: String,
    /// Whether the previous nonce merely expired.
    pub stale: bool,
}

/// Parses the value of a `WWW-Authenticate` header. Returns `None` unless it
/// is a `Digest` challenge (the scheme compared without regard to case) with a
/// realm and a nonce whose algorithm is absent, `MD5`, or `MD5-sess`, so a
/// caller trying several headers moves on to the first challenge irisd can
/// answer (RFC 7616 §3.7).
///
/// ```
/// use iris_events::digest::parse_challenge;
///
/// let challenge = parse_challenge("Digest realm=\"camera\", nonce=\"n1\", qop=\"auth\"")
///     .expect("an MD5 challenge");
/// assert_eq!(challenge.algorithm, "MD5");
/// assert!(parse_challenge("Digest realm=\"camera\", nonce=\"n1\", algorithm=SHA-256").is_none());
/// ```
#[must_use]
pub fn parse_challenge(value: &str) -> Option<Challenge> {
    let value = value.trim();
    let scheme_len = value.find(char::is_whitespace).unwrap_or(value.len());
    let (scheme, rest) = value.split_at(scheme_len);
    if !scheme.eq_ignore_ascii_case("Digest") {
        return None;
    }
    let mut realm = None;
    let mut nonce = None;
    let mut qop = None;
    let mut opaque = None;
    let mut algorithm = String::from("MD5");
    let mut stale = false;
    for (key, val) in split_params(rest.trim_start()) {
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
    if !(algorithm.eq_ignore_ascii_case("MD5") || algorithm.eq_ignore_ascii_case("MD5-sess")) {
        return None;
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

/// `value` as an RFC 7616 quoted-string: `\` and `"` escaped, in quotes.
fn quoted(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for c in value.chars() {
        if matches!(c, '\\' | '"') {
            out.push('\\');
        }
        out.push(c);
    }
    out.push('"');
    out
}

/// Computes the `response` value.
///
/// `ha1` is `MD5(user:realm:password)` (already session-derived for
/// `MD5-sess`), `nonce` the server nonce, `qop` the `(qop, nonce count, client
/// nonce)` of a `qop=auth` exchange or `None` for the RFC 2069 form, and `ha2`
/// is `MD5(method:uri)`.
fn compute_response(ha1: &str, nonce: &str, qop: Option<(&str, u32, &str)>, ha2: &str) -> String {
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

    /// Adopts a challenge from a 401 response.
    pub fn set_challenge(&mut self, challenge: Challenge) {
        self.challenge = Some(challenge);
        self.nonce_count = 0;
    }

    /// Builds an `Authorization` header value for `method` and `uri` with a
    /// fresh random client nonce, or `None` without a challenge.
    #[must_use = "the nonce count has advanced; send the returned header"]
    pub fn authorization(&mut self, method: &str, uri: &str) -> Option<String> {
        let cnonce = client_nonce();
        self.authorization_with(method, uri, &cnonce)
    }

    /// As [`DigestAuth::authorization`] with an explicit client nonce (testable).
    #[must_use = "the nonce count has advanced; send the returned header"]
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
            "Digest username={}, realm={}, nonce={}, uri={}, response=\"{response}\", algorithm={}",
            quoted(&self.username),
            quoted(&challenge.realm),
            quoted(&challenge.nonce),
            quoted(uri),
            challenge.algorithm
        );
        if let Some(q) = &challenge.qop {
            let _ = write!(header, ", qop={q}, nc={nc:08x}, cnonce={}", quoted(cnonce));
        }
        if let Some(opaque) = &challenge.opaque {
            let _ = write!(header, ", opaque={}", quoted(opaque));
        }
        Some(header)
    }
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
    }

    #[test]
    fn only_digest_challenges_are_parsed_whatever_the_scheme_case() {
        assert!(parse_challenge("Basic realm=\"x\"").is_none());
        assert!(parse_challenge("Digestive realm=\"x\", nonce=\"n\"").is_none());
        assert!(parse_challenge("digest realm=\"x\", nonce=\"n\"").is_some());
        assert!(parse_challenge("DIGEST realm=\"x\", nonce=\"n\"").is_some());
    }

    /// RFC 7616 §3.9.1 lists a SHA-256 challenge before the MD5 one; the
    /// first supported challenge is answered with the published response.
    #[test]
    fn an_unsupported_algorithm_is_skipped_for_the_next_challenge() {
        let sha256 = "Digest realm=\"http-auth@example.org\", qop=\"auth, auth-int\", algorithm=SHA-256, nonce=\"7ypf/xlj9XXwfDPEoM4URrv/xwf94BcCAzFZH4GiTo0v\", opaque=\"FQhe/qaU925kfnzjCev0ciny7QMkPqMAFRtzCUYo5tdS\"";
        let md5 = "Digest realm=\"http-auth@example.org\", qop=\"auth, auth-int\", algorithm=MD5, nonce=\"7ypf/xlj9XXwfDPEoM4URrv/xwf94BcCAzFZH4GiTo0v\", opaque=\"FQhe/qaU925kfnzjCev0ciny7QMkPqMAFRtzCUYo5tdS\"";
        assert!(parse_challenge(sha256).is_none());
        let challenge = [sha256, md5]
            .into_iter()
            .find_map(parse_challenge)
            .expect("the MD5 challenge");
        let mut auth = DigestAuth::new("Mufasa", "Circle of Life");
        auth.set_challenge(challenge);
        let header = auth
            .authorization_with(
                "GET",
                "/dir/index.html",
                "f2/wE4q74E6zIJEtWaHKaf5wv/H5QzzpXusqGemxURZJ",
            )
            .unwrap();
        assert!(
            header.contains("response=\"8ca523f5e9506fed4657c9700eebdbec\"")
                && header.contains("algorithm=MD5"),
            "{header}"
        );
        for algorithm in ["SHA-512-256", "md5-sess", "MD5-SESS", "token68"] {
            let accepted = parse_challenge(&format!(
                "Digest realm=\"x\", nonce=\"n\", algorithm={algorithm}"
            ))
            .is_some();
            assert_eq!(
                accepted,
                algorithm.eq_ignore_ascii_case("MD5-sess"),
                "{algorithm}"
            );
        }
    }

    #[test]
    fn quoted_values_are_re_escaped_in_the_header() {
        let mut auth = DigestAuth::new("us\"er", "password");
        auth.set_challenge(
            parse_challenge(
                "Digest realm=\"re\\\"alm\\\\\", nonce=\"no\\\"nce\", opaque=\"o\\\"p\"",
            )
            .unwrap(),
        );
        let header = auth.authorization_with("GET", "/", "cn\"once").unwrap();
        for field in [
            "username=\"us\\\"er\"",
            "realm=\"re\\\"alm\\\\\"",
            "nonce=\"no\\\"nce\"",
            "opaque=\"o\\\"p\"",
        ] {
            assert!(header.contains(field), "{field} in {header}");
        }
        assert!(!header.contains("cnonce="), "no qop, no cnonce: {header}");
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
        assert!(header.contains("nc=00000001"), "{header}");
        assert!(header.contains("qop=auth"), "{header}");
    }

    #[test]
    fn the_nonce_count_advances_per_request() {
        let mut auth = DigestAuth::new("Mufasa", "Circle Of Life");
        auth.set_challenge(parse_challenge("Digest realm=\"testrealm@host.com\", qop=\"auth,auth-int\", nonce=\"dcd98b7102dd2f0e8b11d0f600bfb0c093\", opaque=\"5ccc069c403ebaf9f0171e9517f40e41\"").unwrap());
        let first = auth
            .authorization_with("GET", "/dir/index.html", "0a4f113b")
            .unwrap();
        let second = auth
            .authorization_with("GET", "/dir/index.html", "0a4f113b")
            .unwrap();
        assert!(first.contains("nc=00000001"), "{first}");
        assert!(second.contains("nc=00000002"), "{second}");
    }

    #[test]
    fn client_nonces_are_random_hexadecimal_tokens() {
        let mut auth = DigestAuth::new("user", "password");
        auth.set_challenge(
            parse_challenge("Digest realm=\"r\", nonce=\"n\", qop=\"auth\"").unwrap(),
        );
        let cnonce = |header: String| {
            header
                .split("cnonce=\"")
                .nth(1)
                .and_then(|rest| rest.split('"').next())
                .expect("a cnonce")
                .to_owned()
        };
        let first = cnonce(auth.authorization("GET", "/").unwrap());
        let second = cnonce(auth.authorization("GET", "/").unwrap());
        assert_eq!(first.len(), 16, "{first}");
        assert!(
            first.bytes().all(|byte| byte.is_ascii_hexdigit()),
            "{first}"
        );
        assert_ne!(first, second);
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
        assert!(
            header.contains("response=\"670fd8c2df070c60b045671b8b24ff02\""),
            "{header}"
        );
        assert!(!header.contains("qop="), "{header}");
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
