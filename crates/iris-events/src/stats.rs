//! Counters for one event source.

use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};

use iris_core::metrics::Histogram;
use iris_core::time::WallUs;

/// Lock-free counters for one event source, shared with the supervisor.
#[derive(Debug)]
pub struct EventStats {
    events: AtomicU64,
    messages: AtomicU64,
    reconnects: AtomicU64,
    renewals: AtomicU64,
    errors: AtomicU64,
    connected: AtomicBool,
    pull_latency: Histogram,
    /// `i64::MIN` while no event has been received.
    last_event_us: AtomicI64,
    /// `i64::MIN` while no message has been received.
    last_message_us: AtomicI64,
}

impl Default for EventStats {
    fn default() -> Self {
        Self::new()
    }
}

impl EventStats {
    /// Creates zeroed counters.
    #[must_use]
    pub fn new() -> Self {
        Self {
            events: AtomicU64::new(0),
            messages: AtomicU64::new(0),
            reconnects: AtomicU64::new(0),
            renewals: AtomicU64::new(0),
            errors: AtomicU64::new(0),
            connected: AtomicBool::new(false),
            pull_latency: Histogram::new(),
            last_event_us: AtomicI64::new(i64::MIN),
            last_message_us: AtomicI64::new(i64::MIN),
        }
    }

    /// Events published so far.
    pub fn events(&self) -> u64 {
        self.events.load(Ordering::Relaxed)
    }

    /// Messages received: multipart parts (heartbeats included) or successful
    /// pulls, whether or not they carried an event.
    pub fn messages(&self) -> u64 {
        self.messages.load(Ordering::Relaxed)
    }

    /// Failed-session retry or resubscription cycles entered so far.
    pub fn reconnects(&self) -> u64 {
        self.reconnects.load(Ordering::Relaxed)
    }

    /// Subscription renewals that succeeded.
    pub fn renewals(&self) -> u64 {
        self.renewals.load(Ordering::Relaxed)
    }

    /// Failed sessions, counted once before each retry cycle.
    pub fn errors(&self) -> u64 {
        self.errors.load(Ordering::Relaxed)
    }

    /// Whether the source is receiving (a heartbeat counts).
    pub fn connected(&self) -> bool {
        self.connected.load(Ordering::Relaxed)
    }

    /// Round-trip times of `PullMessages` calls, successes and failures alike.
    pub fn pull_latency(&self) -> &Histogram {
        &self.pull_latency
    }

    /// NVR time of the last published event.
    pub fn last_event_wall(&self) -> Option<WallUs> {
        match self.last_event_us.load(Ordering::Relaxed) {
            i64::MIN => None,
            micros => Some(WallUs::new(micros)),
        }
    }

    /// NVR time of the last message received, heartbeats and empty pulls included.
    pub fn last_message_wall(&self) -> Option<WallUs> {
        match self.last_message_us.load(Ordering::Relaxed) {
            i64::MIN => None,
            micros => Some(WallUs::new(micros)),
        }
    }

    pub(crate) fn record_event(&self, now: WallUs) {
        self.events.fetch_add(1, Ordering::Relaxed);
        self.last_event_us.store(now.get(), Ordering::Relaxed);
    }

    pub(crate) fn record_message(&self, now: WallUs) {
        self.messages.fetch_add(1, Ordering::Relaxed);
        self.last_message_us.store(now.get(), Ordering::Relaxed);
    }

    pub(crate) fn record_reconnect(&self) {
        self.reconnects.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn record_renewal(&self) {
        self.renewals.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn record_error(&self) {
        self.errors.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn set_connected(&self, connected: bool) {
        self.connected.store(connected, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    #[test]
    fn counters_start_empty_and_record_each_kind() {
        let stats = EventStats::new();
        assert_eq!(stats.last_message_wall(), None);
        assert_eq!(stats.last_event_wall(), None);
        assert_eq!(EventStats::default().last_message_wall(), None);
        assert_eq!(
            (stats.messages(), stats.renewals(), stats.errors()),
            (0, 0, 0)
        );
        assert_eq!(stats.pull_latency().snapshot().count(), 0);

        stats.record_message(WallUs::new(5));
        stats.record_event(WallUs::new(7));
        stats.record_renewal();
        stats.record_error();
        stats.record_error();
        stats.pull_latency().record(Duration::from_millis(3));

        assert_eq!(stats.last_message_wall(), Some(WallUs::new(5)));
        assert_eq!(stats.last_event_wall(), Some(WallUs::new(7)));
        assert_eq!((stats.messages(), stats.events()), (1, 1));
        assert_eq!((stats.renewals(), stats.errors()), (1, 2));
        assert_eq!(stats.pull_latency().snapshot().count(), 1);
        assert_eq!(stats.pull_latency().take_max(), Duration::from_millis(3));
    }
}
