//! The writer thread and its handle.

use std::collections::HashMap;
use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, SyncSender, TrySendError, sync_channel};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use iris_core::id::StreamKey;
use iris_core::metrics::{Histogram, Peak};
use iris_mp4::fragment::Fragment;
use tracing::{error, warn};

use crate::observer::{NoopObserver, StoreObserver};
use crate::policy::StoragePolicy;
use crate::stream::{StreamWriter, Written};

/// A write or sync that takes longer than this is counted and logged as slow.
/// Streams are spread over this many phases of the sync interval, in order of
/// first appearance, so that their `fdatasync`s do not land on the disk together.
const SYNC_SLOTS: usize = 16;

/// The sync phase of the stream in `slot`: a fraction of `interval`.
fn sync_phase(slot: usize, interval: Duration) -> Duration {
    interval * u32::try_from(slot % SYNC_SLOTS).unwrap_or(0)
        / u32::try_from(SYNC_SLOTS).unwrap_or(1)
}

const SLOW_IO: Duration = Duration::from_secs(1);

/// Counters maintained by the writer thread and its handle, readable from any
/// thread.
///
/// Byte counts are bytes written to disk, headers and trailers included;
/// latencies are per write or per `fdatasync`; peaks reset when taken. The
/// daemon's `writer` statistics line is built from these accessors.
#[derive(Debug, Default)]
pub struct WriterStats {
    fragments: AtomicU64,
    bytes: AtomicU64,
    syncs: AtomicU64,
    dropped: AtomicU64,
    files_opened: AtomicU64,
    write_errors: AtomicU64,
    write_latency: Histogram,
    sync_latency: Histogram,
    queued: AtomicU64,
    queue_peak: Peak,
    queued_bytes: AtomicU64,
    queue_bytes_peak: Peak,
    slow_writes: AtomicU64,
}

impl WriterStats {
    /// Fragments written to disk.
    pub fn fragments(&self) -> u64 {
        self.fragments.load(Ordering::Relaxed)
    }

    /// Bytes written to disk, trailers and headers included.
    pub fn bytes(&self) -> u64 {
        self.bytes.load(Ordering::Relaxed)
    }

    /// `fdatasync` calls on data files.
    pub fn syncs(&self) -> u64 {
        self.syncs.load(Ordering::Relaxed)
    }

    /// Fragments dropped because the queue was full or no file could take them.
    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }

    /// Recording files created.
    pub fn files_opened(&self) -> u64 {
        self.files_opened.load(Ordering::Relaxed)
    }

    /// I/O errors; each one abandons the affected file until the next sync sample.
    pub fn write_errors(&self) -> u64 {
        self.write_errors.load(Ordering::Relaxed)
    }

    /// Time spent writing one fragment, headers and trailer included.
    pub fn write_latency(&self) -> &Histogram {
        &self.write_latency
    }

    /// Time spent in one `fdatasync` of a data file and its index.
    pub fn sync_latency(&self) -> &Histogram {
        &self.sync_latency
    }

    /// Fragments accepted by the queue and not yet taken by the writer thread.
    pub fn queued(&self) -> u64 {
        self.queued.load(Ordering::Relaxed)
    }

    /// The deepest the queue has been since the peak was last taken.
    pub fn queue_peak(&self) -> &Peak {
        &self.queue_peak
    }

    /// Payload bytes of the queued fragments.
    pub fn queued_bytes(&self) -> u64 {
        self.queued_bytes.load(Ordering::Relaxed)
    }

    /// The most payload bytes the queue has held since the peak was last taken.
    pub fn queue_bytes_peak(&self) -> &Peak {
        &self.queue_bytes_peak
    }

    /// Writes or syncs that took longer than one second.
    pub fn slow_writes(&self) -> u64 {
        self.slow_writes.load(Ordering::Relaxed)
    }

    /// Accounts for a fragment entering the queue.
    fn enqueue(&self, bytes: u64) {
        let depth = self.queued.fetch_add(1, Ordering::Relaxed) + 1;
        self.queue_peak.observe(depth);
        let queued_bytes = self.queued_bytes.fetch_add(bytes, Ordering::Relaxed) + bytes;
        self.queue_bytes_peak.observe(queued_bytes);
    }

    /// Accounts for a fragment leaving the queue (or never entering it). Every
    /// call pairs with an earlier [`WriterStats::enqueue`] for the same fragment,
    /// so the counters never go below zero.
    fn dequeue(&self, bytes: u64) {
        self.queued.fetch_sub(1, Ordering::Relaxed);
        self.queued_bytes.fetch_sub(bytes, Ordering::Relaxed);
    }
}

/// Returned by [`WriterHandle::try_send`] when the bounded queue is full.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct QueueFull;

impl fmt::Display for QueueFull {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("writer queue full")
    }
}

impl std::error::Error for QueueFull {}

enum Command {
    Fragment { key: StreamKey, fragment: Fragment },
    Shutdown,
}

/// A cloneable handle to the writer thread.
#[derive(Clone)]
pub struct WriterHandle {
    tx: SyncSender<Command>,
    stats: Arc<WriterStats>,
    thread: Arc<Mutex<Option<JoinHandle<io::Result<()>>>>>,
}

impl fmt::Debug for WriterHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WriterHandle")
            .field("stats", &self.stats)
            .finish_non_exhaustive()
    }
}

impl WriterHandle {
    /// Creates `root` and starts the writer thread with a queue of
    /// `queue_capacity` fragments and no observer.
    pub fn spawn(root: PathBuf, policy: StoragePolicy, queue_capacity: usize) -> io::Result<Self> {
        Self::spawn_with_observer(root, policy, queue_capacity, Arc::new(NoopObserver))
    }

