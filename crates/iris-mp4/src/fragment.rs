//! One fragment: `prft` + `moof` + `mdat` + trailer.

use std::io::IoSlice;
use std::sync::Arc;

use bytes::Bytes;
use thiserror::Error;

use crate::boxes::BoxWriter;
use crate::crc::Crc32c;
use crate::init::{SampleEntry, VIDEO_TRACK_ID};
use iris_core::time::WallUs;

/// `sample_flags` for a sync sample: `sample_depends_on = 2` (no dependency).
pub const SYNC_SAMPLE_FLAGS: u32 = 0x0200_0000;
/// `sample_flags` for a non-sync sample: `sample_depends_on = 1`, `sample_is_non_sync_sample = 1`.
pub const NON_SYNC_SAMPLE_FLAGS: u32 = 0x0101_0000;
/// `prft` flags. Matches what ffmpeg writes for `-write_prft wallclock`.
pub const PRFT_FLAGS: u32 = 0x18;
/// Length of the trailer box.
pub const TRAILER_LEN: usize = 24;
/// Magic inside the `free` trailer.
pub const TRAILER_MAGIC: [u8; 4] = *b"IRX1";
/// Largest `moof` irisd writes or accepts, in both readers (a fragment of
/// 65 535 samples).
pub const MAX_MOOF_LEN: usize = 1 << 20;
/// Largest `mdat` payload irisd writes or accepts, in both readers.
pub const MAX_PAYLOAD_LEN: u64 = 1 << 30;
/// Smallest `prft` box the readers accept: version 0 with a 32-bit media time.
pub(crate) const MIN_PRFT_LEN: u64 = 24;
/// Largest `prft` box the readers accept.
pub(crate) const MAX_PRFT_LEN: u64 = 64;

pub(crate) const TFHD_BASE_DATA_OFFSET_PRESENT: u32 = 0x1;
pub(crate) const TFHD_SAMPLE_DESCRIPTION_INDEX_PRESENT: u32 = 0x2;
pub(crate) const TFHD_DEFAULT_SAMPLE_DURATION_PRESENT: u32 = 0x8;
pub(crate) const TFHD_DEFAULT_SAMPLE_SIZE_PRESENT: u32 = 0x10;
pub(crate) const TFHD_DEFAULT_SAMPLE_FLAGS_PRESENT: u32 = 0x20;
pub(crate) const TFHD_DEFAULT_BASE_IS_MOOF: u32 = 0x02_0000;
pub(crate) const TRUN_DATA_OFFSET_PRESENT: u32 = 0x1;
pub(crate) const TRUN_FIRST_SAMPLE_FLAGS_PRESENT: u32 = 0x4;
pub(crate) const TRUN_SAMPLE_DURATION_PRESENT: u32 = 0x100;
pub(crate) const TRUN_SAMPLE_SIZE_PRESENT: u32 = 0x200;
pub(crate) const TRUN_SAMPLE_FLAGS_PRESENT: u32 = 0x400;
pub(crate) const TRUN_COMPOSITION_OFFSETS_PRESENT: u32 = 0x800;
/// The `sample_is_non_sync_sample` bit of a `sample_flags` field.
pub(crate) const SAMPLE_IS_NON_SYNC: u32 = 0x0001_0000;

/// One access unit ready to be written: length-prefixed NAL units without
/// parameter sets, its duration in media ticks, and whether it is a sync
/// sample.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Sample {
    /// Length-prefixed NAL units.
    pub data: Bytes,
    /// Duration in 90 kHz ticks.
    pub duration: u32,
    /// Whether the sample is a random access point.
    pub is_sync: bool,
}

/// Per-fragment facts the encoder needs: the `mfhd` sequence number, the
/// `tfdt` decode time, and the wall time written to `prft`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FragmentHeader {
    /// `mfhd` sequence number, monotonic within a file.
    pub sequence: u32,
    /// Decode time of the first sample in 90 kHz ticks (`tfdt`).
    pub decode_time: u64,
    /// NVR wall time of the first sample (`prft`).
    pub wall: WallUs,
}

