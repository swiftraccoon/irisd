//! Shared server state.

use std::collections::HashMap;
use std::fmt;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use bytes::Bytes;
use iris_catalog::{CatalogError, CatalogHandle, CatalogStats};
use iris_core::event::Event;
use iris_core::id::{CameraId, StreamKey};
use iris_core::time::WallUs;
use iris_ingest::{LiveRing, StreamStats};
use tokio::sync::{Mutex, Notify, OwnedSemaphorePermit, Semaphore, broadcast};
use tokio_util::sync::CancellationToken;

/// Number of simultaneous disk-backed media responses admitted process-wide.
pub(crate) const MAX_CONCURRENT_MEDIA_RESPONSES: usize = 8;
/// Catalog aggregate refresh interval shared by all public metrics scrapes.
const CATALOG_STATS_REFRESH: Duration = Duration::from_secs(1);

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
    /// Cameras in the running configuration, separate from historical catalog rows.
    pub active_cameras: HashMap<CameraId, ActiveCamera>,
    /// Request and client counters.
    pub stats: Arc<ApiStats>,
    /// Wall time when the server started, retained for diagnostic context.
    pub started: WallUs,
    /// Monotonic instant when the server started, for uptime reporting.
    pub started_monotonic: Instant,
    /// Cancelled on shutdown so long-lived WebSocket handlers end.
    pub shutdown: CancellationToken,
}

impl fmt::Debug for ApiState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ApiState")
            .field("tokens", &self.tokens.len())
            .field("snapshot_sources", &self.snapshot_sources.len())
            .field("active_cameras", &self.active_cameras.len())
            .field("started", &self.started)
            .finish_non_exhaustive()
    }
}

/// One camera in the running configuration.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ActiveCamera {
    /// Stable camera identifier.
    pub id: CameraId,
    /// Human-readable configured name.
    pub name: String,
    /// Optional configured vendor hint.
    pub vendor: Option<String>,
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

/// Request, client, and event-lag counters exposed on `/metrics` and in the
/// daemon's statistics; updated with relaxed atomics from any handler.
#[derive(Debug)]
pub struct ApiStats {
    requests: AtomicU64,
    live_clients: AtomicU64,
    event_clients: AtomicU64,
    event_messages_lagged: AtomicU64,
    catalog_stats: Mutex<CatalogStatsCache>,
    media_responses: Arc<Semaphore>,
}

impl Default for ApiStats {
    fn default() -> Self {
        Self {
            requests: AtomicU64::new(0),
            live_clients: AtomicU64::new(0),
            event_clients: AtomicU64::new(0),
            event_messages_lagged: AtomicU64::new(0),
            catalog_stats: Mutex::new(CatalogStatsCache::default()),
            media_responses: Arc::new(Semaphore::new(MAX_CONCURRENT_MEDIA_RESPONSES)),
        }
    }
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

    /// Event messages skipped by lagging WebSocket receivers.
    #[must_use]
    pub fn event_messages_lagged(&self) -> u64 {
        self.event_messages_lagged.load(Ordering::Relaxed)
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

    pub(crate) fn record_event_lag(&self, skipped: u64) {
        self.event_messages_lagged
            .fetch_add(skipped, Ordering::Relaxed);
    }

    /// Reserves one process-wide disk-backed media response slot without waiting.
    pub(crate) fn try_media_response(&self) -> Option<OwnedSemaphorePermit> {
        Arc::clone(&self.media_responses).try_acquire_owned().ok()
    }

    /// Returns a shared catalog aggregate snapshot, refreshing it at most once
    /// per cadence. Refresh ownership belongs to this cache rather than to the
    /// initiating request, so cancelling a public scrape cannot enqueue another
    /// catalog query while the first one remains in flight.
    pub(crate) async fn catalog_stats(
        self: &Arc<Self>,
        catalog: &CatalogHandle,
    ) -> Option<CatalogStats> {
        loop {
            let refreshed = {
                let mut cache = self.catalog_stats.lock().await;
                if cache
                    .refreshed
                    .is_some_and(|refreshed| refreshed.elapsed() < CATALOG_STATS_REFRESH)
                {
                    return cache.value;
                }

                // Create the waiter while holding the cache lock. `Notify`
                // records `notify_waiters` generations, so completion cannot
                // race between dropping the lock and polling this future.
                let refreshed = Arc::clone(&cache.refresh_completed).notified_owned();
                if !cache.refreshing {
                    cache.refreshing = true;
                    let stats = Arc::clone(self);
                    let catalog = catalog.clone();
                    tokio::spawn(async move {
                        let result = catalog.stats().await;
                        stats.finish_catalog_stats_refresh(result).await;
                    });
                }
                refreshed
            };
            refreshed.await;
        }
    }

    async fn finish_catalog_stats_refresh(&self, result: Result<CatalogStats, CatalogError>) {
        let mut cache = self.catalog_stats.lock().await;
        match result {
            Ok(value) => cache.value = Some(value),
            Err(error) => {
                tracing::warn!(%error, "catalog stats refresh failed; serving partial or cached metrics");
            }
        }
        // A completed failure also establishes the retry cadence, including
        // before the first successful snapshot.
        cache.refreshed = Some(Instant::now());
        cache.refreshing = false;
        let refresh_completed = Arc::clone(&cache.refresh_completed);
        drop(cache);
        refresh_completed.notify_waiters();
    }
}

/// Attaches one media-admission permit to response bytes. Clones of the
/// returned `Bytes` share the same owner, so admission is not released until
/// the transport has dropped the final response buffer.
pub(crate) fn admitted_media_bytes(bytes: Bytes, permit: Arc<OwnedSemaphorePermit>) -> Bytes {
    Bytes::from_owner(AdmittedMediaBytes {
        bytes,
        _permit: permit,
    })
}

struct AdmittedMediaBytes {
    bytes: Bytes,
    _permit: Arc<OwnedSemaphorePermit>,
}

impl AsRef<[u8]> for AdmittedMediaBytes {
    fn as_ref(&self) -> &[u8] {
        &self.bytes
    }
}

#[derive(Debug)]
struct CatalogStatsCache {
    value: Option<CatalogStats>,
    refreshed: Option<Instant>,
    refreshing: bool,
    refresh_completed: Arc<Notify>,
}

impl Default for CatalogStatsCache {
    fn default() -> Self {
        Self {
            value: None,
            refreshed: None,
            refreshing: false,
            refresh_completed: Arc::new(Notify::new()),
        }
    }
}

/// Live rings by stream key. The daemon inserts one ring per stream as
/// recording starts; the live endpoint looks a stream up by camera and kind.
#[derive(Debug, Default)]
pub struct LiveRegistry {
    rings: RwLock<HashMap<StreamKey, LiveRing>>,
    stats: RwLock<HashMap<StreamKey, Arc<StreamStats>>>,
    external_viewers: RwLock<HashMap<StreamKey, Arc<AtomicUsize>>>,
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
        self.external_viewers
            .write()
            .expect("live registry lock")
            .entry(key.clone())
            .or_insert_with(|| Arc::new(AtomicUsize::new(0)));
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

