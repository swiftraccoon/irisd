//! `iris-gui`: a GTK 4 viewer for irisd.
//!
//! The window shows a grid with one tile per camera, a single-camera view of
//! the main stream when a tile is clicked (or from the start with `--focus`),
//! and a playback view of one camera's recordings over a window, played from
//! the daemon's HLS VOD playlist with a timeline for seeking. Each tile owns a GStreamer pipeline fed from the daemon's live
//! WebSocket endpoint: an init segment followed by one fragment per frame,
//! which `qtdemux` consumes as a fragmented MP4 byte stream, so the viewer
//! never parses video itself. Decoding goes through `decodebin3`, which picks
//! the highest-ranked H.265 decoder present (hardware where the drivers
//! provide one, software otherwise), and the frames land in
//! `gtk4paintablesink`. Live tiles are branches of one shared pipeline, the
//! stage, so they share one GL context. Only the visible view decodes: the
//! grid's branches stop while a camera is focused and start again on return.

mod api;
mod live;
mod playback;
mod snapshot;
mod stage;
mod thumbs;
mod tile;
mod timeline;

use std::cell::RefCell;
use std::num::NonZeroU32;
use std::rc::Rc;
use std::sync::Arc;

use clap::Parser;
use gtk::prelude::*;
use gtk::{gio, glib};
use tracing::{error, info};
use tracing_subscriber::EnvFilter;
use tracing_subscriber::Layer as _;
use tracing_subscriber::filter;
use tracing_subscriber::layer::SubscriberExt as _;
use tracing_subscriber::util::SubscriberInitExt as _;
use url::Url;

use crate::api::{Camera, Client};
use crate::live::Feed;
use crate::playback::{Playback, thumbnail_decoder_pool};
use crate::stage::Stage;
use crate::tile::Tile;

/// Command-line options; the token also comes from the environment so it stays
/// out of process listings.
#[derive(Clone, Debug, Parser)]
#[command(version)]
struct Args {
    /// Base URL of the irisd API.
    #[arg(long, env = "IRISD_API", default_value = "http://127.0.0.1:8554")]
    api: Url,
    /// Bearer token for the API.
    #[arg(long, env = "IRISD_TOKEN", hide_env_values = true)]
    token: Option<String>,
    /// Stream kind shown in the grid tiles.
    #[arg(long, default_value = "sub")]
    kind: String,
    /// Stream kind shown in the single-camera view.
    #[arg(long, default_value = "main")]
    focus_kind: String,
    /// Tiles per row.
    #[arg(long, default_value = "3")]
    columns: NonZeroU32,
    /// Open this camera's single-camera view at start.
    #[arg(long, value_name = "CAMERA")]
    focus: Option<String>,
    /// With `--focus`, open the playback view instead of the live view.
    #[arg(long, requires = "focus")]
    playback: bool,
    /// Length of the playback window, ending at the time the view opens.
    #[arg(long, default_value_t = 60, value_name = "MINUTES")]
    window: u32,
    /// With `--playback`, export the whole window to this file as soon as the
    /// view opens, without a dialog (for scripts and tests).
    #[arg(long, requires = "playback", value_name = "FILE")]
    export_to: Option<std::path::PathBuf>,
}

/// What background tasks and widgets tell the main loop.
enum Event {
    Cameras(Vec<Camera>),
    Status(String),
    Focus(String),
    /// The Playback button of the focused view.
    PlaybackFocused,
    Grid,
}

/// The windows's views and the tiles behind them.
struct Views {
    stack: gtk::Stack,
    grid: gtk::Grid,
    focus_box: gtk::Box,
    focus_title: gtk::Label,
    status: gtk::Label,
    tiles: Vec<Tile>,
    focused: Option<Tile>,
    playback: Option<Rc<Playback>>,
    playback_task: Option<glib::JoinHandle<()>>,
    thumbnail_decoders: Arc<tokio::sync::Semaphore>,
    runtime: Arc<tokio::runtime::Runtime>,
    client: Arc<Client>,
    tx: async_channel::Sender<Event>,
    stage: Rc<Stage>,
}

