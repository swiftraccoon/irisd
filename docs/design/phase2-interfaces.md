# Phase 2 interfaces

The cross-crate interfaces of the phase-2 crates: each crate is implemented against these
signatures so that the crates can be built in parallel, and a change to one is a design change
recorded here first. Each crate's rustdoc holds its full public surface. Shared types live in
`iris-core` so that no two crates need each other for a type definition. Behaviour is specified
in `../research/04-storage-format.md`, `../research/05-ingest-events.md`, and
`../research/07-architecture.md`.

## Shared types (`iris-core`)

```rust
// iris_core::event
pub struct Event {
    pub camera: CameraId,
    pub source: EventSource,       // DahuaCgi | OnvifPullPoint | OnvifMetadata | Manual
    pub kind: EventKind,           // mapped variants, or Other(String) capped at 1 KiB
    pub state: EventState,         // Start | Stop | Pulse
    pub nvr_time: WallUs,          // receipt time on the NVR clock (authoritative)
    pub camera_time: Option<WallUs>,
    pub data: Option<String>,      // valid JSON text; over-limit encoding is {"truncated":true}
}
impl Event { pub fn bounded(self) -> Self; } // bounds kind; validates/quotes/bounds JSON data
// iris_core::config (the fields the crates share; rustdoc lists every field)
pub struct StorageConfig { pub video_roots: Vec<PathBuf>, pub state_dir: PathBuf,
    pub min_free_percent: u8, /* other fields: see rustdoc */ }
pub struct ApiConfig { pub listen: SocketAddr, pub tokens: Vec<String>,     // Config.api
    pub allow_unauthenticated: bool /* false; only with no tokens on a loopback listener */ }
pub struct CameraConfig { /* the other fields */ pub http_port: Option<u16> /* 80, or 443 with https */, pub https: bool, pub tls_verify: bool /* true */, pub retention_days: Option<u32>,
    pub events: Vec<EventSource> /* default [] */, pub snapshot: SnapshotMethod /* auto | dahua_cgi | onvif | none */ }
impl CameraConfig { pub fn host(&self) -> Option<String> /* host part of rtsp_url */,
    pub fn validate(&self) -> Result<(), ConfigError> /* one camera on its own: rtsp:// URLs with a host, ranges, kinds, sources */ }
pub struct ProvisioningConfig { pub enabled: bool /* false */, pub max_cameras: usize /* 16, 1..=64 */,
    pub connect_ports: Vec<u16> /* [554, 80, 443]; the sandbox allows them at start */ }   // Config.provisioning
```

Configuration loading rejects repeated camera ids, repeated recorded stream kinds or event
sources within each camera, storage knobs outside their ranges (`min_free_percent` 0..=50,
where zero disables that floor; `sync_interval_ms` 100..=60,000; `rotate_secs` 60..=86,400;
`rotate_bytes` 64 MiB..=64 GiB; `preallocate_bytes` up to the smaller of `rotate_bytes` and
4 GiB; `quota_gib` at least 1; camera `retention_days` 1..=106,751,991), API tokens shorter
than 32 visible ASCII characters or equal to the example placeholder, an empty token list
without `allow_unauthenticated`, and that switch with tokens or on a non-loopback listener.
Token errors name the entry's position, never its value, and `CameraConfig`'s `Debug` output
redacts the password.
Parse failures expose only a generic syntax/schema description and a safe byte span, with
no input text or value-bearing parser message in display, debug, or error sources.
`CameraConfig::host` returns a bare host, including IPv6 without brackets. Authorities end
at the earliest `/`, `?`, or `#`; `rtsp_ports` retains explicit ports for empty-path URLs.
HTTP builders (`http_base`, event endpoints, and snapshot requests) bracket IPv6 hosts.

## `iris-catalog`

SQLite (rusqlite, bundled) opened in WAL mode with `synchronous = NORMAL` and
`temp_store = MEMORY`, owned by one thread named `iris-catalog`. In-memory temporary query
state avoids any dependency on a sandbox-external temp directory. Deletion intent uses a
scoped `synchronous = FULL` transaction
before filesystem mutation, then restores `NORMAL`. The handle offers nonblocking
notifications plus async queries in one ordered domain. Admission counts commands until
completion and caps the domain at 2,048 commands and 16 MiB of estimated owned input. Segment
open/seal transitions reserve 128 commands and 1 MiB. Events reject under overload,
same-path progress may coalesce only within the unqueried tail, queries return
`CatalogError::Overloaded`, and lifecycle-reserve exhaustion sets a reconciliation-required
metric. Lifecycle eviction cannot cross an admitted query. Shutdown or dropping the final
handle closes admission, drains the bounded admitted tail, and joins the thread.

