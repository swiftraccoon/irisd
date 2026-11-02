//! Storage policy derived from configuration.

use std::time::Duration;

use iris_core::config::StorageConfig;

/// Tunables the writer thread applies to every stream, derived from
/// [`StorageConfig`] (sync every 5 s, preallocate 256 MiB, rotate at 1 GiB or
/// one hour by default).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoragePolicy {
    /// How often dirty files are `fdatasync`ed.
    pub sync_interval: Duration,
    /// Bytes reserved ahead of the write position with `fallocate`.
    pub preallocate_bytes: u64,
    /// A file is sealed once it reaches this many bytes …
    pub rotate_bytes: u64,
    /// … or this age in seconds, whichever comes first, at the next sync sample.
    pub rotate_secs: u64,
}

impl From<&StorageConfig> for StoragePolicy {
    fn from(config: &StorageConfig) -> Self {
        Self {
            sync_interval: Duration::from_millis(config.sync_interval_ms),
            preallocate_bytes: config.preallocate_bytes,
            rotate_bytes: config.rotate_bytes,
            rotate_secs: config.rotate_secs,
        }
    }
}

impl Default for StoragePolicy {
    fn default() -> Self {
        Self::from(&StorageConfig::default())
    }
}
