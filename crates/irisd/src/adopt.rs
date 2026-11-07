//! Startup reconciliation between the catalog and the files on disk:
//! recovery of segments that were open when the daemon last stopped, and
//! adoption of files the catalog does not know.

use std::future::Future;
use std::path::{Path, PathBuf};
use std::{fs::File, mem::MaybeUninit};

use anyhow::Context;
use iris_catalog::{CatalogHandle, DeleteMarkResult, NewSegment};
use iris_core::id::{CameraId, StreamKey, StreamKind};
use iris_core::time::WallUs;
use iris_mp4::index::{
    IndexHeader, IndexRecord, MAX_PERSISTED_INDEX_RECORDS, read_index_file_bound_to_media,
};
use iris_mp4::reader::{ScanOptions, ScanReport, scan};
use iris_store::recover::{RecoverError, recover_segment_beneath};
use iris_store::{
    ApprovedRoot, open_approved_root_directory, open_segment_pair_beneath, remove_segment_beneath,
};
use tracing::{debug, info, warn};

use crate::systemd;

/// Files between two start-up status reports while adopting.
const ADOPT_STATUS_EVERY: usize = 50;
/// Extra start time requested from systemd at each status report.
const ADOPT_TIMEOUT_EXTENSION: std::time::Duration = std::time::Duration::from_secs(60);
const RECONCILE_PAGE: usize = 256;
const RECONCILE_HEARTBEAT: std::time::Duration = std::time::Duration::from_secs(5);

/// Finishes catalog deletion intents under available recording roots. Rows on
/// unavailable roots or behind filesystem errors remain for a later startup.
pub async fn finish_deleting(
    catalog: &CatalogHandle,
    roots: &[ApprovedRoot],
) -> anyhow::Result<usize> {
    let mut finished = 0;
    let mut cursor = None;
    let mut processed = 0usize;
    startup_progress("finishing deletion intents", processed, finished);
    loop {
        let rows = heartbeat_while(
            "finishing deletion intents",
            processed,
            finished,
            catalog.deleting_segments_page(cursor, RECONCILE_PAGE),
        )
        .await
        .context("listing deleting segments")?;
        if rows.is_empty() {
            break;
        }
        for (row, persisted_root) in rows {
            cursor = Some((row.wall_start, row.id));
            processed += 1;
            let path = row.path.clone();
            let Some(root) = approved_root_for_row(roots, persisted_root.as_deref(), &row) else {
                warn!(path = %path.display(), "deleting segment root/path is unavailable or inconsistent; keeping its row");
                continue;
            };
            let root = root.clone();
            let removal_path = path.clone();
            match heartbeat_while(
                "finishing deletion intents",
                processed,
                finished,
                tokio::task::spawn_blocking(move || remove_segment_beneath(&root, &removal_path)),
            )
            .await
            {
                Ok(Ok(_)) => {
                    post_filesystem_catalog_wait(
                        "finishing deletion intents",
                        processed,
                        finished,
                        catalog.delete_segment(row.id),
                    )
                    .await
                    .context("deleting a completed segment tombstone")?;
                    finished += 1;
                    info!(path = %path.display(), "finished interrupted segment deletion");
                }
                Ok(Err(error)) => {
                    warn!(path = %path.display(), %error, "cannot finish segment deletion");
                }
                Err(error) => warn!(%error, "segment deletion task panicked"),
            }
        }
        startup_progress("finishing deletion intents", processed, finished);
    }
    Ok(finished)
}

fn startup_progress(phase: &str, processed: usize, changed: usize) {
    systemd::status(&format!(
        "reconciling recordings: {phase}; {processed} inspected, {changed} changed"
    ));
    systemd::extend_timeout(ADOPT_TIMEOUT_EXTENSION);
}

async fn heartbeat_while<F: Future>(
    phase: &str,
    processed: usize,
    changed: usize,
    future: F,
) -> F::Output {
    heartbeat_while_with(future, RECONCILE_HEARTBEAT, || {
        startup_progress(phase, processed, changed);
    })
    .await
}

