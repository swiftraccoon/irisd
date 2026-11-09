//! Shared server state.

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use iris_catalog::{CatalogError, CatalogHandle, CatalogStats};
use iris_core::config::ResolvedSnapshot;
use iris_core::config::{DEFAULT_API_BOUNDED_MEDIA, DEFAULT_API_STREAMING_MEDIA};
use iris_core::event::Event;
use iris_core::id::{CameraId, StreamKey};
use iris_core::time::WallUs;
use iris_ingest::{LiveRing, StreamStats};
use iris_store::ApprovedRoot;
use tokio::sync::{Mutex, Notify, OwnedSemaphorePermit, Semaphore, broadcast};
use tokio_util::sync::CancellationToken;
use url::Url;

use crate::auth::CapabilitySecret;
use crate::error::ApiError;
use crate::media_admission::{self, MediaPermit};
use crate::provision::CameraControl;

/// Live WebSocket sessions admitted process-wide.
const MAX_LIVE_SOCKETS: usize = 64;
/// Live WebSocket sessions admitted for one stream.
pub(crate) const MAX_LIVE_SOCKETS_PER_STREAM: usize = 16;
/// Event WebSocket sessions admitted process-wide.
const MAX_EVENT_SOCKETS: usize = 32;
/// Catalog aggregate refresh interval shared by all public metrics scrapes.
const CATALOG_STATS_REFRESH: Duration = Duration::from_secs(1);

/// The admission class of a media response.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum MediaClass {
    /// Whole or open-ended recording files, ranges above
    /// [`crate::segments::BOUNDED_RANGE_BYTES`], and exports: responses whose length or
    /// duration the request does not bound.
    Streaming,
    /// VOD playlists, keyframes, LL-HLS initialization, segment, and part resources, and
    /// short ranges.
    Bounded,
}

impl MediaClass {
    fn busy_reason(self) -> &'static str {
        match self {
            Self::Streaming => "too many concurrent streaming media responses",
            Self::Bounded => "too many concurrent bounded media responses",
        }
    }
}

/// Everything the handlers need, shared by every request: the catalog, the
/// live rings, the event channel, the accepted tokens and the capability
/// secret, the approved recording roots, the public-metrics switch, the
/// running cameras and their snapshot sources, the daemon's camera control,
/// counters, and the shutdown token that ends long-lived WebSocket sessions.
/// The daemon assembles it once at startup; the fields are its assembly
/// record.
pub struct ApiState {
    /// Recording and event catalog.
    pub catalog: CatalogHandle,
    /// Live rings by stream.
    pub live: Arc<LiveRegistry>,
    /// Events as they are published; WebSocket clients subscribe here.
    pub events: broadcast::Sender<Arc<Event>>,
    /// Accepted bearer tokens; empty disables authentication.
    pub tokens: Vec<String>,
    /// Signs media capabilities; see [`CapabilitySecret`].
    pub capability_secret: CapabilitySecret,
    /// Recording roots admitted at startup. Media is read only through a descriptor opened
    /// beneath the root that owns a row's path; a row outside every root returns HTTP 404.
    pub recording_roots: Vec<ApprovedRoot>,
    /// Serve `/metrics` without authentication. When false it requires a root bearer token.
    pub public_metrics: bool,
    /// Where to fetch snapshots for each camera; provisioning adds and removes entries.
    pub snapshot_sources: RwLock<HashMap<CameraId, SnapshotSource>>,
    /// Cameras in the running configuration, separate from historical catalog
    /// rows; provisioning adds and removes entries.
    pub active_cameras: RwLock<HashMap<CameraId, ActiveCamera>>,
    /// The daemon's camera control while `[provisioning]` is enabled;
    /// `POST /api/cameras`, `DELETE /api/cameras/{camera}`, `GET
    /// /api/onvif/devices`, and `POST /api/onvif/streams` send their
    /// requests through it and answer HTTP 403 without it.
    pub control: Option<CameraControl>,
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
            .field("recording_roots", &self.recording_roots.len())
            .field("public_metrics", &self.public_metrics)
            .field(
                "snapshot_sources",
                &self
                    .snapshot_sources
                    .read()
                    .map_or(0, |sources| sources.len()),
            )
            .field(
                "active_cameras",
                &self
                    .active_cameras
                    .read()
                    .map_or(0, |cameras| cameras.len()),
            )
            .field("control", &self.control.is_some())
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
    /// Added through the API rather than the configuration file, so the API
    /// may remove it.
    pub provisioned: bool,
}