/// The GTK renderer the viewer runs on. The GL renderer shows the sink's GL
/// textures directly; GTK's Vulkan renderer, the default on some drivers,
/// cannot import them and copies every frame through the CPU.
const RENDERER: &str = "gl";

/// tungstenite serializes the complete handshake, including Authorization,
/// before emitting this dependency trace target. The output layer suppresses
/// it regardless of the caller's `RUST_LOG` filter.
const SENSITIVE_LOG_TARGET: &str = "tungstenite::handshake::client";

fn safe_log_metadata(metadata: &tracing::Metadata<'_>) -> bool {
    metadata.target() != SENSITIVE_LOG_TARGET
}

/// Re-executes the viewer with `GSK_RENDERER` set when nothing set it. GTK reads
/// the variable before any API runs, and setting it in-process is unsafe, so
/// the process replaces itself once; on failure the viewer runs as it is.
fn select_renderer() {
    use std::os::unix::process::CommandExt as _;
    if std::env::var_os("GSK_RENDERER").is_some() {
        return;
    }
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    let error = std::process::Command::new(exe)
        .args(std::env::args_os().skip(1))
        .env("GSK_RENDERER", RENDERER)
        .exec();
    eprintln!("cannot re-execute with GSK_RENDERER={RENDERER}: {error}");
}

fn main() -> glib::ExitCode {
    select_renderer();
    let args = Args::parse();
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::registry()
        .with(filter)
        .with(tracing_subscriber::fmt::layer().with_filter(filter::filter_fn(safe_log_metadata)))
        .init();
    if let Err(error) = gst::init() {
        error!(%error, "GStreamer failed to initialise");
        return glib::ExitCode::FAILURE;
    }
    if let Err(error) = gstgtk4::plugin_register_static() {
        error!(%error, "the gtk4 sink plugin failed to register");
        return glib::ExitCode::FAILURE;
    }
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
    {
        Ok(runtime) => Arc::new(runtime),
        Err(error) => {
            error!(%error, "cannot start the async runtime");
            return glib::ExitCode::FAILURE;
        }
    };
    let app = gtk::Application::builder()
        .application_id("dev.irisd.Viewer")
        .flags(gio::ApplicationFlags::NON_UNIQUE)
        .build();
    let thumbnail_decoders = thumbnail_decoder_pool();
    app.connect_activate(move |app| {
        build_window(app, &args, &runtime, &thumbnail_decoders);
    });
    app.run_with_args::<&str>(&[])
}

fn build_window(
    app: &gtk::Application,
    args: &Args,
    runtime: &Arc<tokio::runtime::Runtime>,
    thumbnail_decoders: &Arc<tokio::sync::Semaphore>,
) {
    let (tx, rx) = async_channel::unbounded::<Event>();
    let (column, stack, grid, focus_box, focus_title, status) = build_widgets(&tx);
    let window = gtk::ApplicationWindow::builder()
        .application(app)
        .title("irisd")
        .default_width(1280)
        .default_height(800)
        .child(&column)
        .build();
    window.present();

    let stage = match Stage::new() {
        Ok(stage) => stage,
        Err(error) => {
            error!(%error, "cannot start the stage pipeline");
            app.quit();
            return;
        }
    };
    let client = Arc::new(Client::new(args.api.clone(), args.token.clone()));
    runtime.spawn({
        let tx = tx.clone();
        let client = Arc::clone(&client);
        async move {
            match client.cameras().await {
                Ok(cameras) => {
                    let _ = tx.send(Event::Cameras(cameras)).await;
                }
                Err(error) => {
                    let _ = tx
                        .send(Event::Status(format!("cannot list cameras: {error}")))
                        .await;
                }
            }
        }
    });

    let views = Rc::new(RefCell::new(Views {
        stack,
        grid,
        focus_box,
        focus_title,
        status,
        tiles: Vec::new(),
        focused: None,
        playback: None,
        playback_task: None,
        thumbnail_decoders: Arc::clone(thumbnail_decoders),
        runtime: Arc::clone(runtime),
        client,
        tx: tx.clone(),
        stage,
    }));
    glib::spawn_future_local(run_events(views, args.clone(), rx));
}

