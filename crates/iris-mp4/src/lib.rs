//! Fragmented MP4 for irisd recordings: the on-disk format, its scanner, the
//! sidecar index, and byte-range HLS playlists.
//!
//! A recording file is `ftyp` + `moov` (the initialization segment) followed
//! by self-describing fragments: an optional `prft` wall-clock box, `moof`,
//! `mdat`, and a 24-byte `free` trailer that carries a magic, a CRC-32C of the
//! fragment, and the fragment length so that recovery can walk the file in
//! either direction. A sidecar index of fixed-size records caches what the
//! `moof` boxes already say. Playback and export are byte ranges over these
//! files; `docs/research/04-storage-format.md` fixes every byte.
//!
//! # Modules
//!
//! - [`init`]: [`init::SampleEntry`] and [`init::build_init_segment`], the
//!   `hvc1` sample entry and the `ftyp` + `moov` bytes.
//! - [`fragment`]: [`fragment::Sample`], [`fragment::Fragment`], and
//!   [`fragment::encode_fragment`], which produces the [`fragment::EncodedFragment`]
//!   a writer lays around the samples with one vectored write; the trailer codec
//!   and the format constants live here too.
//! - [`reader`]: [`reader::scan`], a forward walk that validates boxes, sample
//!   accounting, and trailers, plus [`reader::scan_from_offset`] for a caller-validated
//!   fragment tail; both report the valid length ([`reader::ScanReport`]) and why they
//!   stopped ([`reader::ScanStop`]).
//! - [`index`]: the sidecar `.idx` codec ([`index::IndexHeader`],
//!   [`index::IndexRecord`]) and file ([`index::IndexWriter`], [`index::read_index`]).
//! - [`hls`]: [`hls::media_playlist`] and [`hls::master_playlist`] over one file.
//! - [`boxes`]: [`boxes::BoxWriter`] and [`boxes::BoxHeader`], minimal ISO BMFF.
//! - [`crc`]: [`crc::Crc32c`], the trailer checksum.
//!
//! # Invariants
//!
//! Media time is 90 kHz with 64-bit `tfdt`; samples hold length-prefixed NAL
//! units without parameter sets; the trailer CRC-32C covers `prft` through the
//! end of `mdat`; the index is a rebuildable cache of what the fragments say.
//! Every parser accepts arbitrary bytes without panicking (fuzz targets `scan`,
//! `box_header`, `index`).
//!
//! ```
//! use bytes::Bytes;
//! use iris_core::time::WallUs;
//! use iris_mp4::fragment::{FragmentHeader, Sample, encode_fragment};
//!
//! let samples = [Sample { data: Bytes::from_static(&[0, 0, 0, 2, 0x26, 1]), duration: 3000, is_sync: true }];
//! let encoded = encode_fragment(&FragmentHeader { sequence: 1, decode_time: 0, wall: WallUs::new(0) }, &samples)?;
//! assert_eq!(encoded.total_len(), encoded.prefix.len() as u64 + 6 + 24);
//! # Ok::<(), iris_mp4::fragment::FragmentError>(())
//! ```

pub mod boxes;
pub mod crc;
pub mod export;
pub mod fragment;
pub mod hls;
pub mod index;
pub mod init;
pub mod reader;
