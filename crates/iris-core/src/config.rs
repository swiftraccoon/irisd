//! TOML configuration: cameras and storage policy.
//!
//! ```
//! use iris_core::config::Config;
//!
//! let config = Config::from_toml_str(r#"
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

use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::event::EventSource;
use crate::id::{CameraId, StreamKind};

/// Errors from loading configuration.
#[derive(Debug, Error)]
pub enum ConfigError {
    /// The file could not be read.
    #[error("cannot read configuration file {path}: {source}")]
    Io {
        /// The path that failed.
        path: String,
        /// The underlying error.
        #[source]
        source: std::io::Error,
    },
    /// The TOML did not parse or did not match the schema.
    #[error("invalid configuration syntax or schema (byte span: {span:?})")]
    Parse {
        /// Location supplied by the parser, without input text or value-bearing messages.
        span: Option<std::ops::Range<usize>>,
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
    #[error("camera {camera} subscribes to event source {event_source:?} more than once")]
    DuplicateEventSource {
        /// The camera with the repeated source.
        camera: CameraId,
        /// The repeated event source.
        event_source: EventSource,
    },
    /// One camera requests an event source the daemon cannot subscribe to.
    #[error("camera {camera} requests unsupported event source {event_source:?}")]
    UnsupportedEventSource {
        /// The camera with the unsupported source.
        camera: CameraId,
        /// The source that cannot be configured as a subscription.
        event_source: EventSource,
    },
    /// The free-space floor is not a percentage.
    #[error("storage.min_free_percent must be between 0 and 100, got {0}")]
    InvalidMinFreePercent(u8),
    /// One configured API bearer is empty or contains only whitespace.
    #[error("api.tokens entries must not be blank")]
    BlankApiToken,
    /// The fragment duration is zero or exceeds the supported operational bound.
    #[error("storage.max_fragment_ms must be between 1 and {MAX_FRAGMENT_MS}, got {0}")]
    InvalidMaxFragmentMs(u64),
    /// The per-stream writer queue exceeds the supported operational bound.
    #[error("storage.max_queued_fragments must not exceed {MAX_QUEUED_FRAGMENTS}, got {0}")]
    InvalidMaxQueuedFragments(usize),
    /// A camera retention age cannot be represented safely in microseconds.
    #[error("camera {camera} retention_days must not exceed {MAX_RETENTION_DAYS}, got {days}")]
    InvalidRetentionDays {
        /// The camera with the invalid age policy.
        camera: CameraId,
        /// The rejected number of days.
        days: u32,
    },
}

/// Largest accepted fragment duration, in milliseconds.
///
/// One minute is thirty times the ordinary two-second GOP cap and prevents an
/// invalid configuration from producing a pathological fragment allocation.
pub const MAX_FRAGMENT_MS: u64 = 60_000;

/// Largest accepted number of queued fragments per stream.
///
/// This is 128 times the default and keeps the synchronous writer channel
/// operationally bounded independently of the host pointer width.
pub const MAX_QUEUED_FRAGMENTS: usize = 4_096;

/// Largest retention age whose microsecond span fits in an [`i64`].
pub const MAX_RETENTION_DAYS: u32 = 106_751_991;

/// How camera sockets are read.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields, default)]
pub struct IngestConfig {
    /// How often each camera socket is read, in milliseconds. Zero reads as soon
    /// as a packet arrives, one wake-up per packet. A positive value lets packets
    /// accumulate between reads so one system call covers many of them, and all
    /// streams are read in the same wake-up, at the cost of that much extra live
    /// latency. The default of 20 ms costs about a quarter less CPU than zero
    /// (docs/research/12-ingest-cost.md).
    pub read_interval_ms: u64,
    /// Receive low-water mark per camera socket in KiB (`SO_RCVLOWAT`, set once
    /// a session plays): the kernel reports the socket readable only once this
    /// much is queued, so a steady stream wakes the daemon once per that many
    /// bytes instead of once per packet. Zero leaves the kernel default of one
    /// byte. A slow stream waits up to this many bytes before its frames are
    /// read (a quarter second at 1 Mbit/s with the default of 32), so keep it
    /// well under a second of the slowest stream's bitrate.
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

impl IngestConfig {
    /// The receive low-water mark in bytes, `None` for the kernel default.
    #[must_use]
    pub fn recv_low_water(&self) -> Option<u32> {
        (self.recv_low_water_kib > 0).then(|| self.recv_low_water_kib.saturating_mul(1024))
    }

