//! Synthetic playback regression; the optional tools are not required for unit tests.

use std::collections::HashMap;
use std::fs;
use std::process::Command;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use axum::Router;
use axum::extract::{OriginalUri, Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;

#[derive(Default)]
struct Fixture {
    playlists: AtomicUsize,
    wall_start: Mutex<i64>,
    fetched: Mutex<Vec<String>>,
    media: HashMap<String, Vec<u8>>,
}

async fn respond(
    State(fixture): State<Arc<Fixture>>,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
    Query(query): Query<HashMap<String, String>>,
) -> Response {
    let path = uri.path();
    if path == "/api/cameras" {
        return (
            [(header::CONTENT_TYPE, "application/json")],
            r#"[{"id":"fixture","name":"Fixture"}]"#,
        )
            .into_response();
    }
    if path.ends_with("/segments") {
        let end: i64 = query["to"].parse().unwrap();
        *fixture.wall_start.lock().unwrap() = end - 6_000_000;
        return format!(
            r#"[{{"wall_start_us":{},"wall_end_us":{end},"sealed":true}}]"#,
            end - 6_000_000
        )
        .into_response();
    }
    if path == "/api/events" {
        return "[]".into_response();
    }
    if path.ends_with("vod.m3u8") {
        assert_eq!(
            headers
                .get(header::AUTHORIZATION)
                .and_then(|value| value.to_str().ok()),
            Some("Bearer fixture-token")
        );
        assert!(
            !query.contains_key("token"),
            "root bearer must not enter URLs"
        );
        let first = fixture.playlists.fetch_add(1, Ordering::SeqCst) == 0;
        let wall_start = *fixture.wall_start.lock().unwrap();
        let mut playlist = concat!(
            "#EXTM3U\n#EXT-X-VERSION:7\n#EXT-X-TARGETDURATION:2\n",
            "#EXT-X-PLAYLIST-TYPE:VOD\n#EXT-X-MEDIA-SEQUENCE:0\n",
            "#EXT-X-MAP:URI=\"/init.mp4?token=fixture-token\"\n"
        )
        .to_owned();
        for i in if first { 0..3 } else { 1..3 } {
            use std::fmt::Write as _;
            writeln!(playlist, "#IRIS-FRAGMENT:WALL-US={},DURATION-TICKS=180000\n#EXTINF:2.00000,\ntest{i}.m4s?token=fixture-token", wall_start + i * 2_000_000).unwrap();
        }
        playlist.push_str("#EXT-X-ENDLIST\n");
        return (
            [(header::CONTENT_TYPE, "application/vnd.apple.mpegurl")],
            playlist,
        )
            .into_response();
    }
    let file = path.rsplit('/').next().unwrap_or_default();
    if let Some(bytes) = fixture.media.get(file) {
        if path != "/init.mp4" && !path.starts_with("/api/streams/fixture/main/hls/test") {
            return StatusCode::NOT_FOUND.into_response();
        }
        if query.get("token").map(String::as_str) != Some("fixture-token") {
            return StatusCode::UNAUTHORIZED.into_response();
        }
        fixture.fetched.lock().unwrap().push(file.to_owned());
        return ([(header::CONTENT_TYPE, "video/mp4")], bytes.clone()).into_response();
    }
    StatusCode::NOT_FOUND.into_response()
}

fn synthetic_media(directory: &std::path::Path) -> HashMap<String, Vec<u8>> {
    let generated = Command::new("ffmpeg")
        .args([
            "-v",
            "error",
            "-f",
            "lavfi",
            "-i",
            "color=c=black:s=64x64:r=5",
            "-t",
            "6",
            "-an",
            "-c:v",
            "libx264",
            "-g",
            "10",
            "-sc_threshold",
            "0",
            "-pix_fmt",
            "yuv420p",
            "-f",
            "hls",
            "-hls_time",
            "2",
            "-hls_segment_type",
            "fmp4",
            "-hls_playlist_type",
            "vod",
            "-hls_segment_filename",
            "test%d.m4s",
            "fixture.m3u8",
        ])
        .current_dir(directory)
        .output()
        .expect("run ffmpeg to generate synthetic media");
    assert!(
        generated.status.success(),
        "{}",
        String::from_utf8_lossy(&generated.stderr)
    );
    ["init.mp4", "test0.m4s", "test1.m4s", "test2.m4s"]
        .into_iter()
        .map(|file| (file.to_owned(), fs::read(directory.join(file)).unwrap()))
        .collect()
}

#[test]
#[ignore = "requires ffmpeg with libx264, Xvfb, and GStreamer HLS/H.264 plugins"]
fn player_uses_the_playlist_snapshot_that_supplied_its_timeline() {
    let directory = tempfile::tempdir().unwrap();
    let fixture = Arc::new(Fixture {
        media: synthetic_media(directory.path()),
        ..Fixture::default()
    });
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let listener = runtime
        .block_on(tokio::net::TcpListener::bind("127.0.0.1:0"))
        .unwrap();
    let address = listener.local_addr().unwrap();
    let server = runtime.spawn({
        let fixture = Arc::clone(&fixture);
        async move {
            axum::serve(
                listener,
                Router::new().fallback(get(respond)).with_state(fixture),
            )
            .await
            .unwrap();
        }
    });
    let output = Command::new("timeout")
        .args([
            "12",
            "xvfb-run",
            "-a",
            env!("CARGO_BIN_EXE_iris-gui"),
            "--api",
        ])
        .arg(format!("http://{address}"))
        .args(["--focus", "fixture", "--playback", "--window", "1"])
        .env("IRISD_TOKEN", "fixture-token")
        .env("GSK_RENDERER", "gl")
        .env("GST_PLUGIN_FEATURE_RANK", "nvh264dec:NONE,nvh265dec:NONE")
        .output()
        .expect("run bounded viewer on a virtual display");
    server.abort();
    runtime.shutdown_timeout(Duration::from_secs(1));
    let logs = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        logs.contains("end of window"),
        "playback must reach EOS: {logs}"
    );
    assert!(!logs.contains("playback error"), "playback failed: {logs}");
    assert_eq!(
        fixture.playlists.load(Ordering::SeqCst),
        1,
        "mapping and playback must use one selection: {logs}"
    );
    for file in ["init.mp4", "test0.m4s", "test1.m4s", "test2.m4s"] {
        assert!(
            fixture.fetched.lock().unwrap().iter().any(|f| f == file),
            "snapshot media {file} was not fetched: {logs}"
        );
    }
}