/// A fragment assembled by ingest and handed to the store: one GOP (or a
/// capped slice of one) with its decode time, wall time, the camera's raw RTP
/// timestamp, and the sample entry in force. The store turns it into bytes with
/// [`encode_fragment`].
#[derive(Clone, Debug)]
pub struct Fragment {
    /// The samples in decode order; the first is a sync sample unless the
    /// fragment was split out of a long GOP.
    pub samples: Vec<Sample>,
    /// Decode time of the first sample in 90 kHz ticks, continuous within a run.
    pub decode_time: u64,
    /// NVR wall time of the first sample.
    pub wall: WallUs,
    /// Low 32 bits of the camera's RTP timestamp for the first sample.
    pub raw_rtp: u32,
    /// Whether a timing discontinuity precedes this fragment (a new run).
    pub new_run: bool,
    /// The codec configuration in force for these samples, shared by every
    /// fragment of a parameter-set epoch; the store compares entries with
    /// `Arc::ptr_eq` before comparing bytes.
    pub sample_entry: Arc<SampleEntry>,
}

impl Fragment {
    /// Total payload bytes.
    #[must_use]
    pub fn payload_len(&self) -> u64 {
        self.samples.iter().map(|s| s.data.len() as u64).sum()
    }

    /// Total duration in ticks.
    #[must_use]
    pub fn duration(&self) -> u64 {
        self.samples.iter().map(|s| u64::from(s.duration)).sum()
    }

    /// Whether the first sample is a sync sample.
    #[must_use]
    pub fn starts_with_sync(&self) -> bool {
        self.samples.first().is_some_and(|s| s.is_sync)
    }
}

/// Errors from encoding a fragment.
#[derive(Clone, Debug, PartialEq, Eq, Error)]
#[non_exhaustive]
pub enum FragmentError {
    /// A fragment needs at least one sample.
    #[error("fragment has no samples")]
    Empty,
    /// More samples than the index record can count.
    #[error("fragment has {0} samples; at most 65535 allowed")]
    TooManySamples(usize),
    /// Payload larger than [`MAX_PAYLOAD_LEN`].
    #[error("fragment payload of {0} bytes exceeds the maximum")]
    PayloadTooLarge(u64),
}

/// The serialized parts of a fragment that surround the sample payloads:
/// `prft` + `moof` + the `mdat` header before them and the trailer after.
/// [`EncodedFragment::io_slices`] yields the exact on-disk order for one
/// vectored write. The trailer's length and CRC cover the samples the
/// fragment was encoded from, so only those samples may be written with it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EncodedFragment {
    /// `prft` + `moof` + `mdat` header.
    pub prefix: Bytes,
    /// The 24-byte `free` trailer.
    pub trailer: [u8; TRAILER_LEN],
    /// Sum of sample payload lengths.
    pub payload_len: u64,
}

impl EncodedFragment {
    /// Total on-disk length of the fragment.
    #[must_use]
    pub fn total_len(&self) -> u64 {
        self.prefix.len() as u64 + self.payload_len + TRAILER_LEN as u64
    }

    /// Returns the vectored-write view: prefix, each sample, trailer. `samples`
    /// are the samples passed to [`encode_fragment`], in the same order.
    #[must_use]
    pub fn io_slices<'a>(&'a self, samples: &'a [Sample]) -> Vec<IoSlice<'a>> {
        let mut slices = Vec::with_capacity(samples.len() + 2);
        slices.push(IoSlice::new(&self.prefix));
        slices.extend(samples.iter().map(|s| IoSlice::new(&s.data)));
        slices.push(IoSlice::new(&self.trailer));
        slices
    }
}

