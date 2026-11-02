//! Latency and batch counters of the catalog thread.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use iris_core::metrics::{Histogram, Peak};

/// Counters the catalog thread maintains, readable from any thread.
#[derive(Debug, Default)]
pub struct CatalogMetrics {
    transaction: Histogram,
    query: Histogram,
    wait: Histogram,
    event_commit: Histogram,
    batches: AtomicU64,
    changes: AtomicU64,
    failures: AtomicU64,
    batch_peak: Peak,
}

impl CatalogMetrics {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Time spent committing one batch of changes.
    #[must_use]
    pub fn transaction(&self) -> &Histogram {
        &self.transaction
    }

    /// Time spent running one query on the catalog thread.
    #[must_use]
    pub fn query(&self) -> &Histogram {
        &self.query
    }

    /// Time a query waited in the channel before the thread picked it up.
    #[must_use]
    pub fn wait(&self) -> &Histogram {
        &self.wait
    }

    /// Delay from an event's NVR time to the commit that stored it.
    #[must_use]
    pub fn event_commit(&self) -> &Histogram {
        &self.event_commit
    }

    /// Batches committed.
    #[must_use]
    pub fn batches(&self) -> u64 {
        self.batches.load(Ordering::Relaxed)
    }

    /// Changes committed.
    #[must_use]
    pub fn changes(&self) -> u64 {
        self.changes.load(Ordering::Relaxed)
    }

    /// Batches whose transaction failed.
    #[must_use]
    pub fn failures(&self) -> u64 {
        self.failures.load(Ordering::Relaxed)
    }

    /// Largest batch since the peak was last taken.
    #[must_use]
    pub fn batch_peak(&self) -> &Peak {
        &self.batch_peak
    }

    /// Records the outcome of one batch.
    pub(crate) fn record_batch(&self, changes: usize, elapsed: Duration, committed: bool) {
        self.transaction.record(elapsed);
        self.batch_peak.observe(changes as u64);
        if committed {
            self.batches.fetch_add(1, Ordering::Relaxed);
            self.changes.fetch_add(changes as u64, Ordering::Relaxed);
        } else {
            self.failures.fetch_add(1, Ordering::Relaxed);
        }
    }
}
