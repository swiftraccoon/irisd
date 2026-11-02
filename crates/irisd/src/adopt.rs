//! Startup reconciliation between the catalog and the files on disk:
//! recovery of segments that were open when the daemon last stopped, and
//! adoption of files the catalog does not know.

use std::io::{Read, Seek};
use std::path::{Path, PathBuf};

use anyhow::Context;
use iris_catalog::{CatalogHandle, NewSegment};
use iris_core::id::{CameraId, StreamKey, StreamKind};
use iris_core::time::{MediaTicks, WallUs};
use iris_mp4::fragment::{TRAILER_LEN, decode_trailer};
use iris_mp4::index::read_index;
use iris_mp4::reader::{ScanOptions, scan};
use iris_store::recover::recover_segment;
use tracing::{debug, info, warn};

use crate::systemd;

/// Files between two start-up status reports while adopting.
const ADOPT_STATUS_EVERY: usize = 50;
/// Extra start time requested from systemd at each status report.
const ADOPT_TIMEOUT_EXTENSION: std::time::Duration = std::time::Duration::from_secs(60);

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
    let relative = path.strip_prefix(root).ok()?;
    let mut components = relative.components();
    let camera: CameraId = components.next()?.as_os_str().to_str()?.parse().ok()?;
    let kind: StreamKind = components.next()?.as_os_str().to_str()?.parse().ok()?;
    let _date = components.next()?;
    let _file = components.next()?;
    if components.next().is_some() || path.extension().is_none_or(|e| e != "mp4") {
        return None;
    }
    Some(StreamKey { camera, kind })
}

/// Summarises a file: from its sidecar index when the index and the file
/// agree, which reads a few kilobytes, else by scanning the whole file.
/// `None` when it holds no valid fragment.
pub fn summarize(path: &Path) -> anyhow::Result<Option<ScanSummary>> {
    if let Some(summary) = summarize_from_index(path) {
        debug!(path = %path.display(), "summarised from the sidecar index");
        return Ok(Some(summary));
    }
    scan_summary(path)
}

/// Summarises a sealed file from its sidecar index. The index must describe
/// the file exactly: records contiguous from the end of the init segment to
/// the end of the file, and a trailer at the end of the file that matches the
/// last record. Anything else returns `None`, and the caller scans the file.
#[must_use]
pub fn summarize_from_index(path: &Path) -> Option<ScanSummary> {
    let (header, records) = read_index(&path.with_extension("idx")).ok()?;
    let (first, last) = (records.first()?, records.last()?);
    if first.offset != u64::from(header.init_len) {
        return None;
    }
    if records
        .windows(2)
        .any(|pair| pair[1].offset != pair[0].end())
    {
        return None;
    }
    let file_len = std::fs::metadata(path).ok()?.len();
    if last.end() != file_len {
        return None;
    }
    let mut file = std::fs::File::open(path).ok()?;
    file.seek(std::io::SeekFrom::Start(
        file_len.checked_sub(TRAILER_LEN as u64)?,
    ))
    .ok()?;
    let mut tail = [0u8; TRAILER_LEN];
    file.read_exact(&mut tail).ok()?;
    if decode_trailer(&tail)?.fragment_len != last.len {
        return None;
    }
    Some(ScanSummary {
        valid_len: file_len,
        fragments: u32::try_from(records.len()).ok()?,
        init_len: header.init_len,
        wall_start: first.wall,
        wall_end: last
            .wall
            .offset(MediaTicks::new(i64::from(last.duration)).as_micros()),
        media_start: first.decode_time,
        media_end: last.decode_time + u64::from(last.duration),
    })
}

/// Scans a whole file and summarises it; `None` when it holds no valid fragment.
pub fn scan_summary(path: &Path) -> anyhow::Result<Option<ScanSummary>> {
    let file = std::fs::File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let report = scan(std::io::BufReader::new(file), ScanOptions::default())
        .with_context(|| format!("scanning {}", path.display()))?;
    let (Some(first), Some(last)) = (report.fragments.first(), report.fragments.last()) else {
        return Ok(None);
    };
    let last_wall_end = last
        .wall
        .map(|w| w.offset(MediaTicks::new(i64::from(last.duration)).as_micros()));
    Ok(Some(ScanSummary {
        valid_len: report.valid_len,
        fragments: u32::try_from(report.fragments.len()).unwrap_or(u32::MAX),
        init_len: report.init_len,
        wall_start: first.wall.unwrap_or_default(),
        wall_end: last_wall_end.unwrap_or_default(),
        media_start: first.decode_time,
        media_end: last.decode_time + u64::from(last.duration),
    }))
}

