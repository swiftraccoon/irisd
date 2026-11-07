//! Retention: free space below a threshold, a size quota, and age limits per camera.

use std::collections::HashMap;
use std::future::Future;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::Context;
use iris_catalog::{
    CatalogHandle, DeleteMarkResult, EventProtection, EventProtectionCursor, SegmentId, SegmentRow,
};
use iris_core::id::{CameraId, StreamKey, StreamKind};
use iris_core::time::WallUs;
#[cfg(test)]
use iris_store::remove_segment;
use iris_store::{
    ApprovedRoot, FreeSpace, free_space, free_space_approved, remove_segment_beneath,
};
use tokio::sync::oneshot;
use tracing::{info, warn};

/// Microseconds per day.
const DAY_US: i64 = 86_400_000_000;
/// Segments deleted per catalog query while freeing space.
const BATCH: usize = 16;
/// Upper bound on deletions per pass, so a pathological state cannot stall the daemon.
const MAX_PER_PASS: usize = 512;
/// Catalog pages inspected per pass, including pages that produce no deletion.
const MAX_PAGES_PER_PASS: usize = 64;
/// Candidate rows materialized per pass.
const MAX_ROWS_PER_PASS: usize = 1_024;
/// Work stops between operations once this elapsed budget is reached.
const WORK_BUDGET: Duration = Duration::from_secs(1);
/// Minimal retained-event rows fetched per page.
const PROTECTION_PAGE: usize = 256;
/// Snapshot rows retained as a filtering optimization. The atomic transition
/// and candidate SQL protect events beyond this cap.
const MAX_PROTECTIONS_PER_PASS: usize = 2_048;
/// Retained-event snapshot work cannot consume the whole retention budget.
const PROTECTION_BUDGET: Duration = Duration::from_millis(100);
/// A pass that takes longer than this is logged as a warning.
const SLOW_PASS: Duration = Duration::from_secs(2);

type SegmentCursor = (WallUs, SegmentId);

/// Bounded scan progress retained between periodic retention passes.
#[derive(Debug, Default)]
pub struct RetentionState {
    configured_roots: Vec<PathBuf>,
    min_free_percent: Option<u8>,
    quota_bytes: Option<u64>,
    next_root: usize,
    root_cursors: HashMap<PathBuf, Option<SegmentCursor>>,
    quota_cursor: Option<SegmentCursor>,
    age_cursors: HashMap<StreamKey, AgeScan>,
    next_age_key: usize,
}

#[derive(Debug)]
struct AgeScan {
    days: u32,
    cursor: Option<SegmentCursor>,
}

impl RetentionState {
    fn reconcile(
        &mut self,
        roots: &[PathBuf],
        min_free_percent: u8,
        quota_bytes: Option<u64>,
        age_limits: &HashMap<CameraId, u32>,
    ) {
        if self.configured_roots != roots {
            self.root_cursors.retain(|root, _| roots.contains(root));
            for root in roots {
                self.root_cursors.entry(root.clone()).or_default();
            }
            self.configured_roots = roots.to_vec();
            self.quota_cursor = None;
            self.next_root = 0;
        }
        if self.min_free_percent != Some(min_free_percent) {
            self.min_free_percent = Some(min_free_percent);
            for cursor in self.root_cursors.values_mut() {
                *cursor = None;
            }
        }
        if self.quota_bytes != quota_bytes {
            self.quota_bytes = quota_bytes;
            self.quota_cursor = None;
        }
        self.age_cursors.retain(|key, scan| {
            age_limits
                .get(&key.camera)
                .is_some_and(|days| *days == scan.days)
        });
        for (camera, &days) in age_limits {
            for kind in [StreamKind::Main, StreamKind::Sub, StreamKind::Audio] {
                self.age_cursors
                    .entry(StreamKey {
                        camera: camera.clone(),
                        kind,
                    })
                    .or_insert(AgeScan { days, cursor: None });
            }
        }
        if self.age_cursors.is_empty() {
            self.next_age_key = 0;
        } else {
            self.next_age_key %= self.age_cursors.len();
        }
    }

    fn root_start(&mut self, roots: &[PathBuf]) -> usize {
        if roots.is_empty() {
            return 0;
        }
        let start = self.next_root % roots.len();
        self.next_root = (start + 1) % roots.len();
        start
    }

    fn age_keys(&mut self) -> Vec<StreamKey> {
        let mut keys: Vec<StreamKey> = self.age_cursors.keys().cloned().collect();
        keys.sort();
        if keys.is_empty() {
            return keys;
        }
        let start = self.next_age_key % keys.len();
        self.next_age_key = (start + 1) % keys.len();
        keys.rotate_left(start);
        keys
    }
}

struct CandidatePage {
    rows: Vec<SegmentRow>,
    end: SegmentCursor,
}

struct RootOrder<'a> {
    roots: &'a [PathBuf],
    start: usize,
}

struct FreeRootContext<'a> {
    catalog: &'a CatalogHandle,
    retained: &'a [EventProtection],
    now: WallUs,
    approved_roots: &'a [ApprovedRoot],
}

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
    let days = retention_days?;
    let span = i64::from(days).checked_mul(DAY_US)?;
    now.get().checked_sub(span).map(WallUs::new)
}

/// Picks the sealed rows whose end precedes `cutoff`.
#[must_use]
#[cfg(test)]
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
pub fn protected(row: &SegmentRow, retained: &[EventProtection]) -> bool {
    retained.iter().any(|event| {
        event.camera == row.key.camera
            && event.time.offset(RETAIN_MARGIN_US) > row.wall_start
            && event.time.offset(-RETAIN_MARGIN_US) < row.wall_end
    })
}

/// The next raw-bounded page of deletable sealed segments after `cursor`, oldest first.
/// `None` means the indexed scan is exhausted. Callers commit progress as they consume rows.
async fn deletable_batch(
    catalog: &CatalogHandle,
    cursor: Option<SegmentCursor>,
    root: Option<&Path>,
    now: WallUs,
    tally: &mut Tally,
) -> anyhow::Result<Option<CandidatePage>> {
    let page = catalog
        .retention_candidates(
            root,
            cursor,
            now,
            Duration::from_micros(RETAIN_MARGIN_US as u64),
            BATCH,
        )
        .await
        .context("listing oldest segments")?;
    tally.note_page(page.scanned);
    let Some(end) = page.next else {
        return Ok(None);
    };
    Ok(Some(CandidatePage {
        rows: page.rows,
        end,
    }))
}

