# Phase 2 interfaces

The contract between the phase-2 crates. Each crate is implemented against these signatures
so that the crates can be built in parallel; a deviation is a design change and is recorded
here first. Shared types live in `iris-core` so that no two crates need each other for a
type definition. Behaviour is specified in `../research/04-storage-format.md`,
`05-ingest-events.md`, and `07-architecture.md`.

## Shared types (`iris-core`)

```rust
// iris_core::event
pub struct Event {
    pub camera: CameraId,
    pub source: EventSource,       // DahuaCgi | OnvifPullPoint | OnvifMetadata | Manual
    pub kind: EventKind,           // Motion | Human | Vehicle | LineCross | Intrusion | Tamper |
                                   // Audio | Face | VideoLoss | TimeChange | Disconnect | Other(String)
    pub state: EventState,         // Start | Stop | Pulse
    pub nvr_time: WallUs,          // receipt time on the NVR clock (authoritative)
    pub camera_time: Option<WallUs>,
    pub data: Option<String>,      // vendor payload as JSON text, size-capped at 64 KiB
}
// iris_core::config additions
pub struct StorageConfig { pub video_roots: Vec<PathBuf>, pub state_dir: PathBuf,
    pub min_free_percent: u8, /* existing fields unchanged */ }
pub struct ApiConfig { pub listen: SocketAddr, pub tokens: Vec<String> }   // Config.api
pub struct CameraConfig { /* existing */ pub http_port: Option<u16> /* 80, or 443 with https */, pub https: bool, pub tls_verify: bool /* true */, pub retention_days: Option<u32>,
    pub events: Vec<EventSource> /* default [] */ }
impl CameraConfig { pub fn host(&self) -> Option<String> /* host part of rtsp_url */ }
```

## `iris-catalog`

SQLite (rusqlite, bundled) opened in WAL mode with `synchronous = NORMAL`, owned by one
thread named `iris-catalog`. Writes from the store's writer thread must never block on the
database, so the handle offers fire-and-forget notifications plus async queries.

```rust
#[derive(Clone)] pub struct CatalogHandle;
pub struct SegmentId(pub i64); pub struct EventId(pub i64);
pub struct TimeRange { pub from: WallUs, pub to: WallUs }   // half-open [from, to)

pub struct SegmentRow {
    pub id: SegmentId, pub key: StreamKey, pub path: PathBuf, pub run_seq: u64,
    pub discontinuity: bool, pub wall_start: WallUs, pub wall_end: WallUs,
    pub media_start: u64, pub media_end: u64, pub init_len: u32, pub durable_len: u64,
    pub fragments: u32, pub bytes: u64, pub sealed: bool,
    pub codecs: String, pub width: u16, pub height: u16, pub sample_entry_hash: u32,
}
pub struct EventRow { pub id: EventId, pub event: Event }
pub struct CameraRow { pub id: CameraId, pub name: String, pub vendor: Option<String> }
pub struct CatalogStats { pub segments: u64, pub sealed_bytes: u64, pub events: u64 }

pub enum Change {   // fire-and-forget, applied in order, batched into one transaction per 100 ms
    SegmentOpened { key: StreamKey, path: PathBuf, run_seq: u64, discontinuity: bool,
                    wall_start: WallUs, media_start: u64, init_len: u32, sample_entry: Arc<SampleEntry> },
    SegmentProgress { path: PathBuf, durable_len: u64, fragments: u32, wall_end: WallUs, media_end: u64 },
    SegmentSealed { path: PathBuf, len: u64, fragments: u32, wall_end: WallUs, media_end: u64 },
    Event(Event),
}

impl CatalogHandle {
    pub fn open(path: &Path) -> Result<Self, CatalogError>;      // creates the file, migrates
    pub fn notify(&self, change: Change);                         // never blocks
    pub async fn ensure_camera(&self, id: &CameraId, name: &str, vendor: Option<&str>) -> Result<(), CatalogError>;
    pub async fn cameras(&self) -> Result<Vec<CameraRow>, CatalogError>;
    pub async fn segments(&self, key: &StreamKey, range: TimeRange) -> Result<Vec<SegmentRow>, CatalogError>; // overlapping, by wall_start
    pub async fn segment(&self, id: SegmentId) -> Result<Option<SegmentRow>, CatalogError>;
    pub async fn segment_by_path(&self, path: &Path) -> Result<Option<SegmentRow>, CatalogError>;
    pub async fn writing_segments(&self) -> Result<Vec<SegmentRow>, CatalogError>;  // sealed = false
    pub async fn oldest_sealed(&self, limit: usize) -> Result<Vec<SegmentRow>, CatalogError>;
    pub async fn delete_segment(&self, id: SegmentId) -> Result<(), CatalogError>;
    pub async fn mark_sealed(&self, path: &Path, len: u64, fragments: u32, wall_end: WallUs, media_end: u64) -> Result<(), CatalogError>;
    pub async fn adopt_segment(&self, row: NewSegment) -> Result<SegmentId, CatalogError>; // from a scan of an unknown file
    pub async fn events(&self, camera: Option<&CameraId>, range: TimeRange, limit: usize) -> Result<Vec<EventRow>, CatalogError>;
    pub async fn insert_event(&self, event: &Event) -> Result<EventId, CatalogError>;
    pub async fn stats(&self) -> Result<CatalogStats, CatalogError>;
    pub fn shutdown(&self);   // flushes pending changes, joins; idempotent
}
```

