//! TOML configuration: cameras and storage policy.
//!
//! The example serves a development API without authentication on the default
//! loopback listener; a deployment lists bearer tokens instead, for example
//! generated with `openssl rand -hex 16`.
//!
//! ```
//! use iris_core::config::Config;
//!
//! let config = Config::from_toml_str(r#"
//! [api]
//! allow_unauthenticated = true
//!
//! [[camera]]
//! id = "cam1"
//! name = "Example"
//! rtsp_url = "rtsp://192.0.2.10:554/cam/realmonitor?channel=1&subtype=0"
//! username = "user"
//! password = "secret"
//! "#)?;
//! assert_eq!(config.cameras.len(), 1);
//! assert!(config.cameras[0].sub_url().is_some());
//! # Ok::<(), iris_core::config::ConfigError>(())
//! ```

use std::collections::HashSet;
use std::fmt;
use std::net::SocketAddr;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::event::EventSource;
use crate::id::{CameraId, StreamKind};

/// Errors from loading configuration.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum ConfigError {
    /// The file could not be read.
    #[error("cannot read configuration file {}: {source}", .path.display())]
    Io {
        /// The path that failed.
        path: PathBuf,
        /// The underlying error.
        #[source]
        source: std::io::Error,
    },
    /// The TOML did not parse or did not match the schema.
    #[error("invalid configuration syntax or schema {}", describe_span(.span.as_ref()))]
    Parse {
        /// Location supplied by the parser, without input text or value-bearing messages.
        span: Option<Range<usize>>,
    },
    /// Two cameras share an id.
    #[error("duplicate camera id {0}")]
    DuplicateCamera(CameraId),
    /// One camera records the same stream kind more than once.
    #[error("camera {camera} records stream kind {kind} more than once")]
    DuplicateRecordedKind {
        /// The camera with the repeated kind.
        camera: CameraId,
        /// The repeated stream kind.
        kind: StreamKind,
    },
    /// One camera requests a stream kind the recorder cannot ingest.
    #[error("camera {camera} requests unsupported recorded stream kind {kind}")]
    UnsupportedRecordedKind {
        /// The camera with the unsupported kind.
        camera: CameraId,
        /// The unsupported stream kind.
        kind: StreamKind,
    },
    /// One camera subscribes to the same event source more than once.
    #[error("camera {camera} subscribes to event source {event_source} more than once")]
    DuplicateEventSource {
        /// The camera with the repeated source.
        camera: CameraId,
        /// The repeated event source.
        event_source: EventSource,
    },
    /// One camera requests an event source the daemon cannot subscribe to.
    #[error("camera {camera} requests unsupported event source {event_source}")]
    UnsupportedEventSource {
        /// The camera with the unsupported source.
        camera: CameraId,
        /// The source that cannot be configured as a subscription.
        event_source: EventSource,
    },
    /// The free-space floor is above the supported share of a root.
    #[error("storage.min_free_percent must be between 0 and {MAX_MIN_FREE_PERCENT}, got {0}")]
    InvalidMinFreePercent(u8),
    /// One configured API bearer is empty or contains only whitespace.
    #[error("api.tokens entries must not be blank")]
    BlankApiToken,
    /// The API has no bearer token and the configuration does not opt into an
    /// unauthenticated development API.
    #[error(
        "api.tokens is empty: add a token of at least {MIN_API_TOKEN_LEN} visible ASCII \
         characters (for example from `openssl rand -hex 16`), or set \
         api.allow_unauthenticated = true on a loopback api.listen address for development"
    )]
    MissingApiToken,
    /// An unauthenticated API was requested on an address reachable from the network.
    #[error("api.allow_unauthenticated requires a loopback api.listen address, got {0}")]
    UnauthenticatedNonLoopback(SocketAddr),
    /// Bearer tokens and the unauthenticated switch contradict each other.
    #[error("api.allow_unauthenticated cannot be combined with api.tokens")]
    ConflictingApiAuthentication,
    /// A bearer token is too short or contains characters outside visible ASCII.
    /// The message names the entry's position, never its value.
    #[error(
        "api.tokens entry {index} must be at least {MIN_API_TOKEN_LEN} visible ASCII \
         characters (0x21 to 0x7e)"
    )]
    WeakApiToken {
        /// Zero-based position of the rejected entry in `api.tokens`.
        index: usize,
    },
    /// A bearer token is the placeholder from the example configuration.
    #[error(
        "api.tokens entry {index} is the example placeholder; generate a token, for example \
         with `openssl rand -hex 16`"
    )]
    PlaceholderApiToken {
        /// Zero-based position of the rejected entry in `api.tokens`.
        index: usize,
    },
    /// The sync interval is outside the supported range.
    #[error(
        "storage.sync_interval_ms must be between {MIN_SYNC_INTERVAL_MS} and \
         {MAX_SYNC_INTERVAL_MS}, got {0}"
    )]
    InvalidSyncIntervalMs(u64),
    /// The rotation age is outside the supported range.
    #[error("storage.rotate_secs must be between {MIN_ROTATE_SECS} and {MAX_ROTATE_SECS}, got {0}")]
    InvalidRotateSecs(u64),
    /// The rotation size is outside the supported range.
    #[error(
        "storage.rotate_bytes must be between {MIN_ROTATE_BYTES} and {MAX_ROTATE_BYTES}, got {0}"
    )]
    InvalidRotateBytes(u64),
    /// The preallocation exceeds the rotation size or its own ceiling.
    #[error(
        "storage.preallocate_bytes must be between 0 and {max} (the smaller of \
         storage.rotate_bytes and {MAX_PREALLOCATE_BYTES}), got {value}"
    )]
    InvalidPreallocateBytes {
        /// The rejected preallocation.
        value: u64,
        /// The largest preallocation this configuration accepts.
        max: u64,
    },
    /// A size quota of zero would delete every recording.
    #[error("storage.quota_gib must be at least 1 when set, got {0}")]
    InvalidQuotaGib(u64),
    /// The fragment duration is zero or exceeds the supported operational bound.
    #[error("storage.max_fragment_ms must be between 1 and {MAX_FRAGMENT_MS}, got {0}")]
    InvalidMaxFragmentMs(u64),
    /// The per-stream writer queue exceeds the supported operational bound.
    #[error("storage.max_queued_fragments must not exceed {MAX_QUEUED_FRAGMENTS}, got {0}")]
    InvalidMaxQueuedFragments(usize),
    /// A camera retention age is zero or cannot be represented safely in microseconds.
    #[error(
        "camera {camera} retention_days must be between 1 and {MAX_RETENTION_DAYS}, got {days}"
    )]
    InvalidRetentionDays {
        /// The camera with the invalid age policy.
        camera: CameraId,
        /// The rejected number of days.
        days: u32,
    },
    /// The API connection cap is outside its supported range.
    #[error(
        "api.max_connections must be between {MIN_API_CONNECTIONS} and {MAX_API_CONNECTIONS}, got {0}"
    )]
    InvalidApiMaxConnections(usize),
    /// A media admission class is outside its supported range.
    #[error("api.{key} must be between 1 and {MAX_API_MEDIA_RESPONSES}, got {value}")]
    InvalidApiMediaLimit {
        /// The configuration key, `max_streaming_media` or `max_bounded_media`.
        key: &'static str,
        /// The rejected value.
        value: usize,
    },
    /// A camera URL is not an `rtsp://` URL with a host.
    #[error("camera {camera}: {field} must be an rtsp:// URL with a host")]
    InvalidCameraUrl {
        /// The camera.
        camera: CameraId,
        /// The field, `rtsp_url` or `sub_url`.
        field: &'static str,
    },
    /// A camera URL embeds credentials in its authority. The message names
    /// the field, never the URL.
    #[error("camera {camera}: {field} must not embed credentials; set username and password")]
    CredentialsInCameraUrl {
        /// The camera.
        camera: CameraId,
        /// The field, `rtsp_url` or `sub_url`.
        field: &'static str,
    },
    /// The socket read interval is above its ceiling.
    #[error("ingest.read_interval_ms must be at most {MAX_READ_INTERVAL_MS}, got {0}")]
    InvalidReadIntervalMs(u64),
    /// The receive low-water mark is above its ceiling.
    #[error("ingest.recv_low_water_kib must be at most {MAX_RECV_LOW_WATER_KIB}, got {0}")]
    InvalidRecvLowWaterKib(u32),
    /// The provisioned-camera limit is outside its supported range.
    #[error("provisioning.max_cameras must be between 1 and {MAX_PROVISIONED_CAMERAS}, got {0}")]
    InvalidProvisioningLimit(usize),
    /// Provisioning is enabled without a port a provisioned camera may use.
    #[error("provisioning.connect_ports must name a port when provisioning is enabled")]
    ProvisioningWithoutPorts,
    /// A provisioning port is zero.
    #[error("provisioning.connect_ports must not contain port 0")]
    InvalidProvisioningPort,
}

/// The parser's location for a [`ConfigError::Parse`] message.
fn describe_span(span: Option<&Range<usize>>) -> String {
    match span {
        Some(span) => format!("at bytes {}..{}", span.start, span.end),
        None => "at an unknown location".to_owned(),
    }
}

/// Largest accepted fragment duration, in milliseconds.
///
/// One minute is thirty times the ordinary two-second GOP cap and prevents an
/// invalid configuration from producing a pathological fragment allocation.
pub const MAX_FRAGMENT_MS: u64 = 60_000;

/// Longest accepted socket read interval, in milliseconds.
///
/// Every stream sleeps to the same tick, so a long interval delays live
/// viewers by that much and lets that much of every stream queue in the
/// kernel (about 2.3 MB per second at 18 Mbit/s); one second is the outer
/// bound (`docs/research/12-ingest-cost.md` §3).
pub const MAX_READ_INTERVAL_MS: u64 = 1_000;

/// Largest accepted receive low-water mark, in KiB.
///
/// A slow stream waits for this many bytes before its frames are read, so
/// the mark stays well under a second of the slowest stream; one MiB is
/// eight seconds at 1 Mbit/s and the outer bound.
pub const MAX_RECV_LOW_WATER_KIB: u32 = 1_024;

