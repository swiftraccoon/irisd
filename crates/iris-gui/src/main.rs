//! `iris-gui`: a GTK 4 viewer for irisd.
//!
//! The window shows a grid with one tile per camera, a single-camera view of
//! the main stream when a tile is clicked (or from the start with `--focus`),
//! and a playback view of one camera's recordings over a window, played from
//! the daemon's HLS VOD playlist with a timeline for seeking.
//!
//! Each tile is a branch of one shared GStreamer pipeline, the stage, fed from
//! the daemon's live WebSocket endpoint: an init segment followed by one
//! fragment per frame, which `qtdemux` consumes as a fragmented MP4 byte
//! stream. The viewer's Rust code never parses video; GStreamer's native
//! elements parse and decode it in this process, without a sandbox. Decoding
//! goes through `decodebin3`, which picks the highest-ranked H.265 decoder
//! present (hardware where the drivers provide one, software otherwise), and
//! the frames land in `gtk4paintablesink`; the branches share the stage's GL
//! context. Only the visible view decodes: the grid's branches stop while a
//! camera is focused and start again on return.
//!
//! ```text
//! IRISD_TOKEN=... iris-gui --api https://nvr.example:8443 --kind sub
//! ```
//!
//! The bearer token belongs in `IRISD_TOKEN`, out of process listings. `--api`
//! names the daemon's origin; plain HTTP is accepted on loopback alone unless
//! `--allow-insecure-http` is given.

mod api;
mod live;
mod playback;
mod snapshot;
mod stage;
mod thumbs;
mod tile;
mod timeline;
mod vod;

use std::cell::RefCell;
use std::ffi::{OsStr, OsString};
use std::fmt;
use std::num::NonZeroU32;
use std::path::PathBuf;
use std::rc::{Rc, Weak};
use std::sync::Arc;
use std::time::Duration;

use async_channel::{Receiver, Sender};
use clap::Parser;
use gtk::prelude::*;
use gtk::{gio, glib};
use iris_core::id::{CameraId, StreamKind};
use iris_core::time::WallUs;
use iris_tls::ClientConfig;
use tokio::runtime::Runtime;
use tokio::sync::{Semaphore, oneshot};
use tracing::{error, info, warn};
use tracing_subscriber::EnvFilter;
use tracing_subscriber::Layer as _;
use tracing_subscriber::filter;
use tracing_subscriber::layer::SubscriberExt as _;
use tracing_subscriber::util::SubscriberInitExt as _;
use url::{Host, Url};

use crate::api::{Camera, Client, DiscoveredDevice, NewCamera, ProfileStream, StreamUrlsRequest};
use crate::live::Feed;
use crate::playback::{Playback, thumbnail_decoder_pool};
use crate::stage::Stage;
use crate::tile::{DEFAULT_LIVE_BUDGET_MIB, LiveBudget, MIN_LIVE_BUDGET_MIB, Tile};
use crate::timeline::MICROS_PER_SECOND;

/// The longest `--window`: seven days, the API's longest time range.
const MAX_WINDOW_MINUTES: u32 = 7 * 24 * 60;
/// Microseconds per minute.
const MICROS_PER_MINUTE: i64 = 60 * MICROS_PER_SECOND;

/// Command-line options; the token also comes from the environment so it stays
/// out of process listings. The `Debug` output redacts the token.
#[derive(Clone, Parser)]
#[command(version)]
struct Args {
    /// The irisd API's origin, without a path: `https` (through a TLS proxy) or
    /// `http`.
    #[arg(long, env = "IRISD_API", default_value = "http://127.0.0.1:8554")]
    api: Url,
    /// Accept a plain-HTTP API URL on a non-loopback host. The bearer token and
    /// the video then cross the network unencrypted.
    #[arg(long)]
    allow_insecure_http: bool,
    /// PEM file of certificate authorities trusted beside the platform's store,
    /// for an `https` proxy with a private CA; API reads and `wss` feeds share it.
    #[arg(long, env = "IRISD_CA_FILE", value_name = "PEM")]
    ca_file: Option<PathBuf>,
    /// Bearer token for the API. Prefer `IRISD_TOKEN`: a value given here is
    /// visible to other local users and kept in shell history.
    #[arg(long, env = "IRISD_TOKEN", hide_env_values = true)]
    token: Option<String>,
    /// Stream kind shown in the grid tiles and the playback thumbnails: `main`
    /// or `sub`.
    #[arg(long, default_value = "sub", value_name = "KIND", value_parser = video_kind)]
    kind: StreamKind,
    /// Stream kind shown in the single-camera and playback views: `main` or `sub`.
    #[arg(long, default_value = "main", value_name = "KIND", value_parser = video_kind)]
    focus_kind: StreamKind,
    /// Tiles per row.
    #[arg(long, default_value = "3")]
    columns: NonZeroU32,
    /// Live tiles the grid decodes at once; the tiles after them stay idle and
    /// still open their camera's single-camera view when clicked.
    #[arg(long, default_value = "16", value_name = "N")]
    max_tiles: NonZeroU32,
    /// Live payload bytes alive in the viewer at once, across every tile and the
    /// focused view, in MiB; a feed whose payload does not fit reconnects.
    #[arg(
        long,
        default_value_t = DEFAULT_LIVE_BUDGET_MIB,
        value_name = "MIB",
        value_parser = clap::value_parser!(u32).range(i64::from(MIN_LIVE_BUDGET_MIB)..)
    )]
    live_budget_mib: u32,
    /// Open this camera's single-camera view at start.
    #[arg(long, value_name = "CAMERA")]
    focus: Option<CameraId>,
    /// With `--focus`, open the playback view instead of the live view.
    #[arg(long, requires = "focus")]
    playback: bool,
    /// Length of the playback window in minutes, 1 to 10,080 (seven days, the
    /// API's longest range), ending at the time the view opens.
    #[arg(
        long,
        default_value_t = 60,
        value_name = "MINUTES",
        value_parser = clap::value_parser!(u32).range(1..=i64::from(MAX_WINDOW_MINUTES))
    )]
    window: u32,
    /// With `--playback`, export the whole window to this file as soon as the
    /// view opens, without a dialog (for scripts and tests).
    #[arg(long, requires = "playback", value_name = "FILE")]
    export_to: Option<PathBuf>,
}

impl fmt::Debug for Args {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Args")
            .field("api", &self.api.as_str())
            .field("allow_insecure_http", &self.allow_insecure_http)
            .field("ca_file", &self.ca_file)
            .field("token", &self.token.as_ref().map(|_| "<redacted>"))
            .field("kind", &self.kind)
            .field("focus_kind", &self.focus_kind)
            .field("columns", &self.columns)
            .field("max_tiles", &self.max_tiles)
            .field("live_budget_mib", &self.live_budget_mib)
            .field("focus", &self.focus)
            .field("playback", &self.playback)
            .field("window", &self.window)
            .field("export_to", &self.export_to)
            .finish()
    }
}

/// Parses `--kind` and `--focus-kind`: a stream kind that carries video.
fn video_kind(value: &str) -> Result<StreamKind, String> {
    match value.parse::<StreamKind>() {
        Ok(kind @ (StreamKind::Main | StreamKind::Sub)) => Ok(kind),
        _ => Err("expected main or sub".to_owned()),
    }
}

/// What background tasks and widgets tell the main loop.
enum Event {
    /// The camera list at start.
    Cameras(Vec<Camera>),
    /// The camera list after a camera was added or removed; the grid is rebuilt.
    Refreshed(Vec<Camera>),
    /// List the cameras again.
    Refresh,
    /// A line for the status bar.
    Status(String),
    /// Show this camera alone.
    Focus(CameraId),
    /// The Playback button of the focused view.
    PlaybackFocused,
    /// The "Add camera" button of the grid.
    AddCamera,
    /// The "Remove camera" button of the focused view.
    RemoveFocused,
    /// Show the grid.
    Grid,
}

/// The window's views and the tiles behind them.
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
    thumbnail_decoders: Arc<Semaphore>,
    runtime: Arc<Runtime>,
    client: Arc<Client>,
    tx: Sender<Event>,
    stage: Rc<Stage>,
    /// The live payload budget every tile's feed draws from.
    budget: Arc<LiveBudget>,
    /// The TLS trust the API client and every `wss` feed share.
    tls: Arc<ClientConfig>,
    /// The cameras as last listed, for the focused view's remove button.
    cameras: Vec<Camera>,
    /// The focused view's "Remove camera" button, shown for added cameras.
    remove_button: gtk::Button,
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

