//! Camera provisioning: `POST /api/cameras` adds a camera, `DELETE
//! /api/cameras/{camera}` removes one the API added. Both exist only while the
//! daemon offers a [`CameraControl`] (`[provisioning] enabled`), answer HTTP 403
//! otherwise, and are never authorized by a media capability. The handler
//! validates the camera like a configured one and hands it to the daemon,
//! which owns the ports, the limit, the store, and the running tasks. The
//! same control carries the ONVIF lookups of [`crate::onvif`].

use std::sync::Arc;
use std::time::Duration;

use axum::Json;
use axum::extract::rejection::JsonRejection;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use iris_core::config::CameraConfig;
use iris_core::id::CameraId;
use iris_events::{CameraEndpoint, DiscoveredCamera, ProfileStream};
use thiserror::Error;
use tokio::sync::{mpsc, oneshot};

use crate::cameras::{CameraDto, camera_dto};
use crate::error::ApiError;
use crate::state::{ActiveCamera, ApiState};

/// Longest wait for the daemon to act on one request; adding a camera opens
/// its streams, removing one tears them down.
const CONTROL_TIMEOUT: Duration = Duration::from_secs(20);
/// Time allowed beyond the probe's own wait for the daemon to answer a
/// discovery request.
const DISCOVERY_GRACE: Duration = Duration::from_secs(5);
/// Longest wait for a stream lookup: the lookup's own 60 s deadline in
/// `iris_events::stream_urls`, plus the daemon's handling.
const STREAM_URLS_TIMEOUT: Duration = Duration::from_secs(65);
/// Largest request body on the protected routes; a camera description is a
/// few hundred bytes.
pub(crate) const MAX_PROTECTED_BODY_BYTES: usize = 64 * 1024;

/// A request the API hands the daemon's control loop.
#[derive(Debug)]
pub enum ControlRequest {
    /// Add `camera`: persist it, register it, and start its streams and event
    /// feeds. The reply carries the camera as the API then lists it.
    Add {
        /// The validated camera.
        camera: Box<CameraConfig>,
        /// Where the daemon answers.
        reply: oneshot::Sender<Result<ActiveCamera, ProvisionError>>,
    },
    /// Stop and forget the camera the API added under `id`.
    Remove {
        /// The camera.
        id: CameraId,
        /// Where the daemon answers.
        reply: oneshot::Sender<Result<(), ProvisionError>>,
    },
    /// Probe the network with WS-Discovery for `wait` and list the cameras
    /// that answered.
    Discover {
        /// How long the daemon collects answers.
        wait: Duration,
        /// Where the daemon answers.
        reply: oneshot::Sender<Result<Vec<DiscoveredCamera>, ProvisionError>>,
    },
    /// Ask one camera's ONVIF Media service for the RTSP URLs of its profiles.
    StreamUrls {
        /// The camera's HTTP interface and credentials.
        endpoint: Box<CameraEndpoint>,
        /// Where the daemon answers.
        reply: oneshot::Sender<Result<Vec<ProfileStream>, ProvisionError>>,
    },
}

/// Why the daemon refused a provisioning request.
#[derive(Clone, Debug, PartialEq, Eq, Error)]
#[non_exhaustive]
pub enum ProvisionError {
    /// A camera with this id is running already.
    #[error("camera {0} already exists")]
    Duplicate(CameraId),
    /// The camera comes from the configuration file, which the API does not edit.
    #[error("camera {0} is in the configuration file; remove it there and restart")]
    Configured(CameraId),
    /// No camera with this id was added through the API.
    #[error("no provisioned camera {0}")]
    Unknown(CameraId),
    /// `provisioning.max_cameras` cameras were added already.
    #[error("the limit of {0} provisioned cameras is reached")]
    Limit(usize),
    /// The camera uses a TCP port outside `provisioning.connect_ports`.
    #[error("camera uses TCP port {0}, which provisioning.connect_ports does not allow")]
    Port(u16),
    /// The camera failed validation.
    #[error("{0}")]
    Invalid(String),
    /// The camera store in the state directory could not be written. The
    /// text names paths under the state directory; the API logs it and
    /// answers a fixed body.
    #[error("the camera store could not be written: {0}")]
    Store(String),
    /// The camera's tasks could not be started or stopped. The text is logged
    /// and never returned to the client.
    #[error("the camera could not be started or stopped: {0}")]
    Lifecycle(String),
    /// The WS-Discovery probe could not be sent.
    #[error("discovery failed: {0}")]
    Discovery(String),
    /// The camera's ONVIF service gave no usable answer; the text names the
    /// kind of failure, never camera-supplied text.
    #[error("the camera could not be queried: {0}")]
    Camera(String),
    /// The daemon runs its limit of ONVIF lookups already.
    #[error("another ONVIF lookup is running; retry shortly")]
    Busy,
}

