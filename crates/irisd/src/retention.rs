//! Retention: free space below a threshold, a size quota, and age limits per camera.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use anyhow::Context;
use iris_catalog::{CatalogHandle, EventRow, SegmentRow, TimeRange};
use iris_core::id::{CameraId, StreamKey, StreamKind};
use iris_core::time::WallUs;
use iris_store::{FreeSpace, free_space, remove_segment};
use tracing::{info, warn};

/// Microseconds per day.
const DAY_US: i64 = 86_400_000_000;
/// Segments deleted per catalog query while freeing space.
const BATCH: usize = 16;
/// Upper bound on deletions per pass, so a pathological state cannot stall the daemon.
const MAX_PER_PASS: usize = 512;
/// A pass that takes longer than this is logged as a warning.
const SLOW_PASS: Duration = Duration::from_secs(2);

/// What one retention pass did and found.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct PassReport {
    /// Segments deleted.
    pub deleted: usize,
    /// Bytes freed.
    pub freed: u64,
    /// Wall-clock duration of the pass.
    pub duration: Duration,
    /// The smallest free-space share across the roots after the pass, in percent.
    pub free_pct_min: Option<f64>,
    /// Catalogued bytes after the pass, when a quota is configured.
    pub total_bytes: Option<u64>,
}

/// Free space as a percentage of the root's capacity.
#[must_use]
pub fn free_percent(space: &FreeSpace) -> f64 {
    if space.total == 0 {
        0.0
    } else {
        space.available as f64 / space.total as f64 * 100.0
    }
}

/// Whether a root is below its free-space floor.
#[must_use]
pub fn needs_space(space: &FreeSpace, min_free_percent: u8) -> bool {
    u128::from(space.available) * 100 < u128::from(space.total) * u128::from(min_free_percent)
}

/// Whether the catalogued recordings exceed the quota, if one is set.
#[must_use]
pub fn over_quota(total_bytes: u64, quota_bytes: Option<u64>) -> bool {
    quota_bytes.is_some_and(|quota| total_bytes > quota)
}

/// The cutoff before which a camera's sealed segments expire, if it has an
/// age limit.
#[must_use]
pub fn expiry_cutoff(now: WallUs, retention_days: Option<u32>) -> Option<WallUs> {
    retention_days.map(|days| now.offset(-i64::from(days) * DAY_US))
}

/// Picks the sealed rows whose end precedes `cutoff`.
#[must_use]
pub fn expired(rows: &[SegmentRow], cutoff: WallUs) -> Vec<&SegmentRow> {
    rows.iter()
        .filter(|row| row.sealed && row.wall_end < cutoff)
        .collect()
}

/// Footage this long before and after a retained event is kept with it.
pub const RETAIN_MARGIN_US: i64 = 30 * 1_000_000;

/// Whether a retained event keeps `row` from deletion: an event of the same
/// camera whose time, widened by [`RETAIN_MARGIN_US`] on each side, overlaps
/// the segment.
#[must_use]
pub fn protected(row: &SegmentRow, retained: &[EventRow]) -> bool {
    retained.iter().any(|event| {
        event.event.camera == row.key.camera
            && event.event.nvr_time.offset(RETAIN_MARGIN_US) > row.wall_start
            && event.event.nvr_time.offset(-RETAIN_MARGIN_US) < row.wall_end
    })
}

/// The next batch of deletable sealed segments after `cursor`, oldest first,
/// skipping protected ones; `None` when no sealed segment is left. The cursor
/// advances past the batch so a run of protected segments does not stall a
/// pass.
async fn deletable_batch(
    catalog: &CatalogHandle,
    cursor: &mut Option<WallUs>,
    retained: &[EventRow],
) -> anyhow::Result<Option<Vec<SegmentRow>>> {
    let rows = match *cursor {
        None => catalog.oldest_sealed(BATCH).await,
        Some(after) => catalog.oldest_sealed_after(after, BATCH).await,
    }
    .context("listing oldest segments")?;
    let Some(last) = rows.last() else {
        return Ok(None);
    };
    *cursor = Some(last.wall_start);
    Ok(Some(
        rows.into_iter()
            .filter(|row| !protected(row, retained))
            .collect(),
    ))
}

/// One retention pass: frees space on each root, trims to the quota, then applies
/// age limits. Logs the pass when it deleted something or ran slowly.
pub async fn pass(
    catalog: &CatalogHandle,
    roots: &[std::path::PathBuf],
    min_free_percent: u8,
    quota_bytes: Option<u64>,
    age_limits: &HashMap<CameraId, u32>,
) -> anyhow::Result<PassReport> {
    let started = Instant::now();
    let mut tally = Tally::default();
    let retained = catalog
        .retained_events(WallUs::now())
        .await
        .context("listing retained events")?;
    let free_pct_min = free_roots(catalog, roots, min_free_percent, &retained, &mut tally).await?;
    let total_bytes = match quota_bytes {
        Some(quota) => Some(trim_to_quota(catalog, quota, &retained, &mut tally).await?),
        None => None,
    };
    expire_by_age(catalog, age_limits, &retained, &mut tally).await?;
    let report = PassReport {
        deleted: tally.deleted,
        freed: tally.freed,
        duration: started.elapsed(),
        free_pct_min,
        total_bytes,
    };
    log_pass(&report, quota_bytes);
    Ok(report)
}