/// Recovers every segment the catalog still lists as open.
pub async fn recover_writing(catalog: &CatalogHandle) -> anyhow::Result<usize> {
    let rows = catalog
        .writing_segments()
        .await
        .context("listing open segments")?;
    let mut recovered = 0;
    for row in rows {
        let path = row.path.clone();
        if !path.exists() {
            warn!(path = %path.display(), "open segment is missing; dropping its row");
            catalog
                .delete_segment(row.id)
                .await
                .context("deleting a missing segment")?;
            continue;
        }
        let report_path = path.clone();
        let report = tokio::task::spawn_blocking(move || recover_segment(&report_path))
            .await
            .context("recovery task panicked")?;
        match report {
            Ok(report) => {
                let summary_path = path.clone();
                let summary = tokio::task::spawn_blocking(move || summarize(&summary_path))
                    .await
                    .context("scan task panicked")??;
                if let Some(summary) = summary {
                    catalog
                        .mark_sealed(
                            &path,
                            summary.valid_len,
                            summary.fragments,
                            summary.wall_end,
                            summary.media_end,
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
                    warn!(path = %path.display(), "recovered segment holds no fragments; dropping it");
                    let _ = iris_store::remove_segment(&path);
                    catalog
                        .delete_segment(row.id)
                        .await
                        .context("deleting an empty segment")?;
                }
            }
            Err(err) => warn!(path = %path.display(), %err, "cannot recover segment"),
        }
    }
    Ok(recovered)
}

/// Lists every `.mp4` under a recording root.
pub fn recordings_under(root: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                if path
                    .strip_prefix(root)
                    .map_or(0, |r| r.components().count())
                    < 4
                {
                    stack.push(path);
                }
            } else if path.extension().is_some_and(|e| e == "mp4") {
                found.push(path);
            }
        }
    }
    found.sort();
    found
}

/// Adds files under the roots that the catalog does not know.
pub async fn adopt_unknown(catalog: &CatalogHandle, roots: &[PathBuf]) -> anyhow::Result<usize> {
    let mut adopted = 0;
    let mut seen = 0usize;
    for root in roots {
        let root_for_walk = root.clone();
        let files = tokio::task::spawn_blocking(move || recordings_under(&root_for_walk))
            .await
            .context("directory walk panicked")?;
        let total = files.len();
        for path in files {
            seen += 1;
            if seen.is_multiple_of(ADOPT_STATUS_EVERY) {
                systemd::status(&format!(
                    "reconciling recordings: {seen} of {total} files, {adopted} adopted"
                ));
                systemd::extend_timeout(ADOPT_TIMEOUT_EXTENSION);
            }
            if catalog
                .segment_by_path(&path)
                .await
                .context("looking up a file")?
                .is_some()
            {
                continue;
            }
            let Some(key) = parse_segment_path(root, &path) else {
                warn!(path = %path.display(), "file does not follow the recording layout; ignoring");
                continue;
            };
            let scan_path = path.clone();
            let summary = match tokio::task::spawn_blocking(move || summarize(&scan_path)).await {
                Ok(Ok(Some(summary))) => summary,
                Ok(Ok(None)) => {
                    warn!(path = %path.display(), "no valid fragments; ignoring");
                    continue;
                }
                Ok(Err(err)) => {
                    warn!(path = %path.display(), %err, "cannot scan; ignoring");
                    continue;
                }
                Err(err) => {
                    warn!(%err, "scan task panicked");
                    continue;
                }
            };
            catalog
                .adopt_segment(NewSegment {
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
                })
                .await
                .with_context(|| format!("adopting {}", path.display()))?;
            info!(path = %path.display(), stream = %key, fragments = summary.fragments, "adopted recording");
            adopted += 1;
        }
    }
    Ok(adopted)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::time::Duration;

    use bytes::Bytes;
    use iris_mp4::fragment::{Fragment, Sample};
    use iris_mp4::init::SampleEntry;
    use iris_store::{StoragePolicy, WriterHandle};

    fn nal(header: u8, filler: u8, body: usize) -> Vec<u8> {
        let len = u32::try_from(2 + body).unwrap();
        let mut v = len.to_be_bytes().to_vec();
        v.extend_from_slice(&[header, 1]);
        v.resize(4 + 2 + body, filler);
        v
    }

    /// Writes a sealed three-fragment recording with the store and returns its path.
    fn sealed_recording(root: &Path) -> PathBuf {
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
                decode_time: i * 6_000,
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

        // No index at all: the scan still works.
        std::fs::remove_file(&idx).unwrap();
        assert_eq!(summarize_from_index(&path), None);
        assert_eq!(summarize(&path).unwrap(), Some(by_scan));
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
}