    /// Tracks one external live-WebSocket viewer until the returned guard drops.
    pub(crate) fn track_external_viewer(&self, key: &StreamKey) -> ExternalViewerGuard {
        let viewers = Arc::clone(
            self.external_viewers
                .write()
                .expect("live registry lock")
                .entry(key.clone())
                .or_insert_with(|| Arc::new(AtomicUsize::new(0))),
        );
        viewers.fetch_add(1, Ordering::Relaxed);
        ExternalViewerGuard { viewers }
    }

    /// External live-WebSocket viewers of a stream. Internal LL-HLS
    /// subscriptions are deliberately excluded.
    #[must_use]
    pub(crate) fn external_viewers(&self, key: &StreamKey) -> usize {
        self.external_viewers
            .read()
            .expect("live registry lock")
            .get(key)
            .map_or(0, |viewers| viewers.load(Ordering::Relaxed))
    }
}

/// Cancellation-safe ownership of one external live-WebSocket viewer count.
#[derive(Debug)]
pub(crate) struct ExternalViewerGuard {
    viewers: Arc<AtomicUsize>,
}

impl Drop for ExternalViewerGuard {
    fn drop(&mut self) {
        self.viewers.fetch_sub(1, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use iris_core::id::StreamKind;

    fn key() -> StreamKey {
        StreamKey {
            camera: CameraId::try_new("cam1").expect("valid camera id"),
            kind: StreamKind::Main,
        }
    }

    #[test]
    fn external_viewer_guard_counts_only_its_lifetime() {
        let registry = LiveRegistry::new();
        let key = key();
        assert_eq!(registry.external_viewers(&key), 0);
        let first = registry.track_external_viewer(&key);
        let second = registry.track_external_viewer(&key);
        assert_eq!(registry.external_viewers(&key), 2);
        drop(first);
        assert_eq!(registry.external_viewers(&key), 1);
        drop(second);
        assert_eq!(registry.external_viewers(&key), 0);
    }

    #[test]
    fn media_response_admission_is_process_wide_and_fallible() {
        let stats = ApiStats::default();
        let permits: Vec<_> = (0..MAX_CONCURRENT_MEDIA_RESPONSES)
            .map(|_| stats.try_media_response().expect("within budget"))
            .collect();
        assert!(stats.try_media_response().is_none());
        drop(permits);
        assert!(stats.try_media_response().is_some());
    }

    #[test]
    fn admitted_bytes_hold_the_permit_across_clones() {
        let stats = ApiStats::default();
        let guarded = admitted_media_bytes(
            Bytes::from_static(b"media"),
            Arc::new(stats.try_media_response().expect("first permit")),
        );
        let remaining: Vec<_> = (1..MAX_CONCURRENT_MEDIA_RESPONSES)
            .map(|_| stats.try_media_response().expect("remaining permit"))
            .collect();
        assert!(stats.try_media_response().is_none());
        let clone = guarded.clone();
        drop(guarded);
        assert!(stats.try_media_response().is_none());
        drop(clone);
        assert!(stats.try_media_response().is_some());
        drop(remaining);
    }
}