/// Deletions made so far in a pass.
#[derive(Default)]
struct Tally {
    deleted: usize,
    freed: u64,
}

impl Tally {
    fn full(&self) -> bool {
        self.deleted >= MAX_PER_PASS
    }

    async fn delete(&mut self, catalog: &CatalogHandle, row: &SegmentRow) {
        self.freed += delete(catalog, row).await;
        self.deleted += 1;
    }
}

/// Deletes the oldest sealed segments while a root is below its free-space floor.
/// Returns the smallest free-space share seen across the roots.
async fn free_roots(
    catalog: &CatalogHandle,
    roots: &[std::path::PathBuf],
    min_free_percent: u8,
    retained: &[EventRow],
    tally: &mut Tally,
) -> anyhow::Result<Option<f64>> {
    let mut free_pct_min: Option<f64> = None;
    for root in roots {
        let mut cursor = None;
        loop {
            let space = free_space(root).with_context(|| format!("statvfs {}", root.display()))?;
            let pct = free_percent(&space);
            free_pct_min = Some(free_pct_min.map_or(pct, |min| min.min(pct)));
            if !needs_space(&space, min_free_percent) || tally.full() {
                break;
            }
            let Some(rows) = deletable_batch(catalog, &mut cursor, retained).await? else {
                warn!(root = %root.display(), "below the free-space floor with nothing left to delete");
                break;
            };
            for row in rows {
                if tally.full() {
                    break;
                }
                tally.delete(catalog, &row).await;
            }
        }
    }
    Ok(free_pct_min)
}

/// Deletes the oldest sealed segments while the catalogued bytes exceed `quota`.
/// Returns the catalogued bytes afterwards.
async fn trim_to_quota(
    catalog: &CatalogHandle,
    quota: u64,
    retained: &[EventRow],
    tally: &mut Tally,
) -> anyhow::Result<u64> {
    let mut cursor = None;
    loop {
        let mut total = catalog
            .total_bytes()
            .await
            .context("summing catalogued bytes")?;
        if !over_quota(total, Some(quota)) || tally.full() {
            return Ok(total);
        }
        let Some(rows) = deletable_batch(catalog, &mut cursor, retained).await? else {
            warn!(
                total_mib = total >> 20,
                quota_mib = quota >> 20,
                "over the quota with nothing sealed left to delete"
            );
            return Ok(total);
        };
        for row in rows {
            if total <= quota || tally.full() {
                break;
            }
            let bytes = row.bytes;
            tally.delete(catalog, &row).await;
            total = total.saturating_sub(bytes);
        }
    }
}

/// Deletes sealed segments older than each camera's age limit.
async fn expire_by_age(
    catalog: &CatalogHandle,
    age_limits: &HashMap<CameraId, u32>,
    retained: &[EventRow],
    tally: &mut Tally,
) -> anyhow::Result<()> {
    let now = WallUs::now();
    for (camera, &days) in age_limits {
        let Some(cutoff) = expiry_cutoff(now, Some(days)) else {
            continue;
        };
        for kind in [StreamKind::Main, StreamKind::Sub, StreamKind::Audio] {
            let key = StreamKey {
                camera: camera.clone(),
                kind,
            };
            let range = TimeRange {
                from: WallUs::new(0),
                to: cutoff,
            };
            let rows = catalog
                .segments(&key, range)
                .await
                .context("listing expired segments")?;
            for row in expired(&rows, cutoff) {
                if tally.full() {
                    return Ok(());
                }
                if protected(row, retained) {
                    continue;
                }
                tally.delete(catalog, row).await;
            }
        }
    }
    Ok(())
}

/// Logs a pass that deleted something; a slow pass is a warning.
fn log_pass(report: &PassReport, quota_bytes: Option<u64>) {
    let free_pct = report.free_pct_min.unwrap_or(0.0);
    if report.duration >= SLOW_PASS {
        warn!(
            deleted = report.deleted,
            freed_mib = report.freed >> 20,
            ms = report.duration.as_millis(),
            free_pct = format_args!("{free_pct:.1}"),
            total_gib = format_args!("{:.2}", gib(report.total_bytes)),
            quota_gib = format_args!("{:.2}", gib(quota_bytes)),
            "slow retention pass"
        );
    } else if report.deleted > 0 {
        info!(
            deleted = report.deleted,
            freed_mib = report.freed >> 20,
            ms = report.duration.as_millis(),
            free_pct = format_args!("{free_pct:.1}"),
            total_gib = format_args!("{:.2}", gib(report.total_bytes)),
            quota_gib = format_args!("{:.2}", gib(quota_bytes)),
            "retention pass"
        );
    }
}

/// Bytes as gibibytes, zero when unknown.
fn gib(bytes: Option<u64>) -> f64 {
    bytes.map_or(0.0, |b| b as f64 / f64::from(1u32 << 30))
}

