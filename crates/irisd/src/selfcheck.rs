//! A rotating integrity check of sealed recordings.
//!
//! Each run scans the newest sealed file of one stream with CRC verification and
//! compares the scan with the catalog row, so silent corruption of written data or
//! of the catalog shows up in the log within a day instead of at the first playback.
//! The scan runs on a blocking thread; the monitor loop only launches it.

use std::io;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use iris_catalog::{CatalogHandle, SegmentRow};
use iris_core::id::StreamKey;
use iris_mp4::reader::{ScanOptions, ScanReport, scan};
use tracing::{debug, info, warn};

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

    /// Runs that found no sealed file for the stream.
    pub fn skipped(&self) -> u64 {
        self.skipped.load(Ordering::Relaxed)
    }
}

/// Picks streams in turn and checks each one's newest sealed file.
#[derive(Debug)]
pub struct SelfCheck {
    streams: Vec<StreamKey>,
    next: usize,
    stats: Arc<SelfCheckStats>,
}

impl SelfCheck {
    /// Creates a checker over `streams`.
    #[must_use]
    pub fn new(streams: Vec<StreamKey>) -> Self {
        Self {
            streams,
            next: 0,
            stats: Arc::new(SelfCheckStats::default()),
        }
    }

    /// Shared outcome counters.
    #[must_use]
    pub fn stats(&self) -> Arc<SelfCheckStats> {
        Arc::clone(&self.stats)
    }

    /// The interval between runs that visits every stream once per `cycle`.
    #[must_use]
    pub fn interval(&self, cycle: Duration) -> Duration {
        let streams = u32::try_from(self.streams.len().max(1)).unwrap_or(u32::MAX);
        cycle / streams
    }

    /// Launches the check of the next stream; the scan runs on a blocking thread.
    pub fn launch_next(&mut self, catalog: &CatalogHandle) {
        let Some(key) = self.streams.get(self.next).cloned() else {
            return;
        };
        self.next = (self.next + 1) % self.streams.len();
        let catalog = catalog.clone();
        let stats = Arc::clone(&self.stats);
        tokio::spawn(async move {
            check_stream(&catalog, &key, &stats).await;
        });
    }
}

async fn check_stream(catalog: &CatalogHandle, key: &StreamKey, stats: &SelfCheckStats) {
    let row = match catalog.newest_sealed(key).await {
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
    let scanned = tokio::task::spawn_blocking(move || scan_file(&path)).await;
    let ms = started.elapsed().as_millis();
    let outcome = match scanned {
        Ok(Ok(report)) => compare(&row, &report),
        Ok(Err(err)) => Err(format!("cannot scan: {err}")),
        Err(err) => Err(format!("scan task panicked: {err}")),
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

/// Scans a file completely, verifying every trailer CRC.
fn scan_file(path: &Path) -> io::Result<ScanReport> {
    let file = std::fs::File::open(path)?;
    scan(
        io::BufReader::new(file),
        ScanOptions {
            verify_crc: true,
            collect_samples: false,
        },
    )
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
    if let (Some(first), Some(last)) = (report.fragments.first(), report.fragments.last()) {
        if first.decode_time != row.media_start {
            problems.push(format!(
                "media start {} vs catalog {}",
                first.decode_time, row.media_start
            ));
        }
        let media_end = last.decode_time + u64::from(last.duration);
        if media_end != row.media_end {
            problems.push(format!(
                "media end {} vs catalog {}",
                media_end, row.media_end
            ));
        }
        if let Some(wall) = first.wall
            && wall != row.wall_start
        {
            problems.push(format!("wall start {wall} vs catalog {}", row.wall_start));
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

    use iris_catalog::SegmentId;
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
            SelfCheck::new(keys).interval(Duration::from_secs(3_600)),
            Duration::from_mins(15)
        );
        assert_eq!(
            SelfCheck::new(Vec::new()).interval(Duration::from_secs(60)),
            Duration::from_secs(60)
        );
    }
}