/// Why a token given as `--token` is moved or flagged.
const TOKEN_ARGUMENT_WARNING: &str = "--token on the command line is visible to other local \
    users and kept in shell history; set IRISD_TOKEN instead";

/// Splits one `--token VALUE` or `--token=VALUE` out of the arguments after the
/// program name, returning the remaining arguments and the value. A repeated or
/// valueless option, or one after `--`, returns `None` so clap reports it.
fn take_token_argument(args: &[OsString]) -> Option<(Vec<OsString>, OsString)> {
    use std::os::unix::ffi::OsStrExt as _;

    let mut found: Option<(usize, usize, OsString)> = None;
    let mut index = 0;
    while index < args.len() {
        let arg = args[index].as_bytes();
        if arg == b"--" {
            break;
        }
        let (taken, value) = if arg == b"--token" {
            let value = args.get(index + 1)?;
            if value.as_bytes().starts_with(b"-") {
                return None;
            }
            (2, value.clone())
        } else if let Some(value) = arg.strip_prefix(b"--token=") {
            (1, OsString::from(OsStr::from_bytes(value)))
        } else {
            index += 1;
            continue;
        };
        if found.is_some() {
            return None;
        }
        found = Some((index, taken, value));
        index += taken;
    }
    let (start, taken, value) = found?;
    let mut rest = args[..start].to_vec();
    rest.extend_from_slice(&args[start + taken..]);
    Some((rest, value))
}

/// Refuses API URLs the viewer must not use: schemes other than `http` and
/// `https`, userinfo, a path, query, or fragment (the client joins absolute
/// paths to the origin, so a path prefix would be dropped silently and the
/// bearer sent elsewhere on the host), and plain HTTP to a host other than
/// loopback unless `allow_insecure_http` accepts it.
fn check_api_url(api: &Url, allow_insecure_http: bool) -> anyhow::Result<()> {
    anyhow::ensure!(
        matches!(api.scheme(), "http" | "https"),
        "the API URL must use http or https"
    );
    anyhow::ensure!(
        api.username().is_empty() && api.password().is_none(),
        "the API URL cannot carry userinfo; pass the bearer token in IRISD_TOKEN"
    );
    anyhow::ensure!(
        api.path() == "/" && api.query().is_none() && api.fragment().is_none(),
        "the API URL must name only the daemon's origin, such as https://nvr.example:8443; \
         a path, query, or fragment is not supported"
    );
    let loopback = match api.host() {
        Some(Host::Ipv4(address)) => address.is_loopback(),
        Some(Host::Ipv6(address)) => address.is_loopback(),
        Some(Host::Domain(name)) => name.eq_ignore_ascii_case("localhost"),
        None => anyhow::bail!("the API URL has no host"),
    };
    anyhow::ensure!(
        api.scheme() == "https" || loopback || allow_insecure_http,
        "refusing plain HTTP to {}: the bearer token and video would cross the network \
         unencrypted; use an https URL through a TLS proxy, or pass --allow-insecure-http",
        api.host_str().unwrap_or_default()
    );
    Ok(())
}

/// Re-executes the viewer with `GSK_RENDERER` set when nothing set it. GTK reads
/// the variable before any API runs, and setting it in-process is unsafe, so
/// the process replaces itself once; on failure the viewer runs as it is. A
/// `--token` argument moves into the replacement's `IRISD_TOKEN`, out of its
/// command line.
fn select_renderer() {
    use std::os::unix::process::CommandExt as _;
    if std::env::var_os("GSK_RENDERER").is_some() {
        return;
    }
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    let mut args: Vec<OsString> = std::env::args_os().skip(1).collect();
    let mut command = std::process::Command::new(exe);
    if let Some((rest, token)) = take_token_argument(&args) {
        eprintln!("warning: {TOKEN_ARGUMENT_WARNING}");
        command.env("IRISD_TOKEN", token);
        args = rest;
    }
    let error = command.args(args).env("GSK_RENDERER", RENDERER).exec();
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
    if take_token_argument(&std::env::args_os().skip(1).collect::<Vec<_>>()).is_some() {
        warn!("{TOKEN_ARGUMENT_WARNING}");
    }
    if let Err(error) = check_api_url(&args.api, args.allow_insecure_http) {
        error!("{error}");
        return glib::ExitCode::FAILURE;
    }
    let tls = match iris_tls::client_config(args.ca_file.as_deref()) {
        Ok(tls) => tls,
        Err(error) => {
            error!(error = %format!("{error:#}"), "cannot build the TLS trust configuration");
            return glib::ExitCode::FAILURE;
        }
    };
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
        build_window(app, &args, &runtime, &thumbnail_decoders, &tls);
    });
    app.run_with_args::<&str>(&[])
}

fn build_window(
    app: &gtk::Application,
    args: &Args,
    runtime: &Arc<Runtime>,
    thumbnail_decoders: &Arc<Semaphore>,
    tls: &Arc<ClientConfig>,
) {
    let (tx, rx) = async_channel::unbounded::<Event>();
    let (column, stack, grid, focus_box, focus_title, status, remove_button) = build_widgets(&tx);
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
            error!(error = %format!("{error:#}"), "cannot start the stage pipeline");
            app.quit();
            return;
        }
    };
    let client = Arc::new(Client::new(args.api.clone(), args.token.clone(), tls));
    fetch_cameras(runtime, &client, &tx, Event::Cameras);

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
        budget: LiveBudget::new(u64::from(args.live_budget_mib) << 20),
        tls: Arc::clone(tls),
        cameras: Vec::new(),
        remove_button,
    }));
    glib::spawn_future_local(run_events(views, args.clone(), rx));
}

/// The view the window opens once the cameras are listed.
#[derive(Clone, Debug, PartialEq, Eq)]
enum StartView {
    /// The grid: no `--focus`.
    Grid,
    /// `--focus` names a camera the daemon does not list; the grid opens.
    Unknown(CameraId),
    /// The camera's live view.
    Focus(CameraId),
    /// The camera's playback view (`--playback`).
    Playback(CameraId),
}

/// The view `--focus` and `--playback` ask for, given the listed cameras.
fn start_view(cameras: &[Camera], focus: Option<&CameraId>, playback: bool) -> StartView {
    match focus {
        None => StartView::Grid,
        Some(camera) if !cameras.iter().any(|listed| &listed.id == camera) => {
            StartView::Unknown(camera.clone())
        }
        Some(camera) if playback => StartView::Playback(camera.clone()),
        Some(camera) => StartView::Focus(camera.clone()),
    }
}

/// Receives UI events without awaiting playback I/O. Playback owns a separate
/// cancellable task, so navigation is always able to reach [`leave_views`].
async fn run_events(views: Rc<RefCell<Views>>, args: Args, rx: Receiver<Event>) {
    while let Ok(event) = rx.recv().await {
        match event {
            Event::Cameras(cameras) => {
                let open_playback_for = {
                    let mut v = views.borrow_mut();
                    v.status.set_label(&format!("{} cameras", cameras.len()));
                    v.cameras.clone_from(&cameras);
                    reset_grid(&mut v);
                    populate_grid(&mut v, &cameras, &args);
                    match start_view(&cameras, args.focus.as_ref(), args.playback) {
                        StartView::Grid => {
                            show_grid(&mut v, &args);
                            None
                        }
                        StartView::Unknown(camera) => {
                            show_grid(&mut v, &args);
                            v.status.set_label(&format!("unknown camera {camera}"));
                            None
                        }
                        StartView::Focus(camera) => {
                            focus(&mut v, &camera, &args);
                            None
                        }
                        StartView::Playback(camera) => Some(camera),
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
                    .map(|tile| tile.camera().clone());
                if let Some(camera) = camera {
                    open_playback(&views, &camera, &args);
                }
            }
            Event::Refreshed(cameras) => {
                let mut v = views.borrow_mut();
                v.status.set_label(&format!("{} cameras", cameras.len()));
                v.cameras.clone_from(&cameras);
                reset_grid(&mut v);
                populate_grid(&mut v, &cameras, &args);
                show_grid(&mut v, &args);
            }
            Event::Refresh => {
                let v = views.borrow();
                fetch_cameras(&v.runtime, &v.client, &v.tx, Event::Refreshed);
            }
            Event::AddCamera => open_add_camera_dialog(&views),
            Event::RemoveFocused => confirm_remove_focused(&views),
            Event::Grid => show_grid(&mut views.borrow_mut(), &args),
        }
    }
}

/// Lists the cameras in the background and hands the list to the main loop as
/// `listed(cameras)`: [`Event::Cameras`] at start, [`Event::Refreshed`] after a
/// change, which rebuilds the grid.
fn fetch_cameras(
    runtime: &Runtime,
    client: &Arc<Client>,
    tx: &Sender<Event>,
    listed: fn(Vec<Camera>) -> Event,
) {
    runtime.spawn({
        let tx = tx.clone();
        let client = Arc::clone(client);
        async move {
            let event = match client.cameras().await {
                Ok(cameras) => listed(cameras),
                Err(error) => Event::Status(format!("{error:#}")),
            };
            let _ = tx.send(event).await;
        }
    });
}

/// Stops every tile and takes it out of the grid, ready for a fresh list.
fn reset_grid(views: &mut Views) {
    for tile in views.tiles.drain(..) {
        tile.stop();
        views.grid.remove(tile.widget());
    }
}

/// The add-camera form as typed; it holds the password, so it has no `Debug`.
#[derive(Default)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "each flag is one independent check box of the form"
)]
struct CameraForm {
    id: String,
    name: String,
    rtsp_url: String,
    sub_url: String,
    username: String,
    password: String,
    record_main: bool,
    record_sub: bool,
    dahua_cgi: bool,
    onvif_pull_point: bool,
}

