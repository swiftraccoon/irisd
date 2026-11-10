//! One root's disk work and ordered forwarding to its fallback root.

use std::collections::{HashMap, HashSet};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::sync::mpsc::{Receiver, RecvTimeoutError, SyncSender, TrySendError};
use std::time::{Duration, Instant};

use iris_core::id::StreamKey;
use iris_mp4::fragment::Fragment;
use tracing::{error, warn};

use crate::fs::{ApprovedRoot, FreeSpace};
use crate::observer::StoreObserver;
use crate::policy::StoragePolicy;
use crate::stream::{StreamWriter, Written};
use crate::writer::{Command, SLOW_IO, WriterStats, sync_phase};

/// One root's disk worker: the thread body behind a [`crate::WriterHandle`]
/// queue.
pub(crate) struct RootWorker {
    root: PathBuf,
    policy: StoragePolicy,
    stats: Arc<WriterStats>,
    observer: Arc<dyn StoreObserver>,
    next: Option<SyncSender<Command>>,
    min_free_percent: u8,
    space_probe: fn(&Path) -> io::Result<FreeSpace>,
    approved_root: Option<ApprovedRoot>,
    space_checked: Option<Instant>,
    available: bool,
    retry_after: Option<Instant>,
    streams: HashMap<StreamKey, StreamWriter>,
    forward_losses: HashSet<StreamKey>,
}

impl RootWorker {
    pub(crate) fn new(
        root: PathBuf,
        policy: StoragePolicy,
        stats: Arc<WriterStats>,
        observer: Arc<dyn StoreObserver>,
        next: Option<SyncSender<Command>>,
        min_free_percent: u8,
        space_probe: fn(&Path) -> io::Result<FreeSpace>,
        approved_root: Option<ApprovedRoot>,
    ) -> Self {
        Self {
            root,
            policy,
            stats,
            observer,
            next,
            min_free_percent,
            space_probe,
            approved_root,
            space_checked: None,
            available: true,
            retry_after: None,
            streams: HashMap::new(),
            forward_losses: HashSet::new(),
        }
    }

    pub(crate) fn run(mut self, rx: &Receiver<Command>) -> io::Result<()> {
        loop {
            match rx.recv_timeout(Duration::from_millis(500)) {
                Ok(Command::Fragment { key, fragment }) => {
                    self.stats.dequeue(fragment.payload_len());
                    self.write(key, fragment);
                }
                Ok(Command::Shutdown) | Err(RecvTimeoutError::Disconnected) => break,
                Err(RecvTimeoutError::Timeout) => {}
            }
            if self.available
                && let Err(error) = self.sync()
            {
                self.stats.write_errors.fetch_add(1, Ordering::Relaxed);
                error!(
                    root = %self.root.display(),
                    %error,
                    outcome = self.failure_outcome(),
                    "post-write maintenance or sync failed"
                );
                self.abandon();
            }
        }
        let result = self.close();
        // Every forwarded fragment precedes this marker in the next root's queue.
        if let Some(next) = &self.next {
            let _ = next.send(Command::Shutdown);
        }
        result
    }

    fn write(&mut self, key: StreamKey, fragment: Fragment) {
        if !self.available
            && fragment.starts_with_sync()
            && self
                .retry_after
                .is_some_and(|deadline| Instant::now() >= deadline)
        {
            self.available = true;
            self.retry_after = None;
            self.space_checked = None;
        }
        if self.available {
            match self.has_space() {
                Ok(true) => {}
                Ok(false) => {
                    warn!(
                        root = %self.root.display(),
                        min_free_percent = self.min_free_percent,
                        outcome = self.failure_outcome(),
                        "free-space floor reached"
                    );
                    if let Err(error) = self.close() {
                        self.stats.write_errors.fetch_add(1, Ordering::Relaxed);
                        error!(
                            root = %self.root.display(),
                            %error,
                            "seal failed during root failover"
                        );
                    }
                    for writer in self.streams.values_mut() {
                        writer.mark_discontinuity();
                    }
                    self.retire();
                }
                Err(error) => {
                    self.stats.write_errors.fetch_add(1, Ordering::Relaxed);
                    error!(
                        root = %self.root.display(),
                        %error,
                        outcome = self.failure_outcome(),
                        "space check failed"
                    );
                    self.abandon();
                }
            }
        }
        if !self.available {
            self.forward(key, fragment);
            return;
        }
        if let Err(error) = self.append(&key, &fragment) {
            self.stats.write_errors.fetch_add(1, Ordering::Relaxed);
            error!(
                root = %self.root.display(),
                stream = %key,
                %error,
                outcome = self.failure_outcome(),
                "write failed"
            );
            self.abandon();
            self.forward(key, fragment);
        }
    }

