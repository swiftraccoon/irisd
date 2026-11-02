//! Repairing a file that was open when the process died.

use std::collections::HashMap;
use std::fs::OpenOptions;
use std::io;
use std::path::{Path, PathBuf};

use iris_mp4::index::{
    FLAG_DISCONTINUITY_BEFORE, FLAG_NEW_PARAMETERS, IndexHeader, IndexRecord, IndexWriter,
    read_index,
};
use iris_mp4::reader::{ScanOptions, ScanStop, scan};
use thiserror::Error;

use crate::layout;

/// What recovery did to one file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecoveryReport {
    /// The media file.
    pub path: PathBuf,
    /// Valid fragments found.
    pub fragments: usize,
    /// File length before recovery.
    pub original_len: u64,
    /// Length of the valid prefix, which is the file length afterwards.
    pub valid_len: u64,
    /// Whether the file was shortened.
    pub truncated: bool,
    /// Why the scan stopped before the original end, if it did.
    pub stop: Option<ScanStop>,
    /// Whether the sidecar index was rewritten (always, on success).
    pub index_rewritten: bool,
}

/// Errors from recovery.
#[derive(Debug, Error)]
pub enum RecoverError {
    /// An I/O failure on the named path.
    #[error("{path}: {source}")]
    Io {
        /// The file involved.
        path: PathBuf,
        /// The underlying error.
        #[source]
        source: io::Error,
    },
    /// The file does not start with `ftyp` + `moov`.
    #[error("{0} is not an irisd recording")]
    NotARecording(PathBuf),
}

/// Scans `mp4_path` with CRC verification, truncates it to its valid prefix,
/// and rewrites the sidecar index. Flags and camera timestamps from an
/// existing index are preserved for fragments that survive.
pub fn recover_segment(mp4_path: &Path) -> Result<RecoveryReport, RecoverError> {
    let io_err = |source: io::Error| RecoverError::Io {
        path: mp4_path.to_path_buf(),
        source,
    };
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(mp4_path)
        .map_err(io_err)?;
    let original_len = file.metadata().map_err(io_err)?.len();
    let report = scan(
        &file,
        ScanOptions {
            verify_crc: true,
            collect_samples: false,
        },
    )
    .map_err(io_err)?;
    if report.init_len == 0 {
        return Err(RecoverError::NotARecording(mp4_path.to_path_buf()));
    }
    let truncated = report.valid_len < original_len;
    if truncated {
        file.set_len(report.valid_len).map_err(io_err)?;
        file.sync_data().map_err(io_err)?;
    }

    let index_path = layout::index_path(mp4_path);
    let existing = read_index(&index_path).ok();
    let old_records: HashMap<u64, IndexRecord> = existing
        .as_ref()
        .map(|(_, records)| records.iter().map(|r| (r.offset, *r)).collect())
        .unwrap_or_default();
    let header = IndexHeader {
        start_wall: existing
            .as_ref()
            .map(|(h, _)| h.start_wall)
            .or_else(|| report.fragments.first().and_then(|f| f.wall))
            .unwrap_or_default(),
        run_id: existing.as_ref().map_or(0, |(h, _)| h.run_id),
        init_len: report.init_len,
        sample_entry_hash: existing.as_ref().map_or(0, |(h, _)| h.sample_entry_hash),
        ..IndexHeader::default()
    };
    let idx_err = |source: io::Error| RecoverError::Io {
        path: index_path.clone(),
        source,
    };
    let mut writer = IndexWriter::create(&index_path, &header).map_err(idx_err)?;
    for info in &report.fragments {
        let mut record = IndexRecord::from_scan(info);
        if let Some(old) = old_records.get(&record.offset) {
            record.flags |= old.flags & (FLAG_DISCONTINUITY_BEFORE | FLAG_NEW_PARAMETERS);
            record.raw_rtp = old.raw_rtp;
        }
        writer.append(&record).map_err(idx_err)?;
    }
    writer.sync().map_err(idx_err)?;
    if let Some(parent) = mp4_path.parent() {
        crate::fs::fsync_dir(parent).map_err(io_err)?;
    }

    Ok(RecoveryReport {
        path: mp4_path.to_path_buf(),
        fragments: report.fragments.len(),
        original_len,
        valid_len: report.valid_len,
        truncated,
        stop: report.stop,
        index_rewritten: true,
    })
}
