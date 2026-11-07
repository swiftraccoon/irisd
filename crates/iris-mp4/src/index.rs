//! Sidecar index: a 64-byte header and fixed 40-byte records, one per fragment.
//!
//! The index is a cache of what the fragments' own boxes say; it can be
//! rebuilt with [`crate::reader::scan`].

use std::fs::{File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::Path;

use thiserror::Error;

use crate::boxes::{be_u32, be_u64};
use crate::fragment::TRAILER_LEN;
use crate::reader::{FragmentInfo, ScanReport};
use iris_core::time::{TIMESCALE, WallUs};

/// Magic at the start of an index file.
pub const INDEX_MAGIC: [u8; 4] = *b"IRIX";
/// Format version.
pub const INDEX_VERSION: u16 = 1;
/// Header length in bytes.
pub const HEADER_LEN: usize = 64;
/// Record length in bytes.
pub const RECORD_LEN: usize = 40;
/// Maximum number of records accepted from any persisted sidecar cache.
///
/// A record takes 40 bytes both on disk and in the decoded [`IndexRecord`], so
/// this limits a read to about 5 MiB on disk and 5 MiB of record storage, while
/// still allowing more than three days of one-fragment-per-two-seconds media in
/// one rotated recording. Callers must fall back to the media file when
/// possible. The writer rotates before a file reaches this many fragments, and
/// [`crate::reader::scan`] accepts no more fragments than this.
pub const MAX_PERSISTED_INDEX_RECORDS: usize = 131_072;

/// Record flag: the fragment starts with a sync sample.
pub const FLAG_STARTS_WITH_SYNC: u8 = 0x1;
/// Record flag: a timing discontinuity precedes the fragment.
pub const FLAG_DISCONTINUITY_BEFORE: u8 = 0x2;
/// Record flag: the sample entry changed at this fragment.
pub const FLAG_NEW_PARAMETERS: u8 = 0x4;

/// The 64-byte sidecar header: magic, version, record size, timescale, the
/// wall time and run of the first fragment, the init segment length, and the
/// sample-entry hash that identifies the parameter-set epoch.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IndexHeader {
    /// Media timescale (always [`TIMESCALE`] for irisd files).
    pub timescale: u32,
    /// Reserved flags.
    pub flags: u32,
    /// Wall time of the first fragment.
    pub start_wall: WallUs,
    /// Identifier of the run this file belongs to.
    pub run_id: u64,
    /// Length of `ftyp` + `moov` in the media file.
    pub init_len: u32,
    /// Hash of the sample entry in force.
    pub sample_entry_hash: u32,
}

impl IndexHeader {
    /// Serializes the header.
    #[must_use]
    pub fn encode(&self) -> [u8; HEADER_LEN] {
        let mut b = [0u8; HEADER_LEN];
        b[0..4].copy_from_slice(&INDEX_MAGIC);
        b[4..6].copy_from_slice(&INDEX_VERSION.to_be_bytes());
        b[6..8].copy_from_slice(&(RECORD_LEN as u16).to_be_bytes());
        b[8..12].copy_from_slice(&self.timescale.to_be_bytes());
        b[12..16].copy_from_slice(&self.flags.to_be_bytes());
        b[16..24].copy_from_slice(&self.start_wall.get().to_be_bytes());
        b[24..32].copy_from_slice(&self.run_id.to_be_bytes());
        b[32..36].copy_from_slice(&self.init_len.to_be_bytes());
        b[36..40].copy_from_slice(&self.sample_entry_hash.to_be_bytes());
        b
    }

    /// Parses a header.
    pub fn decode(b: &[u8]) -> Result<Self, IndexError> {
        if b.len() < HEADER_LEN || b[0..4] != INDEX_MAGIC {
            return Err(IndexError::BadMagic);
        }
        let version = u16::from_be_bytes([b[4], b[5]]);
        let record_len = u16::from_be_bytes([b[6], b[7]]);
        if version != INDEX_VERSION || usize::from(record_len) != RECORD_LEN {
            return Err(IndexError::UnsupportedVersion(version));
        }
        Ok(Self {
            timescale: be_u32(b, 8).expect("header long enough"),
            flags: be_u32(b, 12).expect("header long enough"),
            start_wall: WallUs::new(be_u64(b, 16).expect("header long enough") as i64),
            run_id: be_u64(b, 24).expect("header long enough"),
            init_len: be_u32(b, 32).expect("header long enough"),
            sample_entry_hash: be_u32(b, 36).expect("header long enough"),
        })
    }
}