async fn heartbeat_while_with<F: Future>(
    future: F,
    interval: std::time::Duration,
    mut heartbeat: impl FnMut(),
) -> F::Output {
    let mut ticker = tokio::time::interval(interval);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    ticker.tick().await;
    tokio::pin!(future);
    loop {
        tokio::select! {
            output = &mut future => return output,
            _ = ticker.tick() => heartbeat(),
        }
    }
}

async fn post_filesystem_catalog_wait<F: Future>(
    phase: &str,
    processed: usize,
    changed: usize,
    future: F,
) -> F::Output {
    post_filesystem_catalog_wait_with(future, RECONCILE_HEARTBEAT, || {
        startup_progress(phase, processed, changed);
    })
    .await
}

async fn post_filesystem_catalog_wait_with<F: Future>(
    future: F,
    interval: std::time::Duration,
    heartbeat: impl FnMut(),
) -> F::Output {
    heartbeat_while_with(future, interval, heartbeat).await
}

fn approved_root_for_row<'a>(
    roots: &'a [ApprovedRoot],
    persisted_root: Option<&Path>,
    row: &iris_catalog::SegmentRow,
) -> Option<&'a ApprovedRoot> {
    let persisted_root = persisted_root?;
    let root = roots.iter().find(|root| root.path() == persisted_root)?;
    let parsed = parse_segment_path(root.path(), &row.path)?;
    (parsed == row.key).then_some(root)
}

/// Facts about a file derived from a scan.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ScanSummary {
    /// Bytes from the start that hold valid fragments.
    pub valid_len: u64,
    /// Number of valid fragments.
    pub fragments: u32,
    /// Length of `ftyp` + `moov`.
    pub init_len: u32,
    /// Wall time of the first fragment (zero if no `prft`).
    pub wall_start: WallUs,
    /// Wall time at the end of the last fragment (zero if no `prft`).
    pub wall_end: WallUs,
    /// Decode time of the first fragment.
    pub media_start: u64,
    /// Decode time at the end of the last fragment.
    pub media_end: u64,
}

/// Parses `<root>/<camera>/<kind>/<date>/<time>Z.mp4` into its stream key.
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
    let Component::Normal(file) = components.next()? else {
        return None;
    };
    let camera: CameraId = camera.to_str()?.parse().ok()?;
    let kind: StreamKind = kind.to_str()?.parse().ok()?;
    if components.next().is_some() || path.extension().is_none_or(|e| e != "mp4") {
        return None;
    }
    if Path::new(file)
        .extension()
        .is_none_or(|extension| extension != "mp4")
    {
        return None;
    }
    Some(StreamKey { camera, kind })
}

/// Summarises a file: from its sidecar index when the index and the file
/// agree, which reads a few kilobytes, else by scanning the whole file.
/// `None` when it holds no valid fragment.
#[cfg(test)]
pub fn summarize(path: &Path) -> anyhow::Result<Option<ScanSummary>> {
    let media = File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let index = File::open(path.with_extension("idx")).ok();
    summarize_open_pair(media, index, path)
}

/// Summarises a sealed file from its sidecar index. The index must describe
/// the file exactly: records contiguous from the end of the init segment to
/// the end of the file, and a trailer at the end of the file that matches the
/// last record. Anything else returns `None`, and the caller scans the file.
#[must_use]
#[cfg(test)]
pub fn summarize_from_index(path: &Path) -> Option<ScanSummary> {
    let mut media = File::open(path).ok()?;
    let index = File::open(path.with_extension("idx")).ok()?;
    let media_len = media.metadata().ok()?.len();
    let report = scan_authoritative(&mut media).ok()?;
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

fn scan_authoritative(media: &mut File) -> std::io::Result<ScanReport> {
    scan(
        std::io::BufReader::new(media),
        ScanOptions {
            verify_crc: false,
            collect_samples: false,
        },
    )
}

fn checked_wall_end(wall: WallUs, duration: u32) -> Option<WallUs> {
    let micros = i64::from(duration).checked_mul(100)?.div_euclid(9);
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
pub fn scan_summary(path: &Path) -> anyhow::Result<Option<ScanSummary>> {
    let file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
    scan_summary_file(file, path)
}

#[cfg(test)]
fn scan_summary_file(file: File, display_path: &Path) -> anyhow::Result<Option<ScanSummary>> {
    let report = scan(
        std::io::BufReader::new(file),
        ScanOptions {
            verify_crc: false,
            collect_samples: false,
        },
    )
    .with_context(|| format!("scanning {}", display_path.display()))?;
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
        fragments: u32::try_from(report.fragments.len()).unwrap_or(u32::MAX),
        init_len: report.init_len,
        wall_start: first.wall.unwrap_or_default(),
        wall_end: last_wall_end.unwrap_or_default(),
        media_start: first.decode_time,
        media_end,
    }))
}