/// Receives UI events without awaiting playback I/O. Playback owns a separate
/// cancellable task, so navigation is always able to reach [`leave_views`].
async fn run_events(views: Rc<RefCell<Views>>, args: Args, rx: async_channel::Receiver<Event>) {
    while let Ok(event) = rx.recv().await {
        match event {
            Event::Cameras(cameras) => {
                let open_playback_for = {
                    let mut v = views.borrow_mut();
                    v.status.set_label(&format!("{} cameras", cameras.len()));
                    populate_grid(&mut v, &cameras, &args);
                    match &args.focus {
                        Some(camera) if args.playback => Some(camera.clone()),
                        Some(camera) => {
                            focus(&mut v, camera, &args);
                            None
                        }
                        None => {
                            show_grid(&mut v, &args);
                            None
                        }
                    }
                };
                if let Some(camera) = open_playback_for {
                    open_playback(&views, &camera, &args);
                }
            }
            Event::Status(text) => views.borrow().status.set_label(&text),
            Event::Focus(camera) => focus(&mut views.borrow_mut(), &camera, &args),
            Event::PlaybackFocused => {
                let camera = views
                    .borrow()
                    .focused
                    .as_ref()
                    .map(|tile| tile.camera().to_owned());
                if let Some(camera) = camera {
                    open_playback(&views, &camera, &args);
                }
            }
            Event::Grid => show_grid(&mut views.borrow_mut(), &args),
        }
    }
}

/// Leaves whichever view is showing: stops the grid, the focused tile, and playback.
fn leave_views(views: &mut Views) {
    if let Some(task) = views.playback_task.take() {
        task.abort();
    }
    for tile in &views.tiles {
        tile.stop();
    }
    if let Some(tile) = views.focused.take() {
        tile.stop();
        views.focus_box.remove(tile.widget());
    }
    if let Some(playback) = views.playback.take() {
        playback.close();
        views.stack.remove(playback.widget());
    }
}

/// Opens the playback view for `camera` over the last `--window` minutes.
fn open_playback(views: &Rc<RefCell<Views>>, camera: &str, args: &Args) {
    let (playback, client, runtime) = {
        let mut v = views.borrow_mut();
        leave_views(&mut v);
        let back = gtk::Button::with_label("Back to grid");
        back.connect_clicked({
            let tx = v.tx.clone();
            move |_| {
                let _ = tx.try_send(Event::Grid);
            }
        });
        let live = gtk::Button::with_label("Live");
        live.connect_clicked({
            let tx = v.tx.clone();
            let camera = camera.to_owned();
            move |_| {
                let _ = tx.try_send(Event::Focus(camera.clone()));
            }
        });
        let playback = Playback::new(
            camera,
            &args.focus_kind,
            &args.kind,
            &[back, live],
            Arc::clone(&v.thumbnail_decoders),
        );
        v.stack.add_named(playback.widget(), Some("playback"));
        v.stack.set_visible_child_name("playback");
        v.playback = Some(Rc::clone(&playback));
        (playback, Arc::clone(&v.client), Arc::clone(&v.runtime))
    };
    let to = glib::real_time();
    let from = to - i64::from(args.window) * 60 * 1_000_000;
    info!(camera, kind = %args.focus_kind, minutes = args.window, "playback opened");
    let export_to = args.export_to.clone();
    let task = glib::spawn_future_local(async move {
        playback.open(client, runtime, from, to).await;
        if let Some(path) = export_to {
            Rc::clone(&playback).export_to(path, from, to).await;
        }
    });
    views.borrow_mut().playback_task = Some(task);
}