/// Turns the form into the request the daemon validates fully, or names the
/// first problem for the form. Whitespace around the typed values is dropped,
/// except in the password; the stream URLs carry no credentials.
fn camera_from_form(form: &CameraForm) -> Result<NewCamera, String> {
    let required = |value: &str, what: &str| -> Result<String, String> {
        let value = value.trim();
        if value.is_empty() {
            Err(format!("{what} is required"))
        } else {
            Ok(value.to_owned())
        }
    };
    let id = CameraId::try_new(&required(&form.id, "the camera id")?).map_err(|_| {
        "the camera id must be 1 to 32 lowercase letters, digits, - and _".to_owned()
    })?;
    let name = required(&form.name, "the camera name")?;
    let rtsp_url = required(&form.rtsp_url, "the RTSP URL")?;
    if let Some(problem) = api::rtsp_url_problem(&rtsp_url) {
        return Err(format!("the RTSP URL {problem}"));
    }
    let username = required(&form.username, "the username")?;
    let sub_url = Some(form.sub_url.trim())
        .filter(|url| !url.is_empty())
        .map(str::to_owned);
    if let Some(problem) = sub_url.as_deref().and_then(api::rtsp_url_problem) {
        return Err(format!("the sub URL {problem}"));
    }
    let mut record = Vec::new();
    if form.record_main {
        record.push("main".to_owned());
    }
    if form.record_sub {
        record.push("sub".to_owned());
    }
    if record.is_empty() {
        return Err("record at least one stream".to_owned());
    }
    let mut events = Vec::new();
    if form.dahua_cgi {
        events.push("dahua_cgi".to_owned());
    }
    if form.onvif_pull_point {
        events.push("onvif_pull_point".to_owned());
    }
    Ok(NewCamera {
        id,
        name,
        rtsp_url,
        sub_url,
        username,
        password: form.password.clone(),
        record,
        events,
    })
}

/// The main window, for transient dialogs.
fn parent_window(views: &Views) -> Option<gtk::Window> {
    views
        .stack
        .root()
        .and_then(|root| root.downcast::<gtk::Window>().ok())
}

/// A modal dialog window owned by the main window.
fn dialog_window(title: &str, parent: Option<&gtk::Window>, width: i32) -> gtk::Window {
    let dialog = gtk::Window::builder()
        .title(title)
        .modal(true)
        .default_width(width)
        .build();
    dialog.set_transient_for(parent);
    dialog
}

/// A right-aligned row holding a "Cancel" button that closes `dialog` and the
/// action button, which is returned for its handler. The buttons hold the
/// dialog weakly, so closing it releases it.
fn dialog_buttons(dialog: &gtk::Window, action: &str) -> (gtk::Box, gtk::Button) {
    let buttons = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(8)
        .halign(gtk::Align::End)
        .margin_bottom(12)
        .margin_end(12)
        .build();
    let cancel = gtk::Button::with_label("Cancel");
    cancel.connect_clicked({
        let dialog = dialog.downgrade();
        move |_| {
            if let Some(dialog) = dialog.upgrade() {
                dialog.close();
            }
        }
    });
    let action = gtk::Button::with_label(action);
    buttons.append(&cancel);
    buttons.append(&action);
    (buttons, action)
}

/// How long the Add camera form's discovery collects answers.
const DISCOVERY_WAIT: Duration = Duration::from_secs(3);

/// The add-camera form's widgets.
struct FormWidgets {
    grid: gtk::Grid,
    /// The cameras the last discovery found, in the order of `devices`.
    found: RefCell<Vec<DiscoveredDevice>>,
    /// The discovered cameras to pick from.
    devices: gtk::DropDown,
    /// Runs a discovery.
    discover: gtk::Button,
    /// Whether the stream lookup reaches the device service over HTTPS.
    https: gtk::CheckButton,
    /// The device service's port.
    port: gtk::SpinButton,
    /// Whether an HTTPS lookup verifies the camera's certificate.
    tls_verify: gtk::CheckButton,
    /// Asks the chosen camera for its stream URLs.
    fetch_streams: gtk::Button,
    /// Progress and problems of the lookups and of the typed values.
    notice: gtk::Label,
    id: gtk::Entry,
    name: gtk::Entry,
    rtsp_url: gtk::Entry,
    sub_url: gtk::Entry,
    username: gtk::Entry,
    password: gtk::PasswordEntry,
    record_main: gtk::CheckButton,
    record_sub: gtk::CheckButton,
    dahua_cgi: gtk::CheckButton,
    onvif_pull_point: gtk::CheckButton,
}

impl FormWidgets {
    fn build() -> Self {
        let grid = gtk::Grid::builder()
            .row_spacing(6)
            .column_spacing(8)
            .margin_top(12)
            .margin_bottom(12)
            .margin_start(12)
            .margin_end(12)
            .build();
        let label = |row: i32, text: &str| {
            let label = gtk::Label::builder().label(text).xalign(1.0).build();
            grid.attach(&label, 0, row, 1, 1);
        };
        let devices = gtk::DropDown::from_strings(&[]);
        devices.set_hexpand(true);
        let discover = gtk::Button::with_label("Discover");
        let discovered = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        discovered.append(&devices);
        discovered.append(&discover);
        label(0, "Discovered");
        grid.attach(&discovered, 1, 0, 1, 1);
        let entry = |row: i32, text: &str, placeholder: &str| -> gtk::Entry {
            label(row, text);
            let entry = gtk::Entry::builder()
                .hexpand(true)
                .placeholder_text(placeholder)
                .build();
            grid.attach(&entry, 1, row, 1, 1);
            entry
        };
        let id = entry(1, "Id", "lowercase letters, digits, - and _");
        let name = entry(2, "Name", "Garage");
        let rtsp_url = entry(3, "RTSP URL", "rtsp://192.0.2.10:554/...");
        let sub_url = entry(
            4,
            "Sub URL (optional)",
            "derived from a Dahua-style URL when empty",
        );
        let username = entry(5, "Username", "");
        label(6, "Password");
        let password = gtk::PasswordEntry::builder()
            .hexpand(true)
            .show_peek_icon(true)
            .build();
        grid.attach(&password, 1, 6, 1, 1);
        label(7, "ONVIF service");
        let https = gtk::CheckButton::with_label("HTTPS");
        let port = gtk::SpinButton::with_range(1.0, f64::from(u16::MAX), 1.0);
        port.set_value(80.0);
        let tls_verify = gtk::CheckButton::with_label("Verify the certificate");
        tls_verify.set_active(true);
        tls_verify.set_sensitive(false);
        let service = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        service.append(&https);
        service.append(&port);
        service.append(&tls_verify);
        grid.attach(&service, 1, 7, 1, 1);
        let fetch_streams = gtk::Button::with_label("Fetch streams via ONVIF");
        grid.attach(&fetch_streams, 1, 8, 1, 1);
        let record_main = gtk::CheckButton::with_label("Record the main stream");
        record_main.set_active(true);
        let record_sub = gtk::CheckButton::with_label("Record the sub stream");
        record_sub.set_active(true);
        let dahua_cgi = gtk::CheckButton::with_label("Dahua CGI events");
        let onvif_pull_point = gtk::CheckButton::with_label("ONVIF pull-point events");
        for (row, check) in (9..).zip([&record_main, &record_sub, &dahua_cgi, &onvif_pull_point]) {
            grid.attach(check, 1, row, 1, 1);
        }
        let notice = gtk::Label::builder().xalign(0.0).wrap(true).build();
        grid.attach(&notice, 0, 13, 2, 1);
        Self {
            grid,
            found: RefCell::new(Vec::new()),
            devices,
            discover,
            https,
            port,
            tls_verify,
            fetch_streams,
            notice,
            id,
            name,
            rtsp_url,
            sub_url,
            username,
            password,
            record_main,
            record_sub,
            dahua_cgi,
            onvif_pull_point,
        }
    }

