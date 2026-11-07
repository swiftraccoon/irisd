//! Reconnection pacing shared by the event sources.

use std::time::{Duration, Instant};

const INITIAL: Duration = Duration::from_secs(1);
const MAX: Duration = Duration::from_secs(30);
const HEALTHY_RESET: Duration = Duration::from_secs(60);

/// Exponential backoff with ±20 % independently seeded jitter, reset after a
/// session stays healthy for a minute.
#[derive(Debug)]
pub(crate) struct Backoff {
    next: Duration,
    connected_at: Option<Instant>,
    jitter_state: u64,
}

impl Backoff {
    pub(crate) fn new(seed: &str) -> Self {
        Self {
            next: INITIAL,
            connected_at: None,
            jitter_state: seed_state(seed),
        }
    }

    /// Marks the point at which a session is actually receiving.
    pub(crate) fn session_connected(&mut self) {
        self.connected_at = Some(Instant::now());
    }

    /// Returns the delay before the next attempt and advances the schedule.
    pub(crate) fn delay(&mut self) -> Duration {
        if let Some(connected_at) = self.connected_at.take()
            && connected_at.elapsed() >= HEALTHY_RESET
        {
            self.next = INITIAL;
        }
        let base = self.next;
        self.next = (self.next * 2).min(MAX);
        self.jitter_state = xorshift(self.jitter_state);
        let per_mille = 800 + self.jitter_state % 401;
        Duration::from_micros(base.as_micros() as u64 * per_mille / 1000)
    }
}

fn seed_state(seed: &str) -> u64 {
    let mut state = 0xcbf2_9ce4_8422_2325;
    for byte in seed.bytes() {
        state ^= u64::from(byte);
        state = state.wrapping_mul(0x0000_0100_0000_01b3);
    }
    if state == 0 { 1 } else { state }
}

fn xorshift(mut state: u64) -> u64 {
    state ^= state << 13;
    state ^= state >> 7;
    state ^= state << 17;
    state
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn doubles_to_the_cap_within_jitter_bounds() {
        let mut b = Backoff::new("cam1/dahua");
        let mut expected = INITIAL;
        for _ in 0..8 {
            let d = b.delay();
            let lo = expected.mul_f64(0.79);
            let hi = expected.mul_f64(1.21);
            assert!(d >= lo && d <= hi, "{d:?} outside [{lo:?}, {hi:?}]");
            expected = (expected * 2).min(MAX);
        }
        assert_eq!(expected, MAX);
    }

    #[test]
    fn resets_after_a_healthy_session() {
        let mut b = Backoff::new("cam1/dahua");
        for _ in 0..5 {
            b.delay();
        }
        let long_ago = Instant::now()
            .checked_sub(HEALTHY_RESET + Duration::from_secs(1))
            .expect("process uptime exceeds a minute or the clock supports it");
        b.connected_at = Some(long_ago);
        let d = b.delay();
        assert!(d <= INITIAL.mul_f64(1.21));
    }

    #[test]
    fn seeds_decorrelate_simultaneous_sources() {
        let delays: std::collections::HashSet<Duration> = (0..64)
            .map(|camera| Backoff::new(&format!("cam{camera}/onvif")).delay())
            .collect();
        assert!(delays.len() > 32, "only {} distinct delays", delays.len());
    }

    #[test]
    fn the_same_seed_has_a_reproducible_sequence() {
        let mut left = Backoff::new("cam1/onvif");
        let mut right = Backoff::new("cam1/onvif");
        let left: Vec<Duration> = (0..10).map(|_| left.delay()).collect();
        let right: Vec<Duration> = (0..10).map(|_| right.delay()).collect();
        assert_eq!(left, right);
    }

    #[test]
    fn an_unconnected_long_setup_does_not_reset_the_level() {
        let mut backoff = Backoff::new("cam1/onvif");
        for _ in 0..5 {
            backoff.delay();
        }
        backoff.connected_at = None;
        let delay = backoff.delay();
        assert!(delay >= MAX.mul_f64(0.8));
    }
}
