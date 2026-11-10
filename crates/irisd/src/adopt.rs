//! Startup reconciliation between the catalog and the files on disk: finishing
//! deletion intents, recovering segments that were open when the daemon last
//! stopped, and adopting files the catalog does not know.
//!
//! Every phase pages the catalog in fixed pages, opens files only beneath the
//! approved root that owns them, runs its filesystem calls on tokio's blocking
//! pool, and reports its progress to systemd on a heartbeat that extends the
//! start timeout across awaits of any length.

use std::ffi::OsStr;
use std::fs::File;
use std::io;
use std::mem::MaybeUninit;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::Context;
use iris_catalog::{
    CatalogHandle, DeleteMarkResult, NewSegment, SegmentCursor, SegmentRow, StartupRow,
};
use iris_core::id::{CameraId, StreamKey, StreamKind};
use iris_core::time::{MediaTicks, WallUs};
use iris_mp4::index::{
    IndexHeader, IndexRecord, MAX_PERSISTED_INDEX_RECORDS, read_index_file_bound_to_media,
};
use iris_mp4::reader::{self, ScanOptions, ScanReport, ScanStop};
use iris_store::recover::{IndexBinding, RecoverError, RecoveryReport, recover_segment_beneath};
use iris_store::{
    ApprovedRoot, open_approved_root_directory, open_segment_beneath, open_segment_pair_beneath,
    remove_segment_beneath,
};
use rustix::fs::{AtFlags, FileType, Mode, OFlags, RawDir, ResolveFlags};
use tokio::sync::mpsc;
use tokio::time::{Instant, MissedTickBehavior};
use tracing::{info, warn};

use crate::systemd;

/// Files between two start-up status reports while adopting.
const ADOPT_STATUS_EVERY: usize = 50;
/// Extra start time requested from systemd at each status report.
const RECONCILE_TIMEOUT_EXTENSION: Duration = Duration::from_secs(60);
/// Catalog rows per reconciliation page.
const RECONCILE_PAGE: usize = 256;
/// Cadence of the status reports that keep the start timeout extended while
/// a reconciliation step runs.
const RECONCILE_HEARTBEAT: Duration = Duration::from_secs(5);
/// Directory levels below a root that the walk descends into:
/// `<camera>/<kind>/<date>` holds the files.
const LAYOUT_DEPTH: usize = 3;

/// Start-up status reporting for one reconciliation phase: the counts shown
/// to systemd and the time of the last report, so heartbeats keep their
/// cadence across awaits of any length.
struct Progress {
    phase: &'static str,
    processed: usize,
    changed: usize,
    last_report: Instant,
}

impl Progress {
    /// Reports the start of `phase`.
    fn start(phase: &'static str) -> Self {
        let mut progress = Self {
            phase,
            processed: 0,
            changed: 0,
            last_report: Instant::now(),
        };
        progress.report();
        progress
    }

    /// Reports the counts to systemd and extends the start timeout.
    fn report(&mut self) {
        systemd::status(&format!(
            "reconciling recordings: {}; {} inspected, {} changed",
            self.phase, self.processed, self.changed
        ));
        systemd::extend_timeout(RECONCILE_TIMEOUT_EXTENSION);
        self.last_report = Instant::now();
    }

    /// Awaits `future`, reporting whenever [`RECONCILE_HEARTBEAT`] has passed
    /// since the last report, so systemd's start timeout stays extended
    /// through a run of short awaits as well as through one long one.
    async fn heartbeat_while<F: Future>(&mut self, future: F) -> F::Output {
        heartbeat_from(self.last_report, RECONCILE_HEARTBEAT, future, || {
            self.report();
        })
        .await
    }
}

/// Awaits `future`, calling `beat` at every `interval` counted from `last`.
async fn heartbeat_from<F: Future>(
    last: Instant,
    interval: Duration,
    future: F,
    mut beat: impl FnMut(),
) -> F::Output {
    let mut ticker = tokio::time::interval_at(last + interval, interval);
    ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
    tokio::pin!(future);
    loop {
        tokio::select! {
            output = &mut future => return output,
            _ = ticker.tick() => beat(),
        }
    }
}

/// Finishes catalog deletion intents under available recording roots and
/// returns how many rows it removed. Rows on unavailable roots, outside their
/// root's layout, or behind a filesystem error remain for a later startup; a
/// catalog failure or a panicked removal task fails startup.
pub async fn finish_deleting(
    catalog: &CatalogHandle,
    roots: &[ApprovedRoot],
) -> anyhow::Result<usize> {
    let mut progress = Progress::start("finishing deletion intents");
    let mut cursor = None;
    loop {
        let rows = progress
            .heartbeat_while(catalog.deleting_segments_page(cursor, RECONCILE_PAGE))
            .await
            .context("listing deleting segments")?;
        if rows.is_empty() {
            break;
        }
        for StartupRow {
            row,
            root: persisted_root,
        } in rows
        {
            cursor = Some(SegmentCursor::from(&row));
            progress.processed += 1;
            let path = row.path.clone();
            let Some(root) = approved_root_for_row(roots, persisted_root.as_deref(), &row) else {
                warn!(
                    path = %path.display(),
                    "deleting segment root/path is unavailable or inconsistent; keeping its row"
                );
                continue;
            };
            let root = root.clone();
            let removal_path = path.clone();
            let removed = progress
                .heartbeat_while(tokio::task::spawn_blocking(move || {
                    remove_segment_beneath(&root, &removal_path)
                }))
                .await
                .with_context(|| format!("joining the removal task for {}", path.display()))?;
            match removed {
                Ok(_) => {
                    progress
                        .heartbeat_while(catalog.delete_segment(row.id))
                        .await
                        .with_context(|| {
                            format!("deleting the catalog row of {}", path.display())
                        })?;
                    progress.changed += 1;
                    info!(path = %path.display(), "finished interrupted segment deletion");
                }
                Err(error) => {
                    warn!(path = %path.display(), %error, "cannot finish segment deletion");
                }
            }
        }
        progress.report();
    }
    Ok(progress.changed)
}

/// The approved root that owns a catalog row: the row's persisted root must be
/// an approved root, and the row's path must lie in that root's recording
/// layout under the row's own stream key. Startup recovery and the in-run
/// deletion retry share this check.
pub(crate) fn approved_root_for_row<'a>(
    roots: &'a [ApprovedRoot],
    persisted_root: Option<&Path>,
    row: &SegmentRow,
) -> Option<&'a ApprovedRoot> {
    let persisted_root = persisted_root?;
    let root = roots.iter().find(|root| root.path() == persisted_root)?;
    let parsed = parse_segment_path(root.path(), &row.path)?;
    (parsed == row.key).then_some(root)
}

/// Facts about a file derived from a header scan of its media, or from a
/// sidecar bound to that scan.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ScanSummary {
    /// Bytes from the start that hold valid fragments.
    valid_len: u64,
    /// Number of valid fragments.
    fragments: u32,
    /// Length of `ftyp` + `moov`.
    init_len: u32,
    /// Wall time of the first fragment (zero if no `prft`).
    wall_start: WallUs,
    /// Wall time at the end of the last fragment (zero if no `prft`).
    wall_end: WallUs,
    /// Decode time of the first fragment.
    media_start: u64,
    /// Decode time at the end of the last fragment.
    media_end: u64,
}

/// Parses `<root>/<camera>/<kind>/<date>/<file>.mp4` into its stream key:
/// exactly four components below the root, a camera id, a stream kind, and
/// an `.mp4` file name. The date and the file stem are not validated.
#[must_use]
pub fn parse_segment_path(root: &Path, path: &Path) -> Option<StreamKey> {
    use std::path::Component;

    let relative = path.strip_prefix(root).ok()?;
    let mut components = relative.components();
    let Component::Normal(camera) = components.next()? else {
        return None;
    };
    let Component::Normal(kind) = components.next()? else {
        return None;
    };
    let Component::Normal(_date) = components.next()? else {
        return None;
    };
    let Component::Normal(_file) = components.next()? else {
        return None;
    };
    let camera: CameraId = camera.to_str()?.parse().ok()?;
    let kind: StreamKind = kind.to_str()?.parse().ok()?;
    if components.next().is_some() || path.extension().is_none_or(|e| e != "mp4") {
        return None;
    }
    Some(StreamKey { camera, kind })
}

/// Summarises a file from a header scan of its media; a sidecar bound to that
/// scan supplies the same facts. `None` when it holds no valid fragment.
#[cfg(test)]
fn summarize(path: &Path) -> anyhow::Result<Option<ScanSummary>> {
    let media = File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let index = File::open(path.with_extension("idx")).ok();
    summarize_open_pair(media, index, path)
}

/// Summarises a sealed file from its sidecar index when the index is bound to
/// a header scan of the media: records contiguous from the end of the init
/// segment to the end of the file, and a trailer at the end of the file that
/// matches the last record. Anything else returns `None`.
#[must_use]
#[cfg(test)]
fn summarize_from_index(path: &Path) -> Option<ScanSummary> {
    let mut media = File::open(path).ok()?;
    let index = File::open(path.with_extension("idx")).ok()?;
    let media_len = media.metadata().ok()?.len();
    let report = scan_authoritative(&mut media, MAX_PERSISTED_INDEX_RECORDS).ok()?;
    let (header, records) =
        read_index_file_bound_to_media(index, MAX_PERSISTED_INDEX_RECORDS, &report, media_len)
            .ok()??;
    summarize_validated_index(&header, &records, &report, media_len)
}

fn summarize_validated_index(
    header: &IndexHeader,
    records: &[IndexRecord],
    report: &ScanReport,
    media_len: u64,
) -> Option<ScanSummary> {
    let (first, last) = (records.first()?, records.last()?);
    if records
        .windows(2)
        .any(|pair| pair[1].decode_time < pair[0].decode_time)
    {
        return None;
    }
    let media_end = last.decode_time.checked_add(u64::from(last.duration))?;
    if media_end < first.decode_time {
        return None;
    }
    i64::try_from(first.decode_time).ok()?;
    i64::try_from(media_end).ok()?;
    let wall_end = report
        .fragments
        .last()?
        .wall
        .and_then(|wall| checked_wall_end(wall, last.duration))
        .unwrap_or_default();
    Some(ScanSummary {
        valid_len: media_len,
        fragments: u32::try_from(records.len()).ok()?,
        init_len: header.init_len,
        wall_start: first.wall,
        wall_end,
        media_start: first.decode_time,
        media_end,
    })
}

