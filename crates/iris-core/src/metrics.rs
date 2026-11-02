//! Lock-free latency histograms and high-water marks for the statistics lines.
//!
//! A [`Histogram`] counts durations in fixed logarithmic buckets so a reporter can
//! print percentiles once a minute without a lock or an allocation on the hot path.
//! A [`Peak`] keeps the largest value seen since it was last read.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

/// Upper bounds of the closed buckets, in microseconds. One open bucket follows.
const BOUNDS_US: [u64; 16] = [
    100, 200, 500, 1_000, 2_000, 5_000, 10_000, 20_000, 50_000, 100_000, 200_000, 500_000,
    1_000_000, 2_000_000, 5_000_000, 10_000_000,
];
/// Closed buckets plus the open one.
const BUCKETS: usize = BOUNDS_US.len() + 1;

/// A latency histogram with fixed logarithmic buckets, updatable from any thread.
#[derive(Debug, Default)]
pub struct Histogram {
    buckets: [AtomicU64; BUCKETS],
    count: AtomicU64,
    sum_us: AtomicU64,
    max_us: AtomicU64,
}

impl Histogram {
    /// Creates an empty histogram.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Counts one duration.
    pub fn record(&self, duration: Duration) {
        let us = u64::try_from(duration.as_micros()).unwrap_or(u64::MAX);
        let index = BOUNDS_US
            .iter()
            .position(|&bound| us <= bound)
            .unwrap_or(BOUNDS_US.len());
        self.buckets[index].fetch_add(1, Ordering::Relaxed);
        self.count.fetch_add(1, Ordering::Relaxed);
        self.sum_us.fetch_add(us, Ordering::Relaxed);
        self.max_us.fetch_max(us, Ordering::Relaxed);
    }

    /// Copies the counters.
    #[must_use]
    pub fn snapshot(&self) -> HistogramSnapshot {
        HistogramSnapshot {
            buckets: std::array::from_fn(|i| self.buckets[i].load(Ordering::Relaxed)),
            count: self.count.load(Ordering::Relaxed),
            sum_us: self.sum_us.load(Ordering::Relaxed),
            max_us: self.max_us.load(Ordering::Relaxed),
        }
    }

    /// The largest duration recorded since the previous call, which this call resets.
    pub fn take_max(&self) -> Duration {
        Duration::from_micros(self.max_us.swap(0, Ordering::Relaxed))
    }
}

/// A copy of a [`Histogram`]'s counters.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct HistogramSnapshot {
    buckets: [u64; BUCKETS],
    count: u64,
    sum_us: u64,
    max_us: u64,
}

impl HistogramSnapshot {
    /// Durations counted.
    #[must_use]
    pub fn count(&self) -> u64 {
        self.count
    }

    /// The largest duration counted, or zero when empty.
    #[must_use]
    pub fn max(&self) -> Duration {
        Duration::from_micros(self.max_us)
    }

    /// The mean duration, or zero when empty.
    #[must_use]
    pub fn mean(&self) -> Duration {
        self.sum_us
            .checked_div(self.count)
            .map_or(Duration::ZERO, Duration::from_micros)
    }

    /// The counts added after `earlier` was taken. The maximum stays the overall one;
    /// use [`Histogram::take_max`] for an interval maximum.
    #[must_use]
    pub fn since(&self, earlier: &Self) -> Self {
        Self {
            buckets: std::array::from_fn(|i| self.buckets[i].saturating_sub(earlier.buckets[i])),
            count: self.count.saturating_sub(earlier.count),
            sum_us: self.sum_us.saturating_sub(earlier.sum_us),
            max_us: self.max_us,
        }
    }

    /// An upper bound on the `p`-th percentile (0 to 100): the bound of the bucket that
    /// holds that sample, or the maximum when it fell in the open bucket. Zero when empty.
    #[must_use]
    pub fn percentile(&self, p: f64) -> Duration {
        if self.count == 0 {
            return Duration::ZERO;
        }
        let fraction = p.clamp(0.0, 100.0) / 100.0;
        // Rank from 1 to count; the cast keeps the ceiling exact below 2^53 samples.
        let rank = (fraction * self.count as f64).ceil().max(1.0) as u64;
        let mut seen = 0u64;
        for (index, &n) in self.buckets.iter().enumerate() {
            seen += n;
            if seen >= rank {
                return match BOUNDS_US.get(index) {
                    Some(&bound) => Duration::from_micros(bound),
                    None => Duration::from_micros(self.max_us),
                };
            }
        }
        Duration::from_micros(self.max_us)
    }
}

/// The largest value observed since the last read.
#[derive(Debug, Default)]
pub struct Peak(AtomicU64);

impl Peak {
    /// Creates a zero peak.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Raises the peak to `value` if it is larger.
    pub fn observe(&self, value: u64) {
        self.0.fetch_max(value, Ordering::Relaxed);
    }

    /// The peak since the last [`Peak::take`].
    #[must_use]
    pub fn get(&self) -> u64 {
        self.0.load(Ordering::Relaxed)
    }

    /// Returns the peak and resets it to zero.
    pub fn take(&self) -> u64 {
        self.0.swap(0, Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations_land_in_logarithmic_buckets() {
        let h = Histogram::new();
        h.record(Duration::from_micros(50));
        h.record(Duration::from_micros(150));
        h.record(Duration::from_secs(20));
        let s = h.snapshot();
        assert_eq!(s.count(), 3);
        assert_eq!(s.percentile(0.0), Duration::from_micros(100));
        assert_eq!(s.percentile(50.0), Duration::from_micros(200));
        assert_eq!(s.percentile(99.0), Duration::from_secs(20));
        assert_eq!(s.max(), Duration::from_secs(20));
        assert_eq!(s.mean(), Duration::from_micros((50 + 150 + 20_000_000) / 3));
    }

    #[test]
    fn empty_snapshot_reports_zero() {
        let s = Histogram::new().snapshot();
        assert_eq!(s.count(), 0);
        assert_eq!(s.percentile(99.0), Duration::ZERO);
        assert_eq!(s.mean(), Duration::ZERO);
    }

    #[test]
    fn since_yields_only_the_new_counts_and_take_max_resets() {
        let h = Histogram::new();
        h.record(Duration::from_millis(3));
        let earlier = h.snapshot();
        h.record(Duration::from_millis(30));
        h.record(Duration::from_millis(30));
        let delta = h.snapshot().since(&earlier);
        assert_eq!(delta.count(), 2);
        assert_eq!(delta.percentile(50.0), Duration::from_millis(50));
        assert_eq!(h.take_max(), Duration::from_millis(30));
        assert_eq!(h.take_max(), Duration::ZERO);
    }

    #[test]
    fn peak_keeps_the_largest_value_until_taken() {
        let peak = Peak::new();
        peak.observe(4);
        peak.observe(9);
        peak.observe(2);
        assert_eq!(peak.get(), 9);
        assert_eq!(peak.take(), 9);
        assert_eq!(peak.get(), 0);
    }
}