impl Default for IndexHeader {
    fn default() -> Self {
        Self {
            timescale: TIMESCALE,
            flags: 0,
            start_wall: WallUs::default(),
            run_id: 0,
            init_len: 0,
            sample_entry_hash: 0,
        }
    }
}

/// One fragment, as the 40-byte sidecar record describes it: where it is in
/// the media file, its decode and wall time, its length and duration, and the
/// `FLAG_*` bits. All integers are big-endian on disk.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct IndexRecord {
    /// Byte offset of the fragment in the media file.
    pub offset: u64,
    /// `tfdt` decode time.
    pub decode_time: u64,
    /// NVR wall time of the first sample.
    pub wall: WallUs,
    /// Fragment length through the trailer.
    pub len: u32,
    /// Total duration in ticks.
    pub duration: u32,
    /// Number of samples.
    pub sample_count: u16,
    /// `FLAG_*` bits.
    pub flags: u8,
    /// Low 32 bits of the camera RTP timestamp of the first sample.
    pub raw_rtp: u32,
}

impl IndexRecord {
    /// Serializes the record.
    #[must_use]
    pub fn encode(&self) -> [u8; RECORD_LEN] {
        let mut b = [0u8; RECORD_LEN];
        b[0..8].copy_from_slice(&self.offset.to_be_bytes());
        b[8..16].copy_from_slice(&self.decode_time.to_be_bytes());
        b[16..24].copy_from_slice(&self.wall.get().to_be_bytes());
        b[24..28].copy_from_slice(&self.len.to_be_bytes());
        b[28..32].copy_from_slice(&self.duration.to_be_bytes());
        b[32..34].copy_from_slice(&self.sample_count.to_be_bytes());
        b[34] = self.flags;
        b[36..40].copy_from_slice(&self.raw_rtp.to_be_bytes());
        b
    }

    /// Parses a record from exactly [`RECORD_LEN`] bytes.
    #[must_use]
    pub fn decode(b: &[u8; RECORD_LEN]) -> Self {
        Self {
            offset: be_u64(b, 0).expect("fixed size"),
            decode_time: be_u64(b, 8).expect("fixed size"),
            wall: WallUs::new(be_u64(b, 16).expect("fixed size") as i64),
            len: be_u32(b, 24).expect("fixed size"),
            duration: be_u32(b, 28).expect("fixed size"),
            sample_count: u16::from_be_bytes([b[32], b[33]]),
            flags: b[34],
            raw_rtp: be_u32(b, 36).expect("fixed size"),
        }
    }

    /// Builds a record from scanned fragment facts; `raw_rtp` is unknown
    /// after a scan and left zero.
    #[must_use]
    pub fn from_scan(info: &FragmentInfo) -> Self {
        Self {
            offset: info.offset,
            decode_time: info.decode_time,
            wall: info.wall.unwrap_or_default(),
            len: info.len,
            duration: info.duration,
            sample_count: info.sample_count,
            flags: if info.starts_with_sync {
                FLAG_STARTS_WITH_SYNC
            } else {
                0
            },
            raw_rtp: 0,
        }
    }

    /// Whether the fragment starts with a sync sample.
    #[must_use]
    pub fn starts_with_sync(&self) -> bool {
        self.flags & FLAG_STARTS_WITH_SYNC != 0
    }

    /// End offset of the fragment (exclusive).
    #[must_use]
    pub fn end(&self) -> u64 {
        self.offset + u64::from(self.len)
    }
}