/// Builds the window's content: the stack with the grid and the focused view,
/// and the status line under it.
fn build_widgets(
    tx: &async_channel::Sender<Event>,
) -> (
    gtk::Box,
    gtk::Stack,
    gtk::Grid,
    gtk::Box,
    gtk::Label,
    gtk::Label,
) {
    let grid = gtk::Grid::builder()
        .column_homogeneous(true)
        .row_homogeneous(true)
        .column_spacing(4)
        .row_spacing(4)
        .vexpand(true)
        .build();
    let focus_box = gtk::Box::new(gtk::Orientation::Vertical, 4);
    let focus_header = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    let back = gtk::Button::with_label("Back to grid");
    back.connect_clicked({
        let tx = tx.clone();
        move |_| {
            let _ = tx.try_send(Event::Grid);
        }
    });
    let playback = gtk::Button::with_label("Playback");
    playback.connect_clicked({
        let tx = tx.clone();
        move |_| {
            let _ = tx.try_send(Event::PlaybackFocused);
        }
    });
    let focus_title = gtk::Label::builder().xalign(0.0).hexpand(true).build();
    focus_header.append(&back);
    focus_header.append(&playback);
    focus_header.append(&focus_title);
    focus_box.append(&focus_header);
    let stack = gtk::Stack::new();
    stack.add_named(&grid, Some("grid"));
    stack.add_named(&focus_box, Some("focus"));
    let status = gtk::Label::builder()
        .label("connecting")
        .xalign(0.0)
        .margin_start(8)
        .margin_end(8)
        .margin_top(4)
        .margin_bottom(4)
        .build();
    let column = gtk::Box::new(gtk::Orientation::Vertical, 0);
    column.append(&stack);
    column.append(&status);
    (column, stack, grid, focus_box, focus_title, status)
}

fn feed(args: &Args, camera: &str, kind: &str) -> Feed {
    Feed {
        api: args.api.clone(),
        token: args.token.clone(),
        camera: camera.to_owned(),
        kind: kind.to_owned(),
    }
}

/// Creates one tile per camera in the grid; a click on a tile focuses it.
fn populate_grid(views: &mut Views, cameras: &[Camera], args: &Args) {
    let tx = views.tx.clone();
    for (index, camera) in cameras.iter().enumerate() {
        let tile = Tile::new(
            &camera.id,
            &camera.name,
            Arc::clone(&views.runtime),
            Rc::clone(&views.stage),
        );
        let position = u32::try_from(index).unwrap_or(u32::MAX);
        let columns = args.columns.get();
        views.grid.attach(
            tile.widget(),
            i32::try_from(position % columns).unwrap_or(0),
            i32::try_from(position / columns).unwrap_or(0),
            1,
            1,
        );
        let click = gtk::GestureClick::new();
        click.connect_released({
            let tx = tx.clone();
            let id = camera.id.clone();
            move |_, _, _, _| {
                let _ = tx.try_send(Event::Focus(id.clone()));
            }
        });
        tile.widget().add_controller(click);
        views.tiles.push(tile);
    }
}

/// Shows the grid and starts every tile's feed.
fn show_grid(views: &mut Views, args: &Args) {
    leave_views(views);
    for tile in &views.tiles {
        if let Err(error) = tile.start(feed(args, tile.camera(), &args.kind)) {
            error!(camera = tile.camera(), %error, "cannot start a tile");
        }
    }
    views.stack.set_visible_child_name("grid");
    info!(tiles = views.tiles.len(), "grid shown");
}

/// Shows `camera`'s main stream alone and stops the grid's decoders.
fn focus(views: &mut Views, camera: &str, args: &Args) {
    let Some(name) = views
        .tiles
        .iter()
        .find(|t| t.camera() == camera)
        .map(|t| t.name().to_owned())
    else {
        views.status.set_label(&format!("unknown camera {camera}"));
        return;
    };
    leave_views(views);
    let tile = Tile::new(
        camera,
        &name,
        Arc::clone(&views.runtime),
        Rc::clone(&views.stage),
    );
    views
        .focus_title
        .set_label(&format!("{name} ({camera}), {} stream", args.focus_kind));
    views.focus_box.append(tile.widget());
    if let Err(error) = tile.start(feed(args, camera, &args.focus_kind)) {
        error!(camera, %error, "cannot start the focused tile");
    }
    views.focused = Some(tile);
    views.stack.set_visible_child_name("focus");
    info!(camera, kind = %args.focus_kind, "camera focused");
}

#[cfg(test)]
mod tests {
    use std::convert::Infallible;
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::pin::Pin;
    use std::sync::Mutex;
    use std::task::{Context, Poll};

