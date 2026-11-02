//! What to ingest and how.

use std::time::Duration;

use iris_core::config::TimestampMode;
use iris_core::id::StreamKey;
use iris_core::time::MediaTicks;
use url::Url;

use crate::live::LiveRing;
use crate::session::ReadTicker;

/// One RTSP stream to record.
#[derive(Clone, Debug)]
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