```rust
#[derive(Clone)] pub struct CatalogHandle;
pub struct SegmentId(pub i64); pub struct EventId(pub i64);
pub struct EventCursor { pub time: WallUs, pub id: EventId }
pub struct TimeRange { pub from: WallUs, pub to: WallUs }   // half-open [from, to)

pub struct SegmentRow {
    pub id: SegmentId, pub key: StreamKey, pub path: PathBuf, pub run_seq: u64,
    pub discontinuity: bool, pub wall_start: WallUs, pub wall_end: WallUs,
    pub media_start: u64, pub media_end: u64, pub init_len: u32, pub durable_len: u64,
    pub fragments: u32, pub bytes: u64, pub sealed: bool,
    pub codecs: String, pub width: u16, pub height: u16, pub sample_entry_hash: u32,
    pub sample_entry_id: SampleEntryId,   // equal ids mean byte-identical sample entries
}
pub struct EventRow { pub id: EventId, pub event: Event, pub retain_until: Option<WallUs> }
pub struct SegmentCursor { pub wall_start: WallUs, pub id: SegmentId }   // From<&SegmentRow>
pub struct StartupRow { pub row: SegmentRow, pub root: Option<PathBuf> }
pub struct RetentionPage { pub rows: Vec<SegmentRow>,
    pub next: Option<SegmentCursor>, pub scanned: usize }
pub struct CameraRow { pub id: CameraId, pub name: String, pub vendor: Option<String> }
pub struct CatalogStats { pub segments: u64, pub sealed_bytes: u64,
    pub deleting_segments: u64, pub deleting_bytes: u64, pub events: u64,
    pub event_bytes: u64, pub retained_events: u64, pub pruned_events: u64,
    pub db_bytes: u64, pub wal_bytes: u64 }
pub const OPEN_SEGMENT_GRACE: Duration;   // 30 s default: how far an unsealed row extends past its last progress
pub enum DeleteMarkResult { Marked, Protected, NotFound }

pub enum Change {   // fire-and-forget, applied in order, batched into one transaction per 100 ms
    SegmentOpened { key: StreamKey, path: PathBuf, run_seq: u64, discontinuity: bool,
                    wall_start: WallUs, media_start: u64, init_len: u32, sample_entry: Arc<SampleEntry> },
    SegmentProgress { path: PathBuf, durable_len: u64, fragments: u32, wall_end: WallUs, media_end: u64 },
    SegmentSealed { path: PathBuf, len: u64, fragments: u32, wall_end: WallUs, media_end: u64 },
    Event(Event),
}

impl CatalogHandle {
    pub fn open(path: &Path) -> Result<Self, CatalogError>;      // creates or migrates; OPEN_SEGMENT_GRACE
    pub fn open_with_grace(path: &Path, open_segment_grace: Duration) -> Result<Self, CatalogError>;
    pub fn notify(&self, change: Change);                        // never blocks; admission may withhold
    pub async fn ensure_camera(&self, id: &CameraId, name: &str, vendor: Option<&str>) -> Result<(), CatalogError>;
    pub async fn register_recording_root(&self, path: &Path, identity: RootIdentity) -> Result<(), CatalogError>;
    pub async fn configure_video_roots(&self, roots: &[PathBuf]) -> Result<(), CatalogError>;
    pub async fn cameras(&self) -> Result<Vec<CameraRow>, CatalogError>;
    pub async fn segments_limited(&self, key: &StreamKey, range: TimeRange, limit: usize) -> Result<Vec<SegmentRow>, CatalogError>; // overlapping, by (wall_start, id), SQL LIMIT before materialization; unsealed rows need a fragment and end at MIN(now, last progress + grace)
    pub async fn segment(&self, id: SegmentId) -> Result<Option<SegmentRow>, CatalogError>;      // None in deletion intent
    pub async fn segment_by_path(&self, path: &Path) -> Result<Option<SegmentRow>, CatalogError>; // None in deletion intent
    pub async fn writing_segments_page(&self, after: Option<SegmentCursor>, limit: usize)
        -> Result<Vec<StartupRow>, CatalogError>;                                                // sealed = 0, limit clamped to 1..=512
    pub async fn newest_sealed(&self, key: &StreamKey) -> Result<Option<SegmentRow>, CatalogError>;
    pub async fn total_bytes(&self) -> Result<u64, CatalogError>;                                // open files at their written length
    pub async fn mark_empty_writing_deleting(&self, id: SegmentId) -> Result<DeleteMarkResult, CatalogError>; // any open row, whatever its fragment count
    pub async fn mark_deleting_if_unprotected(&self, id: SegmentId, now: WallUs,
        margin: Duration) -> Result<DeleteMarkResult, CatalogError>;
    pub async fn mark_deleting_if_unprotected_at_root(&self, id: SegmentId,
        expected_root: &Path, now: WallUs, margin: Duration) -> Result<DeleteMarkResult, CatalogError>;
    pub async fn deleting_segments_page(&self, after: Option<SegmentCursor>, limit: usize)
        -> Result<Vec<StartupRow>, CatalogError>;
    pub async fn contains_segment_path(&self, path: &Path) -> Result<bool, CatalogError>;
    pub async fn segment_root(&self, id: SegmentId) -> Result<Option<PathBuf>, CatalogError>;
    pub async fn delete_segment(&self, id: SegmentId) -> Result<(), CatalogError>;              // only a row in deletion intent
    pub async fn mark_sealed(&self, path: &Path, len: u64, fragments: u32, wall_end: WallUs, media_end: u64) -> Result<(), CatalogError>;
    pub async fn adopt_segment(&self, row: NewSegment) -> Result<SegmentId, CatalogError>; // upsert by path from a scan of a file on disk
    pub async fn events_after(&self, camera: Option<&CameraId>, range: TimeRange,
        after: Option<EventCursor>, limit: usize) -> Result<Vec<EventRow>, CatalogError>;     // limit capped at 10,000; 8 MiB owned bytes per page
    pub async fn retention_candidates(&self, root: Option<&Path>,
        after: Option<SegmentCursor>, now: WallUs, margin: Duration,
        limit: usize) -> Result<RetentionPage, CatalogError>;
    pub async fn expired_candidates(&self, key: &StreamKey, cutoff: WallUs,
        after: Option<SegmentCursor>, now: WallUs, margin: Duration,
        limit: usize) -> Result<RetentionPage, CatalogError>;
    pub async fn insert_event(&self, event: &Event) -> Result<EventId, CatalogError>;
    pub async fn retain_event(&self, id: EventId, until: Option<WallUs>) -> Result<EventRow, CatalogError>;
    pub async fn maintain_events(&self, now: WallUs) -> Result<(), CatalogError>;
    pub async fn stats(&self) -> Result<CatalogStats, CatalogError>;
    pub fn metrics(&self) -> Arc<CatalogMetrics>;
    pub fn shutdown(&self);   // closes admission, drains the bounded tail, joins; idempotent
    // With the `test-support` feature only: writing_segments, deleting_segments, oldest_sealed,
    // mark_deleting(id) -> DeleteMarkResult (no protection check).
}

pub struct CatalogRootRegistry { /* synchronous, no worker thread */ }
pub enum CatalogRootExpectation { Known(RootIdentity), LegacyOwned, Fresh }
impl CatalogRootRegistry {
    pub fn open(path: &Path) -> Result<Self, CatalogError>;
    pub fn expectations_with_progress(&mut self, roots: &[PathBuf], page_complete: impl FnMut())
        -> Result<Vec<CatalogRootExpectation>, CatalogError>;
}
```