/// A header scan of `media` (no payload, no CRC) accepting at most
/// `max_fragments` fragments.
fn scan_authoritative(media: &mut File, max_fragments: usize) -> io::Result<ScanReport> {
    reader::scan_with_limit(
        io::BufReader::new(media),
        ScanOptions {
            verify_crc: false,
            collect_samples: false,
        },
        max_fragments,
    )
}

fn checked_wall_end(wall: WallUs, duration: u32) -> Option<WallUs> {
    let micros = MediaTicks::new(i64::from(duration)).checked_as_micros()?;
    wall.get().checked_add(micros).map(WallUs::new)
}

fn checked_summary_end(
    first_decode: u64,
    last_decode: u64,
    last_duration: u32,
) -> anyhow::Result<u64> {
    let end = last_decode
        .checked_add(u64::from(last_duration))
        .context("invalid media timeline: decode-time end overflows u64")?;
    if end < first_decode {
        anyhow::bail!("invalid media timeline: final end precedes first decode time");
    }
    Ok(end)
}

fn checked_catalog_media_range(media_start: u64, media_end: u64) -> anyhow::Result<()> {
    i64::try_from(media_start)
        .context("invalid media timeline: decode-time start exceeds the signed catalog range")?;
    i64::try_from(media_end)
        .context("invalid media timeline: decode-time end exceeds the signed catalog range")?;
    Ok(())
}

/// Scans a whole file and summarises it; `None` when it holds no valid fragment.
#[cfg(test)]
fn scan_summary(path: &Path) -> anyhow::Result<Option<ScanSummary>> {
    let file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let report = reader::scan(
        io::BufReader::new(file),
        ScanOptions {
            verify_crc: false,
            collect_samples: false,
        },
    )
    .with_context(|| format!("scanning {}", path.display()))?;
    summarize_report(&report)
}

fn summarize_report(report: &ScanReport) -> anyhow::Result<Option<ScanSummary>> {
    let (Some(first), Some(last)) = (report.fragments.first(), report.fragments.last()) else {
        return Ok(None);
    };
    if report
        .fragments
        .windows(2)
        .any(|window| window[1].decode_time < window[0].decode_time)
    {
        anyhow::bail!("invalid media timeline: decode time decreases between fragments");
    }
    let media_end = checked_summary_end(first.decode_time, last.decode_time, last.duration)?;
    checked_catalog_media_range(first.decode_time, media_end)?;
    let last_wall_end = match last.wall {
        Some(wall) => Some(
            checked_wall_end(wall, last.duration)
                .context("invalid media timeline: wall-clock end overflows i64")?,
        ),
        None => None,
    };
    Ok(Some(ScanSummary {
        valid_len: report.valid_len,
        // The scan limit keeps the count within `u32`; saturating here keeps
        // start-up code free of panics should that limit ever move.
        fragments: u32::try_from(report.fragments.len()).unwrap_or(u32::MAX),
        init_len: report.init_len,
        wall_start: first.wall.unwrap_or_default(),
        wall_end: last_wall_end.unwrap_or_default(),
        media_start: first.decode_time,
        media_end,
    }))
}

fn summarize_open_pair(
    media: File,
    index: Option<File>,
    display_path: &Path,
) -> anyhow::Result<Option<ScanSummary>> {
    summarize_open_pair_limited(media, index, display_path, MAX_PERSISTED_INDEX_RECORDS)
}

/// Summarises a pair whose media may hold at most `max_fragments` fragments.
/// The media's header scan is authoritative; a sidecar bound to it supplies
/// the same facts and any other sidecar is reported and ignored. A file
/// beyond the limit cannot be described by one sidecar, so it is refused
/// rather than adopted as a shortened prefix.
fn summarize_open_pair_limited(
    mut media: File,
    index: Option<File>,
    display_path: &Path,
    max_fragments: usize,
) -> anyhow::Result<Option<ScanSummary>> {
    let media_len = media
        .metadata()
        .with_context(|| format!("reading the status of {}", display_path.display()))?
        .len();
    let report = scan_authoritative(&mut media, max_fragments)
        .with_context(|| format!("scanning {}", display_path.display()))?;
    if let Some(ScanStop::FragmentLimit { limit, .. }) = report.stop {
        anyhow::bail!(
            "holds more than {limit} fragments, the most one sidecar index may describe; not \
             adopted"
        );
    }
    if let Some(index) = index {
        match read_index_file_bound_to_media(index, MAX_PERSISTED_INDEX_RECORDS, &report, media_len)
        {
            Ok(Some((header, records))) => {
                let summary = summarize_validated_index(&header, &records, &report, media_len)
                    .context("validated sidecar contains an invalid media timeline")?;
                return Ok(Some(summary));
            }
            Ok(None) => {
                warn!(
                    path = %display_path.display(),
                    "sidecar index does not describe the media; adopting from the scan"
                );
            }
            Err(error) => {
                warn!(
                    path = %display_path.display(),
                    %error,
                    "sidecar index is unreadable or over the limit; adopting from the scan"
                );
            }
        }
    }
    summarize_report(&report)
}

/// Recovers open segments under available recording roots and returns how
/// many it sealed. Rows on unavailable roots or behind lookup errors remain
/// for a later startup; a catalog failure or a panicked recovery task fails
/// startup.
///
/// Recovery stamps the row's run and sample-entry hash into each rebuilt
/// sidecar header that lacks them, so a recovered file stays bound to its row.
/// A row whose media or layout directory is absent beneath its verified root,
/// or whose recovered file holds no fragment, enters deletion intent; its
/// orphan sidecar and then the row are removed, and lost fragments are logged.
pub async fn recover_writing(
    catalog: &CatalogHandle,
    roots: &[ApprovedRoot],
) -> anyhow::Result<usize> {
    let mut progress = Progress::start("recovering open segments");
    let mut recovered = 0;
    let mut dropped = 0usize;
    let mut cleaned = 0usize;
    let mut fragments_lost = 0u64;
    let mut cursor = None;
    loop {
        let rows = progress
            .heartbeat_while(catalog.writing_segments_page(cursor, RECONCILE_PAGE))
            .await
            .context("listing open segments")?;
        if rows.is_empty() {
            break;
        }
        for StartupRow {
            row,
            root: persisted_root,
        } in rows
        {
            cursor = Some(SegmentCursor::from(&row));
            progress.processed += 1;
            let path = row.path.clone();
            let Some(root) = approved_root_for_row(roots, persisted_root.as_deref(), &row) else {
                warn!(
                    path = %path.display(),
                    "open segment root/path is unavailable or inconsistent; keeping its row"
                );
                continue;
            };
            let report_path = path.clone();
            let recovery_root = root.clone();
            let expected = IndexBinding::new(row.run_seq, row.sample_entry_hash);
            let report = progress
                .heartbeat_while(tokio::task::spawn_blocking(move || {
                    recover_segment_beneath(&recovery_root, &report_path, expected)
                }))
                .await
                .with_context(|| format!("joining the recovery task for {}", path.display()))?;
            match report {
                Ok(report) if report.fragments != 0 => {
                    if seal_recovered(catalog, &report, expected, &mut progress).await? {
                        recovered += 1;
                        progress.changed += 1;
                    }
                }
                Ok(_) => {
                    warn!(
                        path = %path.display(),
                        "recovered segment holds no fragments; scheduling durable cleanup"
                    );
                    if cleanup_unrecoverable_writing(catalog, root, &row, &mut progress).await? {
                        cleaned += 1;
                        progress.changed += 1;
                    }
                }
                Err(RecoverError::Absent(_)) => {
                    warn!(
                        path = %path.display(),
                        fragments = row.fragments,
                        "open segment is absent beneath its verified root; dropping its row and \
                         recorded fragments"
                    );
                    if cleanup_unrecoverable_writing(catalog, root, &row, &mut progress).await? {
                        dropped += 1;
                        progress.changed += 1;
                        fragments_lost += u64::from(row.fragments);
                    }
                }
                Err(err) => warn!(path = %path.display(), %err, "cannot recover segment"),
            }
        }
        progress.report();
    }
    if dropped != 0 || cleaned != 0 {
        warn!(
            recovered,
            dropped,
            cleaned,
            fragments_lost,
            "removed open segments whose files were absent or held no fragment"
        );
    }
    Ok(recovered)
}

/// Seals the catalog row of a recovered file that holds fragments. Returns
/// `false`, keeping the writing row, when the recovered timeline cannot be
/// stored. A rebuilt sidecar whose binding disagrees with the row is logged,
/// because the API refuses to serve it.
async fn seal_recovered(
    catalog: &CatalogHandle,
    report: &RecoveryReport,
    expected: IndexBinding,
    progress: &mut Progress,
) -> anyhow::Result<bool> {
    let path = &report.path;
    if !binding_matches(report.binding, expected) {
        warn!(
            path = %path.display(),
            sidecar = ?report.binding,
            catalog = ?expected,
            "recovered sidecar binding differs from the catalog row; the segment will not be served"
        );
    }
    if let Err(error) = checked_catalog_media_range(report.media_start, report.media_end) {
        warn!(
            path = %path.display(),
            %error,
            "recovered timeline cannot be represented in the catalog; keeping its writing row"
        );
        return Ok(false);
    }
    progress
        .heartbeat_while(catalog.mark_sealed(
            path,
            report.valid_len,
            // The recovery scan accepts at most the sidecar record limit, which
            // fits `u32`; saturating keeps start-up code free of panics.
            u32::try_from(report.fragments).unwrap_or(u32::MAX),
            report.wall_end,
            report.media_end,
        ))
        .await
        .with_context(|| format!("sealing the recovered segment {}", path.display()))?;
    info!(
        path = %path.display(),
        fragments = report.fragments,
        truncated = report.truncated,
        "recovered open segment"
    );
    Ok(true)
}

