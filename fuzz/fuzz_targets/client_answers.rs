//! Fuzzes how a viewer reads an answer (`iris_client::api`) of any status,
//! content type, and body, held whole or not, from the API origin or another.
//! `HttpRequest::body_to_read` of every request says what to read of the
//! body, and every check gives the call's value or a refusal whose text a
//! viewer can show; neither panics.
//!
//! The input starts with two bytes of status, big-endian. A flags byte
//! follows: its lowest bit says the transport holds the body, and the next
//! that the answer came from another origin. Then come the length of the
//! content type and its bytes, which stand for no header when they are not
//! UTF-8. The rest is the body.
#![no_main]

use std::sync::LazyLock;
use std::time::Duration;

use iris_client::api::{
    Answer, ApiError, EventId, HttpRequest, MAX_EVENTS, NewCamera, Retention, StreamUrlsRequest,
    Window, answer,
};
use iris_client::origin::{ApiOrigin, PlainHttp};
use iris_core::id::{CameraId, StreamKind};
use iris_core::time::WallUs;
use libfuzzer_sys::fuzz_target;
use url::Url;

/// One request of each call, as the call's builder makes it. What
/// `HttpRequest::body_to_read` returns depends on the call alone, so one
/// request of each call covers every request.
static REQUESTS: LazyLock<[HttpRequest; 11]> = LazyLock::new(|| {
    let origin = ApiOrigin::parse("https://nvr.example:8443", PlainHttp::LoopbackOnly)
        .expect("a fixed origin");
    let camera = CameraId::try_new("cam1").expect("a fixed camera id");
    let window = Window::try_new(WallUs::new(0), WallUs::new(1)).expect("a microsecond");
    let added = NewCamera {
        id: camera.clone(),
        name: "Front".into(),
        rtsp_url: "rtsp://192.0.2.10/x".into(),
        sub_url: None,
        username: "viewer".into(),
        password: "secret".into(),
        record: vec![StreamKind::Main],
        events: Vec::new(),
    };
    let lookup = StreamUrlsRequest {
        host: "192.0.2.20".into(),
        http_port: 80,
        https: false,
        tls_verify: true,
        username: "viewer".into(),
        password: "secret".into(),
    };
    let event = EventId::try_new(1).expect("a positive event id");
    [
        origin.cameras(),
        origin
            .add_camera(&added)
            .expect("a camera the builder takes"),
        origin.remove_camera(&camera),
        origin.segments(&camera, StreamKind::Main, window),
        origin
            .events(&camera, window, MAX_EVENTS)
            .expect("a page the builder takes"),
        origin.export(&camera, StreamKind::Main, window),
        origin.retain_event(event, Retention::Release),
        origin.keyframe(&camera, StreamKind::Main, WallUs::new(0)),
        origin.vod_playlist(&camera, StreamKind::Main, window),
        origin
            .discover(Duration::from_secs(3))
            .expect("a wait the builder takes"),
        origin.stream_urls(&lookup),
    ]
});

/// Shows a refusal as a viewer would, so the texts are built too.
fn show<T>(result: Result<T, ApiError>) {
    if let Err(refusal) = result {
        let _ = refusal.to_string();
    }
}

fuzz_target!(|data: &[u8]| {
    let Some((&[high, low, flags, type_len], rest)) = data.split_first_chunk::<4>() else {
        return;
    };
    let (content_type, body) = rest.split_at(usize::from(type_len).min(rest.len()));
    let origin = ApiOrigin::parse("https://nvr.example:8443", PlainHttp::LoopbackOnly)
        .expect("a fixed origin");
    let final_url = if flags & 2 == 0 {
        Url::parse("https://nvr.example:8443/api/streams/cam1/main/hls/vod.m3u8?from=0&to=1")
    } else {
        Url::parse("https://other.example/api/cameras")
    }
    .expect("a fixed URL");
    let answer = Answer {
        status: u16::from_be_bytes([high, low]),
        content_type: std::str::from_utf8(content_type).ok(),
        final_url: &final_url,
        body: (flags & 1 == 1).then_some(body),
    };
    for request in REQUESTS.iter() {
        let _ = request.body_to_read(answer.status, answer.content_type);
    }
    let camera = CameraId::try_new("cam1").expect("a fixed camera id");
    let window = Window::try_new(WallUs::new(0), WallUs::new(3_600_000_000)).expect("an hour");
    show(answer::cameras(&origin, &answer));
    show(answer::add_camera(&origin, &answer, &camera));
    show(answer::remove_camera(&origin, &answer));
    show(answer::segments(&origin, &answer, window));
    show(answer::events(&origin, &answer, window, MAX_EVENTS));
    show(answer::export(&origin, &answer));
    show(answer::retain_event(
        &origin,
        &answer,
        EventId::try_new(1).expect("a positive event id"),
    ));
    show(answer::keyframe(&origin, &answer));
    show(answer::vod_playlist(&origin, &answer, window));
    show(answer::discover(&origin, &answer));
    show(answer::stream_urls(&origin, &answer));
});
