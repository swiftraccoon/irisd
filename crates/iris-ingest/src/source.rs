//! What to ingest and how.

use std::fmt;
use std::time::Duration;

use iris_core::config::TimestampMode;
use iris_core::id::StreamKey;
use iris_core::time::MediaTicks;
use url::Url;

use crate::live::LiveRing;
use crate::session::ReadTicker;

/// One RTSP stream to record.
///
/// The `Debug` output redacts the credentials, and the whole URL if it
/// carries user information anyway.
#[derive(Clone)]
pub struct StreamSource {
    /// Owning camera and stream kind.
    pub key: StreamKey,
    /// RTSP URL without credentials.
    pub url: Url,
    /// RTSP username.
    pub username: String,
    /// RTSP password.
    pub password: String,
}

impl fmt::Debug for StreamSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let url: &dyn fmt::Debug =
            if self.url.username().is_empty() && self.url.password().is_none() {
                &self.url
            } else {
                &"<redacted>"
            };
        f.debug_struct("StreamSource")
            .field("key", &self.key)
            .field("url", url)
            .field("username", &"<redacted>")
            .field("password", &"<redacted>")
            .finish()
    }
}

/// Ingest policy.
#[derive(Clone, Debug)]
pub struct IngestOptions {
    /// Longest fragment; a GOP longer than this is split.
    pub max_fragment: MediaTicks,
    /// How frame durations are derived.
    pub timestamp_mode: TimestampMode,
    /// A session with no frames for this long is torn down and reconnected.
    pub inactivity_timeout: Duration,
    /// Timestamp jumps larger than this many seconds end the session.
    pub max_jump_secs: u32,
    /// User-Agent sent to cameras.
    pub user_agent: String,
    /// Where to publish frames for live viewers, if anyone watches.
    pub live: Option<LiveRing>,
    /// Read the camera socket on this cadence instead of on every packet: the
    /// socket is drained without arming a wake-up, then the task sleeps until
    /// the next tick of a clock shared by every stream, so one runtime wake-up
    /// reads all sockets and each read covers many packets.
    pub read_interval: Option<Duration>,
    /// The shared read ticker for `read_interval`, from [`ReadTicker::spawn`]
    /// on the runtime the sessions run on. Without it each stream runs its own
    /// timer, which costs one runtime unpark per stream per tick.
    pub ticker: Option<ReadTicker>,
    /// Ask the kernel to report the camera socket readable only once this many
    /// bytes are queued (`SO_RCVLOWAT`, set after `PLAY`), so a steady RTP flow
    /// wakes the runtime once per that many bytes instead of once per packet.
    pub recv_low_water: Option<u32>,
}

impl Default for IngestOptions {
    fn default() -> Self {
        Self {
            max_fragment: MediaTicks::from_secs(2),
            timestamp_mode: TimestampMode::Regularize,
            inactivity_timeout: Duration::from_secs(30),
            max_jump_secs: 10,
            user_agent: "irisd/0.1".to_owned(),
            live: None,
            read_interval: None,
            ticker: None,
            recv_low_water: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_redacts_credentials() {
        let source = StreamSource {
            key: StreamKey {
                camera: "cam1".parse().expect("camera id"),
                kind: iris_core::id::StreamKind::Main,
            },
            url: "rtsp://192.0.2.10/cam".parse().expect("URL"),
            username: "distinctive-user".to_owned(),
            password: "distinctive-password".to_owned(),
        };
        let embedded = StreamSource {
            url: "rtsp://distinctive-user:distinctive-password@192.0.2.10/cam"
                .parse()
                .expect("URL"),
            ..source.clone()
        };
        for (source, shows_url) in [(source, true), (embedded, false)] {
            for rendered in [format!("{source:?}"), format!("{source:#?}")] {
                assert!(rendered.contains("<redacted>"), "{rendered}");
                assert!(rendered.contains("cam1"), "{rendered}");
                assert_eq!(rendered.contains("192.0.2.10"), shows_url, "{rendered}");
                for marker in ["distinctive-user", "distinctive-password"] {
                    assert!(!rendered.contains(marker), "debug leaked {marker}");
                }
            }
        }
    }
}
