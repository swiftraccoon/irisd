//! A playlist whose references leave the API origin, or that carries a tag the
//! daemon does not emit, is refused before GStreamer sees it; the optional
//! display tools are not required for unit tests.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::process::Command;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use axum::Router;
use axum::extract::{OriginalUri, Query, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing;

/// How the fixture's playlist leaves the daemon's contract.
#[derive(Clone, Copy, Debug)]
enum Mode {
    /// Init and media URIs name a second loopback origin (another port).
    CrossOrigin,
    /// Init and media URIs are `file://` paths.
    FileScheme,
    /// Same-origin media plus an `EXT-X-KEY` tag naming the second origin.
    KeyTag,
}

struct Api {
    mode: Mode,
    other: String,
    wall_start: Mutex<i64>,
    playlists: AtomicUsize,
    media: Mutex<Vec<String>>,
}

async fn api(
    State(api): State<Arc<Api>>,
    OriginalUri(uri): OriginalUri,
    Query(query): Query<HashMap<String, String>>,
) -> Response {
    let path = uri.path();
    if path == "/api/cameras" {
        return (
            [(header::CONTENT_TYPE, "application/json")],
            r#"[{"id":"fixture","name":"Fixture","provisioned":false}]"#,
        )
            .into_response();
    }
    if path.ends_with("/segments") {
        let end: i64 = query["to"].parse().unwrap();
        *api.wall_start.lock().unwrap() = end - 6_000_000;
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
        api.playlists.fetch_add(1, Ordering::SeqCst);
        let wall_start = *api.wall_start.lock().unwrap();
        let prefix = match api.mode {
            Mode::CrossOrigin => format!("http://{}/", api.other),
            Mode::FileScheme => "file:///nonexistent/".to_owned(),
            Mode::KeyTag => "/media/".to_owned(),
        };
        let mut playlist = String::from(concat!(
            "#EXTM3U\n#EXT-X-VERSION:7\n#EXT-X-TARGETDURATION:2\n",
            "#EXT-X-PLAYLIST-TYPE:VOD\n#EXT-X-MEDIA-SEQUENCE:0\n",
        ));
        writeln!(playlist, "#EXT-X-MAP:URI=\"{prefix}init.mp4\"").unwrap();
        if let Mode::KeyTag = api.mode {
            writeln!(
                playlist,
                "#EXT-X-KEY:METHOD=AES-128,URI=\"http://{}/key.bin\",IV=0x{:032x}",
                api.other, 0
            )
            .unwrap();
        }
        for i in 0..3 {
            writeln!(
                playlist,
                "#IRIS-FRAGMENT:WALL-US={},DURATION-TICKS=180000\n\
                 #EXTINF:2.00000,\n{prefix}test{i}.m4s",
                wall_start + i * 2_000_000
            )
            .unwrap();
        }
        playlist.push_str("#EXT-X-ENDLIST\n");
        return (
            [(header::CONTENT_TYPE, "application/vnd.apple.mpegurl")],
            playlist,
        )
            .into_response();
    }
    api.media.lock().unwrap().push(path.to_owned());
    StatusCode::NOT_FOUND.into_response()
}

async fn other(State(fetched): State<Arc<Mutex<Vec<String>>>>, uri: OriginalUri) -> StatusCode {
    fetched.lock().unwrap().push(uri.path().to_owned());
    StatusCode::NOT_FOUND
}

fn run(mode: Mode, expected: &str) {
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let other_listener = runtime
        .block_on(tokio::net::TcpListener::bind("127.0.0.1:0"))
        .unwrap();
    let other_address = other_listener.local_addr().unwrap();
    let other_fetched = Arc::new(Mutex::new(Vec::new()));
    let other_server = runtime.spawn({
        let fetched = Arc::clone(&other_fetched);
        async move {
            axum::serve(
                other_listener,
                Router::new()
                    .fallback(routing::get(other))
                    .with_state(fetched),
            )
            .await
            .unwrap();
        }
    });
    let listener = runtime
        .block_on(tokio::net::TcpListener::bind("127.0.0.1:0"))
        .unwrap();
    let address = listener.local_addr().unwrap();
    let api_state = Arc::new(Api {
        mode,
        other: other_address.to_string(),
        wall_start: Mutex::new(0),
        playlists: AtomicUsize::new(0),
        media: Mutex::new(Vec::new()),
    });
    let server = runtime.spawn({
        let state = Arc::clone(&api_state);
        async move {
            axum::serve(
                listener,
                Router::new().fallback(routing::get(api)).with_state(state),
            )
            .await
            .unwrap();
        }
    });
    let output = Command::new("timeout")
        .args([
            "10",
            "xvfb-run",
            "-a",
            env!("CARGO_BIN_EXE_iris-gui"),
            "--api",
        ])
        .arg(format!("http://{address}"))
        .args(["--focus", "fixture", "--playback", "--window", "1"])
        .env("IRISD_TOKEN", "fixture-token")
        .env("GSK_RENDERER", "gl")
        .output()
        .expect("run bounded viewer on a virtual display");
    server.abort();
    other_server.abort();
    runtime.shutdown_timeout(Duration::from_secs(1));
    let logs = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        api_state.playlists.load(Ordering::SeqCst) >= 1,
        "{mode:?}: the viewer must reach the playlist: {logs}"
    );
    assert!(
        logs.contains(expected),
        "{mode:?}: the playlist must be refused: {logs}"
    );
    assert_eq!(
        *other_fetched.lock().unwrap(),
        Vec::<String>::new(),
        "{mode:?}: the second origin must receive nothing"
    );
    assert_eq!(
        *api_state.media.lock().unwrap(),
        Vec::<String>::new(),
        "{mode:?}: no media may be fetched from a refused playlist"
    );
    assert!(!logs.contains("end of window"), "{mode:?}: {logs}");
}

#[test]
#[ignore = "requires Xvfb and the GTK and GStreamer runtime libraries"]
fn playlists_leaving_the_api_origin_are_refused() {
    run(Mode::CrossOrigin, "VOD playlist references another origin");
    run(Mode::FileScheme, "VOD playlist references another origin");
    run(
        Mode::KeyTag,
        "VOD playlist has a tag the daemon does not emit",
    );
}