    /// What a failure on this root leads to: forwarding to the fallback root,
    /// or, on the final root, dropping until a sync-start retry.
    fn failure_outcome(&self) -> &'static str {
        if self.next.is_some() {
            "switching to the fallback root"
        } else {
            "dropping until a sync-start retry"
        }
    }

    fn has_space(&mut self) -> io::Result<bool> {
        if self
            .space_checked
            .is_some_and(|time| time.elapsed() < Duration::from_secs(1))
            || (self.min_free_percent == 0 && self.approved_root.is_none())
        {
            return Ok(true);
        }
        let probe = self.approved_root.as_ref().map_or_else(
            || (self.space_probe)(&self.root),
            crate::fs::free_space_approved,
        );
        self.space_checked = Some(Instant::now());
        let space = match probe {
            Ok(space) => space,
            Err(error) if crate::fs::is_transient_resource_error(&error) => {
                // The process, not the root, ran out of a resource. Open files
                // stay in use; the probe runs again after the cadence.
                warn!(
                    root = %self.root.display(),
                    %error,
                    "free-space probe failed transiently; keeping open files, retrying in a second"
                );
                return Ok(true);
            }
            Err(error) => {
                self.space_checked = None;
                return Err(error);
            }
        };
        Ok(self.min_free_percent == 0
            || (space.total > 0
                && u128::from(space.available) * 100
                    >= u128::from(space.total) * u128::from(self.min_free_percent)))
    }

    fn append(&mut self, key: &StreamKey, fragment: &Fragment) -> io::Result<()> {
        let slot = self.streams.len();
        let writer = self.streams.entry(key.clone()).or_insert_with(|| {
            StreamWriter::new(
                key.clone(),
                self.root.clone(),
                self.policy,
                Arc::clone(&self.observer),
                sync_phase(slot, self.policy.sync_interval),
                self.approved_root.clone(),
            )
        });
        let before = writer.files_opened();
        let started = Instant::now();
        let written = writer.write(fragment);
        let elapsed = started.elapsed();
        self.stats.write_latency.record(elapsed);
        if elapsed > SLOW_IO {
            self.stats.slow_writes.fetch_add(1, Ordering::Relaxed);
            warn!(stream = %key, ms = elapsed.as_millis(), "slow write");
        }
        self.stats
            .files_opened
            .fetch_add(writer.files_opened() - before, Ordering::Relaxed);
        match written? {
            Written::Record(record) => {
                self.stats.fragments.fetch_add(1, Ordering::Relaxed);
                self.stats
                    .bytes
                    .fetch_add(u64::from(record.len), Ordering::Relaxed);
            }
            Written::Dropped => {
                self.stats.dropped.fetch_add(1, Ordering::Relaxed);
            }
        }
        Ok(())
    }

    fn sync(&mut self) -> io::Result<()> {
        let now = Instant::now();
        for (key, writer) in &mut self.streams {
            let started = Instant::now();
            if writer.maybe_sync(now, self.policy.sync_interval)? {
                let elapsed = started.elapsed();
                self.stats.syncs.fetch_add(1, Ordering::Relaxed);
                self.stats.sync_latency.record(elapsed);
                if elapsed > SLOW_IO {
                    self.stats.slow_writes.fetch_add(1, Ordering::Relaxed);
                    warn!(stream = %key, ms = elapsed.as_millis(), "slow sync");
                }
            }
        }
        Ok(())
    }

    fn forward(&mut self, key: StreamKey, mut fragment: Fragment) {
        let recovering_loss = self.forward_losses.contains(&key);
        if recovering_loss {
            if !fragment.starts_with_sync() {
                self.stats.dropped.fetch_add(1, Ordering::Relaxed);
                return;
            }
            fragment.new_run = true;
        }
        let bytes = fragment.payload_len();
        if let Some(next) = &self.next {
            let (depth, queued_bytes) = self.stats.enqueue(bytes);
            match next.try_send(Command::Fragment {
                key: key.clone(),
                fragment,
            }) {
                Ok(()) => {
                    self.stats.observe_queue(depth, queued_bytes);
                    if recovering_loss {
                        self.forward_losses.remove(&key);
                    }
                    return;
                }
                Err(TrySendError::Full(_) | TrySendError::Disconnected(_)) => {
                    self.stats.dequeue(bytes);
                    self.forward_losses.insert(key);
                }
            }
        }
        self.stats.dropped.fetch_add(1, Ordering::Relaxed);
    }

    fn abandon(&mut self) {
        self.retire();
        for writer in self.streams.values_mut() {
            writer.abandon();
        }
    }

    fn retire(&mut self) {
        self.available = false;
        if self.next.is_none() {
            self.retry_after = Some(Instant::now() + Duration::from_secs(1));
            warn!(root = %self.root.display(),
                "no fallback root; dropping fragments until a sync-start retry after one second");
        } else {
            warn!(root = %self.root.display(), "root retired until writer restart");
        }
    }

    fn close(&mut self) -> io::Result<()> {
        let mut first_error = None;
        for (key, writer) in &mut self.streams {
            if let Err(error) = writer.close() {
                error!(root = %self.root.display(), stream = %key, %error, "seal failed");
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
    use rustix::io::Errno;
    use std::fs;
    use std::sync::atomic::AtomicUsize;
    use std::sync::mpsc::{TryRecvError, sync_channel};

    use iris_mp4::reader::{self, ScanOptions};

    use crate::observer::NoopObserver;

    /// Whether a test fragment starts with a sync sample.
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Start {
        Sync,
        Continuation,
    }

    fn key() -> StreamKey {
        StreamKey {
            camera: "cam1".parse().unwrap(),
            kind: StreamKind::Main,
        }
    }

    fn fragment(index: u64, start: Start) -> Fragment {
        Fragment {
            samples: vec![Sample {
                data: Bytes::from_static(&[0, 0, 0, 2, 0x26, 1]),
                duration: 3_000,
                is_sync: start == Start::Sync,
            }],
            decode_time: index * 3_000,
            wall: WallUs::new(i64::try_from(index).unwrap() * 33_333),
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

    static TRANSIENT_PROBES: AtomicUsize = AtomicUsize::new(0);

    fn exhausted_descriptors(_: &Path) -> io::Result<FreeSpace> {
        TRANSIENT_PROBES.fetch_add(1, Ordering::SeqCst);
        Err(io::Error::from_raw_os_error(Errno::MFILE.raw_os_error()))
    }

    fn missing_root(_: &Path) -> io::Result<FreeSpace> {
        Err(io::Error::from_raw_os_error(Errno::NOENT.raw_os_error()))
    }

    #[test]
    fn transient_probe_errors_are_resource_errors_only() {
        for errno in [
            Errno::MFILE,
            Errno::NFILE,
            Errno::NOMEM,
            Errno::AGAIN,
            Errno::WOULDBLOCK,
            Errno::INTR,
        ] {
            assert!(
                crate::fs::is_transient_resource_error(&errno.into()),
                "{errno:?}"
            );
        }
        for errno in [
            Errno::NOENT,
            Errno::IO,
            Errno::STALE,
            Errno::NOTDIR,
            Errno::LOOP,
            Errno::ACCESS,
        ] {
            assert!(
                !crate::fs::is_transient_resource_error(&errno.into()),
                "{errno:?}"
            );
        }
        assert!(!crate::fs::is_transient_resource_error(&io::Error::new(
            io::ErrorKind::InvalidData,
            "recording-root marker changed after admission"
        )));
    }

    #[test]
    fn a_transient_probe_failure_keeps_writing_and_probes_again_after_the_cadence() {
        let dir = tempfile::tempdir().unwrap();
        let stats = Arc::new(WriterStats::default());
        let mut worker = RootWorker::new(
            dir.path().to_path_buf(),
            StoragePolicy::default(),
            Arc::clone(&stats),
            Arc::new(NoopObserver),
            None,
            1,
            exhausted_descriptors,
            None,
        );
        worker.write(key(), fragment(0, Start::Sync));
        worker.write(key(), fragment(1, Start::Continuation));
        assert!(worker.available, "the root stays in use");
        assert_eq!(
            TRANSIENT_PROBES.load(Ordering::SeqCst),
            1,
            "cached for one second"
        );

        let checked = worker
            .space_checked
            .expect("a failed probe still starts the cadence");
        worker.space_checked = Some(checked.checked_sub(Duration::from_secs(2)).unwrap());
        worker.write(key(), fragment(2, Start::Continuation));
        assert_eq!(
            TRANSIENT_PROBES.load(Ordering::SeqCst),
            2,
            "probed again after the cadence"
        );
        assert!(worker.available);
        assert_eq!(stats.fragments(), 3);
        assert_eq!(stats.dropped(), 0);
        assert_eq!(stats.write_errors(), 0);
        worker.close().unwrap();

        let mut media = Vec::new();
        let mut directories = vec![dir.path().to_path_buf()];
        while let Some(directory) = directories.pop() {
            for entry in fs::read_dir(directory).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    directories.push(path);
                } else if path.extension().is_some_and(|extension| extension == "mp4") {
                    media.push(path);
                }
            }
        }
        assert_eq!(media.len(), 1, "the open file was never abandoned");
        let report =
            reader::scan(fs::File::open(&media[0]).unwrap(), ScanOptions::default()).unwrap();
        assert_eq!(report.fragments.len(), 3);
    }

    #[test]
    fn an_unavailable_root_probe_failure_still_abandons() {
        let dir = tempfile::tempdir().unwrap();
        let stats = Arc::new(WriterStats::default());
        let mut worker = RootWorker::new(
            dir.path().to_path_buf(),
            StoragePolicy::default(),
            Arc::clone(&stats),
            Arc::new(NoopObserver),
            None,
            1,
            missing_root,
            None,
        );
        worker.write(key(), fragment(0, Start::Sync));
        assert!(!worker.available);
        assert_eq!(stats.write_errors(), 1);
        assert_eq!(stats.dropped(), 1);
        assert_eq!(stats.fragments(), 0);
    }

    #[test]
    fn approved_space_admission_rejects_a_replaced_root() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("root");
        let displaced = dir.path().join("displaced");
        let approved =
            crate::fs::admit_recording_root(&root, crate::fs::RootExpectation::Fresh).unwrap();
        let stats = Arc::new(WriterStats::default());
        let mut worker = RootWorker::new(
            root.clone(),
            StoragePolicy::default(),
            stats,
            Arc::new(NoopObserver),
            None,
            0,
            crate::fs::free_space,
            Some(approved),
        );
        fs::rename(&root, &displaced).unwrap();
        fs::create_dir(&root).unwrap();
        fs::copy(
            displaced.join(crate::fs::ROOT_IDENTITY_MARKER),
            root.join(crate::fs::ROOT_IDENTITY_MARKER),
        )
        .unwrap();
        fs::write(root.join("sentinel"), b"replacement bytes").unwrap();

        assert!(worker.has_space().is_err());
        assert_eq!(
            fs::read(root.join("sentinel")).unwrap(),
            b"replacement bytes"
        );
    }

    #[test]
    fn a_full_fallback_discards_dependents_until_a_sync_can_mark_a_new_run() {
        let dir = tempfile::tempdir().unwrap();
        let (tx, rx) = sync_channel(1);
        tx.send(Command::Shutdown).unwrap(); // deterministically hold the edge full
        let stats = Arc::new(WriterStats::default());
        let mut worker = RootWorker::new(
            dir.path().to_path_buf(),
            StoragePolicy::default(),
            Arc::clone(&stats),
            Arc::new(NoopObserver),
            Some(tx),
            0,
            crate::fs::free_space,
            None,
        );

        worker.forward(key(), fragment(0, Start::Sync));
        assert!(worker.forward_losses.contains(&key()));
        assert_eq!(stats.dropped(), 1);
        assert!(matches!(rx.recv().unwrap(), Command::Shutdown));

        worker.forward(key(), fragment(1, Start::Continuation));
        assert_eq!(stats.dropped(), 2);
        assert!(matches!(rx.try_recv(), Err(TryRecvError::Empty)));

        worker.forward(key(), fragment(2, Start::Sync));
        let Command::Fragment {
            key: sent_key,
            fragment,
        } = rx.recv().unwrap()
        else {
            panic!("expected recovered fragment");
        };
        assert_eq!(sent_key, key());
        assert_eq!(fragment.decode_time, 6_000);
        assert!(fragment.new_run);
        assert!(!worker.forward_losses.contains(&key()));
        assert_eq!(stats.queued(), 1);
        stats.dequeue(fragment.payload_len());
        assert_eq!(stats.queued(), 0);
    }
}
