//! The writer thread and its handle.

use std::fmt;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{SyncSender, TrySendError, sync_channel};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use iris_core::id::StreamKey;
use iris_core::metrics::{Histogram, Peak};
use iris_mp4::fragment::Fragment;
use tracing::warn;

use crate::fs::{ApprovedRoot, FreeSpace};
use crate::observer::{NoopObserver, StoreObserver};
use crate::policy::StoragePolicy;

mod worker;

use worker::RootWorker;

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

type WriterThreads = Vec<JoinHandle<io::Result<()>>>;

/// A cloneable handle to an ordered chain of bounded per-root writer queues.
#[derive(Clone)]
pub struct WriterHandle {
    tx: SyncSender<Command>,
    stats: Arc<WriterStats>,
    threads: Arc<Mutex<Option<WriterThreads>>>,
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
        Self::spawn_with_roots(vec![root], policy, queue_capacity, 0, observer)
    }

    /// Starts one writer thread per root, trying roots in configuration order.
    ///
    /// Each root has a bounded queue of `queue_capacity` fragments. Before every
    /// fragment append, a root consults a free-space result cached for at most
    /// one second. A root below `min_free_percent` seals and retires before
    /// writing that fragment. Forwarding drops dependent fragments until a sync
    /// fragment can start on the next root. Write or sync errors abandon files
    /// for recovery and forward subsequent fragments; an incomplete sync-start
    /// media write is retried on the next root. Complete media is never retried
    /// after a sidecar or preallocation error.
    /// Roots that forward remain skipped until the writer is restarted. The final
    /// root retries at a sync fragment after a one-second cooldown, marking a gap.
    /// All counters are shared across roots and shutdown drains the entire chain.
    ///
    /// # Errors
    ///
    /// Returns an error for an empty root list, when every root fails directory
    /// creation, or when a writer thread cannot be started.
    pub fn spawn_with_roots(
        roots: Vec<PathBuf>,
        policy: StoragePolicy,
        queue_capacity: usize,
        min_free_percent: u8,
        observer: Arc<dyn StoreObserver>,
    ) -> io::Result<Self> {
        Self::spawn_with_probe(
            roots,
            policy,
            queue_capacity,
            min_free_percent,
            observer,
            crate::fs::free_space,
        )
    }

    /// Starts writers only for roots previously admitted against their durable
    /// identities. Every root is reopened no-follow and its marker plus
    /// physical inode are rechecked immediately before threads start and
    /// before every segment open.
    /// Unlike [`WriterHandle::spawn_with_roots`], this never creates a root.
    pub fn spawn_with_approved_roots(
        roots: Vec<ApprovedRoot>,
        policy: StoragePolicy,
        queue_capacity: usize,
        min_free_percent: u8,
        observer: Arc<dyn StoreObserver>,
    ) -> io::Result<Self> {
        if roots.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "no approved recording roots",
            ));
        }
        let mut ready = Vec::with_capacity(roots.len());
        for root in roots {
            crate::fs::verify_approved_root(&root)?;
            ready.push((root.path().to_path_buf(), Some(root)));
        }
        Self::spawn_ready(
            ready,
            policy,
            queue_capacity,
            min_free_percent,
            observer,
            crate::fs::free_space,
        )
    }

    fn spawn_with_probe(
        roots: Vec<PathBuf>,
        policy: StoragePolicy,
        queue_capacity: usize,
        min_free_percent: u8,
        observer: Arc<dyn StoreObserver>,
        space_probe: fn(&Path) -> io::Result<FreeSpace>,
    ) -> io::Result<Self> {
        let mut ready = Vec::new();
        let mut last_error = io::Error::new(io::ErrorKind::InvalidInput, "no recording roots");
        for root in roots {
            match crate::fs::create_dir_all_durable(&root) {
                Ok(()) if !ready.contains(&root) => ready.push(root),
                Ok(()) => {}
                Err(error) => {
                    warn!(root = %root.display(), %error, "recording root unavailable");
                    last_error = error;
                }
            }
        }
        if ready.is_empty() {
            return Err(last_error);
        }
        Self::spawn_ready(
            ready.into_iter().map(|root| (root, None)).collect(),
            policy,
            queue_capacity,
            min_free_percent,
            observer,
            space_probe,
        )
    }

    fn spawn_ready(
        ready: Vec<(PathBuf, Option<ApprovedRoot>)>,
        policy: StoragePolicy,
        queue_capacity: usize,
        min_free_percent: u8,
        observer: Arc<dyn StoreObserver>,
        space_probe: fn(&Path) -> io::Result<FreeSpace>,
    ) -> io::Result<Self> {
        let stats = Arc::new(WriterStats::default());
        let mut threads = Vec::with_capacity(ready.len());
        let mut next = None;
        let configurations = vec![(policy, observer); ready.len()];
        for ((root, approved_root), (policy, observer)) in
            ready.into_iter().rev().zip(configurations)
        {
            let (tx, rx) = sync_channel(queue_capacity.max(1));
            let worker = RootWorker::new(
                root,
                policy,
                Arc::clone(&stats),
                observer,
                next,
                min_free_percent,
                space_probe,
                approved_root,
            );
            let thread = thread::Builder::new()
                .name("iris-writer".into())
                .spawn(move || worker.run(&rx))?;
            threads.push(thread);
            next = Some(tx);
        }
        Ok(Self {
            tx: next.expect("at least one recording root"),
            stats,
            threads: Arc::new(Mutex::new(Some(threads))),
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
        let threads = self
            .threads
            .lock()
            .map_err(|_| io::Error::other("writer thread mutex poisoned"))?
            .take();
        let Some(threads) = threads else {
            return Ok(());
        };
        // A send error means the thread already exited; join reports why.
        let _ = self.tx.send(Command::Shutdown);
        let mut first_error = None;
        for thread in threads {
            let result = thread
                .join()
                .map_err(|_| io::Error::other("writer thread panicked"))
                .and_then(|result| result);
            if let Err(error) = result {
                first_error.get_or_insert(error);
            }
        }
        first_error.map_or(Ok(()), Err)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use iris_core::id::StreamKind;
    use iris_core::time::WallUs;
    use iris_mp4::fragment::Sample;
    use iris_mp4::init::SampleEntry;
    use iris_mp4::reader::{ScanOptions, scan};
    use std::fs::{self, File};
    use std::time::Instant;

    fn key() -> StreamKey {
        StreamKey {
            camera: "cam1".parse().unwrap(),
            kind: StreamKind::Main,
        }
    }

    fn fragment(index: u64, is_sync: bool) -> Fragment {
        Fragment {
            samples: vec![Sample {
                data: Bytes::from_static(&[0, 0, 0, 2, 0x26, 1]),
                duration: 3000,
                is_sync,
            }],
            decode_time: index * 3000,
            wall: WallUs::new(1_700_000_000_000_000 + i64::try_from(index).unwrap() * 1_000_000),
            raw_rtp: 0,
            new_run: false,
            sample_entry: Arc::new(SampleEntry::new(
                Bytes::from_static(b"\0\0\0\x10hvc1\0\0\0\0\0\0\0\0"),
                "hvc1".into(),
                16,
                16,
            )),
        }
    }

    fn marker_space(root: &Path) -> io::Result<FreeSpace> {
        fs::metadata(root)?;
        Ok(FreeSpace {
            total: 100,
            available: if root.join("space-low").exists() {
                0
            } else {
                100
            },
        })
    }

    fn wait_for(mut ready: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(30);
        while !ready() {
            assert!(Instant::now() < deadline, "writer did not make progress");
            thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn an_approved_writer_rejects_a_root_replaced_before_its_first_open() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("root");
        let displaced = dir.path().join("displaced");
        let approved =
            crate::fs::admit_recording_root(&root, crate::fs::RootExpectation::Fresh).unwrap();
        let writer = WriterHandle::spawn_with_approved_roots(
            vec![approved],
            StoragePolicy {
                preallocate_bytes: 0,
                ..StoragePolicy::default()
            },
            8,
            0,
            Arc::new(NoopObserver),
        )
        .unwrap();

        fs::rename(&root, &displaced).unwrap();
        fs::create_dir(&root).unwrap();
        fs::copy(
            displaced.join(crate::fs::ROOT_IDENTITY_MARKER),
            root.join(crate::fs::ROOT_IDENTITY_MARKER),
        )
        .unwrap();

        writer.try_send(key(), fragment(0, true)).unwrap();
        writer.shutdown().unwrap();
        assert_eq!(writer.stats().write_errors(), 1);
        assert!(!root.join("cam1").exists());
        assert!(!displaced.join("cam1").exists());
    }

    #[test]
    fn a_final_root_recovers_after_low_space_clears() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("space-low"), b"").unwrap();
        let writer = WriterHandle::spawn_with_probe(
            vec![dir.path().to_owned()],
            StoragePolicy {
                preallocate_bytes: 0,
                ..StoragePolicy::default()
            },
            8,
            5,
            Arc::new(NoopObserver),
            marker_space,
        )
        .unwrap();
        writer.try_send(key(), fragment(0, true)).unwrap();
        wait_for(|| writer.stats().dropped() == 1);
        fs::remove_file(dir.path().join("space-low")).unwrap();
        thread::sleep(Duration::from_millis(1100));
        writer.try_send(key(), fragment(1, true)).unwrap();
        writer.shutdown().unwrap();
        assert_eq!(writer.stats().fragments(), 1);
    }

    #[test]
    fn the_first_root_with_space_receives_fragments_in_order() {
        let dir = tempfile::tempdir().unwrap();
        let first = dir.path().join("first");
        let second = dir.path().join("second");
        fs::create_dir_all(&first).unwrap();
        fs::write(first.join("space-low"), b"").unwrap();
        let writer = WriterHandle::spawn_with_probe(
            vec![first.clone(), second.clone()],
            StoragePolicy {
                preallocate_bytes: 0,
                ..StoragePolicy::default()
            },
            8,
            5,
            Arc::new(NoopObserver),
            marker_space,
        )
        .unwrap();
        for index in 0..3 {
            writer.try_send(key(), fragment(index, index != 1)).unwrap();
        }
        writer.shutdown().unwrap();
        assert!(!first.join("cam1").exists());
        let path = crate::layout::segment_path(&second, &key(), fragment(0, true).wall);
        let report = scan(
            File::open(path).unwrap(),
            ScanOptions {
                verify_crc: true,
                collect_samples: false,
            },
        )
        .unwrap();
        assert_eq!(report.stop, None);
        assert_eq!(
            report
                .fragments
                .iter()
                .map(|f| f.decode_time)
                .collect::<Vec<_>>(),
            vec![0, 3000, 6000]
        );
        assert_eq!(writer.stats().fragments(), 3);
        assert_eq!(writer.stats().dropped(), 0);
        assert_eq!(writer.stats().queued(), 0);
        assert_eq!(writer.stats().queued_bytes(), 0);
    }

    #[test]
    fn a_failed_write_is_retried_on_the_next_root_before_queued_continuations() {
        let dir = tempfile::tempdir().unwrap();
        let first = dir.path().join("first");
        let second = dir.path().join("second");
        fs::create_dir_all(&first).unwrap();
        fs::write(first.join("cam1"), b"blocks camera directory creation").unwrap();
        let writer = WriterHandle::spawn_with_roots(
            vec![first, second.clone()],
            StoragePolicy {
                preallocate_bytes: 0,
                ..StoragePolicy::default()
            },
            8,
            0,
            Arc::new(NoopObserver),
        )
        .unwrap();
        for index in 0..3 {
            writer.try_send(key(), fragment(index, index != 1)).unwrap();
        }
        writer.shutdown().unwrap();
        let path = crate::layout::segment_path(&second, &key(), fragment(0, true).wall);
        let report = scan(
            File::open(path).unwrap(),
            ScanOptions {
                verify_crc: true,
                collect_samples: false,
            },
        )
        .unwrap();
        assert_eq!(report.stop, None);
        assert_eq!(
            report
                .fragments
                .iter()
                .map(|f| f.decode_time)
                .collect::<Vec<_>>(),
            vec![0, 3000, 6000]
        );
        assert_eq!(writer.stats().write_errors(), 1);
        assert_eq!(writer.stats().fragments(), 3);
        assert_eq!(writer.stats().dropped(), 0);
        assert_eq!(writer.stats().queued(), 0);
        assert_eq!(writer.stats().queued_bytes(), 0);
    }

    #[test]
    fn a_final_root_retries_transient_write_errors_only_at_sync_after_cooldown() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("cam1"), b"blocks camera directory creation").unwrap();
        let writer = WriterHandle::spawn(
            dir.path().to_owned(),
            StoragePolicy {
                preallocate_bytes: 0,
                ..StoragePolicy::default()
            },
            8,
        )
        .unwrap();
        writer.try_send(key(), fragment(0, true)).unwrap();
        wait_for(|| writer.stats().write_errors() == 1);
        fs::remove_file(dir.path().join("cam1")).unwrap();
        thread::sleep(Duration::from_millis(1100));
        writer.try_send(key(), fragment(1, false)).unwrap();
        writer.try_send(key(), fragment(2, true)).unwrap();
        writer.shutdown().unwrap();
        let path = crate::layout::segment_path(dir.path(), &key(), fragment(2, true).wall);
        let report = scan(
            File::open(path).unwrap(),
            ScanOptions {
                verify_crc: true,
                collect_samples: false,
            },
        )
        .unwrap();
        assert_eq!(report.fragments.len(), 1);
        assert!(report.fragments[0].starts_with_sync);
        assert_eq!(report.fragments[0].decode_time, 6000);
        assert_eq!(writer.stats().dropped(), 2);
        assert_eq!(writer.stats().queued(), 0);
    }

    #[test]
    fn an_uncreatable_root_does_not_prevent_recording_on_a_later_root() {
        let dir = tempfile::tempdir().unwrap();
        let blocked = dir.path().join("blocked");
        fs::write(&blocked, b"not a directory").unwrap();
        let good = dir.path().join("good");
        let writer = WriterHandle::spawn_with_roots(
            vec![blocked, good.clone()],
            StoragePolicy {
                preallocate_bytes: 0,
                ..StoragePolicy::default()
            },
            8,
            0,
            Arc::new(NoopObserver),
        )
        .unwrap();
        writer.try_send(key(), fragment(0, true)).unwrap();
        writer.shutdown().unwrap();
        assert!(crate::layout::segment_path(&good, &key(), fragment(0, true).wall).exists());
        assert_eq!(writer.stats().fragments(), 1);
    }

    #[test]
    fn pressure_switches_every_stream_at_its_next_sync_and_keeps_the_upstream_retired() {
        let dir = tempfile::tempdir().unwrap();
        let first = dir.path().join("first");
        let second = dir.path().join("second");
        let other = StreamKey {
            camera: "cam2".parse().unwrap(),
            kind: StreamKind::Main,
        };
        let writer = WriterHandle::spawn_with_probe(
            vec![first.clone(), second.clone()],
            StoragePolicy {
                preallocate_bytes: 0,
                ..StoragePolicy::default()
            },
            8,
            5,
            Arc::new(NoopObserver),
            marker_space,
        )
        .unwrap();
        writer.try_send(key(), fragment(0, true)).unwrap();
        writer.try_send(other.clone(), fragment(0, true)).unwrap();
        wait_for(|| writer.stats().fragments() == 2);
        fs::write(first.join("space-low"), b"").unwrap();
        thread::sleep(Duration::from_millis(1100));
        writer.try_send(key(), fragment(1, true)).unwrap();
        writer.try_send(other.clone(), fragment(1, false)).unwrap();
        writer.try_send(other.clone(), fragment(2, true)).unwrap();
        wait_for(|| writer.stats().fragments() == 4);
        fs::remove_file(first.join("space-low")).unwrap();
        writer.try_send(key(), fragment(3, true)).unwrap();
        writer.shutdown().unwrap();
        let old = crate::layout::segment_path(&first, &key(), fragment(0, true).wall);
        let fallback = crate::layout::segment_path(&second, &other, fragment(2, true).wall);
        let options = ScanOptions {
            verify_crc: true,
            collect_samples: false,
        };
        assert_eq!(
            scan(File::open(old).unwrap(), options)
                .unwrap()
                .fragments
                .len(),
            1
        );
        let report = scan(File::open(fallback).unwrap(), options).unwrap();
        assert_eq!(report.fragments.len(), 1);
        assert!(report.fragments[0].starts_with_sync);
        assert_eq!(writer.stats().fragments(), 5);
        assert_eq!(writer.stats().dropped(), 1);
        assert_eq!(writer.stats().queued(), 0);
        assert_eq!(writer.stats().queued_bytes(), 0);
    }

    #[test]
    fn a_non_sync_continuation_cannot_write_after_the_space_cache_expires() {
        let dir = tempfile::tempdir().unwrap();
        let first = dir.path().join("first");
        let second = dir.path().join("second");
        let writer = WriterHandle::spawn_with_probe(
            vec![first.clone(), second.clone()],
            StoragePolicy {
                preallocate_bytes: 0,
                ..StoragePolicy::default()
            },
            8,
            5,
            Arc::new(NoopObserver),
            marker_space,
        )
        .unwrap();
        writer.try_send(key(), fragment(0, true)).unwrap();
        wait_for(|| writer.stats().fragments() == 1);
        fs::write(first.join("space-low"), b"").unwrap();
        thread::sleep(Duration::from_millis(1_100));

        writer.try_send(key(), fragment(1, false)).unwrap();
        writer.try_send(key(), fragment(2, true)).unwrap();
        writer.shutdown().unwrap();

        let options = ScanOptions {
            verify_crc: true,
            collect_samples: false,
        };
        let old = crate::layout::segment_path(&first, &key(), fragment(0, true).wall);
        let old_report = scan(File::open(old).unwrap(), options).unwrap();
        assert_eq!(old_report.fragments.len(), 1);
        let fallback = crate::layout::segment_path(&second, &key(), fragment(2, true).wall);
        let fallback_report = scan(File::open(fallback).unwrap(), options).unwrap();
        assert_eq!(fallback_report.fragments.len(), 1);
        assert_eq!(fallback_report.fragments[0].decode_time, 6_000);
        assert_eq!(writer.stats().dropped(), 1);
    }

    #[test]
    fn sync_phases_spread_streams_over_the_interval() {
        let interval = Duration::from_secs(5);
        assert_eq!(sync_phase(0, interval), Duration::ZERO);
        assert_eq!(sync_phase(1, interval), Duration::from_micros(312_500));
        assert_eq!(sync_phase(8, interval), Duration::from_millis(2_500));
        assert_eq!(sync_phase(16, interval), Duration::ZERO);
    }
}
