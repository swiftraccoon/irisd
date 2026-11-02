//! Reconnection pacing shared by the event sources.

use std::time::{Duration, Instant};

const INITIAL: Duration = Duration::from_secs(1);
const MAX: Duration = Duration::from_secs(30);
const HEALTHY_RESET: Duration = Duration::from_secs(60);

/// Exponential backoff with ±20 % clock-derived jitter, reset after a
/// session stays healthy for a minute.
#[derive(Debug)]
pub(crate) struct Backoff {
    next: Duration,
    session_started: Option<Instant>,
}

impl Backoff {
    pub(crate) fn new() -> Self {
        Self {
            next: INITIAL,
            session_started: None,
        }
    }

    /// Marks the start of a session.
    pub(crate) fn session_started(&mut self) {
        self.session_started = Some(Instant::now());
    }

    /// Returns the delay before the next attempt and advances the schedule.
    pub(crate) fn delay(&mut self) -> Duration {
        if let Some(started) = self.session_started.take()
            && started.elapsed() >= HEALTHY_RESET
        {
            self.next = INITIAL;
        }
        let base = self.next;
        self.next = (self.next * 2).min(MAX);
        jitter(base)
    }
}

/// Scales `base` by a factor in `[0.8, 1.2]` derived from the clock.
fn jitter(base: Duration) -> Duration {
    let nanos = u64::from(Instant::now().elapsed().subsec_nanos());
    let per_mille = 800 + (nanos.wrapping_mul(0x9e37_79b9_7f4a_7c15) >> 40) % 401;
    Duration::from_micros(base.as_micros() as u64 * per_mille / 1000)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn doubles_to_the_cap_within_jitter_bounds() {
        let mut b = Backoff::new();
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
        let mut b = Backoff::new();
        for _ in 0..5 {
            b.delay();
        }
        let long_ago = Instant::now()
            .checked_sub(HEALTHY_RESET + Duration::from_secs(1))
            .expect("process uptime exceeds a minute or the clock supports it");
        b.session_started = Some(long_ago);
        let d = b.delay();
        assert!(d <= INITIAL.mul_f64(1.21));
    }
}
