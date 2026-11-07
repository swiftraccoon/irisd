//! Camera addressing and the sink events are delivered to.

use std::fmt;

use iris_core::event::Event;
use iris_core::id::CameraId;

/// How to reach one camera's HTTP interface.
#[derive(Clone)]
pub struct CameraEndpoint {
    /// The camera the events belong to.
    pub camera: CameraId,
    /// Host name or bare IP address; IPv6 brackets are added by [`Self::base_url`].
    pub host: String,
    /// HTTP port for CGI and ONVIF.
    pub http_port: u16,
    /// Use HTTPS instead of HTTP.
    pub https: bool,
    /// Verify the camera's TLS certificate (false for self-signed cameras).
    pub tls_verify: bool,
    /// HTTP user.
    pub username: String,
    /// HTTP password; never logged.
    pub password: String,
}

impl CameraEndpoint {
    /// Returns the base URL for CGI and ONVIF calls, such as `https://host:443`.
    #[must_use]
    pub fn base_url(&self) -> String {
        let scheme = if self.https { "https" } else { "http" };
        if self.host.contains(':') {
            format!("{scheme}://[{}]:{}", self.host, self.http_port)
        } else {
            format!("{scheme}://{}:{}", self.host, self.http_port)
        }
    }
}

impl fmt::Debug for CameraEndpoint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CameraEndpoint")
            .field("camera", &self.camera)
            .field("host", &self.host)
            .field("http_port", &self.http_port)
            .field("https", &self.https)
            .field("tls_verify", &self.tls_verify)
            .field("username", &self.username)
            .field("password", &"<redacted>")
            .finish()
    }
}

/// Receives normalised events.
pub trait EventSink: Send + Sync {
    /// Delivers one event. Implementations must not block for long; the
    /// sources call this from their receive loops.
    fn publish(&self, event: Event);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base_urls_bracket_ipv6_and_preserve_other_hosts() {
        for host in ["2001:db8::1", "192.0.2.7", "camera.example"] {
            for (https, scheme, port) in [(false, "http", 8080), (true, "https", 8443)] {
                let endpoint = CameraEndpoint {
                    camera: CameraId::try_new("a").unwrap(),
                    host: host.into(),
                    http_port: port,
                    https,
                    tls_verify: true,
                    username: String::new(),
                    password: String::new(),
                };
                let authority = if host.contains(':') {
                    format!("[{host}]")
                } else {
                    host.into()
                };
                assert_eq!(
                    endpoint.base_url(),
                    format!("{scheme}://{authority}:{port}")
                );
                let request = reqwest::Client::new()
                    .get(endpoint.base_url())
                    .build()
                    .unwrap();
                assert_eq!(request.url().scheme(), scheme);
                assert_eq!(request.url().port(), Some(port));
            }
        }
    }
}
