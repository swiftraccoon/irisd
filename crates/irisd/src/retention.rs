//! Retention: the free-space floor per root, the size quota, and age limits
//! per camera, after the bounded event maintenance and deletion-intent retry
//! that open every pass. Filesystem probes and removals run on counted
//! threads ([`RawCalls`]), so a pass that times out never leaves an uncounted
//! call in the kernel.

use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::Context;
use iris_catalog::{CatalogHandle, DeleteMarkResult, SegmentCursor, SegmentRow, StartupRow};
use iris_core::id::{CameraId, StreamKey, StreamKind};
use iris_core::time::WallUs;
use iris_store::{ApprovedRoot, FreeSpace, free_space_approved, remove_segment_beneath};
use tokio::sync::oneshot;
use tracing::{debug, info, warn};

use crate::adopt::{approved_root_for_row, parse_segment_path};
use crate::raw_calls::RawCalls;

/// Microseconds per day.
const DAY_US: i64 = 86_400_000_000;
/// Raw candidate rows requested per catalog page by every policy.
const BATCH: usize = 16;
/// Upper bound on deletion attempts per pass, so a pathological state cannot
/// stall the daemon.
const MAX_PER_PASS: usize = 512;
/// Catalog pages inspected per pass, including pages that produce no deletion.
const MAX_PAGES_PER_PASS: usize = 64;
/// Raw candidate rows scanned per pass.
const MAX_ROWS_PER_PASS: usize = 1_024;
/// Work stops between operations once this elapsed budget is reached.
const WORK_BUDGET: Duration = Duration::from_secs(1);
/// A pass that takes longer than this is logged as a warning.
const SLOW_PASS: Duration = Duration::from_secs(2);
/// Deletion intents retried per pass.
const DELETION_RETRY_ROWS: usize = 16;
/// Elapsed budget for retrying deletion intents in one pass.
const DELETION_RETRY_BUDGET: Duration = Duration::from_millis(100);
/// Footage this long before and after a retained event is kept with it. The
/// catalog widens each active event by this margin when it selects
/// candidates and again when it commits deletion intent.
const RETAIN_MARGIN: Duration = Duration::from_secs(30);

/// Bounded scan progress retained between periodic retention passes.
#[derive(Debug)]
pub struct RetentionState {
    configured_roots: Vec<PathBuf>,
    min_free_percent: Option<u8>,
    quota_bytes: Option<u64>,
    next_root: usize,
    root_cursors: HashMap<PathBuf, Option<SegmentCursor>>,
    quota_cursor: Option<SegmentCursor>,
    age_cursors: HashMap<StreamKey, AgeScan>,
    next_age_key: usize,
    deleting_cursor: Option<SegmentCursor>,
    raw: RawCalls,
}

#[derive(Debug)]
struct AgeScan {
    days: u32,
    cursor: Option<SegmentCursor>,
}

impl RetentionState {
    /// Empty progress whose raw filesystem calls are counted in `raw`, the
    /// count the retention owner consults before launching a pass.
    pub fn new(raw: RawCalls) -> Self {
        Self {
            configured_roots: Vec::new(),
            min_free_percent: None,
            quota_bytes: None,
            next_root: 0,
            root_cursors: HashMap::new(),
            quota_cursor: None,
            age_cursors: HashMap::new(),
            next_age_key: 0,
            deleting_cursor: None,
            raw,
        }
    }

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
    now: WallUs,
    approved_roots: &'a [ApprovedRoot],
}

/// What the free-space step found.
#[derive(Debug, Default)]
struct FreeRootsOutcome {
    /// The smallest free share seen in any probe, in percent.
    free_pct_min: Option<f64>,
    /// Roots whose probe failed; the pass leaves their rows alone.
    unavailable: Vec<PathBuf>,
}

impl FreeRootsOutcome {
    fn observe(&mut self, space: &FreeSpace) {
        let pct = free_percent(space);
        self.free_pct_min = Some(self.free_pct_min.map_or(pct, |min| min.min(pct)));
    }
}

/// What one retention pass did and found.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct PassReport {
    /// Segments deleted.
    pub deleted: usize,
    /// Bytes freed.
    pub freed: u64,
    /// Earlier deletion intents completed by the pass's bounded retry.
    pub finished_deletions: usize,
    /// Roots whose free-space probe failed; the pass left their rows alone.
    pub roots_unavailable: usize,
    /// Elapsed (monotonic) duration of the pass.
    pub duration: Duration,
    /// The smallest free-space share observed during the free-space step, in
    /// percent; for a root that started below its floor this is the share
    /// before its deletions. `None` without a successful probe.
    pub free_pct_min: Option<f64>,
    /// Catalogued bytes after the quota step, when a quota is configured; age
    /// expiry later in the same pass is not subtracted.
    pub total_bytes: Option<u64>,
}

/// Free space as a percentage of the root's capacity.
#[must_use]
pub(crate) fn free_percent(space: &FreeSpace) -> f64 {
    if space.total == 0 {
        0.0
    } else {
        space.available as f64 / space.total as f64 * 100.0
    }
}

/// Whether a root is below its free-space floor.
fn needs_space(space: &FreeSpace, min_free_percent: u8) -> bool {
    u128::from(space.available) * 100 < u128::from(space.total) * u128::from(min_free_percent)
}

/// Whether the catalogued recordings exceed the quota, if one is set.
fn over_quota(total_bytes: u64, quota_bytes: Option<u64>) -> bool {
    quota_bytes.is_some_and(|quota| total_bytes > quota)
}

/// The cutoff before which a camera's sealed segments expire, if it has an
/// age limit; `None` when the age does not fit the wall clock.
fn expiry_cutoff(now: WallUs, retention_days: Option<u32>) -> Option<WallUs> {
    let days = retention_days?;
    let span = i64::from(days).checked_mul(DAY_US)?;
    now.get().checked_sub(span).map(WallUs::new)
}