/// A camera's HTTP endpoint for snapshots: the Dahua CGI path, or the URL its
/// ONVIF Media service names, resolved on first use and kept for an hour.
/// Built with [`SnapshotSource::new`]; clones share the resolved URL.
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
    /// Where the snapshot comes from.
    pub method: ResolvedSnapshot,
    /// The ONVIF snapshot URL and when it was resolved, shared by every clone
    /// of the source; `None` until the first request or after a failed fetch.
    pub(crate) resolved: Arc<std::sync::Mutex<Option<(Url, Instant)>>>,
}

impl SnapshotSource {
    /// A source whose ONVIF URL, if its method needs one, is resolved on the
    /// first request.
    #[must_use]
    pub fn new(
        host: String,
        port: u16,
        https: bool,
        tls_verify: bool,
        username: String,
        password: String,
        method: ResolvedSnapshot,
    ) -> Self {
        Self {
            host,
            port,
            https,
            tls_verify,
            username,
            password,
            method,
            resolved: Arc::default(),
        }
    }
}

impl fmt::Debug for SnapshotSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SnapshotSource")
            .field("host", &self.host)
            .field("port", &self.port)
            .field("username", &"<redacted>")
            .field("method", &self.method)
            .finish_non_exhaustive()
    }
}

/// Request, client, event-lag, and admission counters exposed on `/metrics` and
/// in the daemon's statistics, updated with relaxed atomics from any handler,
/// plus the admission budgets every handler shares: two media response
/// classes, the live and event WebSocket caps, and one in-flight snapshot per
/// camera.
#[derive(Debug)]
pub struct ApiStats {
    requests: AtomicU64,
    live_clients: AtomicU64,
    event_clients: AtomicU64,
    event_messages_lagged: AtomicU64,
    catalog_stats: Mutex<CatalogStatsCache>,
    streaming_media: Arc<Semaphore>,
    bounded_media: Arc<Semaphore>,
    live_sockets: Arc<Semaphore>,
    event_sockets: Arc<Semaphore>,
    snapshots_in_flight: std::sync::Mutex<HashSet<CameraId>>,
    media_rejected: AtomicU64,
    websocket_rejected: AtomicU64,
    snapshot_rejected: AtomicU64,
    unapproved_rows: AtomicU64,
    live_oversized_closes: AtomicU64,
}

impl Default for ApiStats {
    /// Counters with the configuration's default media admission classes:
    /// [`DEFAULT_API_STREAMING_MEDIA`] streaming and [`DEFAULT_API_BOUNDED_MEDIA`] bounded slots.
    fn default() -> Self {
        Self::new_with_media_limits(DEFAULT_API_STREAMING_MEDIA, DEFAULT_API_BOUNDED_MEDIA)
    }
}

impl ApiStats {
    /// Creates counters whose media admission admits `streaming` streaming and `bounded`
    /// bounded responses at once (`[api] max_streaming_media` and `max_bounded_media`).
    /// Each class has at least one slot.
    #[must_use]
    pub fn new_with_media_limits(streaming: usize, bounded: usize) -> Self {
        Self {
            requests: AtomicU64::new(0),
            live_clients: AtomicU64::new(0),
            event_clients: AtomicU64::new(0),
            event_messages_lagged: AtomicU64::new(0),
            catalog_stats: Mutex::new(CatalogStatsCache::default()),
            streaming_media: Arc::new(Semaphore::new(streaming.max(1))),
            bounded_media: Arc::new(Semaphore::new(bounded.max(1))),
            live_sockets: Arc::new(Semaphore::new(MAX_LIVE_SOCKETS)),
            event_sockets: Arc::new(Semaphore::new(MAX_EVENT_SOCKETS)),
            snapshots_in_flight: std::sync::Mutex::new(HashSet::new()),
            media_rejected: AtomicU64::new(0),
            websocket_rejected: AtomicU64::new(0),
            snapshot_rejected: AtomicU64::new(0),
            unapproved_rows: AtomicU64::new(0),
            live_oversized_closes: AtomicU64::new(0),
        }
    }

    /// Counts one request.
    pub fn record_request(&self) {
        self.requests.fetch_add(1, Ordering::Relaxed);
    }

