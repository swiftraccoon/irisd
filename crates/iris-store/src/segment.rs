//! One open recording file and its sidecar index.

use std::fs::{self, File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use iris_core::time::{MediaTicks, WallUs};
use iris_mp4::fragment::{Fragment, FragmentHeader, encode_fragment};
use iris_mp4::index::{IndexHeader, IndexRecord, IndexWriter};

use crate::layout;
use crate::observer::{SegmentProgress, SegmentSealed};

/// An open `.mp4` with its `.idx`, written strictly sequentially.
#[derive(Debug)]
pub(crate) struct SegmentFile {
    path: PathBuf,
    file: File,
    index: IndexWriter,
    end: u64,
    allocated_end: u64,
    preallocate_bytes: u64,
    dirty: bool,
    /// When the next `fdatasync` is due; advances by the interval so the
    /// stream keeps its phase against the other streams.
    next_sync: Instant,
    opened_at: Instant,
    init_len: u32,
    fragments: u32,
    wall_end: WallUs,
    media_end: u64,
}

impl SegmentFile {
    /// Creates the file (and its directories), writes the init segment, and
    /// reserves space ahead of it. When `path` exists, a numbered sibling is
    /// used so that two runs starting in the same second never collide.
    pub(crate) fn create(
        path: &Path,
        init: &[u8],
        header: IndexHeader,
        preallocate_bytes: u64,
        sync_phase: Duration,
    ) -> io::Result<Self> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let (path, file) = create_unique(path)?;
        let index = IndexWriter::create(&layout::index_path(&path), &header)?;
        let mut segment = Self {
            path,
            file,
            index,
            end: 0,
            allocated_end: 0,
            preallocate_bytes,
            dirty: true,
            next_sync: Instant::now() + sync_phase,
            opened_at: Instant::now(),
            init_len: u32::try_from(init.len()).map_err(io::Error::other)?,
            fragments: 0,
            wall_end: header.start_wall,
            media_end: 0,
        };
        crate::fs::pwritev_all(&segment.file, &[io::IoSlice::new(init)], 0)?;
        segment.end = init.len() as u64;
        segment.allocated_end = segment.end;
        segment.ensure_preallocated()?;
        Ok(segment)
    }

    /// Appends one fragment and its index record; returns the record.
    pub(crate) fn write_fragment(
        &mut self,
        fragment: &Fragment,
        sequence: u32,
        flags: u8,
    ) -> io::Result<IndexRecord> {
        let header = FragmentHeader {
            sequence,
            decode_time: fragment.decode_time,
            wall: fragment.wall,
        };
        let encoded = encode_fragment(&header, &fragment.samples).map_err(io::Error::other)?;
        crate::fs::pwritev_all(&self.file, &encoded.io_slices(&fragment.samples), self.end)?;
        let record = IndexRecord {
            offset: self.end,
            decode_time: fragment.decode_time,
            wall: fragment.wall,
            len: u32::try_from(encoded.total_len()).map_err(io::Error::other)?,
            duration: u32::try_from(fragment.duration()).unwrap_or(u32::MAX),
            sample_count: u16::try_from(fragment.samples.len()).map_err(io::Error::other)?,
            flags,
            raw_rtp: fragment.raw_rtp,
        };
        self.index.append(&record)?;
        self.end += encoded.total_len();
        self.dirty = true;
        let duration = fragment.duration();
        self.fragments += 1;
        self.media_end = fragment.decode_time + duration;
        let duration_micros =
            MediaTicks::new(i64::try_from(duration).unwrap_or(i64::MAX)).as_micros();
        self.wall_end = fragment.wall.offset(duration_micros);
        self.ensure_preallocated()?;
        Ok(record)
    }

    /// What is durable right after a successful sync.
    pub(crate) fn progress(&self) -> SegmentProgress {
        SegmentProgress {
            path: self.path.clone(),
            durable_len: self.end,
            fragments: self.fragments,
            wall_end: self.wall_end,
            media_end: self.media_end,
        }
    }

    /// Length of the init segment at the start of the file.
    pub(crate) fn init_len(&self) -> u32 {
        self.init_len
    }

    /// `fdatasync`s data and index when the next sync is due and the file is
    /// dirty. The due time advances by `interval` whether or not a sync ran,
    /// so each stream keeps the phase it was given at creation; a sync that
    /// finishes after the next due time pushes it to now.
    pub(crate) fn maybe_sync(&mut self, now: Instant, interval: Duration) -> io::Result<bool> {
        if now < self.next_sync {
            return Ok(false);
        }
        self.next_sync = (self.next_sync + interval).max(now);
        if !self.dirty {
            return Ok(false);
        }
        self.file.sync_data()?;
        self.index.sync()?;
        self.dirty = false;
        Ok(true)
    }

    /// When the next sync is due.
    #[cfg(test)]
    pub(crate) fn next_sync(&self) -> Instant {
        self.next_sync
    }

    /// Trims preallocated space, flushes everything, and closes the file.
    pub(crate) fn seal(self) -> io::Result<SegmentSealed> {
        self.file.set_len(self.end)?;
        self.file.sync_data()?;
        self.index.sync()?;
        if let Some(parent) = self.path.parent() {
            crate::fs::fsync_dir(parent)?;
        }
        Ok(SegmentSealed {
            path: self.path,
            len: self.end,
            fragments: self.fragments,
            wall_end: self.wall_end,
            media_end: self.media_end,
        })
    }

    /// Bytes written so far, including the init segment.
    pub(crate) fn bytes_written(&self) -> u64 {
        self.end
    }

    /// When the file was created.
    pub(crate) fn opened_at(&self) -> Instant {
        self.opened_at
    }

    /// The media file path.
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    fn ensure_preallocated(&mut self) -> io::Result<()> {
        if self.preallocate_bytes == 0 {
            return Ok(());
        }
        let remaining = self.allocated_end.saturating_sub(self.end);
        if remaining < self.preallocate_bytes / 4 {
            crate::fs::preallocate(&self.file, self.allocated_end, self.preallocate_bytes)?;
            self.allocated_end += self.preallocate_bytes;
        }
        Ok(())
    }
}