/// Largest accepted number of queued fragments per stream.
///
/// This is 128 times the default and keeps the synchronous writer channel
/// operationally bounded independently of the host pointer width.
pub const MAX_QUEUED_FRAGMENTS: usize = 4_096;

/// Largest retention age whose microsecond span fits in an [`i64`].
pub const MAX_RETENTION_DAYS: u32 = 106_751_991;

/// Largest accepted `provisioning.max_cameras`: the writer queue and the
/// descriptor budget are sized for the configured cameras plus this many.
pub const MAX_PROVISIONED_CAMERAS: usize = 64;

/// Largest accepted free-space floor, in percent of a root's capacity.
///
/// A floor above half the device would delete most recordings to satisfy it.
pub const MAX_MIN_FREE_PERCENT: u8 = 50;

/// Shortest accepted sync interval, in milliseconds.
///
/// Each sync is an `fdatasync` of every open file; below a tenth of a second
/// the device spends its time flushing.
pub const MIN_SYNC_INTERVAL_MS: u64 = 100;

/// Longest accepted sync interval, in milliseconds.
///
/// The interval bounds the power-loss window and the catalog's progress
/// reports; one minute is the outer bound.
pub const MAX_SYNC_INTERVAL_MS: u64 = 60_000;

/// Shortest accepted rotation age, in seconds.
pub const MIN_ROTATE_SECS: u64 = 60;

/// Longest accepted rotation age, in seconds (one day).
pub const MAX_ROTATE_SECS: u64 = 86_400;

/// Smallest accepted rotation size: the largest fragment the ingest assembler
/// accepts (64 MiB) must fit in one file.
pub const MIN_ROTATE_BYTES: u64 = 64 << 20;

/// Largest accepted rotation size (64 GiB).
pub const MAX_ROTATE_BYTES: u64 = 64 << 30;

/// Largest accepted preallocation (4 GiB). Reserved space counts as used for
/// the free-space floor, so it is also capped at the rotation size.
pub const MAX_PREALLOCATE_BYTES: u64 = 4 << 30;

/// Shortest accepted API bearer token, in bytes of visible ASCII.
///
/// Thirty-two characters carry at least 128 bits when generated as documented
/// (`openssl rand -hex 16`).
pub const MIN_API_TOKEN_LEN: usize = 32;

/// The placeholder token of `config/cameras.example.toml`, rejected so a copied
/// example cannot serve the API.
const EXAMPLE_API_TOKEN: &str = "replace-with-a-long-random-token";

/// Smallest accepted [`ApiConfig::max_connections`].
pub const MIN_API_CONNECTIONS: usize = 16;

/// Largest accepted [`ApiConfig::max_connections`].
pub const MAX_API_CONNECTIONS: usize = 65_536;

/// Default [`ApiConfig::max_connections`].
pub const DEFAULT_API_CONNECTIONS: usize = 256;

/// Largest accepted size of either media admission class
/// ([`ApiConfig::max_streaming_media`], [`ApiConfig::max_bounded_media`]).
pub const MAX_API_MEDIA_RESPONSES: usize = 1_024;

/// Default [`ApiConfig::max_streaming_media`].
pub const DEFAULT_API_STREAMING_MEDIA: usize = 8;

/// Default [`ApiConfig::max_bounded_media`].
pub const DEFAULT_API_BOUNDED_MEDIA: usize = 32;

/// How camera sockets are read.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields, default)]
pub struct IngestConfig {
    /// How often each camera socket is read, in milliseconds. Zero reads as soon
    /// as a packet arrives, one wake-up per packet. A positive value lets packets
    /// accumulate between reads so one system call covers many of them, and all
    /// streams are read in the same wake-up, at the cost of that much extra live
    /// latency. The default of 20 ms costs about a quarter less CPU than zero
    /// (docs/research/12-ingest-cost.md). Must be at most
    /// [`MAX_READ_INTERVAL_MS`].
    pub read_interval_ms: u64,
    /// Receive low-water mark per camera socket in KiB (`SO_RCVLOWAT`, set once
    /// a session plays): the kernel reports the socket readable only once this
    /// much is queued, so a steady stream wakes the daemon once per that many
    /// bytes instead of once per packet. Zero leaves the kernel default of one
    /// byte. A slow stream waits up to this many bytes before its frames are
    /// read (a quarter second at 1 Mbit/s with the default of 32), so keep it
    /// well under a second of the slowest stream's bitrate. Must be at most
    /// [`MAX_RECV_LOW_WATER_KIB`].
    pub recv_low_water_kib: u32,
}

impl Default for IngestConfig {
    fn default() -> Self {
        Self {
            read_interval_ms: 20,
            recv_low_water_kib: 32,
        }
    }
}

/// Runtime camera provisioning through the API (`[provisioning]`).
///
/// With `enabled`, `POST /api/cameras` adds a camera and
/// `DELETE /api/cameras/{camera}` removes one the API added; the daemon keeps
/// them in `cameras.toml` in its state directory, so they survive a restart.
/// Configured `[[camera]]` entries stay authoritative and are never removed
/// through the API. Added cameras may use only the TCP ports in
/// `connect_ports`, which the sandbox allows at startup.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields, default)]
pub struct ProvisioningConfig {
    /// Accept camera additions and removals through the API.
    pub enabled: bool,
    /// Most cameras the API may add beyond the configured ones
    /// (1 to [`MAX_PROVISIONED_CAMERAS`]).
    pub max_cameras: usize,
    /// TCP ports an added camera may use for RTSP and HTTP(S).
    pub connect_ports: Vec<u16>,
}

impl Default for ProvisioningConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            max_cameras: 16,
            connect_ports: vec![554, 80, 443],
        }
    }
}

impl ProvisioningConfig {
    fn validate(&self) -> Result<(), ConfigError> {
        if self.max_cameras == 0 || self.max_cameras > MAX_PROVISIONED_CAMERAS {
            return Err(ConfigError::InvalidProvisioningLimit(self.max_cameras));
        }
        if self.connect_ports.contains(&0) {
            return Err(ConfigError::InvalidProvisioningPort);
        }
        if self.enabled && self.connect_ports.is_empty() {
            return Err(ConfigError::ProvisioningWithoutPorts);
        }
        Ok(())
    }

    /// The allowed ports, sorted without repeats.
    #[must_use]
    pub fn sorted_connect_ports(&self) -> Vec<u16> {
        let mut ports = self.connect_ports.clone();
        ports.sort_unstable();
        ports.dedup();
        ports
    }

    /// The first port `camera` connects to that is not an allowed port, or
    /// `None` when every RTSP and HTTP port of the camera is allowed.
    #[must_use]
    pub fn refused_port(&self, camera: &CameraConfig) -> Option<u16> {
        camera
            .connect_ports()
            .into_iter()
            .find(|port| !self.connect_ports.contains(port))
    }
}

impl IngestConfig {
    /// The receive low-water mark in bytes, `None` for the kernel default.
    #[must_use]
    pub fn recv_low_water(&self) -> Option<u32> {
        (self.recv_low_water_kib > 0).then(|| self.recv_low_water_kib.saturating_mul(1024))
    }

    /// The read interval, `None` when sockets are read as packets arrive.
    #[must_use]
    pub fn read_interval(&self) -> Option<Duration> {
        (self.read_interval_ms > 0).then(|| Duration::from_millis(self.read_interval_ms))
    }
}

/// Top-level configuration: storage, API, ingest policy, and cameras.
///
/// Unknown keys are errors rather than silent defaults, camera ids must be
/// unique, recorded stream kinds must be distinct supported video kinds, event sources
/// must be unique per camera, and the storage knobs must lie in the ranges their
/// fields document. The API needs at least one bearer token of
/// [`MIN_API_TOKEN_LEN`] visible ASCII characters, or `api.allow_unauthenticated`
/// with no token on a loopback listener. Every section other than `camera` has a
/// default, but the default API section has no token, so a configuration states
/// its API authentication explicitly. `config/cameras.example.toml` documents each key.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Storage policy; every field has a default.
    #[serde(default)]
    pub storage: StorageConfig,
    /// HTTP API settings; every field has a default.
    #[serde(default)]
    pub api: ApiConfig,
    /// Ingest policy; every field has a default.
    #[serde(default)]
    pub ingest: IngestConfig,
    /// Runtime camera provisioning through the API; off by default.
    #[serde(default)]
    pub provisioning: ProvisioningConfig,
    /// The cameras to record.
    #[serde(default, rename = "camera")]
    pub cameras: Vec<CameraConfig>,
}

impl Config {
    /// Parses configuration from TOML text.
    ///
    /// Parse failures expose only a generic description and byte span, because
    /// parser messages and source excerpts can contain credentials.
    ///
    /// # Errors
    ///
    /// [`ConfigError::Parse`] for text that is not the schema, then the
    /// validation errors: every other [`ConfigError`] variant names the rule
    /// it checks.
    pub fn from_toml_str(text: &str) -> Result<Self, ConfigError> {
        let config: Self = toml::from_str(text)
            .map_err(|error: toml::de::Error| ConfigError::Parse { span: error.span() })?;
        config.validate()?;
        Ok(config)
    }

    /// Loads configuration from a file.
    ///
    /// # Errors
    ///
    /// [`ConfigError::Io`] when the file cannot be read, then the errors of
    /// [`Config::from_toml_str`].
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let text = std::fs::read_to_string(path).map_err(|source| ConfigError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        Self::from_toml_str(&text)
    }