    fn values(&self) -> CameraForm {
        CameraForm {
            id: self.id.text().to_string(),
            name: self.name.text().to_string(),
            rtsp_url: self.rtsp_url.text().to_string(),
            sub_url: self.sub_url.text().to_string(),
            username: self.username.text().to_string(),
            password: self.password.text().to_string(),
            record_main: self.record_main.is_active(),
            record_sub: self.record_sub.is_active(),
            dahua_cgi: self.dahua_cgi.is_active(),
            onvif_pull_point: self.onvif_pull_point.is_active(),
        }
    }

    /// The lookup "Fetch streams" sends, or the first problem.
    fn stream_request(&self) -> Result<StreamUrlsRequest, String> {
        stream_request(&LookupForm {
            device: self.selected_device(),
            rtsp_url: self.rtsp_url.text().to_string(),
            port: u16::try_from(self.port.value_as_int()).unwrap_or(0),
            https: self.https.is_active(),
            tls_verify: self.tls_verify.is_active(),
            username: self.username.text().to_string(),
            password: self.password.text().to_string(),
        })
    }

    /// Shows the discovered cameras and selects the first.
    fn show_devices(&self, devices: Vec<DiscoveredDevice>) {
        let labels: Vec<String> = devices.iter().map(device_label).collect();
        let labels: Vec<&str> = labels.iter().map(String::as_str).collect();
        self.notice.set_label(&match devices.len() {
            0 => "no camera answered the probe".to_owned(),
            found => format!(
                "{found} camera(s) found; pick one, enter its credentials, then fetch its streams"
            ),
        });
        // The list is in place before the model changes, since the selection
        // notification that follows reads it.
        *self.found.borrow_mut() = devices;
        self.devices.set_model(Some(&gtk::StringList::new(&labels)));
        self.apply_selected_device();
    }

    /// The camera picked in the dropdown, if any.
    fn selected_device(&self) -> Option<DiscoveredDevice> {
        let index = usize::try_from(self.devices.selected()).ok()?;
        self.found.borrow().get(index).cloned()
    }

    /// Fills the service controls and an empty name from the picked camera.
    fn apply_selected_device(&self) {
        let Some(device) = self.selected_device() else {
            return;
        };
        // The HTTPS box first: its handler moves a default port, and the
        // device's own port then wins.
        self.https.set_active(device.https);
        self.port.set_value(f64::from(device.port));
        if self.name.text().is_empty()
            && let Some(name) = &device.name
        {
            self.name.set_text(name);
        }
    }

    /// Fills the URLs from the camera's profiles and selects the ONVIF event
    /// source in place of the Dahua CGI one, since the camera answered ONVIF.
    fn apply_streams(&self, streams: &[ProfileStream]) {
        let Some((main, sub)) = stream_fill(streams) else {
            return;
        };
        self.rtsp_url.set_text(&main);
        self.sub_url.set_text(sub.as_deref().unwrap_or(""));
        self.onvif_pull_point.set_active(true);
        self.dahua_cgi.set_active(false);
        self.notice.set_label(&format!(
            "{} stream(s) from the camera's profiles",
            streams.len()
        ));
    }
}

/// The dropdown label of a discovered camera: its name or hardware, then its
/// device service.
fn device_label(device: &DiscoveredDevice) -> String {
    let what = device
        .name
        .as_deref()
        .or(device.hardware.as_deref())
        .unwrap_or("camera");
    let scheme = if device.https { "https" } else { "http" };
    let host = if device.host.contains(':') {
        format!("[{}]", device.host)
    } else {
        device.host.clone()
    };
    format!("{what} ({scheme}://{host}:{})", device.port)
}

/// The stream lookup's part of the form as typed; it holds the password, so it
/// has no `Debug`.
struct LookupForm {
    /// The camera picked in the Discovered list.
    device: Option<DiscoveredDevice>,
    rtsp_url: String,
    port: u16,
    https: bool,
    tls_verify: bool,
    username: String,
    password: String,
}

/// The lookup for the picked camera, or for the host of the typed RTSP URL
/// when nothing was picked, at the form's port and scheme; needs a username.
fn stream_request(form: &LookupForm) -> Result<StreamUrlsRequest, String> {
    let username = form.username.trim();
    if username.is_empty() {
        return Err("enter the camera's username first".to_owned());
    }
    if form.port == 0 {
        return Err("the ONVIF port must be 1 to 65535".to_owned());
    }
    let host = if let Some(device) = &form.device {
        device.host.clone()
    } else {
        let url = Url::parse(form.rtsp_url.trim()).map_err(|_| {
            "pick a discovered camera or type the camera's RTSP URL first".to_owned()
        })?;
        match url.host() {
            Some(Host::Domain(domain)) if !domain.is_empty() => domain.to_owned(),
            Some(Host::Ipv4(address)) => address.to_string(),
            Some(Host::Ipv6(address)) => address.to_string(),
            _ => return Err("the RTSP URL names no host".to_owned()),
        }
    };
    Ok(StreamUrlsRequest {
        host,
        http_port: form.port,
        https: form.https,
        tls_verify: form.tls_verify,
        username: username.to_owned(),
        password: form.password.clone(),
    })
}

/// The device service port after the HTTPS box changes: a port at the other
/// scheme's default follows the scheme, and any other port stays.
fn port_for_scheme(port: u16, https: bool) -> u16 {
    match (port, https) {
        (80, true) => 443,
        (443, false) => 80,
        (port, _) => port,
    }
}

/// The main and sub URLs the profiles fill in: the first and the second.
fn stream_fill(streams: &[ProfileStream]) -> Option<(String, Option<String>)> {
    let main = streams.first()?.url.clone();
    Some((main, streams.get(1).map(|stream| stream.url.clone())))
}