    /// Requests served so far.
    #[must_use]
    pub fn requests(&self) -> u64 {
        self.requests.load(Ordering::Relaxed)
    }

    /// Connected live WebSocket clients.
    #[must_use]
    pub fn live_clients(&self) -> u64 {
        self.live_clients.load(Ordering::Relaxed)
    }

    /// Connected event WebSocket clients.
    #[must_use]
    pub fn event_clients(&self) -> u64 {
        self.event_clients.load(Ordering::Relaxed)
    }

    /// Event messages skipped by lagging WebSocket receivers.
    #[must_use]
    pub fn event_messages_lagged(&self) -> u64 {
        self.event_messages_lagged.load(Ordering::Relaxed)
    }

    /// Media requests answered HTTP 503 because their admission class was full.
    #[must_use]
    pub fn media_rejected(&self) -> u64 {
        self.media_rejected.load(Ordering::Relaxed)
    }

    /// Live and event WebSocket upgrades answered HTTP 503 by the socket caps.
    #[must_use]
    pub fn websocket_rejected(&self) -> u64 {
        self.websocket_rejected.load(Ordering::Relaxed)
    }

    /// Snapshot requests answered HTTP 503 while the camera's previous request was in flight.
    #[must_use]
    pub fn snapshot_rejected(&self) -> u64 {
        self.snapshot_rejected.load(Ordering::Relaxed)
    }

    /// Recording rows refused because their path is outside every approved recording root.
    #[must_use]
    pub fn unapproved_rows(&self) -> u64 {
        self.unapproved_rows.load(Ordering::Relaxed)
    }