    fn validate(&self) -> Result<(), ConfigError> {
        if self.storage.min_free_percent > MAX_MIN_FREE_PERCENT {
            return Err(ConfigError::InvalidMinFreePercent(
                self.storage.min_free_percent,
            ));
        }
        if self.api.tokens.iter().any(|token| token.trim().is_empty()) {
            return Err(ConfigError::BlankApiToken);
        }
        self.api.validate_authentication()?;
        self.storage.validate_ranges()?;
        if !(MIN_API_CONNECTIONS..=MAX_API_CONNECTIONS).contains(&self.api.max_connections) {
            return Err(ConfigError::InvalidApiMaxConnections(
                self.api.max_connections,
            ));
        }
        for (key, value) in [
            ("max_streaming_media", self.api.max_streaming_media),
            ("max_bounded_media", self.api.max_bounded_media),
        ] {
            if !(1..=MAX_API_MEDIA_RESPONSES).contains(&value) {
                return Err(ConfigError::InvalidApiMediaLimit { key, value });
            }
        }
        if self.storage.max_fragment_ms == 0 || self.storage.max_fragment_ms > MAX_FRAGMENT_MS {
            return Err(ConfigError::InvalidMaxFragmentMs(
                self.storage.max_fragment_ms,
            ));
        }
        if self.storage.max_queued_fragments > MAX_QUEUED_FRAGMENTS {
            return Err(ConfigError::InvalidMaxQueuedFragments(
                self.storage.max_queued_fragments,
            ));
        }
        if self.ingest.read_interval_ms > MAX_READ_INTERVAL_MS {
            return Err(ConfigError::InvalidReadIntervalMs(
                self.ingest.read_interval_ms,
            ));
        }
        if self.ingest.recv_low_water_kib > MAX_RECV_LOW_WATER_KIB {
            return Err(ConfigError::InvalidRecvLowWaterKib(
                self.ingest.recv_low_water_kib,
            ));
        }
        let mut seen = HashSet::new();
        for camera in &self.cameras {
            if !seen.insert(&camera.id) {
                return Err(ConfigError::DuplicateCamera(camera.id.clone()));
            }
            camera.validate()?;
        }
        self.provisioning.validate()
    }
}

/// Storage policy. Field names carry their units (`_bytes`, `_gib`, `_ms`, `_secs`,
/// `_percent`).
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields, default)]
pub struct StorageConfig {
    /// Directories that receive recordings: the first eligible root is used, with later roots
    /// as ordered failover targets (`docs/research/04-storage-format.md` §8). A root path must
    /// hold no symbolic link in any component: configure the resolved physical path.
    pub video_roots: Vec<PathBuf>,
    /// Directory for the catalog database and other small state.
    pub state_dir: PathBuf,
    /// Retention deletes the oldest sealed segments while a root has less free space than this.
    /// Must be in `0..=`[`MAX_MIN_FREE_PERCENT`]; zero disables the free-space floor.
    pub min_free_percent: u8,
    /// Retention also deletes the oldest sealed segments while the catalogued recordings (open
    /// files at their written length) exceed this many GiB. Unset means no cap; when set it
    /// must be at least 1.
    pub quota_gib: Option<u64>,
    /// How often open files are `fdatasync`ed. Bounds the power-loss window. Must be in
    /// [`MIN_SYNC_INTERVAL_MS`]`..=`[`MAX_SYNC_INTERVAL_MS`].
    pub sync_interval_ms: u64,
    /// Fragment duration cap: a GOP is split once it spans this plus a quarter, so timestamp
    /// jitter never splits a GOP of exactly this length, and a fragment holds at most 1.25
    /// times this of frames; frames the camera skipped lengthen its last frame by the gap.
    /// Must be in `1..=`[`MAX_FRAGMENT_MS`].
    pub max_fragment_ms: u64,
    /// Space reserved ahead of the write position with `fallocate`. Must not exceed the
    /// smaller of `rotate_bytes` and [`MAX_PREALLOCATE_BYTES`]; zero disables it.
    pub preallocate_bytes: u64,
    /// Size at which a file is sealed and the next one started (at the next sync fragment),
    /// whichever of size and `rotate_secs` comes first. Must be in
    /// [`MIN_ROTATE_BYTES`]`..=`[`MAX_ROTATE_BYTES`].
    pub rotate_bytes: u64,
    /// Age in seconds at which a file is sealed and the next one started (at the next sync
    /// fragment), whichever of age and `rotate_bytes` comes first. Must be in
    /// [`MIN_ROTATE_SECS`]`..=`[`MAX_ROTATE_SECS`].
    pub rotate_secs: u64,
    /// Fragments queued for the writer per stream before new ones are dropped.
    /// Must not exceed [`MAX_QUEUED_FRAGMENTS`]; zero uses a capacity of one.
    pub max_queued_fragments: usize,
}

impl StorageConfig {
    /// The quota in bytes, if one is set.
    #[must_use]
    pub fn quota_bytes(&self) -> Option<u64> {
        self.quota_gib.map(|gib| gib.saturating_mul(1 << 30))
    }

    /// Checks the knobs whose out-of-range values silently break durability,
    /// stop recording, or delete every recording.
    fn validate_ranges(&self) -> Result<(), ConfigError> {
        if !(MIN_SYNC_INTERVAL_MS..=MAX_SYNC_INTERVAL_MS).contains(&self.sync_interval_ms) {
            return Err(ConfigError::InvalidSyncIntervalMs(self.sync_interval_ms));
        }
        if !(MIN_ROTATE_SECS..=MAX_ROTATE_SECS).contains(&self.rotate_secs) {
            return Err(ConfigError::InvalidRotateSecs(self.rotate_secs));
        }
        if !(MIN_ROTATE_BYTES..=MAX_ROTATE_BYTES).contains(&self.rotate_bytes) {
            return Err(ConfigError::InvalidRotateBytes(self.rotate_bytes));
        }
        let max = self.rotate_bytes.min(MAX_PREALLOCATE_BYTES);
        if self.preallocate_bytes > max {
            return Err(ConfigError::InvalidPreallocateBytes {
                value: self.preallocate_bytes,
                max,
            });
        }
        if self.quota_gib == Some(0) {
            return Err(ConfigError::InvalidQuotaGib(0));
        }
        Ok(())
    }
}

impl Default for StorageConfig {
    fn default() -> Self {
        Self {
            video_roots: vec![PathBuf::from("data/recordings")],
            state_dir: PathBuf::from("data/state"),
            min_free_percent: 5,
            quota_gib: None,
            sync_interval_ms: 5_000,
            max_fragment_ms: 2_000,
            preallocate_bytes: 256 << 20,
            rotate_bytes: 1 << 30,
            rotate_secs: 3_600,
            max_queued_fragments: 32,
        }
    }
}

/// HTTP API settings. `Debug` prints the number of tokens, never their values.
#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields, default)]
pub struct ApiConfig {
    /// Address to listen on.
    pub listen: SocketAddr,
    /// Bearer tokens accepted by the API. Each entry is at least [`MIN_API_TOKEN_LEN`]
    /// characters of visible ASCII (0x21 to 0x7e) and is not the example placeholder;
    /// generate one with `openssl rand -hex 16`. At least one token is required unless
    /// `allow_unauthenticated` is set.
    pub tokens: Vec<String>,
    /// Serves the API without authentication, for development. Accepted only with an
    /// empty token list and a loopback `listen` address. Defaults to false.
    pub allow_unauthenticated: bool,
    /// TCP connections served at once, WebSocket connections included; further clients wait
    /// in the kernel's listen backlog. Must be between [`MIN_API_CONNECTIONS`] and
    /// [`MAX_API_CONNECTIONS`] (16 to 65,536).
    pub max_connections: usize,
    /// Streaming media responses served at once: whole or open-ended recording files, ranges
    /// above 1 MiB, and exports. Must be between 1 and [`MAX_API_MEDIA_RESPONSES`].
    pub max_streaming_media: usize,
    /// Bounded media responses served at once: VOD playlists, keyframes, LL-HLS
    /// initialization, segment, and part resources, and ranges of at most 1 MiB. Must be
    /// between 1 and [`MAX_API_MEDIA_RESPONSES`].
    pub max_bounded_media: usize,
    /// Serve `/metrics` without authentication. When false, the default, a scrape presents a
    /// root bearer token like every other protected route.
    pub public_metrics: bool,
}

impl fmt::Debug for ApiConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ApiConfig")
            .field("listen", &self.listen)
            .field(
                "tokens",
                &format_args!("[<redacted>; {}]", self.tokens.len()),
            )
            .field("allow_unauthenticated", &self.allow_unauthenticated)
            .field("max_connections", &self.max_connections)
            .field("max_streaming_media", &self.max_streaming_media)
            .field("max_bounded_media", &self.max_bounded_media)
            .field("public_metrics", &self.public_metrics)
            .finish()
    }
}

impl Default for ApiConfig {
    fn default() -> Self {
        Self {
            listen: "127.0.0.1:8554".parse().expect("valid literal"),
            tokens: Vec::new(),
            allow_unauthenticated: false,
            max_connections: DEFAULT_API_CONNECTIONS,
            max_streaming_media: DEFAULT_API_STREAMING_MEDIA,
            max_bounded_media: DEFAULT_API_BOUNDED_MEDIA,
            public_metrics: false,
        }
    }
}

impl ApiConfig {
    /// Requires either well-formed bearer tokens or an explicit unauthenticated
    /// loopback listener. Errors name a token's position, never its value.
    fn validate_authentication(&self) -> Result<(), ConfigError> {
        if self.tokens.is_empty() {
            if !self.allow_unauthenticated {
                return Err(ConfigError::MissingApiToken);
            }
            if !self.listen.ip().is_loopback() {
                return Err(ConfigError::UnauthenticatedNonLoopback(self.listen));
            }
            return Ok(());
        }
        if self.allow_unauthenticated {
            return Err(ConfigError::ConflictingApiAuthentication);
        }
        for (index, token) in self.tokens.iter().enumerate() {
            if token.len() < MIN_API_TOKEN_LEN
                || !token.bytes().all(|byte| (0x21..=0x7e).contains(&byte))
            {
                return Err(ConfigError::WeakApiToken { index });
            }
            if token == EXAMPLE_API_TOKEN {
                return Err(ConfigError::PlaceholderApiToken { index });
            }
        }
        Ok(())
    }
}

/// How frame durations are derived from camera timestamps.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TimestampMode {
    /// Assign constant-frame-rate durations locked to the camera's long-term
    /// rate (the default; see the storage design, time model).
    #[default]
    Regularize,
    /// Use raw RTP timestamp deltas as durations.
    Passthrough,
}