/// Encodes the boxes around `samples`: `prft`, `moof`, and the `mdat` header
/// as the prefix, and the trailer over the prefix and the sample bytes. Only
/// the first sample's sync flag is encoded (as `first_sample_flags`); every
/// later sample carries the `tfhd` non-sync default, so a fragment holds at
/// most one random access point, at its start.
///
/// # Errors
///
/// [`FragmentError::Empty`] without samples, [`FragmentError::TooManySamples`]
/// above 65,535 samples, and [`FragmentError::PayloadTooLarge`] above
/// [`MAX_PAYLOAD_LEN`] bytes of payload.
#[expect(
    clippy::missing_panics_doc,
    reason = "the conversions follow the sample-count and payload-size checks"
)]
pub fn encode_fragment(
    header: &FragmentHeader,
    samples: &[Sample],
) -> Result<EncodedFragment, FragmentError> {
    if samples.is_empty() {
        return Err(FragmentError::Empty);
    }
    if samples.len() > usize::from(u16::MAX) {
        return Err(FragmentError::TooManySamples(samples.len()));
    }
    let payload_len: u64 = samples.iter().map(|s| s.data.len() as u64).sum();
    if payload_len > MAX_PAYLOAD_LEN {
        return Err(FragmentError::PayloadTooLarge(payload_len));
    }

    let mut w = BoxWriter::with_capacity(32 + 128 + samples.len() * 8 + 8);

    let prft = w.begin_full(b"prft", 1, PRFT_FLAGS);
    w.u32(VIDEO_TRACK_ID);
    w.u64(header.wall.to_ntp64());
    w.u64(header.decode_time);
    w.end(prft);

    let moof_start = w.len();
    let moof = w.begin(b"moof");
    let mfhd = w.begin_full(b"mfhd", 0, 0);
    w.u32(header.sequence);
    w.end(mfhd);

    let traf = w.begin(b"traf");
    let tfhd = w.begin_full(
        b"tfhd",
        0,
        TFHD_DEFAULT_BASE_IS_MOOF | TFHD_DEFAULT_SAMPLE_FLAGS_PRESENT,
    );
    w.u32(VIDEO_TRACK_ID);
    w.u32(NON_SYNC_SAMPLE_FLAGS);
    w.end(tfhd);

    let tfdt = w.begin_full(b"tfdt", 1, 0);
    w.u64(header.decode_time);
    w.end(tfdt);

    let first_is_sync = samples[0].is_sync;
    let mut trun_flags =
        TRUN_DATA_OFFSET_PRESENT | TRUN_SAMPLE_DURATION_PRESENT | TRUN_SAMPLE_SIZE_PRESENT;
    if first_is_sync {
        trun_flags |= TRUN_FIRST_SAMPLE_FLAGS_PRESENT;
    }
    let trun = w.begin_full(b"trun", 0, trun_flags);
    w.u32(samples.len() as u32);
    let data_offset_pos = w.len();
    w.i32(0); // patched below
    if first_is_sync {
        w.u32(SYNC_SAMPLE_FLAGS);
    }
    for s in samples {
        w.u32(s.duration);
        w.u32(s.data.len() as u32);
    }
    w.end(trun);
    w.end(traf);
    w.end(moof);

    // data_offset is relative to the start of moof and points at the first
    // sample byte: moof length + mdat header.
    let moof_len = w.len() - moof_start;
    let data_offset = i32::try_from(moof_len + 8).expect("moof of at most 65 535 samples");
    let mut prefix = w.into_vec();
    prefix[data_offset_pos..data_offset_pos + 4].copy_from_slice(&data_offset.to_be_bytes());

    let mdat_size = u32::try_from(payload_len + 8).expect("payload at most MAX_PAYLOAD_LEN");
    prefix.extend_from_slice(&mdat_size.to_be_bytes());
    prefix.extend_from_slice(b"mdat");

    let mut crc = Crc32c::new();
    crc.update(&prefix);
    for s in samples {
        crc.update(&s.data);
    }
    let crc = crc.finish();
    let fragment_len = u32::try_from(prefix.len() as u64 + payload_len + TRAILER_LEN as u64)
        .expect("payload at most MAX_PAYLOAD_LEN and moof at most MAX_MOOF_LEN");
    let trailer = encode_trailer(crc, fragment_len);

    Ok(EncodedFragment {
        prefix: Bytes::from(prefix),
        trailer,
        payload_len,
    })
}

/// Serializes the trailer: `free` box with magic, CRC-32C, fragment length, flags.
#[must_use]
pub fn encode_trailer(crc: u32, fragment_len: u32) -> [u8; TRAILER_LEN] {
    let mut t = [0u8; TRAILER_LEN];
    t[0..4].copy_from_slice(&(TRAILER_LEN as u32).to_be_bytes());
    t[4..8].copy_from_slice(b"free");
    t[8..12].copy_from_slice(&TRAILER_MAGIC);
    t[12..16].copy_from_slice(&crc.to_be_bytes());
    t[16..20].copy_from_slice(&fragment_len.to_be_bytes());
    t[20..24].copy_from_slice(&0u32.to_be_bytes());
    t
}

/// The fields of a decoded trailer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Trailer {
    /// CRC-32C over `prft` through the end of `mdat`.
    pub crc: u32,
    /// Length from the start of `prft` to the end of the trailer.
    pub fragment_len: u32,
    /// Reserved flags.
    pub flags: u32,
}