    /// The read interval, `None` when sockets are read as packets arrive.
    #[must_use]
    pub fn read_interval(&self) -> Option<std::time::Duration> {
        (self.read_interval_ms > 0).then(|| std::time::Duration::from_millis(self.read_interval_ms))
    }
}

/// Top-level configuration: storage, API, ingest policy, and cameras.
///
/// Unknown keys are errors rather than silent defaults, camera ids must be
/// unique, recorded stream kinds must be distinct supported video kinds, event sources
/// must be unique per camera,
/// and the free-space percentage must be in 0..=100. Every section other than
/// `camera` has a complete default. `config/cameras.example.toml` documents each key.
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
    /// The cameras to record.
    #[serde(default, rename = "camera")]
    pub cameras: Vec<CameraConfig>,
}

impl Config {
    /// Parses configuration from TOML text.
    ///
    /// Parse failures expose only a generic description and byte span, because
    /// parser messages and source excerpts can contain credentials.
    pub fn from_toml_str(text: &str) -> Result<Self, ConfigError> {
        let config: Self = toml::from_str(text)
            .map_err(|error: toml::de::Error| ConfigError::Parse { span: error.span() })?;
        config.validate()?;
        Ok(config)
    }

    /// Loads configuration from a file.
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let text = std::fs::read_to_string(path).map_err(|source| ConfigError::Io {
            path: path.display().to_string(),
            source,
        })?;
        Self::from_toml_str(&text)
    }