/// Where a camera's snapshot comes from (`snapshot` in a `[[camera]]` table).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SnapshotMethod {
    /// ONVIF `GetSnapshotUri` for a camera whose event sources are ONVIF
    /// pull-point alone, else the Dahua CGI snapshot endpoint.
    #[default]
    Auto,
    /// `/cgi-bin/snapshot.cgi?channel=1` with digest authentication.
    DahuaCgi,
    /// The URL the camera's ONVIF Media service names for its first profile.
    Onvif,
    /// No snapshot route for this camera.
    None,
}

/// The snapshot source a camera's configuration resolves to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResolvedSnapshot {
    /// The Dahua CGI snapshot endpoint.
    DahuaCgi,
    /// The camera's ONVIF Media service.
    Onvif,
}

/// One camera. Its `Debug` output shows the password as `<redacted>`.
#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CameraConfig {
    /// Stable identifier used in paths and URLs.
    pub id: CameraId,
    /// Human-readable name.
    pub name: String,
    /// RTSP URL of the main stream, without credentials: a URL that embeds
    /// them is rejected.
    pub rtsp_url: String,
    /// RTSP URL of the sub stream. Derived from `rtsp_url` for Dahua-style
    /// URLs (`subtype=0` → `subtype=1`) when absent.
    #[serde(default)]
    pub sub_url: Option<String>,
    /// RTSP/HTTP username.
    pub username: String,
    /// RTSP/HTTP password.
    pub password: String,
    /// Optional vendor label recorded in the catalog and returned by the API.
    #[serde(default)]
    pub vendor: Option<String>,
    /// Which streams to record, with no repeated kinds.
    #[serde(default = "default_record")]
    pub record: Vec<StreamKind>,
    /// Timestamp policy for this camera.
    #[serde(default)]
    pub timestamp_mode: TimestampMode,
    /// Port for vendor CGI and ONVIF; defaults to 80, or 443 when `https` is set.
    #[serde(default)]
    pub http_port: Option<u16>,
    /// Use HTTPS for vendor CGI and ONVIF (cameras that redirect HTTP to HTTPS).
    #[serde(default)]
    pub https: bool,
    /// Verify the camera's TLS certificate. Cameras ship self-signed
    /// certificates, so this is commonly set to false together with `https`.
    #[serde(default = "default_true")]
    pub tls_verify: bool,
    /// Delete this camera's recordings after this many days, regardless of free space.
    /// Must be in `1..=`[`MAX_RETENTION_DAYS`]; leave it unset for no age limit.
    #[serde(default)]
    pub retention_days: Option<u32>,
    /// Distinct event sources to subscribe to.
    #[serde(default)]
    pub events: Vec<EventSource>,
    /// Where snapshots come from; `auto` follows the event sources.
    #[serde(default)]
    pub snapshot: SnapshotMethod,
}

impl fmt::Debug for CameraConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Destructured so a field added later cannot bypass this decision.
        let Self {
            id,
            name,
            rtsp_url,
            sub_url,
            username,
            password: _,
            vendor,
            record,
            timestamp_mode,
            http_port,
            https,
            tls_verify,
            retention_days,
            events,
            snapshot,
        } = self;
        f.debug_struct("CameraConfig")
            .field("id", id)
            .field("name", name)
            .field("rtsp_url", rtsp_url)
            .field("sub_url", sub_url)
            .field("username", username)
            .field("password", &"<redacted>")
            .field("vendor", vendor)
            .field("record", record)
            .field("timestamp_mode", timestamp_mode)
            .field("http_port", http_port)
            .field("https", https)
            .field("tls_verify", tls_verify)
            .field("retention_days", retention_days)
            .field("events", events)
            .field("snapshot", snapshot)
            .finish()
    }
}

fn default_true() -> bool {
    true
}

fn default_record() -> Vec<StreamKind> {
    vec![StreamKind::Main, StreamKind::Sub]
}

impl CameraConfig {
    /// The snapshot source: the configured method, with `auto` choosing the
    /// ONVIF Media service for a camera whose event sources are ONVIF
    /// pull-point alone and the Dahua CGI endpoint otherwise; `None` for a
    /// camera without snapshots.
    #[must_use]
    pub fn snapshot_method(&self) -> Option<ResolvedSnapshot> {
        match self.snapshot {
            SnapshotMethod::None => None,
            SnapshotMethod::DahuaCgi => Some(ResolvedSnapshot::DahuaCgi),
            SnapshotMethod::Onvif => Some(ResolvedSnapshot::Onvif),
            SnapshotMethod::Auto => {
                let onvif_only = self.events.contains(&EventSource::OnvifPullPoint)
                    && !self.events.contains(&EventSource::DahuaCgi);
                Some(if onvif_only {
                    ResolvedSnapshot::Onvif
                } else {
                    ResolvedSnapshot::DahuaCgi
                })
            }
        }
    }

    /// Checks one camera on its own: `rtsp://` URLs with a host and without
    /// embedded credentials, a retention age in range, recorded kinds and
    /// event sources that are supported and not repeated. Configuration
    /// loading applies it to every camera, and the API applies it to a camera
    /// it is asked to add.
    ///
    /// # Errors
    ///
    /// [`ConfigError::InvalidCameraUrl`], [`ConfigError::CredentialsInCameraUrl`],
    /// [`ConfigError::InvalidRetentionDays`],
    /// [`ConfigError::UnsupportedRecordedKind`],
    /// [`ConfigError::DuplicateRecordedKind`],
    /// [`ConfigError::UnsupportedEventSource`], or
    /// [`ConfigError::DuplicateEventSource`], for the first rule that fails.
    pub fn validate(&self) -> Result<(), ConfigError> {
        for (field, url) in [
            ("rtsp_url", Some(&self.rtsp_url)),
            ("sub_url", self.sub_url.as_ref()),
        ] {
            let Some(url) = url else {
                continue;
            };
            if url_has_userinfo(url) {
                return Err(ConfigError::CredentialsInCameraUrl {
                    camera: self.id.clone(),
                    field,
                });
            }
            if !url.starts_with("rtsp://")
                || !url_host(url).is_some_and(|host| is_valid_host(&host))
            {
                return Err(ConfigError::InvalidCameraUrl {
                    camera: self.id.clone(),
                    field,
                });
            }
        }
        if let Some(days) = self.retention_days
            && (days == 0 || days > MAX_RETENTION_DAYS)
        {
            return Err(ConfigError::InvalidRetentionDays {
                camera: self.id.clone(),
                days,
            });
        }
        let mut kinds = HashSet::new();
        for &kind in &self.record {
            if kind == StreamKind::Audio {
                return Err(ConfigError::UnsupportedRecordedKind {
                    camera: self.id.clone(),
                    kind,
                });
            }
            if !kinds.insert(kind) {
                return Err(ConfigError::DuplicateRecordedKind {
                    camera: self.id.clone(),
                    kind,
                });
            }
        }
        let mut event_sources = HashSet::new();
        for &event_source in &self.events {
            if matches!(
                event_source,
                EventSource::OnvifMetadata | EventSource::Manual
            ) {
                return Err(ConfigError::UnsupportedEventSource {
                    camera: self.id.clone(),
                    event_source,
                });
            }
            if !event_sources.insert(event_source) {
                return Err(ConfigError::DuplicateEventSource {
                    camera: self.id.clone(),
                    event_source,
                });
            }
        }
        Ok(())
    }

    /// Returns the sub-stream URL, deriving it from the main URL when the
    /// configuration omits it and the URL follows the Dahua `subtype=` scheme.
    #[must_use]
    pub fn sub_url(&self) -> Option<String> {
        if let Some(url) = &self.sub_url {
            return Some(url.clone());
        }
        self.rtsp_url
            .contains("subtype=0")
            .then(|| self.rtsp_url.replacen("subtype=0", "subtype=1", 1))
    }

    /// Returns the CGI/ONVIF port: the configured one, else 443 with `https`, else 80.
    #[must_use]
    pub fn http_port_or_default(&self) -> u16 {
        self.http_port.unwrap_or(if self.https { 443 } else { 80 })
    }

    /// Returns the base URL for vendor HTTP and ONVIF calls, such as `https://host:443`.
    /// IPv6 hosts are enclosed in brackets for the URL authority.
    #[must_use]
    pub fn http_base(&self) -> Option<String> {
        let scheme = if self.https { "https" } else { "http" };
        let host = self.host()?;
        let host = if host.contains(':') {
            format!("[{host}]")
        } else {
            host
        };
        Some(format!("{scheme}://{host}:{}", self.http_port_or_default()))
    }

    /// The TCP ports of the RTSP URLs (`rtsp_url` and `sub_url`), 554 when a
    /// URL names none. Unparsable URLs are skipped.
    #[must_use]
    pub fn rtsp_ports(&self) -> Vec<u16> {
        [Some(&self.rtsp_url), self.sub_url.as_ref()]
            .into_iter()
            .flatten()
            .filter_map(|url| rtsp_port(url))
            .collect()
    }

    /// The TCP ports the daemon connects to for this camera: its RTSP ports
    /// ([`Self::rtsp_ports`]) followed by its HTTP port
    /// ([`Self::http_port_or_default`]).
    #[must_use]
    pub fn connect_ports(&self) -> Vec<u16> {
        let mut ports = self.rtsp_ports();
        ports.push(self.http_port_or_default());
        ports
    }

    /// Returns the host part of `rtsp_url`, used for vendor HTTP and ONVIF calls.
    /// IPv6 hosts have no brackets; URL builders must add them to the authority.
    #[must_use]
    pub fn host(&self) -> Option<String> {
        url_host(&self.rtsp_url)
    }

    /// Returns the URL for a stream kind, if that kind is a video stream with
    /// a known URL.
    #[must_use]
    pub fn url_for(&self, kind: StreamKind) -> Option<String> {
        match kind {
            StreamKind::Main => Some(self.rtsp_url.clone()),
            StreamKind::Sub => self.sub_url(),
            StreamKind::Audio => None,
        }
    }
}

