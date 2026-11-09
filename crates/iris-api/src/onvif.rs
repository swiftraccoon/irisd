//! ONVIF lookups for the Add camera flow: `GET /api/onvif/devices` probes the
//! network through the daemon, `POST /api/onvif/streams` asks one camera's
//! Media service for the RTSP URLs of its profiles. Both exist only with the
//! daemon's [`CameraControl`](crate::CameraControl) (`[provisioning] enabled`,
//! HTTP 403 otherwise), need the root token, and relay to the daemon, which
//! owns the sockets, the port policy, and the lookup limit.

use std::ops::RangeInclusive;
use std::sync::Arc;
use std::time::Duration;

use axum::Json;
use axum::extract::rejection::JsonRejection;
use axum::extract::{Query, State};
use iris_core::config::is_valid_host;
use iris_core::id::CameraId;
use iris_events::{CameraEndpoint, DiscoveredCamera, ProfileStream};
use serde::{Deserialize, Serialize};

use crate::error::ApiError;
use crate::provision::{control, json_rejection};
use crate::state::ApiState;

/// How long a probe collects answers when the request names no `wait_ms`.
const DEFAULT_DISCOVERY_WAIT: Duration = Duration::from_secs(3);
/// The `wait_ms` values accepted.
const DISCOVERY_WAIT_MS: RangeInclusive<u64> = 500..=10_000;

/// `GET /api/onvif/devices` query.
#[derive(Debug, Deserialize)]
pub(crate) struct DevicesQuery {
    /// How long to collect answers, in milliseconds.
    wait_ms: Option<u64>,
}

/// One discovered camera, as the API lists it.
#[derive(Debug, Serialize)]
pub(crate) struct DiscoveredDeviceDto {
    endpoint: String,
    host: String,
    port: u16,
    https: bool,
    name: Option<String>,
    hardware: Option<String>,
    location: Option<String>,
}

impl From<DiscoveredCamera> for DiscoveredDeviceDto {
    fn from(camera: DiscoveredCamera) -> Self {
        Self {
            endpoint: camera.endpoint,
            host: camera.host,
            port: camera.port,
            https: camera.https,
            name: camera.name,
            hardware: camera.hardware,
            location: camera.location,
        }
    }
}

/// `POST /api/onvif/streams` body: one camera's HTTP interface and
/// credentials. `http_port` defaults to 443 with `https` and to 80 otherwise;
/// `tls_verify` defaults to true.
#[derive(Clone, Deserialize)]
pub(crate) struct StreamUrlsRequest {
    /// Host name or address, bare (IPv6 without brackets).
    host: String,
    /// HTTP port of the ONVIF device service.
    #[serde(default)]
    http_port: Option<u16>,
    /// Reach the camera over HTTPS.
    #[serde(default)]
    https: bool,
    /// Verify the camera's TLS certificate.
    #[serde(default = "default_true")]
    tls_verify: bool,
    /// HTTP user.
    username: String,
    /// HTTP password; never logged.
    password: String,
}

fn default_true() -> bool {
    true
}

impl std::fmt::Debug for StreamUrlsRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StreamUrlsRequest")
            .field("host", &self.host)
            .field("http_port", &self.http_port)
            .field("https", &self.https)
            .field("tls_verify", &self.tls_verify)
            .field("username", &self.username)
            .field("password", &"<redacted>")
            .finish()
    }
}

impl StreamUrlsRequest {
    /// The endpoint the daemon queries, or why the request is unusable.
    fn endpoint(&self) -> Result<CameraEndpoint, ApiError> {
        if !is_valid_host(&self.host) {
            return Err(ApiError::BadRequest(
                "host must be a bare host name or address".into(),
            ));
        }
        if self.username.trim().is_empty() {
            return Err(ApiError::BadRequest("username is required".into()));
        }
        let camera =
            CameraId::try_new("onvif-lookup").expect("\"onvif-lookup\" is a valid camera id");
        Ok(CameraEndpoint {
            camera,
            host: self.host.clone(),
            http_port: self.http_port.unwrap_or(if self.https { 443 } else { 80 }),
            https: self.https,
            tls_verify: self.tls_verify,
            username: self.username.clone(),
            password: self.password.clone(),
        })
    }
}

