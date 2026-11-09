//! `POST /api/cameras` and `DELETE /api/cameras/{camera}` against a stand-in
//! for the daemon's control loop: the routes validate, authenticate, and
//! relay; the stand-in registers and forgets cameras as the daemon would.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use std::time::Instant;

use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode, header};
use iris_api::{
    ActiveCamera, ApiState, ApiStats, CameraControl, CapabilitySecret, ControlRequest,
    LiveRegistry, ProvisionError, SnapshotSource, router,
};
use iris_catalog::CatalogHandle;
use iris_core::id::CameraId;
use iris_core::time::WallUs;
use iris_events::{DiscoveredCamera, ProfileStream};
use iris_store::RootExpectation;
use serde_json::{Value, json};
use tokio::sync::{broadcast, mpsc};
use tokio_util::sync::CancellationToken;
use tower::ServiceExt as _;

const TOKEN: &str = "0123456789abcdef0123456789abcdef";

fn api_state(dir: &std::path::Path, control: Option<CameraControl>) -> Arc<ApiState> {
    let catalog = CatalogHandle::open(&dir.join("catalog.db")).unwrap();
    let root =
        iris_store::admit_recording_root(&dir.join("video"), RootExpectation::Fresh).unwrap();
    let (events, _) = broadcast::channel(16);
    let configured = CameraId::try_new("static").unwrap();
    Arc::new(ApiState {
        catalog,
        live: Arc::new(LiveRegistry::new()),
        events,
        tokens: vec![TOKEN.to_owned()],
        capability_secret: CapabilitySecret::new([3; 32]),
        recording_roots: vec![root],
        public_metrics: false,
        snapshot_sources: RwLock::new(HashMap::new()),
        active_cameras: RwLock::new(HashMap::from([(
            configured.clone(),
            ActiveCamera {
                id: configured,
                name: "Configured".into(),
                vendor: None,
                provisioned: false,
            },
        )])),
        control,
        stats: Arc::new(ApiStats::default()),
        started: WallUs::now(),
        started_monotonic: Instant::now(),
        shutdown: CancellationToken::new(),
    })
}

/// Acts as the daemon: adds and removes cameras in the API's registries,
/// refusing the configured camera and repeated ids as the daemon does.
fn stand_in_daemon(state: Arc<ApiState>, mut requests: mpsc::Receiver<ControlRequest>) {
    tokio::spawn(async move {
        while let Some(request) = requests.recv().await {
            match request {
                ControlRequest::Add { camera, reply } => {
                    let mut cameras = state.active_cameras.write().unwrap();
                    let result = if cameras.contains_key(&camera.id) {
                        Err(ProvisionError::Duplicate(camera.id.clone()))
                    } else {
                        let active = ActiveCamera {
                            id: camera.id.clone(),
                            name: camera.name.clone(),
                            vendor: camera.vendor.clone(),
                            provisioned: true,
                        };
                        cameras.insert(camera.id.clone(), active.clone());
                        state.snapshot_sources.write().unwrap().insert(
                            camera.id.clone(),
                            SnapshotSource::new(
                                camera.host().unwrap(),
                                camera.http_port_or_default(),
                                camera.https,
                                camera.tls_verify,
                                camera.username.clone(),
                                camera.password.clone(),
                                iris_core::config::ResolvedSnapshot::DahuaCgi,
                            ),
                        );
                        Ok(active)
                    };
                    let _ = reply.send(result);
                }
                ControlRequest::Remove { id, reply } => {
                    let result = if id.as_str() == "static" {
                        Err(ProvisionError::Configured(id))
                    } else if state.active_cameras.write().unwrap().remove(&id).is_some() {
                        state.snapshot_sources.write().unwrap().remove(&id);
                        Ok(())
                    } else {
                        Err(ProvisionError::Unknown(id))
                    };
                    let _ = reply.send(result);
                }
                ControlRequest::Discover { wait, reply } => {
                    let found = vec![DiscoveredCamera {
                        endpoint: "urn:uuid:1".into(),
                        host: "192.0.2.20".into(),
                        port: 80,
                        https: false,
                        name: Some(format!("Garage after {} ms", wait.as_millis())),
                        hardware: Some("IPC".into()),
                        location: None,
                    }];
                    let _ = reply.send(Ok(found));
                }
                ControlRequest::StreamUrls { endpoint, reply } => {
                    let result = if endpoint.host == "192.0.2.99" {
                        Err(ProvisionError::Camera("ONVIF call failed".into()))
                    } else if endpoint.http_port == 8080 {
                        Err(ProvisionError::Port(8080))
                    } else {
                        Ok(vec![ProfileStream {
                            token: "profile-1".into(),
                            name: "Main".into(),
                            url: format!("rtsp://{}:554/live/1", endpoint.host)
                                .parse()
                                .unwrap(),
                        }])
                    };
                    let _ = reply.send(result);
                }
            }
        }
    });
}