/// The next raw-bounded page of deletable sealed segments after `cursor`,
/// oldest first, with rows under active event protection already excluded.
/// `None` means the indexed scan is exhausted. Callers commit progress as
/// they consume rows.
async fn deletable_batch(
    catalog: &CatalogHandle,
    cursor: Option<SegmentCursor>,
    root: Option<&Path>,
    now: WallUs,
    tally: &mut Tally,
) -> anyhow::Result<Option<CandidatePage>> {
    let page = catalog
        .retention_candidates(root, cursor, now, RETAIN_MARGIN, BATCH)
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

/// One retention pass over the roots admitted at daemon startup: event
/// maintenance, a bounded retry of earlier deletion intents, then the
/// free-space floor of each root, the quota, and the age limits. Logs the
/// pass when it deleted or finished something or ran slowly.
///
/// A root whose free-space probe fails is skipped for the pass: no row
/// beneath it is marked, the other roots' steps run, and the root is counted
/// in [`PassReport::roots_unavailable`]. Rows on roots that were not admitted
/// are left alone and counted in one warning.
///
/// # Errors
///
/// Fails when a catalog query fails; the state keeps its cursors for the
/// next pass.
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
    state.reconcile(&roots, min_free_percent, quota_bytes, age_limits);
    let started = Instant::now();
    let now = WallUs::now();
    catalog
        .maintain_events(now)
        .await
        .context("maintaining the event lifecycle")?;
    // A tombstone still counts toward the quota; finishing earlier intents first
    // keeps a phantom row from pushing the quota pass onto newer footage.
    let finished_deletions = finish_deletion_intents(
        catalog,
        approved_roots,
        &mut state.deleting_cursor,
        &state.raw,
    )
    .await?;
    let mut tally = Tally::new(started, state.raw.clone());
    let root_start = state.root_start(&roots);
    let probed = approved_roots.to_vec();
    let probe_raw = state.raw.clone();
    let free = free_roots(
        FreeRootContext {
            catalog,
            now,
            approved_roots,
        },
        RootOrder {
            roots: &roots,
            start: root_start,
        },
        min_free_percent,
        &mut tally,
        &mut state.root_cursors,
        move |root| {
            let approved = probed
                .iter()
                .find(|approved| approved.path() == root)
                .cloned();
            let raw = probe_raw.clone();
            async move {
                match approved {
                    Some(approved) => probe_free_space(approved, &raw).await,
                    None => Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "retention root was not admitted",
                    )),
                }
            }
        },
    )
    .await?;
    tally.unavailable_roots = free.unavailable;
    let total_bytes = match quota_bytes {
        Some(quota) => Some(
            trim_to_quota(
                catalog,
                quota,
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
        now,
        &mut tally,
        approved_roots,
    )
    .await?;
    if tally.unavailable_rows > 0 {
        warn!(
            rows = tally.unavailable_rows,
            "candidate rows on unavailable or unapproved recording roots were left alone"
        );
    }
    let report = PassReport {
        deleted: tally.deleted,
        freed: tally.freed,
        finished_deletions,
        roots_unavailable: tally.unavailable_roots.len(),
        duration: started.elapsed(),
        free_pct_min: free.free_pct_min,
        total_bytes,
    };
    log_pass(&report, quota_bytes);
    Ok(report)
}

/// Deletions made so far in a pass, and the roots the pass must leave alone.
struct Tally {
    started: Instant,
    attempted: usize,
    deleted: usize,
    freed: u64,
    pages: usize,
    rows: usize,
    raw: RawCalls,
    /// Roots whose free-space probe failed this pass; no row beneath them is
    /// marked.
    unavailable_roots: Vec<PathBuf>,
    /// Candidate rows left alone because their root is unapproved or
    /// unavailable this pass.
    unavailable_rows: usize,
}

impl Tally {
    fn new(started: Instant, raw: RawCalls) -> Self {
        Self {
            started,
            attempted: 0,
            deleted: 0,
            freed: 0,
            pages: 0,
            rows: 0,
            raw,
            unavailable_roots: Vec::new(),
            unavailable_rows: 0,
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

    /// Attempts one deletion and records its outcome; true when the pair is
    /// gone.
    async fn delete(
        &mut self,
        catalog: &CatalogHandle,
        row: &SegmentRow,
        approved_roots: &[ApprovedRoot],
    ) -> bool {
        self.attempted += 1;
        match delete(
            catalog,
            row,
            approved_roots,
            &self.unavailable_roots,
            &self.raw,
        )
        .await
        {
            DeleteOutcome::Deleted(freed) => {
                self.freed += freed;
                self.deleted += 1;
                true
            }
            DeleteOutcome::RootUnavailable => {
                self.unavailable_rows += 1;
                false
            }
            DeleteOutcome::Kept => false,
        }
    }
}

/// Deletes the oldest sealed segments while a root is below its free-space
/// floor. A root whose probe fails is skipped with a warning and reported in
/// the outcome; the other roots are still handled.
async fn free_roots<F, Probe>(
    context: FreeRootContext<'_>,
    order: RootOrder<'_>,
    min_free_percent: u8,
    tally: &mut Tally,
    cursors: &mut HashMap<PathBuf, Option<SegmentCursor>>,
    mut inspect_space: F,
) -> anyhow::Result<FreeRootsOutcome>
where
    F: FnMut(PathBuf) -> Probe,
    Probe: Future<Output = io::Result<FreeSpace>>,
{
    let mut outcome = FreeRootsOutcome::default();
    for offset in 0..order.roots.len() {
        let root = &order.roots[(order.start + offset) % order.roots.len()];
        let cursor = cursors.entry(root.clone()).or_default();
        'root: loop {
            let space = match inspect_space(root.clone()).await {
                Ok(space) => space,
                Err(error) => {
                    skip_root(root, &error, &mut outcome);
                    break 'root;
                }
            };
            outcome.observe(&space);
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
                warn!(
                    root = %root.display(),
                    "below the free-space floor with nothing left to delete"
                );
                break;
            };
            let mut complete = true;
            for row in &page.rows {
                if tally.full() {
                    complete = false;
                    break;
                }
                tally
                    .delete(context.catalog, row, context.approved_roots)
                    .await;
                *cursor = Some(SegmentCursor::from(row));

                let space = match inspect_space(root.clone()).await {
                    Ok(space) => space,
                    Err(error) => {
                        skip_root(root, &error, &mut outcome);
                        break 'root;
                    }
                };
                outcome.observe(&space);
                if !needs_space(&space, min_free_percent) {
                    *cursor = None;
                    break 'root;
                }
            }
            if complete {
                *cursor = Some(page.end);
            }
        }
    }
    Ok(outcome)
}

fn skip_root(root: &Path, error: &io::Error, outcome: &mut FreeRootsOutcome) {
    warn!(
        root = %root.display(),
        %error,
        "cannot probe free space; skipping this root's retention for the pass"
    );
    outcome.unavailable.push(root.to_path_buf());
}

/// Probes `root`'s free space through its identity-verified descriptor on a
/// counted thread.
async fn probe_free_space(root: ApprovedRoot, raw: &RawCalls) -> io::Result<FreeSpace> {
    probe_free_space_with(root, raw, |root| free_space_approved(&root)).await
}

async fn probe_free_space_with<F>(
    root: ApprovedRoot,
    raw: &RawCalls,
    probe: F,
) -> io::Result<FreeSpace>
where
    F: FnOnce(ApprovedRoot) -> io::Result<FreeSpace> + Send + 'static,
{
    let (result_tx, result_rx) = oneshot::channel();
    // A blocked `statvfs` cannot be cancelled safely. If the pass times out the
    // counted thread outlives it, and the retention owner starts no other pass
    // until the call returns; the thread cannot hold process exit. Its last
    // action is the send, so nothing waits for its exit.
    raw.spawn("irisd-retention-probe", move || {
        let _ = result_tx.send(probe(root));
    })?;
    result_rx
        .await
        .map_err(|err| io::Error::other(format!("free-space probe stopped: {err}")))?
}

/// Deletes the oldest sealed segments while the catalogued bytes exceed
/// `quota`. Returns the catalogued bytes afterwards.
async fn trim_to_quota(
    catalog: &CatalogHandle,
    quota: u64,
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
            if tally.full() {
                complete = false;
                break;
            }
            let bytes = row.bytes;
            if tally.delete(catalog, row, approved_roots).await {
                total = total.saturating_sub(bytes);
            }
            *cursor = Some(SegmentCursor::from(row));
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
                .expired_candidates(key, cutoff, scan.cursor, now, RETAIN_MARGIN, BATCH)
                .await
                .with_context(|| format!("listing expired segments of {key}"))?;
            tally.note_page(page.scanned);
            let Some(end) = page.next else {
                scan.cursor = None;
                break;
            };
            let mut complete = true;
            for row in &page.rows {
                if tally.full() {
                    complete = false;
                    break;
                }
                tally.delete(catalog, row, approved_roots).await;
                scan.cursor = Some(SegmentCursor::from(row));
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

/// Logs a pass that deleted or finished something; a slow pass is a warning
/// whatever it did. Unknown values (no quota, no successful probe) read `-`.
fn log_pass(report: &PassReport, quota_bytes: Option<u64>) {
    let free_pct = report
        .free_pct_min
        .map_or_else(|| "-".to_owned(), |pct| format!("{pct:.1}"));
    if report.duration >= SLOW_PASS {
        warn!(
            deleted = report.deleted,
            finished_deletions = report.finished_deletions,
            freed_mib = report.freed >> 20,
            ms = report.duration.as_millis(),
            free_pct = %free_pct,
            total_gib = %gib(report.total_bytes),
            quota_gib = %gib(quota_bytes),
            roots_unavailable = report.roots_unavailable,
            "slow retention pass"
        );
    } else if report.deleted > 0 || report.finished_deletions > 0 {
        info!(
            deleted = report.deleted,
            finished_deletions = report.finished_deletions,
            freed_mib = report.freed >> 20,
            ms = report.duration.as_millis(),
            free_pct = %free_pct,
            total_gib = %gib(report.total_bytes),
            quota_gib = %gib(quota_bytes),
            roots_unavailable = report.roots_unavailable,
            "retention pass"
        );
    }
}

/// Bytes as gibibytes with two decimals, `-` when unknown.
fn gib(bytes: Option<u64>) -> String {
    bytes.map_or_else(
        || "-".to_owned(),
        |bytes| format!("{:.2}", bytes as f64 / f64::from(1u32 << 30)),
    )
}

/// The outcome of one deletion attempt.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DeleteOutcome {
    /// The pair is gone and the row removed; the bytes freed.
    Deleted(u64),
    /// The row's root is not approved, or its probe failed this pass; the row
    /// was left alone.
    RootUnavailable,
    /// The row was kept: protected, outside its layout, or a step failed and
    /// left a retryable deletion intent.
    Kept,
}

/// Commits deletion intent, removes one segment's pair durably, and then
/// removes its catalog row. A row whose root is in `unavailable`, or whose
/// path is outside its root's recording layout, is never marked.
async fn delete(
    catalog: &CatalogHandle,
    row: &SegmentRow,
    approved_roots: &[ApprovedRoot],
    unavailable: &[PathBuf],
    raw: &RawCalls,
) -> DeleteOutcome {
    delete_with(
        catalog,
        row,
        approved_roots,
        unavailable,
        raw,
        |root, path| remove_segment_beneath(&root, &path),
    )
    .await
}

async fn delete_with<F>(
    catalog: &CatalogHandle,
    row: &SegmentRow,
    approved_roots: &[ApprovedRoot],
    unavailable: &[PathBuf],
    raw: &RawCalls,
    remove: F,
) -> DeleteOutcome
where
    F: FnOnce(ApprovedRoot, PathBuf) -> io::Result<u64> + Send + 'static,
{
    let persisted_root = match catalog.segment_root(row.id).await {
        Ok(Some(root)) => root,
        Ok(None) => {
            warn!(path = %row.path.display(), "catalog row has no recording-root ownership");
            return DeleteOutcome::Kept;
        }
        Err(err) => {
            warn!(path = %row.path.display(), %err, "cannot read catalog row ownership");
            return DeleteOutcome::Kept;
        }
    };
    let Some(approved) = approved_roots
        .iter()
        .find(|root| root.path() == persisted_root)
        .cloned()
    else {
        debug!(
            path = %row.path.display(),
            root = %persisted_root.display(),
            "catalog row belongs to an unapproved recording root; left alone"
        );
        return DeleteOutcome::RootUnavailable;
    };
    if unavailable.iter().any(|root| root == approved.path()) {
        debug!(
            path = %row.path.display(),
            root = %approved.path().display(),
            "recording root is unavailable this pass; left alone"
        );
        return DeleteOutcome::RootUnavailable;
    }
    // Pair removal and startup only accept layout paths; marking any other row
    // would leave a tombstone that can never finish.
    if parse_segment_path(approved.path(), &row.path).as_ref() != Some(&row.key) {
        warn!(
            path = %row.path.display(),
            root = %approved.path().display(),
            "catalog row is outside its root's recording layout; not deleting it"
        );
        return DeleteOutcome::Kept;
    }

    match catalog
        .mark_deleting_if_unprotected_at_root(row.id, approved.path(), WallUs::now(), RETAIN_MARGIN)
        .await
    {
        Ok(DeleteMarkResult::Marked) => {}
        Ok(DeleteMarkResult::Protected) => return DeleteOutcome::Kept,
        Ok(DeleteMarkResult::NotFound) => {
            warn!(path = %row.path.display(), "catalog row disappeared before deletion intent");
            return DeleteOutcome::Kept;
        }
        Err(err) => {
            warn!(path = %row.path.display(), %err, "cannot mark catalog row deleting");
            return DeleteOutcome::Kept;
        }
    }
    let Some(freed) = remove_pair_with(approved, &row.path, raw, remove).await else {
        return DeleteOutcome::Kept;
    };
    if let Err(err) = catalog.delete_segment(row.id).await {
        warn!(path = %row.path.display(), %err, "cannot delete catalog row");
        return DeleteOutcome::Kept;
    }
    DeleteOutcome::Deleted(freed)
}

/// Removes one recording pair beneath its approved root on a counted raw
/// thread and returns the bytes freed. `None` leaves the deletion intent for
/// a retry.
async fn remove_pair(approved: ApprovedRoot, path: &Path, raw: &RawCalls) -> Option<u64> {
    remove_pair_with(approved, path, raw, |root, path| {
        remove_segment_beneath(&root, &path)
    })
    .await
}

async fn remove_pair_with<F>(
    approved: ApprovedRoot,
    path: &Path,
    raw: &RawCalls,
    remove: F,
) -> Option<u64>
where
    F: FnOnce(ApprovedRoot, PathBuf) -> io::Result<u64> + Send + 'static,
{
    let (result_tx, result_rx) = oneshot::channel();
    let removal_path = path.to_path_buf();
    // Unlink and the directory durability barrier can block in the kernel.
    // Deletion intent remains durable if a timeout detaches this one call; the
    // owner starts no other pass until it returns, and a later pass or startup
    // finishes the row idempotently. The thread's last action is the send, so
    // nothing waits for its exit.
    if let Err(err) = raw.spawn("irisd-retention-remove", move || {
        let _ = result_tx.send(remove(approved, removal_path));
    }) {
        warn!(path = %path.display(), %err, "cannot start segment removal");
        return None;
    }
    match result_rx.await {
        Ok(Ok(bytes)) => Some(bytes),
        Ok(Err(err)) => {
            warn!(path = %path.display(), %err, "cannot remove segment files");
            None
        }
        Err(_) => {
            warn!(path = %path.display(), "removal thread stopped without a result");
            None
        }
    }
}

/// Retries one bounded page of durable deletion intents beneath approved roots:
/// at most [`DELETION_RETRY_ROWS`] rows or [`DELETION_RETRY_BUDGET`], continuing
/// after `cursor` and wrapping after the last intent. A cleared fault, or a
/// tombstone whose files are already absent, therefore stops counting toward
/// the quota without a restart. Rows on unavailable roots or outside the
/// recording layout remain for startup; failures remain for a later pass.
async fn finish_deletion_intents(
    catalog: &CatalogHandle,
    approved_roots: &[ApprovedRoot],
    cursor: &mut Option<SegmentCursor>,
    raw: &RawCalls,
) -> anyhow::Result<usize> {
    if approved_roots.is_empty() {
        return Ok(0);
    }
    let started = Instant::now();
    let rows = catalog
        .deleting_segments_page(*cursor, DELETION_RETRY_ROWS)
        .await
        .context("listing deletion intents")?;
    let exhausted = rows.len() < DELETION_RETRY_ROWS;
    let mut complete = true;
    let mut finished = 0;
    for StartupRow {
        row,
        root: persisted_root,
    } in rows
    {
        if started.elapsed() >= DELETION_RETRY_BUDGET {
            complete = false;
            break;
        }
        *cursor = Some(SegmentCursor::from(&row));
        let Some(root) = approved_root_for_row(approved_roots, persisted_root.as_deref(), &row)
        else {
            debug!(
                path = %row.path.display(),
                "deletion intent is not beneath an approved root's layout; leaving it for startup"
            );
            continue;
        };
        let Some(freed) = remove_pair(root.clone(), &row.path, raw).await else {
            continue;
        };
        match catalog.delete_segment(row.id).await {
            Ok(()) => {
                finished += 1;
                info!(path = %row.path.display(), freed, "finished interrupted segment deletion");
            }
            Err(err) => warn!(path = %row.path.display(), %err, "cannot delete catalog row"),
        }
    }
    if complete && exhausted {
        *cursor = None;
    }
    Ok(finished)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    use iris_catalog::SegmentId;
    use iris_core::event::{Event, EventKind, EventSource, EventState};
    use iris_store::{RootExpectation, admit_recording_root, free_space};

    use super::*;

    /// Releases a gated raw-call worker when dropped, so a failing test never
    /// leaves one blocked.
    struct ReleaseOnDrop(Option<std::sync::mpsc::Sender<()>>);

    impl Drop for ReleaseOnDrop {
        fn drop(&mut self) {
            if let Some(release) = self.0.take() {
                let _ = release.send(());
            }
        }
    }

    /// A catalog with one admitted, configured recording root.
    async fn approved_catalog(dir: &Path) -> (CatalogHandle, PathBuf, ApprovedRoot) {
        let catalog = CatalogHandle::open(&dir.join("catalog.db")).unwrap();
        let root = dir.join("video");
        let approved = admit_recording_root(&root, RootExpectation::Fresh).unwrap();
        catalog
            .configure_video_roots(std::slice::from_ref(&root))
            .await
            .unwrap();
        (catalog, root, approved)
    }

    /// A catalog with two admitted, configured recording roots.
    async fn two_root_catalog(dir: &Path) -> (CatalogHandle, [PathBuf; 2], [ApprovedRoot; 2]) {
        let catalog = CatalogHandle::open(&dir.join("catalog.db")).unwrap();
        let roots = [dir.join("first"), dir.join("second")];
        let approved = [
            admit_recording_root(&roots[0], RootExpectation::Fresh).unwrap(),
            admit_recording_root(&roots[1], RootExpectation::Fresh).unwrap(),
        ];
        catalog.configure_video_roots(&roots).await.unwrap();
        (catalog, roots, approved)
    }

    /// A layout path for `camera`'s main stream beneath `root`.
    fn layout_path(root: &Path, camera: &str, index: usize) -> PathBuf {
        root.join(camera)
            .join("main/1970-01-01")
            .join(format!("{index:06}Z.mp4"))
    }

    async fn seed_segment(catalog: &CatalogHandle, path: &Path, camera: &str) {
        seed_segment_with_bytes(catalog, path, camera, b"x").await;
    }

    async fn seed_segment_with_bytes(
        catalog: &CatalogHandle,
        path: &Path,
        camera: &str,
        bytes: &[u8],
    ) {
        seed_segment_starting(catalog, path, camera, bytes, 1_000_000).await;
    }

    /// Writes `bytes` at `path` and adopts it as a one-second sealed recording
    /// starting at `wall_start`.
    async fn seed_segment_starting(
        catalog: &CatalogHandle,
        path: &Path,
        camera: &str,
        bytes: &[u8],
        wall_start: i64,
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
                wall_start: WallUs::new(wall_start),
                wall_end: WallUs::new(wall_start + 1_000_000),
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

    /// Retains an event of `camera` at 1.5 s for ever, so its rows between
    /// 1 s and 2 s stay protected.
    async fn retain_forever(catalog: &CatalogHandle, camera: &str) {
        let id = catalog
            .insert_event(&Event {
                camera: CameraId::try_new(camera).unwrap(),
                source: EventSource::Manual,
                kind: EventKind::Motion,
                state: EventState::Pulse,
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
    }

    fn state() -> RetentionState {
        RetentionState::new(RawCalls::default())
    }

    fn tally() -> Tally {
        Tally::new(Instant::now(), RawCalls::default())
    }

    async fn finish_empty_age_scan(
        catalog: &CatalogHandle,
        key: &StreamKey,
        cursors: &mut HashMap<StreamKey, AgeScan>,
        now: WallUs,
        approved: &[ApprovedRoot],
    ) {
        if cursors[key].cursor.is_none() {
            return;
        }
        let mut tally = tally();
        expire_by_age(
            catalog,
            std::slice::from_ref(key),
            cursors,
            now,
            &mut tally,
            approved,
        )
        .await
        .unwrap();
        assert_eq!(tally.deleted, 0);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cancelled_retention_bounds_and_reaps_its_single_raw_removal_tail() {
        let dir = tempfile::tempdir().unwrap();
        let (catalog, root, approved) = approved_catalog(dir.path()).await;
        seed_segment(&catalog, &layout_path(&root, "cam", 0), "cam").await;
        let row = catalog.oldest_sealed(1).await.unwrap().remove(0);
        let active = Arc::new(AtomicBool::new(false));
        let worker_active = Arc::clone(&active);
        let (started_tx, started_rx) = std::sync::mpsc::sync_channel(0);
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let mut release = ReleaseOnDrop(Some(release_tx));
        let catalog_for_task = catalog.clone();
        let raw = RawCalls::default();
        let task_raw = raw.clone();
        let task = tokio::spawn(async move {
            delete_with(
                &catalog_for_task,
                &row,
                std::slice::from_ref(&approved),
                &[],
                &task_raw,
                move |_, _| {
                    worker_active.store(true, Ordering::SeqCst);
                    let _ = started_tx.send(());
                    let _ = release_rx.recv();
                    worker_active.store(false, Ordering::SeqCst);
                    Ok(0)
                },
            )
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
        assert_eq!(raw.in_flight(), 1, "the detached removal stays counted");
        release.0.take().unwrap().send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(1), async {
            while active.load(Ordering::SeqCst) || raw.in_flight() != 0 {
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
        let dir = tempfile::tempdir().unwrap();
        let approved =
            admit_recording_root(&dir.path().join("video"), RootExpectation::Fresh).unwrap();
        let active = Arc::new(AtomicBool::new(false));
        let calls = Arc::new(AtomicUsize::new(0));
        let worker_active = Arc::clone(&active);
        let worker_calls = Arc::clone(&calls);
        let (started_tx, started_rx) = std::sync::mpsc::sync_channel(0);
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let mut release = ReleaseOnDrop(Some(release_tx));
        let raw = RawCalls::default();
        let task_raw = raw.clone();
        let mut task = tokio::spawn(async move {
            probe_free_space_with(approved, &task_raw, move |_| {
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
        assert_eq!(raw.in_flight(), 1, "the detached statvfs stays counted");
        release.0.take().unwrap().send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(1), async {
            while active.load(Ordering::SeqCst) || raw.in_flight() != 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("released statvfs tail must exit");
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_replaced_root_is_skipped_for_the_pass_without_marking_its_rows() {
        let dir = tempfile::tempdir().unwrap();
        let (catalog, root, approved) = approved_catalog(dir.path()).await;
        let displaced = dir.path().join("displaced-video");
        let ordinary_space = probe_free_space(approved.clone(), &RawCalls::default())
            .await
            .unwrap();
        assert!(ordinary_space.total > 0);
        let recording = layout_path(&root, "cam", 0);
        seed_segment_with_bytes(&catalog, &recording, "cam", b"original recording").await;

        std::fs::rename(&root, &displaced).unwrap();
        std::fs::create_dir(&root).unwrap();
        let mut state = state();
        let report = pass_approved(
            &catalog,
            std::slice::from_ref(&approved),
            &mut state,
            0,
            Some(0),
            &HashMap::new(),
        )
        .await
        .unwrap();
        assert_eq!(report.roots_unavailable, 1);
        assert_eq!(report.deleted, 0);
        assert_eq!(report.free_pct_min, None);
        assert_eq!(catalog.deleting_segments().await.unwrap(), Vec::new());
        assert_eq!(catalog.oldest_sealed(2).await.unwrap().len(), 1);

        std::fs::remove_dir(&root).unwrap();
        std::fs::rename(&displaced, &root).unwrap();
        assert_eq!(std::fs::read(&recording).unwrap(), b"original recording");
        catalog.shutdown();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_failed_probe_on_one_root_leaves_the_other_roots_retention_running() {
        let dir = tempfile::tempdir().unwrap();
        let (catalog, roots, approved) = two_root_catalog(dir.path()).await;
        let kept = layout_path(&roots[0], "cam", 0);
        let deletable = layout_path(&roots[1], "cam", 1);
        seed_segment(&catalog, &kept, "cam").await;
        seed_segment(&catalog, &deletable, "cam").await;
        let displaced = dir.path().join("displaced");
        std::fs::rename(&roots[0], &displaced).unwrap();
        std::fs::create_dir(&roots[0]).unwrap();

        let mut state = state();
        let report = pass_approved(&catalog, &approved, &mut state, 0, Some(0), &HashMap::new())
            .await
            .unwrap();
        assert_eq!(report.roots_unavailable, 1);
        assert_eq!(report.deleted, 1, "the healthy root's quota deletion ran");
        assert!(!deletable.exists());
        assert!(report.free_pct_min.is_some(), "the healthy root was probed");
        assert_eq!(
            catalog.deleting_segments().await.unwrap(),
            Vec::new(),
            "no row of the displaced root was marked"
        );
        std::fs::remove_dir(&roots[0]).unwrap();
        std::fs::rename(&displaced, &roots[0]).unwrap();
        assert!(kept.exists());
        catalog.shutdown();
    }

    #[tokio::test]
    async fn quota_reaches_unprotected_segments_with_tied_start_times() {
        let dir = tempfile::tempdir().unwrap();
        let (catalog, root, approved) = approved_catalog(dir.path()).await;
        for i in 0..=BATCH {
            let camera = if i < BATCH { "protected" } else { "deletable" };
            seed_segment(&catalog, &layout_path(&root, camera, i), camera).await;
        }
        retain_forever(&catalog, "protected").await;
        let mut state = state();
        let result = pass_approved(
            &catalog,
            std::slice::from_ref(&approved),
            &mut state,
            0,
            Some(BATCH as u64),
            &HashMap::new(),
        )
        .await
        .unwrap();
        assert_eq!(result.deleted, 1);
        assert_eq!(catalog.total_bytes().await.unwrap(), BATCH as u64);
        assert!(!layout_path(&root, "deletable", BATCH).exists());
        catalog.shutdown();
    }

    #[tokio::test]
    async fn quota_does_not_starve_older_rows_under_sustained_arrivals() {
        let dir = tempfile::tempdir().unwrap();
        let (catalog, root, approved) = approved_catalog(dir.path()).await;
        let path = |camera: &str, index: usize| layout_path(&root, camera, index);
        for index in 1..=BATCH + 1 {
            seed_segment(&catalog, &path("cam", index), "cam").await;
        }
        retain_forever(&catalog, "sentinel").await;
        let approved = std::slice::from_ref(&approved);

        let mut state = state();
        let first = pass_approved(
            &catalog,
            approved,
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
            seed_segment_with_bytes(&catalog, &path("sentinel", sentinel), "sentinel", b"").await;
            seed_segment(&catalog, &path("cam", arrival), "cam").await;
            first_arrival.get_or_insert(arrival);
            last_arrival = Some(arrival);
            let report = pass_approved(
                &catalog,
                approved,
                &mut state,
                0,
                Some(BATCH as u64),
                &HashMap::new(),
            )
            .await
            .unwrap();
            assert_eq!(report.deleted, 1, "cycle {cycle}");
            assert_eq!(state.quota_cursor, None, "cycle {cycle}");
        }

        for index in 2..=BATCH + 1 {
            assert!(
                !path("cam", index).exists(),
                "older segment {index} was starved"
            );
        }
        assert!(!path("cam", first_arrival.unwrap()).exists());
        assert!(path("cam", last_arrival.unwrap()).exists());
        assert_eq!(catalog.total_bytes().await.unwrap(), BATCH as u64);
        catalog.shutdown();
    }

    #[tokio::test]
    async fn free_space_retention_only_deletes_from_the_pressured_root() {
        let dir = tempfile::tempdir().unwrap();
        let (catalog, roots, approved) = two_root_catalog(dir.path()).await;
        let [pressured, other] = &roots;
        for index in 0..128 {
            seed_segment(&catalog, &layout_path(other, "cam", index), "cam").await;
        }
        let local = layout_path(pressured, "cam", 0);
        seed_segment(&catalog, &local, "cam").await;
        let mut state = state();
        let result = pass_approved(
            &catalog,
            std::slice::from_ref(&approved[0]),
            &mut state,
            100,
            None,
            &HashMap::new(),
        )
        .await
        .unwrap();
        assert_eq!(result.deleted, 1);
        assert!(layout_path(other, "cam", 0).exists());
        assert!(layout_path(other, "cam", 127).exists());
        assert!(!local.exists());
        catalog.shutdown();
    }

    #[tokio::test]
    async fn free_space_resumes_at_oldest_row_after_partial_page_satisfaction() {
        let dir = tempfile::tempdir().unwrap();
        let (catalog, root, approved) = approved_catalog(dir.path()).await;
        let roots = vec![root.clone()];
        let paths: Vec<PathBuf> = (0..3).map(|i| layout_path(&root, "cam", i)).collect();
        for path in &paths {
            seed_segment(&catalog, path, "cam").await;
        }

        let mut cursors = HashMap::new();
        for expected in &paths[..2] {
            let mut checks = 0;
            let mut tally = tally();
            free_roots(
                FreeRootContext {
                    catalog: &catalog,
                    now: WallUs::now(),
                    approved_roots: std::slice::from_ref(&approved),
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
        assert!(paths[2].exists());
        catalog.shutdown();
    }

    #[tokio::test]
    async fn pressured_roots_take_turns_when_the_global_budget_is_exhausted() {
        let dir = tempfile::tempdir().unwrap();
        let (catalog, roots, approved) = two_root_catalog(dir.path()).await;
        let first_file = layout_path(&roots[0], "cam", 0);
        let second_file = layout_path(&roots[1], "cam", 1);
        seed_segment(&catalog, &first_file, "cam").await;
        seed_segment(&catalog, &second_file, "cam").await;
        let roots = roots.to_vec();

        let mut first_tally = tally();
        first_tally.attempted = MAX_PER_PASS - 1;
        let mut cursors = HashMap::new();
        free_roots(
            FreeRootContext {
                catalog: &catalog,
                now: WallUs::now(),
                approved_roots: &approved,
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

        let mut second_tally = tally();
        second_tally.attempted = MAX_PER_PASS - 1;
        free_roots(
            FreeRootContext {
                catalog: &catalog,
                now: WallUs::now(),
                approved_roots: &approved,
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
        let (catalog, root, approved) = approved_catalog(dir.path()).await;
        let segments = MAX_ROWS_PER_PASS + 1;
        for index in 0..segments {
            let camera = if index < MAX_ROWS_PER_PASS {
                "protected"
            } else {
                "deletable"
            };
            seed_segment(&catalog, &layout_path(&root, camera, index), camera).await;
        }
        retain_forever(&catalog, "protected").await;
        let approved = std::slice::from_ref(&approved);
        let before = catalog.metrics().query().snapshot();
        let mut state = state();
        let first = pass_approved(&catalog, approved, &mut state, 0, Some(0), &HashMap::new())
            .await
            .unwrap();
        let queries = catalog.metrics().query().snapshot().since(&before).count();
        assert_eq!(first.deleted, 0);
        assert_eq!(catalog.total_bytes().await.unwrap(), segments as u64);
        assert_eq!(
            state.quota_cursor,
            Some(SegmentCursor {
                wall_start: WallUs::new(1_000_000),
                id: SegmentId(MAX_ROWS_PER_PASS as i64),
            })
        );
        assert!(
            queries <= u64::try_from(MAX_PAGES_PER_PASS * 2 + 4).unwrap(),
            "bounded pass issued {queries} catalog queries"
        );

        let second = pass_approved(&catalog, approved, &mut state, 0, Some(0), &HashMap::new())
            .await
            .unwrap();
        assert_eq!(second.deleted, 1);
        assert_eq!(
            catalog.total_bytes().await.unwrap(),
            MAX_ROWS_PER_PASS as u64
        );
        assert_eq!(state.quota_cursor, None, "true exhaustion wraps the cursor");
        assert!(!layout_path(&root, "deletable", MAX_ROWS_PER_PASS).exists());

        let third = pass_approved(&catalog, approved, &mut state, 0, Some(0), &HashMap::new())
            .await
            .unwrap();
        assert_eq!(third.deleted, 0);
        assert_eq!(
            state.quota_cursor,
            Some(SegmentCursor {
                wall_start: WallUs::new(1_000_000),
                id: SegmentId(MAX_ROWS_PER_PASS as i64),
            })
        );
        catalog.shutdown();
    }

    #[tokio::test]
    async fn age_resumes_after_deleted_cursor_and_wraps_only_after_exhaustion() {
        let dir = tempfile::tempdir().unwrap();
        let (catalog, root, approved) = approved_catalog(dir.path()).await;
        let approved = std::slice::from_ref(&approved);
        let paths: Vec<PathBuf> = (0..3).map(|i| layout_path(&root, "cam", i)).collect();
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
            let mut tally = tally();
            tally.attempted = MAX_PER_PASS - 1;
            expire_by_age(&catalog, &keys, &mut cursors, now, &mut tally, approved)
                .await
                .unwrap();
            assert_eq!(tally.deleted, 1);
            assert!(!paths[index].exists());
            assert_eq!(
                cursors[&key].cursor,
                Some(SegmentCursor::from(&rows[index]))
            );
            assert!(paths[index + 1].exists());
        }

        let mut exhaustion_tally = tally();
        expire_by_age(
            &catalog,
            &keys,
            &mut cursors,
            now,
            &mut exhaustion_tally,
            approved,
        )
        .await
        .unwrap();
        assert_eq!(exhaustion_tally.deleted, 1);
        assert!(!paths[2].exists());
        finish_empty_age_scan(&catalog, &key, &mut cursors, now, approved).await;
        assert_eq!(cursors[&key].cursor, None);

        let after_wrap = layout_path(&root, "cam", 9);
        seed_segment(&catalog, &after_wrap, "cam").await;
        let mut wrap_tally = tally();
        expire_by_age(
            &catalog,
            &keys,
            &mut cursors,
            now,
            &mut wrap_tally,
            approved,
        )
        .await
        .unwrap();
        assert_eq!(wrap_tally.deleted, 1);
        assert!(!after_wrap.exists());
        finish_empty_age_scan(&catalog, &key, &mut cursors, now, approved).await;
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
        let mut state = state();
        state.reconcile(&[first.clone(), second.clone()], 5, Some(10), &ages);
        let cursor = Some(SegmentCursor {
            wall_start: WallUs::new(10),
            id: SegmentId(10),
        });
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
    async fn a_failed_pair_removal_does_not_claim_deletion_or_quota_progress() {
        let dir = tempfile::tempdir().unwrap();
        let (catalog, root, approved) = approved_catalog(dir.path()).await;
        let path = layout_path(&root, "cam", 0);
        seed_segment(&catalog, &path, "cam").await;
        let index = path.with_extension("idx");
        std::fs::create_dir(&index).unwrap();

        let mut state = state();
        let report = pass_approved(
            &catalog,
            std::slice::from_ref(&approved),
            &mut state,
            0,
            Some(0),
            &HashMap::new(),
        )
        .await
        .unwrap();
        assert_eq!(report.deleted, 0);
        assert_eq!(report.freed, 0);
        assert_eq!(report.total_bytes, Some(1));
        assert!(
            path.exists(),
            "a pair that cannot be removed whole is left in place"
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
    async fn a_cleared_fault_finishes_the_tombstone_without_restart() {
        let dir = tempfile::tempdir().unwrap();
        let (catalog, root, approved) = approved_catalog(dir.path()).await;
        let path = layout_path(&root, "cam", 0);
        seed_segment(&catalog, &path, "cam").await;
        // A non-empty directory in place of the sidecar makes pair removal fail.
        let index = path.with_extension("idx");
        std::fs::create_dir(&index).unwrap();
        std::fs::write(index.join("blocker"), b"x").unwrap();
        let approved = std::slice::from_ref(&approved);
        let mut state = state();

        let report = pass_approved(&catalog, approved, &mut state, 0, Some(0), &HashMap::new())
            .await
            .unwrap();
        assert_eq!((report.deleted, report.finished_deletions), (0, 0));
        assert_eq!(catalog.deleting_segments().await.unwrap().len(), 1);
        assert_eq!(
            catalog.total_bytes().await.unwrap(),
            1,
            "the tombstone still counts"
        );
        let catalog_stats = catalog.stats().await.unwrap();
        assert_eq!(
            (
                catalog_stats.deleting_segments,
                catalog_stats.deleting_bytes
            ),
            (1, 1)
        );

        let report = pass_approved(&catalog, approved, &mut state, 0, Some(0), &HashMap::new())
            .await
            .unwrap();
        assert_eq!(
            report.finished_deletions, 0,
            "a persisting fault keeps the tombstone"
        );
        assert_eq!(catalog.deleting_segments().await.unwrap().len(), 1);

        std::fs::remove_dir_all(&index).unwrap();
        let report = pass_approved(&catalog, approved, &mut state, 0, Some(0), &HashMap::new())
            .await
            .unwrap();
        assert_eq!(report.finished_deletions, 1);
        assert_eq!(catalog.deleting_segments().await.unwrap(), Vec::new());
        assert_eq!(catalog.total_bytes().await.unwrap(), 0);
        assert_eq!(report.total_bytes, Some(0));
        assert!(!path.exists());
        catalog.shutdown();
    }

    #[tokio::test]
    async fn a_phantom_tombstone_is_reclaimed_before_the_quota_deletes_newer_footage() {
        let dir = tempfile::tempdir().unwrap();
        let (catalog, root, approved) = approved_catalog(dir.path()).await;
        let older = layout_path(&root, "cam", 1);
        let newest = layout_path(&root, "cam", 2);
        seed_segment_starting(&catalog, &older, "cam", &[0; 1_000], 1_000_000).await;
        seed_segment_starting(&catalog, &newest, "cam", &[0; 1_000], 2_000_000).await;
        // A deletion whose row removal failed after both unlinks leaves a row
        // in deletion intent whose files are already gone.
        let phantom = catalog.segment_by_path(&older).await.unwrap().unwrap();
        assert_eq!(
            catalog.mark_deleting(phantom.id).await.unwrap(),
            DeleteMarkResult::Marked
        );
        std::fs::remove_file(&older).unwrap();
        assert_eq!(catalog.total_bytes().await.unwrap(), 2_000);

        let mut state = state();
        let report = pass_approved(
            &catalog,
            std::slice::from_ref(&approved),
            &mut state,
            0,
            Some(1_000),
            &HashMap::new(),
        )
        .await
        .unwrap();
        assert_eq!(report.finished_deletions, 1);
        assert_eq!(
            report.deleted, 0,
            "the quota holds once the phantom is gone"
        );
        assert!(newest.exists(), "the newest recording survives");
        assert_eq!(report.total_bytes, Some(1_000));
        assert_eq!(catalog.oldest_sealed(4).await.unwrap().len(), 1);
        catalog.shutdown();
    }

    #[tokio::test]
    async fn rows_outside_the_recording_layout_are_never_marked() {
        let dir = tempfile::tempdir().unwrap();
        let (catalog, root, approved) = approved_catalog(dir.path()).await;
        let stray = root.join("stray.mp4");
        let shallow = root.join("cam/main/000000Z.mp4");
        let valid = layout_path(&root, "cam", 0);
        for path in [&stray, &shallow, &valid] {
            seed_segment(&catalog, path, "cam").await;
        }

        let mut state = state();
        let report = pass_approved(
            &catalog,
            std::slice::from_ref(&approved),
            &mut state,
            0,
            Some(0),
            &HashMap::new(),
        )
        .await
        .unwrap();
        assert_eq!(report.deleted, 1, "only the layout row is deleted");
        assert!(!valid.exists());
        assert!(stray.exists() && shallow.exists());
        assert_eq!(catalog.deleting_segments().await.unwrap(), Vec::new());
        let remaining: Vec<PathBuf> = catalog
            .oldest_sealed(4)
            .await
            .unwrap()
            .into_iter()
            .map(|row| row.path)
            .collect();
        assert_eq!(remaining.len(), 2);
        assert!(remaining.contains(&stray) && remaining.contains(&shallow));
        catalog.shutdown();
    }

    #[tokio::test]
    async fn rows_on_an_unapproved_root_are_left_alone_and_counted() {
        let dir = tempfile::tempdir().unwrap();
        let (catalog, roots, approved) = two_root_catalog(dir.path()).await;
        let unapproved = layout_path(&roots[0], "cam", 0);
        let deletable = layout_path(&roots[1], "cam", 1);
        seed_segment(&catalog, &unapproved, "cam").await;
        seed_segment(&catalog, &deletable, "cam").await;
        let mut tally = tally();
        let rows = catalog.oldest_sealed(2).await.unwrap();
        assert!(
            !tally
                .delete(&catalog, &rows[0], std::slice::from_ref(&approved[1]))
                .await
        );
        assert!(
            tally
                .delete(&catalog, &rows[1], std::slice::from_ref(&approved[1]))
                .await
        );
        assert_eq!((tally.unavailable_rows, tally.deleted), (1, 1));
        assert!(unapproved.exists());
        assert!(!deletable.exists());
        assert_eq!(catalog.deleting_segments().await.unwrap(), Vec::new());
        catalog.shutdown();
    }

    #[tokio::test]
    async fn the_deletion_intent_retry_is_bounded_and_wraps() {
        let dir = tempfile::tempdir().unwrap();
        let (catalog, root, approved) = approved_catalog(dir.path()).await;
        let count = DELETION_RETRY_ROWS + 4;
        for second in 0..count {
            let path = layout_path(&root, "cam", second);
            let start = 1_000_000 * i64::try_from(second + 1).unwrap();
            seed_segment_starting(&catalog, &path, "cam", b"x", start).await;
            let row = catalog.segment_by_path(&path).await.unwrap().unwrap();
            assert_eq!(
                catalog.mark_deleting(row.id).await.unwrap(),
                DeleteMarkResult::Marked
            );
        }
        let raw = RawCalls::default();
        let mut cursor = None;
        let approved = std::slice::from_ref(&approved);
        let first = finish_deletion_intents(&catalog, approved, &mut cursor, &raw)
            .await
            .unwrap();
        assert!(
            first <= DELETION_RETRY_ROWS,
            "one page per pass, finished {first}"
        );
        let mut finished = first;
        for _ in 0..count {
            if finished == count {
                break;
            }
            finished += finish_deletion_intents(&catalog, approved, &mut cursor, &raw)
                .await
                .unwrap();
        }
        assert_eq!(finished, count);
        assert_eq!(cursor, None, "the cursor wraps after the last intent");
        tokio::time::timeout(Duration::from_secs(1), async {
            while raw.in_flight() != 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("every removal thread exits");
        assert_eq!(catalog.deleting_segments().await.unwrap(), Vec::new());
        catalog.shutdown();
    }

    #[tokio::test]
    async fn a_deletion_intent_failure_prevents_either_unlink() {
        let dir = tempfile::tempdir().unwrap();
        let (catalog, root, approved) = approved_catalog(dir.path()).await;
        let path = layout_path(&root, "cam", 0);
        let index = path.with_extension("idx");
        seed_segment(&catalog, &path, "cam").await;
        std::fs::write(&index, b"index").unwrap();
        let row = catalog.oldest_sealed(1).await.unwrap().remove(0);

        catalog.shutdown();
        assert_eq!(
            delete(
                &catalog,
                &row,
                std::slice::from_ref(&approved),
                &[],
                &RawCalls::default()
            )
            .await,
            DeleteOutcome::Kept
        );
        assert!(path.exists(), "media must remain before durable intent");
        assert!(index.exists(), "index must remain before durable intent");
    }

    #[tokio::test]
    async fn retention_acknowledged_after_enumeration_prevents_the_atomic_mark() {
        let dir = tempfile::tempdir().unwrap();
        let (catalog, root, approved) = approved_catalog(dir.path()).await;
        let path = layout_path(&root, "cam", 0);
        seed_segment(&catalog, &path, "cam").await;
        let row = catalog
            .retention_candidates(None, None, WallUs::now(), RETAIN_MARGIN, 1)
            .await
            .unwrap()
            .rows
            .remove(0);

        retain_forever(&catalog, "cam").await;

        assert_eq!(
            delete(
                &catalog,
                &row,
                std::slice::from_ref(&approved),
                &[],
                &RawCalls::default()
            )
            .await,
            DeleteOutcome::Kept
        );
        assert!(path.exists());
        assert!(catalog.segment(row.id).await.unwrap().is_some());
        assert_eq!(catalog.deleting_segments().await.unwrap(), Vec::new());
        catalog.shutdown();
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
    fn the_expiry_cutoff_is_the_age_limit_before_now() {
        let now = WallUs::new(100 * DAY_US);
        assert_eq!(expiry_cutoff(now, Some(21)), Some(WallUs::new(79 * DAY_US)));
        assert_eq!(expiry_cutoff(now, None), None);
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