/// Removes one segment's files and catalog row; a missing file still drops the row.
async fn delete(catalog: &CatalogHandle, row: &SegmentRow) -> u64 {
    let path = row.path.clone();
    let freed = match tokio::task::spawn_blocking(move || remove_segment(&path)).await {
        Ok(Ok(bytes)) => bytes,
        Ok(Err(err)) if err.kind() == std::io::ErrorKind::NotFound => 0,
        Ok(Err(err)) => {
            warn!(path = %row.path.display(), %err, "cannot remove segment files");
            return 0;
        }
        Err(err) => {
            warn!(%err, "removal task panicked");
            return 0;
        }
    };
    if let Err(err) = catalog.delete_segment(row.id).await {
        warn!(path = %row.path.display(), %err, "cannot delete catalog row");
    }
    freed
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sealed_row(camera: &str, start: i64, end: i64) -> SegmentRow {
        SegmentRow {
            id: iris_catalog::SegmentId(1),
            key: StreamKey {
                camera: CameraId::try_new(camera).unwrap(),
                kind: StreamKind::Main,
            },
            path: std::path::PathBuf::from("/nonexistent"),
            run_seq: 1,
            discontinuity: false,
            wall_start: WallUs::new(start),
            wall_end: WallUs::new(end),
            media_start: 0,
            media_end: 0,
            init_len: 0,
            durable_len: 0,
            fragments: 0,
            bytes: 0,
            sealed: true,
            codecs: String::new(),
            width: 0,
            height: 0,
            sample_entry_hash: 0,
        }
    }

    fn retained(camera: &str, at: i64) -> EventRow {
        EventRow {
            id: iris_catalog::EventId(1),
            event: iris_core::event::Event {
                camera: CameraId::try_new(camera).unwrap(),
                source: iris_core::event::EventSource::DahuaCgi,
                kind: iris_core::event::EventKind::Motion,
                state: iris_core::event::EventState::Pulse,
                nvr_time: WallUs::new(at),
                camera_time: None,
                data: None,
            },
            retain_until: Some(WallUs::new(i64::MAX)),
        }
    }

    #[test]
    fn a_retained_event_protects_segments_within_the_margin_of_its_camera() {
        let minute = 60 * 1_000_000;
        let events = [retained("cam1", 10 * minute)];
        // Overlaps the widened window (9.5 to 10.5 minutes).
        assert!(protected(
            &sealed_row("cam1", 8 * minute, 576_000_000),
            &events
        ));
        assert!(protected(
            &sealed_row("cam1", 10 * minute + 20_000_000, 12 * minute),
            &events
        ));
        // Ends before the window opens, or starts after it closes.
        assert!(!protected(&sealed_row("cam1", 0, 9 * minute), &events));
        assert!(!protected(
            &sealed_row("cam1", 11 * minute, 12 * minute),
            &events
        ));
        // Another camera is never protected by it.
        assert!(!protected(
            &sealed_row("cam2", 9 * minute, 11 * minute),
            &events
        ));
        assert!(!protected(
            &sealed_row("cam1", 9 * minute, 11 * minute),
            &[]
        ));
    }

    #[test]
    fn free_space_floor_uses_percent_of_total() {
        let space = FreeSpace {
            total: 1_000,
            available: 49,
        };
        assert!(needs_space(&space, 5));
        let space = FreeSpace {
            total: 1_000,
            available: 50,
        };
        assert!(!needs_space(&space, 5));
        assert!(!needs_space(
            &FreeSpace {
                total: 0,
                available: 0
            },
            5
        ));
    }

    #[test]
    fn quota_compares_total_bytes_with_the_cap() {
        assert!(!over_quota(1 << 40, None));
        assert!(!over_quota(100, Some(100)));
        assert!(over_quota(101, Some(100)));
    }

    #[test]
    fn expiry_selects_only_sealed_rows_before_the_cutoff() {
        let now = WallUs::new(100 * DAY_US);
        let cutoff = expiry_cutoff(now, Some(21)).unwrap();
        assert_eq!(cutoff, WallUs::new(79 * DAY_US));
        assert_eq!(expiry_cutoff(now, None), None);
        let row = |end_day: i64, sealed: bool| SegmentRow {
            id: iris_catalog::SegmentId(end_day),
            key: StreamKey {
                camera: "cam".parse().unwrap(),
                kind: StreamKind::Main,
            },
            path: std::path::PathBuf::from("x"),
            run_seq: 0,
            discontinuity: false,
            wall_start: WallUs::new((end_day - 1) * DAY_US),
            wall_end: WallUs::new(end_day * DAY_US),
            media_start: 0,
            media_end: 0,
            init_len: 0,
            durable_len: 0,
            fragments: 0,
            bytes: 0,
            sealed,
            codecs: String::new(),
            width: 0,
            height: 0,
            sample_entry_hash: 0,
        };
        let rows = vec![
            row(70, true),
            row(78, true),
            row(79, true),
            row(60, false),
            row(90, true),
        ];
        let picked: Vec<i64> = expired(&rows, cutoff).iter().map(|r| r.id.0).collect();
        assert_eq!(picked, vec![70, 78]);
    }
}