fn summarize_open_pair(
    mut media: File,
    index: Option<File>,
    display_path: &Path,
) -> anyhow::Result<Option<ScanSummary>> {
    let media_len = media.metadata()?.len();
    let report = scan_authoritative(&mut media)
        .with_context(|| format!("scanning {}", display_path.display()))?;
    if let Some(index) = index {
        match read_index_file_bound_to_media(index, MAX_PERSISTED_INDEX_RECORDS, &report, media_len)
        {
            Ok(Some((header, records))) => {
                let summary = summarize_validated_index(&header, &records, &report, media_len)
                    .context("validated sidecar contains an invalid media timeline")?;
                debug!(path = %display_path.display(), "summarised from bounded media-bound sidecar index");
                return Ok(Some(summary));
            }
            Ok(None) => {
                debug!(path = %display_path.display(), "sidecar is not bound to media; using authoritative scan");
            }
            Err(error) => {
                debug!(path = %display_path.display(), %error, "sidecar is unreadable or over limit; using authoritative scan");
            }
        }
    }
    summarize_report(&report)
}

/// Recovers open segments under available recording roots. Rows on unavailable
/// roots or behind lookup errors remain for a later startup.
pub async fn recover_writing(
    catalog: &CatalogHandle,
    roots: &[ApprovedRoot],
) -> anyhow::Result<usize> {
    let mut recovered = 0;
    let mut cursor = None;
    let mut processed = 0usize;
    startup_progress("recovering open segments", processed, recovered);
    loop {
        let rows = heartbeat_while(
            "recovering open segments",
            processed,
            recovered,
            catalog.writing_segments_page(cursor, RECONCILE_PAGE),
        )
        .await
        .context("listing open segments")?;
        if rows.is_empty() {
            break;
        }
        for (row, persisted_root) in rows {
            cursor = Some((row.wall_start, row.id));
            processed += 1;
            let path = row.path.clone();
            let Some(root) = approved_root_for_row(roots, persisted_root.as_deref(), &row) else {
                warn!(path = %path.display(), "open segment root/path is unavailable or inconsistent; keeping its row");
                continue;
            };
            let report_path = path.clone();
            let recovery_root = root.clone();
            let report = heartbeat_while(
                "recovering open segments",
                processed,
                recovered,
                tokio::task::spawn_blocking(move || {
                    recover_segment_beneath(&recovery_root, &report_path)
                }),
            )
            .await
            .context("recovery task panicked")?;
            match report {
                Ok(report) => {
                    if report.fragments != 0 {
                        if let Err(error) =
                            checked_catalog_media_range(report.media_start, report.media_end)
                        {
                            warn!(path = %path.display(), %error, "recovered timeline cannot be represented in the catalog; keeping its writing row");
                            continue;
                        }
                        post_filesystem_catalog_wait(
                            "sealing a recovered segment",
                            processed,
                            recovered,
                            catalog.mark_sealed(
                                &path,
                                report.valid_len,
                                u32::try_from(report.fragments).unwrap_or(u32::MAX),
                                report.wall_end,
                                report.media_end,
                            ),
                        )
                        .await
                        .context("sealing a recovered segment")?;
                        info!(
                            path = %path.display(),
                            fragments = report.fragments,
                            truncated = report.truncated,
                            "recovered open segment"
                        );
                        recovered += 1;
                    } else {
                        warn!(path = %path.display(), "recovered segment holds no fragments; scheduling durable cleanup");
                        cleanup_empty_writing(catalog, root, &row).await?;
                    }
                }
                Err(RecoverError::Io { source, .. })
                    if source.kind() == std::io::ErrorKind::NotFound && row.fragments == 0 =>
                {
                    warn!(path = %path.display(), "empty open segment pair is incomplete; scheduling durable cleanup");
                    cleanup_empty_writing(catalog, root, &row).await?;
                }
                Err(err) => warn!(path = %path.display(), %err, "cannot recover segment"),
            }
        }
        startup_progress("recovering open segments", processed, recovered);
    }
    Ok(recovered)
}