    use clap::error::ErrorKind;
    use tokio_tungstenite::tungstenite::client::IntoClientRequest as _;
    use tracing_subscriber::fmt::MakeWriter;

    use super::*;

    #[derive(Clone, Debug)]
    struct Captured(Arc<Mutex<Vec<u8>>>);

    struct CapturedWriter(Arc<Mutex<Vec<u8>>>);

    struct DropSignal(async_channel::Sender<()>);

    impl Drop for DropSignal {
        fn drop(&mut self) {
            let _ = self.0.try_send(());
        }
    }

    struct PendingResponse {
        _drop_signal: DropSignal,
    }

    impl futures::Stream for PendingResponse {
        type Item = Result<axum::body::Bytes, Infallible>;

        fn poll_next(self: Pin<&mut Self>, _context: &mut Context<'_>) -> Poll<Option<Self::Item>> {
            Poll::Pending
        }
    }

    impl Write for CapturedWriter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0
                .lock()
                .expect("capture mutex is not poisoned")
                .extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'writer> MakeWriter<'writer> for Captured {
        type Writer = CapturedWriter;

        fn make_writer(&'writer self) -> Self::Writer {
            CapturedWriter(Arc::clone(&self.0))
        }
    }

    #[test]
    fn broad_trace_does_not_emit_websocket_authorization() {
        const TOKEN: &str = "gui-v01-unique-sentinel";
        const CONTROL: &str = "gui-v01-unrelated-trace";

        let bytes = Arc::new(Mutex::new(Vec::new()));
        tracing_subscriber::registry()
            .with(EnvFilter::new("trace"))
            .with(
                tracing_subscriber::fmt::layer()
                    .with_ansi(false)
                    .without_time()
                    .with_writer(Captured(Arc::clone(&bytes)))
                    .with_filter(filter::filter_fn(safe_log_metadata)),
            )
            .try_init()
            .expect("the test process must not install another global subscriber");

        tracing::trace!(target: "iris_gui::log_filter_fixture", CONTROL);
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback fixture");
        let address = listener.local_addr().expect("fixture address");
        let server = std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().expect("accept fixture client");
            socket
                .set_read_timeout(Some(std::time::Duration::from_secs(2)))
                .expect("set fixture timeout");
            let mut request = Vec::new();
            let mut buffer = [0u8; 1024];
            while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                let read = socket.read(&mut buffer).expect("read fixture request");
                if read == 0 {
                    break;
                }
                request.extend_from_slice(&buffer[..read]);
            }
            socket
                .write_all(b"HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\n\r\n")
                .expect("finish fixture response");
            request
        });

        let mut request = format!("ws://{address}/live")
            .into_client_request()
            .expect("build fixture WebSocket request");
        request.headers_mut().insert(
            tokio_tungstenite::tungstenite::http::header::AUTHORIZATION,
            tokio_tungstenite::tungstenite::http::HeaderValue::from_str(&format!("Bearer {TOKEN}"))
                .expect("fixture authorization header"),
        );
        let stream = TcpStream::connect(address).expect("connect fixture client");
        let _ = tokio_tungstenite::tungstenite::client(request, stream);

        let request = server.join().expect("join fixture server");
        assert!(
            String::from_utf8_lossy(&request).contains(TOKEN),
            "the exercised handshake must contain the sentinel"
        );
        let output =
            String::from_utf8(bytes.lock().expect("capture mutex is not poisoned").clone())
                .expect("captured logs are UTF-8");
        assert!(
            output.contains(CONTROL),
            "unrelated trace must remain visible"
        );
        assert!(
            output.contains("Doing handshake round"),
            "the dependency's non-sensitive trace must reach the output layer"
        );
        assert!(!output.contains(TOKEN), "authorization must not reach logs");
    }

    #[test]
    fn columns_rejects_zero_and_accepts_one() {
        let error = Args::try_parse_from(["iris-gui", "--columns", "0"])
            .expect_err("zero columns must be rejected");
        assert_eq!(error.kind(), ErrorKind::ValueValidation);

        let args =
            Args::try_parse_from(["iris-gui", "--columns", "1"]).expect("one column is valid");
        assert_eq!(args.columns.get(), 1);
    }

    pub(super) fn navigation_event_cancels_a_stalled_playback_request() {
        gtk::init().unwrap();
        gst::init().unwrap();
        gstgtk4::plugin_register_static().unwrap();
        let runtime = Arc::new(tokio::runtime::Runtime::new().unwrap());
        let listener = runtime
            .block_on(tokio::net::TcpListener::bind("127.0.0.1:0"))
            .unwrap();
        let address = listener.local_addr().unwrap();
        let (started_tx, started_rx) = async_channel::bounded(1);
        let (dropped_tx, dropped_rx) = async_channel::bounded(1);
        let server = runtime.spawn(async move {
            let app = axum::Router::new().route(
                "/api/streams/fixture/main/segments",
                axum::routing::get(move || {
                    let started = started_tx.clone();
                    let dropped = dropped_tx.clone();
                    async move {
                        started.send(()).await.unwrap();
                        axum::http::Response::builder()
                            .header(axum::http::header::CONTENT_TYPE, "application/json")
                            .body(axum::body::Body::from_stream(PendingResponse {
                                _drop_signal: DropSignal(dropped),
                            }))
                            .unwrap()
                    }
                }),
            );
            axum::serve(listener, app).await.unwrap();
        });
        let (tx, rx) = async_channel::unbounded();
        let (_, stack, grid, focus_box, focus_title, status) = build_widgets(&tx);
        let views = Rc::new(RefCell::new(Views {
            stack,
            grid,
            focus_box,
            focus_title,
            status,
            tiles: Vec::new(),
            focused: None,
            playback: None,
            playback_task: None,
            thumbnail_decoders: thumbnail_decoder_pool(),
            runtime: Arc::clone(&runtime),
            client: Arc::new(Client::new(
                Url::parse(&format!("http://{address}")).unwrap(),
                None,
            )),
            tx: tx.clone(),
            stage: Stage::new().unwrap(),
        }));
        let args = Args {
            api: Url::parse(&format!("http://{address}")).unwrap(),
            token: None,
            kind: "sub".to_owned(),
            focus_kind: "main".to_owned(),
            columns: NonZeroU32::new(1).unwrap(),
            focus: Some("fixture".to_owned()),
            playback: true,
            window: 1,
            export_to: None,
        };
        let event_task = glib::spawn_future_local(run_events(Rc::clone(&views), args, rx));
        glib::MainContext::default().block_on(async {
            tx.send(Event::Cameras(Vec::new())).await.unwrap();
            receive_before_timeout(&started_rx, "the stalled playback request must start").await;
            tx.send(Event::Grid).await.unwrap();
            wait_for_grid(&views).await;
            receive_before_timeout(
                &dropped_rx,
                "navigation must cancel the response without releasing a server gate",
            )
            .await;
        });
        event_task.abort();
        server.abort();
    }

    async fn receive_before_timeout(rx: &async_channel::Receiver<()>, failure: &str) {
        let received = rx.recv();
        let timeout = glib::timeout_future(std::time::Duration::from_secs(2));
        futures::pin_mut!(received, timeout);
        assert!(
            matches!(
                futures::future::select(received, timeout).await,
                futures::future::Either::Left(_)
            ),
            "{failure}"
        );
    }

    async fn wait_for_grid(views: &Rc<RefCell<Views>>) {
        let transitioned = async {
            loop {
                let is_grid = {
                    let views = views.borrow();
                    views.playback.is_none()
                        && views.playback_task.is_none()
                        && views.stack.visible_child_name().as_deref() == Some("grid")
                };
                if is_grid {
                    break;
                }
                glib::timeout_future(std::time::Duration::from_millis(5)).await;
            }
        };
        let timeout = glib::timeout_future(std::time::Duration::from_secs(2));
        futures::pin_mut!(transitioned, timeout);
        assert!(
            matches!(
                futures::future::select(transitioned, timeout).await,
                futures::future::Either::Left(_)
            ),
            "the real navigation event must return to the grid"
        );
    }
}