/// Decodes a trailer; `None` if the box or magic does not match.
#[must_use]
pub fn decode_trailer(buf: &[u8]) -> Option<Trailer> {
    if buf.len() < TRAILER_LEN
        || buf[0..4] != (TRAILER_LEN as u32).to_be_bytes()
        || &buf[4..8] != b"free"
        || buf[8..12] != TRAILER_MAGIC
    {
        return None;
    }
    let field = |i: usize| u32::from_be_bytes([buf[i], buf[i + 1], buf[i + 2], buf[i + 3]]);
    Some(Trailer {
        crc: field(12),
        fragment_len: field(16),
        flags: field(20),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::boxes::BoxHeader;

    fn samples() -> Vec<Sample> {
        vec![
            Sample {
                data: Bytes::from_static(b"\0\0\0\x03\x26\x01\xaa"),
                duration: 3000,
                is_sync: true,
            },
            Sample {
                data: Bytes::from_static(b"\0\0\0\x03\x02\x01\xbb"),
                duration: 3000,
                is_sync: false,
            },
        ]
    }

    #[test]
    fn layout_is_prft_moof_mdat_trailer() {
        let enc = encode_fragment(
            &FragmentHeader {
                sequence: 7,
                decode_time: 90_000,
                wall: WallUs::new(1_000_000),
            },
            &samples(),
        )
        .unwrap();
        let p = &enc.prefix;
        let prft = BoxHeader::parse(p).unwrap();
        assert_eq!((prft.kind, prft.size), (*b"prft", Some(32)));
        let moof = BoxHeader::parse(&p[32..]).unwrap();
        assert_eq!(moof.kind, *b"moof");
        let mdat = BoxHeader::parse(&p[32 + moof.size.unwrap() as usize..]).unwrap();
        assert_eq!((mdat.kind, mdat.size), (*b"mdat", Some(8 + 14)));
        let trailer = decode_trailer(&enc.trailer).unwrap();
        assert_eq!(u64::from(trailer.fragment_len), enc.total_len());
        let mut crc = Crc32c::new();
        crc.update(p);
        for s in samples() {
            crc.update(&s.data);
        }
        assert_eq!(trailer.crc, crc.finish());
    }

    fn header() -> FragmentHeader {
        FragmentHeader {
            sequence: 1,
            decode_time: 0,
            wall: WallUs::new(0),
        }
    }

    #[test]
    fn rejects_empty() {
        assert_eq!(
            encode_fragment(&header(), &[]).unwrap_err(),
            FragmentError::Empty
        );
    }

    #[test]
    fn rejects_more_than_65535_samples() {
        let samples: Vec<Sample> = (0..=usize::from(u16::MAX))
            .map(|_| Sample {
                data: Bytes::new(),
                duration: 1,
                is_sync: false,
            })
            .collect();
        assert_eq!(
            encode_fragment(&header(), &samples).unwrap_err(),
            FragmentError::TooManySamples(65_536)
        );
    }

    #[test]
    fn rejects_payload_over_the_maximum() {
        static ZEROS: [u8; 1 << 20] = [0; 1 << 20];
        let samples: Vec<Sample> = (0..1_025)
            .map(|_| Sample {
                data: Bytes::from_static(&ZEROS),
                duration: 1,
                is_sync: false,
            })
            .collect();
        assert_eq!(
            encode_fragment(&header(), &samples).unwrap_err(),
            FragmentError::PayloadTooLarge(1_025 << 20)
        );
    }

    #[test]
    fn decode_trailer_rejects_wrong_size_type_and_magic() {
        let good = encode_trailer(1, 2);
        assert_eq!(
            decode_trailer(&good),
            Some(Trailer {
                crc: 1,
                fragment_len: 2,
                flags: 0
            })
        );
        assert!(decode_trailer(&good[..TRAILER_LEN - 1]).is_none());
        let mut wrong_size = good;
        wrong_size[3] = 25;
        assert!(decode_trailer(&wrong_size).is_none());
        let mut wrong_type = good;
        wrong_type[4..8].copy_from_slice(b"skip");
        assert!(decode_trailer(&wrong_type).is_none());
        let mut wrong_magic = good;
        wrong_magic[8] = b'J';
        assert!(decode_trailer(&wrong_magic).is_none());
    }
}