Schema version 10 has `camera`, `sample_entry`, `segment`, `event`, `catalog_state`,
`recording_root`, `recording_root_state`, and `schema_version`. `recording_root` binds each
configured spelling to a unique 128-bit lowercase-hex marker; `recording_root_state` keeps the
fail-closed pre-identity migration bit. `segment` stores `root` and `deleting`; partial indexes
order global and per-root sealed candidates by explicit `(wall_start_us, id)` keys, and a
stream-local partial index orders age work. `event` stores built-in `kind` and vendor `kind_other`
separately, `retain_until_us`, and `storage_bytes`. Its camera and global time indexes use explicit
`(nvr_time_us, id)` ordering; partial indexes cover deadline-leading active protection, inactive-age
work, and protection checks by camera/deadline/receipt time. `catalog_state` transactionally tracks
event rows, logical string bytes, and pruned rows. Migrations add event retention in v2, deletion
intent in v3, injective vendor kinds in v4, root/global indexes in v5, event accounting in v6, the
partial camera-horizon index in v7, the explicit tie-key plus retention access paths in v8, durable
recording-root identities in v9, and sample entries unique by `(hash, config)` in v10, a table
rebuild that keeps every id and checks the foreign keys before it commits.

Open reads the exact single schema version before persistent pragmas or writes and returns
`UnsupportedSchema` above version 10. Open validates `segment.sealed` and `segment.deleting` as
integer 0/1 values before state-filtered queries can hide a malformed row. Row decoding checks
identifiers, enum spellings, booleans, signed-to-unsigned conversions, and aggregates; a
violation returns `Corrupt`.
Event listings use separate camera/global statements and an exclusive `(nvr_time_us, id)`
cursor, with 10,000-row and 8 MiB approximate materialized-owned-row ceilings (fixed row
storage plus owned camera, kind, and payload strings). Protection pages omit
payload/source/kind/state, contain at most 512 rows, and continue by `(retain_until_us, id)`.
Event lifecycle maintenance spends at most 1,000 row-work units per transaction, carries its
inactive scan cursor across zero-match pages, and applies the recording horizon, 90-day age,
two-million-row, or 512 MiB logical-string limits. Active marks are preserved and admitted up to
100,000 rows or 128 MiB. `Other` kind values use the separate tagged payload column; the v4
migration moves bare vendor values into it, and a read rejects an unknown bare kind as corrupt.
Event insertion performs at most the same 1,000-row pruning work before admission. A direct
insertion returns `EventStorageLimit` when it still cannot fit; an async batch omits that event
and commits its segment lifecycle changes. Expired marks are cleared incrementally and count
as inactive for later pruning. Bounding camera strings also releases excess backing capacity.

Retention candidate SQL materializes a bounded raw indexed page before excluding active
same-camera protection. `RetentionPage::next` and `scanned` describe that raw page even when
`rows` is empty, so pass page/row/time budgets can stop between statements. Age input first
seeks starts before the cutoff and checks end time outside the raw page. Equal-key continuation
uses disjoint same-key/id and later-key index seeks, and protection checks seek beyond inactive
and expired history before checking overlap.
`mark_deleting_if_unprotected` repeats the active-protection predicate and records deletion
intent in one FULL-synchronous transaction. Callers mutate files only after `Marked`.
Root assignment and startup writing/deleting scans use stable pages of at most 512 rows.
Non-null root assignments are preserved. An open row whose pair is empty or confirmed
absent enters the same FULL-synchronous deletion-intent ownership before filesystem removal,
whatever its fragment count, and exact path lookups avoid a global in-memory deletion set.
Every write transaction begins `IMMEDIATE`, so the busy timeout covers lock acquisition, and
migration rereads the schema version under the write lock. Open creates missing parents 0700
and a missing database file 0600.

## `iris-store`

