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
    #[error("invalid configuration: {0}")]
    Parse(#[from] toml::de::Error),
    /// Two cameras share an id.
    #[error("duplicate camera id {0}")]
    DuplicateCamera(CameraId),
}

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
/// unique, and every section other than `camera` has a complete default.
/// `config/cameras.example.toml` documents each key.
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
    pub fn from_toml_str(text: &str) -> Result<Self, ConfigError> {
        let config: Self = toml::from_str(text)?;
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
        let mut seen = std::collections::HashSet::new();
        for camera in &self.cameras {
            if !seen.insert(&camera.id) {
                return Err(ConfigError::DuplicateCamera(camera.id.clone()));
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
    pub min_free_percent: u8,
    /// Retention also deletes the oldest sealed segments while the catalogued recordings (open
    /// files at their written length) exceed this many GiB. Unset means no cap.
    pub quota_gib: Option<u64>,
    /// How often open files are `fdatasync`ed. Bounds the power-loss window.
    pub sync_interval_ms: u64,
    /// Longest fragment; a GOP longer than this is split.
    pub max_fragment_ms: u64,
    /// Space reserved ahead of the write position with `fallocate`.
    pub preallocate_bytes: u64,
    /// A file is sealed and a new one started once it reaches this size …
    pub rotate_bytes: u64,
    /// … or this age in seconds, whichever comes first (at the next IRAP).
    pub rotate_secs: u64,
    /// Fragments queued for the writer per stream before new ones are dropped.
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
    /// Bearer tokens accepted by the API. Empty means unauthenticated (development only).
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
    /// Vendor hint enabling vendor-specific event feeds and provisioning.
    #[serde(default)]
    pub vendor: Option<String>,
    /// Which streams to record.
    #[serde(default = "default_record")]
    pub record: Vec<StreamKind>,
    /// Whether to record audio (requires an AAC stream).
    #[serde(default)]
    pub audio: bool,
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
    #[serde(default)]
    pub retention_days: Option<u32>,
    /// Event sources to subscribe to.
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
    #[must_use]
    pub fn http_base(&self) -> Option<String> {
        let scheme = if self.https { "https" } else { "http" };
        Some(format!(
            "{scheme}://{}:{}",
            self.host()?,
            self.http_port_or_default()
        ))
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
    #[must_use]
    pub fn host(&self) -> Option<String> {
        let rest = self.rtsp_url.split("://").nth(1)?;
        let authority = rest.split('/').next()?;
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
    let authority = rest.split('/').next()?;
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
    }
}
