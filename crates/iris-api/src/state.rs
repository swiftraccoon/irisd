//! Shared server state.

use std::collections::HashMap;
use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};

use iris_catalog::CatalogHandle;
use iris_core::event::Event;
use iris_core::id::{CameraId, StreamKey};
use iris_core::time::WallUs;
use iris_ingest::{LiveRing, StreamStats};
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

/// Everything the handlers need, shared by every request: the catalog, the
/// live rings, the event channel, the accepted tokens, snapshot sources,
/// counters, and the shutdown token that ends long-lived WebSocket sessions.
pub struct ApiState {
    /// Recording and event catalog.
    pub catalog: CatalogHandle,
    /// Live rings by stream.
    pub live: Arc<LiveRegistry>,
    /// Events as they are published; WebSocket clients subscribe here.
    pub events: broadcast::Sender<Arc<Event>>,
    /// Accepted bearer tokens; empty disables authentication.
    pub tokens: Vec<String>,
    /// Where to fetch snapshots for each camera.
    pub snapshot_sources: HashMap<CameraId, SnapshotSource>,
    /// Request and client counters.
    pub stats: Arc<ApiStats>,
    /// When the server started, for uptime reporting.
    pub started: WallUs,
    /// Cancelled on shutdown so long-lived WebSocket handlers end.
    pub shutdown: CancellationToken,
}

impl fmt::Debug for ApiState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ApiState")
            .field("tokens", &self.tokens.len())
            .field("snapshot_sources", &self.snapshot_sources.len())
            .field("started", &self.started)
            .finish_non_exhaustive()
    }
}

/// A camera's HTTP endpoint for `snapshot.cgi`.
#[derive(Clone)]
pub struct SnapshotSource {
    /// Host name or address.
    pub host: String,
    /// HTTP port.
    pub port: u16,
    /// Use HTTPS instead of HTTP.
    pub https: bool,
    /// Verify the camera's TLS certificate (false for self-signed cameras).
    pub tls_verify: bool,
    /// Digest-auth user.
    pub username: String,
    /// Digest-auth password.
    pub password: String,
}

impl fmt::Debug for SnapshotSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SnapshotSource")
            .field("host", &self.host)
            .field("port", &self.port)
            .field("username", &self.username)
            .finish_non_exhaustive()
    }
}

/// Request and client counters exposed on `/metrics` and in the daemon's
/// statistics; updated with relaxed atomics from any handler.
#[derive(Debug, Default)]
pub struct ApiStats {
    requests: AtomicU64,
    live_clients: AtomicU64,
    event_clients: AtomicU64,
}

impl ApiStats {
    /// Counts one request.
    pub fn record_request(&self) {
        self.requests.fetch_add(1, Ordering::Relaxed);
    }

    /// Requests served so far.
    #[must_use]
    pub fn requests(&self) -> u64 {
        self.requests.load(Ordering::Relaxed)
    }

    /// Live WebSocket clients connected now.
    #[must_use]
    pub fn live_clients(&self) -> u64 {
        self.live_clients.load(Ordering::Relaxed)
    }

    /// Event WebSocket clients connected now.
    #[must_use]
    pub fn event_clients(&self) -> u64 {
        self.event_clients.load(Ordering::Relaxed)
    }

    pub(crate) fn live_client_joined(&self) {
        self.live_clients.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn live_client_left(&self) {
        self.live_clients.fetch_sub(1, Ordering::Relaxed);
    }

    pub(crate) fn event_client_joined(&self) {
        self.event_clients.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn event_client_left(&self) {
        self.event_clients.fetch_sub(1, Ordering::Relaxed);
    }
}

/// Live rings by stream key. The daemon inserts one ring per stream as
/// recording starts; the live endpoint looks a stream up by camera and kind.
#[derive(Debug, Default)]
pub struct LiveRegistry {
    rings: RwLock<HashMap<StreamKey, LiveRing>>,
    stats: RwLock<HashMap<StreamKey, Arc<StreamStats>>>,
    hls: crate::llhls::LiveHlsRegistry,
}

impl LiveRegistry {
    /// Creates an empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers (or replaces) the ring for a stream.
    pub fn insert(&self, key: StreamKey, ring: LiveRing) {
        self.rings
            .write()
            .expect("live registry lock")
            .insert(key, ring);
    }

    /// The low-latency HLS states started from these rings.
    pub(crate) fn hls(&self) -> &crate::llhls::LiveHlsRegistry {
        &self.hls
    }

    /// Registers the statistics of a stream, which the camera detail reports.
    pub fn insert_stats(&self, key: StreamKey, stats: Arc<StreamStats>) {
        self.stats
            .write()
            .expect("live registry lock")
            .insert(key, stats);
    }

    /// The statistics of every registered stream of `camera`, by kind.
    #[must_use]
    pub fn camera_stats(&self, camera: &CameraId) -> Vec<(StreamKey, Arc<StreamStats>)> {
        let mut out: Vec<(StreamKey, Arc<StreamStats>)> = self
            .stats
            .read()
            .expect("live registry lock")
            .iter()
            .filter(|(key, _)| &key.camera == camera)
            .map(|(key, stats)| (key.clone(), Arc::clone(stats)))
            .collect();
        out.sort_by_key(|(key, _)| key.kind.to_string());
        out
    }

    /// Returns the ring for a stream, if registered.
    #[must_use]
    pub fn get(&self, key: &StreamKey) -> Option<LiveRing> {
        self.rings
            .read()
            .expect("live registry lock")
            .get(key)
            .cloned()
    }

    /// The registered streams.
    #[must_use]
    pub fn keys(&self) -> Vec<StreamKey> {
        self.rings
            .read()
            .expect("live registry lock")
            .keys()
            .cloned()
            .collect()
    }
}