fn create_unique(path: &Path) -> io::Result<(PathBuf, File)> {
    let mut candidate = path.to_path_buf();
    for n in 0..100u32 {
        if n > 0 {
            let stem = path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("segment");
            candidate = path.with_file_name(format!("{stem}-{n}.mp4"));
        }
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&candidate)
        {
            Ok(file) => return Ok((candidate, file)),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        format!("too many files named like {}", path.display()),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use iris_core::id::{CameraId, StreamKey, StreamKind};
    use iris_core::time::WallUs;

    fn header() -> IndexHeader {
        IndexHeader {
            start_wall: WallUs::new(1_700_000_000_000_000),
            ..IndexHeader::default()
        }
    }

    #[test]
    fn syncs_keep_their_phase_and_skip_clean_files() {
        let dir = tempfile::tempdir().unwrap();
        let key = StreamKey {
            camera: CameraId::try_new("cam1").unwrap(),
            kind: StreamKind::Main,
        };
        let _ = key;
        let interval = Duration::from_secs(5);
        let phase = Duration::from_millis(1_250);
        let mut segment =
            SegmentFile::create(&dir.path().join("a.mp4"), b"init", header(), 1 << 16, phase)
                .unwrap();
        let t0 = segment.next_sync().checked_sub(phase).unwrap();
        // Not due before the phase, due at it; the file is dirty from creation.
        assert!(
            !segment
                .maybe_sync(
                    (t0 + phase).checked_sub(Duration::from_millis(1)).unwrap(),
                    interval
                )
                .unwrap()
        );
        assert!(segment.maybe_sync(t0 + phase, interval).unwrap());
        assert_eq!(segment.next_sync(), t0 + phase + interval);
        // Clean at the next due time: no sync, but the schedule still advances.
        assert!(!segment.maybe_sync(t0 + phase + interval, interval).unwrap());
        assert_eq!(segment.next_sync(), t0 + phase + 2 * interval);
        // Far behind (a slow disk): the next due time is now, not in the past.
        let late = t0 + phase + 10 * interval;
        assert!(!segment.maybe_sync(late, interval).unwrap());
        assert_eq!(segment.next_sync(), late);
    }
}
