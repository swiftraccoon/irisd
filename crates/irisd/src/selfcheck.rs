//! A rotating integrity check of sealed recordings.
//!
//! Each run scans the newest sealed file of one stream with CRC verification
//! and compares the scan with the catalog row, so silent corruption of written
//! data or of the catalog shows up in the log within a day instead of at the
//! first playback. The scan runs on a thread of its own; the monitor loop only
//! launches it.

use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use iris_catalog::{CatalogHandle, SegmentRow};
use iris_core::id::StreamKey;
use iris_core::time::{MediaTicks, WallUs};
use iris_mp4::reader::{self, ScanOptions, ScanReport};
use iris_store::ApprovedRoot;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

/// The result a scan thread sends when it finishes; a dropped sender means
/// the thread ended without one.
type ScanResult = oneshot::Receiver<io::Result<ScanReport>>;

type ScanStarter = Arc<
    dyn Fn(Arc<[ApprovedRoot]>, PathBuf, CancellationToken) -> io::Result<ScanResult> + Send + Sync,
>;

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
    /// The roots admitted at startup; a recording is opened beneath its root.
    roots: Arc<[ApprovedRoot]>,
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
            .field("roots", &self.roots)
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
            roots: Arc::from(Vec::new()),
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

    /// Points the checker at the roots admitted at startup. Each check opens
    /// its recording beneath the root that holds it, without following a link
    /// or crossing a mount; a recording outside every root fails its check.
    pub fn set_roots(&mut self, roots: Vec<ApprovedRoot>) {
        self.roots = Arc::from(roots);
    }

    /// Replaces the streams the checker rotates over, as cameras are added
    /// and removed at run time; the rotation continues from its position,
    /// wrapping to the first stream when the list got shorter.
    pub fn set_streams(&mut self, streams: Vec<StreamKey>) {
        if self.next >= streams.len() {
            self.next = 0;
        }
        self.streams = streams;
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
        let roots = Arc::clone(&self.roots);
        let scan_starter = Arc::clone(&self.scan_starter);
        self.active = Some(tokio::spawn(async move {
            check_stream(&catalog, &key, &stats, &shutdown, roots, scan_starter).await;
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
    roots: Arc<[ApprovedRoot]>,
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
    let result = match scan_starter(roots, path, shutdown.clone()) {
        Ok(result) => result,
        Err(err) => {
            stats.failed.fetch_add(1, Ordering::Relaxed);
            warn!(stream = %key, %err, "selfcheck failed: cannot start scan thread");
            return;
        }
    };
    let scanned = result.await;
    let ms = started.elapsed().as_millis();
    if shutdown.is_cancelled() {
        debug!(stream = %key, ms, "selfcheck cancelled");
        return;
    }
    let outcome = match scanned {
        Ok(Ok(report)) => compare(&row, &report),
        Ok(Err(err)) => Err(format!("cannot scan: {err}")),
        Err(_) => Err("scan thread stopped without a result".to_owned()),
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

/// Starts the scan of `path` on a thread named `irisd-selfcheck`; the
/// thread's last action is sending its result.
fn start_scan(
    roots: Arc<[ApprovedRoot]>,
    path: PathBuf,
    shutdown: CancellationToken,
) -> io::Result<ScanResult> {
    let (result_tx, result) = oneshot::channel();
    std::thread::Builder::new()
        .name("irisd-selfcheck".into())
        .spawn(move || {
            let _ = result_tx.send(scan_file(&roots, &path, shutdown));
        })?;
    Ok(result)
}

/// Scans a file completely, verifying every trailer CRC. The file is opened
/// beneath the approved root that holds it, so the open follows no link,
/// crosses no mount, never blocks on a FIFO or device, and refuses a path
/// outside every root; the scan accepts at most the sidecar record limit of
/// fragments.
fn scan_file(
    roots: &[ApprovedRoot],
    path: &Path,
    shutdown: CancellationToken,
) -> io::Result<ScanReport> {
    let root = roots
        .iter()
        .filter(|root| path.starts_with(root.path()))
        .max_by_key(|root| root.path().as_os_str().len())
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "recording path is outside every approved root",
            )
        })?;
    let file = iris_store::open_segment_beneath(root, path)?;
    reader::scan(
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

/// A reader that fails every read and seek once `shutdown` is cancelled. The
/// failure is not `Interrupted`: the standard library's `read_exact` retries
/// that kind, which would spin for as long as the token stays cancelled.
struct CancellableReader<R> {
    inner: R,
    shutdown: CancellationToken,
}

impl<R> CancellableReader<R> {
    fn cancelled() -> io::Error {
        io::Error::other("scan cancelled")
    }
}

impl<R: Read> Read for CancellableReader<R> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if self.shutdown.is_cancelled() {
            return Err(Self::cancelled());
        }
        self.inner.read(buffer)
    }
}

impl<R: Seek> Seek for CancellableReader<R> {
    fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
        if self.shutdown.is_cancelled() {
            return Err(Self::cancelled());
        }
        self.inner.seek(position)
    }
}

/// Compares a sealed row with a CRC-verified scan of its file. The error lists
/// every mismatch; a fragment without a `prft` leaves a wall boundary
/// unverifiable, which is a mismatch too.
fn compare(row: &SegmentRow, report: &ScanReport) -> Result<(), String> {
    let mut problems = Vec::new();
    if let Some(stop) = &report.stop {
        problems.push(format!("scan stopped: {stop}"));
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
        match first.wall {
            Some(wall) if wall != row.wall_start => {
                problems.push(format!("wall start {wall} vs catalog {}", row.wall_start));
            }
            Some(_) => {}
            None => {
                problems.push("wall start unavailable: first fragment has no prft".to_owned());
            }
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
    use iris_core::id::{CameraId, StreamKind};
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
            sample_entry_id: iris_catalog::SampleEntryId(1),
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

    fn key(camera: &str) -> StreamKey {
        StreamKey {
            camera: CameraId::try_new(camera).unwrap(),
            kind: StreamKind::Main,
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
        assert!(
            reason.contains("scan stopped: CRC mismatch on the fragment at 1768"),
            "{reason}"
        );
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
    fn replacing_the_streams_keeps_the_rotation_in_range() {
        let mut check =
            SelfCheck::new(vec![key("a"), key("b"), key("c")], CancellationToken::new());
        check.next = 2;
        check.set_streams(vec![key("a"), key("b"), key("c"), key("d")]);
        assert_eq!(check.next, 2, "a longer list keeps the position");
        assert_eq!(
            check.interval(Duration::from_secs(400)),
            Some(Duration::from_secs(100))
        );
        check.set_streams(vec![key("a")]);
        assert_eq!(check.next, 0, "a shorter list wraps to the first stream");
        check.set_streams(Vec::new());
        assert_eq!(check.next, 0);
        assert_eq!(
            check.interval(Duration::from_secs(400)),
            Some(Duration::from_secs(400)),
            "no streams still yields a positive interval"
        );
    }

    #[test]
    fn interval_divides_the_cycle_over_the_streams() {
        let keys = vec![key("a"); 4];
        assert_eq!(
            SelfCheck::new(keys, CancellationToken::new()).interval(Duration::from_secs(3_600)),
            Some(Duration::from_mins(15))
        );
        assert_eq!(
            SelfCheck::new(Vec::new(), CancellationToken::new()).interval(Duration::from_secs(60)),
            Some(Duration::from_secs(60))
        );
        assert_eq!(
            SelfCheck::new(vec![key("a"); 2], CancellationToken::new())
                .interval(Duration::from_nanos(1)),
            None
        );
    }

    #[test]
    fn wall_start_is_derived_from_the_first_prft() {
        let mut mismatched = report();
        mismatched.fragments[0].wall = Some(WallUs::new(2_000_000));
        let reason = compare(&row(), &mismatched).unwrap_err();
        assert!(reason.contains("wall start"), "{reason}");

        let mut no_prft = report();
        no_prft.fragments[0].wall = None;
        let reason = compare(&row(), &no_prft).unwrap_err();
        assert!(reason.contains("first fragment has no prft"), "{reason}");
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
        assert!(reason.contains("final fragment has no prft"), "{reason}");

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
        let key = key("cam1");
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
        let scan_starter: ScanStarter = Arc::new(move |_roots, path, _shutdown| {
            *selected_by_scan.lock().unwrap() = Some(path);
            let (result_tx, result) = oneshot::channel();
            std::thread::spawn(move || {
                let _ = result_tx.send(Ok(report()));
            });
            Ok(result)
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
    fn selfcheck_rejects_decode_end_overflow() {
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
    }

    #[test]
    fn selfcheck_accepts_u64_media_boundaries() {
        let mut boundary_report = report();
        boundary_report.fragments = vec![fragment(768, u64::MAX - 1, Some(WallUs::new(0)))];
        boundary_report.fragments[0].duration = 1;
        boundary_report.valid_len = 1_768;
        let mut boundary_row = row();
        boundary_row.fragments = 1;
        boundary_row.bytes = 1_768;
        boundary_row.media_start = u64::MAX - 1;
        boundary_row.media_end = u64::MAX;
        boundary_row.wall_start = WallUs::new(0);
        boundary_row.wall_end = WallUs::new(11);
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
    async fn a_running_scan_is_single_flight_until_cancellation_releases_it() {
        use std::sync::atomic::AtomicBool;

        let dir = tempfile::tempdir().unwrap();
        let catalog = CatalogHandle::open(&dir.path().join("catalog.db")).unwrap();
        let key = key("a");
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
        let scan_starter: ScanStarter = Arc::new(move |_roots, path, shutdown| {
            let (result_tx, result) = oneshot::channel();
            let opened = Arc::clone(&scan_opened);
            let closed = Arc::clone(&scan_closed);
            std::thread::Builder::new()
                .name("irisd-selfcheck-gated-test".into())
                .spawn(move || {
                    let file = std::fs::File::open(path).unwrap();
                    opened.store(true, Ordering::Release);
                    while !shutdown.is_cancelled() {
                        std::thread::park_timeout(Duration::from_millis(1));
                    }
                    drop(file);
                    closed.store(true, Ordering::Release);
                    let _ = result_tx.send(Err(io::Error::other("gated scan cancelled")));
                })?;
            Ok(result)
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

    /// A reader that yields one zero byte per read and cancels its token
    /// after the first, so the scan's next read sees the cancellation.
    struct OneByteThenCancel {
        shutdown: CancellationToken,
    }

    impl Read for OneByteThenCancel {
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            let Some(first) = buffer.first_mut() else {
                return Ok(0);
            };
            *first = 0;
            self.shutdown.cancel();
            Ok(1)
        }
    }

    impl Seek for OneByteThenCancel {
        fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
            Ok(match position {
                SeekFrom::Start(offset) => offset,
                SeekFrom::End(_) => 1 << 20,
                SeekFrom::Current(_) => 0,
            })
        }
    }

    #[test]
    fn a_cancelled_scan_stops_instead_of_retrying_the_read() {
        let shutdown = CancellationToken::new();
        let reader = CancellableReader {
            inner: OneByteThenCancel {
                shutdown: shutdown.clone(),
            },
            shutdown,
        };
        let (result_tx, result_rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = result_tx.send(reader::scan(
                io::BufReader::new(reader),
                ScanOptions {
                    verify_crc: true,
                    collect_samples: false,
                },
            ));
        });
        let error = result_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("the scan must return once its token is cancelled")
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::Other);
        assert!(error.to_string().contains("scan cancelled"), "{error}");
    }

    /// Admits `dir` as a fresh root and returns it with a layout-conforming
    /// recording path whose directories exist.
    fn rooted(dir: &tempfile::TempDir) -> (Vec<ApprovedRoot>, PathBuf) {
        let approved =
            iris_store::admit_recording_root(dir.path(), iris_store::RootExpectation::Fresh)
                .unwrap();
        let parent = dir.path().join("cam1").join("main").join("2026-01-01");
        std::fs::create_dir_all(&parent).unwrap();
        (vec![approved], parent.join("recording.mp4"))
    }

    #[test]
    fn a_recording_outside_every_root_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let (roots, _) = rooted(&dir);
        let elsewhere = tempfile::tempdir().unwrap();
        let path = elsewhere.path().join("recording.mp4");
        std::fs::write(&path, b"synthetic").unwrap();
        assert_eq!(
            scan_file(&roots, &path, CancellationToken::new())
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidInput
        );
    }

    #[test]
    fn a_symlinked_layout_directory_is_not_followed() {
        let dir = tempfile::tempdir().unwrap();
        let (roots, path) = rooted(&dir);
        let elsewhere = tempfile::tempdir().unwrap();
        let target = elsewhere.path().join("main").join("2026-01-01");
        std::fs::create_dir_all(&target).unwrap();
        std::fs::write(target.join("recording.mp4"), b"synthetic").unwrap();
        std::fs::remove_dir_all(dir.path().join("cam1")).unwrap();
        std::os::unix::fs::symlink(elsewhere.path(), dir.path().join("cam1")).unwrap();
        // The rooted open refuses the link: ELOOP from the no-symlink resolution,
        // or ENOTDIR when the kernel checks the directory flag first.
        let error = scan_file(&roots, &path, CancellationToken::new()).unwrap_err();
        assert!(
            matches!(
                error.raw_os_error(),
                Some(code) if code == rustix::io::Errno::LOOP.raw_os_error()
                    || code == rustix::io::Errno::NOTDIR.raw_os_error()
            ),
            "{error}"
        );
    }

    #[test]
    fn a_fifo_in_place_of_a_recording_fails_the_scan_promptly() {
        let dir = tempfile::tempdir().unwrap();
        let (roots, path) = rooted(&dir);
        rustix::fs::mkfifoat(
            rustix::fs::CWD,
            &path,
            rustix::fs::Mode::RUSR | rustix::fs::Mode::WUSR,
        )
        .unwrap();
        let (sender, receiver) = std::sync::mpsc::channel();
        let scanned = path.clone();
        std::thread::spawn(move || {
            let _ = sender.send(scan_file(&roots, &scanned, CancellationToken::new()).map(drop));
        });
        let Ok(result) = receiver.recv_timeout(Duration::from_secs(2)) else {
            // Release a reader blocked in open; this never blocks the test.
            let _ = rustix::fs::openat(
                rustix::fs::CWD,
                &path,
                rustix::fs::OFlags::WRONLY | rustix::fs::OFlags::NONBLOCK,
                rustix::fs::Mode::empty(),
            );
            panic!("the self-check scan blocked on a FIFO");
        };
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn cancelled_reader_stops_before_more_io() {
        let shutdown = CancellationToken::new();
        shutdown.cancel();
        let mut reader = CancellableReader {
            inner: io::Cursor::new([1, 2, 3]),
            shutdown,
        };
        let error = reader.read(&mut [0; 1]).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::Other);
        assert_ne!(
            error.kind(),
            io::ErrorKind::Interrupted,
            "read_exact retries Interrupted"
        );
    }

    #[test]
    fn cancelled_scan_closes_its_file_descriptor() {
        let dir = tempfile::tempdir().unwrap();
        let (roots, path) = rooted(&dir);
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
            scan_file(&roots, &path, shutdown).unwrap_err().kind(),
            io::ErrorKind::Other
        );
        assert_eq!(descriptors_for_path(), 0);
    }

    #[test]
    fn a_file_beyond_the_fragment_limit_fails_the_check() {
        let mut limited = report();
        limited.stop = Some(ScanStop::FragmentLimit {
            offset: 2_768,
            limit: 2,
        });
        let reason = compare(&row(), &limited).unwrap_err();
        assert!(reason.contains("more than 2 fragments"), "{reason}");
    }
}
