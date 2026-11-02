//! Camera addressing and the sink events are delivered to.

use std::fmt;

use iris_core::event::Event;
use iris_core::id::CameraId;

/// How to reach one camera's HTTP interface.
#[derive(Clone)]
pub struct CameraEndpoint {
    /// The camera the events belong to.
    pub camera: CameraId,
    /// Host name or address.
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
        format!("{scheme}://{}:{}", self.host, self.http_port)
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