impl From<ProvisionError> for ApiError {
    fn from(error: ProvisionError) -> Self {
        match error {
            ProvisionError::Duplicate(_) | ProvisionError::Configured(_) => {
                Self::Conflict(error.to_string())
            }
            ProvisionError::Unknown(_) => Self::NotFound,
            ProvisionError::Limit(_) | ProvisionError::Port(_) | ProvisionError::Invalid(_) => {
                Self::BadRequest(error.to_string())
            }
            ProvisionError::Store(detail) => {
                tracing::warn!(%detail, "camera store write failed");
                Self::Unavailable("the camera store could not be written".into())
            }
            ProvisionError::Lifecycle(detail) => {
                tracing::warn!(%detail, "camera tasks could not be started or stopped");
                Self::Unavailable("the camera could not be started or stopped".into())
            }
            ProvisionError::Discovery(_) => Self::Unavailable(error.to_string()),
            ProvisionError::Camera(_) => Self::Upstream(error.to_string()),
            ProvisionError::Busy => Self::Busy(error.to_string()),
        }
    }
}

/// The API's handle on the daemon's control loop.
///
/// ```
/// use iris_api::{CameraControl, ControlRequest};
///
/// let (sender, _receiver) = tokio::sync::mpsc::channel::<ControlRequest>(4);
/// let control = CameraControl::new(sender);
/// assert_eq!(format!("{control:?}"), "CameraControl");
/// ```
#[derive(Clone)]
pub struct CameraControl {
    sender: mpsc::Sender<ControlRequest>,
}

impl std::fmt::Debug for CameraControl {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("CameraControl")
    }
}

impl CameraControl {
    /// Wraps the sending side of the daemon's control channel.
    #[must_use]
    pub fn new(sender: mpsc::Sender<ControlRequest>) -> Self {
        Self { sender }
    }

    async fn add(&self, camera: CameraConfig) -> Result<ActiveCamera, ApiError> {
        let (reply, answer) = oneshot::channel();
        self.send(ControlRequest::Add {
            camera: Box::new(camera),
            reply,
        })
        .await?;
        Ok(Self::answer(answer).await??)
    }

    async fn remove(&self, id: CameraId) -> Result<(), ApiError> {
        let (reply, answer) = oneshot::channel();
        self.send(ControlRequest::Remove { id, reply }).await?;
        Ok(Self::answer(answer).await??)
    }

    pub(crate) async fn discover(&self, wait: Duration) -> Result<Vec<DiscoveredCamera>, ApiError> {
        let (reply, answer) = oneshot::channel();
        self.send(ControlRequest::Discover { wait, reply }).await?;
        Ok(Self::answer_within(wait + DISCOVERY_GRACE, answer).await??)
    }

    pub(crate) async fn stream_urls(
        &self,
        endpoint: CameraEndpoint,
    ) -> Result<Vec<ProfileStream>, ApiError> {
        let (reply, answer) = oneshot::channel();
        self.send(ControlRequest::StreamUrls {
            endpoint: Box::new(endpoint),
            reply,
        })
        .await?;
        Ok(Self::answer_within(STREAM_URLS_TIMEOUT, answer).await??)
    }

    async fn send(&self, request: ControlRequest) -> Result<(), ApiError> {
        self.sender
            .send(request)
            .await
            .map_err(|_| ApiError::Unavailable("the daemon's camera control is gone".into()))
    }

    async fn answer<T>(
        answer: oneshot::Receiver<Result<T, ProvisionError>>,
    ) -> Result<Result<T, ProvisionError>, ApiError> {
        Self::answer_within(CONTROL_TIMEOUT, answer).await
    }

    async fn answer_within<T>(
        deadline: Duration,
        answer: oneshot::Receiver<Result<T, ProvisionError>>,
    ) -> Result<Result<T, ProvisionError>, ApiError> {
        match tokio::time::timeout(deadline, answer).await {
            Ok(Ok(result)) => Ok(result),
            Ok(Err(_)) => Err(ApiError::Unavailable(
                "the daemon dropped the camera request".into(),
            )),
            Err(_) => Err(ApiError::Unavailable(
                "the daemon did not act on the camera request in time".into(),
            )),
        }
    }
}

pub(crate) fn control(state: &ApiState) -> Result<&CameraControl, ApiError> {
    state.control.as_ref().ok_or_else(|| {
        ApiError::Forbidden("camera provisioning is disabled in the configuration".into())
    })
}

/// A JSON body the extractor refused (missing or mistyped field, wrong
/// `Content-Type`) as the documented HTTP 400 with a JSON body; the body
/// limit's 413 keeps its status.
pub(crate) fn json_rejection(rejection: JsonRejection) -> ApiError {
    match rejection {
        JsonRejection::BytesRejection(rejection) => ApiError::Rejected {
            status: rejection.status(),
            message: rejection.body_text(),
        },
        other => ApiError::BadRequest(other.body_text()),
    }
}