    /// Like [`WriterHandle::spawn`], notifying `observer` about every file
    /// opened, synced, and sealed. Notifications run on the writer thread.
    pub fn spawn_with_observer(
        root: PathBuf,
        policy: StoragePolicy,
        queue_capacity: usize,
        observer: Arc<dyn StoreObserver>,
    ) -> io::Result<Self> {
        fs::create_dir_all(&root)?;
        let (tx, rx) = sync_channel(queue_capacity.max(1));
        let stats = Arc::new(WriterStats::default());
        let thread_stats = Arc::clone(&stats);
        let handle = thread::Builder::new()
            .name("iris-writer".into())
            .spawn(move || run(&rx, &root, &policy, &thread_stats, &observer))?;
        Ok(Self {
            tx,
            stats,
            thread: Arc::new(Mutex::new(Some(handle))),
        })
    }

    /// Queues a fragment without blocking. A full (or closed) queue drops the
    /// fragment, counts it, and returns [`QueueFull`].
    pub fn try_send(&self, key: StreamKey, fragment: Fragment) -> Result<(), QueueFull> {
        // Counted before the send so the writer thread's matching decrement can
        // never run first.
        let bytes = fragment.payload_len();
        self.stats.enqueue(bytes);
        match self.tx.try_send(Command::Fragment { key, fragment }) {
            Ok(()) => Ok(()),
            Err(TrySendError::Full(_) | TrySendError::Disconnected(_)) => {
                self.stats.dequeue(bytes);
                self.stats.dropped.fetch_add(1, Ordering::Relaxed);
                Err(QueueFull)
            }
        }
    }

    /// Shared counters.
    pub fn stats(&self) -> Arc<WriterStats> {
        Arc::clone(&self.stats)
    }

    /// Drains the queue, seals every open file, and joins the thread. A
    /// second call returns `Ok(())` without doing anything.
    pub fn shutdown(&self) -> io::Result<()> {
        let handle = self
            .thread
            .lock()
            .map_err(|_| io::Error::other("writer thread mutex poisoned"))?
            .take();
        let Some(handle) = handle else {
            return Ok(());
        };
        // A send error means the thread already exited; join reports why.
        let _ = self.tx.send(Command::Shutdown);
        handle
            .join()
            .map_err(|_| io::Error::other("writer thread panicked"))?
    }
}

fn run(
    rx: &Receiver<Command>,
    root: &Path,
    policy: &StoragePolicy,
    stats: &WriterStats,
    observer: &Arc<dyn StoreObserver>,
) -> io::Result<()> {
    let mut streams: HashMap<StreamKey, StreamWriter> = HashMap::new();
    loop {
        match rx.recv_timeout(Duration::from_millis(500)) {
            Ok(Command::Fragment { key, fragment }) => {
                stats.dequeue(fragment.payload_len());
                let slot = streams.len();
                let writer = streams.entry(key.clone()).or_insert_with(|| {
                    StreamWriter::new(
                        key.clone(),
                        root.to_path_buf(),
                        policy.clone(),
                        Arc::clone(observer),
                        sync_phase(slot, policy.sync_interval),
                    )
                });
                let before = writer.files_opened();
                let started = Instant::now();
                let written = writer.write(&fragment);
                let elapsed = started.elapsed();
                stats.write_latency.record(elapsed);
                if elapsed > SLOW_IO {
                    stats.slow_writes.fetch_add(1, Ordering::Relaxed);
                    warn!(stream = %key, ms = elapsed.as_millis(), "slow write");
                }
                match written {
                    Ok(Written::Record(record)) => {
                        stats.fragments.fetch_add(1, Ordering::Relaxed);
                        stats
                            .bytes
                            .fetch_add(u64::from(record.len), Ordering::Relaxed);
                    }
                    Ok(Written::Dropped) => {
                        stats.dropped.fetch_add(1, Ordering::Relaxed);
                    }
                    Err(e) => {
                        error!(error = %e, "write failed; abandoning file");
                        stats.write_errors.fetch_add(1, Ordering::Relaxed);
                        writer.abandon();
                    }
                }
                stats
                    .files_opened
                    .fetch_add(writer.files_opened() - before, Ordering::Relaxed);
            }
            Ok(Command::Shutdown) | Err(RecvTimeoutError::Disconnected) => break,
            Err(RecvTimeoutError::Timeout) => {}
        }
        let now = Instant::now();
        for (key, writer) in &mut streams {
            let started = Instant::now();
            match writer.maybe_sync(now, policy.sync_interval) {
                Ok(true) => {
                    let elapsed = started.elapsed();
                    stats.syncs.fetch_add(1, Ordering::Relaxed);
                    stats.sync_latency.record(elapsed);
                    if elapsed > SLOW_IO {
                        stats.slow_writes.fetch_add(1, Ordering::Relaxed);
                        warn!(stream = %key, ms = elapsed.as_millis(), "slow sync");
                    }
                }
                Ok(false) => {}
                Err(e) => {
                    error!(error = %e, "sync failed; abandoning file");
                    stats.write_errors.fetch_add(1, Ordering::Relaxed);
                    writer.abandon();
                }
            }
        }
    }
    let mut first_error = None;
    for writer in streams.values_mut() {
        if let Err(e) = writer.close() {
            error!(error = %e, "seal failed");
            first_error.get_or_insert(e);
        }
    }
    first_error.map_or(Ok(()), Err)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sync_phases_spread_streams_over_the_interval() {
        let interval = Duration::from_secs(5);
        assert_eq!(sync_phase(0, interval), Duration::ZERO);
        assert_eq!(sync_phase(1, interval), Duration::from_micros(312_500));
        assert_eq!(sync_phase(8, interval), Duration::from_millis(2_500));
        assert_eq!(sync_phase(16, interval), Duration::ZERO);
    }
}