async fn cleanup_empty_writing(
    catalog: &CatalogHandle,
    root: &ApprovedRoot,
    row: &iris_catalog::SegmentRow,
) -> anyhow::Result<()> {
    cleanup_empty_writing_with(catalog, root, row, |root, path| {
        remove_segment_beneath(&root, &path)
    })
    .await
}

async fn cleanup_empty_writing_with<F>(
    catalog: &CatalogHandle,
    root: &ApprovedRoot,
    row: &iris_catalog::SegmentRow,
    remove: F,
) -> anyhow::Result<()>
where
    F: FnOnce(ApprovedRoot, PathBuf) -> std::io::Result<u64> + Send + 'static,
{
    match post_filesystem_catalog_wait(
        "recording empty-segment cleanup intent",
        1,
        0,
        catalog.mark_empty_writing_deleting(row.id),
    )
    .await
    .context("recording durable empty-segment cleanup intent")?
    {
        DeleteMarkResult::Marked => {}
        DeleteMarkResult::NotFound => return Ok(()),
        DeleteMarkResult::Protected => {
            anyhow::bail!("empty writing segment unexpectedly reported protected")
        }
    }
    let root = root.clone();
    let path = row.path.clone();
    let removed = heartbeat_while(
        "cleaning empty open segment",
        1,
        0,
        tokio::task::spawn_blocking(move || remove(root, path)),
    )
    .await
    .context("empty-segment cleanup task panicked")?;
    match removed {
        Ok(_) => post_filesystem_catalog_wait(
            "deleting a durably removed empty segment row",
            1,
            0,
            catalog.delete_segment(row.id),
        )
        .await
        .context("deleting a durably removed empty segment row"),
        Err(error) => {
            warn!(path = %row.path.display(), %error, "empty segment cleanup remains retryable");
            Ok(())
        }
    }
}

/// Lists every `.mp4` under a recording root.
#[cfg(test)]
pub fn recordings_under(root: &Path) -> Vec<PathBuf> {
    let mut found: Vec<_> = RecordingWalker::new(root)
        .into_iter()
        .flatten()
        .flatten()
        .collect();
    found.sort();
    found
}

#[cfg(test)]
struct WalkFrame {
    entries: std::fs::ReadDir,
    depth: usize,
}

#[cfg(test)]
struct RecordingWalker {
    frames: Vec<WalkFrame>,
}

fn walk_recordings_beneath(
    root: &ApprovedRoot,
    sender: &tokio::sync::mpsc::Sender<std::io::Result<PathBuf>>,
) -> std::io::Result<()> {
    let directory = open_approved_root_directory(root)?;
    walk_directory_beneath(&directory, root.path(), 0, sender)
}