/// Longest host name accepted in a camera address (RFC 1035).
pub const MAX_HOST_BYTES: usize = 253;

/// Whether `host` is a bare host name or IP address: what `http://{host}/`
/// parses back to, with no port, path, query, or userinfo smuggled in, and at
/// most [`MAX_HOST_BYTES`] long. An IPv6 address is given without brackets.
/// The daemon builds camera URLs from such a host, so a host that fails this
/// check is refused when it is configured or added rather than when a call is
/// made.
///
/// ```
/// use iris_core::config::is_valid_host;
///
/// assert!(is_valid_host("192.0.2.10"));
/// assert!(is_valid_host("camera.example"));
/// assert!(is_valid_host("2001:db8::7"));
/// assert!(!is_valid_host("cam host"));
/// assert!(!is_valid_host("192.0.2.10:80"));
/// assert!(!is_valid_host("[2001:db8::7]"));
/// ```
#[must_use]
pub fn is_valid_host(host: &str) -> bool {
    if host.is_empty() || host.len() > MAX_HOST_BYTES {
        return false;
    }
    let authority = if host.contains(':') {
        format!("[{host}]")
    } else {
        host.to_owned()
    };
    let Ok(url) = url::Url::parse(&format!("http://{authority}/")) else {
        return false;
    };
    url.host_str()
        .is_some_and(|parsed| parsed.eq_ignore_ascii_case(&authority))
        && url.port().is_none()
        && url.path() == "/"
        && url.query().is_none()
        && url.username().is_empty()
}

/// The authority of a URL (`host`, `host:port`, `[v6]:port`, with any
/// userinfo before an `@`); `None` without a scheme separator.
fn url_authority(url: &str) -> Option<&str> {
    let rest = url.split("://").nth(1)?;
    rest.split(['/', '?', '#']).next()
}

/// Whether the URL's authority carries userinfo (`user:password@host`).
fn url_has_userinfo(url: &str) -> bool {
    url_authority(url).is_some_and(|authority| authority.contains('@'))
}

/// The host of a URL's authority, without brackets, port, or userinfo; `None`
/// without a scheme separator or with an empty host.
fn url_host(url: &str) -> Option<String> {
    let host_port = url_authority(url)?.rsplit('@').next()?;
    let host = if let Some(stripped) = host_port.strip_prefix('[') {
        stripped.split(']').next()?
    } else {
        host_port.split(':').next()?
    };
    (!host.is_empty()).then(|| host.to_owned())
}

