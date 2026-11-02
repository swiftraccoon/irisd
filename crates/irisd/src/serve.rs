//! `irisd serve`: a static file server with range support for playback tests.

use std::path::PathBuf;

use anyhow::Context;
use axum::Router;
use axum::extract::Request;
use axum::http::{HeaderValue, header};
use axum::middleware::{self, Next};
use axum::response::Response;
use clap::Args as ClapArgs;
use tower_http::services::ServeDir;
use tracing::info;

/// Arguments for `serve`.
#[derive(Debug, ClapArgs)]
pub struct Args {
    /// Directory to serve (for example the output of `irisd hls`).
    #[arg(long, value_name = "DIR")]
    dir: PathBuf,
    /// Address to listen on.
    #[arg(long, default_value = "0.0.0.0:8080")]
    listen: String,
}

/// Runs the subcommand until Ctrl-C.
pub async fn run(args: Args) -> anyhow::Result<()> {
    let dir = args
        .dir
        .canonicalize()
        .with_context(|| format!("resolving {}", args.dir.display()))?;
    let app = Router::new()
        .fallback_service(ServeDir::new(&dir))
        .layer(middleware::from_fn(log_and_fix_types));
    let listener = tokio::net::TcpListener::bind(&args.listen)
        .await
        .with_context(|| format!("binding {}", args.listen))?;
    let addr = listener.local_addr()?;
    info!(dir = %dir.display(), %addr, "serving; open http://<this-host>:{}/master.m3u8", addr.port());
    axum::serve(listener, app)
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await?;
    Ok(())
}

async fn log_and_fix_types(req: Request, next: Next) -> Response {
    let method = req.method().clone();
    let path = req.uri().path().to_owned();
    let range = req
        .headers()
        .get(header::RANGE)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    let mut res = next.run(req).await;
    let extension = std::path::Path::new(&path)
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase);
    let content_type = match extension.as_deref() {
        Some("m3u8") => Some("application/vnd.apple.mpegurl"),
        Some("mp4") => Some("video/mp4"),
        _ => None,
    };
    if let Some(content_type) = content_type
        && res.status().is_success()
    {
        res.headers_mut()
            .insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
    }
    info!(%method, %path, status = res.status().as_u16(), range = range.as_deref().unwrap_or("-"), "request");
    res
}