    /// Live WebSocket sessions closed with code 1009 because a sync frame exceeded the
    /// message limit.
    #[must_use]
    pub fn live_oversized_closes(&self) -> u64 {
        self.live_oversized_closes.load(Ordering::Relaxed)
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

    /// Reserves one process-wide media response slot of `class` without waiting.
    pub(crate) fn try_media(&self, class: MediaClass) -> Option<OwnedSemaphorePermit> {
        let semaphore = match class {
            MediaClass::Streaming => &self.streaming_media,
            MediaClass::Bounded => &self.bounded_media,
        };
        Arc::clone(semaphore).try_acquire_owned().ok()
    }

    /// Admits one media response of `class`, or counts the rejection and returns
    /// [`ApiError::Busy`] when the class is full.
    pub(crate) fn admit_media(&self, class: MediaClass) -> Result<MediaPermit, ApiError> {
        self.try_media(class)
            .map(media_admission::shared)
            .ok_or_else(|| {
                self.media_rejected.fetch_add(1, Ordering::Relaxed);
                ApiError::Busy(class.busy_reason().to_owned())
            })
    }

    /// Admits one live WebSocket against the process-wide cap.
    pub(crate) fn admit_live_socket(&self) -> Result<OwnedSemaphorePermit, ApiError> {
        Arc::clone(&self.live_sockets)
            .try_acquire_owned()
            .map_err(|_| self.reject_websocket("too many live WebSockets"))
    }

    /// Admits one event WebSocket against the process-wide cap.
    pub(crate) fn admit_event_socket(&self) -> Result<OwnedSemaphorePermit, ApiError> {
        Arc::clone(&self.event_sockets)
            .try_acquire_owned()
            .map_err(|_| self.reject_websocket("too many event WebSockets"))
    }

    /// Counts one WebSocket upgrade refused by a socket cap.
    pub(crate) fn reject_websocket(&self, reason: &str) -> ApiError {
        self.websocket_rejected.fetch_add(1, Ordering::Relaxed);
        ApiError::Busy(reason.to_owned())
    }

    /// Admits the only in-flight snapshot request for `camera`; the guard
    /// releases it when the request completes or is cancelled.
    pub(crate) fn admit_snapshot(
        self: &Arc<Self>,
        camera: &CameraId,
    ) -> Result<SnapshotGuard, ApiError> {
        let admitted = self
            .snapshots_in_flight
            .lock()
            .expect("snapshot admission lock")
            .insert(camera.clone());
        if !admitted {
            self.snapshot_rejected.fetch_add(1, Ordering::Relaxed);
            return Err(ApiError::Busy(
                "a snapshot of this camera is already in flight".to_owned(),
            ));
        }
        Ok(SnapshotGuard {
            stats: Arc::clone(self),
            camera: camera.clone(),
        })
    }

    /// Counts one recording row whose path is outside every approved root.
    pub(crate) fn record_unapproved_row(&self) {
        self.unapproved_rows.fetch_add(1, Ordering::Relaxed);
    }

    /// Counts one live WebSocket closed because a sync frame exceeded the message limit.
    pub(crate) fn record_live_oversized_close(&self) {
        self.live_oversized_closes.fetch_add(1, Ordering::Relaxed);
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

/// The single in-flight snapshot request of one camera.
#[derive(Debug)]
#[must_use = "dropping the guard releases the camera's snapshot slot"]
pub(crate) struct SnapshotGuard {
    stats: Arc<ApiStats>,
    camera: CameraId,
}

impl Drop for SnapshotGuard {
    fn drop(&mut self) {
        self.stats
            .snapshots_in_flight
            .lock()
            .expect("snapshot admission lock")
            .remove(&self.camera);
    }
}

#[derive(Debug, Default)]
struct CatalogStatsCache {
    value: Option<CatalogStats>,
    refreshed: Option<Instant>,
    refreshing: bool,
    refresh_completed: Arc<Notify>,
}

/// Live rings by stream key. The daemon inserts one ring per stream as
/// recording starts; the live endpoint looks a stream up by camera and kind.
///
/// # Panics
///
/// A method panics when a registry lock is poisoned, which takes a panic in
/// another caller holding it for writing; the release profile aborts at that
/// panic instead (`panic = "abort"`).
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
    ///
    /// # Panics
    ///
    /// When a registry lock is poisoned (see [`LiveRegistry`]).
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

    /// Forgets every stream of `camera`: its rings, statistics, viewer counts,
    /// and live playlist states. Live sockets on those streams end when their
    /// ring stops; the camera's recordings stay reachable through the catalog.
    ///
    /// # Panics
    ///
    /// When a registry lock is poisoned (see [`LiveRegistry`]).
    pub fn remove_camera(&self, camera: &CameraId) {
        self.rings
            .write()
            .expect("live registry lock")
            .retain(|key, _| key.camera != *camera);
        self.stats
            .write()
            .expect("live registry lock")
            .retain(|key, _| key.camera != *camera);
        self.external_viewers
            .write()
            .expect("live registry lock")
            .retain(|key, _| key.camera != *camera);
        self.hls.remove_camera(camera);
    }

    /// Registers the statistics of a stream, which the camera detail reports.
    ///
    /// # Panics
    ///
    /// When a registry lock is poisoned (see [`LiveRegistry`]).
    pub fn insert_stats(&self, key: StreamKey, stats: Arc<StreamStats>) {
        self.stats
            .write()
            .expect("live registry lock")
            .insert(key, stats);
    }

    /// The statistics of every registered stream of `camera`, by kind.
    ///
    /// # Panics
    ///
    /// When a registry lock is poisoned (see [`LiveRegistry`]).
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
    ///
    /// # Panics
    ///
    /// When a registry lock is poisoned (see [`LiveRegistry`]).
    #[must_use]
    pub fn get(&self, key: &StreamKey) -> Option<LiveRing> {
        self.rings
            .read()
            .expect("live registry lock")
            .get(key)
            .cloned()
    }

    /// The registered streams.
    ///
    /// # Panics
    ///
    /// When a registry lock is poisoned (see [`LiveRegistry`]).
    #[must_use]
    pub fn keys(&self) -> Vec<StreamKey> {
        self.rings
            .read()
            .expect("live registry lock")
            .keys()
            .cloned()
            .collect()
    }

    /// Tracks one external live-WebSocket viewer until the returned guard drops,
    /// or returns `None` when the stream already has `limit` viewers.
    #[must_use]
    pub(crate) fn try_track_external_viewer(
        &self,
        key: &StreamKey,
        limit: usize,
    ) -> Option<ExternalViewerGuard> {
        let viewers = Arc::clone(
            self.external_viewers
                .write()
                .expect("live registry lock")
                .entry(key.clone())
                .or_insert_with(|| Arc::new(AtomicUsize::new(0))),
        );
        let mut count = viewers.load(Ordering::Relaxed);
        loop {
            if count >= limit {
                return None;
            }
            match viewers.compare_exchange_weak(
                count,
                count + 1,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => return Some(ExternalViewerGuard { viewers }),
                Err(actual) => count = actual,
            }
        }
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
    fn snapshot_source_debug_omits_the_credentials() {
        let source = SnapshotSource::new(
            "192.0.2.7".into(),
            80,
            false,
            true,
            "synthetic-user".into(),
            "synthetic-password".into(),
            ResolvedSnapshot::DahuaCgi,
        );
        let rendered = format!("{source:?}");
        assert!(rendered.contains("192.0.2.7"), "{rendered}");
        assert!(!rendered.contains("synthetic"), "{rendered}");
    }

    #[test]
    fn external_viewer_guard_counts_only_its_lifetime() {
        let registry = LiveRegistry::new();
        let key = key();
        assert_eq!(registry.external_viewers(&key), 0);
        let first = registry.try_track_external_viewer(&key, 2).expect("first");
        let second = registry.try_track_external_viewer(&key, 2).expect("second");
        assert_eq!(registry.external_viewers(&key), 2);
        assert!(
            registry.try_track_external_viewer(&key, 2).is_none(),
            "a stream at its viewer cap admits no third viewer"
        );
        assert_eq!(registry.external_viewers(&key), 2);
        drop(first);
        assert_eq!(registry.external_viewers(&key), 1);
        drop(second);
        assert_eq!(registry.external_viewers(&key), 0);
    }

    #[test]
    fn media_classes_are_independent_process_wide_and_fallible() {
        let stats = ApiStats::new_with_media_limits(2, 3);
        let streaming: Vec<_> = (0..2)
            .map(|_| {
                stats
                    .try_media(MediaClass::Streaming)
                    .expect("within budget")
            })
            .collect();
        assert!(stats.try_media(MediaClass::Streaming).is_none());
        let bounded: Vec<_> = (0..3)
            .map(|_| stats.try_media(MediaClass::Bounded).expect("within budget"))
            .collect();
        assert!(matches!(
            stats.admit_media(MediaClass::Bounded),
            Err(ApiError::Busy(_))
        ));
        assert_eq!(stats.media_rejected(), 1);
        drop(streaming);
        assert!(stats.try_media(MediaClass::Streaming).is_some());
        drop(bounded);
        assert!(stats.try_media(MediaClass::Bounded).is_some());
    }

    #[test]
    fn default_media_classes_match_the_configuration_defaults() {
        let stats = ApiStats::default();
        let streaming: Vec<_> = (0..DEFAULT_API_STREAMING_MEDIA)
            .map(|_| stats.try_media(MediaClass::Streaming).expect("streaming"))
            .collect();
        assert!(stats.try_media(MediaClass::Streaming).is_none());
        let bounded: Vec<_> = (0..DEFAULT_API_BOUNDED_MEDIA)
            .map(|_| stats.try_media(MediaClass::Bounded).expect("bounded"))
            .collect();
        assert!(stats.try_media(MediaClass::Bounded).is_none());
        drop((streaming, bounded));
    }

    #[test]
    fn websocket_caps_reject_and_count_beyond_their_limits() {
        let stats = ApiStats::default();
        let live: Vec<_> = (0..MAX_LIVE_SOCKETS)
            .map(|_| stats.admit_live_socket().expect("live socket"))
            .collect();
        assert!(matches!(stats.admit_live_socket(), Err(ApiError::Busy(_))));
        let events: Vec<_> = (0..MAX_EVENT_SOCKETS)
            .map(|_| stats.admit_event_socket().expect("event socket"))
            .collect();
        assert!(matches!(stats.admit_event_socket(), Err(ApiError::Busy(_))));
        assert_eq!(stats.websocket_rejected(), 2);
        drop(live);
        assert!(stats.admit_live_socket().is_ok());
        drop(events);
        assert!(stats.admit_event_socket().is_ok());
    }

    #[test]
    fn one_snapshot_per_camera_is_in_flight() {
        let stats = Arc::new(ApiStats::default());
        let first = CameraId::try_new("cam1").expect("valid camera id");
        let second = CameraId::try_new("cam2").expect("valid camera id");
        let guard = stats.admit_snapshot(&first).expect("first request");
        assert!(matches!(
            stats.admit_snapshot(&first),
            Err(ApiError::Busy(_))
        ));
        let other = stats
            .admit_snapshot(&second)
            .expect("another camera is independent");
        assert_eq!(stats.snapshot_rejected(), 1);
        drop(guard);
        assert!(stats.admit_snapshot(&first).is_ok());
        drop(other);
    }
}