fn segment_cursor(row: &SegmentRow) -> SegmentCursor {
    (row.wall_start, row.id)
}

async fn load_protections(
    catalog: &CatalogHandle,
    now: WallUs,
    started: Instant,
) -> anyhow::Result<Vec<EventProtection>> {
    let mut result = Vec::new();
    let mut after = None;
    while result.len() < MAX_PROTECTIONS_PER_PASS && started.elapsed() < PROTECTION_BUDGET {
        let limit = PROTECTION_PAGE.min(MAX_PROTECTIONS_PER_PASS - result.len());
        let page = catalog
            .event_protections(now, after, limit)
            .await
            .context("listing retained-event protections")?;
        let Some(last) = page.last() else {
            break;
        };
        after = Some(EventProtectionCursor {
            retain_until: last.retain_until,
            id: last.id,
        });
        let complete = page.len() < limit;
        result.extend(page);
        if complete {
            break;
        }
    }
    Ok(result)
}

/// One retention pass: frees space on each root, trims to the quota, then applies
/// age limits. Logs the pass when it deleted something or ran slowly.
#[cfg(test)]
pub async fn pass(
    catalog: &CatalogHandle,
    roots: &[PathBuf],
    state: &mut RetentionState,
    min_free_percent: u8,
    quota_bytes: Option<u64>,
    age_limits: &HashMap<CameraId, u32>,
) -> anyhow::Result<PassReport> {
    pass_inner(
        catalog,
        roots,
        &[],
        state,
        min_free_percent,
        quota_bytes,
        age_limits,
    )
    .await
}

/// One retention pass using roots admitted at daemon startup.
pub async fn pass_approved(
    catalog: &CatalogHandle,
    approved_roots: &[ApprovedRoot],
    state: &mut RetentionState,
    min_free_percent: u8,
    quota_bytes: Option<u64>,
    age_limits: &HashMap<CameraId, u32>,
) -> anyhow::Result<PassReport> {
    let roots: Vec<PathBuf> = approved_roots
        .iter()
        .map(|root| root.path().to_path_buf())
        .collect();
    pass_inner(
        catalog,
        &roots,
        approved_roots,
        state,
        min_free_percent,
        quota_bytes,
        age_limits,
    )
    .await
}

async fn pass_inner(
    catalog: &CatalogHandle,
    roots: &[PathBuf],
    approved_roots: &[ApprovedRoot],
    state: &mut RetentionState,
    min_free_percent: u8,
    quota_bytes: Option<u64>,
    age_limits: &HashMap<CameraId, u32>,
) -> anyhow::Result<PassReport> {
    state.reconcile(roots, min_free_percent, quota_bytes, age_limits);
    let started = Instant::now();
    let now = WallUs::now();
    catalog
        .maintain_events(now)
        .await
        .context("maintaining the event lifecycle")?;
    let retained = load_protections(catalog, now, started).await?;
    let mut tally = Tally::new(started);
    let root_start = state.root_start(roots);
    let approved_for_probe = approved_roots.to_vec();
    let allow_unapproved_probe = approved_roots.is_empty();
    let free_pct_min = free_roots(
        FreeRootContext {
            catalog,
            retained: &retained,
            now,
            approved_roots,
        },
        RootOrder {
            roots,
            start: root_start,
        },
        min_free_percent,
        &mut tally,
        &mut state.root_cursors,
        move |root| {
            let approved = approved_for_probe
                .iter()
                .find(|approved| approved.path() == root)
                .cloned();
            async move {
                match approved {
                    Some(approved) => probe_approved_free_space(approved).await,
                    None if allow_unapproved_probe => probe_free_space(root).await,
                    None => Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "retention root was not admitted",
                    )),
                }
            }
        },
    )
    .await?;
    let total_bytes = match quota_bytes {
        Some(quota) => Some(
            trim_to_quota(
                catalog,
                quota,
                &retained,
                now,
                &mut tally,
                &mut state.quota_cursor,
                approved_roots,
            )
            .await?,
        ),
        None => None,
    };
    let age_keys = state.age_keys();
    expire_by_age(
        catalog,
        &age_keys,
        &mut state.age_cursors,
        &retained,
        now,
        &mut tally,
        approved_roots,
    )
    .await?;
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
struct Tally {
    started: Instant,
    attempted: usize,
    deleted: usize,
    freed: u64,
    pages: usize,
    rows: usize,
}

impl Tally {
    fn new(started: Instant) -> Self {
        Self {
            started,
            attempted: 0,
            deleted: 0,
            freed: 0,
            pages: 0,
            rows: 0,
        }
    }

    fn full(&self) -> bool {
        self.attempted >= MAX_PER_PASS
            || self.pages >= MAX_PAGES_PER_PASS
            || self.rows >= MAX_ROWS_PER_PASS
            || self.started.elapsed() >= WORK_BUDGET
    }

    fn note_page(&mut self, rows: usize) {
        self.pages = self.pages.saturating_add(1);
        self.rows = self.rows.saturating_add(rows);
    }

    async fn delete(
        &mut self,
        catalog: &CatalogHandle,
        row: &SegmentRow,
        approved_roots: &[ApprovedRoot],
    ) -> bool {
        self.attempted += 1;
        let Some(freed) = delete(catalog, row, approved_roots).await else {
            return false;
        };
        self.freed += freed;
        self.deleted += 1;
        true
    }
}