```rust
pub trait StoreObserver: Send + Sync {
    fn segment_opened(&self, info: SegmentOpened);
    fn segment_progress(&self, info: SegmentProgress);   // after each fdatasync
    fn segment_sealed(&self, info: SegmentSealed);
}
pub struct SegmentOpened { pub key: StreamKey, pub path: PathBuf, pub run_seq: u64, pub discontinuity: bool,
    pub wall_start: WallUs, pub media_start: u64, pub init_len: u32, pub sample_entry: Arc<SampleEntry> }
pub struct SegmentProgress { pub path: PathBuf, pub durable_len: u64, pub fragments: u32, pub wall_end: WallUs, pub media_end: u64 }
pub struct SegmentSealed { pub path: PathBuf, pub len: u64, pub fragments: u32, pub wall_end: WallUs, pub media_end: u64 }
pub struct RootIdentity([u8; 16]);   // defined in iris_core::id, re-exported here
pub enum RootExpectation { Known(RootIdentity), LegacyOwned, Fresh }
pub struct ApprovedRoot { /* configured path, durable identity, physical device/inode */ }

impl WriterHandle {
    pub fn spawn_with_observer(root: PathBuf, policy: StoragePolicy, queue_capacity: usize,
                               observer: Arc<dyn StoreObserver>) -> io::Result<WriterHandle>;
    pub fn spawn_with_approved_roots(roots: Vec<ApprovedRoot>, policy: StoragePolicy,
        queue_capacity: usize, min_free_percent: u8,
        observer: Arc<dyn StoreObserver>) -> io::Result<WriterHandle>;
}
pub fn remove_segment(mp4_path: &Path) -> io::Result<u64>;            // idempotently unlinks .mp4 and .idx independently, fsyncs the directory, returns bytes found
pub fn admit_recording_root(path: &Path, expectation: RootExpectation) -> io::Result<ApprovedRoot>;
pub fn remove_segment_beneath(root: &ApprovedRoot, mp4_path: &Path) -> io::Result<u64>;
pub fn free_space(root: &Path) -> io::Result<FreeSpace>;              // FreeSpace { total: u64, available: u64 } via statvfs
```

## `iris-ingest` (live ring)

```rust
pub struct LiveFrame { pub key: StreamKey, pub sample_entry: Arc<SampleEntry>, pub data: Bytes,
    pub duration: u32 /* the regulariser's nominal, provisional */, pub is_sync: bool,
    pub decode_time: u64 /* provisional: fragment start + index × nominal */, pub wall: WallUs }
#[derive(Clone)] pub struct LiveRing;                      // one per stream, cheap to clone
impl LiveRing {
    pub fn new(capacity: usize) -> Self;                  // subscriber fan-out capacity in frames (256)
    pub fn subscribe(&self) -> LiveSubscription;
    pub fn init_segment(&self) -> Option<Bytes>;          // current ftyp+moov, None before the first frame
    pub fn recent_from_sync(&self) -> Vec<Arc<LiveFrame>>; // frames since the last sync frame, for fast start
}
pub struct LiveSubscription;
impl LiveSubscription { pub async fn next(&mut self) -> Result<Arc<LiveFrame>, LiveError>; } // LiveError::Lagged(n): resync at the next sync frame; LiveError::Closed: the stream is gone
pub struct IngestOptions { /* the other policy fields */ pub live: Option<LiveRing> }
```
`run_stream` publishes every frame to `options.live` as soon as it is pushed into the
assembler (before its final duration is known), and publishes a new init segment when the
sample entry changes. Recording-sink rejection does not interrupt this publication.

The fast-start cache begins at a sync frame and retains at most 600 frames and 64 MiB of
payload. If either limit would be crossed, it releases that run and accepts no dependent
frames until a later sync frame fits. Subscriber fan-out retains each frame once for all
subscribers, with both the `capacity` frame limit and a 64 MiB aggregate payload limit per
stream; it retains no fan-out payload when no subscribers exist. Eviction produces
`LiveError::Lagged`, after which that subscription discards dependent frames and returns the
next sync frame. The fast-start and fan-out windows hold shared `Bytes`, so overlapping
frames do not duplicate their payload allocation.

## `iris-events`

```rust
pub struct CameraEndpoint { pub camera: CameraId, pub host: String, pub http_port: u16, pub https: bool, pub tls_verify: bool, pub username: String, pub password: String }  // base_url() builds the scheme://host:port prefix; host passes iris_core::config::is_valid_host
pub trait EventSink: Send + Sync { fn publish(&self, event: Event); }
pub struct EventStats;    // events(), messages(), reconnects(), renewals(), errors(), connected(), pull_latency(), last_event_wall(), last_message_wall()
pub async fn run_dahua_cgi(endpoint: CameraEndpoint, sink: Arc<dyn EventSink>, stats: Arc<EventStats>, shutdown: CancellationToken);
pub async fn run_onvif_pullpoint(endpoint: CameraEndpoint, sink: Arc<dyn EventSink>, stats: Arc<EventStats>, shutdown: CancellationToken);
pub mod dahua { pub struct RawEvent { pub code: String, pub action: String, pub index: u32, pub data: Option<String> }
    pub fn parse_part(body: &str) -> Option<RawEvent>; pub fn normalize(camera: &CameraId, raw: &RawEvent, now: WallUs) -> Option<Event>; }
pub mod onvif { #[non_exhaustive] pub enum SessionError { Onvif(oxvif::OnvifError), NoEventsService, Timeout, HttpClient(reqwest::Error), Cancelled, InvalidEndpoint }
    pub fn kind_for_topic(topic: &str) -> EventKind; pub fn normalize_notification(camera: &CameraId, message: &NotificationMessage, now: WallUs) -> Option<Event>; pub fn parse_utc(text: &str) -> Option<WallUs>; }
pub mod media { #[non_exhaustive] pub enum MediaUrlError { Session(onvif::SessionError), NoMediaService, NoProfile, AdvertisedUrl }  // kind() -> &'static str for logs and API answers
    pub struct ProfileStream { pub token: String, pub name: String, pub url: Url }
    pub const MAX_STREAM_PROFILES: usize = 8; pub const MEDIA_LOOKUP_DEADLINE: Duration = 60 s;
    pub async fn snapshot_url(endpoint: &CameraEndpoint) -> Result<Url, MediaUrlError>;
    pub async fn stream_urls(endpoint: &CameraEndpoint) -> Result<Vec<ProfileStream>, MediaUrlError>; }
pub mod discovery { pub struct DiscoveredCamera { pub endpoint: String, pub host: String, pub port: u16, pub https: bool, pub name: Option<String>, pub hardware: Option<String>, pub location: Option<String> }
    pub const MAX_DISCOVERED: usize = 64; pub const MAX_TEXT_CHARS: usize = 128; pub const MAX_LIST_ENTRIES: usize = 16;
    pub async fn discover(wait: Duration) -> io::Result<Vec<DiscoveredCamera>>; }
pub const MAX_ADVERTISED_URL_LEN: usize = 2_048;
```
Dahua feed: `GET /cgi-bin/eventManager.cgi?action=attach&codes=[All]&heartbeat=5`, HTTP digest
(MD5), linear incremental `multipart/x-mixed-replace` parsing with the bounded boundary from the
response header; a part without a `Code=` line is a heartbeat; no completed part for 2 × heartbeat
+ 5 s means the feed stalled; reconnect with 1 s → 30 s independently seeded backoff.
`TimeChange`/`NTPAdjustTime` map to `TimeChange`,
`VideoMotion` to `Motion`, `SmartMotionHuman` to `Human`, `SmartMotionVehicle` to `Vehicle`,
`CrossLineDetection` to `LineCross`, `CrossRegionDetection` to `Intrusion`, `VideoBlind` to
`Tamper`, `AudioMutation`/`AudioAnomaly` to `Audio`, `FaceDetection` to `Face`, `VideoLoss` to
`VideoLoss`, `RtspSessionDisconnect` to `Disconnect`, anything else to `Other(code)`;
`action` Start/Stop/Pulse map to the states, `State` maps to `Pulse`. Dahua data is always valid
JSON and retains nonzero indexes even without vendor data.

