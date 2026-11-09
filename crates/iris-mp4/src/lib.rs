//! Fragmented MP4 for irisd recordings: the on-disk format, its scanner, the
//! sidecar index, and byte-range HLS playlists.
//!
//! A recording file is `ftyp` + `moov` (the initialization segment) followed
//! by self-describing fragments: an optional `prft` wall-clock box, `moof`,
//! `mdat`, and a 24-byte `free` trailer that carries a magic, a CRC-32C of the
//! fragment, and the fragment length so that recovery can walk the file in
//! either direction. A sidecar index of fixed-size records caches what the
//! `moof` boxes say plus the facts only the writer knows. Playback and export
//! are byte ranges over these files; `docs/research/04-storage-format.md`
//! fixes every byte.
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
//!   accounting, and trailers, which reports the valid length
//!   ([`reader::ScanReport`]) and why it stopped ([`reader::ScanStop`]) and
//!   accepts at most [`index::MAX_PERSISTED_INDEX_RECORDS`] fragments
//!   ([`reader::scan_with_limit`] takes another limit); and
//!   [`reader::parse_fragment`] for one fragment held in memory.
//! - [`index`]: the sidecar `.idx` codec ([`index::IndexHeader`],
//!   [`index::IndexRecord`], [`index::decode_index`]), the file writer
//!   ([`index::IndexWriter`]), and the bounded readers
//!   ([`index::read_index_bounded`], [`index::read_index_file_bounded`],
//!   [`index::read_index_file_bound_to_media`]).
//! - [`export`]: [`export::rebase_fragment`], [`export::trim_fragment_end`], and
//!   [`export::with_edit_list`], the rewrites that make an export a stand-alone
//!   recording.
//! - [`hls`]: [`hls::media_playlist`] and [`hls::master_playlist`] over one file.
//! - [`boxes`]: [`boxes::BoxWriter`] and [`boxes::BoxHeader`], minimal ISO BMFF.
//! - [`crc`]: [`crc::Crc32c`], the trailer checksum.
//!
//! # Invariants
//!
//! Media time is 90 kHz with 64-bit `tfdt`; samples hold length-prefixed NAL
//! units without parameter sets; the trailer CRC-32C covers `prft` through the
//! end of `mdat`; a scan rebuilds the index's box-derived fields, while
//! `raw_rtp`, `run_id`, and the discontinuity and new-parameter flags survive
//! only in a sidecar whose records match. Every parser accepts arbitrary bytes
//! without panicking (fuzz targets `scan`, `box_header`, `index`, `trailer`,
//! and `export_rebase`).
//!
//! ```
//! use bytes::Bytes;
//! use iris_core::time::WallUs;
//! use iris_mp4::fragment::{FragmentHeader, Sample, encode_fragment};
//!
//! let samples = [Sample {
//!     data: Bytes::from_static(&[0, 0, 0, 2, 0x26, 1]),
//!     duration: 3000,
//!     is_sync: true,
//! }];
//! let header = FragmentHeader {
//!     sequence: 1,
//!     decode_time: 0,
//!     wall: WallUs::new(0),
//! };
//! let encoded = encode_fragment(&header, &samples)?;
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
