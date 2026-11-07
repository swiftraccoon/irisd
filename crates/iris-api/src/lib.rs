//! HTTP API for irisd: catalog queries, byte-range HLS playlists, fMP4 export,
//! live WebSocket streaming, events, snapshots, health, and metrics.
//!
//! The server never decodes video. Playback endpoints hand out byte ranges of
//! the recording files described by the catalog; the live endpoint wraps the
//! frames of a [`LiveRing`](iris_ingest::LiveRing) in one `moof`+`mdat` each.
//!
//! # Surface
//!
//! [`router`] builds the routes; [`bind`] obtains the listener and
//! [`serve_listener`] runs it until a cancellation token fires. [`serve`] is
//! the combined convenience entry point. `/api/health` and `/metrics` are public; every
//! other route requires a bearer token when [`ApiState::tokens`] is not empty.
//!
//! | Route | Purpose |
//! |---|---|
//! | `GET /api/health` | liveness: status, uptime, version |
//! | `GET /metrics` | Prometheus text: requests, clients, event lag, and catalog queue/failure/state/storage metrics |
//! | `GET /api/cameras` | the configured cameras |
//! | `GET /api/cameras/{camera}` | one camera and the health of its recorded streams |
//! | `GET /api/cameras/{camera}/snapshot.jpg` | a JPEG proxied from the camera |
//! | `GET /api/streams/{camera}/{kind}/segments` | catalog rows in a time range |
//! | `GET /api/streams/{camera}/{kind}/hls/vod.m3u8` | a byte-range VOD playlist |
//! | `GET /api/streams/{camera}/{kind}/export.mp4` | init segment plus the fragments in range |
//! | `GET /api/streams/{camera}/{kind}/live` | WebSocket: init segment, then a fragment per frame |
//! | `GET /api/streams/{camera}/{kind}/hls/live.m3u8` | LL-HLS playlist with in-memory init, segment, and part resources |
//! | `GET /api/streams/{camera}/{kind}/keyframe` | a playable MP4 containing one sync sample |
//! | `GET /api/segments/{id}/file.mp4` | one recording file with range support |
//! | `GET /api/events` | bounded event pages with an optional `(time, id)` cursor, or WebSocket push |
//! | `POST /api/events/{id}/retain` | set or clear footage retention around an event |
//!
//! `from` and `to` take Unix microseconds or RFC 3339 and may span at most
//! seven days. Event continuations pair `after_us` with a positive `after_id`.
//! Catalog overload returns HTTP 503. Live WebSocket messages carry a type byte first: `0x01` an
//! initialization segment, `0x02` a fragment holding one frame.
//!
//! # State
//!
//! [`ApiState`] holds the catalog handle, the [`LiveRegistry`] the daemon fills
//! as streams start, the event broadcast channel, tokens, active configured cameras,
//! [`SnapshotSource`]s, [`ApiStats`], a monotonic start instant, and the shutdown token.
//! Failures are [`ApiError`]s rendered as JSON bodies with the matching status.
//!
//! ```no_run
//! use std::sync::Arc;
//! use iris_api::{ApiState, router, serve};
//! # async fn run(state: Arc<ApiState>) -> std::io::Result<()> {
//! let shutdown = tokio_util::sync::CancellationToken::new();
//! serve("127.0.0.1:8554".parse().expect("literal"), router(Arc::clone(&state)), shutdown).await
//! # }
//! ```

mod auth;
mod cameras;
mod error;
mod events;
mod export;
mod health;
mod hls;
mod keyframe;
mod live;
mod llhls;
mod media_admission;
mod metrics;
mod query;
mod segments;
mod snapshot;
mod state;

use std::io;
use std::net::SocketAddr;
use std::sync::Arc;

use axum::Router;
use axum::middleware;
use axum::routing::get;
use tokio_util::sync::CancellationToken;

pub use error::ApiError;
pub use state::{ActiveCamera, ApiState, ApiStats, LiveRegistry, SnapshotSource};

/// Builds the router. Routes under `/api` other than `/api/health` require a
/// bearer token when [`ApiState::tokens`] is not empty.
pub fn router(state: Arc<ApiState>) -> Router {
    if state.tokens.is_empty() {
        tracing::warn!("API tokens are empty: every endpoint is unauthenticated");
    }
    let public = Router::new()
        .route("/api/health", get(health::health))
        .route("/metrics", get(metrics::metrics));
    let protected = Router::new()
        .route("/api/cameras", get(cameras::list))
        .route("/api/cameras/{camera}", get(cameras::detail))
        .route(
            "/api/cameras/{camera}/snapshot.jpg",
            get(snapshot::snapshot),
        )
        .route("/api/streams/{camera}/{kind}/segments", get(segments::list))
        .route("/api/streams/{camera}/{kind}/hls/vod.m3u8", get(hls::vod))
        .route(
            "/api/streams/{camera}/{kind}/export.mp4",
            get(export::export),
        )
        .route("/api/streams/{camera}/{kind}/live", get(live::live))
        .route(
            "/api/streams/{camera}/{kind}/hls/live.m3u8",
            get(llhls::playlist),
        )
        .route(
            "/api/streams/{camera}/{kind}/hls/init.mp4",
            get(llhls::init),
        )
        .route(
            "/api/streams/{camera}/{kind}/hls/seg/{msn}/media.m4s",
            get(llhls::segment),
        )
        .route(
            "/api/streams/{camera}/{kind}/hls/part/{msn}/{index}/part.m4s",
            get(llhls::part),
        )
        .route(
            "/api/streams/{camera}/{kind}/keyframe",
            get(keyframe::keyframe),
        )
        .route("/api/segments/{id}/file.mp4", get(segments::file))
        .route("/api/events", get(events::list_or_subscribe))
        .route(
            "/api/events/{id}/retain",
            axum::routing::post(events::retain),
        )
        .route_layer(middleware::from_fn_with_state(
            Arc::clone(&state),
            auth::require_token,
        ));
    public.merge(protected).with_state(state)
}

/// Serves `router` on `listen` until `shutdown` is cancelled.
pub async fn serve(
    listen: SocketAddr,
    router: Router,
    shutdown: CancellationToken,
) -> io::Result<()> {
    let listener = bind(listen).await?;
    serve_listener(listener, router, shutdown).await
}

/// Binds the API listener so an orchestrator can prove admission before
/// reporting service readiness.
pub async fn bind(listen: SocketAddr) -> io::Result<tokio::net::TcpListener> {
    tokio::net::TcpListener::bind(listen).await
}

/// Serves `router` on an already-bound listener until `shutdown` is cancelled.
pub async fn serve_listener(
    listener: tokio::net::TcpListener,
    router: Router,
    shutdown: CancellationToken,
) -> io::Result<()> {
    let listen = listener.local_addr()?;
    tracing::info!(%listen, "API listening");
    axum::serve(listener, router)
        .with_graceful_shutdown(async move { shutdown.cancelled().await })
        .await
}