ONVIF SOAP is restricted to the configured full origin, follows no authenticated redirects and no
proxy, caps response bodies at 1 MiB on every status, refuses advertised URLs over 2,048 bytes,
and applies independent XML depth/node/attribute/text limits. Pull-point renewal derives from the
camera-granted current/termination times and is capped at 8 min with a 750 ms anti-spin floor for
zero or tiny leases. Authentication retries wait at least 30 s; host-time fallback is limited to a
structured clock-authentication fault identified by code, subcode, or reason, excluding free-form
Detail. Cancellation with a reference runs one bounded unsubscribe and clears connected state.
Topic matching is QName-segment-aware (`tns1:VideoSource/MotionAlarm`, `CellMotionDetector/Motion`,
and `MotionRegionDetector/Motion` to `Motion`; `FieldDetector/ObjectsInside` to `Intrusion`;
`LineDetector/Crossed` to `LineCross`; `TamperDetector/Tamper` and `GlobalSceneChange` to
`Tamper`; `AudioAnalytics/Audio/DetectedSound` to `Audio`; `ObjectDetector/Face` to `Face`;
anything else to `Other(topic)`), property operations precede state extraction, and bounded
`ElementItem` content retains child-adjacent mixed-content whitespace without analytics
interpretation. The media lookups (`snapshot_url`, `stream_urls`) share the transport, the 20 s
call deadline, and the clock fallback, run under a 60 s lookup deadline, move every URL onto the
configured camera, and expose only `MediaUrlError::kind` to logs and API answers. Camera text
reaches a log line only through `iris_core::text::LogSafe`.

## `iris-api`

```rust
pub struct ApiState {
    pub catalog: CatalogHandle,
    pub live: Arc<LiveRegistry>,                 // LiveRegistry::get(&StreamKey) -> Option<LiveRing>, insert(key, ring)
    pub events: tokio::sync::broadcast::Sender<Arc<Event>>,
    pub tokens: Vec<String>,                     // bearer tokens; empty only with api.allow_unauthenticated on a loopback listener
    pub capability_secret: CapabilitySecret,     // 32 random bytes kept at <state_dir>/capability.key (0600); signs media capabilities
    pub recording_roots: Vec<ApprovedRoot>,      // media is opened only beneath the root owning a row
    pub public_metrics: bool,                    // serve /metrics without a token ([api] public_metrics)
    pub snapshot_sources: RwLock<HashMap<CameraId, SnapshotSource>>,   // SnapshotSource::new(host, port, https, tls_verify, username, password, method): Dahua CGI or ONVIF, with the ONVIF URL cache its clones share; provisioning edits it
    pub active_cameras: RwLock<HashMap<CameraId, ActiveCamera>>,       // the running cameras, configured or added (`provisioned` marks the added ones); catalog camera rows remain historical
    pub control: Option<CameraControl>,          // the daemon's control loop while [provisioning] is enabled; POST/DELETE /api/cameras relay through it
    pub stats: Arc<ApiStats>,                    // counters and admission budgets; ApiStats::new_with_media_limits(streaming, bounded)
    pub started: WallUs,                         // diagnostic wall timestamp
    pub started_monotonic: Instant,              // health/metrics uptime
    pub shutdown: CancellationToken,             // ends long-lived WebSocket sessions
}
pub struct CameraControl;                        // Clone; wraps an mpsc::Sender<ControlRequest> the daemon creates with [provisioning] enabled
pub enum ControlRequest {                        // what the provisioning and ONVIF routes hand the daemon's monitor loop
    Add { camera: Box<CameraConfig>, reply: oneshot::Sender<Result<ActiveCamera, ProvisionError>> },
    Remove { id: CameraId, reply: oneshot::Sender<Result<(), ProvisionError>> },
    Discover { wait: Duration, reply: oneshot::Sender<Result<Vec<DiscoveredCamera>, ProvisionError>> },         // GET /api/onvif/devices
    StreamUrls { endpoint: Box<CameraEndpoint>, reply: oneshot::Sender<Result<Vec<ProfileStream>, ProvisionError>> }, // POST /api/onvif/streams
}
#[non_exhaustive] pub enum ProvisionError { Duplicate(CameraId), Configured(CameraId), Unknown(CameraId), Limit(usize),
    Port(u16), Invalid(String), Store(String), Lifecycle(String),    // 409, 409, 404, 400, 400, 400, 503, 503 (Store and Lifecycle details are logged, the bodies fixed)
    Discovery(String), Camera(String), Busy }                        // 503, 502, 503 (Retry-After: 1)
pub struct ServeLimits {                         // Default: 256 connections, 10 s request head, 30 s write stall
    pub max_connections: usize,
    pub header_read_timeout: Duration,
    pub write_stall_timeout: Duration,
}
pub const MAX_LIVE_MESSAGE_BYTES: usize;         // 1 + 16 MiB; also MSG_INIT (0x01), MSG_FRAGMENT (0x02)
pub fn router(state: Arc<ApiState>) -> axum::Router;
pub async fn bind(listen: SocketAddr) -> io::Result<tokio::net::TcpListener>;
pub async fn serve_listener(listener: tokio::net::TcpListener, router: axum::Router, limits: ServeLimits, shutdown: CancellationToken) -> io::Result<()>;
pub async fn serve(listen: SocketAddr, router: axum::Router, limits: ServeLimits, shutdown: CancellationToken) -> io::Result<()>;
```

