//! Sidecar index: a 64-byte header and fixed 40-byte records, one per fragment.
//!
//! The index is a cache of what the fragments' own boxes say; it can be
//! rebuilt with [`crate::reader::scan`].

use std::fs::{File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::Path;

use thiserror::Error;

use crate::boxes::{be_u32, be_u64};
use crate::reader::FragmentInfo;
use iris_core::time::{TIMESCALE, WallUs};

/// Magic at the start of an index file.
pub const INDEX_MAGIC: [u8; 4] = *b"IRIX";
/// Format version.
pub const INDEX_VERSION: u16 = 1;
/// Header length in bytes.
pub const HEADER_LEN: usize = 64;
/// Record length in bytes.
pub const RECORD_LEN: usize = 40;

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
        let mut file = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(path)?;
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
pub fn read_index(path: &Path) -> Result<(IndexHeader, Vec<IndexRecord>), IndexError> {
    let mut bytes = Vec::new();
    File::open(path)?.read_to_end(&mut bytes)?;
    let header = IndexHeader::decode(&bytes)?;
    let (chunks, _partial_tail) = bytes[HEADER_LEN..].as_chunks::<RECORD_LEN>();
    let records = chunks.iter().map(IndexRecord::decode).collect();
    Ok((header, records))
}

#[cfg(test)]
mod tests {
    use super::*;

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
                offset: i,
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
            vec![0, 1, 2]
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