/// Deletes the oldest sealed segments while a root is below its free-space floor.
/// Returns the smallest free-space share seen across the roots.
async fn free_roots<F, Probe>(
    context: FreeRootContext<'_>,
    order: RootOrder<'_>,
    min_free_percent: u8,
    tally: &mut Tally,
    cursors: &mut HashMap<PathBuf, Option<SegmentCursor>>,
    mut inspect_space: F,
) -> anyhow::Result<Option<f64>>
where
    F: FnMut(PathBuf) -> Probe,
    Probe: Future<Output = io::Result<FreeSpace>>,
{
    let mut free_pct_min: Option<f64> = None;
    for offset in 0..order.roots.len() {
        let root = &order.roots[(order.start + offset) % order.roots.len()];
        let cursor = cursors.entry(root.clone()).or_default();
        loop {
            let space = inspect_space(root.clone())
                .await
                .with_context(|| format!("statvfs {}", root.display()))?;
            let pct = free_percent(&space);
            free_pct_min = Some(free_pct_min.map_or(pct, |min| min.min(pct)));
            if !needs_space(&space, min_free_percent) {
                *cursor = None;
                break;
            }
            if tally.full() {
                break;
            }
            let Some(page) =
                deletable_batch(context.catalog, *cursor, Some(root), context.now, tally).await?
            else {
                *cursor = None;
                warn!(root = %root.display(), "below the free-space floor with nothing left to delete");
                break;
            };
            let mut complete = true;
            let mut satisfied = false;
            for row in &page.rows {
                if protected(row, context.retained) {
                    *cursor = Some(segment_cursor(row));
                    continue;
                }
                if tally.full() {
                    complete = false;
                    break;
                }
                tally
                    .delete(context.catalog, row, context.approved_roots)
                    .await;
                *cursor = Some(segment_cursor(row));

                let space = inspect_space(root.clone())
                    .await
                    .with_context(|| format!("statvfs {}", root.display()))?;
                let pct = free_percent(&space);
                free_pct_min = Some(free_pct_min.map_or(pct, |min| min.min(pct)));
                if !needs_space(&space, min_free_percent) {
                    *cursor = None;
                    satisfied = true;
                    complete = false;
                    break;
                }
            }
            if satisfied {
                break;
            }
            if complete {
                *cursor = Some(page.end);
            }
        }
    }
    Ok(free_pct_min)
}

async fn probe_free_space(root: PathBuf) -> io::Result<FreeSpace> {
    probe_free_space_with(root, |root| free_space(&root)).await
}

async fn probe_approved_free_space(root: ApprovedRoot) -> io::Result<FreeSpace> {
    let path = root.path().to_path_buf();
    probe_free_space_with(path, move |_| free_space_approved(&root)).await
}

async fn probe_free_space_with<F>(root: PathBuf, probe: F) -> io::Result<FreeSpace>
where
    F: FnOnce(PathBuf) -> io::Result<FreeSpace> + Send + 'static,
{
    let (result_tx, result_rx) = oneshot::channel();
    // A blocked `statvfs` cannot be cancelled safely. The retention owner is
    // single-flight and forces bounded process exit instead of launching a
    // replacement; this standard-library thread cannot hold that exit.
    let thread = std::thread::Builder::new()
        .name("irisd-retention-probe".into())
        .spawn(move || {
            let result = probe(root);
            let _ = result_tx.send(result);
        })?;
    let result = result_rx
        .await
        .map_err(|err| io::Error::other(format!("free-space probe stopped: {err}")))?;
    thread
        .join()
        .map_err(|_| io::Error::other("free-space probe thread panicked"))?;
    result
}

/// Deletes the oldest sealed segments while the catalogued bytes exceed `quota`.
/// Returns the catalogued bytes afterwards.
async fn trim_to_quota(
    catalog: &CatalogHandle,
    quota: u64,
    retained: &[EventProtection],
    now: WallUs,
    tally: &mut Tally,
    cursor: &mut Option<SegmentCursor>,
    approved_roots: &[ApprovedRoot],
) -> anyhow::Result<u64> {
    loop {
        let mut total = catalog
            .total_bytes()
            .await
            .context("summing catalogued bytes")?;
        if !over_quota(total, Some(quota)) {
            *cursor = None;
            return Ok(total);
        }
        if tally.full() {
            return Ok(total);
        }
        let Some(page) = deletable_batch(catalog, *cursor, None, now, tally).await? else {
            *cursor = None;
            warn!(
                total_mib = total >> 20,
                quota_mib = quota >> 20,
                "over the quota with nothing sealed left to delete"
            );
            return Ok(total);
        };
        let mut complete = true;
        for row in &page.rows {
            if total <= quota {
                *cursor = None;
                return Ok(total);
            }
            if protected(row, retained) {
                *cursor = Some(segment_cursor(row));
                continue;
            }
            if tally.full() {
                complete = false;
                break;
            }
            let bytes = row.bytes;
            if tally.delete(catalog, row, approved_roots).await {
                total = total.saturating_sub(bytes);
            }
            *cursor = Some(segment_cursor(row));
        }
        if total <= quota {
            *cursor = None;
            return Ok(total);
        }
        if complete {
            *cursor = Some(page.end);
        }
    }
}