`serve_listener` speaks HTTP/1.1 only and takes one of `limits.max_connections` slots before
each `accept`; the slot stays with the connection, including after a WebSocket upgrade. A
request head must arrive within `header_read_timeout` (idle kept-alive connections close after
it too), and a response write without progress for `write_stall_timeout` closes the connection.
Cancelling `shutdown` stops accepting, finishes in-flight responses, and returns when every
connection has closed.

| Method | Path | Behaviour |
|---|---|---|
| GET | `/api/health` | unauthenticated; JSON `{status, uptime_s, version}` |
| GET | `/metrics` | Prometheus text, root bearer token required unless `public_metrics`: `ApiStats` (including media, WebSocket, and snapshot admission rejections, rows outside the approved roots, and oversized live closes), event-WebSocket lagged-message count, catalog queue count/bytes and peaks, coalescing/rejections/reconciliation, failed change batches, event state, and database/WAL sizes |
| GET | `/api/cameras` | JSON list of the running cameras, configured or added through the API (`provisioned` marks the added ones); historical catalog rows are not active cameras |
| POST | `/api/cameras` | adds a camera described like a `[[camera]]` table; root token only, 403 without `[provisioning] enabled`; 201, or 400 (invalid camera, port outside `connect_ports`, limit reached, or a refused JSON body), 409 (id in use), 413 (body over 64 KiB), 503 (store or lifecycle failure, fixed body) |
| DELETE | `/api/cameras/{camera}` | stops and forgets a camera the API added; root token only, 403 without the control; 204, or 404 (unknown), 409 (configured camera), 503 |
| GET | `/api/onvif/devices?wait_ms` | the cameras a bounded WS-Discovery probe finds; root token only, 403 without the control; 400 for a `wait_ms` outside 500 to 10,000, 503 when the probe cannot be sent or four lookups run already |
| POST | `/api/onvif/streams` | the RTSP URLs of one camera's ONVIF profiles, moved onto the given host; same gating; 400 for a host that is not a bare name or address, a port outside `connect_ports`, or a refused JSON body, 502 when the camera gives no usable answer, 503 when four lookups run already |
| GET | `/api/cameras/{camera}` | a running camera plus health for each recorded stream |
| GET | `/api/streams/{camera}/{kind}/segments?from=<rfc3339\|unix_us>&to=…` | JSON objects with `id`, `camera`, `kind`, `run_seq`, `discontinuity`, `wall_start` and `wall_start_us`, `wall_end` and `wall_end_us`, `media_start`, `media_end`, `init_len`, `durable_len`, `fragments`, `bytes`, `sealed`, `codecs`, `width`, and `height`; no paths |
| GET | `/api/segments/{id}/file.mp4` | the recording file with `Range` support, `video/mp4`, served from the descriptor its sidecar was validated against beneath the owning approved root; full and range bodies stop at one validated complete sidecar extent; a row outside every root is 404; the extension keeps ffmpeg's HLS demuxer (`allowed_segment_extensions`) happy |
| GET | `/api/streams/{camera}/{kind}/hls/vod.m3u8?from&to` | byte-range VOD playlist across segments: one `EXT-X-MAP` per segment (`BYTERANGE="init_len@0"`), media segments that start at a sync fragment and run until the first sync fragment after their third fragment (continuations always join), `EXT-X-DISCONTINUITY` before a segment with `discontinuity`, after a skipped row, or with a different sample entry (the catalog's `sample_entry_id`, never the CRC alone), fragments read from each segment's sidecar (`iris_mp4::index::read_index_file_bounded`); clipping includes preceding sync preroll; `#IRIS-FRAGMENT` comments describe the served fragments |
| GET | `/api/streams/{camera}/{kind}/export.mp4?from&to` | streamed fMP4: the first segment's init with a version-1 `elst` trimming presentation to `[from, to)`, then whole fragments including sync preroll, rebased onto a timeline from zero with fresh sequence numbers and trailers; HTTP 400 before streaming if actual initialization bytes differ, including adopted files without catalog codec hashes; `Content-Disposition: attachment` |
| GET | `/api/streams/{camera}/{kind}/hls/live.m3u8` | LL-HLS from the live ring, with blocking reload (three target durations, then 503) and memory-backed parts and segments; `EXT-X-TARGETDURATION` and `PART-TARGET` pinned per feeder, complete segments kept until three target durations follow (at most 12), `EXT-X-VERSION:6`, `EXT-X-PROGRAM-DATE-TIME` after each discontinuity; non-increasing decode timestamps, lag, and budget breaches end the open segment at sync with `EXT-X-DISCONTINUITY`, and `EXT-X-DISCONTINUITY-SEQUENCE` preserves the epoch as entries leave the playlist |
| GET | `/api/streams/{camera}/{kind}/hls/{init.mp4,seg/{msn}/media.m4s,part/{msn}/{index}/part.m4s}` | memory-backed LL-HLS child resources named by the live playlist |
| GET | `/api/streams/{camera}/{kind}/keyframe?t=...` | initialization plus one bounded single-sample sync fragment at or before the requested wall time |
| GET (WebSocket) | `/api/streams/{camera}/{kind}/live` | binary frames: message type byte `0x01` + init segment, then `0x02` + one complete `prft` + `moof` + `mdat` + trailer fragment per frame built with `iris_mp4::fragment::encode_fragment` on a single sample (sequence counts up, `decode_time` from the frame); every message is at most `MAX_LIVE_MESSAGE_BYTES` (1 + 16 MiB); on `Lagged` or an oversized dependent frame, resend init and resume at the next sync frame; an oversized sync frame closes with 1009 |
| GET (WebSocket) | `/api/events` | JSON `Event` per message as published; a receiver that lags the bounded ring is closed with code 1013 and a bounded gap/retry reason, then may list the relevant catalog window |
| GET | `/api/events?camera&from&to&limit&after_us&after_id` | bounded JSON `EventRow` pages (`limit` defaults to 200, clamped to 1–10,000); cursor values appear together and are exclusive |
| POST | `/api/events/{id}/retain` | set or clear the bounded retention horizon for the event; the body is read as JSON whatever its `Content-Type`, an empty body clears, an unknown field or unparseable time is 400 |
| GET | `/api/cameras/{camera}/snapshot.jpg` | redirect-free, proxy-free proxied `GET /cgi-bin/snapshot.cgi?channel=1` with digest auth (the first supported challenge among the `WWW-Authenticate` headers) or, for a camera whose `snapshot` is `onvif`, the URL its Media service names for the first profile, rebased onto the configured origin, reused for an hour and forgotten after a failed fetch; 5 s timeout, an 8 MiB streamed ceiling, one request per camera in flight (503 with `Retry-After: 1` otherwise), a body starting `FF D8 FF` with an `FF D9` in its last 64 KiB served through that marker, and fixed `image/jpeg`, `nosniff`, `no-store` response policy |

Auth: clients send the root credential only as `Authorization: Bearer <token>`, including for the
initial WebSocket or playlist request. Authenticated playlists mint bounded tamper-resistant
`?token=` capabilities limited to the exact follow-up media path and `GET`/`HEAD`, rather than
copying the root bearer; the tag is an HMAC-SHA256 under the daemon's `capability_secret`, so a
captured URL reveals nothing about a token. Rolling live-HLS capabilities last 15 minutes; immutable
VOD children cover the selected span plus a 15-minute playback margin, capped at seven days. Other
query bearer tokens are rejected; 401 otherwise. All request bodies and query values are bounded;
paths are looked up only through the catalog (no user-supplied file paths).

Time ranges accept Unix microseconds or ASCII RFC 3339, default to one hour, and span at
most seven days. RFC 3339 dates use Gregorian month/leap-year rules, clocks and numeric offsets
use their grammar bounds, and leap seconds are rejected because the recorder has no leap-second
table. An unrepresentable span or default start returns HTTP 400. Malformed Unicode timestamps
return HTTP 400.
Event pages use the final row's `nvr_time_us` and positive `id` as the paired continuation,
so tied timestamps are neither repeated nor omitted. Catalog admission overload and retained
event capacity exhaustion return HTTP 503; a full API admission budget returns HTTP 503 with
`Retry-After: 1` (`ApiError::Busy`). HTTP 500 bodies are `{"error":"internal error",
"request_id":<n>}`, with the detail logged under the same id.
Every live or event WebSocket send observes daemon shutdown and has a two-second deadline. Client
gauges use drop guards, so timeout, cancellation, peer close, and ordinary completion release them.
At most 64 live sessions (16 per stream) and 32 event sessions are admitted; their slots are taken
before the upgrade. Inbound messages and frames are limited to 4 KiB, and any data message closes
the session with 1008.

Disk-backed requests open media and sidecar beneath the approved root that owns the row, validate
the sidecar read from its descriptor against catalog/header/media identity (a zero header run or
sample-entry hash, as recovery rebuilds it, is unknown), and capture a stable complete extent with
the media's device and inode; exports and keyframes reopen beneath the root and require that inode.
Limits are 4,096 rows, 131,072 records per sidecar, 100,000 selected
records, a 16 MiB playlist, a 4 MiB init, and a 66 MiB stored fragment. Media responses are
admitted in two process-wide classes, streaming (whole or open-ended files, ranges above 1 MiB,
exports; `[api] max_streaming_media`, default 8) and bounded (VOD, keyframes, LL-HLS resources,
ranges of at most 1 MiB; `[api] max_bounded_media`, default 32), with admission held through
detached sidecar/read/encode work and final response-buffer/body drop. Raw bodies reserve their
capacity-one channel before each bounded read
and attach admission to every emitted chunk. Export reserves its sole queue slot before
materializing the next fragment. Fragment transforms verify source CRC and parsed
timing/sample/sync facts. A VOD omission carries one pending discontinuity to the next usable row.
Keyframe lookup enforces its two-minute lower bound; implicit newest is `private, no-store`.
`live_viewers` counts external live WebSockets only. Metrics catalog aggregates refresh under one
cache-owned single-flight at most once per second; request cancellation cannot abandon it. Refresh
failure retains stale data, while cold failures omit aggregates until that cadence expires.

Each VOD fragment has an ignored playlist comment
`#IRIS-FRAGMENT:WALL-US=<i64>,DURATION-TICKS=<u32>` in playback order, including sync preroll.
`WALL-US` is the fragment's Unix wall time in microseconds; `DURATION-TICKS` is its media
duration at 90,000 ticks per second. Clients accumulate ticks before converting units and
map each fragment to its own wall time, preserving gaps and avoiding cumulative rounding.

## `irisd run`

```
irisd run --config <toml>
```
Startup order: load and fully validate config/derived capacities/timers (warning when the file is
accessible beyond root and the daemon's account and group; no `[[camera]]` is accepted when
`[provisioning]` is enabled) → read `state_dir/cameras.toml` (no-follow, nonblocking, a regular
file of at most 1 MiB; invalid, unparsable, or repeated entries dropped with a warning; read before
the permission repair by design) and append the provisioned cameras → arm CPU sampling, tickers,
SIGTERM, and SIGINT → raise the soft descriptor limit to the hard limit → prepare the state
directory (created 0700; directory and catalog files checked for owner, type, and link count, and
group or other permissions removed) → synchronously migrate `state_dir/irisd.db` and resolve each
configured root expectation → admit roots by no-follow marker plus physical identity and reject
aliases → apply Landlock before any thread → open the catalog worker → register those
identities → assign only previously unassigned rows in bounded pages → ensure cameras →
complete paged deletion intents under their exact approved roots → page all open rows, skipping
those whose persisted root is not approved → `recover_segment_beneath` → `mark_sealed`; scan
roots for files unknown to the catalog through a streaming no-follow walk (a directory that cannot
be read is reported and skipped) and `adopt_segment` them from a header scan → bind the API
listener → spawn the writer with a catalog observer → build live rings → spawn ingest tasks
(`run_stream`) and event tasks (`run_dahua_cgi` / `run_onvif_pullpoint` per `camera.events`) →
serve the already-bound listener → `sd_notify(READY=1)`. Reconciliation extends the startup
deadline on one heartbeat whose cadence holds across catalog pages, entry enumeration, and media
scans however short each await is, starting before the first file. Landlock permits reading the
config, `/etc`, and `/proc`, reading/writing regular files and directories beneath the video roots
and state dir, connecting over TCP to camera and provisioning ports, and binding the API port; its
ABI 1 filesystem and ABI 4 TCP rights are required unless `--no-sandbox`, and later rights and
scopes are best effort. Then every 30 s: `WATCHDOG=1` only if every ingest task produced a frame or
reconnected in the interval; every 60 s: retention starts with one bounded event-maintenance step
and a retry of at most 16 earlier deletion intents (or 100 ms) beneath approved roots, then applies
free-space, quota, and camera-age policies through indexed root-aware pages. A pass stops at 512
deletion attempts, 64 pages, 1,024 candidate rows, or one second; candidate rows count raw indexed
input before protection filtering, and the catalog's SQL plus the atomic mark are the only
protection checks. A root whose free-space probe fails is skipped for the pass without marking its
rows. The first free-space root rotates between passes so persistent pressure on one device cannot
starve the others. Exact-root, global-quota, and per-stream age cursors persist across bounded
passes, retain the last handled candidate at a partial-page budget exit, and adopt raw page progress
only after complete returned-candidate consumption. Satisfied root/quota policies reset; age wraps
only after indexed exhaustion. Free-space, quota, root, and age policy changes prune or reset their
bounded state. `mark_deleting_if_unprotected_at_root` → `Marked` → `remove_segment_beneath` →
`delete_segment`; other outcomes leave the files untouched, and a row outside its root's layout is
never marked. Failed transitions remain for the next pass's retry and for startup, and do not count
as quota/deletion progress. Reporting, retention, and self-check execute as owned single-flight
jobs; blocking filesystem operations do not run on the monitor thread. A report or retention job
past its 30 s deadline keeps its state and counts a timeout; its owner starts no further job while a
raw filesystem call of the dropped job is in flight, and a call still in flight ten minutes later
stops the daemon. Shutdown on SIGTERM/SIGINT uses one 25-second absolute budget: emit `STOPPING=1`,
cancel once, drain all producers against one shared grace, then abort-and-observe any remainder and
attempt writer, API, and catalog closure exactly once before returning the first saved error.

Packaging: `packaging/systemd/irisd.service` (hardening set from
`../research/03-language-runtime.md` §5, `Type=notify`, `WatchdogSec=60`,
`StateDirectoryMode=0700`, `LimitNOFILE=65536`, `SyslogLevelPrefix=no`, and `IPAddressDeny=any` with
a loopback-only `IPAddressAllow=` that a drop-in extends to the camera subnet),
`packaging/sysusers.d/irisd.conf`, `packaging/README.md`.
