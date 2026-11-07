//! A rotating integrity check of sealed recordings.
//!
//! Each run scans the newest sealed file of one stream with CRC verification and
//! compares the scan with the catalog row, so silent corruption of written data or
//! of the catalog shows up in the log within a day instead of at the first playback.
//! The scan runs on a blocking thread; the monitor loop only launches it.

use std::io;
use std::io::Read;
use std::io::Seek;
use std::io::SeekFrom;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use iris_catalog::{CatalogHandle, SegmentRow};
use iris_core::id::StreamKey;
use iris_core::time::{MediaTicks, WallUs};
use iris_mp4::reader::{ScanOptions, ScanReport, scan};
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

type ScanStarter = Arc<dyn Fn(PathBuf, CancellationToken) -> io::Result<ScanWorker> + Send + Sync>;

struct ScanWorker {
    result: oneshot::Receiver<io::Result<ScanReport>>,
    thread: std::thread::JoinHandle<()>,
}

/// Outcome counters shared with the statistics reporter.
#[derive(Debug, Default)]
pub struct SelfCheckStats {
    ok: AtomicU64,
    failed: AtomicU64,
    skipped: AtomicU64,
}

impl SelfCheckStats {
    /// Files that matched their catalog row.
    pub fn ok(&self) -> u64 {
        self.ok.load(Ordering::Relaxed)
    }

    /// Files that could not be read or did not match their catalog row.
    pub fn failed(&self) -> u64 {
        self.failed.load(Ordering::Relaxed)
    }

    /// Checks skipped because no sealed file was available, the catalog query
    /// failed, or the preceding single-flight scan was still running.
    pub fn skipped(&self) -> u64 {
        self.skipped.load(Ordering::Relaxed)
    }
}

/// Picks streams in turn and checks each one's newest sealed file.
pub struct SelfCheck {
    streams: Vec<StreamKey>,
    next: usize,
    stats: Arc<SelfCheckStats>,
    active: Option<JoinHandle<()>>,
    shutdown: CancellationToken,
    scan_starter: ScanStarter,
}

impl std::fmt::Debug for SelfCheck {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SelfCheck")
            .field("streams", &self.streams)
            .field("next", &self.next)
            .field("stats", &self.stats)
            .field("active", &self.active)
            .field("shutdown", &self.shutdown)
            .finish_non_exhaustive()
    }
}

impl SelfCheck {
    /// Creates a checker over `streams`.
    #[must_use]
    pub fn new(streams: Vec<StreamKey>, shutdown: CancellationToken) -> Self {
        Self {
            streams,
            next: 0,
            stats: Arc::new(SelfCheckStats::default()),
            active: None,
            shutdown,
            scan_starter: Arc::new(start_scan),
        }
    }

    /// Shared outcome counters.
    #[must_use]
    pub fn stats(&self) -> Arc<SelfCheckStats> {
        Arc::clone(&self.stats)
    }

    /// The interval between runs that visits every stream once per `cycle`.
    #[must_use]
    pub fn interval(&self, cycle: Duration) -> Option<Duration> {
        let streams = u32::try_from(self.streams.len().max(1)).unwrap_or(u32::MAX);
        let interval = cycle / streams;
        (!interval.is_zero()).then_some(interval)
    }

    /// Launches the check of the next stream when no check is running.
    pub async fn launch_next(&mut self, catalog: &CatalogHandle) {
        if let Some(mut active) = self.active.take() {
            if active.is_finished() {
                if let Err(err) = (&mut active).await {
                    warn!(%err, "selfcheck task failed");
                }
            } else {
                self.active = Some(active);
                self.stats.skipped.fetch_add(1, Ordering::Relaxed);
                debug!("selfcheck skipped: previous scan is still running");
                return;
            }
        }
        let Some(key) = self.streams.get(self.next).cloned() else {
            return;
        };
        self.next = (self.next + 1) % self.streams.len();
        let catalog = catalog.clone();
        let stats = Arc::clone(&self.stats);
        let shutdown = self.shutdown.clone();
        let scan_starter = Arc::clone(&self.scan_starter);
        self.active = Some(tokio::spawn(async move {
            check_stream(&catalog, &key, &stats, &shutdown, scan_starter).await;
        }));
    }