Schema: `camera(id TEXT PK, name, vendor)`, `sample_entry(id PK, hash INTEGER UNIQUE, codecs,
width, height, config BLOB)`, `segment(id PK, camera_id, kind, path UNIQUE, run_seq,
discontinuity, wall_start_us, wall_end_us, media_start, media_end, init_len, durable_len,
fragments, bytes, sealed, sample_entry_id)` with index `(camera_id, kind, wall_start_us)`,
`event(id PK, camera_id, source, kind, state, nvr_time_us, camera_time_us, data)` with index
`(camera_id, nvr_time_us)`, `schema_version(version)`.

## `iris-store` additions

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

impl WriterHandle {
    pub fn spawn_with_observer(root: PathBuf, policy: StoragePolicy, queue_capacity: usize,
                               observer: Arc<dyn StoreObserver>) -> io::Result<WriterHandle>;
}
pub fn remove_segment(mp4_path: &Path) -> io::Result<u64>;            // unlinks .mp4 and .idx, fsyncs the directory, returns bytes freed
pub fn free_space(root: &Path) -> io::Result<FreeSpace>;              // FreeSpace { total: u64, available: u64 } via statvfs
```

## `iris-ingest` additions (live ring)

```rust
pub struct LiveFrame { pub key: StreamKey, pub sample_entry: Arc<SampleEntry>, pub data: Bytes,
    pub duration: u32 /* the regulariser's nominal, provisional */, pub is_sync: bool,
    pub decode_time: u64 /* provisional: fragment start + index × nominal */, pub wall: WallUs }
#[derive(Clone)] pub struct LiveRing;                      // one per stream, cheap to clone
impl LiveRing {
    pub fn new(capacity: usize) -> Self;                  // broadcast capacity in frames (256)
    pub fn subscribe(&self) -> LiveSubscription;
    pub fn init_segment(&self) -> Option<Bytes>;          // current ftyp+moov, None before the first frame
    pub fn recent_from_sync(&self) -> Vec<Arc<LiveFrame>>; // frames since the last sync frame, for fast start
}
pub struct LiveSubscription;
impl LiveSubscription { pub async fn next(&mut self) -> Result<Arc<LiveFrame>, LiveError>; } // LiveError::Lagged(n): resync at the next sync frame; LiveError::Closed: the stream is gone
pub struct IngestOptions { /* existing */ pub live: Option<LiveRing> }
```
`run_stream` publishes every frame to `options.live` as soon as it is pushed into the
assembler (before its final duration is known), and publishes a new init segment when the
sample entry changes.

## `iris-events`

```rust
pub struct CameraEndpoint { pub camera: CameraId, pub host: String, pub http_port: u16, pub https: bool, pub tls_verify: bool, pub username: String, pub password: String }  // base_url() builds the scheme://host:port prefix
pub trait EventSink: Send + Sync { fn publish(&self, event: Event); }
pub struct EventStats;    // events(), reconnects(), connected(), last_event_wall()
pub async fn run_dahua_cgi(endpoint: CameraEndpoint, sink: Arc<dyn EventSink>, stats: Arc<EventStats>, shutdown: CancellationToken);
pub async fn run_onvif_pullpoint(endpoint: CameraEndpoint, sink: Arc<dyn EventSink>, stats: Arc<EventStats>, shutdown: CancellationToken);
pub mod dahua { pub struct RawEvent { pub code: String, pub action: String, pub index: u32, pub data: Option<String> }
    pub fn parse_part(body: &str) -> Option<RawEvent>; pub fn normalize(camera: &CameraId, raw: &RawEvent, now: WallUs) -> Option<Event>; }