    fn validate(&self) -> Result<(), ConfigError> {
        if self.storage.min_free_percent > 100 {
            return Err(ConfigError::InvalidMinFreePercent(
                self.storage.min_free_percent,
            ));
        }
        if self.api.tokens.iter().any(|token| token.trim().is_empty()) {
            return Err(ConfigError::BlankApiToken);
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
        let mut seen = std::collections::HashSet::new();
        for camera in &self.cameras {
            if !seen.insert(&camera.id) {
                return Err(ConfigError::DuplicateCamera(camera.id.clone()));
            }
            if let Some(days) = camera.retention_days
                && days > MAX_RETENTION_DAYS
            {
                return Err(ConfigError::InvalidRetentionDays {
                    camera: camera.id.clone(),
                    days,
                });
            }
            let mut kinds = std::collections::HashSet::new();
            for &kind in &camera.record {
                if kind == StreamKind::Audio {
                    return Err(ConfigError::UnsupportedRecordedKind {
                        camera: camera.id.clone(),
                        kind,
                    });
                }
                if !kinds.insert(kind) {
                    return Err(ConfigError::DuplicateRecordedKind {
                        camera: camera.id.clone(),
                        kind,
                    });
                }
            }
            let mut event_sources = std::collections::HashSet::new();
            for &event_source in &camera.events {
                if matches!(
                    event_source,
                    EventSource::OnvifMetadata | EventSource::Manual
                ) {
                    return Err(ConfigError::UnsupportedEventSource {
                        camera: camera.id.clone(),
                        event_source,
                    });
                }
                if !event_sources.insert(event_source) {
                    return Err(ConfigError::DuplicateEventSource {
                        camera: camera.id.clone(),
                        event_source,
                    });
                }
            }
        }
        Ok(())
    }
}

/// Storage policy. Sizes are in bytes, intervals in milliseconds.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields, default)]
pub struct StorageConfig {
    /// Directories that receive recordings; the first with enough free space is used.
    pub video_roots: Vec<PathBuf>,
    /// Directory for the catalog database and other small state.
    pub state_dir: PathBuf,
    /// Retention deletes the oldest sealed segments while a root has less free space than this.
    /// Must be in 0..=100; zero disables the free-space floor.
    pub min_free_percent: u8,
    /// Retention also deletes the oldest sealed segments while the catalogued recordings (open
    /// files at their written length) exceed this many GiB. Unset means no cap.
    pub quota_gib: Option<u64>,
    /// How often open files are `fdatasync`ed. Bounds the power-loss window.
    pub sync_interval_ms: u64,
    /// Longest fragment; a GOP longer than this is split. Must be in
    /// `1..=MAX_FRAGMENT_MS`.
    pub max_fragment_ms: u64,
    /// Space reserved ahead of the write position with `fallocate`.
    pub preallocate_bytes: u64,
    /// A file is sealed and a new one started once it reaches this size …
    pub rotate_bytes: u64,
    /// … or this age in seconds, whichever comes first (at the next IRAP).
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

/// HTTP API settings.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields, default)]
pub struct ApiConfig {
    /// Address to listen on.
    pub listen: SocketAddr,
    /// Bearer tokens accepted by the API. The list may be empty to disable authentication for
    /// development, but individual entries must contain a non-whitespace token.
    pub tokens: Vec<String>,
}

impl Default for ApiConfig {
    fn default() -> Self {
        Self {
            listen: "127.0.0.1:8554".parse().expect("valid literal"),
            tokens: Vec::new(),
        }
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

/// One camera.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CameraConfig {
    /// Stable identifier used in paths and URLs.
    pub id: CameraId,
    /// Human-readable name.
    pub name: String,
    /// RTSP URL of the main stream, without credentials.
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
    /// Must not exceed [`MAX_RETENTION_DAYS`].
    #[serde(default)]
    pub retention_days: Option<u32>,
    /// Distinct event sources to subscribe to.
    #[serde(default)]
    pub events: Vec<EventSource>,
}

fn default_true() -> bool {
    true
}

fn default_record() -> Vec<StreamKind> {
    vec![StreamKind::Main, StreamKind::Sub]
}

impl CameraConfig {
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