/// Connects the form's Discover button, device list, HTTPS box, and "Fetch
/// streams" button: each lookup runs on the runtime and its answer applies on
/// the main loop. The handlers hold the form weakly, so the dialog that owns
/// the form releases it when it closes.
fn wire_lookups(form: &Rc<FormWidgets>, client: &Arc<Client>, runtime: &Arc<Runtime>) {
    form.discover.connect_clicked({
        let form = Rc::downgrade(form);
        let client = Arc::clone(client);
        let runtime = Arc::clone(runtime);
        move |button| {
            let Some(shown) = form.upgrade() else {
                return;
            };
            button.set_sensitive(false);
            shown.notice.set_label(&format!(
                "probing the network for {} s",
                DISCOVERY_WAIT.as_secs()
            ));
            let (reply, answer) = oneshot::channel();
            runtime.spawn({
                let client = Arc::clone(&client);
                async move {
                    let _ = reply.send(client.discover(DISCOVERY_WAIT).await);
                }
            });
            glib::spawn_future_local({
                let form = form.clone();
                let button = button.downgrade();
                async move {
                    let answer = answer.await;
                    let (Some(form), Some(button)) = (form.upgrade(), button.upgrade()) else {
                        return;
                    };
                    match answer {
                        Ok(Ok(devices)) => form.show_devices(devices),
                        Ok(Err(error)) => form.notice.set_label(&format!("{error:#}")),
                        Err(_) => form.notice.set_label("discovery was abandoned"),
                    }
                    button.set_sensitive(true);
                }
            });
        }
    });
    form.devices.connect_selected_notify({
        let form = Rc::downgrade(form);
        move |_| {
            if let Some(form) = form.upgrade() {
                form.apply_selected_device();
            }
        }
    });
    form.https.connect_toggled({
        let form = Rc::downgrade(form);
        move |https| {
            let Some(form) = form.upgrade() else {
                return;
            };
            let port = u16::try_from(form.port.value_as_int()).unwrap_or(0);
            form.port
                .set_value(f64::from(port_for_scheme(port, https.is_active())));
            form.tls_verify.set_sensitive(https.is_active());
        }
    });
    form.fetch_streams.connect_clicked({
        let form = Rc::downgrade(form);
        let client = Arc::clone(client);
        let runtime = Arc::clone(runtime);
        move |button| {
            let Some(shown) = form.upgrade() else {
                return;
            };
            let request = match shown.stream_request() {
                Ok(request) => request,
                Err(problem) => {
                    shown.notice.set_label(&problem);
                    return;
                }
            };
            button.set_sensitive(false);
            shown.notice.set_label("asking the camera for its streams");
            let (reply, answer) = oneshot::channel();
            runtime.spawn({
                let client = Arc::clone(&client);
                async move {
                    let _ = reply.send(client.stream_urls(&request).await);
                }
            });
            glib::spawn_future_local({
                let form = form.clone();
                let button = button.downgrade();
                async move {
                    let answer = answer.await;
                    let (Some(form), Some(button)) = (form.upgrade(), button.upgrade()) else {
                        return;
                    };
                    match answer {
                        Ok(Ok(streams)) => form.apply_streams(&streams),
                        Ok(Err(error)) => form.notice.set_label(&format!("{error:#}")),
                        Err(_) => form.notice.set_label("the lookup was abandoned"),
                    }
                    button.set_sensitive(true);
                }
            });
        }
    });
}

/// Sends `POST /api/cameras` in the background, reports the outcome in the
/// status bar, and lists the cameras again on success.
fn add_camera_in_background(
    runtime: &Runtime,
    client: &Arc<Client>,
    tx: &Sender<Event>,
    camera: NewCamera,
) {
    let _ = tx.try_send(Event::Status(format!("adding camera {}", camera.id)));
    runtime.spawn({
        let client = Arc::clone(client);
        let tx = tx.clone();
        async move {
            match client.add_camera(&camera).await {
                Ok(added) => {
                    let _ = tx
                        .send(Event::Status(format!(
                            "added camera {} ({})",
                            added.name, added.id
                        )))
                        .await;
                    let _ = tx.send(Event::Refresh).await;
                }
                Err(error) => {
                    let _ = tx.send(Event::Status(format!("{error:#}"))).await;
                }
            }
        }
    });
}

/// Sends `DELETE /api/cameras/{camera}` in the background, reports the
/// outcome in the status bar, and lists the cameras again on success.
fn remove_camera_in_background(
    runtime: &Runtime,
    client: &Arc<Client>,
    tx: &Sender<Event>,
    camera: CameraId,
) {
    let _ = tx.try_send(Event::Status(format!("removing camera {camera}")));
    runtime.spawn({
        let client = Arc::clone(client);
        let tx = tx.clone();
        async move {
            match client.remove_camera(&camera).await {
                Ok(()) => {
                    let _ = tx
                        .send(Event::Status(format!("removed camera {camera}")))
                        .await;
                    let _ = tx.send(Event::Refresh).await;
                }
                Err(error) => {
                    let _ = tx.send(Event::Status(format!("{error:#}"))).await;
                }
            }
        }
    });
}

/// Builds the add-camera form in a dialog. "Add" closes the dialog and sends
/// `POST /api/cameras`, whose outcome shows in the status bar before the
/// cameras are listed again; a problem with the typed values shows in the form,
/// which stays open. The dialog's Add button is the form's one owner, so
/// closing the dialog releases the form; the returned reference is weak.
fn add_camera_dialog(
    client: &Arc<Client>,
    runtime: &Arc<Runtime>,
    tx: &Sender<Event>,
    parent: Option<&gtk::Window>,
) -> (gtk::Window, Weak<FormWidgets>) {
    let dialog = dialog_window("Add camera", parent, 520);
    let form = Rc::new(FormWidgets::build());
    wire_lookups(&form, client, runtime);
    let (buttons, add) = dialog_buttons(&dialog, "Add");
    let column = gtk::Box::new(gtk::Orientation::Vertical, 0);
    column.append(&form.grid);
    column.append(&buttons);
    dialog.set_child(Some(&column));
    let weak = Rc::downgrade(&form);
    add.connect_clicked({
        let dialog = dialog.downgrade();
        let client = Arc::clone(client);
        let runtime = Arc::clone(runtime);
        let tx = tx.clone();
        move |_| match camera_from_form(&form.values()) {
            Err(problem) => form.notice.set_label(&problem),
            Ok(camera) => {
                if let Some(dialog) = dialog.upgrade() {
                    dialog.close();
                }
                add_camera_in_background(&runtime, &client, &tx, camera);
            }
        }
    });
    (dialog, weak)
}

/// Opens the add-camera form.
fn open_add_camera_dialog(views: &Rc<RefCell<Views>>) {
    let (dialog, _) = {
        let v = views.borrow();
        add_camera_dialog(&v.client, &v.runtime, &v.tx, parent_window(&v).as_ref())
    };
    dialog.present();
}

/// Asks before `DELETE /api/cameras/{camera}` for the focused camera; the
/// outcome goes to the status bar, then the cameras are listed again.
fn confirm_remove_focused(views: &Rc<RefCell<Views>>) {
    let (camera, name, client, runtime, tx, parent) = {
        let v = views.borrow();
        let Some(tile) = v.focused.as_ref() else {
            return;
        };
        (
            tile.camera().clone(),
            tile.name().to_owned(),
            Arc::clone(&v.client),
            Arc::clone(&v.runtime),
            v.tx.clone(),
            parent_window(&v),
        )
    };
    let dialog = dialog_window("Remove camera", parent.as_ref(), 420);
    let text = gtk::Label::builder()
        .label(format!(
            "Stop recording {name} ({camera}) and remove it from the daemon?\n\
             Its recordings stay. A camera from the configuration file cannot be removed here."
        ))
        .wrap(true)
        .margin_top(12)
        .margin_bottom(12)
        .margin_start(12)
        .margin_end(12)
        .build();
    let (buttons, remove) = dialog_buttons(&dialog, "Remove");
    remove.connect_clicked({
        let dialog = dialog.downgrade();
        move |_| {
            if let Some(dialog) = dialog.upgrade() {
                dialog.close();
            }
            remove_camera_in_background(&runtime, &client, &tx, camera.clone());
        }
    });
    let column = gtk::Box::new(gtk::Orientation::Vertical, 0);
    column.append(&text);
    column.append(&buttons);
    dialog.set_child(Some(&column));
    dialog.present();
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
fn open_playback(views: &Rc<RefCell<Views>>, camera: &CameraId, args: &Args) {
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
            let camera = camera.clone();
            move |_| {
                let _ = tx.try_send(Event::Focus(camera.clone()));
            }
        });
        let playback = Playback::new(
            camera,
            args.focus_kind,
            args.kind,
            &[back, live],
            Arc::clone(&v.thumbnail_decoders),
        );
        v.stack.add_named(playback.widget(), Some("playback"));
        v.stack.set_visible_child_name("playback");
        v.playback = Some(Rc::clone(&playback));
        (playback, Arc::clone(&v.client), Arc::clone(&v.runtime))
    };
    let to = WallUs::new(glib::real_time());
    let from = to.offset(-i64::from(args.window) * MICROS_PER_MINUTE);
    info!(camera = %camera, kind = %args.focus_kind, minutes = args.window, "playback opened");
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
    tx: &Sender<Event>,
) -> (
    gtk::Box,
    gtk::Stack,
    gtk::Grid,
    gtk::Box,
    gtk::Label,
    gtk::Label,
    gtk::Button,
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
    let button = |label: &str, event: fn() -> Event| {
        let button = gtk::Button::with_label(label);
        button.connect_clicked({
            let tx = tx.clone();
            move |_| {
                let _ = tx.try_send(event());
            }
        });
        button
    };
    let back = button("Back to grid", || Event::Grid);
    let playback = button("Playback", || Event::PlaybackFocused);
    let remove = button("Remove camera", || Event::RemoveFocused);
    let focus_title = gtk::Label::builder().xalign(0.0).hexpand(true).build();
    focus_header.append(&back);
    focus_header.append(&playback);
    focus_header.append(&remove);
    focus_header.append(&focus_title);
    focus_box.append(&focus_header);
    let grid_header = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    grid_header.append(&button("Add camera", || Event::AddCamera));
    let grid_box = gtk::Box::new(gtk::Orientation::Vertical, 4);
    grid_box.append(&grid_header);
    grid_box.append(&grid);
    let stack = gtk::Stack::new();
    stack.add_named(&grid_box, Some("grid"));
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
    (column, stack, grid, focus_box, focus_title, status, remove)
}