```
Dahua feed: `GET /cgi-bin/eventManager.cgi?action=attach&codes=[All]&heartbeat=5`, HTTP digest
(MD5), `multipart/x-mixed-replace` with the boundary from the response header; a part without
a `Code=` line is a heartbeat; no part for 2 × heartbeat + 5 s means the feed stalled;
reconnect with 1 s → 30 s backoff. `TimeChange`/`NTPAdjustTime` map to `TimeChange`,
`VideoMotion` to `Motion`, `SmartMotionHuman` to `Human`, `SmartMotionVehicle` to `Vehicle`,
`CrossLineDetection` to `LineCross`, `CrossRegionDetection` to `Intrusion`, `VideoBlind` to
`Tamper`, `AudioMutation`/`AudioAnomaly` to `Audio`, `FaceDetection` to `Face`, `VideoLoss` to
`VideoLoss`, `RtspSessionDisconnect` to `Disconnect`, anything else to `Other(code)`;
`action` Start/Stop/Pulse map to the states, `State` maps to `Pulse`.

## `iris-api`

```rust
pub struct ApiState {
    pub catalog: CatalogHandle,
    pub live: Arc<LiveRegistry>,                 // LiveRegistry::get(&StreamKey) -> Option<LiveRing>, insert(key, ring)
    pub events: tokio::sync::broadcast::Sender<Arc<Event>>,
    pub tokens: Vec<String>,                     // bearer tokens; empty = unauthenticated (LAN dev only)
    pub snapshot_sources: HashMap<CameraId, SnapshotSource>,   // host, port, https, tls_verify, username, password for snapshot.cgi
    pub stats: Arc<ApiStats>,
    pub started: WallUs,
}
pub fn router(state: Arc<ApiState>) -> axum::Router;
pub async fn serve(listen: SocketAddr, router: axum::Router, shutdown: CancellationToken) -> io::Result<()>;
```

| Method | Path | Behaviour |
|---|---|---|
| GET | `/api/health` | unauthenticated; JSON `{status, uptime_s, version}` |
| GET | `/metrics` | unauthenticated Prometheus text: `irisd_*` counters from `ApiStats` and catalog stats |
| GET | `/api/cameras` | JSON list from the catalog |
| GET | `/api/streams/{camera}/{kind}/segments?from=<rfc3339\|unix_us>&to=…` | JSON `SegmentRow`s |
| GET | `/api/segments/{id}/file.mp4` | the recording file with `Range` support, `video/mp4`; the extension keeps ffmpeg's HLS demuxer (`allowed_segment_extensions`) happy |
| GET | `/api/streams/{camera}/{kind}/hls/vod.m3u8?from&to` | byte-range VOD playlist across segments: one `EXT-X-MAP` per segment (`BYTERANGE="init_len@0"`), three fragments per media segment grouped at sync fragments, `EXT-X-DISCONTINUITY` before a segment with `discontinuity` or a different sample entry hash, fragments read from each segment's sidecar (`iris_mp4::index::read_index`), range clipped to `[from, to)` at fragment granularity |
| GET | `/api/streams/{camera}/{kind}/export.mp4?from&to` | streamed fMP4: the first segment's init segment followed by every fragment byte range in order (fragment granularity, no `elst` yet); `Content-Disposition: attachment` |
| GET (WebSocket) | `/api/streams/{camera}/{kind}/live` | binary frames: message type byte `0x01` + init segment, then `0x02` + one `prft`-less `moof`+`mdat` per frame built with `iris_mp4::fragment::encode_fragment` on a single sample (sequence counts up, `decode_time` from the frame); on `Lagged`, resend init and resume at the next sync frame |
| GET (WebSocket) | `/api/events` | JSON `Event` per message as they are published |
| GET | `/api/events?camera&from&to&limit` | JSON `EventRow`s |
| GET | `/api/cameras/{id}/snapshot.jpg` | proxied `GET /cgi-bin/snapshot.cgi?channel=1` with digest auth, 5 s timeout |

Auth: `Authorization: Bearer <token>` or `?token=` for playlists/WebSockets; 401 otherwise.
All request bodies and query values are bounded; paths are looked up only through the catalog
(no user-supplied file paths).

## `irisd run`

```
irisd run --config <toml>
```
Startup order: load config → open catalog (`state_dir/irisd.db`) → ensure cameras → for each
video root, `writing_segments()` → `recover_segment` → `mark_sealed`; scan roots for files
unknown to the catalog and `adopt_segment` them → spawn the writer with a catalog observer →
build live rings → spawn ingest tasks (`run_stream`) and event tasks (`run_dahua_cgi` /
`run_onvif_pullpoint` per `camera.events`) → start the API → apply Landlock (read: config
file; read-write: video roots and state dir; TCP connect: 554 and `http_port`; TCP bind: the
API port) → `sd_notify(READY=1)`. Then every 30 s: `WATCHDOG=1` only if every ingest task
produced a frame or reconnected in the interval; every 60 s: retention (while
`free_space(root).available < min_free_percent`, while `total_bytes() > quota_gib`, or while
a camera's oldest segment is older than `retention_days`: `oldest_sealed` → `remove_segment`
→ `delete_segment`). Shutdown on
SIGTERM/SIGINT: cancel token, ingest flush, writer shutdown, catalog shutdown, `STOPPING=1`.

Packaging: `packaging/systemd/irisd.service` (hardening set from
`../research/03-language-runtime.md` §5, `Type=notify`, `WatchdogSec=60`), `packaging/sysusers.d/irisd.conf`,
`packaging/README.md`.