    /// Returns the host part of `rtsp_url`, used for vendor HTTP and ONVIF calls.
    /// IPv6 hosts have no brackets; URL builders must add them to the authority.
    #[must_use]
    pub fn host(&self) -> Option<String> {
        let rest = self.rtsp_url.split("://").nth(1)?;
        let authority = rest.split(['/', '?', '#']).next()?;
        let host_port = authority.rsplit('@').next()?;
        let host = if let Some(stripped) = host_port.strip_prefix('[') {
            stripped.split(']').next()?
        } else {
            host_port.split(':').next()?
        };
        (!host.is_empty()).then(|| host.to_owned())
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

/// The port of an RTSP URL: the explicit one, or 554 for an `rtsp://` URL
/// without one. `None` when the URL has no authority.
#[must_use]
pub fn rtsp_port(url: &str) -> Option<u16> {
    let rest = url.split("://").nth(1)?;
    let authority = rest.split(['/', '?', '#']).next()?;
    let host_port = authority.rsplit('@').next()?;
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
        assert!(error.to_string().contains("byte span"));
    }

    #[test]
    fn blank_api_tokens_fail_closed() {
        assert!(Config::from_toml_str("[api]\ntokens=[]\n").is_ok());
        for tokens in ["['']", "['   ']", "['valid', '']", "[\"valid\", \"\\t\"]"] {
            assert!(matches!(
                Config::from_toml_str(&format!("[api]\ntokens={tokens}\n")),
                Err(ConfigError::BlankApiToken)
            ));
        }
    }

    #[test]
    fn recorded_stream_kinds_are_unique_per_camera() {
        let camera = "[[camera]]\nid='a'\nname=''\nrtsp_url=''\nusername=''\npassword=''\n";
        for kinds in ["['main','main']", "['sub','sub']", "['audio','audio']"] {
            assert!(Config::from_toml_str(&format!("{camera}record={kinds}\n")).is_err());
        }
        assert!(matches!(
            Config::from_toml_str(&format!("{camera}record=['audio']\n")),
            Err(ConfigError::UnsupportedRecordedKind { .. })
        ));
        for kinds in ["['main','sub']", "[]"] {
            assert!(Config::from_toml_str(&format!("{camera}record={kinds}\n")).is_ok());
        }
    }

    #[test]
    fn event_sources_are_unique_per_camera() {
        let camera =
            "[[camera]]\nid='a'\nname=''\nrtsp_url=''\nusername=''\npassword=''\nrecord=[]\n";
        for sources in [
            "['dahua_cgi','dahua_cgi']",
            "['onvif_pull_point','onvif_pull_point']",
        ] {
            assert!(matches!(
                Config::from_toml_str(&format!("{camera}events={sources}\n")),
                Err(ConfigError::DuplicateEventSource { .. })
            ));
        }
        for sources in ["[]", "['dahua_cgi']", "['dahua_cgi','onvif_pull_point']"] {
            assert!(
                Config::from_toml_str(&format!("{camera}events={sources}\n")).is_ok(),
                "sources {sources}"
            );
        }
        for sources in ["['onvif_metadata']", "['manual']"] {
            assert!(matches!(
                Config::from_toml_str(&format!("{camera}events={sources}\n")),
                Err(ConfigError::UnsupportedEventSource { .. })
            ));
        }

        let two_cameras = format!(
            "{camera}events=['dahua_cgi']\n{}events=['dahua_cgi']\n",
            camera.replace("id='a'", "id='b'")
        );
        assert!(Config::from_toml_str(&two_cameras).is_ok());
    }

    #[test]
    fn free_space_percentage_is_between_zero_and_one_hundred() {
        for value in [0, 100, 101, 255] {
            let result = Config::from_toml_str(&format!("[storage]\nmin_free_percent={value}\n"));
            assert_eq!(result.is_ok(), value <= 100, "percentage {value}");
        }
        assert!(Config::from_toml_str("").is_ok());
    }

    #[test]
    fn storage_resource_limits_are_profile_independent() {
        for value in [1, MAX_FRAGMENT_MS] {
            assert!(
                Config::from_toml_str(&format!("[storage]\nmax_fragment_ms={value}\n")).is_ok(),
                "fragment duration {value}"
            );
        }
        for value in [0, MAX_FRAGMENT_MS + 1, u64::MAX] {
            assert!(matches!(
                Config::from_toml_str(&format!(
                    "[storage]\nmax_fragment_ms={value}\n"
                )),
                Err(ConfigError::InvalidMaxFragmentMs(v)) if v == value
            ));
        }

        for value in [0, 1, MAX_QUEUED_FRAGMENTS] {
            assert!(
                Config::from_toml_str(&format!("[storage]\nmax_queued_fragments={value}\n"))
                    .is_ok(),
                "queue bound {value}"
            );
        }
        for value in [MAX_QUEUED_FRAGMENTS + 1, usize::MAX] {
            assert!(matches!(
                Config::from_toml_str(&format!(
                    "[storage]\nmax_queued_fragments={value}\n"
                )),
                Err(ConfigError::InvalidMaxQueuedFragments(v)) if v == value
            ));
        }
    }

    #[test]
    fn retention_age_rejects_unrepresentable_microsecond_spans() {
        let camera = "[[camera]]\nid='a'\nname=''\nrtsp_url=''\nusername=''\npassword=''\n";
        for days in [0, 1, MAX_RETENTION_DAYS] {
            assert!(
                Config::from_toml_str(&format!("{camera}retention_days={days}\n")).is_ok(),
                "retention age {days}"
            );
        }
        for days in [MAX_RETENTION_DAYS + 1, 200_000_000, u32::MAX] {
            assert!(matches!(
                Config::from_toml_str(&format!("{camera}retention_days={days}\n")),
                Err(ConfigError::InvalidRetentionDays { days: rejected, .. }) if rejected == days
            ));
        }
    }

    #[test]
    fn rtsp_authorities_end_before_queries_and_fragments() {
        let mut camera = Config::from_toml_str(
            "[[camera]]\nid='a'\nname=''\nrtsp_url=''\nusername=''\npassword=''\n",
        )
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
        let mut camera = Config::from_toml_str(
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
    fn ingest_read_interval_defaults_to_twenty_milliseconds() {
        let config = Config::from_toml_str("[[camera]]\nid=\"a\"\nname=\"\"\nrtsp_url=\"rtsp://192.0.2.1/x\"\nusername=\"\"\npassword=\"\"\n").unwrap();
        assert_eq!(
            config.ingest.read_interval(),
            Some(std::time::Duration::from_millis(20))
        );
        assert_eq!(config.ingest.recv_low_water(), Some(32 * 1024));
        let config =
            Config::from_toml_str("[ingest]\nread_interval_ms = 0\nrecv_low_water_kib = 0\n")
                .unwrap();
        assert_eq!(config.ingest.read_interval(), None);
        assert_eq!(config.ingest.recv_low_water(), None);
        let config = Config::from_toml_str("[ingest]\nread_interval_ms = 40\n").unwrap();
        assert_eq!(
            config.ingest.read_interval(),
            Some(std::time::Duration::from_millis(40))
        );
    }

    #[test]
    fn rtsp_ports_come_from_the_urls_or_default_to_554() {
        assert_eq!(rtsp_port("rtsp://192.0.2.1/x"), Some(554));
        assert_eq!(rtsp_port("rtsp://u:p@192.0.2.1:18554/x"), Some(18554));
        assert_eq!(rtsp_port("rtsp://[2001:db8::1]:8554/x"), Some(8554));
        assert_eq!(rtsp_port("rtsp://[2001:db8::1]/x"), Some(554));
        assert_eq!(rtsp_port("rtsp://192.0.2.1:bad/x"), None);
        assert_eq!(rtsp_port("not a url"), None);
    }

    use super::*;

    #[test]
    fn derives_dahua_sub_url() {
        let config = Config::from_toml_str(
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
        assert_eq!(config.storage.sync_interval_ms, 5_000);
        assert_eq!(config.cameras[0].host().as_deref(), Some("h"));
        assert_eq!(config.cameras[0].http_port_or_default(), 80);
        assert!(config.cameras[0].tls_verify);
        assert_eq!(
            config.cameras[0].http_base().as_deref(),
            Some("http://h:80")
        );
        assert_eq!(config.api.listen.port(), 8554);
    }

    #[test]
    fn parses_host_from_urls() {
        let mut camera = Config::from_toml_str(
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
        let camera = Config::from_toml_str(
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
    fn rejects_duplicate_ids_and_unknown_fields() {
        let dup = "[[camera]]\nid=\"a\"\nname=\"\"\nrtsp_url=\"\"\nusername=\"\"\npassword=\"\"\n[[camera]]\nid=\"a\"\nname=\"\"\nrtsp_url=\"\"\nusername=\"\"\npassword=\"\"\n";
        assert!(matches!(
            Config::from_toml_str(dup),
            Err(ConfigError::DuplicateCamera(_))
        ));
        assert!(Config::from_toml_str("[storage]\nbogus = 1\n").is_err());
        let unsupported_audio = "[[camera]]\nid=\"a\"\nname=\"\"\nrtsp_url=\"\"\nusername=\"\"\npassword=\"\"\naudio=false\n";
        assert!(Config::from_toml_str(unsupported_audio).is_err());
    }
}