/// Errors reading an index.
#[derive(Debug, Error)]
pub enum IndexError {
    /// Not an index file.
    #[error("not an irisd index file")]
    BadMagic,
    /// A version this build does not read.
    #[error("unsupported index version {0}")]
    UnsupportedVersion(u16),
    /// A complete record has an impossible, overflowing or noncontiguous media range.
    #[error("invalid fragment byte range in index")]
    InvalidByteRange,
    /// The index exceeds a caller-supplied serving budget.
    #[error("index is {bytes} bytes; serving limit is {max_bytes} bytes")]
    TooLarge {
        /// Observed bytes, or one past the limit when the file grew while being read.
        bytes: u64,
        /// Maximum accepted bytes.
        max_bytes: u64,
    },
    /// I/O failure.
    #[error(transparent)]
    Io(#[from] io::Error),
}

/// Appends records to an index file.
#[derive(Debug)]
pub struct IndexWriter {
    file: File,
}

impl IndexWriter {
    /// Creates (truncating) an index file and writes its header.
    pub fn create(path: &Path, header: &IndexHeader) -> io::Result<Self> {
        let file = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(path)?;
        Self::from_claimed_file(file, header)
    }

    /// Initializes an empty file that the caller has already claimed for this
    /// index and writes its header.
    ///
    /// This entry point lets a caller claim a media/index pair exclusively and
    /// retain exact ownership for rollback if initialization fails.
    pub fn from_claimed_file(mut file: File, header: &IndexHeader) -> io::Result<Self> {
        file.write_all(&header.encode())?;
        Ok(Self { file })
    }

    /// Appends one record.
    pub fn append(&mut self, record: &IndexRecord) -> io::Result<()> {
        self.file.write_all(&record.encode())
    }

