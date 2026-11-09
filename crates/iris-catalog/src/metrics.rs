//! Latency and batch counters of the catalog thread.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use iris_core::metrics::{Histogram, Peak};

/// Counters maintained by the catalog thread and its producers, readable
/// from any thread.
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
    queue_depth: AtomicU64,
    queue_bytes: AtomicU64,
    queue_peak: Peak,
    queue_bytes_peak: Peak,
    coalesced: AtomicU64,
    rejected: AtomicU64,
    rejected_events: AtomicU64,
    rejected_progress: AtomicU64,
    rejected_queries: AtomicU64,
    rejected_lifecycle: AtomicU64,
    reconciliation_required: AtomicBool,
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

    /// Time a query waited in the command queue before the thread picked it up.
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

    /// Commands admitted and still outstanding on the catalog thread.
    #[must_use]
    pub fn queue_depth(&self) -> u64 {
        self.queue_depth.load(Ordering::Relaxed)
    }

    /// Estimated owned bytes admitted and still outstanding.
    #[must_use]
    pub fn queue_bytes(&self) -> u64 {
        self.queue_bytes.load(Ordering::Relaxed)
    }

    /// Largest outstanding command count since the peak was last taken.
    #[must_use]
    pub fn queue_peak(&self) -> &Peak {
        &self.queue_peak
    }

    /// Largest outstanding owned-byte estimate since the peak was last taken.
    #[must_use]
    pub fn queue_bytes_peak(&self) -> &Peak {
        &self.queue_bytes_peak
    }

    /// Progress notifications replaced by a newer queued value for the same path.
    #[must_use]
    pub fn coalesced(&self) -> u64 {
        self.coalesced.load(Ordering::Relaxed)
    }

    /// Commands rejected by bounded admission: events, progress reports,
    /// queries, and lifecycle changes. Events omitted at the state-database
    /// capacity count in [`CatalogMetrics::rejected_events`] only.
    #[must_use]
    pub fn rejected(&self) -> u64 {
        self.rejected.load(Ordering::Relaxed)
    }

    /// Event notifications rejected by bounded admission or omitted at the
    /// state-database capacity.
    #[must_use]
    pub fn rejected_events(&self) -> u64 {
        self.rejected_events.load(Ordering::Relaxed)
    }

    /// Progress notifications rejected after coalescing was unavailable.
    #[must_use]
    pub fn rejected_progress(&self) -> u64 {
        self.rejected_progress.load(Ordering::Relaxed)
    }

    /// Queries rejected with a bounded overload error.
    #[must_use]
    pub fn rejected_queries(&self) -> u64 {
        self.rejected_queries.load(Ordering::Relaxed)
    }

    /// Lifecycle notifications that exhausted even reserved admission capacity.
    #[must_use]
    pub fn rejected_lifecycle(&self) -> u64 {
        self.rejected_lifecycle.load(Ordering::Relaxed)
    }

    /// Whether startup filesystem reconciliation is required because reserved
    /// lifecycle admission was exhausted.
    #[must_use]
    pub fn reconciliation_required(&self) -> bool {
        self.reconciliation_required.load(Ordering::Relaxed)
    }

    /// Records a committed batch of `changes` changes.
    pub(crate) fn record_committed_batch(&self, changes: usize, elapsed: Duration) {
        self.transaction.record(elapsed);
        self.batch_peak.observe(changes as u64);
        self.batches.fetch_add(1, Ordering::Relaxed);
        self.changes.fetch_add(changes as u64, Ordering::Relaxed);
    }

    /// Records a batch of `changes` changes whose transaction failed.
    pub(crate) fn record_failed_batch(&self, changes: usize, elapsed: Duration) {
        self.transaction.record(elapsed);
        self.batch_peak.observe(changes as u64);
        self.failures.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn set_queue_usage(&self, depth: usize, bytes: usize) {
        let depth = u64::try_from(depth).unwrap_or(u64::MAX);
        let bytes = u64::try_from(bytes).unwrap_or(u64::MAX);
        self.queue_depth.store(depth, Ordering::Relaxed);
        self.queue_bytes.store(bytes, Ordering::Relaxed);
        self.queue_peak.observe(depth);
        self.queue_bytes_peak.observe(bytes);
    }

    pub(crate) fn record_coalesced(&self) {
        self.coalesced.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn reject_event(&self) {
        self.rejected.fetch_add(1, Ordering::Relaxed);
        self.rejected_events.fetch_add(1, Ordering::Relaxed);
    }

    /// Counts events that were admitted and then omitted at the
    /// state-database capacity; they are not admission rejections.
    pub(crate) fn omit_events(&self, count: usize) {
        let count = u64::try_from(count).unwrap_or(u64::MAX);
        self.rejected_events.fetch_add(count, Ordering::Relaxed);
    }

    pub(crate) fn reject_progress(&self) {
        self.rejected.fetch_add(1, Ordering::Relaxed);
        self.rejected_progress.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn reject_query(&self) {
        self.rejected.fetch_add(1, Ordering::Relaxed);
        self.rejected_queries.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn reject_lifecycle(&self) {
        self.rejected.fetch_add(1, Ordering::Relaxed);
        self.rejected_lifecycle.fetch_add(1, Ordering::Relaxed);
        self.reconciliation_required.store(true, Ordering::Relaxed);
    }
}
