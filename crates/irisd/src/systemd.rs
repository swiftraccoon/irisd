//! Thin wrappers over `sd_notify` so the daemon reports readiness, watchdog
//! liveness, and shutdown to systemd when it runs as a `Type=notify` unit.

use std::time::Duration;

use sd_notify::NotifyState;
use tracing::debug;

/// Reports `READY=1`. Harmless outside systemd.
pub fn ready() {
    send(&[NotifyState::Ready]);
}

/// Reports `STOPPING=1`.
pub fn stopping() {
    send(&[NotifyState::Stopping]);
}

/// Reports `WATCHDOG=1`.
pub fn watchdog() {
    send(&[NotifyState::Watchdog]);
}

/// Reports a free-form status line shown by `systemctl status`.
pub fn status(text: &str) {
    send(&[NotifyState::Status(text)]);
}

/// Asks for `by` more time before the start timeout, while a long start-up
/// step (adopting many files) is still making progress.
pub fn extend_timeout(by: Duration) {
    let micros = u32::try_from(by.as_micros()).unwrap_or(u32::MAX);
    send(&[NotifyState::ExtendTimeoutUsec(micros)]);
}

/// Returns how often the daemon should ping the watchdog: half of
/// `WatchdogSec`, or `None` when no watchdog is configured.
pub fn watchdog_interval() -> Option<Duration> {
    watchdog_interval_from_timeout(sd_notify::watchdog_enabled())
}

fn watchdog_interval_from_timeout(timeout: Option<Duration>) -> Option<Duration> {
    timeout
        .map(|duration| duration / 2)
        .filter(|duration| !duration.is_zero())
}

fn send(state: &[NotifyState<'_>]) {
    if let Err(err) = sd_notify::notify(state) {
        debug!(%err, "sd_notify failed");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zero_or_derived_zero_watchdogs_are_disabled() {
        assert_eq!(watchdog_interval_from_timeout(None), None);
        assert_eq!(watchdog_interval_from_timeout(Some(Duration::ZERO)), None);
        assert_eq!(
            watchdog_interval_from_timeout(Some(Duration::from_nanos(1))),
            None
        );
        assert_eq!(
            watchdog_interval_from_timeout(Some(Duration::from_nanos(2))),
            Some(Duration::from_nanos(1))
        );
    }
}