/// Deletes sealed segments older than each camera's age limit.
async fn expire_by_age(
    catalog: &CatalogHandle,
    keys: &[StreamKey],
    cursors: &mut HashMap<StreamKey, AgeScan>,
    retained: &[EventProtection],
    now: WallUs,
    tally: &mut Tally,
    approved_roots: &[ApprovedRoot],
) -> anyhow::Result<()> {
    for key in keys {
        let scan = cursors
            .get_mut(key)
            .expect("age key comes from retention state");
        let days = scan.days;
        let Some(cutoff) = expiry_cutoff(now, Some(days)) else {
            continue;
        };
        loop {
            if tally.full() {
                return Ok(());
            }
            let page = catalog
                .expired_candidates(
                    key,
                    cutoff,
                    scan.cursor,
                    now,
                    Duration::from_micros(RETAIN_MARGIN_US as u64),
                    BATCH,
                )
                .await
                .context("listing expired segments")?;
            tally.note_page(page.scanned);
            let Some(end) = page.next else {
                scan.cursor = None;
                break;
            };
            let mut complete = true;
            for row in &page.rows {
                if protected(row, retained) {
                    scan.cursor = Some(segment_cursor(row));
                    continue;
                }
                if tally.full() {
                    complete = false;
                    break;
                }
                tally.delete(catalog, row, approved_roots).await;
                scan.cursor = Some(segment_cursor(row));
            }
            if complete {
                scan.cursor = Some(end);
            }
            if tally.full() {
                return Ok(());
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

/// Commits deletion intent, removes one segment's pair durably, and then
/// removes its catalog row. `None` leaves a deleting row for startup retry.
async fn delete(
    catalog: &CatalogHandle,
    row: &SegmentRow,
    approved_roots: &[ApprovedRoot],
) -> Option<u64> {
    // The legacy unit-test harness predates persisted root ownership. Keep it
    // isolated from the daemon path, which always supplies admitted roots.
    #[cfg(test)]
    if approved_roots.is_empty() {
        return delete_unapproved_for_test(catalog, row).await;
    }

    let persisted_root = match catalog.segment_root(row.id).await {
        Ok(Some(root)) => root,
        Ok(None) => {
            warn!(path = %row.path.display(), "catalog row has no recording-root ownership");
            return None;
        }
        Err(err) => {
            warn!(path = %row.path.display(), %err, "cannot read catalog row ownership");
            return None;
        }
    };
    let Some(approved) = approved_roots
        .iter()
        .find(|root| root.path() == persisted_root)
        .cloned()
    else {
        warn!(
            path = %row.path.display(),
            root = %persisted_root.display(),
            "catalog row belongs to an unapproved recording root"
        );
        return None;
    };

    match catalog
        .mark_deleting_if_unprotected_at_root(
            row.id,
            approved.path(),
            WallUs::now(),
            Duration::from_micros(RETAIN_MARGIN_US as u64),
        )
        .await
    {
        Ok(DeleteMarkResult::Marked) => {}
        Ok(DeleteMarkResult::Protected) => return None,
        Ok(DeleteMarkResult::NotFound) => {
            warn!(path = %row.path.display(), "catalog row disappeared before deletion intent");
            return None;
        }
        Err(err) => {
            warn!(path = %row.path.display(), %err, "cannot mark catalog row deleting");
            return None;
        }
    }
    let path = row.path.clone();
    let (result_tx, result_rx) = oneshot::channel();
    let removal_path = path.clone();
    // Unlink and the directory durability barrier can block in the kernel.
    // Deletion intent remains durable if cancellation detaches this one call,
    // so startup can finish it idempotently after bounded process exit.
    let thread = std::thread::Builder::new()
        .name("irisd-retention-remove".into())
        .spawn(move || {
            let _ = result_tx.send(remove_segment_beneath(&approved, &removal_path));
        });
    let thread = match thread {
        Ok(thread) => thread,
        Err(err) => {
            warn!(path = %row.path.display(), %err, "cannot start segment removal");
            return None;
        }
    };
    let freed = match result_rx.await {
        Ok(Ok(bytes)) => bytes,
        Ok(Err(err)) => {
            warn!(path = %row.path.display(), %err, "cannot remove segment files");
            return None;
        }
        Err(err) => {
            warn!(%err, "removal thread stopped without a result");
            return None;
        }
    };
    if thread.join().is_err() {
        warn!(path = %row.path.display(), "segment removal thread panicked after returning a result");
        return None;
    }
    if let Err(err) = catalog.delete_segment(row.id).await {
        warn!(path = %row.path.display(), %err, "cannot delete catalog row");
        return None;
    }
    Some(freed)
}

#[cfg(test)]
async fn delete_unapproved_for_test(catalog: &CatalogHandle, row: &SegmentRow) -> Option<u64> {
    delete_unapproved_with(catalog, row, |path| remove_segment(&path)).await
}

#[cfg(test)]
async fn delete_unapproved_with<F>(
    catalog: &CatalogHandle,
    row: &SegmentRow,
    remove: F,
) -> Option<u64>
where
    F: FnOnce(PathBuf) -> io::Result<u64> + Send + 'static,
{
    match catalog
        .mark_deleting_if_unprotected(
            row.id,
            WallUs::now(),
            Duration::from_micros(RETAIN_MARGIN_US as u64),
        )
        .await
    {
        Ok(DeleteMarkResult::Marked) => {}
        Ok(DeleteMarkResult::Protected) => return None,
        Ok(DeleteMarkResult::NotFound) => {
            warn!(path = %row.path.display(), "catalog row disappeared before deletion intent");
            return None;
        }
        Err(err) => {
            warn!(path = %row.path.display(), %err, "cannot mark catalog row deleting");
            return None;
        }
    }
    let path = row.path.clone();
    let (result_tx, result_rx) = oneshot::channel();
    let removal_path = path.clone();
    let thread = match std::thread::Builder::new()
        .name("irisd-retention-test-remove".into())
        .spawn(move || {
            let _ = result_tx.send(remove(removal_path));
        }) {
        Ok(thread) => thread,
        Err(err) => {
            warn!(path = %row.path.display(), %err, "cannot start segment removal");
            return None;
        }
    };
    let freed = match result_rx.await {
        Ok(Ok(bytes)) => bytes,
        Ok(Err(err)) => {
            warn!(path = %row.path.display(), %err, "cannot remove segment files");
            return None;
        }
        Err(err) => {
            warn!(%err, "removal thread stopped without a result");
            return None;
        }
    };
    if thread.join().is_err() {
        warn!(path = %row.path.display(), "segment removal thread panicked after returning a result");
        return None;
    }
    if let Err(err) = catalog.delete_segment(row.id).await {
        warn!(path = %row.path.display(), %err, "cannot delete catalog row");
        return None;
    }
    Some(freed)
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn seed_segment(catalog: &CatalogHandle, path: &Path, camera: &str) {
        seed_segment_with_bytes(catalog, path, camera, b"x").await;
    }

    async fn seed_segment_with_bytes(
        catalog: &CatalogHandle,
        path: &Path,
        camera: &str,
        bytes: &[u8],
    ) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, bytes).unwrap();
        catalog
            .adopt_segment(iris_catalog::NewSegment {
                key: StreamKey {
                    camera: camera.parse().unwrap(),
                    kind: StreamKind::Main,
                },
                path: path.to_path_buf(),
                run_seq: 1,
                discontinuity: false,
                wall_start: WallUs::new(1_000_000),
                wall_end: WallUs::new(2_000_000),
                media_start: 0,
                media_end: 90_000,
                init_len: 0,
                len: u64::try_from(bytes.len()).unwrap(),
                fragments: 1,
                sample_entry: None,
            })
            .await
            .unwrap();
    }

    async fn finish_empty_age_scan(
        catalog: &CatalogHandle,
        key: &StreamKey,
        cursors: &mut HashMap<StreamKey, AgeScan>,
        now: WallUs,
    ) {
        if cursors[key].cursor.is_none() {
            return;
        }
        let mut tally = Tally::new(Instant::now());
        expire_by_age(
            catalog,
            std::slice::from_ref(key),
            cursors,
            &[],
            now,
            &mut tally,
            &[],
        )
        .await
        .unwrap();
        assert_eq!(tally.deleted, 0);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cancelled_retention_bounds_and_reaps_its_single_raw_removal_tail() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};

        struct ReleaseOnDrop(Option<std::sync::mpsc::Sender<()>>);

        impl Drop for ReleaseOnDrop {
            fn drop(&mut self) {
                if let Some(release) = self.0.take() {
                    let _ = release.send(());
                }
            }
        }

        let dir = tempfile::tempdir().unwrap();
        let catalog = CatalogHandle::open(&dir.path().join("catalog.db")).unwrap();
        let path = dir.path().join("recording.mp4");
        seed_segment(&catalog, &path, "cam").await;
        let row = catalog.oldest_sealed(1).await.unwrap().remove(0);
        let active = Arc::new(AtomicBool::new(false));
        let worker_active = Arc::clone(&active);
        let (started_tx, started_rx) = std::sync::mpsc::sync_channel(0);
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let mut release = ReleaseOnDrop(Some(release_tx));
        let catalog_for_task = catalog.clone();
        let task = tokio::spawn(async move {
            delete_unapproved_with(&catalog_for_task, &row, move |_| {
                worker_active.store(true, Ordering::SeqCst);
                let _ = started_tx.send(());
                let _ = release_rx.recv();
                worker_active.store(false, Ordering::SeqCst);
                Ok(0)
            })
            .await
        });
        started_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("raw removal worker must reach the gate");
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        assert_eq!(catalog.deleting_segments().await.unwrap().len(), 1);
        tokio::time::sleep(Duration::from_millis(10)).await;
        assert!(
            active.load(Ordering::SeqCst),
            "the gate must reproduce the uncancellable syscall tail"
        );
        release.0.take().unwrap().send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(1), async {
            while active.load(Ordering::SeqCst) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("released raw removal tail must exit");
        assert_eq!(
            catalog.deleting_segments().await.unwrap().len(),
            1,
            "durable deletion intent remains retryable after owner cancellation"
        );
        catalog.shutdown();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn gated_statvfs_keeps_control_responsive_and_has_one_bounded_raw_tail() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

        struct ReleaseOnDrop(Option<std::sync::mpsc::Sender<()>>);

        impl Drop for ReleaseOnDrop {
            fn drop(&mut self) {
                if let Some(release) = self.0.take() {
                    let _ = release.send(());
                }
            }
        }

        let active = Arc::new(AtomicBool::new(false));
        let calls = Arc::new(AtomicUsize::new(0));
        let worker_active = Arc::clone(&active);
        let worker_calls = Arc::clone(&calls);
        let (started_tx, started_rx) = std::sync::mpsc::sync_channel(0);
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let mut release = ReleaseOnDrop(Some(release_tx));
        let mut task = tokio::spawn(async move {
            probe_free_space_with(PathBuf::from("synthetic-root"), move |_| {
                worker_calls.fetch_add(1, Ordering::SeqCst);
                worker_active.store(true, Ordering::SeqCst);
                let _ = started_tx.send(());
                let _ = release_rx.recv();
                worker_active.store(false, Ordering::SeqCst);
                Ok(FreeSpace {
                    total: 100,
                    available: 50,
                })
            })
            .await
        });
        started_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("raw statvfs worker must reach the gate");
        tokio::select! {
            result = &mut task => panic!("gated statvfs unexpectedly completed: {result:?}"),
            () = tokio::time::sleep(Duration::from_millis(5)) => {}
        }
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        assert!(active.load(Ordering::SeqCst));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        release.0.take().unwrap().send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(1), async {
            while active.load(Ordering::SeqCst) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("released statvfs tail must exit");
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn approved_probe_rejects_a_replaced_root_before_retention_marks_rows() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("video");
        let displaced = dir.path().join("displaced-video");
        let approved =
            iris_store::admit_recording_root(&root, iris_store::RootExpectation::Fresh).unwrap();
        let ordinary_space = probe_approved_free_space(approved.clone()).await.unwrap();
        assert!(ordinary_space.total > 0);

        let catalog = CatalogHandle::open(&dir.path().join("catalog.db")).unwrap();
        catalog
            .configure_video_roots(std::slice::from_ref(&root))
            .await
            .unwrap();
        let recording = root.join("cam/main/1970-01-01/000000Z.mp4");
        seed_segment_with_bytes(&catalog, &recording, "cam", b"original recording").await;

        std::fs::rename(&root, &displaced).unwrap();
        std::fs::create_dir(&root).unwrap();
        let mut state = RetentionState::default();
        let error = pass_approved(
            &catalog,
            std::slice::from_ref(&approved),
            &mut state,
            0,
            Some(0),
            &HashMap::new(),
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("statvfs"));
        assert_eq!(catalog.deleting_segments().await.unwrap(), Vec::new());
        assert_eq!(catalog.oldest_sealed(2).await.unwrap().len(), 1);

        std::fs::remove_dir(&root).unwrap();
        std::fs::rename(&displaced, &root).unwrap();
        assert_eq!(std::fs::read(&recording).unwrap(), b"original recording");
        assert_eq!(catalog.oldest_sealed(2).await.unwrap().len(), 1);
        catalog.shutdown();
    }

    #[tokio::test]
    async fn quota_reaches_unprotected_segments_with_tied_start_times() {
        let dir = tempfile::tempdir().unwrap();
        let catalog = CatalogHandle::open(&dir.path().join("catalog.db")).unwrap();
        for i in 0..=BATCH {
            let camera = if i < BATCH { "protected" } else { "deletable" };
            seed_segment(&catalog, &dir.path().join(format!("{i}.mp4")), camera).await;
        }
        let event = iris_core::event::Event {
            camera: CameraId::try_new("protected").unwrap(),
            source: iris_core::event::EventSource::DahuaCgi,
            kind: iris_core::event::EventKind::Motion,
            state: iris_core::event::EventState::Pulse,
            nvr_time: WallUs::new(1_500_000),
            camera_time: None,
            data: None,
        };
        let id = catalog.insert_event(&event).await.unwrap();
        catalog
            .retain_event(id, Some(WallUs::new(i64::MAX)))
            .await
            .unwrap();
        let mut state = RetentionState::default();
        let result = pass(
            &catalog,
            &[],
            &mut state,
            0,
            Some(BATCH as u64),
            &HashMap::new(),
        )
        .await
        .unwrap();
        assert_eq!(result.deleted, 1);
        assert_eq!(catalog.total_bytes().await.unwrap(), BATCH as u64);
        assert!(!dir.path().join(format!("{BATCH}.mp4")).exists());
        catalog.shutdown();
    }

    #[tokio::test]
    async fn quota_does_not_starve_older_rows_under_sustained_arrivals() {
        let dir = tempfile::tempdir().unwrap();
        let catalog = CatalogHandle::open(&dir.path().join("catalog.db")).unwrap();
        let path = |index| dir.path().join(format!("segment-{index:03}.mp4"));
        for index in 1..=BATCH + 1 {
            seed_segment(&catalog, &path(index), "cam").await;
        }
        let event = catalog
            .insert_event(&iris_core::event::Event {
                camera: CameraId::try_new("sentinel").unwrap(),
                source: iris_core::event::EventSource::Manual,
                kind: iris_core::event::EventKind::Motion,
                state: iris_core::event::EventState::Pulse,
                nvr_time: WallUs::new(1_500_000),
                camera_time: None,
                data: None,
            })
            .await
            .unwrap();
        catalog
            .retain_event(event, Some(WallUs::new(i64::MAX)))
            .await
            .unwrap();

        let mut state = RetentionState::default();
        let first = pass(
            &catalog,
            &[],
            &mut state,
            0,
            Some(BATCH as u64),
            &HashMap::new(),
        )
        .await
        .unwrap();
        assert_eq!(first.deleted, 1);
        assert_eq!(state.quota_cursor, None);

        let mut first_arrival = None;
        let mut last_arrival = None;
        for cycle in 0..65 {
            let sentinel = BATCH + 2 + cycle * 2;
            let arrival = sentinel + 1;
            seed_segment_with_bytes(&catalog, &path(sentinel), "sentinel", b"").await;
            seed_segment(&catalog, &path(arrival), "cam").await;
            first_arrival.get_or_insert(arrival);
            last_arrival = Some(arrival);
            let report = pass(
                &catalog,
                &[],
                &mut state,
                0,
                Some(BATCH as u64),
                &HashMap::new(),
            )
            .await
            .unwrap();
            assert_eq!(report.deleted, 1);
            assert_eq!(state.quota_cursor, None);
        }

        for index in 2..=BATCH + 1 {
            assert!(!path(index).exists(), "older segment {index} was starved");
        }
        assert!(!path(first_arrival.unwrap()).exists());
        assert!(path(last_arrival.unwrap()).exists());
        assert_eq!(catalog.total_bytes().await.unwrap(), BATCH as u64);
        catalog.shutdown();
    }

    #[tokio::test]
    async fn free_space_retention_only_deletes_from_the_pressured_root() {
        let dir = tempfile::tempdir().unwrap();
        let catalog = CatalogHandle::open(&dir.path().join("catalog.db")).unwrap();
        let pressured = dir.path().join("pressured");
        let other = dir.path().join("other");
        catalog
            .configure_video_roots(&[pressured.clone(), other.clone()])
            .await
            .unwrap();
        for index in 0..128 {
            seed_segment(
                &catalog,
                &other.join(format!("older-{index:03}.mp4")),
                "cam",
            )
            .await;
        }
        let local = pressured.join("recording.mp4");
        seed_segment(&catalog, &local, "cam").await;
        let mut state = RetentionState::default();
        let result = pass(
            &catalog,
            &[pressured],
            &mut state,
            100,
            None,
            &HashMap::new(),
        )
        .await
        .unwrap();
        assert_eq!(result.deleted, 1);
        assert!(other.join("older-000.mp4").exists());
        assert!(other.join("older-127.mp4").exists());
        assert!(!local.exists());
        catalog.shutdown();
    }

    #[tokio::test]
    async fn free_space_resumes_at_oldest_row_after_partial_page_satisfaction() {
        let dir = tempfile::tempdir().unwrap();
        let catalog = CatalogHandle::open(&dir.path().join("catalog.db")).unwrap();
        let root = dir.path().join("video");
        let roots = vec![root.clone()];
        catalog.configure_video_roots(&roots).await.unwrap();
        let first = root.join("first.mp4");
        let second = root.join("second.mp4");
        let third = root.join("third.mp4");
        seed_segment(&catalog, &first, "cam").await;
        seed_segment(&catalog, &second, "cam").await;
        seed_segment(&catalog, &third, "cam").await;

        let mut cursors = HashMap::new();
        for expected in [&first, &second] {
            let mut checks = 0;
            let mut tally = Tally::new(Instant::now());
            free_roots(
                FreeRootContext {
                    catalog: &catalog,
                    retained: &[],
                    now: WallUs::now(),
                    approved_roots: &[],
                },
                RootOrder {
                    roots: &roots,
                    start: 0,
                },
                50,
                &mut tally,
                &mut cursors,
                |_| {
                    checks += 1;
                    std::future::ready(Ok(FreeSpace {
                        total: 100,
                        available: if checks == 1 { 0 } else { 50 },
                    }))
                },
            )
            .await
            .unwrap();
            assert_eq!(tally.deleted, 1);
            assert!(!expected.exists());
            assert_eq!(cursors[&root], None);
        }
        assert!(third.exists());
        catalog.shutdown();
    }

    #[tokio::test]
    async fn pressured_roots_take_turns_when_the_global_budget_is_exhausted() {
        let dir = tempfile::tempdir().unwrap();
        let catalog = CatalogHandle::open(&dir.path().join("catalog.db")).unwrap();
        let first = dir.path().join("first");
        let second = dir.path().join("second");
        let roots = vec![first.clone(), second.clone()];
        catalog.configure_video_roots(&roots).await.unwrap();
        let first_file = first.join("recording.mp4");
        let second_file = second.join("recording.mp4");
        seed_segment(&catalog, &first_file, "cam").await;
        seed_segment(&catalog, &second_file, "cam").await;

        let mut first_tally = Tally::new(Instant::now());
        first_tally.attempted = MAX_PER_PASS - 1;
        let mut cursors = HashMap::new();
        free_roots(
            FreeRootContext {
                catalog: &catalog,
                retained: &[],
                now: WallUs::now(),
                approved_roots: &[],
            },
            RootOrder {
                roots: &roots,
                start: 0,
            },
            100,
            &mut first_tally,
            &mut cursors,
            |root| std::future::ready(free_space(&root)),
        )
        .await
        .unwrap();
        assert!(!first_file.exists());
        assert!(second_file.exists());

        let mut second_tally = Tally::new(Instant::now());
        second_tally.attempted = MAX_PER_PASS - 1;
        free_roots(
            FreeRootContext {
                catalog: &catalog,
                retained: &[],
                now: WallUs::now(),
                approved_roots: &[],
            },
            RootOrder {
                roots: &roots,
                start: 1,
            },
            100,
            &mut second_tally,
            &mut cursors,
            |root| std::future::ready(free_space(&root)),
        )
        .await
        .unwrap();
        assert!(!second_file.exists());
        catalog.shutdown();
    }

    #[tokio::test]
    async fn protected_prefix_progresses_across_passes_and_wraps_after_exhaustion() {
        let dir = tempfile::tempdir().unwrap();
        let catalog = CatalogHandle::open(&dir.path().join("catalog.db")).unwrap();
        let segments = MAX_ROWS_PER_PASS + 1;
        for index in 0..segments {
            let camera = if index < MAX_ROWS_PER_PASS {
                "protected"
            } else {
                "deletable"
            };
            seed_segment(
                &catalog,
                &dir.path().join(format!("protected-{index:04}.mp4")),
                camera,
            )
            .await;
        }
        let id = catalog
            .insert_event(&iris_core::event::Event {
                camera: CameraId::try_new("protected").unwrap(),
                source: iris_core::event::EventSource::Manual,
                kind: iris_core::event::EventKind::Motion,
                state: iris_core::event::EventState::Pulse,
                nvr_time: WallUs::new(1_500_000),
                camera_time: None,
                data: None,
            })
            .await
            .unwrap();
        catalog
            .retain_event(id, Some(WallUs::new(i64::MAX)))
            .await
            .unwrap();
        let before = catalog.metrics().query().snapshot();
        let mut state = RetentionState::default();
        let first = pass(&catalog, &[], &mut state, 0, Some(0), &HashMap::new())
            .await
            .unwrap();
        let queries = catalog.metrics().query().snapshot().since(&before).count();
        assert_eq!(first.deleted, 0);
        assert_eq!(catalog.total_bytes().await.unwrap(), segments as u64);
        assert_eq!(
            state.quota_cursor,
            Some((WallUs::new(1_000_000), SegmentId(MAX_ROWS_PER_PASS as i64)))
        );
        assert!(
            queries <= u64::try_from(MAX_PAGES_PER_PASS * 2 + 4).unwrap(),
            "bounded pass issued {queries} catalog queries"
        );

        let second = pass(&catalog, &[], &mut state, 0, Some(0), &HashMap::new())
            .await
            .unwrap();
        assert_eq!(second.deleted, 1);
        assert_eq!(
            catalog.total_bytes().await.unwrap(),
            MAX_ROWS_PER_PASS as u64
        );
        assert_eq!(state.quota_cursor, None, "true exhaustion wraps the cursor");
        assert!(
            !dir.path()
                .join(format!("protected-{MAX_ROWS_PER_PASS:04}.mp4"))
                .exists()
        );

        let third = pass(&catalog, &[], &mut state, 0, Some(0), &HashMap::new())
            .await
            .unwrap();
        assert_eq!(third.deleted, 0);
        assert_eq!(
            state.quota_cursor,
            Some((WallUs::new(1_000_000), SegmentId(MAX_ROWS_PER_PASS as i64)))
        );
        catalog.shutdown();
    }

    #[tokio::test]
    async fn age_resumes_after_deleted_cursor_and_wraps_only_after_exhaustion() {
        let dir = tempfile::tempdir().unwrap();
        let catalog = CatalogHandle::open(&dir.path().join("catalog.db")).unwrap();
        let paths = [
            dir.path().join("first.mp4"),
            dir.path().join("second.mp4"),
            dir.path().join("third.mp4"),
        ];
        for path in &paths {
            seed_segment(&catalog, path, "cam").await;
        }
        let rows = catalog.oldest_sealed(paths.len()).await.unwrap();
        let key = rows[0].key.clone();
        let mut cursors = HashMap::from([(
            key.clone(),
            AgeScan {
                days: 1,
                cursor: None,
            },
        )]);
        let keys = [key.clone()];
        let now = WallUs::new(2 * DAY_US);

        for index in 0..2 {
            let mut tally = Tally::new(Instant::now());
            tally.attempted = MAX_PER_PASS - 1;
            expire_by_age(&catalog, &keys, &mut cursors, &[], now, &mut tally, &[])
                .await
                .unwrap();
            assert_eq!(tally.deleted, 1);
            assert!(!paths[index].exists());
            assert_eq!(
                cursors[&key].cursor,
                Some((rows[index].wall_start, rows[index].id))
            );
            assert!(paths[index + 1].exists());
        }

        let mut exhaustion_tally = Tally::new(Instant::now());
        expire_by_age(
            &catalog,
            &keys,
            &mut cursors,
            &[],
            now,
            &mut exhaustion_tally,
            &[],
        )
        .await
        .unwrap();
        assert_eq!(exhaustion_tally.deleted, 1);
        assert!(!paths[2].exists());
        finish_empty_age_scan(&catalog, &key, &mut cursors, now).await;
        assert_eq!(cursors[&key].cursor, None);

        let after_wrap = dir.path().join("after-wrap.mp4");
        seed_segment(&catalog, &after_wrap, "cam").await;
        let mut wrap_tally = Tally::new(Instant::now());
        expire_by_age(
            &catalog,
            &keys,
            &mut cursors,
            &[],
            now,
            &mut wrap_tally,
            &[],
        )
        .await
        .unwrap();
        assert_eq!(wrap_tally.deleted, 1);
        assert!(!after_wrap.exists());
        finish_empty_age_scan(&catalog, &key, &mut cursors, now).await;
        assert_eq!(cursors[&key].cursor, None);
        catalog.shutdown();
    }

    #[test]
    fn retention_state_reconciles_policy_changes_with_bounded_memory() {
        let first = PathBuf::from("/video/first");
        let second = PathBuf::from("/video/second");
        let third = PathBuf::from("/video/third");
        let cam1 = CameraId::try_new("cam1").unwrap();
        let cam2 = CameraId::try_new("cam2").unwrap();
        let mut ages = HashMap::from([(cam1.clone(), 7)]);
        let mut state = RetentionState::default();
        state.reconcile(&[first.clone(), second.clone()], 5, Some(10), &ages);
        let cursor = Some((WallUs::new(10), SegmentId(10)));
        *state.root_cursors.get_mut(&first).unwrap() = cursor;
        *state.root_cursors.get_mut(&second).unwrap() = cursor;
        state.quota_cursor = cursor;
        let main = StreamKey {
            camera: cam1.clone(),
            kind: StreamKind::Main,
        };
        state.age_cursors.get_mut(&main).unwrap().cursor = cursor;

        state.reconcile(&[first.clone(), second.clone()], 5, Some(10), &ages);
        assert_eq!(state.root_cursors[&first], cursor);
        assert_eq!(state.quota_cursor, cursor);
        assert_eq!(state.age_cursors[&main].cursor, cursor);

        state.reconcile(&[first.clone(), second.clone()], 6, Some(10), &ages);
        assert_eq!(state.root_cursors[&first], None);
        assert_eq!(state.root_cursors[&second], None);
        assert_eq!(state.quota_cursor, cursor);
        assert_eq!(state.age_cursors[&main].cursor, cursor);
        *state.root_cursors.get_mut(&second).unwrap() = cursor;

        ages = HashMap::from([(cam1.clone(), 14), (cam2, 30)]);
        state.reconcile(&[second.clone(), third.clone()], 6, Some(20), &ages);
        assert!(!state.root_cursors.contains_key(&first));
        assert_eq!(state.root_cursors[&second], cursor);
        assert_eq!(state.root_cursors[&third], None);
        assert_eq!(state.quota_cursor, None);
        assert_eq!(state.root_cursors.len(), 2);
        assert_eq!(state.age_cursors.len(), ages.len() * 3);
        assert_eq!(state.age_cursors[&main].days, 14);
        assert_eq!(state.age_cursors[&main].cursor, None);
        let first_age = state.age_keys();
        let second_age = state.age_keys();
        assert_ne!(first_age.first(), second_age.first());

        ages.remove(&cam1);
        state.reconcile(&[second, third], 6, Some(20), &ages);
        assert!(state.age_cursors.keys().all(|key| key.camera != cam1));
        assert_eq!(state.age_cursors.len(), ages.len() * 3);
    }

    #[tokio::test]
    async fn a_partial_pair_failure_does_not_claim_deletion_or_quota_progress() {
        let dir = tempfile::tempdir().unwrap();
        let catalog = CatalogHandle::open(&dir.path().join("catalog.db")).unwrap();
        let root = dir.path().join("video");
        let approved =
            iris_store::admit_recording_root(&root, iris_store::RootExpectation::Fresh).unwrap();
        catalog
            .configure_video_roots(std::slice::from_ref(&root))
            .await
            .unwrap();
        let path = root.join("cam/main/1970-01-01/000000Z.mp4");
        seed_segment(&catalog, &path, "cam").await;
        let index = path.with_extension("idx");
        std::fs::create_dir(&index).unwrap();

        let mut state = RetentionState::default();
        let report = pass(&catalog, &[], &mut state, 0, Some(0), &HashMap::new())
            .await
            .unwrap();
        assert_eq!(report.deleted, 0);
        assert_eq!(report.freed, 0);
        assert_eq!(report.total_bytes, Some(1));
        assert!(
            !path.exists(),
            "the media unlink completed before the fault"
        );
        assert!(index.is_dir());
        assert_eq!(catalog.deleting_segments().await.unwrap().len(), 1);

        std::fs::remove_dir(&index).unwrap();
        assert_eq!(
            crate::adopt::finish_deleting(&catalog, &[approved])
                .await
                .unwrap(),
            1
        );
        assert_eq!(catalog.total_bytes().await.unwrap(), 0);
        catalog.shutdown();
    }

    #[tokio::test]
    async fn a_deletion_intent_failure_prevents_either_unlink() {
        let dir = tempfile::tempdir().unwrap();
        let catalog = CatalogHandle::open(&dir.path().join("catalog.db")).unwrap();
        let path = dir.path().join("recording.mp4");
        let index = path.with_extension("idx");
        seed_segment(&catalog, &path, "cam").await;
        std::fs::write(&index, b"index").unwrap();
        let row = catalog.oldest_sealed(1).await.unwrap().remove(0);

        catalog.shutdown();
        assert_eq!(delete(&catalog, &row, &[]).await, None);
        assert!(path.exists(), "media must remain before durable intent");
        assert!(index.exists(), "index must remain before durable intent");
    }

    #[tokio::test]
    async fn retention_acknowledged_after_enumeration_prevents_the_atomic_mark() {
        let dir = tempfile::tempdir().unwrap();
        let catalog = CatalogHandle::open(&dir.path().join("catalog.db")).unwrap();
        let path = dir.path().join("recording.mp4");
        seed_segment(&catalog, &path, "cam").await;
        let row = catalog
            .retention_candidates(
                None,
                None,
                WallUs::now(),
                Duration::from_micros(RETAIN_MARGIN_US as u64),
                1,
            )
            .await
            .unwrap()
            .rows
            .remove(0);

        let id = catalog
            .insert_event(&iris_core::event::Event {
                camera: CameraId::try_new("cam").unwrap(),
                source: iris_core::event::EventSource::Manual,
                kind: iris_core::event::EventKind::Motion,
                state: iris_core::event::EventState::Pulse,
                nvr_time: WallUs::new(1_500_000),
                camera_time: None,
                data: None,
            })
            .await
            .unwrap();
        catalog
            .retain_event(id, Some(WallUs::new(i64::MAX)))
            .await
            .unwrap();

        assert_eq!(delete(&catalog, &row, &[]).await, None);
        assert!(path.exists());
        assert!(catalog.segment(row.id).await.unwrap().is_some());
        assert_eq!(catalog.deleting_segments().await.unwrap(), Vec::new());
        catalog.shutdown();
    }

    fn sealed_row(camera: &str, start: i64, end: i64) -> SegmentRow {
        SegmentRow {
            id: SegmentId(1),
            key: StreamKey {
                camera: CameraId::try_new(camera).unwrap(),
                kind: StreamKind::Main,
            },
            path: PathBuf::from("/nonexistent"),
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

    fn retained(camera: &str, at: i64) -> EventProtection {
        EventProtection {
            id: iris_catalog::EventId(1),
            camera: CameraId::try_new(camera).unwrap(),
            time: WallUs::new(at),
            retain_until: WallUs::new(i64::MAX),
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
            id: SegmentId(end_day),
            key: StreamKey {
                camera: "cam".parse().unwrap(),
                kind: StreamKind::Main,
            },
            path: PathBuf::from("x"),
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

    #[test]
    fn expiry_cutoff_fails_closed_at_arithmetic_boundaries() {
        let now = WallUs::new(2_000_000_000_000_000);
        let values = [0, 1, iris_core::config::MAX_RETENTION_DAYS];
        let cutoffs: Vec<WallUs> = values
            .into_iter()
            .map(|days| expiry_cutoff(now, Some(days)).expect("accepted retention age"))
            .collect();
        assert_eq!(cutoffs[0], now);
        assert!(cutoffs.windows(2).all(|pair| pair[1] < pair[0]));
        for days in [
            iris_core::config::MAX_RETENTION_DAYS + 1,
            200_000_000,
            u32::MAX,
        ] {
            assert_eq!(expiry_cutoff(now, Some(days)), None, "days {days}");
        }
        assert_eq!(
            expiry_cutoff(WallUs::new(i64::MIN), Some(1)),
            None,
            "wall subtraction also fails closed"
        );
    }
}