fn walk_directory_beneath(
    directory: &File,
    display_path: &Path,
    depth: usize,
    sender: &tokio::sync::mpsc::Sender<std::io::Result<PathBuf>>,
) -> std::io::Result<()> {
    use std::os::unix::ffi::OsStrExt as _;

    let mut buffer = [MaybeUninit::uninit(); 16 * 1024];
    let mut entries = rustix::fs::RawDir::new(directory, &mut buffer);
    while let Some(entry) = entries.next() {
        let entry = entry?;
        let bytes = entry.file_name().to_bytes();
        if bytes == b"." || bytes == b".." {
            continue;
        }
        let name = std::ffi::OsStr::from_bytes(bytes).to_owned();
        let path = display_path.join(&name);
        if entry.file_type().is_dir() && depth + 1 < 4 {
            let child = rustix::fs::openat2(
                directory,
                &name,
                rustix::fs::OFlags::RDONLY
                    | rustix::fs::OFlags::DIRECTORY
                    | rustix::fs::OFlags::NOFOLLOW
                    | rustix::fs::OFlags::CLOEXEC,
                rustix::fs::Mode::empty(),
                rustix::fs::ResolveFlags::BENEATH
                    | rustix::fs::ResolveFlags::NO_SYMLINKS
                    | rustix::fs::ResolveFlags::NO_MAGICLINKS
                    | rustix::fs::ResolveFlags::NO_XDEV,
            )
            .map(File::from);
            match child {
                Ok(child) => walk_directory_beneath(&child, &path, depth + 1, sender)?,
                Err(error) => {
                    if sender.blocking_send(Err(error.into())).is_err() {
                        return Ok(());
                    }
                }
            }
        } else if entry.file_type().is_file()
            && path.extension().is_some_and(|extension| extension == "mp4")
            && sender.blocking_send(Ok(path)).is_err()
        {
            return Ok(());
        }
    }
    Ok(())
}

#[cfg(test)]
impl RecordingWalker {
    fn new(root: &Path) -> std::io::Result<Self> {
        Ok(Self {
            frames: vec![WalkFrame {
                entries: std::fs::read_dir(root)?,
                depth: 0,
            }],
        })
    }
}

#[cfg(test)]
impl Iterator for RecordingWalker {
    type Item = std::io::Result<PathBuf>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            let frame = self.frames.last_mut()?;
            let Some(entry) = frame.entries.next() else {
                self.frames.pop();
                continue;
            };
            let entry = match entry {
                Ok(entry) => entry,
                Err(error) => return Some(Err(error)),
            };
            let file_type = match entry.file_type() {
                Ok(file_type) => file_type,
                Err(error) => return Some(Err(error)),
            };
            let path = entry.path();
            let child_depth = frame.depth + 1;
            if file_type.is_dir() && child_depth < 4 {
                match std::fs::read_dir(&path) {
                    Ok(entries) => self.frames.push(WalkFrame {
                        entries,
                        depth: child_depth,
                    }),
                    Err(error) => return Some(Err(error)),
                }
            } else if file_type.is_file() && path.extension().is_some_and(|ext| ext == "mp4") {
                return Some(Ok(path));
            }
        }
    }
}

async fn scan_discovered_recording(
    root: &ApprovedRoot,
    path: &Path,
    seen: usize,
    adopted: usize,
) -> Option<ScanSummary> {
    let scan_path = path.to_path_buf();
    let scan_root = root.clone();
    match heartbeat_while(
        "scanning discovered recording",
        seen,
        adopted,
        tokio::task::spawn_blocking(move || {
            let (media, index) =
                open_segment_pair_beneath(&scan_root, &scan_path).with_context(|| {
                    format!("opening {} beneath approved root", scan_path.display())
                })?;
            summarize_open_pair(media, index, &scan_path)
        }),
    )
    .await
    {
        Ok(Ok(Some(summary))) => Some(summary),
        Ok(Ok(None)) => {
            warn!(path = %path.display(), "no valid fragments; ignoring");
            None
        }
        Ok(Err(error)) => {
            warn!(path = %path.display(), %error, "cannot scan; ignoring");
            None
        }
        Err(error) => {
            warn!(%error, "scan task panicked");
            None
        }
    }
}