/// Whether a recovered header carries every binding field the catalog row
/// knows; the API refuses a segment whose known fields differ.
fn binding_matches(found: IndexBinding, expected: IndexBinding) -> bool {
    (expected.run_id() == 0 || found.run_id() == expected.run_id())
        && (expected.sample_entry_hash() == 0
            || found.sample_entry_hash() == expected.sample_entry_hash())
}

/// Removes an open row whose file is absent or holds nothing recoverable.
/// Returns whether the row is gone; a row that could not enter deletion
/// intent or whose pair removal failed remains for a later start.
async fn cleanup_unrecoverable_writing(
    catalog: &CatalogHandle,
    root: &ApprovedRoot,
    row: &SegmentRow,
    progress: &mut Progress,
) -> anyhow::Result<bool> {
    cleanup_unrecoverable_writing_with(catalog, root, row, progress, |root, path| {
        remove_segment_beneath(&root, &path)
    })
    .await
}

async fn cleanup_unrecoverable_writing_with<F>(
    catalog: &CatalogHandle,
    root: &ApprovedRoot,
    row: &SegmentRow,
    progress: &mut Progress,
    remove: F,
) -> anyhow::Result<bool>
where
    F: FnOnce(ApprovedRoot, PathBuf) -> io::Result<u64> + Send + 'static,
{
    match progress
        .heartbeat_while(catalog.mark_empty_writing_deleting(row.id))
        .await
        .with_context(|| format!("recording the cleanup intent of {}", row.path.display()))?
    {
        DeleteMarkResult::Marked => {}
        DeleteMarkResult::NotFound => {
            warn!(
                path = %row.path.display(),
                "open segment row cannot enter deletion intent; keeping it for a later start"
            );
            return Ok(false);
        }
        DeleteMarkResult::Protected => {
            anyhow::bail!("empty writing segment unexpectedly reported protected")
        }
    }
    let root = root.clone();
    let path = row.path.clone();
    let removed = progress
        .heartbeat_while(tokio::task::spawn_blocking(move || remove(root, path)))
        .await
        .with_context(|| format!("joining the cleanup task for {}", row.path.display()))?;
    match removed {
        Ok(_) => {
            progress
                .heartbeat_while(catalog.delete_segment(row.id))
                .await
                .with_context(|| format!("deleting the catalog row of {}", row.path.display()))?;
            Ok(true)
        }
        Err(error) => {
            warn!(path = %row.path.display(), %error, "empty segment cleanup remains retryable");
            Ok(false)
        }
    }
}

/// Lists every `.mp4` within the layout depth under a recording root.
#[cfg(test)]
pub fn recordings_under(root: &Path) -> Vec<PathBuf> {
    fn visit(dir: &Path, depth: usize, found: &mut Vec<PathBuf>) {
        for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
            let path = entry.path();
            if path.is_dir() && depth < LAYOUT_DEPTH {
                visit(&path, depth + 1, found);
            } else if path.is_file() && path.extension().is_some_and(|e| e == "mp4") {
                found.push(path);
            }
        }
    }
    let mut found = Vec::new();
    visit(root, 0, &mut found);
    found.sort();
    found
}

/// A directory the walk could not read, with the error.
#[derive(Debug)]
struct WalkFailure {
    path: PathBuf,
    error: io::Error,
}

/// What the walk sends: a recording's path, or a directory it could not read.
type WalkItem = Result<PathBuf, WalkFailure>;

/// Streams the recordings beneath `root` into `sender`: the root is opened
/// through its approved descriptor, each child directory through `openat2`
/// beneath its parent without following links, magic links, or mounts, at
/// most [`LAYOUT_DEPTH`] levels deep, and only regular `.mp4` entries are
/// sent. A directory that cannot be opened or read is sent as a failure and
/// skipped; its siblings are still walked. The walk stops silently once the
/// receiver is gone.
fn walk_recordings_beneath(root: &ApprovedRoot, sender: &mpsc::Sender<WalkItem>) {
    match open_approved_root_directory(root) {
        Ok(directory) => walk_directory_beneath(&directory, root.path(), 0, sender),
        Err(error) => {
            let _ = sender.blocking_send(Err(WalkFailure {
                path: root.path().to_path_buf(),
                error,
            }));
        }
    }
}

fn walk_directory_beneath(
    directory: &File,
    display_path: &Path,
    depth: usize,
    sender: &mpsc::Sender<WalkItem>,
) {
    use std::os::unix::ffi::OsStrExt as _;

    let mut buffer = [MaybeUninit::uninit(); 16 * 1024];
    let mut entries = RawDir::new(directory, &mut buffer);
    while let Some(entry) = entries.next() {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                let _ = sender.blocking_send(Err(WalkFailure {
                    path: display_path.to_path_buf(),
                    error: error.into(),
                }));
                return;
            }
        };
        let bytes = entry.file_name().to_bytes();
        if bytes == b"." || bytes == b".." {
            continue;
        }
        let name = OsStr::from_bytes(bytes).to_owned();
        let path = display_path.join(&name);
        let kind = entry_kind(entry.file_type(), || {
            rustix::fs::statat(directory, &name, AtFlags::SYMLINK_NOFOLLOW)
                .map(|stat| FileType::from_raw_mode(stat.st_mode))
        });
        let kind = match kind {
            Ok(kind) => kind,
            Err(error) => {
                if sender
                    .blocking_send(Err(WalkFailure {
                        path,
                        error: error.into(),
                    }))
                    .is_err()
                {
                    return;
                }
                continue;
            }
        };
        if kind.is_dir() && depth < LAYOUT_DEPTH {
            let child = rustix::fs::openat2(
                directory,
                &name,
                OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::empty(),
                ResolveFlags::BENEATH
                    | ResolveFlags::NO_SYMLINKS
                    | ResolveFlags::NO_MAGICLINKS
                    | ResolveFlags::NO_XDEV,
            )
            .map(File::from);
            match child {
                Ok(child) => {
                    walk_directory_beneath(&child, &path, depth + 1, sender);
                    if sender.is_closed() {
                        return;
                    }
                }
                Err(error) => {
                    if sender
                        .blocking_send(Err(WalkFailure {
                            path,
                            error: error.into(),
                        }))
                        .is_err()
                    {
                        return;
                    }
                }
            }
        } else if kind.is_file()
            && path.extension().is_some_and(|extension| extension == "mp4")
            && sender.blocking_send(Ok(path)).is_err()
        {
            return;
        }
    }
}

/// The type of a directory entry: what `getdents` reported, or `stat` when it
/// reported none, as filesystems without `d_type` do.
fn entry_kind(
    reported: FileType,
    stat: impl FnOnce() -> rustix::io::Result<FileType>,
) -> rustix::io::Result<FileType> {
    if reported == FileType::Unknown {
        stat()
    } else {
        Ok(reported)
    }
}

/// Summarises a discovered recording on the blocking pool; `None` when it
/// holds no valid fragment or cannot be scanned, which is logged.
///
/// # Errors
///
/// Fails when the scan task panicked.
async fn scan_discovered_recording(
    root: &ApprovedRoot,
    path: &Path,
    progress: &mut Progress,
) -> anyhow::Result<Option<ScanSummary>> {
    let scan_path = path.to_path_buf();
    let scan_root = root.clone();
    let summary = progress
        .heartbeat_while(tokio::task::spawn_blocking(move || {
            let (media, index) = open_discovered_pair(&scan_root, &scan_path)?;
            summarize_open_pair(media, index, &scan_path)
        }))
        .await
        .with_context(|| format!("joining the scan task for {}", path.display()))?;
    Ok(match summary {
        Ok(Some(summary)) => Some(summary),
        Ok(None) => {
            warn!(path = %path.display(), "no valid fragments; ignoring");
            None
        }
        Err(error) => {
            warn!(path = %path.display(), %error, "cannot scan; ignoring");
            None
        }
    })
}

/// Opens a discovered recording and its sidecar. A sidecar that cannot be
/// opened (a symlink, FIFO, device, directory, or other open failure) is
/// ignored: the media is opened alone and its scan alone describes it.
fn open_discovered_pair(root: &ApprovedRoot, path: &Path) -> anyhow::Result<(File, Option<File>)> {
    match open_segment_pair_beneath(root, path) {
        Ok(pair) => Ok(pair),
        Err(pair_error) => {
            let media = open_segment_beneath(root, path)
                .with_context(|| format!("opening {} beneath approved root", path.display()))?;
            warn!(
                path = %path.display(),
                %pair_error,
                "sidecar cannot be opened; adopting from the media scan"
            );
            Ok((media, None))
        }
    }
}