/// `POST /api/cameras`: adds a camera described like a `[[camera]]` table and
/// answers 201 with the camera as `/api/cameras` lists it.
pub(crate) async fn add(
    State(state): State<Arc<ApiState>>,
    camera: Result<Json<CameraConfig>, JsonRejection>,
) -> Result<(StatusCode, Json<CameraDto>), ApiError> {
    let control = control(&state)?;
    let Json(camera) = camera.map_err(json_rejection)?;
    camera
        .validate()
        .map_err(|error| ApiError::BadRequest(error.to_string()))?;
    let added = control.add(camera).await?;
    Ok((StatusCode::CREATED, Json(camera_dto(&added, &state))))
}

/// `DELETE /api/cameras/{camera}`: stops and forgets a camera the API added
/// and answers 204. Recordings and catalog rows stay.
pub(crate) async fn remove(
    State(state): State<Arc<ApiState>>,
    Path(camera): Path<String>,
) -> Result<StatusCode, ApiError> {
    let control = control(&state)?;
    let id = CameraId::try_new(&camera).map_err(|error| ApiError::BadRequest(error.to_string()))?;
    control.remove(id).await?;
    Ok(StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn daemon_refusals_map_to_the_matching_status() {
        let id = CameraId::try_new("cam1").unwrap();
        for (error, status) in [
            (ProvisionError::Duplicate(id.clone()), StatusCode::CONFLICT),
            (ProvisionError::Configured(id.clone()), StatusCode::CONFLICT),
            (ProvisionError::Unknown(id), StatusCode::NOT_FOUND),
            (ProvisionError::Limit(16), StatusCode::BAD_REQUEST),
            (ProvisionError::Port(8554), StatusCode::BAD_REQUEST),
            (
                ProvisionError::Invalid("bad".into()),
                StatusCode::BAD_REQUEST,
            ),
            (
                ProvisionError::Store("disk".into()),
                StatusCode::SERVICE_UNAVAILABLE,
            ),
            (
                ProvisionError::Lifecycle("task".into()),
                StatusCode::SERVICE_UNAVAILABLE,
            ),
            (
                ProvisionError::Discovery("socket".into()),
                StatusCode::SERVICE_UNAVAILABLE,
            ),
            (
                ProvisionError::Camera("ONVIF call failed".into()),
                StatusCode::BAD_GATEWAY,
            ),
            (ProvisionError::Busy, StatusCode::SERVICE_UNAVAILABLE),
        ] {
            let response = axum::response::IntoResponse::into_response(ApiError::from(error));
            assert_eq!(response.status(), status);
        }
    }

    #[test]
    fn store_and_lifecycle_failures_answer_a_fixed_body() {
        for error in [
            ProvisionError::Store("creating /srv/state/cameras.toml.tmp: denied".into()),
            ProvisionError::Lifecycle("registering the camera: marker".into()),
        ] {
            let mapped = ApiError::from(error.clone());
            let text = mapped.to_string();
            assert!(matches!(mapped, ApiError::Unavailable(_)), "{text}");
            assert!(
                !text.contains("/srv") && !text.contains("marker"),
                "{text} leaks the detail of {error}"
            );
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_silent_daemon_times_out_as_unavailable() {
        let (sender, mut receiver) = mpsc::channel(1);
        let control = CameraControl::new(sender);
        let held = tokio::spawn(async move {
            let request = receiver.recv().await.expect("the request arrives");
            tokio::time::sleep(CONTROL_TIMEOUT * 2).await;
            drop(request);
        });
        let error = control
            .remove(CameraId::try_new("cam1").unwrap())
            .await
            .unwrap_err();
        assert!(
            matches!(&error, ApiError::Unavailable(text) if text.contains("in time")),
            "{error}"
        );
        held.abort();
    }

    #[tokio::test]
    async fn a_dropped_daemon_or_reply_is_unavailable() {
        let (sender, receiver) = mpsc::channel(1);
        let control = CameraControl::new(sender);
        drop(receiver);
        let error = control
            .remove(CameraId::try_new("cam1").unwrap())
            .await
            .unwrap_err();
        assert!(matches!(error, ApiError::Unavailable(_)), "{error}");

        let (sender, mut receiver) = mpsc::channel(1);
        let control = CameraControl::new(sender);
        let dropped = tokio::spawn(async move {
            let request = receiver.recv().await.expect("the request arrives");
            drop(request);
        });
        let error = control
            .remove(CameraId::try_new("cam1").unwrap())
            .await
            .unwrap_err();
        assert!(matches!(error, ApiError::Unavailable(_)), "{error}");
        dropped.await.unwrap();
    }
}