/// Adds files under the roots that the catalog does not know.
pub async fn adopt_unknown(
    catalog: &CatalogHandle,
    roots: &[ApprovedRoot],
) -> anyhow::Result<usize> {
    let mut adopted = 0;
    let mut seen = 0usize;
    for root in roots {
        startup_progress("discovering unknown recordings", seen, adopted);
        let (sender, mut receiver) = tokio::sync::mpsc::channel(64);
        let walk_root = root.clone();
        let walk = tokio::task::spawn_blocking(move || -> std::io::Result<()> {
            walk_recordings_beneath(&walk_root, &sender)
        });
        while let Some(item) = heartbeat_while(
            "discovering unknown recordings",
            seen,
            adopted,
            receiver.recv(),
        )
        .await
        {
            let path = match item {
                Ok(path) => path,
                Err(error) => {
                    warn!(root = %root.path().display(), %error, "cannot enumerate recording entry");
                    continue;
                }
            };
            seen += 1;
            if seen.is_multiple_of(ADOPT_STATUS_EVERY) {
                startup_progress("discovering unknown recordings", seen, adopted);
            }
            if heartbeat_while(
                "looking up discovered recordings",
                seen,
                adopted,
                catalog.contains_segment_path(&path),
            )
            .await
            .context("looking up a file")?
            {
                continue;
            }
            let Some(key) = parse_segment_path(root.path(), &path) else {
                warn!(path = %path.display(), "file does not follow the recording layout; ignoring");
                continue;
            };
            let Some(summary) = scan_discovered_recording(root, &path, seen, adopted).await else {
                continue;
            };
            heartbeat_while(
                "adopting discovered recording",
                seen,
                adopted,
                catalog.adopt_segment(NewSegment {
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
                }),
            )
            .await
            .with_context(|| format!("adopting {}", path.display()))?;
            info!(path = %path.display(), stream = %key, fragments = summary.fragments, "adopted recording");
            adopted += 1;
        }
        walk.await
            .context("directory walk panicked")?
            .with_context(|| format!("walking {}", root.path().display()))?;
    }
    Ok(adopted)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::time::Duration;

    use bytes::Bytes;
    use iris_catalog::{CatalogRootExpectation, CatalogRootRegistry};
    use iris_mp4::fragment::{Fragment, Sample};
    use iris_mp4::init::SampleEntry;
    use iris_store::{RootExpectation, StoragePolicy, WriterHandle, admit_recording_root};

    async fn approve_root(catalog: &CatalogHandle, root: &Path) -> ApprovedRoot {
        let approved = admit_recording_root(root, RootExpectation::Fresh).unwrap();
        catalog
            .register_recording_root(root, &approved.identity().to_string())
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
            catalog.mark_deleting(id).await.unwrap();
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

            assert_eq!(finish_deleting(&catalog, &[approved]).await.unwrap(), 1);
            assert!(!path.exists());
            assert!(!index.exists());
            assert_eq!(
                catalog.deleting_segments().await.unwrap(),
                Vec::<iris_catalog::SegmentRow>::new()
            );
            assert_eq!(catalog.total_bytes().await.unwrap(), 0);
            catalog.shutdown();
        }
    }

    #[tokio::test]
    async fn startup_leaves_normal_rows_and_unavailable_deletion_intents_alone() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("video");
        let unavailable = dir.path().join("unavailable");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&unavailable).unwrap();
        let catalog = CatalogHandle::open(&dir.path().join("catalog.db")).unwrap();
        let approved = approve_root(&catalog, &root).await;

        let retained = root.join("cam1/main/2026-01-01/retained.mp4");
        std::fs::create_dir_all(retained.parent().unwrap()).unwrap();
        std::fs::write(&retained, b"media").unwrap();
        register_sealed(&catalog, retained.clone()).await;
        let pending = unavailable.join("pending.mp4");
        std::fs::write(&pending, b"media").unwrap();
        let pending_id = register_sealed(&catalog, pending.clone()).await;
        catalog.mark_deleting(pending_id).await.unwrap();

        assert_eq!(finish_deleting(&catalog, &[approved]).await.unwrap(), 0);
        assert!(retained.exists());
        assert!(catalog.segment_by_path(&retained).await.unwrap().is_some());
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
        catalog.mark_deleting(id).await.unwrap();

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
            .register_recording_root(&root, &expected_identity.to_string())
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
            CatalogRootExpectation::Known(expected_identity.to_string())
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
        catalog.mark_deleting(linked_id).await.unwrap();
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
        catalog.mark_deleting(parent_id).await.unwrap();
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
    async fn empty_recovery_keeps_a_durable_tombstone_when_pair_removal_fails() {
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
        #[derive(Clone, Copy)]
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

            cleanup_empty_writing_with(&catalog, &approved, &row, move |_, _| {
                match fault {
                    Fault::FirstUnlink => {}
                    Fault::SecondUnlink => std::fs::remove_file(&media_for_fault)?,
                    Fault::DirectorySync => {
                        std::fs::remove_file(&media_for_fault)?;
                        std::fs::remove_file(&index_for_fault)?;
                    }
                }
                Err(std::io::Error::other(
                    "injected empty-cleanup boundary failure",
                ))
            })
            .await
            .unwrap();

            let deleting = catalog.deleting_segments().await.unwrap();
            assert_eq!(deleting.len(), 1);
            assert_eq!(deleting[0].id, row.id);
            assert_eq!(
                finish_deleting(&catalog, std::slice::from_ref(&approved))
                    .await
                    .unwrap(),
                1
            );
            assert_eq!(finish_deleting(&catalog, &[approved]).await.unwrap(), 0);
            assert!(!path.exists());
            assert!(!index.exists());
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
            rotate_secs: 3_600,
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
    fn index_summary_equals_the_scan_and_rejects_a_disagreeing_index() {
        let dir = tempfile::tempdir().unwrap();
        let path = sealed_recording(dir.path());
        let by_index = summarize_from_index(&path).expect("sealed file with a sidecar");
        let by_scan = scan_summary(&path).unwrap().expect("valid fragments");
        assert_eq!(by_index, by_scan);
        assert_eq!(by_index.fragments, 3);
        assert_eq!(summarize(&path).unwrap(), Some(by_index));

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
        assert_eq!(page.rows, Vec::<iris_catalog::SegmentRow>::new());
        catalog.shutdown();
    }

    fn timeline_fragment(decode_time: u64, duration: u32) -> iris_mp4::reader::FragmentInfo {
        iris_mp4::reader::FragmentInfo {
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

    #[test]
    fn walks_only_the_layout_depth() {
        let dir = tempfile::tempdir().unwrap();
        let deep = dir.path().join("cam1/main/2026-10-31");
        std::fs::create_dir_all(&deep).unwrap();
        std::fs::write(deep.join("a.mp4"), b"x").unwrap();
        std::fs::write(deep.join("a.idx"), b"x").unwrap();
        let too_deep = deep.join("nested");
        std::fs::create_dir_all(&too_deep).unwrap();
        std::fs::write(too_deep.join("b.mp4"), b"x").unwrap();
        let found = recordings_under(dir.path());
        assert_eq!(found, vec![deep.join("a.mp4")]);
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

        let (sender, mut receiver) = tokio::sync::mpsc::channel(3);
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
        walk.await.unwrap().unwrap();
        found.sort();
        found.dedup();
        assert_eq!(found.len(), 100);
    }

    #[tokio::test]
    async fn a_post_filesystem_catalog_wait_emits_repeated_elapsed_heartbeats() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let heartbeats = Arc::new(AtomicUsize::new(0));
        let observed = Arc::clone(&heartbeats);
        let (release, gate) = tokio::sync::oneshot::channel();
        let wait = tokio::spawn(async move {
            post_filesystem_catalog_wait_with(gate, Duration::from_millis(10), move || {
                observed.fetch_add(1, Ordering::Relaxed);
            })
            .await
        });
        tokio::time::timeout(Duration::from_secs(1), async {
            while heartbeats.load(Ordering::Relaxed) < 3 {
                tokio::task::yield_now().await;
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .unwrap();
        assert!(!wait.is_finished());
        release.send(7).unwrap();
        assert_eq!(wait.await.unwrap().unwrap(), 7);
    }
}