/// Adds files under the roots that the catalog does not know and returns how
/// many it adopted. Every discovered file gets one header scan of its media;
/// a file outside the layout, without a valid fragment, beyond the fragment
/// limit, or that cannot be scanned is logged and skipped, and so is a
/// directory that cannot be read. A catalog failure or a panicked task fails
/// startup.
pub async fn adopt_unknown(
    catalog: &CatalogHandle,
    roots: &[ApprovedRoot],
) -> anyhow::Result<usize> {
    let mut progress = Progress::start("discovering unknown recordings");
    for root in roots {
        let (sender, mut receiver) = mpsc::channel(64);
        let walk_root = root.clone();
        let walk =
            tokio::task::spawn_blocking(move || walk_recordings_beneath(&walk_root, &sender));
        while let Some(item) = progress.heartbeat_while(receiver.recv()).await {
            let path = match item {
                Ok(path) => path,
                Err(failure) => {
                    warn!(
                        path = %failure.path.display(),
                        error = %failure.error,
                        "cannot read a recording directory; its files are not adopted"
                    );
                    continue;
                }
            };
            progress.processed += 1;
            if progress.processed.is_multiple_of(ADOPT_STATUS_EVERY) {
                progress.report();
            }
            if progress
                .heartbeat_while(catalog.contains_segment_path(&path))
                .await
                .with_context(|| format!("looking up {}", path.display()))?
            {
                continue;
            }
            let Some(key) = parse_segment_path(root.path(), &path) else {
                warn!(
                    path = %path.display(),
                    "file does not follow the recording layout; ignoring"
                );
                continue;
            };
            let Some(summary) = scan_discovered_recording(root, &path, &mut progress).await? else {
                continue;
            };
            progress
                .heartbeat_while(catalog.adopt_segment(NewSegment {
                    key: key.clone(),
                    path: path.clone(),
                    run_seq: 0,
                    discontinuity: true,
                    wall_start: summary.wall_start,
                    wall_end: summary.wall_end,
                    media_start: summary.media_start,
                    media_end: summary.media_end,
                    init_len: summary.init_len,
                    len: summary.valid_len,
                    fragments: summary.fragments,
                    sample_entry: None,
                }))
                .await
                .with_context(|| format!("adopting {}", path.display()))?;
            info!(
                path = %path.display(),
                stream = %key,
                fragments = summary.fragments,
                "adopted recording"
            );
            progress.changed += 1;
        }
        walk.await
            .with_context(|| format!("joining the directory walk of {}", root.path().display()))?;
    }
    Ok(progress.changed)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use bytes::Bytes;
    use iris_catalog::{CatalogRootExpectation, CatalogRootRegistry};
    use iris_mp4::fragment::{Fragment, Sample};
    use iris_mp4::init::SampleEntry;
    use iris_store::{RootExpectation, StoragePolicy, WriterHandle, admit_recording_root};

    use super::*;

    async fn approve_root(catalog: &CatalogHandle, root: &Path) -> ApprovedRoot {
        let approved = admit_recording_root(root, RootExpectation::Fresh).unwrap();
        catalog
            .register_recording_root(root, approved.identity())
            .await
            .unwrap();
        catalog
            .configure_video_roots(&[root.to_path_buf()])
            .await
            .unwrap();
        approved
    }

    fn register_writing(catalog: &CatalogHandle, path: PathBuf) {
        register_writing_at(catalog, path, 0);
    }

    fn register_writing_at(catalog: &CatalogHandle, path: PathBuf, media_start: u64) {
        catalog.notify(iris_catalog::Change::SegmentOpened {
            key: StreamKey {
                camera: "cam1".parse().unwrap(),
                kind: StreamKind::Main,
            },
            path,
            run_seq: 1,
            discontinuity: true,
            wall_start: WallUs::new(1_700_000_000_000_000),
            media_start,
            init_len: 768,
            sample_entry: Arc::new(SampleEntry::new(
                Bytes::from_static(b"\0\0\0\x10hvc1\0\0\0\0\0\0\0\0"),
                "hvc1".into(),
                16,
                16,
            )),
        });
    }

    async fn register_sealed(catalog: &CatalogHandle, path: PathBuf) -> iris_catalog::SegmentId {
        catalog
            .adopt_segment(NewSegment {
                key: StreamKey {
                    camera: "cam1".parse().unwrap(),
                    kind: StreamKind::Main,
                },
                path,
                run_seq: 1,
                discontinuity: true,
                wall_start: WallUs::new(1_700_000_000_000_000),
                wall_end: WallUs::new(1_700_000_001_000_000),
                media_start: 0,
                media_end: 90_000,
                init_len: 700,
                len: 5,
                fragments: 1,
                sample_entry: None,
            })
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn startup_rolls_every_available_deletion_boundary_forward() {
        for state in 0..4 {
            let dir = tempfile::tempdir().unwrap();
            let root = dir.path().join("video");
            std::fs::create_dir_all(&root).unwrap();
            let catalog = CatalogHandle::open(&dir.path().join("catalog.db")).unwrap();
            let approved = approve_root(&catalog, &root).await;
            let path = root.join(format!("cam1/main/2026-01-01/boundary-{state}.mp4"));
            let index = path.with_extension("idx");
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, b"media").unwrap();
            std::fs::write(&index, b"index").unwrap();
            let id = register_sealed(&catalog, path.clone()).await;
            assert_eq!(
                catalog.mark_deleting(id).await.unwrap(),
                DeleteMarkResult::Marked
            );
            match state {
                0 => {} // immediately after durable intent
                1 => std::fs::remove_file(&path).unwrap(),
                2 => std::fs::remove_file(&index).unwrap(),
                3 => {
                    std::fs::remove_file(&path).unwrap();
                    std::fs::remove_file(&index).unwrap();
                    File::open(&root).unwrap().sync_all().unwrap();
                }
                _ => unreachable!(),
            }

            assert_eq!(
                finish_deleting(&catalog, &[approved]).await.unwrap(),
                1,
                "{state}"
            );
            assert!(!path.exists(), "{state}");
            assert!(!index.exists(), "{state}");
            assert_eq!(
                catalog.deleting_segments().await.unwrap(),
                Vec::<SegmentRow>::new(),
                "{state}"
            );
            assert_eq!(catalog.total_bytes().await.unwrap(), 0, "{state}");
            catalog.shutdown();
        }
    }

    #[tokio::test]
    async fn finishing_deletions_leaves_rows_without_intent_alone() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("video");
        std::fs::create_dir_all(&root).unwrap();
        let catalog = CatalogHandle::open(&dir.path().join("catalog.db")).unwrap();
        let approved = approve_root(&catalog, &root).await;
        let retained = root.join("cam1/main/2026-01-01/retained.mp4");
        std::fs::create_dir_all(retained.parent().unwrap()).unwrap();
        std::fs::write(&retained, b"media").unwrap();
        register_sealed(&catalog, retained.clone()).await;

        assert_eq!(finish_deleting(&catalog, &[approved]).await.unwrap(), 0);
        assert!(retained.exists());
        assert!(catalog.segment_by_path(&retained).await.unwrap().is_some());
        catalog.shutdown();
    }

    #[tokio::test]
    async fn deletion_intents_on_unavailable_roots_wait_for_a_later_start() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("video");
        let unavailable = dir.path().join("unavailable");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&unavailable).unwrap();
        let catalog = CatalogHandle::open(&dir.path().join("catalog.db")).unwrap();
        let approved = approve_root(&catalog, &root).await;
        let pending = unavailable.join("pending.mp4");
        std::fs::write(&pending, b"media").unwrap();
        let pending_id = register_sealed(&catalog, pending.clone()).await;
        assert_eq!(
            catalog.mark_deleting(pending_id).await.unwrap(),
            DeleteMarkResult::Marked
        );

        assert_eq!(finish_deleting(&catalog, &[approved]).await.unwrap(), 0);
        assert!(pending.exists());
        assert_eq!(catalog.deleting_segments().await.unwrap().len(), 1);
        catalog.shutdown();
    }

    #[tokio::test]
    async fn adoption_cannot_resurrect_a_deletion_intent_with_media_remaining() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("video");
        let catalog = CatalogHandle::open(&dir.path().join("catalog.db")).unwrap();
        std::fs::create_dir_all(&root).unwrap();
        let approved = approve_root(&catalog, &root).await;
        let path = sealed_recording(&root);
        let id = register_sealed(&catalog, path.clone()).await;
        assert_eq!(
            catalog.mark_deleting(id).await.unwrap(),
            DeleteMarkResult::Marked
        );

        assert_eq!(adopt_unknown(&catalog, &[approved]).await.unwrap(), 0);
        assert!(path.exists());
        assert_eq!(catalog.deleting_segments().await.unwrap()[0].id, id);
        assert!(catalog.segment_by_path(&path).await.unwrap().is_none());
        catalog.shutdown();
    }

    #[tokio::test]
    async fn unavailable_roots_keep_their_writing_catalog_rows() {
        let dir = tempfile::tempdir().unwrap();
        let available = dir.path().join("available");
        std::fs::create_dir_all(&available).unwrap();
        let catalog = CatalogHandle::open(&dir.path().join("catalog.db")).unwrap();
        let approved = approve_root(&catalog, &available).await;
        let path = dir.path().join("unavailable/cam1/main/recording.mp4");
        register_writing(&catalog, path.clone());
        assert_eq!(recover_writing(&catalog, &[approved]).await.unwrap(), 0);
        assert!(
            catalog.segment_by_path(&path).await.unwrap().is_some(),
            "an unavailable root is not evidence that its recording was deleted"
        );
        catalog.shutdown();
    }

    #[tokio::test]
    async fn a_replaced_root_preserves_its_open_row_until_the_original_identity_returns() {
        let dir = tempfile::tempdir().unwrap();
        let catalog_path = dir.path().join("catalog.db");
        let root = dir.path().join("video");
        let displaced = dir.path().join("video-displaced");
        let approved = admit_recording_root(&root, RootExpectation::Fresh).unwrap();
        let expected_identity = approved.identity();
        let path = sealed_recording(&root);

        let catalog = CatalogHandle::open(&catalog_path).unwrap();
        catalog
            .register_recording_root(&root, expected_identity)
            .await
            .unwrap();
        catalog
            .configure_video_roots(std::slice::from_ref(&root))
            .await
            .unwrap();
        register_writing(&catalog, path.clone());
        assert!(catalog.segment_by_path(&path).await.unwrap().is_some());
        catalog.shutdown();

        std::fs::rename(&root, &displaced).unwrap();
        std::fs::create_dir(&root).unwrap();
        let expectation = CatalogRootRegistry::open(&catalog_path)
            .unwrap()
            .expectations_with_progress(std::slice::from_ref(&root), || {})
            .unwrap()
            .pop()
            .unwrap();
        assert_eq!(
            expectation,
            CatalogRootExpectation::Known(expected_identity)
        );
        assert!(
            admit_recording_root(&root, RootExpectation::Known(expected_identity)).is_err(),
            "the empty replacement must not be initialized or approved"
        );

        let catalog = CatalogHandle::open(&catalog_path).unwrap();
        assert_eq!(recover_writing(&catalog, &[]).await.unwrap(), 0);
        assert_eq!(adopt_unknown(&catalog, &[]).await.unwrap(), 0);
        assert!(catalog.segment_by_path(&path).await.unwrap().is_some());
        assert_eq!(std::fs::read_dir(&root).unwrap().count(), 0);
        catalog.shutdown();

        std::fs::remove_dir(&root).unwrap();
        std::fs::rename(&displaced, &root).unwrap();
        let returned =
            admit_recording_root(&root, RootExpectation::Known(expected_identity)).unwrap();
        let catalog = CatalogHandle::open(&catalog_path).unwrap();
        catalog
            .configure_video_roots(std::slice::from_ref(&root))
            .await
            .unwrap();
        assert_eq!(
            recover_writing(&catalog, std::slice::from_ref(&returned))
                .await
                .unwrap(),
            1
        );
        let row = catalog.segment_by_path(&path).await.unwrap().unwrap();
        assert!(row.sealed);
        assert_eq!(row.fragments, 3);
        catalog.shutdown();
    }

    #[tokio::test]
    async fn cross_root_links_and_parent_components_never_mutate_the_other_root() {
        let dir = tempfile::tempdir().unwrap();
        let root_a = dir.path().join("root-a");
        let root_b = dir.path().join("root-b");
        let catalog = CatalogHandle::open(&dir.path().join("catalog.db")).unwrap();
        let approved_a = approve_root(&catalog, &root_a).await;
        let approved_b = approve_root(&catalog, &root_b).await;
        catalog
            .configure_video_roots(&[root_a.clone(), root_b.clone()])
            .await
            .unwrap();

        let victim = sealed_recording(&root_b);
        let victim_index = victim.with_extension("idx");
        let media_bytes = std::fs::read(&victim).unwrap();
        let index_bytes = std::fs::read(&victim_index).unwrap();
        let date = victim.parent().unwrap().file_name().unwrap();
        let linked_parent = root_a.join("cam1/main");
        std::fs::create_dir_all(&linked_parent).unwrap();
        std::os::unix::fs::symlink(victim.parent().unwrap(), linked_parent.join(date)).unwrap();
        let linked_path = linked_parent.join(date).join(victim.file_name().unwrap());

        let linked_id = register_sealed(&catalog, linked_path.clone()).await;
        assert_eq!(
            catalog.mark_deleting(linked_id).await.unwrap(),
            DeleteMarkResult::Marked
        );
        assert_eq!(
            finish_deleting(&catalog, &[approved_a.clone(), approved_b.clone()])
                .await
                .unwrap(),
            0
        );
        assert_eq!(std::fs::read(&victim).unwrap(), media_bytes);
        assert_eq!(std::fs::read(&victim_index).unwrap(), index_bytes);

        let parent_path = root_a
            .join("cam1/main/2026-01-01/../../../../root-b")
            .join(victim.strip_prefix(&root_b).unwrap());
        let parent_id = register_sealed(&catalog, parent_path.clone()).await;
        assert_eq!(
            catalog.mark_deleting(parent_id).await.unwrap(),
            DeleteMarkResult::Marked
        );
        assert_eq!(
            finish_deleting(&catalog, &[approved_a.clone(), approved_b.clone()])
                .await
                .unwrap(),
            0
        );
        assert_eq!(std::fs::read(&victim).unwrap(), media_bytes);
        assert_eq!(std::fs::read(&victim_index).unwrap(), index_bytes);

        assert_eq!(
            adopt_unknown(&catalog, &[approved_a, approved_b])
                .await
                .unwrap(),
            1
        );
        assert!(catalog.segment_by_path(&victim).await.unwrap().is_some());
        assert!(
            catalog
                .segment_by_path(&linked_path)
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            catalog
                .segment_by_path(&parent_path)
                .await
                .unwrap()
                .is_none()
        );
        let deleting = catalog.deleting_segments().await.unwrap();
        assert_eq!(deleting.len(), 2);
        assert!(deleting.iter().any(|row| row.path == linked_path));
        assert!(deleting.iter().any(|row| row.path == parent_path));
        catalog.shutdown();
    }

    #[tokio::test]
    async fn missing_recordings_on_available_roots_drop_their_rows() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("video");
        std::fs::create_dir_all(&root).unwrap();
        let catalog = CatalogHandle::open(&dir.path().join("catalog.db")).unwrap();
        let approved = approve_root(&catalog, &root).await;
        let path = root.join("cam1/main/2026-01-01/missing.mp4");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        register_writing(&catalog, path.clone());
        assert_eq!(recover_writing(&catalog, &[approved]).await.unwrap(), 0);
        assert!(catalog.segment_by_path(&path).await.unwrap().is_none());
        catalog.shutdown();
    }

    #[tokio::test]
    async fn absent_recovery_keeps_a_durable_tombstone_when_pair_removal_fails() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("video");
        let catalog = CatalogHandle::open(&dir.path().join("catalog.db")).unwrap();
        let approved = approve_root(&catalog, &root).await;
        let path = root.join("cam1/main/2026-01-01/missing.mp4");
        let index = path.with_extension("idx");
        std::fs::create_dir_all(&index).unwrap();
        register_writing(&catalog, path.clone());

        assert_eq!(
            recover_writing(&catalog, std::slice::from_ref(&approved))
                .await
                .unwrap(),
            0
        );
        assert!(catalog.segment_by_path(&path).await.unwrap().is_none());
        let deleting = catalog.deleting_segments().await.unwrap();
        assert_eq!(
            deleting.len(),
            1,
            "failed removal must retain its tombstone"
        );
        assert_eq!(deleting[0].path, path);

        std::fs::remove_dir(&index).unwrap();
        std::fs::write(&index, b"retryable sidecar").unwrap();
        assert_eq!(finish_deleting(&catalog, &[approved]).await.unwrap(), 1);
        assert_eq!(catalog.deleting_segments().await.unwrap(), Vec::new());
        assert!(!index.exists());
        catalog.shutdown();
    }

    #[tokio::test]
    async fn every_empty_cleanup_boundary_keeps_catalog_ownership_until_idempotent_retry() {
        #[derive(Clone, Copy, Debug)]
        enum Fault {
            FirstUnlink,
            SecondUnlink,
            DirectorySync,
        }

        for fault in [
            Fault::FirstUnlink,
            Fault::SecondUnlink,
            Fault::DirectorySync,
        ] {
            let dir = tempfile::tempdir().unwrap();
            let root = dir.path().join("video");
            let catalog = CatalogHandle::open(&dir.path().join("catalog.db")).unwrap();
            let approved = approve_root(&catalog, &root).await;
            let path = root.join("cam1/main/2026-01-01/empty.mp4");
            let index = path.with_extension("idx");
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, b"media").unwrap();
            std::fs::write(&index, b"index").unwrap();
            register_writing(&catalog, path.clone());
            let row = catalog.writing_segments().await.unwrap().remove(0);
            let media_for_fault = path.clone();
            let index_for_fault = index.clone();

            let mut progress = Progress::start("test cleanup");
            let removed = cleanup_unrecoverable_writing_with(
                &catalog,
                &approved,
                &row,
                &mut progress,
                move |_, _| {
                    match fault {
                        Fault::FirstUnlink => {}
                        Fault::SecondUnlink => std::fs::remove_file(&media_for_fault)?,
                        Fault::DirectorySync => {
                            std::fs::remove_file(&media_for_fault)?;
                            std::fs::remove_file(&index_for_fault)?;
                        }
                    }
                    Err(io::Error::other("injected empty-cleanup boundary failure"))
                },
            )
            .await
            .unwrap();
            assert!(!removed, "{fault:?}");

            let deleting = catalog.deleting_segments().await.unwrap();
            assert_eq!(deleting.len(), 1, "{fault:?}");
            assert_eq!(deleting[0].id, row.id, "{fault:?}");
            assert_eq!(
                finish_deleting(&catalog, std::slice::from_ref(&approved))
                    .await
                    .unwrap(),
                1,
                "{fault:?}"
            );
            assert_eq!(finish_deleting(&catalog, &[approved]).await.unwrap(), 0);
            assert!(!path.exists(), "{fault:?}");
            assert!(!index.exists(), "{fault:?}");
            assert_eq!(catalog.deleting_segments().await.unwrap(), Vec::new());
            catalog.shutdown();
        }
    }

    #[tokio::test]
    async fn available_recordings_are_recovered_and_sealed() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("video");
        let catalog = CatalogHandle::open(&dir.path().join("catalog.db")).unwrap();
        std::fs::create_dir_all(&root).unwrap();
        let approved = approve_root(&catalog, &root).await;
        let path = sealed_recording(&root);
        register_writing(&catalog, path.clone());
        assert_eq!(recover_writing(&catalog, &[approved]).await.unwrap(), 1);
        assert_eq!(catalog.writing_segments().await.unwrap(), []);
        let row = catalog.segment_by_path(&path).await.unwrap().unwrap();
        assert_eq!(row.fragments, 3);
        assert!(row.sealed);
        assert_eq!(row.bytes, std::fs::metadata(path).unwrap().len());
        catalog.shutdown();
    }

    #[tokio::test]
    async fn permission_failures_keep_writing_catalog_rows() {
        use std::os::unix::fs::PermissionsExt as _;
        if rustix::process::geteuid().is_root() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("video");
        std::fs::create_dir_all(&root).unwrap();
        let catalog = CatalogHandle::open(&dir.path().join("catalog.db")).unwrap();
        let approved = approve_root(&catalog, &root).await;
        let path = root.join("cam1/main/2026-01-01/recording.mp4");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, b"inaccessible").unwrap();
        register_writing(&catalog, path.clone());
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o0)).unwrap();
        let result = recover_writing(&catalog, std::slice::from_ref(&approved)).await;
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700)).unwrap();
        assert_eq!(result.unwrap(), 0);
        assert!(
            catalog.segment_by_path(&path).await.unwrap().is_some(),
            "PermissionDenied must not be treated as NotFound"
        );
        catalog.shutdown();
    }

    fn nal(header: u8, filler: u8, body: usize) -> Vec<u8> {
        let len = u32::try_from(2 + body).unwrap();
        let mut v = len.to_be_bytes().to_vec();
        v.extend_from_slice(&[header, 1]);
        v.resize(4 + 2 + body, filler);
        v
    }

    /// Writes a sealed three-fragment recording with the store and returns its path.
    fn sealed_recording(root: &Path) -> PathBuf {
        sealed_recording_at(root, 0)
    }

    fn sealed_recording_at(root: &Path, media_start: u64) -> PathBuf {
        let entry = Arc::new(SampleEntry::new(
            Bytes::from_static(b"\0\0\0\x10hvc1\0\0\0\0\0\0\0\0"),
            "hvc1.1.6.L150.B0".into(),
            640,
            480,
        ));
        let policy = StoragePolicy {
            sync_interval: Duration::from_millis(50),
            preallocate_bytes: 1 << 20,
            rotate_bytes: 1 << 30,
            rotate_age: Duration::from_secs(3_600),
        };
        let writer = WriterHandle::spawn(root.to_path_buf(), policy, 16).unwrap();
        let key = StreamKey {
            camera: "cam1".parse().unwrap(),
            kind: StreamKind::Main,
        };
        for i in 0..3u64 {
            let fragment = Fragment {
                samples: vec![
                    Sample {
                        data: Bytes::from(nal(0x26, 0xaa, 4_000)),
                        duration: 3_000,
                        is_sync: true,
                    },
                    Sample {
                        data: Bytes::from(nal(0x02, 0xbb, 1_000)),
                        duration: 3_000,
                        is_sync: false,
                    },
                ],
                decode_time: media_start + i * 6_000,
                wall: WallUs::new(1_700_000_000_000_000 + i64::try_from(i).unwrap() * 66_667),
                raw_rtp: 42,
                new_run: i == 0,
                sample_entry: Arc::clone(&entry),
            };
            writer.try_send(key.clone(), fragment).unwrap();
        }
        writer.shutdown().unwrap();
        recordings_under(root).remove(0)
    }

    #[test]
    fn a_bound_sidecar_summarises_like_the_scan() {
        let dir = tempfile::tempdir().unwrap();
        let path = sealed_recording(dir.path());
        let by_index = summarize_from_index(&path).expect("sealed file with a sidecar");
        let by_scan = scan_summary(&path).unwrap().expect("valid fragments");
        assert_eq!(by_index, by_scan);
        assert_eq!(by_index.fragments, 3);
        assert_eq!(summarize(&path).unwrap(), Some(by_index));
    }

    #[test]
    fn a_sidecar_that_disagrees_with_the_media_is_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let path = sealed_recording(dir.path());
        let by_scan = scan_summary(&path).unwrap().expect("valid fragments");

        // An index missing its last record disagrees with the file: back to the scan.
        let idx = path.with_extension("idx");
        let bytes = std::fs::read(&idx).unwrap();
        std::fs::write(&idx, &bytes[..bytes.len() - 40]).unwrap();
        assert_eq!(summarize_from_index(&path), None);
        assert_eq!(summarize(&path).unwrap(), Some(by_scan));

        let oversized = iris_mp4::index::HEADER_LEN
            + MAX_PERSISTED_INDEX_RECORDS * iris_mp4::index::RECORD_LEN
            + iris_mp4::index::RECORD_LEN;
        std::fs::OpenOptions::new()
            .write(true)
            .open(&idx)
            .unwrap()
            .set_len(u64::try_from(oversized).unwrap())
            .unwrap();
        assert_eq!(summarize_from_index(&path), None);
        assert_eq!(summarize(&path).unwrap(), Some(by_scan));

        // No index at all: the scan still works.
        std::fs::remove_file(&idx).unwrap();
        assert_eq!(summarize_from_index(&path), None);
        assert_eq!(summarize(&path).unwrap(), Some(by_scan));
    }

    #[test]
    fn adoption_rejects_every_cache_fact_that_disagrees_with_media() {
        let dir = tempfile::tempdir().unwrap();
        let path = sealed_recording(dir.path());
        let index = path.with_extension("idx");
        let expected = scan_summary(&path).unwrap().unwrap();
        let (header, records) =
            iris_mp4::index::read_index_bounded(&index, MAX_PERSISTED_INDEX_RECORDS).unwrap();
        assert_eq!(records.len(), 3);
        let assert_fallback = |header: IndexHeader, records: &[IndexRecord]| {
            let mut bytes = header.encode().to_vec();
            for record in records {
                bytes.extend(record.encode());
            }
            std::fs::write(&index, bytes).unwrap();
            assert_eq!(summarize_from_index(&path), None);
            assert_eq!(summarize(&path).unwrap(), Some(expected));
        };

        for changed_header in [
            IndexHeader {
                timescale: header.timescale + 1,
                ..header
            },
            IndexHeader { flags: 1, ..header },
            IndexHeader {
                start_wall: WallUs::new(header.start_wall.get() + 1),
                ..header
            },
            IndexHeader {
                init_len: header.init_len + 1,
                ..header
            },
        ] {
            assert_fallback(changed_header, &records);
        }
        for (index_to_change, mutate) in [
            (
                0,
                (|record: &mut IndexRecord| record.wall = WallUs::new(record.wall.get() + 1))
                    as fn(&mut IndexRecord),
            ),
            (0, |record: &mut IndexRecord| record.decode_time += 1),
            (1, |record: &mut IndexRecord| record.duration += 1),
            (1, |record: &mut IndexRecord| record.sample_count += 1),
            (1, |record: &mut IndexRecord| {
                record.flags ^= iris_mp4::index::FLAG_STARTS_WITH_SYNC;
            }),
            (2, |record: &mut IndexRecord| {
                record.wall = WallUs::new(record.wall.get() + 1);
            }),
            (2, |record: &mut IndexRecord| record.decode_time += 1),
            (2, |record: &mut IndexRecord| record.duration += 1),
            (2, |record: &mut IndexRecord| record.sample_count += 1),
        ] {
            let mut changed = records.clone();
            mutate(&mut changed[index_to_change]);
            assert_fallback(header, &changed);
        }
    }

    /// An adopted recording's row carries the bounds of its fragments,
    /// whether they were read from a sidecar that matches the media or from
    /// a scan: three fragments of 6,000 ticks stamped 66,667 us apart.
    #[tokio::test]
    async fn an_adopted_recording_carries_the_bounds_of_its_fragments() {
        for with_sidecar in [true, false] {
            let dir = tempfile::tempdir().unwrap();
            let root = dir.path().join("video");
            let catalog = CatalogHandle::open(&dir.path().join("catalog.db")).unwrap();
            let approved = approve_root(&catalog, &root).await;
            let path = sealed_recording(&root);
            if !with_sidecar {
                std::fs::remove_file(path.with_extension("idx")).unwrap();
            }
            assert_eq!(adopt_unknown(&catalog, &[approved]).await.unwrap(), 1);
            let row = catalog.segment_by_path(&path).await.unwrap().unwrap();
            let start = 1_700_000_000_000_000;
            assert_eq!(row.wall_start, WallUs::new(start), "{with_sidecar}");
            // The last fragment starts 133,334 us in and lasts 66,666 us.
            assert_eq!(row.wall_end, WallUs::new(start + 200_000), "{with_sidecar}");
            assert_eq!((row.media_start, row.media_end), (0, 18_000));
            assert_eq!(row.fragments, 3);
            assert!(row.sealed);
            assert_eq!(row.bytes, std::fs::metadata(&path).unwrap().len());
            catalog.shutdown();
        }
    }

    #[tokio::test]
    async fn forged_old_cache_times_cannot_make_adopted_media_age_expired() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("video");
        let catalog = CatalogHandle::open(&dir.path().join("catalog.db")).unwrap();
        let approved = approve_root(&catalog, &root).await;
        let path = sealed_recording(&root);
        let expected = scan_summary(&path).unwrap().unwrap();
        let index = path.with_extension("idx");
        let (mut header, mut records) =
            iris_mp4::index::read_index_bounded(&index, MAX_PERSISTED_INDEX_RECORDS).unwrap();
        header.start_wall = WallUs::new(1);
        records[0].wall = WallUs::new(1);
        records.last_mut().unwrap().wall = WallUs::new(2);
        let mut bytes = header.encode().to_vec();
        for record in records {
            bytes.extend(record.encode());
        }
        std::fs::write(&index, bytes).unwrap();

        assert_eq!(adopt_unknown(&catalog, &[approved]).await.unwrap(), 1);
        let row = catalog.segment_by_path(&path).await.unwrap().unwrap();
        assert_eq!(row.wall_start, expected.wall_start);
        assert_eq!(row.wall_end, expected.wall_end);
        let page = catalog
            .expired_candidates(
                &row.key,
                WallUs::new(1_000_000_000_000_000),
                None,
                WallUs::new(2_000_000_000_000_000),
                Duration::ZERO,
                10,
            )
            .await
            .unwrap();
        assert_eq!(page.rows, Vec::<SegmentRow>::new());
        catalog.shutdown();
    }

    fn timeline_fragment(decode_time: u64, duration: u32) -> reader::FragmentInfo {
        reader::FragmentInfo {
            offset: 700,
            len: 100,
            payload_offset: 740,
            sequence: 1,
            decode_time,
            duration,
            sample_count: 1,
            starts_with_sync: true,
            wall: None,
            sample_durations: Vec::new(),
            sample_sizes: Vec::new(),
        }
    }

    #[tokio::test]
    async fn scan_and_index_adoption_reject_unrepresentable_catalog_times_and_round_trip_the_limit()
    {
        for keep_index in [false, true] {
            for (media_start, accepted) in [
                (i64::MAX as u64 - 18_000, true),
                (i64::MAX as u64 - 17_999, false),
            ] {
                let dir = tempfile::tempdir().unwrap();
                let root = dir.path().join("video");
                let catalog = CatalogHandle::open(&dir.path().join("catalog.db")).unwrap();
                let approved = approve_root(&catalog, &root).await;
                let path = sealed_recording_at(&root, media_start);
                if !keep_index {
                    std::fs::remove_file(path.with_extension("idx")).unwrap();
                }

                assert_eq!(
                    adopt_unknown(&catalog, &[approved]).await.unwrap(),
                    usize::from(accepted)
                );
                let row = catalog.segment_by_path(&path).await.unwrap();
                if accepted {
                    let row = row.expect("largest signed catalog time is accepted");
                    assert_eq!(row.media_start, media_start);
                    assert_eq!(row.media_end, i64::MAX as u64);
                } else {
                    assert!(
                        row.is_none(),
                        "an unrepresentable timeline must not be persisted"
                    );
                }
                catalog.shutdown();
            }
        }
    }

    #[tokio::test]
    async fn writing_recovery_rejects_unrepresentable_catalog_end_and_round_trips_the_limit() {
        for (media_start, accepted) in [
            (i64::MAX as u64 - 18_000, true),
            (i64::MAX as u64 - 17_999, false),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let root = dir.path().join("video");
            let catalog = CatalogHandle::open(&dir.path().join("catalog.db")).unwrap();
            let approved = approve_root(&catalog, &root).await;
            let path = sealed_recording_at(&root, media_start);
            register_writing_at(&catalog, path.clone(), media_start);

            assert_eq!(
                recover_writing(&catalog, &[approved]).await.unwrap(),
                usize::from(accepted)
            );
            let row = catalog.segment_by_path(&path).await.unwrap().unwrap();
            assert_eq!(row.media_start, media_start);
            if accepted {
                assert!(row.sealed);
                assert_eq!(row.media_end, i64::MAX as u64);
            } else {
                assert!(!row.sealed);
                assert_ne!(row.media_end, i64::MAX as u64);
            }
            catalog.shutdown();
        }
    }

    #[test]
    fn scan_and_index_adoption_share_checked_timeline_boundaries() {
        let largest = timeline_fragment(i64::MAX as u64 - 1, 1);
        let largest_report = ScanReport {
            init_len: 700,
            fragments: vec![largest.clone()],
            valid_len: 800,
            stop: None,
        };
        let summary = summarize_report(&largest_report).unwrap().unwrap();
        assert_eq!(summary.media_end, i64::MAX as u64);
        let header = IndexHeader {
            init_len: 700,
            ..IndexHeader::default()
        };
        let records = [IndexRecord::from_scan(&largest)];
        assert_eq!(
            summarize_validated_index(&header, &records, &largest_report, 800)
                .unwrap()
                .media_end,
            i64::MAX as u64
        );

        let maximum = ScanReport {
            fragments: vec![timeline_fragment(u64::MAX, 1)],
            ..largest_report.clone()
        };
        assert!(summarize_report(&maximum).is_err());
        let maximum_records = [IndexRecord::from_scan(&maximum.fragments[0])];
        assert!(summarize_validated_index(&header, &maximum_records, &maximum, 800).is_none());

        let decreasing = ScanReport {
            fragments: vec![timeline_fragment(3_000, 1), timeline_fragment(0, 1)],
            ..largest_report
        };
        assert!(summarize_report(&decreasing).is_err());
        let decreasing_records: Vec<_> = decreasing
            .fragments
            .iter()
            .map(IndexRecord::from_scan)
            .collect();
        assert!(
            summarize_validated_index(&header, &decreasing_records, &decreasing, 900).is_none()
        );

        let wide = ScanReport {
            fragments: vec![
                timeline_fragment(0, 0),
                timeline_fragment(i64::MAX as u64 + 1, 0),
            ],
            init_len: 700,
            valid_len: 900,
            stop: None,
        };
        assert!(summarize_report(&wide).is_err());
        let wide_records: Vec<_> = wide.fragments.iter().map(IndexRecord::from_scan).collect();
        assert!(summarize_validated_index(&header, &wide_records, &wide, 900).is_none());
    }

    #[test]
    fn parses_the_recording_layout() {
        let root = Path::new("/srv/irisd/video");
        let key = parse_segment_path(
            root,
            Path::new("/srv/irisd/video/cam1/main/2026-10-31/172119Z.mp4"),
        )
        .unwrap();
        assert_eq!(key.camera.as_str(), "cam1");
        assert_eq!(key.kind, StreamKind::Main);
        assert!(parse_segment_path(root, Path::new("/srv/irisd/video/cam1/main/x.mp4")).is_none());
        assert!(
            parse_segment_path(
                root,
                Path::new("/srv/irisd/video/cam1/bogus/2026-10-31/a.mp4")
            )
            .is_none()
        );
        assert!(
            parse_segment_path(
                root,
                Path::new("/srv/irisd/video/cam1/main/2026-10-31/a.idx")
            )
            .is_none()
        );
        assert!(
            parse_segment_path(root, Path::new("/elsewhere/cam1/main/2026-10-31/a.mp4")).is_none()
        );
    }

    /// Walks an admitted root with the production walker and collects what it
    /// sends.
    async fn walked(approved: &ApprovedRoot) -> Vec<WalkItem> {
        let approved = approved.clone();
        let (sender, mut receiver) = mpsc::channel(64);
        let walk = tokio::task::spawn_blocking(move || walk_recordings_beneath(&approved, &sender));
        let mut items = Vec::new();
        while let Some(item) = receiver.recv().await {
            items.push(item);
        }
        walk.await.unwrap();
        items
    }

    #[tokio::test]
    async fn the_walk_stops_at_the_layout_depth() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("video");
        let approved = admit_recording_root(&root, RootExpectation::Fresh).unwrap();
        let deep = root.join("cam1/main/2026-10-31");
        std::fs::create_dir_all(&deep).unwrap();
        std::fs::write(deep.join("a.mp4"), b"x").unwrap();
        std::fs::write(deep.join("a.idx"), b"x").unwrap();
        let too_deep = deep.join("nested");
        std::fs::create_dir_all(&too_deep).unwrap();
        std::fs::write(too_deep.join("b.mp4"), b"x").unwrap();
        let found: Vec<PathBuf> = walked(&approved)
            .await
            .into_iter()
            .map(|item| item.unwrap())
            .collect();
        assert_eq!(found, vec![deep.join("a.mp4")]);
        assert_eq!(recordings_under(&root), found);
    }

    #[tokio::test]
    async fn an_unreadable_directory_is_reported_and_its_siblings_are_still_walked() {
        use std::os::unix::fs::PermissionsExt as _;

        if rustix::process::geteuid().is_root() {
            return; // Root reads a directory whatever its mode.
        }
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("video");
        let approved = admit_recording_root(&root, RootExpectation::Fresh).unwrap();
        let readable = root.join("cam1/main/2026-01-01");
        let unreadable = root.join("cam1/main/2026-01-02");
        std::fs::create_dir_all(&readable).unwrap();
        std::fs::create_dir_all(&unreadable).unwrap();
        std::fs::write(readable.join("a.mp4"), b"x").unwrap();
        std::fs::write(unreadable.join("b.mp4"), b"x").unwrap();
        std::fs::set_permissions(&unreadable, std::fs::Permissions::from_mode(0o000)).unwrap();
        let items = walked(&approved).await;
        std::fs::set_permissions(&unreadable, std::fs::Permissions::from_mode(0o700)).unwrap();
        assert_eq!(items.len(), 2, "{items:?}");
        let found: Vec<&PathBuf> = items.iter().filter_map(|item| item.as_ref().ok()).collect();
        assert_eq!(found, [&readable.join("a.mp4")]);
        let failure = items
            .iter()
            .find_map(|item| item.as_ref().err())
            .expect("the unreadable directory is reported");
        assert_eq!(failure.path, unreadable);
        assert_eq!(failure.error.kind(), io::ErrorKind::PermissionDenied);
    }

    #[test]
    fn unknown_entry_types_are_classified_by_stat() {
        assert_eq!(
            entry_kind(FileType::Unknown, || Ok(FileType::RegularFile)).unwrap(),
            FileType::RegularFile
        );
        assert_eq!(
            entry_kind(FileType::Directory, || panic!(
                "a reported type needs no stat"
            ))
            .unwrap(),
            FileType::Directory
        );
        assert_eq!(
            entry_kind(FileType::Unknown, || Err(rustix::io::Errno::NOENT)).unwrap_err(),
            rustix::io::Errno::NOENT
        );
    }

    #[tokio::test]
    async fn production_walker_keeps_fixed_in_flight_state_and_emits_each_file_once() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("video");
        let approved = admit_recording_root(&root, RootExpectation::Fresh).unwrap();
        let date = root.join("cam1/main/2026-01-01");
        std::fs::create_dir_all(&date).unwrap();
        for index in 0..100 {
            std::fs::write(date.join(format!("{index:04}.mp4")), b"synthetic").unwrap();
        }

        let (sender, mut receiver) = mpsc::channel(3);
        let walk_root = approved.clone();
        let walk =
            tokio::task::spawn_blocking(move || walk_recordings_beneath(&walk_root, &sender));
        tokio::time::timeout(Duration::from_secs(1), async {
            while receiver.len() < 3 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(receiver.len(), 3);
        assert!(
            !walk.is_finished(),
            "the bounded sender must apply backpressure"
        );

        let mut found = Vec::new();
        while let Some(item) = receiver.recv().await {
            found.push(item.unwrap());
        }
        walk.await.unwrap();
        found.sort();
        found.dedup();
        assert_eq!(found.len(), 100);
    }

    fn mkfifo(path: &Path) {
        rustix::fs::mkfifoat(rustix::fs::CWD, path, Mode::RUSR | Mode::WUSR).unwrap();
    }

    /// Awaits `future` for at most two seconds. A blocking task stuck opening
    /// `fifo` is released before the test fails, so the runtime can shut down
    /// and the test never hangs.
    async fn within_deadline<F: Future>(fifo: &Path, future: F) -> F::Output {
        if let Ok(output) = tokio::time::timeout(Duration::from_secs(2), future).await {
            return output;
        }
        for _ in 0..20 {
            let _ = rustix::fs::openat(
                rustix::fs::CWD,
                fifo,
                OFlags::WRONLY | OFlags::NONBLOCK,
                Mode::empty(),
            );
            std::thread::sleep(Duration::from_millis(50));
        }
        panic!(
            "reconciliation blocked on {} for two seconds",
            fifo.display()
        );
    }

    fn sidecar_binding(index: &Path) -> (u64, u32) {
        let (header, _) =
            iris_mp4::index::read_index_bounded(index, MAX_PERSISTED_INDEX_RECORDS).unwrap();
        (header.run_id, header.sample_entry_hash)
    }

    #[tokio::test]
    async fn a_recovered_segment_keeps_the_catalog_binding_in_its_sidecar() {
        // How the sidecar looks after the unclean stop: intact, rebuilt with
        // zeros by an earlier recovery, lost, or a planted FIFO.
        for damage in ["intact", "zeroed", "missing", "fifo"] {
            let dir = tempfile::tempdir().unwrap();
            let root = dir.path().join("video");
            let catalog = CatalogHandle::open(&dir.path().join("catalog.db")).unwrap();
            let approved = approve_root(&catalog, &root).await;
            let path = sealed_recording(&root);
            let index = path.with_extension("idx");
            register_writing(&catalog, path.clone());
            let row = catalog.segment_by_path(&path).await.unwrap().unwrap();
            assert_eq!(
                sidecar_binding(&index),
                (row.run_seq, row.sample_entry_hash)
            );
            assert_ne!(row.sample_entry_hash, 0);
            match damage {
                "intact" => {}
                "zeroed" => {
                    let (mut header, records) =
                        iris_mp4::index::read_index_bounded(&index, MAX_PERSISTED_INDEX_RECORDS)
                            .unwrap();
                    header.run_id = 0;
                    header.sample_entry_hash = 0;
                    let mut bytes = header.encode().to_vec();
                    for record in records {
                        bytes.extend(record.encode());
                    }
                    std::fs::write(&index, bytes).unwrap();
                }
                "missing" => std::fs::remove_file(&index).unwrap(),
                _ => {
                    std::fs::remove_file(&index).unwrap();
                    mkfifo(&index);
                }
            }

            let recovered = within_deadline(
                &index,
                recover_writing(&catalog, std::slice::from_ref(&approved)),
            )
            .await
            .unwrap();
            assert_eq!(recovered, 1, "{damage}");
            let sealed = catalog.segment_by_path(&path).await.unwrap().unwrap();
            assert!(sealed.sealed, "{damage}");
            assert!(std::fs::symlink_metadata(&index).unwrap().is_file());
            assert_eq!(
                sidecar_binding(&index),
                (sealed.run_seq, sealed.sample_entry_hash),
                "{damage}"
            );
            catalog.shutdown();
        }
    }

    #[tokio::test]
    async fn adoption_does_not_block_on_a_fifo_sidecar() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("video");
        let catalog = CatalogHandle::open(&dir.path().join("catalog.db")).unwrap();
        let approved = approve_root(&catalog, &root).await;
        let path = sealed_recording(&root);
        let index = path.with_extension("idx");
        std::fs::remove_file(&index).unwrap();
        mkfifo(&index);

        let adopted = within_deadline(
            &index,
            adopt_unknown(&catalog, std::slice::from_ref(&approved)),
        )
        .await
        .unwrap();
        assert_eq!(adopted, 1, "the media is adopted by scanning it");
        let row = catalog.segment_by_path(&path).await.unwrap().unwrap();
        assert_eq!(row.fragments, 3);
        catalog.shutdown();
    }

    #[tokio::test]
    async fn deletion_does_not_block_on_a_fifo_sidecar() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("video");
        let catalog = CatalogHandle::open(&dir.path().join("catalog.db")).unwrap();
        let approved = approve_root(&catalog, &root).await;
        let path = sealed_recording(&root);
        let index = path.with_extension("idx");
        let id = register_sealed(&catalog, path.clone()).await;
        std::fs::remove_file(&index).unwrap();
        mkfifo(&index);

        assert_eq!(
            catalog.mark_deleting(id).await.unwrap(),
            DeleteMarkResult::Marked
        );
        let finished = within_deadline(
            &index,
            finish_deleting(&catalog, std::slice::from_ref(&approved)),
        )
        .await
        .unwrap();
        assert_eq!(finished, 1);
        assert!(std::fs::symlink_metadata(&path).is_err());
        assert!(std::fs::symlink_metadata(&index).is_err());
        assert_eq!(catalog.deleting_segments().await.unwrap(), Vec::new());
        catalog.shutdown();
    }

    #[tokio::test]
    async fn adoption_scans_media_beside_a_symlinked_sidecar() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("video");
        let catalog = CatalogHandle::open(&dir.path().join("catalog.db")).unwrap();
        let approved = approve_root(&catalog, &root).await;
        let path = sealed_recording(&root);
        let index = path.with_extension("idx");
        let outside = dir.path().join("outside.idx");
        std::fs::rename(&index, &outside).unwrap();
        std::os::unix::fs::symlink(&outside, &index).unwrap();

        assert_eq!(adopt_unknown(&catalog, &[approved]).await.unwrap(), 1);
        assert_eq!(
            catalog
                .segment_by_path(&path)
                .await
                .unwrap()
                .unwrap()
                .fragments,
            3
        );
        catalog.shutdown();
    }

    #[test]
    fn adoption_refuses_a_file_beyond_the_fragment_limit() {
        let dir = tempfile::tempdir().unwrap();
        let path = sealed_recording(dir.path());
        let open = || {
            (
                File::open(&path).unwrap(),
                Some(File::open(path.with_extension("idx")).unwrap()),
            )
        };
        let (media, index) = open();
        let error = summarize_open_pair_limited(media, index, &path, 2).unwrap_err();
        assert!(
            error.to_string().contains("more than 2 fragments"),
            "{error:#}"
        );
        let (media, index) = open();
        let summary = summarize_open_pair_limited(media, index, &path, 3)
            .unwrap()
            .unwrap();
        assert_eq!(summary.fragments, 3);
    }

    #[tokio::test]
    async fn a_missing_layout_directory_completes_empty_cleanup() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("video");
        let catalog = CatalogHandle::open(&dir.path().join("catalog.db")).unwrap();
        let approved = approve_root(&catalog, &root).await;
        let open = root.join("cam1/main/2026-01-01/never-created.mp4");
        register_writing(&catalog, open.clone());
        assert_eq!(
            recover_writing(&catalog, std::slice::from_ref(&approved))
                .await
                .unwrap(),
            0
        );
        assert!(catalog.segment_by_path(&open).await.unwrap().is_none());
        assert_eq!(catalog.writing_segments().await.unwrap(), Vec::new());
        catalog.shutdown();
    }

    #[tokio::test]
    async fn a_missing_layout_directory_completes_deletion() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("video");
        let catalog = CatalogHandle::open(&dir.path().join("catalog.db")).unwrap();
        let approved = approve_root(&catalog, &root).await;
        let gone = root.join("cam1/main/2026-01-02/gone.mp4");
        let id = register_sealed(&catalog, gone).await;
        assert_eq!(
            catalog.mark_deleting(id).await.unwrap(),
            DeleteMarkResult::Marked
        );
        assert_eq!(finish_deleting(&catalog, &[approved]).await.unwrap(), 1);
        assert_eq!(catalog.deleting_segments().await.unwrap(), Vec::new());
        catalog.shutdown();
    }

    #[tokio::test]
    async fn missing_open_media_with_fragments_drops_its_row() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("video");
        let catalog = CatalogHandle::open(&dir.path().join("catalog.db")).unwrap();
        let approved = approve_root(&catalog, &root).await;
        let path = root.join("cam1/main/2026-01-01/lost.mp4");
        let orphan = path.with_extension("idx");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&orphan, b"orphan sidecar").unwrap();
        register_writing(&catalog, path.clone());
        catalog.notify(iris_catalog::Change::SegmentProgress {
            path: path.clone(),
            durable_len: 5_000_000,
            fragments: 3,
            wall_end: WallUs::new(1_700_000_003_000_000),
            media_end: 270_000,
        });
        assert_eq!(
            catalog
                .segment_by_path(&path)
                .await
                .unwrap()
                .unwrap()
                .fragments,
            3
        );

        assert_eq!(recover_writing(&catalog, &[approved]).await.unwrap(), 0);
        assert!(catalog.segment_by_path(&path).await.unwrap().is_none());
        assert_eq!(catalog.writing_segments().await.unwrap(), Vec::new());
        assert_eq!(catalog.deleting_segments().await.unwrap(), Vec::new());
        assert_eq!(catalog.total_bytes().await.unwrap(), 0);
        assert!(!orphan.exists());
        catalog.shutdown();
    }

    #[tokio::test(start_paused = true)]
    async fn a_long_wait_emits_heartbeats_at_the_cadence() {
        let mut beats = 0;
        let start = Instant::now();
        let output = heartbeat_from(
            start,
            Duration::from_millis(10),
            tokio::time::sleep(Duration::from_millis(35)),
            || beats += 1,
        )
        .await;
        assert_eq!(output, ());
        assert_eq!(beats, 3, "ticks at 10, 20, and 30 ms");
    }

    #[tokio::test(start_paused = true)]
    async fn short_waits_keep_the_heartbeat_cadence_between_them() {
        // Seven 3 ms awaits span 21 ms. The cadence is counted from the last
        // report, which the beat refreshes, so the ticks fall at 10 and 20 ms.
        let mut last = Instant::now();
        let mut beats = 0;
        for _ in 0..7 {
            heartbeat_from(
                last,
                Duration::from_millis(10),
                tokio::time::sleep(Duration::from_millis(3)),
                || {
                    beats += 1;
                    last = Instant::now();
                },
            )
            .await;
        }
        assert_eq!(beats, 2);
    }
}