/// One profile's stream, as the API lists it.
#[derive(Debug, Serialize)]
pub(crate) struct ProfileStreamDto {
    token: String,
    name: String,
    url: String,
}

impl From<ProfileStream> for ProfileStreamDto {
    fn from(stream: ProfileStream) -> Self {
        Self {
            token: stream.token,
            name: stream.name,
            url: stream.url.to_string(),
        }
    }
}

/// `GET /api/onvif/devices?wait_ms=`: the cameras a WS-Discovery probe finds
/// within `wait_ms` (default 3000, between 500 and 10000).
pub(crate) async fn devices(
    State(state): State<Arc<ApiState>>,
    Query(query): Query<DevicesQuery>,
) -> Result<Json<Vec<DiscoveredDeviceDto>>, ApiError> {
    let control = control(&state)?;
    let wait = match query.wait_ms {
        None => DEFAULT_DISCOVERY_WAIT,
        Some(ms) if DISCOVERY_WAIT_MS.contains(&ms) => Duration::from_millis(ms),
        Some(_) => {
            return Err(ApiError::BadRequest(format!(
                "wait_ms must be between {} and {}",
                DISCOVERY_WAIT_MS.start(),
                DISCOVERY_WAIT_MS.end()
            )));
        }
    };
    let cameras = control.discover(wait).await?;
    Ok(Json(
        cameras.into_iter().map(DiscoveredDeviceDto::from).collect(),
    ))
}

/// `POST /api/onvif/streams`: the RTSP URLs of one camera's media profiles,
/// on the host named in the request.
pub(crate) async fn streams(
    State(state): State<Arc<ApiState>>,
    request: Result<Json<StreamUrlsRequest>, JsonRejection>,
) -> Result<Json<Vec<ProfileStreamDto>>, ApiError> {
    let control = control(&state)?;
    let Json(request) = request.map_err(json_rejection)?;
    let endpoint = request.endpoint()?;
    let streams = control.stream_urls(endpoint).await?;
    Ok(Json(
        streams.into_iter().map(ProfileStreamDto::from).collect(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(host: &str, https: bool, http_port: Option<u16>) -> StreamUrlsRequest {
        StreamUrlsRequest {
            host: host.to_owned(),
            http_port,
            https,
            tls_verify: true,
            username: "user".to_owned(),
            password: "secret".to_owned(),
        }
    }

    #[test]
    fn the_endpoint_takes_the_scheme_default_port_and_requires_a_user() {
        let plain = request("192.0.2.7", false, None).endpoint().unwrap();
        assert_eq!((plain.http_port, plain.https), (80, false));
        let secure = request("192.0.2.7", true, None).endpoint().unwrap();
        assert_eq!((secure.http_port, secure.https), (443, true));
        let explicit = request("192.0.2.7", false, Some(8080)).endpoint().unwrap();
        assert_eq!(explicit.http_port, 8080);
        assert_eq!(explicit.camera.as_str(), "onvif-lookup");

        let mut nameless = request("192.0.2.7", false, None);
        nameless.username = " ".to_owned();
        assert!(matches!(
            nameless.endpoint(),
            Err(ApiError::BadRequest(ref message)) if message.contains("username")
        ));
        assert!(matches!(
            request("a/b", false, None).endpoint(),
            Err(ApiError::BadRequest(_))
        ));
    }

    #[test]
    fn debug_output_redacts_the_password() {
        let shown = format!("{:?}", request("192.0.2.7", false, None));
        assert!(
            shown.contains("<redacted>") && !shown.contains("secret"),
            "{shown}"
        );
    }
}