fn feed(args: &Args, tls: &Arc<ClientConfig>, camera: &CameraId, kind: StreamKind) -> Feed {
    Feed {
        api: args.api.clone(),
        token: args.token.clone(),
        camera: camera.clone(),
        kind,
        tls: Arc::clone(tls),
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
            Arc::clone(&views.budget),
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

/// Shows the grid and starts the feeds of the first `--max-tiles` tiles; the
/// others say why they are idle and still focus their camera when clicked.
fn show_grid(views: &mut Views, args: &Args) {
    leave_views(views);
    let limit = usize::try_from(args.max_tiles.get()).unwrap_or(usize::MAX);
    for (index, tile) in views.tiles.iter().enumerate() {
        if index < limit {
            if let Err(error) = tile.start(feed(args, &views.tls, tile.camera(), args.kind)) {
                error!(
                    camera = %tile.camera(),
                    error = %format!("{error:#}"),
                    "cannot start a tile"
                );
            }
        } else {
            tile.idle(&format!(
                "not started: the grid decodes at most {limit} live tiles (--max-tiles)"
            ));
        }
    }
    views.stack.set_visible_child_name("grid");
    info!(
        tiles = views.tiles.len(),
        live = views.tiles.len().min(limit),
        "grid shown"
    );
}

/// Shows `camera`'s main stream alone and stops the grid's decoders.
fn focus(views: &mut Views, camera: &CameraId, args: &Args) {
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
    let provisioned = views
        .cameras
        .iter()
        .any(|known| &known.id == camera && known.provisioned);
    views.remove_button.set_visible(provisioned);
    let tile = Tile::new(
        camera,
        &name,
        Arc::clone(&views.runtime),
        Rc::clone(&views.stage),
        Arc::clone(&views.budget),
    );
    views
        .focus_title
        .set_label(&format!("{name} ({camera}), {} stream", args.focus_kind));
    views.focus_box.append(tile.widget());
    if let Err(error) = tile.start(feed(args, &views.tls, camera, args.focus_kind)) {
        error!(camera = %camera, error = %format!("{error:#}"), "cannot start the focused tile");
    }
    views.focused = Some(tile);
    views.stack.set_visible_child_name("focus");
    info!(camera = %camera, kind = %args.focus_kind, "camera focused");
}

#[cfg(test)]
mod tests {
    use std::convert::Infallible;
    use std::io::{self, Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::pin::Pin;
    use std::sync::Mutex;
    use std::task::{Context, Poll};

    use clap::error::ErrorKind;
    use tokio_tungstenite::tungstenite::client::IntoClientRequest as _;
    use tracing_subscriber::fmt::MakeWriter;

    use super::*;

    /// The checks that need a display, on GTK's one initializer thread: the
    /// test harness gives each test its own thread even with `--test-threads=1`.
    #[test]
    #[ignore = "requires a display and GStreamer HLS plugins; run under xvfb-run"]
    fn gtk_lifecycle_checks() {
        playback::tests::dropping_playback_releases_the_view_and_button_callbacks();
        playback::tests::closing_playback_cancels_a_pending_playlist();
        playback::tests::closing_playback_cancels_bounded_thumbnail_requests();
        playback::tests::replacement_views_share_uninterruptible_decoder_permits();
        playback::tests::export_terminal_status_cannot_be_overwritten_by_progress();
        playback::tests::a_window_holding_only_a_stale_open_segment_shows_nothing_recorded();
        playback::tests::playback_error_releases_its_snapshot_listener();
        navigation_event_cancels_a_stalled_playback_request();
        grid_decodes_at_most_max_tiles_feeds();
        the_remove_button_shows_only_for_provisioned_cameras();
        closing_the_add_camera_dialog_releases_it_and_its_form();
    }

    fn camera(id: &str) -> CameraId {
        CameraId::try_new(id).unwrap()
    }

    fn listed(id: &str, provisioned: bool) -> Camera {
        Camera {
            id: camera(id),
            name: format!("Camera {id}"),
            provisioned,
        }
    }

    #[derive(Clone, Debug)]
    struct Captured(Arc<Mutex<Vec<u8>>>);

    struct CapturedWriter(Arc<Mutex<Vec<u8>>>);

    struct DropSignal(Sender<()>);

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
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.0
                .lock()
                .expect("capture mutex is not poisoned")
                .extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> io::Result<()> {
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
                .set_read_timeout(Some(Duration::from_secs(2)))
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

    fn check(url: &str, allow: bool) -> anyhow::Result<()> {
        check_api_url(&Url::parse(url).unwrap(), allow)
    }

    #[test]
    fn plain_http_to_loopback_and_https_anywhere_are_accepted() {
        for accepted in [
            "http://127.0.0.1:8554",
            "http://127.3.4.5:8554",
            "http://[::1]:8554",
            "http://localhost:8554",
            "https://192.0.2.10:8443",
            "https://nvr.example:8443/",
        ] {
            assert!(check(accepted, false).is_ok(), "{accepted}");
        }
    }

    #[test]
    fn plain_http_to_a_remote_api_needs_an_explicit_opt_in() {
        let refused = check("http://192.0.2.10:8554", false).unwrap_err();
        assert!(refused.to_string().contains("--allow-insecure-http"));
        assert!(check("http://192.0.2.10:8554", true).is_ok());
    }

    #[test]
    fn api_urls_with_another_scheme_or_userinfo_are_refused() {
        for refused in [
            "ftp://127.0.0.1/",
            "file:///example",
            "http://user:secret@127.0.0.1:8554",
            "https://user@192.0.2.10/",
        ] {
            assert!(check(refused, true).is_err(), "{refused}");
        }
    }

    #[test]
    fn api_urls_with_a_path_query_or_fragment_are_refused() {
        for refused in [
            "https://nvr.example/nvr/",
            "https://nvr.example/api",
            "https://nvr.example/?token=x",
            "https://nvr.example/#top",
        ] {
            let error = check(refused, true).unwrap_err();
            assert!(
                error.to_string().contains("only the daemon's origin"),
                "{refused}"
            );
        }
    }

    #[test]
    fn the_insecure_http_opt_in_is_off_unless_given() {
        let args = Args::try_parse_from([
            "iris-gui",
            "--api",
            "http://192.0.2.10:8554",
            "--allow-insecure-http",
        ])
        .unwrap();
        assert!(args.allow_insecure_http);
        assert!(
            !Args::try_parse_from(["iris-gui"])
                .unwrap()
                .allow_insecure_http
        );
    }

    #[test]
    fn the_arguments_redact_the_token() {
        let args = Args::try_parse_from(["iris-gui", "--token", "t0ken"]).unwrap();
        let shown = format!("{args:?}");
        assert!(
            shown.contains("<redacted>") && !shown.contains("t0ken"),
            "{shown}"
        );
    }

    fn os_args(items: &[&str]) -> Vec<OsString> {
        items.iter().map(OsString::from).collect()
    }

    #[test]
    fn a_command_line_token_moves_out_of_the_reexecuted_arguments() {
        assert_eq!(
            take_token_argument(&os_args(&["--token", "secret", "--kind", "main"])),
            Some((os_args(&["--kind", "main"]), OsString::from("secret")))
        );
        assert_eq!(
            take_token_argument(&os_args(&["--columns", "2", "--token=se=cret"])),
            Some((os_args(&["--columns", "2"]), OsString::from("se=cret")))
        );
    }

    #[test]
    fn absent_valueless_repeated_or_escaped_tokens_are_left_to_clap() {
        for unchanged in [
            &["--kind", "main"][..],
            &["--token"][..],
            &["--token", "--kind"][..],
            &["--token", "a", "--token", "b"][..],
            &["--token=a", "--token=b"][..],
            &["--", "--token", "a"][..],
        ] {
            assert_eq!(
                take_token_argument(&os_args(unchanged)),
                None,
                "{unchanged:?}"
            );
        }
    }

    #[test]
    fn stream_kinds_are_main_or_sub() {
        let args = Args::try_parse_from(["iris-gui"]).unwrap();
        assert_eq!(
            (args.kind, args.focus_kind),
            (StreamKind::Sub, StreamKind::Main)
        );
        let args =
            Args::try_parse_from(["iris-gui", "--kind", "main", "--focus-kind", "sub"]).unwrap();
        assert_eq!(
            (args.kind, args.focus_kind),
            (StreamKind::Main, StreamKind::Sub)
        );
        for refused in ["audio", "Main", "../x", ""] {
            let error = Args::try_parse_from(["iris-gui", "--kind", refused])
                .expect_err("only video kinds are accepted");
            assert_eq!(error.kind(), ErrorKind::ValueValidation, "{refused}");
        }
    }

    #[test]
    fn the_playback_window_is_one_minute_to_seven_days() {
        assert_eq!(Args::try_parse_from(["iris-gui"]).unwrap().window, 60);
        let longest = MAX_WINDOW_MINUTES.to_string();
        assert!(Args::try_parse_from(["iris-gui", "--window", &longest]).is_ok());
        for refused in ["0", "10081"] {
            let error = Args::try_parse_from(["iris-gui", "--window", refused])
                .expect_err("the window must fit the API's range");
            assert_eq!(error.kind(), ErrorKind::ValueValidation, "{refused}");
        }
    }

    #[test]
    fn focus_takes_a_camera_id() {
        let args = Args::try_parse_from(["iris-gui", "--focus", "cam1"]).unwrap();
        assert_eq!(args.focus, Some(camera("cam1")));
        let error = Args::try_parse_from(["iris-gui", "--focus", "../cam"])
            .expect_err("a focus outside the id contract is refused");
        assert_eq!(error.kind(), ErrorKind::ValueValidation);
    }

    #[test]
    fn the_start_view_opens_a_listed_focus_camera_only() {
        let cameras = [listed("cam1", false)];
        assert_eq!(start_view(&cameras, None, false), StartView::Grid);
        assert_eq!(
            start_view(&cameras, Some(&camera("cam1")), false),
            StartView::Focus(camera("cam1"))
        );
        assert_eq!(
            start_view(&cameras, Some(&camera("cam1")), true),
            StartView::Playback(camera("cam1"))
        );
        assert_eq!(
            start_view(&cameras, Some(&camera("cam9")), true),
            StartView::Unknown(camera("cam9"))
        );
    }

    fn valid_form() -> CameraForm {
        CameraForm {
            id: "cam8".into(),
            name: "Garage".into(),
            rtsp_url: "rtsp://192.0.2.10/x".into(),
            sub_url: "rtsp://192.0.2.10/y".into(),
            username: "viewer".into(),
            password: "p".into(),
            record_main: true,
            record_sub: false,
            dahua_cgi: false,
            onvif_pull_point: true,
        }
    }

    #[test]
    fn the_add_camera_form_becomes_a_trimmed_request() {
        let form = CameraForm {
            id: " cam8 ".into(),
            rtsp_url: " rtsp://192.0.2.10:554/cam/realmonitor?channel=1&subtype=0 ".into(),
            sub_url: "  ".into(),
            password: " p ass ".into(),
            record_sub: true,
            dahua_cgi: true,
            onvif_pull_point: false,
            ..valid_form()
        };
        let camera = camera_from_form(&form).unwrap();
        assert_eq!(camera.id.as_str(), "cam8");
        assert_eq!(
            camera.rtsp_url,
            "rtsp://192.0.2.10:554/cam/realmonitor?channel=1&subtype=0"
        );
        assert_eq!(
            camera.sub_url, None,
            "an empty sub URL is left to the daemon"
        );
        assert_eq!(camera.password, " p ass ", "passwords keep their spaces");
        assert_eq!(camera.record, ["main", "sub"]);
        assert_eq!(camera.events, ["dahua_cgi"]);
        let json = serde_json::to_value(&camera).unwrap();
        assert_eq!(json["id"], "cam8");
        assert!(json.get("sub_url").is_none());
    }

    #[test]
    fn the_new_camera_redacts_its_password() {
        let camera = camera_from_form(&CameraForm {
            password: "p ass".into(),
            ..valid_form()
        })
        .unwrap();
        assert!(!format!("{camera:?}").contains("p ass"), "{camera:?}");
    }

    #[test]
    fn the_add_camera_form_names_its_first_problem() {
        for (problem, mutate) in [
            (
                "the camera id is required",
                Box::new(|f: &mut CameraForm| f.id.clear()) as Box<dyn Fn(&mut CameraForm)>,
            ),
            (
                "the camera id must be 1 to 32 lowercase letters, digits, - and _",
                Box::new(|f| f.id = "Cam 8".into()),
            ),
            (
                "the RTSP URL must start with rtsp://",
                Box::new(|f| f.rtsp_url = "http://x/".into()),
            ),
            (
                "the RTSP URL cannot carry credentials; enter them as the username and password",
                Box::new(|f| f.rtsp_url = "rtsp://viewer:secret@192.0.2.10/x".into()),
            ),
            (
                "the sub URL names no host",
                Box::new(|f| f.sub_url = "rtsp:y".into()),
            ),
            (
                "record at least one stream",
                Box::new(|f| {
                    f.record_main = false;
                    f.record_sub = false;
                }),
            ),
            (
                "the username is required",
                Box::new(|f| f.username = " ".into()),
            ),
        ] {
            let mut broken = valid_form();
            mutate(&mut broken);
            assert_eq!(camera_from_form(&broken).unwrap_err(), problem);
        }
    }

    #[test]
    fn the_grid_decodes_sixteen_tiles_by_default() {
        assert_eq!(
            Args::try_parse_from(["iris-gui"]).unwrap().max_tiles.get(),
            16
        );
    }

    #[test]
    fn max_tiles_takes_a_positive_count() {
        assert_eq!(
            Args::try_parse_from(["iris-gui", "--max-tiles", "4"])
                .unwrap()
                .max_tiles
                .get(),
            4
        );
        let error = Args::try_parse_from(["iris-gui", "--max-tiles", "0"])
            .expect_err("zero live tiles must be rejected");
        assert_eq!(error.kind(), ErrorKind::ValueValidation);
    }

    #[test]
    fn the_live_budget_defaults_to_sixteen_full_queues() {
        assert_eq!(
            Args::try_parse_from(["iris-gui"]).unwrap().live_budget_mib,
            256
        );
    }

    #[test]
    fn a_live_budget_below_one_full_queue_is_refused() {
        assert_eq!(
            Args::try_parse_from(["iris-gui", "--live-budget-mib", "16"])
                .unwrap()
                .live_budget_mib,
            16
        );
        let error = Args::try_parse_from(["iris-gui", "--live-budget-mib", "15"])
            .expect_err("a budget below one source queue must be rejected");
        assert_eq!(error.kind(), ErrorKind::ValueValidation);
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

    /// Views over fresh widgets whose client and feeds talk to `api`.
    fn test_views(runtime: &Arc<Runtime>, api: &Url, tx: Sender<Event>) -> Views {
        let (_, stack, grid, focus_box, focus_title, status, remove_button) = build_widgets(&tx);
        Views {
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
            runtime: Arc::clone(runtime),
            client: Arc::new(Client::for_tests(api.clone(), None)),
            tx,
            stage: Stage::new().unwrap(),
            budget: LiveBudget::new(u64::from(DEFAULT_LIVE_BUDGET_MIB) << 20),
            tls: iris_tls::client_config(None).unwrap(),
            cameras: Vec::new(),
            remove_button,
        }
    }

    fn init_gtk_and_gstreamer() {
        gtk::init().unwrap();
        gst::init().unwrap();
        // Registration may already have happened in this process.
        let _ = gstgtk4::plugin_register_static();
    }

    /// Under a display: the grid starts feeds for the first `--max-tiles`
    /// tiles only and labels the rest.
    fn grid_decodes_at_most_max_tiles_feeds() {
        init_gtk_and_gstreamer();
        let runtime = Arc::new(Runtime::new().unwrap());
        // Nothing listens on the discard port, so the feeds only retry.
        let api = Url::parse("http://127.0.0.1:9").unwrap();
        let (tx, _rx) = async_channel::unbounded();
        let mut views = test_views(&runtime, &api, tx);
        let mut args = Args::try_parse_from(["iris-gui", "--max-tiles", "2"]).unwrap();
        args.api = api;
        let cameras = ["cam1", "cam2", "cam3"].map(|id| listed(id, false));
        populate_grid(&mut views, &cameras, &args);
        show_grid(&mut views, &args);
        assert!(views.tiles[0].is_running() && views.tiles[1].is_running());
        assert!(!views.tiles[2].is_running());
        assert!(
            views.tiles[2].label_text().contains("--max-tiles"),
            "{}",
            views.tiles[2].label_text()
        );
        leave_views(&mut views);
        assert!(views.tiles.iter().all(|tile| !tile.is_running()));
    }

    /// Under a display: the focused view offers "Remove camera" for a camera
    /// the API added and hides it for one from the configuration file.
    fn the_remove_button_shows_only_for_provisioned_cameras() {
        init_gtk_and_gstreamer();
        let runtime = Arc::new(Runtime::new().unwrap());
        let api = Url::parse("http://127.0.0.1:9").unwrap();
        let (tx, _rx) = async_channel::unbounded();
        let mut views = test_views(&runtime, &api, tx);
        let mut args = Args::try_parse_from(["iris-gui"]).unwrap();
        args.api = api;
        let cameras = [listed("added", true), listed("configured", false)];
        views.cameras = cameras.to_vec();
        populate_grid(&mut views, &cameras, &args);
        focus(&mut views, &camera("added"), &args);
        assert!(views.remove_button.is_visible());
        focus(&mut views, &camera("configured"), &args);
        assert!(!views.remove_button.is_visible());
        leave_views(&mut views);
    }

    /// Under a display: closing the Add camera dialog releases the dialog and
    /// its form, whose callbacks hold them weakly.
    fn closing_the_add_camera_dialog_releases_it_and_its_form() {
        gtk::init().unwrap();
        let runtime = Arc::new(Runtime::new().unwrap());
        let client = Arc::new(Client::for_tests(
            Url::parse("http://127.0.0.1:9").unwrap(),
            None,
        ));
        let (tx, _rx) = async_channel::unbounded();
        let (dialog, form) = add_camera_dialog(&client, &runtime, &tx, None);
        dialog.present();
        let window = dialog.downgrade();
        dialog.close();
        drop(dialog);
        let context = glib::MainContext::default();
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while (window.upgrade().is_some() || form.upgrade().is_some())
            && std::time::Instant::now() < deadline
        {
            while context.iteration(false) {}
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(
            window.upgrade().is_none(),
            "a closed dialog must be released"
        );
        assert!(
            form.upgrade().is_none(),
            "a closed dialog must release its form"
        );
    }

    fn navigation_event_cancels_a_stalled_playback_request() {
        gtk::init().unwrap();
        gst::init().unwrap();
        gstgtk4::plugin_register_static().unwrap();
        let runtime = Arc::new(Runtime::new().unwrap());
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
        let api = Url::parse(&format!("http://{address}")).unwrap();
        let (tx, rx) = async_channel::unbounded();
        let views = Rc::new(RefCell::new(test_views(&runtime, &api, tx.clone())));
        let args = Args::try_parse_from([
            "iris-gui",
            "--api",
            api.as_str(),
            "--columns",
            "1",
            "--focus",
            "fixture",
            "--playback",
            "--window",
            "1",
        ])
        .unwrap();
        let event_task = glib::spawn_future_local(run_events(Rc::clone(&views), args, rx));
        glib::MainContext::default().block_on(async {
            tx.send(Event::Cameras(vec![listed("fixture", false)]))
                .await
                .unwrap();
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
        leave_views(&mut views.borrow_mut());
        server.abort();
    }

    async fn receive_before_timeout(rx: &Receiver<()>, failure: &str) {
        let received = rx.recv();
        let timeout = glib::timeout_future(Duration::from_secs(2));
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
                glib::timeout_future(Duration::from_millis(5)).await;
            }
        };
        let timeout = glib::timeout_future(Duration::from_secs(2));
        futures::pin_mut!(transitioned, timeout);
        assert!(
            matches!(
                futures::future::select(transitioned, timeout).await,
                futures::future::Either::Left(_)
            ),
            "the real navigation event must return to the grid"
        );
    }

    fn device(name: Option<&str>, host: &str, https: bool) -> DiscoveredDevice {
        DiscoveredDevice {
            endpoint: "urn:uuid:1".into(),
            host: host.into(),
            port: if https { 443 } else { 8080 },
            https,
            name: name.map(str::to_owned),
            hardware: Some("IPC".into()),
            location: None,
        }
    }

    #[test]
    fn device_labels_name_the_camera_and_its_service() {
        assert_eq!(
            device_label(&device(Some("Garage"), "192.0.2.5", false)),
            "Garage (http://192.0.2.5:8080)"
        );
        assert_eq!(
            device_label(&device(None, "2001:db8::7", true)),
            "IPC (https://[2001:db8::7]:443)"
        );
    }

    fn lookup_form(device: Option<DiscoveredDevice>, rtsp_url: &str) -> LookupForm {
        LookupForm {
            device,
            rtsp_url: rtsp_url.into(),
            port: 80,
            https: false,
            tls_verify: true,
            username: " viewer ".into(),
            password: "secret".into(),
        }
    }

    #[test]
    fn a_stream_request_asks_the_picked_camera_at_the_form_port_and_scheme() {
        let form = LookupForm {
            port: 443,
            https: true,
            tls_verify: false,
            ..lookup_form(Some(device(Some("Garage"), "192.0.2.5", false)), "")
        };
        let request = stream_request(&form).unwrap();
        assert_eq!(
            (
                request.host.as_str(),
                request.http_port,
                request.https,
                request.tls_verify,
                request.username.as_str()
            ),
            ("192.0.2.5", 443, true, false, "viewer")
        );
    }

    #[test]
    fn a_stream_request_without_a_pick_asks_the_typed_url_host() {
        let typed = stream_request(&lookup_form(None, " rtsp://192.0.2.9:554/live ")).unwrap();
        assert_eq!(
            (typed.host.as_str(), typed.http_port, typed.https),
            ("192.0.2.9", 80, false)
        );
        let named = stream_request(&lookup_form(None, "rtsp://[2001:db8::9]/live")).unwrap();
        assert_eq!(named.host, "2001:db8::9");
    }

    #[test]
    fn a_stream_request_needs_a_username_a_host_and_a_port() {
        let nameless = LookupForm {
            username: " ".into(),
            ..lookup_form(Some(device(None, "192.0.2.5", false)), "")
        };
        assert!(stream_request(&nameless).is_err());
        assert!(stream_request(&lookup_form(None, "")).is_err());
        assert!(stream_request(&lookup_form(None, "rtsp:live")).is_err());
        let portless = LookupForm {
            port: 0,
            ..lookup_form(None, "rtsp://192.0.2.9/live")
        };
        assert!(stream_request(&portless).is_err());
    }

    #[test]
    fn the_port_follows_the_scheme_only_from_its_default() {
        assert_eq!(port_for_scheme(80, true), 443);
        assert_eq!(port_for_scheme(443, false), 80);
        assert_eq!(port_for_scheme(8080, true), 8080);
        assert_eq!(port_for_scheme(8443, false), 8443);
        assert_eq!(port_for_scheme(443, true), 443);
    }

    #[test]
    fn profiles_fill_the_main_and_then_the_sub_url() {
        let stream = |token: &str, url: &str| ProfileStream {
            token: token.into(),
            name: token.into(),
            url: url.into(),
        };
        assert_eq!(stream_fill(&[]), None);
        assert_eq!(
            stream_fill(&[stream("p1", "rtsp://192.0.2.5/1")]),
            Some(("rtsp://192.0.2.5/1".to_owned(), None))
        );
        assert_eq!(
            stream_fill(&[
                stream("p1", "rtsp://192.0.2.5/1"),
                stream("p2", "rtsp://192.0.2.5/2"),
                stream("p3", "rtsp://192.0.2.5/3"),
            ]),
            Some((
                "rtsp://192.0.2.5/1".to_owned(),
                Some("rtsp://192.0.2.5/2".to_owned())
            ))
        );
    }
}