    /// Flushes data to the device.
    pub fn sync(&self) -> io::Result<()> {
        self.file.sync_data()
    }
}

/// Reads an index file, ignoring a trailing partial record.
/// Complete fragment ranges must exceed the trailer length and be contiguous
/// from the initialization segment, with non-overflowing ends.
pub fn read_index(path: &Path) -> Result<(IndexHeader, Vec<IndexRecord>), IndexError> {
    let mut bytes = Vec::new();
    File::open(path)?.read_to_end(&mut bytes)?;
    decode_index(&bytes, usize::MAX)
}

/// Reads an index while accepting at most `max_records` complete records.
///
/// The byte limit includes one possible trailing partial record. File length
/// is checked before allocation, and the limited reader catches growth after
/// that check.
///
/// The path is opened with [`File::open`]: every symlink is followed, and a
/// FIFO or device at `path` can block the calling thread indefinitely. This
/// suits offline tools and tests. Daemon code opens the sidecar itself,
/// without following links and without blocking, and passes the descriptor to
/// [`read_index_file_bounded`] or [`read_index_file_bound_to_media`].
pub fn read_index_bounded(
    path: &Path,
    max_records: usize,
) -> Result<(IndexHeader, Vec<IndexRecord>), IndexError> {
    read_index_file_bounded(File::open(path)?, max_records)
}

/// Reads an already-opened sidecar under the same ceiling as
/// [`read_index_bounded`]. This lets descriptor-relative callers bind path
/// authorization and I/O without reopening a pathname.
pub fn read_index_file_bounded(
    file: File,
    max_records: usize,
) -> Result<(IndexHeader, Vec<IndexRecord>), IndexError> {
    read_index_file_bounded_with(file, max_records, |_| Ok(()))
}

fn read_index_file_bounded_with(
    file: File,
    max_records: usize,
    after_metadata: impl FnOnce(&File) -> io::Result<()>,
) -> Result<(IndexHeader, Vec<IndexRecord>), IndexError> {
    let max_bytes = HEADER_LEN
        .checked_add(
            max_records
                .checked_mul(RECORD_LEN)
                .ok_or(IndexError::TooLarge {
                    bytes: u64::MAX,
                    max_bytes: u64::MAX,
                })?,
        )
        .and_then(|bytes| bytes.checked_add(RECORD_LEN - 1))
        .ok_or(IndexError::TooLarge {
            bytes: u64::MAX,
            max_bytes: u64::MAX,
        })?;
    let max_bytes_u64 = u64::try_from(max_bytes).unwrap_or(u64::MAX);
    let file_len = file.metadata()?.len();
    if file_len > max_bytes_u64 {
        return Err(IndexError::TooLarge {
            bytes: file_len,
            max_bytes: max_bytes_u64,
        });
    }
    after_metadata(&file)?;
    let read_limit = max_bytes.checked_add(1).ok_or(IndexError::TooLarge {
        bytes: u64::MAX,
        max_bytes: max_bytes_u64,
    })?;
    let allocation = usize::try_from(file_len)
        .unwrap_or(max_bytes)
        .min(max_bytes)
        .saturating_add(1)
        .min(read_limit);
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(allocation)
        .map_err(|error| IndexError::Io(io::Error::other(error.to_string())))?;
    file.take(u64::try_from(read_limit).unwrap_or(u64::MAX))
        .read_to_end(&mut bytes)?;
    if bytes.len() > max_bytes {
        return Err(IndexError::TooLarge {
            bytes: u64::try_from(bytes.len()).unwrap_or(u64::MAX),
            max_bytes: max_bytes_u64,
        });
    }
    decode_index(&bytes, max_records)
}

/// Reads a bounded sidecar and accepts it only when every fact used by
/// adoption or playlist generation agrees with one authoritative scan of the
/// selected media file.
///
/// `media_len` must be the length of that same already-open media file. A scan
/// that stopped early, an incomplete extent, a header mismatch, or any record
/// mismatch returns `Ok(None)`, allowing callers to use the scan as fallback.
pub fn read_index_file_bound_to_media(
    file: File,
    max_records: usize,
    media: &ScanReport,
    media_len: u64,
) -> Result<Option<(IndexHeader, Vec<IndexRecord>)>, IndexError> {
    let (header, records) = read_index_file_bounded(file, max_records)?;
    let start_wall = media
        .fragments
        .first()
        .and_then(|fragment| fragment.wall)
        .unwrap_or_default();
    let bound = media.stop.is_none()
        && media.valid_len == media_len
        && header.timescale == TIMESCALE
        && header.flags == 0
        && header.init_len == media.init_len
        && header.start_wall == start_wall
        && records.len() == media.fragments.len()
        && records
            .iter()
            .zip(&media.fragments)
            .all(|(record, fragment)| index_record_matches_fragment(record, fragment));
    Ok(bound.then_some((header, records)))
}

fn index_record_matches_fragment(record: &IndexRecord, fragment: &FragmentInfo) -> bool {
    record.offset == fragment.offset
        && record.len == fragment.len
        && record.decode_time == fragment.decode_time
        && record.wall == fragment.wall.unwrap_or_default()
        && record.duration == fragment.duration
        && record.sample_count == fragment.sample_count
        && record.starts_with_sync() == fragment.starts_with_sync
}

fn decode_index(
    bytes: &[u8],
    max_records: usize,
) -> Result<(IndexHeader, Vec<IndexRecord>), IndexError> {
    let header = IndexHeader::decode(bytes)?;
    let (chunks, _partial_tail) = bytes[HEADER_LEN..].as_chunks::<RECORD_LEN>();
    if chunks.len() > max_records {
        return Err(IndexError::TooLarge {
            bytes: u64::try_from(bytes.len()).unwrap_or(u64::MAX),
            max_bytes: u64::try_from(
                HEADER_LEN.saturating_add(max_records.saturating_mul(RECORD_LEN)),
            )
            .unwrap_or(u64::MAX),
        });
    }
    let mut records = Vec::new();
    records
        .try_reserve_exact(chunks.len())
        .map_err(|error| IndexError::Io(io::Error::other(error.to_string())))?;
    let mut next_offset = u64::from(header.init_len);
    for chunk in chunks {
        let record = IndexRecord::decode(chunk);
        if record.len <= TRAILER_LEN as u32 || record.offset != next_offset {
            return Err(IndexError::InvalidByteRange);
        }
        next_offset = record
            .offset
            .checked_add(u64::from(record.len))
            .ok_or(IndexError::InvalidByteRange)?;
        records.push(record);
    }
    Ok((header, records))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_index(path: &Path, header: IndexHeader, records: &[IndexRecord], tail: &[u8]) {
        let mut bytes = header.encode().to_vec();
        for record in records {
            bytes.extend(record.encode());
        }
        bytes.extend(tail);
        std::fs::write(path, bytes).unwrap();
    }

    fn media_binding_fixture() -> (IndexHeader, Vec<IndexRecord>, ScanReport) {
        let fragments = vec![
            FragmentInfo {
                offset: 700,
                len: 100,
                payload_offset: 740,
                sequence: 1,
                decode_time: 0,
                duration: 90_000,
                sample_count: 30,
                starts_with_sync: true,
                wall: Some(WallUs::new(1_000_000)),
                sample_durations: Vec::new(),
                sample_sizes: Vec::new(),
            },
            FragmentInfo {
                offset: 800,
                len: 120,
                payload_offset: 840,
                sequence: 2,
                decode_time: 90_000,
                duration: 90_000,
                sample_count: 30,
                starts_with_sync: false,
                wall: Some(WallUs::new(2_000_000)),
                sample_durations: Vec::new(),
                sample_sizes: Vec::new(),
            },
            FragmentInfo {
                offset: 920,
                len: 140,
                payload_offset: 960,
                sequence: 3,
                decode_time: 180_000,
                duration: 45_000,
                sample_count: 15,
                starts_with_sync: true,
                wall: Some(WallUs::new(3_000_000)),
                sample_durations: Vec::new(),
                sample_sizes: Vec::new(),
            },
        ];
        let report = ScanReport {
            init_len: 700,
            fragments,
            valid_len: 1_060,
            stop: None,
        };
        let header = IndexHeader {
            start_wall: WallUs::new(1_000_000),
            init_len: 700,
            sample_entry_hash: 7,
            ..IndexHeader::default()
        };
        let records = report
            .fragments
            .iter()
            .map(IndexRecord::from_scan)
            .collect();
        (header, records, report)
    }

    #[test]
    fn header_and_record_round_trip() {
        let header = IndexHeader {
            start_wall: WallUs::new(123),
            run_id: 9,
            init_len: 700,
            sample_entry_hash: 0xdead_beef,
            ..IndexHeader::default()
        };
        assert_eq!(IndexHeader::decode(&header.encode()).unwrap(), header);
        let record = IndexRecord {
            offset: 1 << 40,
            decode_time: 90_000,
            wall: WallUs::new(-5),
            len: 2_000_000,
            duration: 180_000,
            sample_count: 60,
            flags: FLAG_STARTS_WITH_SYNC | FLAG_NEW_PARAMETERS,
            raw_rtp: 0xffff_ffff,
        };
        assert_eq!(IndexRecord::decode(&record.encode()), record);
        assert!(record.starts_with_sync());
    }

    #[test]
    fn file_loading_rejects_impossible_fragment_byte_ranges() {
        let dir = std::env::temp_dir().join(format!("iris-index-ranges-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("bad.idx");
        let header = IndexHeader {
            init_len: 700,
            ..IndexHeader::default()
        };
        let valid = IndexRecord {
            offset: 700,
            len: 100,
            ..IndexRecord::default()
        };
        for records in [
            vec![IndexRecord { len: 0, ..valid }],
            vec![IndexRecord { len: 1, ..valid }],
            vec![IndexRecord { len: 23, ..valid }],
            vec![IndexRecord { len: 24, ..valid }],
            vec![
                valid,
                IndexRecord {
                    offset: u64::MAX,
                    ..valid
                },
            ],
            vec![valid, IndexRecord { offset: 1, ..valid }],
            vec![
                valid,
                IndexRecord {
                    offset: 799,
                    ..valid
                },
            ],
        ] {
            let mut bytes = header.encode().to_vec();
            for record in records {
                bytes.extend(record.encode());
            }
            std::fs::write(&path, bytes).unwrap();
            assert!(read_index(&path).is_err());
        }
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn writes_and_reads_a_file_ignoring_partial_tail() {
        let dir = std::env::temp_dir().join(format!("iris-index-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("t.idx");
        let header = IndexHeader {
            run_id: 1,
            ..IndexHeader::default()
        };
        let mut w = IndexWriter::create(&path, &header).unwrap();
        for i in 0..3u64 {
            w.append(&IndexRecord {
                offset: i * 100,
                len: 100,
                ..IndexRecord::default()
            })
            .unwrap();
        }
        w.sync().unwrap();
        drop(w);
        let mut f = OpenOptions::new().append(true).open(&path).unwrap();
        f.write_all(&[1, 2, 3]).unwrap();
        let (h, records) = read_index(&path).unwrap();
        assert_eq!(h, header);
        assert_eq!(
            records.iter().map(|r| r.offset).collect::<Vec<_>>(),
            vec![0, 100, 200]
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn bounded_loading_rejects_record_count_and_sparse_length_before_reading() {
        let dir = std::env::temp_dir().join(format!("iris-index-bound-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("bounded.idx");
        let header = IndexHeader {
            init_len: 700,
            ..IndexHeader::default()
        };
        let mut bytes = header.encode().to_vec();
        bytes.extend(
            IndexRecord {
                offset: 700,
                len: 100,
                ..IndexRecord::default()
            }
            .encode(),
        );
        std::fs::write(&path, bytes).unwrap();
        assert!(matches!(
            read_index_bounded(&path, 0),
            Err(IndexError::TooLarge { .. })
        ));
        assert_eq!(read_index_bounded(&path, 1).unwrap().1.len(), 1);

        let sparse = dir.join("sparse.idx");
        let file = File::create(&sparse).unwrap();
        file.set_len(1 << 30).unwrap();
        assert!(matches!(
            read_index_bounded(&sparse, 1),
            Err(IndexError::TooLarge { .. })
        ));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn bounded_loading_accepts_the_exact_limit_and_partial_tail_but_rejects_one_more_byte() {
        let dir =
            std::env::temp_dir().join(format!("iris-index-exact-bound-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("bounded.idx");
        let header = IndexHeader {
            init_len: 700,
            ..IndexHeader::default()
        };
        let record = IndexRecord {
            offset: 700,
            len: 100,
            ..IndexRecord::default()
        };

        write_index(&path, header, &[record], &[]);
        assert_eq!(read_index_bounded(&path, 1).unwrap().1, [record]);

        let partial_tail = [0x5a; RECORD_LEN - 1];
        write_index(&path, header, &[record], &partial_tail);
        assert_eq!(read_index_bounded(&path, 1).unwrap().1, [record]);

        write_index(&path, header, &[record], &[0x5a; RECORD_LEN]);
        assert!(matches!(
            read_index_bounded(&path, 1),
            Err(IndexError::TooLarge { .. })
        ));

        write_index(&path, header, &[record], &[]);
        let growing = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        let grown_len = HEADER_LEN + RECORD_LEN + RECORD_LEN;
        assert!(matches!(
            read_index_file_bounded_with(growing, 1, |file| {
                file.set_len(u64::try_from(grown_len).unwrap())
            }),
            Err(IndexError::TooLarge { .. })
        ));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn media_binding_rejects_every_header_record_and_media_mismatch() {
        use crate::reader::ScanStop;

        let dir =
            std::env::temp_dir().join(format!("iris-index-media-bound-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("bound.idx");
        let (header, records, report) = media_binding_fixture();
        let accepted =
            |header: IndexHeader, records: &[IndexRecord], report: &ScanReport, media_len: u64| {
                write_index(&path, header, records, &[]);
                matches!(
                    read_index_file_bound_to_media(
                        File::open(&path).unwrap(),
                        records.len(),
                        report,
                        media_len,
                    ),
                    Ok(Some(_))
                )
            };
        assert!(accepted(header, &records, &report, report.valid_len));

        for bad_header in [
            IndexHeader {
                timescale: TIMESCALE + 1,
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
            assert!(!accepted(bad_header, &records, &report, report.valid_len));
        }

        for mutate in [
            |record: &mut IndexRecord| record.decode_time += 1,
            |record: &mut IndexRecord| record.wall = WallUs::new(record.wall.get() + 1),
            |record: &mut IndexRecord| record.duration += 1,
            |record: &mut IndexRecord| record.sample_count += 1,
            |record: &mut IndexRecord| record.flags ^= FLAG_STARTS_WITH_SYNC,
        ] {
            let mut changed = records.clone();
            mutate(&mut changed[1]);
            assert!(!accepted(header, &changed, &report, report.valid_len));
        }

        let mut past_eof = records.clone();
        past_eof[2].len += 1;
        assert!(!accepted(header, &past_eof, &report, report.valid_len));

        let mut foreign_same_length = report.clone();
        foreign_same_length.fragments[1].wall = Some(WallUs::new(9_000_000));
        assert!(!accepted(
            header,
            &records,
            &foreign_same_length,
            report.valid_len
        ));

        let mut truncated = report.clone();
        truncated.stop = Some(ScanStop::Truncated { offset: 1_060 });
        assert!(!accepted(header, &records, &truncated, report.valid_len));
        assert!(!accepted(header, &records, &report, report.valid_len + 1));
        std::fs::remove_dir_all(dir).unwrap();
    }
}