    /// Cancels the active scan and returns its owned task for coordinated shutdown.
    #[must_use]
    pub fn cancel_and_take(&mut self) -> Option<JoinHandle<()>> {
        self.shutdown.cancel();
        self.active.take()
    }
}

async fn check_stream(
    catalog: &CatalogHandle,
    key: &StreamKey,
    stats: &SelfCheckStats,
    shutdown: &CancellationToken,
    scan_starter: ScanStarter,
) {
    let row = match tokio::select! {
        () = shutdown.cancelled() => return,
        result = catalog.newest_sealed(key) => result,
    } {
        Ok(Some(row)) => row,
        Ok(None) => {
            debug!(stream = %key, "selfcheck skipped: no sealed file");
            stats.skipped.fetch_add(1, Ordering::Relaxed);
            return;
        }
        Err(err) => {
            warn!(stream = %key, %err, "selfcheck skipped: catalog query failed");
            stats.skipped.fetch_add(1, Ordering::Relaxed);
            return;
        }
    };
    let path = row.path.clone();
    let started = Instant::now();
    let worker = match scan_starter(path, shutdown.clone()) {
        Ok(worker) => worker,
        Err(err) => {
            stats.failed.fetch_add(1, Ordering::Relaxed);
            warn!(stream = %key, %err, "selfcheck failed: cannot start scan thread");
            return;
        }
    };
    let ScanWorker { result, thread } = worker;
    let scanned = result.await;
    let thread_panicked = thread.join().is_err();
    let ms = started.elapsed().as_millis();
    if shutdown.is_cancelled() {
        debug!(stream = %key, ms, "selfcheck cancelled");
        return;
    }
    let outcome = match scanned {
        Ok(Ok(report)) => compare(&row, &report),
        Ok(Err(err)) => Err(format!("cannot scan: {err}")),
        Err(err) if thread_panicked => Err(format!("scan thread panicked: {err}")),
        Err(err) => Err(format!("scan thread stopped without a result: {err}")),
    };
    match outcome {
        Ok(()) => {
            stats.ok.fetch_add(1, Ordering::Relaxed);
            info!(
                stream = %key,
                path = %row.path.display(),
                fragments = row.fragments,
                mib = row.bytes >> 20,
                ms,
                "selfcheck ok"
            );
        }
        Err(reason) => {
            stats.failed.fetch_add(1, Ordering::Relaxed);
            warn!(
                stream = %key,
                path = %row.path.display(),
                fragments = row.fragments,
                mib = row.bytes >> 20,
                ms,
                reason,
                "selfcheck failed"
            );
        }
    }
}

fn start_scan(path: PathBuf, shutdown: CancellationToken) -> io::Result<ScanWorker> {
    let (result_tx, result) = oneshot::channel();
    let thread = std::thread::Builder::new()
        .name("irisd-selfcheck".into())
        .spawn(move || {
            let _ = result_tx.send(scan_file(&path, shutdown));
        })?;
    Ok(ScanWorker { result, thread })
}

/// Scans a file completely, verifying every trailer CRC.
fn scan_file(path: &Path, shutdown: CancellationToken) -> io::Result<ScanReport> {
    let file = std::fs::File::open(path)?;
    scan(
        io::BufReader::new(CancellableReader {
            inner: file,
            shutdown,
        }),
        ScanOptions {
            verify_crc: true,
            collect_samples: false,
        },
    )
}

struct CancellableReader<R> {
    inner: R,
    shutdown: CancellationToken,
}

impl<R: Read> Read for CancellableReader<R> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if self.shutdown.is_cancelled() {
            return Err(io::Error::new(io::ErrorKind::Interrupted, "scan cancelled"));
        }
        self.inner.read(buffer)
    }
}