/// The port of a URL with a host: the explicit one, or 554, the RTSP default,
/// without one (the scheme is not checked). `None` without a scheme separator
/// or a host, or when the port text is not a `u16`.
fn rtsp_port(url: &str) -> Option<u16> {
    url_host(url)?;
    let host_port = url_authority(url)?.rsplit('@').next()?;
    let port = if host_port.starts_with('[') {
        host_port
            .split(']')
            .nth(1)
            .and_then(|p| p.strip_prefix(':'))
    } else {
        host_port.split_once(':').map(|(_, p)| p)
    };
    match port {
        Some(p) => p.parse().ok(),
        None => Some(554),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An unauthenticated loopback API section, so a fixture exercises only the
    /// rule under test.
    const DEV_API: &str = "[api]\nallow_unauthenticated = true\n";

    fn parse(text: &str) -> Result<Config, ConfigError> {
        Config::from_toml_str(&format!("{DEV_API}{text}"))
    }

    #[test]
    fn the_quota_converts_from_gibibytes_and_is_absent_when_unset() {
        assert_eq!(parse("").unwrap().storage.quota_bytes(), None);
        let quota = |gib: u64| {
            parse(&format!("[storage]\nquota_gib={gib}\n"))
                .unwrap()
                .storage
                .quota_bytes()
        };
        assert_eq!(quota(1), Some(1_073_741_824));
        assert_eq!(quota(500), Some(500 * 1_073_741_824));
    }

    #[test]
    fn a_camera_records_both_streams_by_default_and_names_their_urls() {
        let camera = |extra: &str| {
            parse(&format!(
                "[[camera]]\nid=\"cam1\"\nname=\"n\"\nusername=\"u\"\npassword=\"p\"\n{extra}"
            ))
            .unwrap()
            .cameras
            .remove(0)
        };
        let derived = camera("rtsp_url=\"rtsp://192.0.2.7/s?channel=1&subtype=0\"\n");
        assert_eq!(derived.record, [StreamKind::Main, StreamKind::Sub]);
        assert_eq!(
            derived.url_for(StreamKind::Main).as_deref(),
            Some("rtsp://192.0.2.7/s?channel=1&subtype=0")
        );
        // Without a configured secondary URL, the main one with its subtype
        // switched.
        assert_eq!(
            derived.url_for(StreamKind::Sub).as_deref(),
            Some("rtsp://192.0.2.7/s?channel=1&subtype=1")
        );
        assert_eq!(derived.url_for(StreamKind::Audio), None);
        let explicit =
            camera("rtsp_url=\"rtsp://192.0.2.7/main\"\nsub_url=\"rtsp://192.0.2.7/low\"\n");
        assert_eq!(
            explicit.url_for(StreamKind::Sub).as_deref(),
            Some("rtsp://192.0.2.7/low")
        );
        assert_eq!(
            camera("rtsp_url=\"rtsp://192.0.2.7/main\"\nrecord=['main']\n")
                .url_for(StreamKind::Sub),
            None
        );
    }

    #[test]
    fn a_host_name_may_be_253_bytes_and_no_longer() {
        // Labels of 63 bytes joined by dots: 63 * 3 + 61 + 3 dots = 253.
        let label = "a".repeat(63);
        let longest = format!("{label}.{label}.{label}.{}", "b".repeat(61));
        assert_eq!(longest.len(), MAX_HOST_BYTES);
        assert!(is_valid_host(&longest));
        let longer = format!("{label}.{label}.{label}.{}", "b".repeat(62));
        assert!(!is_valid_host(&longer));
    }

    #[test]
    fn loading_reads_the_file_and_reports_a_missing_one() {
        let dir = std::env::temp_dir().join(format!("iris-config-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("irisd.toml");
        std::fs::write(&path, format!("{DEV_API}[storage]\nquota_gib=7\n")).unwrap();
        let loaded = Config::load(&path).unwrap();
        assert_eq!(loaded.storage.quota_gib, Some(7));
        std::fs::remove_file(&path).unwrap();
        assert!(matches!(
            Config::load(&path),
            Err(ConfigError::Io { path: missing, .. }) if missing == path
        ));
        std::fs::remove_dir(&dir).unwrap();
    }

    #[test]
    fn the_snapshot_method_follows_the_event_sources_unless_configured() {
        let camera = |extra: &str| {
            Config::from_toml_str(&format!(
                "[api]\nallow_unauthenticated=true\n[[camera]]\nid=\"cam1\"\nname=\"n\"\n\
                 rtsp_url=\"rtsp://192.0.2.7/x\"\nusername=\"u\"\npassword=\"p\"\n{extra}"
            ))
            .unwrap()
            .cameras
            .remove(0)
        };
        assert_eq!(camera("").snapshot, SnapshotMethod::Auto);
        assert_eq!(
            camera("").snapshot_method(),
            Some(ResolvedSnapshot::DahuaCgi)
        );
        assert_eq!(
            camera("events=['onvif_pull_point']\n").snapshot_method(),
            Some(ResolvedSnapshot::Onvif)
        );
        assert_eq!(
            camera("events=['dahua_cgi','onvif_pull_point']\n").snapshot_method(),
            Some(ResolvedSnapshot::DahuaCgi)
        );
        assert_eq!(
            camera("snapshot='onvif'\n").snapshot_method(),
            Some(ResolvedSnapshot::Onvif)
        );
        assert_eq!(
            camera("snapshot='dahua_cgi'\nevents=['onvif_pull_point']\n").snapshot_method(),
            Some(ResolvedSnapshot::DahuaCgi)
        );
        assert_eq!(camera("snapshot='none'\n").snapshot_method(), None);
        assert!(
            Config::from_toml_str(
                "[api]\nallow_unauthenticated=true\n[[camera]]\nid=\"cam1\"\nname=\"n\"\n\
                 rtsp_url=\"rtsp://192.0.2.7/x\"\nusername=\"u\"\npassword=\"p\"\nsnapshot='cgi'\n"
            )
            .is_err(),
            "an unknown method is rejected"
        );
    }

    #[test]
    fn provisioning_is_off_by_default_with_the_standard_ports() {
        let config = Config::from_toml_str("[api]\nallow_unauthenticated=true\n").unwrap();
        assert!(!config.provisioning.enabled);
        assert_eq!(config.provisioning.max_cameras, 16);
        assert_eq!(config.provisioning.sorted_connect_ports(), [80, 443, 554]);
    }

    #[test]
    fn provisioning_limits_and_ports_are_checked() {
        for (section, expected) in [
            (
                "[provisioning]\nmax_cameras = 0\n",
                "provisioning.max_cameras",
            ),
            (
                "[provisioning]\nmax_cameras = 65\n",
                "provisioning.max_cameras",
            ),
            ("[provisioning]\nconnect_ports = [554, 0]\n", "port 0"),
            (
                "[provisioning]\nenabled = true\nconnect_ports = []\n",
                "must name a port",
            ),
        ] {
            let text = format!("[api]\nallow_unauthenticated=true\n{section}");
            let error = Config::from_toml_str(&text).unwrap_err().to_string();
            assert!(error.contains(expected), "{section}: {error}");
        }
        let text = "[api]\nallow_unauthenticated=true\n[provisioning]\nenabled = true\n\
                    max_cameras = 64\nconnect_ports = [8554, 554, 554]\n";
        let config = Config::from_toml_str(text).unwrap();
        assert_eq!(config.provisioning.sorted_connect_ports(), [554, 8554]);
    }

    #[test]
    fn camera_urls_must_be_rtsp_with_a_host_and_ports_match_the_provisioning_set() {
        let camera = |url: &str, sub: Option<&str>| {
            format!(
                "[api]\nallow_unauthenticated=true\n[[camera]]\nid=\"cam1\"\nname=\"n\"\n\
                 rtsp_url=\"{url}\"\n{}username=\"u\"\npassword=\"p\"\n",
                sub.map(|sub| format!("sub_url=\"{sub}\"\n"))
                    .unwrap_or_default()
            )
        };
        assert!(Config::from_toml_str(&camera("rtsp://192.0.2.10:554/a", None)).is_ok());
        for (url, sub) in [
            ("http://192.0.2.10/a", None),
            ("rtsp:///a", None),
            ("rtsp://192.0.2.10/a", Some("rtsp://")),
            ("rtsp://cam host/a", None),
            ("rtsp://[fe80::1%25eth0]/a", None),
        ] {
            let error = Config::from_toml_str(&camera(url, sub))
                .unwrap_err()
                .to_string();
            assert!(
                error.contains("rtsp:// URL with a host"),
                "{url} {sub:?}: {error}"
            );
        }
        let provisioning = ProvisioningConfig::default();
        let other_port = Config::from_toml_str(&camera("rtsp://192.0.2.10:8554/a", None)).unwrap();
        assert_eq!(
            provisioning.refused_port(&other_port.cameras[0]),
            Some(8554)
        );
        assert_eq!(other_port.cameras[0].connect_ports(), vec![8554, 80]);
        let standard = Config::from_toml_str(&camera("rtsp://192.0.2.10/a", None)).unwrap();
        assert_eq!(provisioning.refused_port(&standard.cameras[0]), None);
        assert!(standard.cameras[0].validate().is_ok());
    }

    #[test]
    fn camera_urls_must_not_embed_credentials() {
        for (url, sub, field) in [
            (
                "rtsp://distinctive-user:distinctive-password@192.0.2.10/a",
                None,
                "rtsp_url",
            ),
            (
                "rtsp://192.0.2.10/a",
                Some("rtsp://distinctive-user@192.0.2.10/b"),
                "sub_url",
            ),
        ] {
            let text = format!(
                "[api]\nallow_unauthenticated=true\n[[camera]]\nid=\"cam1\"\nname=\"n\"\n\
                 rtsp_url=\"{url}\"\n{}username=\"u\"\npassword=\"p\"\n",
                sub.map(|sub| format!("sub_url=\"{sub}\"\n"))
                    .unwrap_or_default()
            );
            let error = Config::from_toml_str(&text).unwrap_err();
            assert!(
                matches!(&error, ConfigError::CredentialsInCameraUrl { field: rejected, .. } if *rejected == field),
                "{error:?}"
            );
            let rendered = format!("{error}\n{error:?}");
            assert!(!rendered.contains("distinctive"), "{rendered}");
        }
    }

    #[test]
    fn parse_errors_do_not_expose_configuration_values() {
        use std::error::Error as _;

        for text in [
            "[api]\ntokens=['redaction-marker',123]\n",
            "[api]\ntokens='redaction-marker'\n",
            "[[camera]]\npassword=\"redaction-marker\\q\"\n",
            "[api]\ntokens=['redaction-marker']\ntokens=['redaction-marker']\n",
        ] {
            let error = Config::from_toml_str(text).unwrap_err();
            assert!(!format!("{error}\n{error:?}").contains("redaction-marker"));
            let mut source = error.source();
            while let Some(error) = source {
                assert!(!format!("{error}\n{error:?}").contains("redaction-marker"));
                source = error.source();
            }
        }
    }

    #[test]
    fn parse_errors_retain_a_safe_byte_location() {
        let text = "[storage]\nmin_free_percent='invalid'\n";
        let error = Config::from_toml_str(text).unwrap_err();
        let ConfigError::Parse { span: Some(span) } = &error else {
            panic!("expected a safe parser location: {error:?}");
        };
        assert!(span.start >= "[storage]\n".len());
        assert!(span.end <= text.len());
        assert!(error.to_string().contains("at bytes "), "{error}");
    }

    #[test]
    fn blank_api_tokens_fail_closed() {
        for tokens in ["['']", "['   ']", "['valid', '']", "[\"valid\", \"\\t\"]"] {
            assert!(matches!(
                Config::from_toml_str(&format!("[api]\ntokens={tokens}\n")),
                Err(ConfigError::BlankApiToken)
            ));
        }
    }

    /// A token of exactly the minimum length, built so no test embeds a usable secret.
    fn token(fill: char) -> String {
        std::iter::repeat_n(fill, MIN_API_TOKEN_LEN).collect()
    }

    #[test]
    fn an_api_without_tokens_requires_the_explicit_switch() {
        for text in ["", "[api]\n", "[api]\ntokens=[]\n"] {
            assert!(
                matches!(
                    Config::from_toml_str(text),
                    Err(ConfigError::MissingApiToken)
                ),
                "{text:?}"
            );
        }
        let error = Config::from_toml_str("").unwrap_err().to_string();
        assert!(error.contains("openssl rand -hex 16"), "{error}");
        for listen in ["127.0.0.1:8554", "127.0.0.2:9000", "[::1]:8554"] {
            let config = Config::from_toml_str(&format!(
                "[api]\nlisten='{listen}'\nallow_unauthenticated=true\n"
            ))
            .unwrap();
            assert!(config.api.allow_unauthenticated);
            assert_eq!(config.api.tokens, Vec::<String>::new());
        }
    }

    #[test]
    fn an_unauthenticated_api_must_listen_on_loopback() {
        for listen in [
            "0.0.0.0:8554",
            "192.0.2.10:8554",
            "[::]:8554",
            "[2001:db8::1]:8554",
        ] {
            assert!(
                matches!(
                    Config::from_toml_str(&format!(
                        "[api]\nlisten='{listen}'\nallow_unauthenticated=true\n"
                    )),
                    Err(ConfigError::UnauthenticatedNonLoopback(address))
                        if address == listen.parse::<SocketAddr>().unwrap()
                ),
                "{listen}"
            );
        }
    }

    #[test]
    fn tokens_and_the_unauthenticated_switch_contradict() {
        assert!(matches!(
            Config::from_toml_str(&format!(
                "[api]\ntokens=['{}']\nallow_unauthenticated=true\n",
                token('a')
            )),
            Err(ConfigError::ConflictingApiAuthentication)
        ));
    }

    #[test]
    fn api_tokens_need_thirty_two_visible_ascii_characters() {
        let valid = token('a');
        let config = Config::from_toml_str(&format!(
            "[api]\nlisten='0.0.0.0:8554'\ntokens=['{valid}']\n"
        ))
        .unwrap();
        assert_eq!(config.api.tokens, std::slice::from_ref(&valid));
        let visible_extremes = format!("!{}~", "a".repeat(MIN_API_TOKEN_LEN - 2));
        assert!(Config::from_toml_str(&format!("[api]\ntokens=['{visible_extremes}']\n")).is_ok());

        let short = "a".repeat(MIN_API_TOKEN_LEN - 1);
        let with_space = format!("{} a", "a".repeat(MIN_API_TOKEN_LEN));
        let with_control = format!("{}\\u0001", "a".repeat(MIN_API_TOKEN_LEN));
        let non_ascii = format!("{}é", "a".repeat(MIN_API_TOKEN_LEN));
        for (index, weak) in [short, with_space, with_control, non_ascii]
            .into_iter()
            .enumerate()
        {
            let text = format!("[api]\ntokens=['{valid}', \"{weak}\"]\n");
            let error = Config::from_toml_str(&text).unwrap_err();
            assert!(
                matches!(error, ConfigError::WeakApiToken { index: 1 }),
                "case {index}: {error:?}"
            );
            let message = format!("{error}\n{error:?}");
            assert!(!message.contains(&valid), "{message}");
            assert!(!message.contains("aaaa"), "{message}");
        }
    }

    #[test]
    fn the_example_configuration_loads_once_its_token_is_generated() {
        let example = include_str!("../../../config/cameras.example.toml");
        assert!(matches!(
            Config::from_toml_str(example),
            Err(ConfigError::PlaceholderApiToken { index: 0 })
        ));
        let config = Config::from_toml_str(&example.replace(EXAMPLE_API_TOKEN, &token('c')))
            .expect("the documented example is otherwise valid");
        assert_eq!(config.cameras.len(), 1);
        assert!(!config.api.allow_unauthenticated);
    }

    #[test]
    fn the_example_placeholder_token_is_rejected() {
        assert_eq!(EXAMPLE_API_TOKEN.len(), MIN_API_TOKEN_LEN);
        let error = Config::from_toml_str(&format!(
            "[api]\ntokens=['{}', '{EXAMPLE_API_TOKEN}']\n",
            token('b')
        ))
        .unwrap_err();
        assert!(
            matches!(error, ConfigError::PlaceholderApiToken { index: 1 }),
            "{error:?}"
        );
        assert!(!error.to_string().contains(EXAMPLE_API_TOKEN));
    }

    #[test]
    fn api_debug_redacts_every_token_and_keeps_the_other_fields() {
        let (first, second) = (token('r'), token('s'));
        let config = Config::from_toml_str(&format!(
            "[api]\ntokens=['{first}', '{second}']\nmax_connections=64\n"
        ))
        .unwrap();
        let rendered = format!("{:?}", config.api);
        assert!(
            !rendered.contains(&first) && !rendered.contains(&second),
            "{rendered}"
        );
        assert!(rendered.contains("tokens: [<redacted>; 2]"), "{rendered}");
        for field in [
            "listen: 127.0.0.1:8554",
            "max_connections: 64",
            "max_streaming_media: 8",
            "max_bounded_media: 32",
            "public_metrics: false",
        ] {
            assert!(rendered.contains(field), "missing {field} in {rendered}");
        }
        let pretty = format!("{config:#?}");
        assert!(
            !pretty.contains(&first) && !pretty.contains(&second),
            "{pretty}"
        );
    }

    #[test]
    fn api_admission_limits_have_defaults_and_bounds() {
        const DEV: &str = "[api]\nallow_unauthenticated=true\n";
        let defaults = Config::from_toml_str(DEV).unwrap().api;
        assert_eq!(defaults.max_connections, 256);
        assert_eq!(defaults.max_streaming_media, 8);
        assert_eq!(defaults.max_bounded_media, 32);
        assert!(!defaults.public_metrics);

        let edges = Config::from_toml_str(
            "[api]\nallow_unauthenticated=true\nmax_connections=16\nmax_streaming_media=1\nmax_bounded_media=1024\npublic_metrics=true\n",
        )
        .unwrap()
        .api;
        assert_eq!(edges.max_connections, MIN_API_CONNECTIONS);
        assert_eq!(edges.max_streaming_media, 1);
        assert_eq!(edges.max_bounded_media, MAX_API_MEDIA_RESPONSES);
        assert!(edges.public_metrics);
        assert_eq!(
            Config::from_toml_str(&format!("{DEV}max_connections=65536\n"))
                .unwrap()
                .api
                .max_connections,
            MAX_API_CONNECTIONS
        );

        for connections in [0, 15, 65_537] {
            assert!(matches!(
                Config::from_toml_str(&format!("{DEV}max_connections={connections}\n")),
                Err(ConfigError::InvalidApiMaxConnections(value)) if value == connections
            ));
        }
        for (key, value) in [
            ("max_streaming_media", 0),
            ("max_streaming_media", 1_025),
            ("max_bounded_media", 0),
            ("max_bounded_media", 1_025),
        ] {
            assert!(matches!(
                Config::from_toml_str(&format!("{DEV}{key}={value}\n")),
                Err(ConfigError::InvalidApiMediaLimit { key: rejected, value: got })
                    if rejected == key && got == value
            ));
        }
    }

    #[test]
    fn recorded_stream_kinds_are_unique_per_camera() {
        let camera = "[[camera]]\nid='a'\nname=''\nrtsp_url='rtsp://192.0.2.7/x'\nusername=''\npassword=''\n";
        for kinds in ["['main','main']", "['sub','sub']"] {
            assert!(matches!(
                parse(&format!("{camera}record={kinds}\n")),
                Err(ConfigError::DuplicateRecordedKind { .. })
            ));
        }
        for kinds in ["['audio']", "['audio','audio']"] {
            assert!(matches!(
                parse(&format!("{camera}record={kinds}\n")),
                Err(ConfigError::UnsupportedRecordedKind { .. })
            ));
        }
        for kinds in ["['main','sub']", "[]"] {
            assert!(parse(&format!("{camera}record={kinds}\n")).is_ok());
        }
    }

    #[test]
    fn event_sources_are_unique_per_camera() {
        let camera = "[[camera]]\nid='a'\nname=''\nrtsp_url='rtsp://192.0.2.7/x'\nusername=''\npassword=''\nrecord=[]\n";
        for sources in [
            "['dahua_cgi','dahua_cgi']",
            "['onvif_pull_point','onvif_pull_point']",
        ] {
            assert!(matches!(
                parse(&format!("{camera}events={sources}\n")),
                Err(ConfigError::DuplicateEventSource { .. })
            ));
        }
        for sources in ["[]", "['dahua_cgi']", "['dahua_cgi','onvif_pull_point']"] {
            assert!(
                parse(&format!("{camera}events={sources}\n")).is_ok(),
                "sources {sources}"
            );
        }
        for sources in ["['onvif_metadata']", "['manual']"] {
            assert!(matches!(
                parse(&format!("{camera}events={sources}\n")),
                Err(ConfigError::UnsupportedEventSource { .. })
            ));
        }

        let two_cameras = format!(
            "{camera}events=['dahua_cgi']\n{}events=['dahua_cgi']\n",
            camera.replace("id='a'", "id='b'")
        );
        assert!(parse(&two_cameras).is_ok());
    }

    #[test]
    fn free_space_percentage_is_between_zero_and_fifty() {
        for value in [0, 1, MAX_MIN_FREE_PERCENT] {
            assert!(
                parse(&format!("[storage]\nmin_free_percent={value}\n")).is_ok(),
                "percentage {value}"
            );
        }
        for value in [MAX_MIN_FREE_PERCENT + 1, 100, 101, 255] {
            assert!(
                matches!(
                    parse(&format!("[storage]\nmin_free_percent={value}\n")),
                    Err(ConfigError::InvalidMinFreePercent(v)) if v == value
                ),
                "percentage {value}"
            );
        }
        assert!(parse("").is_ok());
    }

    /// Accepts each value in `accepted` and rejects each in `rejected` for one
    /// storage key, with the `context` lines and every other key at its default.
    fn check_storage_range(
        context: &str,
        key: &str,
        accepted: &[u64],
        rejected: &[u64],
        is_range_error: impl Fn(&ConfigError, u64) -> bool,
    ) {
        for &value in accepted {
            assert!(
                parse(&format!("[storage]\n{context}{key}={value}\n")).is_ok(),
                "{key}={value} must be accepted"
            );
        }
        for &value in rejected {
            match parse(&format!("[storage]\n{context}{key}={value}\n")) {
                Err(error) if is_range_error(&error, value) => {}
                other => panic!("{key}={value} must be rejected, got {other:?}"),
            }
        }
    }

    #[test]
    fn sync_interval_is_between_a_tenth_of_a_second_and_a_minute() {
        check_storage_range(
            "",
            "sync_interval_ms",
            &[MIN_SYNC_INTERVAL_MS, 5_000, MAX_SYNC_INTERVAL_MS],
            &[
                0,
                MIN_SYNC_INTERVAL_MS - 1,
                MAX_SYNC_INTERVAL_MS + 1,
                u64::MAX,
            ],
            |error, value| matches!(error, ConfigError::InvalidSyncIntervalMs(v) if *v == value),
        );
    }

    #[test]
    fn rotation_age_is_between_a_minute_and_a_day() {
        check_storage_range(
            "",
            "rotate_secs",
            &[MIN_ROTATE_SECS, 3_600, MAX_ROTATE_SECS],
            &[0, MIN_ROTATE_SECS - 1, MAX_ROTATE_SECS + 1, u64::MAX],
            |error, value| matches!(error, ConfigError::InvalidRotateSecs(v) if *v == value),
        );
    }

    #[test]
    fn rotation_size_is_between_one_fragment_cap_and_sixty_four_gib() {
        check_storage_range(
            "preallocate_bytes=0\n",
            "rotate_bytes",
            &[MIN_ROTATE_BYTES, 1 << 30, MAX_ROTATE_BYTES],
            &[0, MIN_ROTATE_BYTES - 1, MAX_ROTATE_BYTES + 1, u64::MAX],
            |error, value| matches!(error, ConfigError::InvalidRotateBytes(v) if *v == value),
        );
    }

    #[test]
    fn preallocation_is_capped_by_rotation_size_and_four_gib() {
        let default_rotation = StorageConfig::default().rotate_bytes;
        check_storage_range(
            "",
            "preallocate_bytes",
            &[0, 1, default_rotation],
            &[default_rotation + 1, MAX_PREALLOCATE_BYTES + 1, u64::MAX],
            |error, value| {
                matches!(
                    error,
                    ConfigError::InvalidPreallocateBytes { value: v, max }
                        if *v == value && *max == default_rotation
                )
            },
        );
        let large_files = format!("rotate_bytes={MAX_ROTATE_BYTES}\n");
        assert!(
            parse(&format!(
                "[storage]\n{large_files}preallocate_bytes={MAX_PREALLOCATE_BYTES}\n"
            ))
            .is_ok()
        );
        assert!(matches!(
            parse(&format!(
                "[storage]\n{large_files}preallocate_bytes={}\n",
                MAX_PREALLOCATE_BYTES + 1
            )),
            Err(ConfigError::InvalidPreallocateBytes { max, .. }) if max == MAX_PREALLOCATE_BYTES
        ));
    }

    #[test]
    fn a_configured_quota_is_at_least_one_gib() {
        check_storage_range(
            "",
            "quota_gib",
            &[1, 2_000, u64::MAX],
            &[0],
            |error, value| matches!(error, ConfigError::InvalidQuotaGib(v) if *v == value),
        );
        assert_eq!(parse("").unwrap().storage.quota_gib, None);
    }

    #[test]
    fn default_storage_settings_are_in_range() {
        let storage = StorageConfig::default();
        assert!(storage.validate_ranges().is_ok());
        assert!(storage.min_free_percent <= MAX_MIN_FREE_PERCENT);
    }

    #[test]
    fn fragment_cap_and_queue_bound_are_checked() {
        for value in [1, MAX_FRAGMENT_MS] {
            assert!(
                parse(&format!("[storage]\nmax_fragment_ms={value}\n")).is_ok(),
                "fragment duration {value}"
            );
        }
        for value in [0, MAX_FRAGMENT_MS + 1, u64::MAX] {
            assert!(matches!(
                parse(&format!(
                    "[storage]\nmax_fragment_ms={value}\n"
                )),
                Err(ConfigError::InvalidMaxFragmentMs(v)) if v == value
            ));
        }

        for value in [0, 1, MAX_QUEUED_FRAGMENTS] {
            assert!(
                parse(&format!("[storage]\nmax_queued_fragments={value}\n")).is_ok(),
                "queue bound {value}"
            );
        }
        for value in [MAX_QUEUED_FRAGMENTS + 1, usize::MAX] {
            assert!(matches!(
                parse(&format!(
                    "[storage]\nmax_queued_fragments={value}\n"
                )),
                Err(ConfigError::InvalidMaxQueuedFragments(v)) if v == value
            ));
        }
    }

    #[test]
    fn retention_age_rejects_unrepresentable_microsecond_spans() {
        let camera = "[[camera]]\nid='a'\nname=''\nrtsp_url='rtsp://192.0.2.7/x'\nusername=''\npassword=''\n";
        for days in [1, 2, MAX_RETENTION_DAYS] {
            assert!(
                parse(&format!("{camera}retention_days={days}\n")).is_ok(),
                "retention age {days}"
            );
        }
        for days in [0, MAX_RETENTION_DAYS + 1, 200_000_000, u32::MAX] {
            assert!(matches!(
                parse(&format!("{camera}retention_days={days}\n")),
                Err(ConfigError::InvalidRetentionDays { days: rejected, .. }) if rejected == days
            ));
        }
    }

    #[test]
    fn rtsp_authorities_end_before_queries_and_fragments() {
        let mut camera =
            parse("[[camera]]\nid='a'\nname=''\nrtsp_url='rtsp://192.0.2.7/x'\nusername=''\npassword=''\n")
                .unwrap()
                .cameras
                .remove(0);
        for (authority, host) in [
            ("192.0.2.7", "192.0.2.7"),
            ("camera.example", "camera.example"),
            ("[2001:db8::1]", "2001:db8::1"),
        ] {
            for suffix in ["?stream=1", "#main", "/x?stream=1#main"] {
                for (port_suffix, port) in [("", 554), (":18554", 18554)] {
                    camera.rtsp_url = format!("rtsp://{authority}{port_suffix}{suffix}");
                    camera.sub_url = Some(format!("rtsp://{authority}:18555{suffix}"));
                    assert_eq!(camera.host().as_deref(), Some(host), "{}", camera.rtsp_url);
                    assert_eq!(rtsp_port(&camera.rtsp_url), Some(port));
                    assert_eq!(camera.rtsp_ports(), vec![port, 18555]);
                }
                assert_eq!(rtsp_port(&format!("rtsp://{authority}:bad{suffix}")), None);
            }
        }
    }

    #[test]
    fn http_base_brackets_ipv6_hosts() {
        let mut camera = parse(
            "[[camera]]\nid='a'\nname=''\nrtsp_url='rtsp://[2001:db8::1]/x'\nusername=''\npassword=''\n",
        )
        .unwrap()
        .cameras
        .remove(0);
        assert_eq!(camera.host().as_deref(), Some("2001:db8::1"));
        assert_eq!(
            camera.http_base().as_deref(),
            Some("http://[2001:db8::1]:80")
        );
        camera.https = true;
        assert_eq!(
            camera.http_base().as_deref(),
            Some("https://[2001:db8::1]:443")
        );
        camera.http_port = Some(8443);
        assert_eq!(
            camera.http_base().as_deref(),
            Some("https://[2001:db8::1]:8443")
        );
    }

    #[test]
    fn ingest_defaults_are_a_twenty_millisecond_tick_and_a_32_kib_low_water_mark() {
        let config = parse("").unwrap();
        assert_eq!(
            config.ingest.read_interval(),
            Some(Duration::from_millis(20))
        );
        assert_eq!(config.ingest.recv_low_water(), Some(32 * 1024));
    }

    #[test]
    fn zero_ingest_knobs_mean_packet_driven_reads_and_the_kernel_low_water_default() {
        let config = parse("[ingest]\nread_interval_ms = 0\nrecv_low_water_kib = 0\n").unwrap();
        assert_eq!(config.ingest.read_interval(), None);
        assert_eq!(config.ingest.recv_low_water(), None);
    }

    #[test]
    fn an_explicit_read_interval_is_kept() {
        let config = parse("[ingest]\nread_interval_ms = 40\n").unwrap();
        assert_eq!(
            config.ingest.read_interval(),
            Some(Duration::from_millis(40))
        );
    }

    #[test]
    fn ingest_knobs_are_bounded() {
        assert!(
            parse(&format!(
                "[ingest]\nread_interval_ms = {MAX_READ_INTERVAL_MS}\n"
            ))
            .is_ok()
        );
        assert!(matches!(
            parse(&format!("[ingest]\nread_interval_ms = {}\n", MAX_READ_INTERVAL_MS + 1)),
            Err(ConfigError::InvalidReadIntervalMs(value)) if value == MAX_READ_INTERVAL_MS + 1
        ));
        assert!(
            parse(&format!(
                "[ingest]\nrecv_low_water_kib = {MAX_RECV_LOW_WATER_KIB}\n"
            ))
            .is_ok()
        );
        assert!(matches!(
            parse(&format!("[ingest]\nrecv_low_water_kib = {}\n", MAX_RECV_LOW_WATER_KIB + 1)),
            Err(ConfigError::InvalidRecvLowWaterKib(value)) if value == MAX_RECV_LOW_WATER_KIB + 1
        ));
    }

    #[test]
    fn rtsp_ports_come_from_the_urls_or_default_to_554() {
        assert_eq!(rtsp_port("rtsp://192.0.2.1/x"), Some(554));
        assert_eq!(rtsp_port("rtsp://u:p@192.0.2.1:18554/x"), Some(18554));
        assert_eq!(rtsp_port("rtsp://[2001:db8::1]:8554/x"), Some(8554));
        assert_eq!(rtsp_port("rtsp://[2001:db8::1]/x"), Some(554));
        assert_eq!(rtsp_port("rtsp://192.0.2.1:bad/x"), None);
        assert_eq!(rtsp_port("rtsp:///x"), None);
        assert_eq!(rtsp_port("rtsp://"), None);
        assert_eq!(rtsp_port("not a url"), None);
    }

    #[test]
    fn debug_redacts_the_camera_password() {
        let config = parse(
            "[[camera]]\nid='cam1'\nname='distinctive-name'\n\
             rtsp_url='rtsp://192.0.2.10/main'\nusername='distinctive-user'\n\
             password='distinctive-password'\nretention_days=7\n",
        )
        .unwrap();
        let camera = &config.cameras[0];
        for rendered in [
            format!("{camera:?}"),
            format!("{camera:#?}"),
            format!("{config:?}"),
        ] {
            assert!(rendered.contains("password: \"<redacted>\""), "{rendered}");
            assert!(
                !rendered.contains("distinctive-password"),
                "debug leaked the password"
            );
            for kept in [
                "distinctive-name",
                "distinctive-user",
                "rtsp://192.0.2.10/main",
                "retention_days",
            ] {
                assert!(rendered.contains(kept), "debug lost {kept}: {rendered}");
            }
        }
    }

    #[test]
    fn derives_dahua_sub_url() {
        let config = parse(
            r#"
[[camera]]
id = "cam1"
name = "n"
rtsp_url = "rtsp://h:554/cam/realmonitor?channel=1&subtype=0&unicast=true"
username = "u"
password = "p"
"#,
        )
        .unwrap();
        assert_eq!(
            config.cameras[0].sub_url().as_deref(),
            Some("rtsp://h:554/cam/realmonitor?channel=1&subtype=1&unicast=true")
        );
        assert_eq!(config.cameras[0].host().as_deref(), Some("h"));
    }

    #[test]
    fn sections_left_out_take_their_defaults() {
        let config = parse("").unwrap();
        assert_eq!(config.storage.sync_interval_ms, 5_000);
        assert_eq!(config.api.listen.port(), 8554);
    }

    #[test]
    fn http_helpers_default_to_port_80_with_verified_tls() {
        let camera = parse(
            "[[camera]]\nid=\"a\"\nname=\"\"\nrtsp_url=\"rtsp://h:554/x\"\nusername=\"\"\npassword=\"\"\n",
        )
        .unwrap()
        .cameras
        .remove(0);
        assert_eq!(camera.http_port_or_default(), 80);
        assert!(camera.tls_verify);
        assert_eq!(camera.http_base().as_deref(), Some("http://h:80"));
    }

    #[test]
    fn hosts_are_bare_names_or_addresses() {
        for good in [
            "192.0.2.7",
            "camera.example",
            "Camera.Example",
            "2001:db8::7",
        ] {
            assert!(is_valid_host(good), "{good}");
        }
        let long = "a".repeat(MAX_HOST_BYTES + 1);
        for bad in [
            "",
            "a b",
            "a/b",
            "a:1",
            "u@h",
            "h?x",
            "[::1]",
            "h#f",
            "fe80::1%25eth0",
            long.as_str(),
        ] {
            assert!(!is_valid_host(bad), "{bad}");
        }
    }

    #[test]
    fn parses_host_from_urls() {
        let mut camera = parse(
            "[[camera]]\nid=\"a\"\nname=\"\"\nrtsp_url=\"rtsp://192.0.2.7:554/x\"\nusername=\"\"\npassword=\"\"\n",
        )
        .unwrap()
        .cameras
        .remove(0);
        assert_eq!(camera.host().as_deref(), Some("192.0.2.7"));
        camera.rtsp_url = "rtsp://[2001:db8::1]:554/x".into();
        assert_eq!(camera.host().as_deref(), Some("2001:db8::1"));
        camera.rtsp_url = "nonsense".into();
        assert_eq!(camera.host(), None);
    }

    #[test]
    fn https_cameras_default_to_port_443() {
        let camera = parse(
            "[[camera]]\nid=\"a\"\nname=\"\"\nrtsp_url=\"rtsp://192.0.2.7/x\"\nusername=\"\"\npassword=\"\"\nhttps=true\ntls_verify=false\n",
        )
        .unwrap()
        .cameras
        .remove(0);
        assert_eq!(camera.http_port_or_default(), 443);
        assert!(!camera.tls_verify);
        assert_eq!(camera.http_base().as_deref(), Some("https://192.0.2.7:443"));
    }

    #[test]
    fn rejects_duplicate_camera_ids() {
        let dup = "[[camera]]\nid=\"a\"\nname=\"\"\nrtsp_url=\"rtsp://192.0.2.7/x\"\nusername=\"\"\npassword=\"\"\n[[camera]]\nid=\"a\"\nname=\"\"\nrtsp_url=\"rtsp://192.0.2.7/x\"\nusername=\"\"\npassword=\"\"\n";
        assert!(matches!(parse(dup), Err(ConfigError::DuplicateCamera(_))));
    }

    #[test]
    fn rejects_unknown_keys_in_any_table() {
        assert!(matches!(
            parse("[storage]\nbogus = 1\n"),
            Err(ConfigError::Parse { .. })
        ));
        let unknown_camera_key = "[[camera]]\nid=\"a\"\nname=\"\"\nrtsp_url=\"rtsp://192.0.2.7/x\"\nusername=\"\"\npassword=\"\"\naudio=false\n";
        assert!(matches!(
            parse(unknown_camera_key),
            Err(ConfigError::Parse { .. })
        ));
    }
}