fn camera_body(id: &str, url: &str) -> Value {
    json!({
        "id": id,
        "name": format!("Camera {id}"),
        "rtsp_url": url,
        "username": "user",
        "password": "secret",
        "record": ["main"],
    })
}

fn post(body: &Value, token: Option<&str>) -> Request<Body> {
    let mut request =
        Request::post("/api/cameras").header(header::CONTENT_TYPE, "application/json");
    if let Some(token) = token {
        request = request.header(header::AUTHORIZATION, format!("Bearer {token}"));
    }
    request.body(Body::from(body.to_string())).unwrap()
}

fn delete(camera: &str) -> Request<Body> {
    Request::delete(format!("/api/cameras/{camera}"))
        .header(header::AUTHORIZATION, format!("Bearer {TOKEN}"))
        .body(Body::empty())
        .unwrap()
}

async fn listed(state: &Arc<ApiState>) -> Vec<String> {
    let response = router(Arc::clone(state))
        .oneshot(
            Request::get("/api/cameras")
                .header(header::AUTHORIZATION, format!("Bearer {TOKEN}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body: Vec<Value> =
        serde_json::from_slice(&to_bytes(response.into_body(), 1 << 20).await.unwrap()).unwrap();
    body.iter()
        .map(|camera| camera["id"].as_str().unwrap().to_owned())
        .collect()
}

#[tokio::test]
async fn cameras_are_added_listed_and_removed_through_the_daemon_control() {
    let dir = tempfile::tempdir().unwrap();
    let (sender, receiver) = mpsc::channel(4);
    let state = api_state(dir.path(), Some(CameraControl::new(sender)));
    stand_in_daemon(Arc::clone(&state), receiver);

    let response = router(Arc::clone(&state))
        .oneshot(post(
            &camera_body("added1", "rtsp://192.0.2.10/stream"),
            Some(TOKEN),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);
    let body: Value =
        serde_json::from_slice(&to_bytes(response.into_body(), 1 << 20).await.unwrap()).unwrap();
    assert_eq!(body["id"], "added1");
    assert_eq!(body["name"], "Camera added1");
    assert_eq!(body["has_snapshot"], true);
    assert_eq!(body["provisioned"], true);
    assert!(
        body.get("password").is_none(),
        "the response never carries credentials"
    );
    assert_eq!(listed(&state).await, ["added1", "static"]);

    let duplicate = router(Arc::clone(&state))
        .oneshot(post(
            &camera_body("added1", "rtsp://192.0.2.10/stream"),
            Some(TOKEN),
        ))
        .await
        .unwrap();
    assert_eq!(duplicate.status(), StatusCode::CONFLICT);

    let configured = router(Arc::clone(&state))
        .oneshot(delete("static"))
        .await
        .unwrap();
    assert_eq!(configured.status(), StatusCode::CONFLICT);
    let unknown = router(Arc::clone(&state))
        .oneshot(delete("nobody"))
        .await
        .unwrap();
    assert_eq!(unknown.status(), StatusCode::NOT_FOUND);

    let removed = router(Arc::clone(&state))
        .oneshot(delete("added1"))
        .await
        .unwrap();
    assert_eq!(removed.status(), StatusCode::NO_CONTENT);
    assert_eq!(listed(&state).await, ["static"]);
}

#[tokio::test]
async fn invalid_cameras_and_bodies_are_refused_before_the_daemon_sees_them() {
    let dir = tempfile::tempdir().unwrap();
    let (sender, mut receiver) = mpsc::channel(4);
    let state = api_state(dir.path(), Some(CameraControl::new(sender)));

    let not_rtsp = router(Arc::clone(&state))
        .oneshot(post(
            &camera_body("bad", "http://192.0.2.10/stream"),
            Some(TOKEN),
        ))
        .await
        .unwrap();
    assert_eq!(not_rtsp.status(), StatusCode::BAD_REQUEST);

    let missing_password =
        json!({"id": "bad", "name": "n", "rtsp_url": "rtsp://192.0.2.10/s", "username": "u"});
    let missing = router(Arc::clone(&state))
        .oneshot(post(&missing_password, Some(TOKEN)))
        .await
        .unwrap();
    assert_eq!(missing.status(), StatusCode::BAD_REQUEST);
    let missing: Value =
        serde_json::from_slice(&to_bytes(missing.into_body(), 4096).await.unwrap())
            .expect("a JSON error body");
    assert!(
        missing["error"]
            .as_str()
            .is_some_and(|text| text.contains("password")),
        "{missing}"
    );

    let mut oversized = camera_body("big", "rtsp://192.0.2.10/stream");
    oversized["name"] = Value::String("x".repeat(65 * 1024));
    let too_large = router(Arc::clone(&state))
        .oneshot(post(&oversized, Some(TOKEN)))
        .await
        .unwrap();
    assert_eq!(too_large.status(), StatusCode::PAYLOAD_TOO_LARGE);

    assert!(
        receiver.try_recv().is_err(),
        "no refused request reached the daemon"
    );
}

#[tokio::test]
async fn provisioning_requires_the_root_token_and_the_control() {
    let dir = tempfile::tempdir().unwrap();
    let (sender, _receiver) = mpsc::channel(4);
    let state = api_state(dir.path(), Some(CameraControl::new(sender)));
    let body = camera_body("added1", "rtsp://192.0.2.10/stream");

    let anonymous = router(Arc::clone(&state))
        .oneshot(post(&body, None))
        .await
        .unwrap();
    assert_eq!(anonymous.status(), StatusCode::UNAUTHORIZED);
    let wrong = router(Arc::clone(&state))
        .oneshot(post(&body, Some("not-the-token-not-the-token-1234")))
        .await
        .unwrap();
    assert_eq!(wrong.status(), StatusCode::UNAUTHORIZED);

    let disabled = api_state(&dir.path().join("disabled"), None);
    let forbidden = router(Arc::clone(&disabled))
        .oneshot(post(&body, Some(TOKEN)))
        .await
        .unwrap();
    assert_eq!(forbidden.status(), StatusCode::FORBIDDEN);
    let forbidden = router(disabled).oneshot(delete("static")).await.unwrap();
    assert_eq!(forbidden.status(), StatusCode::FORBIDDEN);
}

fn get_devices(path: &str, token: Option<&str>) -> Request<Body> {
    let mut request = Request::get(path);
    if let Some(token) = token {
        request = request.header(header::AUTHORIZATION, format!("Bearer {token}"));
    }
    request.body(Body::empty()).unwrap()
}

fn post_streams(body: &Value, token: Option<&str>) -> Request<Body> {
    let mut request =
        Request::post("/api/onvif/streams").header(header::CONTENT_TYPE, "application/json");
    if let Some(token) = token {
        request = request.header(header::AUTHORIZATION, format!("Bearer {token}"));
    }
    request.body(Body::from(body.to_string())).unwrap()
}

async fn json_body(response: axum::response::Response) -> Value {
    serde_json::from_slice(&to_bytes(response.into_body(), 1 << 20).await.unwrap()).unwrap()
}

#[tokio::test]
async fn onvif_devices_are_listed_with_a_bounded_wait() {
    let dir = tempfile::tempdir().unwrap();
    let (sender, receiver) = mpsc::channel(4);
    let state = api_state(dir.path(), Some(CameraControl::new(sender)));
    stand_in_daemon(Arc::clone(&state), receiver);

    let response = router(Arc::clone(&state))
        .oneshot(get_devices("/api/onvif/devices", Some(TOKEN)))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        json_body(response).await,
        json!([{
            "endpoint": "urn:uuid:1",
            "host": "192.0.2.20",
            "port": 80,
            "https": false,
            "name": "Garage after 3000 ms",
            "hardware": "IPC",
            "location": null,
        }])
    );

    let custom = router(Arc::clone(&state))
        .oneshot(get_devices("/api/onvif/devices?wait_ms=500", Some(TOKEN)))
        .await
        .unwrap();
    assert_eq!(custom.status(), StatusCode::OK);
    assert_eq!(json_body(custom).await[0]["name"], "Garage after 500 ms");

    for bad in [
        "/api/onvif/devices?wait_ms=100",
        "/api/onvif/devices?wait_ms=10001",
        "/api/onvif/devices?wait_ms=soon",
    ] {
        let refused = router(Arc::clone(&state))
            .oneshot(get_devices(bad, Some(TOKEN)))
            .await
            .unwrap();
        assert_eq!(refused.status(), StatusCode::BAD_REQUEST, "{bad}");
    }
}

#[tokio::test]
async fn onvif_stream_urls_follow_the_host_and_map_the_daemon_refusals() {
    let dir = tempfile::tempdir().unwrap();
    let (sender, receiver) = mpsc::channel(4);
    let state = api_state(dir.path(), Some(CameraControl::new(sender)));
    stand_in_daemon(Arc::clone(&state), receiver);

    let good = json!({"host": "192.0.2.20", "username": "user", "password": "secret"});
    let response = router(Arc::clone(&state))
        .oneshot(post_streams(&good, Some(TOKEN)))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        json_body(response).await,
        json!([{"token": "profile-1", "name": "Main", "url": "rtsp://192.0.2.20:554/live/1"}])
    );

    let failing = json!({"host": "192.0.2.99", "username": "user", "password": "secret"});
    let upstream = router(Arc::clone(&state))
        .oneshot(post_streams(&failing, Some(TOKEN)))
        .await
        .unwrap();
    assert_eq!(upstream.status(), StatusCode::BAD_GATEWAY);

    let port =
        json!({"host": "192.0.2.20", "http_port": 8080, "username": "user", "password": "secret"});
    let refused_port = router(Arc::clone(&state))
        .oneshot(post_streams(&port, Some(TOKEN)))
        .await
        .unwrap();
    assert_eq!(refused_port.status(), StatusCode::BAD_REQUEST);

    let (quiet_sender, mut quiet_receiver) = mpsc::channel(4);
    let quiet_dir = tempfile::tempdir().unwrap();
    let quiet = api_state(quiet_dir.path(), Some(CameraControl::new(quiet_sender)));
    for host in ["a b", "192.0.2.20:80", "user@192.0.2.20", ""] {
        let body = json!({"host": host, "username": "user", "password": "secret"});
        let refused = router(Arc::clone(&quiet))
            .oneshot(post_streams(&body, Some(TOKEN)))
            .await
            .unwrap();
        assert_eq!(refused.status(), StatusCode::BAD_REQUEST, "{host:?}");
    }
    let nameless = json!({"host": "192.0.2.20", "username": " ", "password": "secret"});
    let refused = router(Arc::clone(&quiet))
        .oneshot(post_streams(&nameless, Some(TOKEN)))
        .await
        .unwrap();
    assert_eq!(refused.status(), StatusCode::BAD_REQUEST);
    assert!(
        quiet_receiver.try_recv().is_err(),
        "no refused lookup reached the daemon"
    );
}

#[tokio::test]
async fn onvif_lookups_need_the_control_and_the_root_token() {
    let disabled_dir = tempfile::tempdir().unwrap();
    let disabled = api_state(disabled_dir.path(), None);
    let body = json!({"host": "192.0.2.20", "username": "user", "password": "secret"});
    let devices = router(Arc::clone(&disabled))
        .oneshot(get_devices("/api/onvif/devices", Some(TOKEN)))
        .await
        .unwrap();
    assert_eq!(devices.status(), StatusCode::FORBIDDEN);
    let streams = router(Arc::clone(&disabled))
        .oneshot(post_streams(&body, Some(TOKEN)))
        .await
        .unwrap();
    assert_eq!(streams.status(), StatusCode::FORBIDDEN);

    let enabled_dir = tempfile::tempdir().unwrap();
    let (sender, _receiver) = mpsc::channel(4);
    let enabled = api_state(enabled_dir.path(), Some(CameraControl::new(sender)));
    for request in [
        get_devices("/api/onvif/devices", None),
        get_devices(
            "/api/onvif/devices",
            Some("not-the-token-not-the-token-1234"),
        ),
        post_streams(&body, None),
    ] {
        let response = router(Arc::clone(&enabled)).oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }
}