impl<R: Seek> Seek for CancellableReader<R> {
    fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
        if self.shutdown.is_cancelled() {
            return Err(io::Error::new(io::ErrorKind::Interrupted, "scan cancelled"));
        }
        self.inner.seek(position)
    }
}

/// Compares a sealed row with a CRC-verified scan of its file. The error lists
/// every mismatch.
pub fn compare(row: &SegmentRow, report: &ScanReport) -> Result<(), String> {
    let mut problems = Vec::new();
    if let Some(stop) = &report.stop {
        problems.push(format!("scan stopped: {stop:?}"));
    }
    let fragments = u32::try_from(report.fragments.len()).unwrap_or(u32::MAX);
    if fragments != row.fragments {
        problems.push(format!(
            "fragments {} vs catalog {}",
            fragments, row.fragments
        ));
    }
    if report.valid_len != row.bytes {
        problems.push(format!(
            "valid bytes {} vs catalog {}",
            report.valid_len, row.bytes
        ));
    }
    if report.init_len != row.init_len {
        problems.push(format!(
            "init {} vs catalog {}",
            report.init_len, row.init_len
        ));
    }
    for pair in report.fragments.windows(2) {
        if pair[1].decode_time < pair[0].decode_time {
            problems.push(format!(
                "media timeline decreases from {} to {}",
                pair[0].decode_time, pair[1].decode_time
            ));
        }
    }
    if let (Some(first), Some(last)) = (report.fragments.first(), report.fragments.last()) {
        if first.decode_time != row.media_start {
            problems.push(format!(
                "media start {} vs catalog {}",
                first.decode_time, row.media_start
            ));
        }
        if let Some(media_end) = last.decode_time.checked_add(u64::from(last.duration)) {
            if media_end != row.media_end {
                problems.push(format!(
                    "media end {} vs catalog {}",
                    media_end, row.media_end
                ));
            }
        } else {
            problems.push("media end overflows".to_owned());
        }
        if let Some(wall) = first.wall
            && wall != row.wall_start
        {
            problems.push(format!("wall start {wall} vs catalog {}", row.wall_start));
        }
        match last.wall {
            Some(wall) => {
                let duration_us = MediaTicks::new(i64::from(last.duration)).as_micros();
                match wall.get().checked_add(duration_us).map(WallUs::new) {
                    Some(wall_end) if wall_end != row.wall_end => {
                        problems.push(format!("wall end {wall_end} vs catalog {}", row.wall_end));
                    }
                    Some(_) => {}
                    None => problems.push("wall end overflows".to_owned()),
                }
            }
            None => problems.push("wall end unavailable: final fragment has no prft".to_owned()),
        }
    } else if row.fragments > 0 {
        problems.push("no fragments found".to_owned());
    }
    if problems.is_empty() {
        Ok(())
    } else {
        Err(problems.join("; "))
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use iris_catalog::{NewSegment, SegmentId};
    use iris_core::id::StreamKind;
    use iris_core::time::WallUs;
    use iris_mp4::reader::{FragmentInfo, ScanStop};

    use super::*;

    fn fragment(offset: u64, decode_time: u64, wall: Option<WallUs>) -> FragmentInfo {
        FragmentInfo {
            offset,
            len: 1_000,
            payload_offset: offset + 200,
            sequence: 1,
            decode_time,
            duration: 180_000,
            sample_count: 60,
            starts_with_sync: true,
            wall,
            sample_durations: Vec::new(),
            sample_sizes: Vec::new(),
        }
    }

    fn row() -> SegmentRow {
        SegmentRow {
            id: SegmentId(1),
            key: StreamKey {
                camera: "cam1".parse().unwrap(),
                kind: StreamKind::Main,
            },
            path: PathBuf::from("/x/cam1/main/2026-01-01/000000Z.mp4"),
            run_seq: 1,
            discontinuity: false,
            wall_start: WallUs::new(1_000_000),
            wall_end: WallUs::new(5_000_000),
            media_start: 0,
            media_end: 360_000,
            init_len: 768,
            durable_len: 2_768,
            fragments: 2,
            bytes: 2_768,
            sealed: true,
            codecs: "hvc1".into(),
            width: 1,
            height: 1,
            sample_entry_hash: 0,
        }
    }

    fn report() -> ScanReport {
        ScanReport {
            init_len: 768,
            fragments: vec![
                fragment(768, 0, Some(WallUs::new(1_000_000))),
                fragment(1_768, 180_000, Some(WallUs::new(3_000_000))),
            ],
            valid_len: 2_768,
            stop: None,
        }
    }

    #[test]
    fn matching_scan_passes() {
        assert_eq!(compare(&row(), &report()), Ok(()));
    }

    #[test]
    fn every_mismatch_is_listed() {
        let mut bad = report();
        bad.valid_len = 2_000;
        bad.stop = Some(ScanStop::CrcMismatch { offset: 1_768 });
        bad.fragments.pop();
        let reason = compare(&row(), &bad).unwrap_err();
        assert!(reason.contains("scan stopped"), "{reason}");
        assert!(reason.contains("fragments 1 vs catalog 2"), "{reason}");
        assert!(
            reason.contains("valid bytes 2000 vs catalog 2768"),
            "{reason}"
        );
        assert!(
            reason.contains("media end 180000 vs catalog 360000"),
            "{reason}"
        );
    }

    #[test]
    fn interval_divides_the_cycle_over_the_streams() {
        let keys = vec![
            StreamKey {
                camera: "a".parse().unwrap(),
                kind: StreamKind::Main
            };
            4
        ];
        assert_eq!(
            SelfCheck::new(keys, CancellationToken::new()).interval(Duration::from_secs(3_600)),
            Some(Duration::from_mins(15))
        );
        assert_eq!(
            SelfCheck::new(Vec::new(), CancellationToken::new()).interval(Duration::from_secs(60)),
            Some(Duration::from_secs(60))
        );
        let keys = vec![
            StreamKey {
                camera: "a".parse().unwrap(),
                kind: StreamKind::Main,
            };
            2
        ];
        assert_eq!(
            SelfCheck::new(keys, CancellationToken::new()).interval(Duration::from_nanos(1)),
            None
        );
    }

    #[test]
    fn wall_end_is_derived_from_the_final_prft() {
        let mut mismatched = report();
        mismatched.fragments[1].wall = Some(WallUs::new(4_000_000));
        let reason = compare(&row(), &mismatched).unwrap_err();
        assert!(reason.contains("wall end"), "{reason}");

        let mut no_prft = report();
        no_prft.fragments[1].wall = None;
        let reason = compare(&row(), &no_prft).unwrap_err();
        assert!(reason.contains("no prft"), "{reason}");

        let mut zero_wall = report();
        zero_wall.fragments[1].wall = Some(WallUs::new(0));
        let reason = compare(&row(), &zero_wall).unwrap_err();
        assert!(reason.contains("wall end"), "{reason}");

        let mut overflowing = report();
        overflowing.fragments[1].wall = Some(WallUs::new(i64::MAX));
        let reason = compare(&row(), &overflowing).unwrap_err();
        assert!(reason.contains("wall end overflows"), "{reason}");
    }

    #[tokio::test]
    async fn a_corrupt_wall_end_that_changes_newest_selection_fails_selfcheck() {
        use std::sync::Mutex;

        let dir = tempfile::tempdir().unwrap();
        let catalog = CatalogHandle::open(&dir.path().join("catalog.db")).unwrap();
        let key = StreamKey {
            camera: "cam1".parse().unwrap(),
            kind: StreamKind::Main,
        };
        let corrupt_path = PathBuf::from("/x/corrupt-newest.mp4");
        catalog
            .adopt_segment(NewSegment {
                key: key.clone(),
                path: corrupt_path.clone(),
                run_seq: 1,
                discontinuity: false,
                wall_start: WallUs::new(1_000_000),
                // The scanned report ends at 5_000_000. This corrupt value
                // incorrectly makes the row newest in the catalog query.
                wall_end: WallUs::new(9_000_000),
                media_start: 0,
                media_end: 360_000,
                init_len: 768,
                len: 2_768,
                fragments: 2,
                sample_entry: None,
            })
            .await
            .unwrap();
        catalog
            .adopt_segment(NewSegment {
                key: key.clone(),
                path: PathBuf::from("/x/genuinely-newest.mp4"),
                run_seq: 2,
                discontinuity: false,
                wall_start: WallUs::new(6_000_000),
                wall_end: WallUs::new(8_000_000),
                media_start: 0,
                media_end: 360_000,
                init_len: 768,
                len: 2_768,
                fragments: 2,
                sample_entry: None,
            })
            .await
            .unwrap();

        let selected = Arc::new(Mutex::new(None));
        let selected_by_scan = Arc::clone(&selected);
        let scan_starter: ScanStarter = Arc::new(move |path, _shutdown| {
            *selected_by_scan.lock().unwrap() = Some(path);
            let (result_tx, result) = oneshot::channel();
            let thread = std::thread::spawn(move || {
                let _ = result_tx.send(Ok(report()));
            });
            Ok(ScanWorker { result, thread })
        });
        let mut check = SelfCheck::new(vec![key], CancellationToken::new());
        check.scan_starter = scan_starter;
        check.launch_next(&catalog).await;
        check.active.take().unwrap().await.unwrap();

        assert_eq!(*selected.lock().unwrap(), Some(corrupt_path));
        assert_eq!(check.stats().ok(), 0);
        assert_eq!(check.stats().failed(), 1);
        catalog.shutdown();
    }

    #[test]
    fn decreasing_fragment_timeline_fails_selfcheck() {
        let mut decreasing = report();
        decreasing.fragments[0].decode_time = 200_000;
        decreasing.fragments[1].decode_time = 100_000;
        let reason = compare(&row(), &decreasing).unwrap_err();
        assert!(reason.contains("media timeline decreases"), "{reason}");
    }

    #[test]
    fn selfcheck_rejects_decode_end_overflow_and_accepts_u64_boundaries() {
        let mut overflow_report = report();
        overflow_report.fragments = vec![fragment(768, u64::MAX, Some(WallUs::new(0)))];
        overflow_report.fragments[0].duration = 1;
        overflow_report.valid_len = 1_768;
        let mut overflow_row = row();
        overflow_row.fragments = 1;
        overflow_row.bytes = 1_768;
        overflow_row.media_start = u64::MAX;
        overflow_row.media_end = u64::MAX;
        overflow_row.wall_start = WallUs::new(0);
        overflow_row.wall_end = WallUs::new(11);
        let reason = compare(&overflow_row, &overflow_report).unwrap_err();
        assert!(reason.contains("media end overflows"), "{reason}");

        let mut boundary_report = overflow_report.clone();
        boundary_report.fragments[0].decode_time = u64::MAX - 1;
        let mut boundary_row = overflow_row.clone();
        boundary_row.media_start = u64::MAX - 1;
        assert_eq!(compare(&boundary_row, &boundary_report), Ok(()));

        let mut wide_report = report();
        wide_report.fragments[0].duration = 0;
        wide_report.fragments[1].decode_time = i64::MAX as u64 + 1;
        wide_report.fragments[1].duration = 0;
        wide_report.fragments[0].wall = Some(WallUs::new(0));
        wide_report.fragments[1].wall = Some(WallUs::new(0));
        let mut wide_row = row();
        wide_row.media_end = i64::MAX as u64 + 1;
        wide_row.wall_start = WallUs::new(0);
        wide_row.wall_end = WallUs::new(0);
        assert_eq!(compare(&wide_row, &wide_report), Ok(()));
    }

    #[tokio::test]
    async fn catalogued_gated_scan_is_single_flight_cancelled_joined_and_closes_its_file() {
        use std::sync::atomic::AtomicBool;

        let dir = tempfile::tempdir().unwrap();
        let catalog = CatalogHandle::open(&dir.path().join("catalog.db")).unwrap();
        let key = StreamKey {
            camera: "a".parse().unwrap(),
            kind: StreamKind::Main,
        };
        let path = dir.path().join("recording.mp4");
        std::fs::write(&path, b"synthetic scan gate").unwrap();
        catalog
            .adopt_segment(NewSegment {
                key: key.clone(),
                path: path.clone(),
                run_seq: 1,
                discontinuity: false,
                wall_start: WallUs::new(1),
                wall_end: WallUs::new(2),
                media_start: 0,
                media_end: 1,
                init_len: 0,
                len: 19,
                fragments: 1,
                sample_entry: None,
            })
            .await
            .unwrap();

        let opened = Arc::new(AtomicBool::new(false));
        let closed = Arc::new(AtomicBool::new(false));
        let scan_opened = Arc::clone(&opened);
        let scan_closed = Arc::clone(&closed);
        let scan_starter: ScanStarter = Arc::new(move |path, shutdown| {
            let (result_tx, result) = oneshot::channel();
            let opened = Arc::clone(&scan_opened);
            let closed = Arc::clone(&scan_closed);
            let thread = std::thread::Builder::new()
                .name("irisd-selfcheck-gated-test".into())
                .spawn(move || {
                    let file = std::fs::File::open(path).unwrap();
                    opened.store(true, Ordering::Release);
                    while !shutdown.is_cancelled() {
                        std::thread::park_timeout(Duration::from_millis(1));
                    }
                    drop(file);
                    closed.store(true, Ordering::Release);
                    let _ = result_tx.send(Err(io::Error::new(
                        io::ErrorKind::Interrupted,
                        "gated scan cancelled",
                    )));
                })?;
            Ok(ScanWorker { result, thread })
        });
        let shutdown = CancellationToken::new();
        let mut check = SelfCheck::new(vec![key], shutdown);
        check.scan_starter = scan_starter;
        check.launch_next(&catalog).await;
        tokio::time::timeout(Duration::from_secs(1), async {
            while !opened.load(Ordering::Acquire) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("catalog query must reach the gated scan");
        let task_id = check.active.as_ref().unwrap().id();
        for _ in 0..100 {
            check.launch_next(&catalog).await;
            assert_eq!(check.active.as_ref().unwrap().id(), task_id);
        }
        assert_eq!(check.stats().skipped(), 100);
        let handle = check.cancel_and_take().expect("active task remains owned");
        tokio::time::timeout(Duration::from_secs(1), handle)
            .await
            .expect("global cancellation must release and join the scan")
            .unwrap();
        assert_eq!(check.stats().failed(), 0);
        assert!(closed.load(Ordering::Acquire));
        catalog.shutdown();
    }

    #[test]
    fn cancelled_reader_stops_before_more_io() {
        let shutdown = CancellationToken::new();
        shutdown.cancel();
        let mut reader = CancellableReader {
            inner: io::Cursor::new([1, 2, 3]),
            shutdown,
        };
        assert_eq!(
            reader.read(&mut [0; 1]).unwrap_err().kind(),
            io::ErrorKind::Interrupted
        );
    }

    #[test]
    fn cancelled_scan_closes_its_file_descriptor() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("recording.mp4");
        std::fs::write(&path, b"synthetic").unwrap();
        let descriptors_for_path = || {
            std::fs::read_dir("/proc/self/fd")
                .unwrap()
                .filter_map(Result::ok)
                .filter(|entry| std::fs::read_link(entry.path()).is_ok_and(|target| target == path))
                .count()
        };
        assert_eq!(descriptors_for_path(), 0);
        let shutdown = CancellationToken::new();
        shutdown.cancel();
        assert_eq!(
            scan_file(&path, shutdown).unwrap_err().kind(),
            io::ErrorKind::Interrupted
        );
        assert_eq!(descriptors_for_path(), 0);
    }
}
