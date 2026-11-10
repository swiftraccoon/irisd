# iris-mp4

The on-disk format of irisd recordings and the readers over it. A recording is an
initialization segment (`ftyp` + `moov`) followed by self-describing fragments, each
`prft` + `moof` + `mdat` + a 24-byte trailer carrying a CRC-32C and the fragment length,
with a sidecar index of fixed-size records. This crate encodes those bytes for the writer,
scans and validates them for recovery and self-checks, rewrites them for export, and builds
byte-range HLS playlists for offline serving. It sits at the centre of the record path (RTSP
in → fMP4 on disk → byte-range serving).

## What it provides

- `init::SampleEntry` and `init::build_init_segment` — the `hvc1` sample entry and the
  `ftyp` + `moov` bytes of a file.
- `fragment::Sample`, `fragment::Fragment`, `fragment::FragmentHeader`, and
  `fragment::encode_fragment` — one access unit, one assembled GOP, and the `prft`,
  `moof`, `mdat` header, and trailer around its samples (`EncodedFragment`).
- `fragment::encode_trailer`, `decode_trailer`, and `Trailer` — the 24-byte `free` box with
  magic, CRC-32C, fragment length, and reserved bytes that must be zero.
- `export::rebase_fragment`, `export::trim_fragment_end`, and `export::with_edit_list` — the
  rewrites that make an export a stand-alone recording: a fragment's sequence, decode time, and
  `prft` media time with a recomputed trailer, a tail prefix containing only samples before the
  cutoff, and an `edts`/`elst` spliced into an init segment for start preroll. Tail clipping
  preserves retained sample bytes and shortens a crossing sample's duration.
- `reader::parse_fragment` — one fragment held in memory, with its sample tables, for a
  serving path that has read it out of a file (the keyframe resource).
- `reader::scan`, `reader::scan_with_limit`, `ScanOptions`, `ScanReport`, `FragmentInfo`, and
  `ScanStop` — a forward walk of a file that reports the valid length and why it stopped.
- `index::IndexHeader`, `IndexRecord`, `IndexWriter`, `decode_index`, `read_index_bounded`,
  `read_index_file_bounded`, and `read_index_file_bound_to_media` — the sidecar `.idx` codec,
  the writer, and the bounded readers, which reject a file whose metadata can hold more than the
  caller's record budget before allocating; `IndexWriter::from_claimed_file` initializes an
  exclusively claimed file.
- `hls::media_playlist`, `hls::master_playlist`, `hls::variant_bit_rates`, `PlaylistOptions`,
  `VariantBitRates`, and `InvalidPlaylistText` —
  VOD playlists as byte ranges over one file; a URI or codecs string that would break the
  playlist syntax is refused.
- `boxes::BoxWriter`, `boxes::BoxHeader`, `be_u32`, and `be_u64` — minimal ISO BMFF
  serialization, header parsing, and bounds-checked field reads.
- `crc::Crc32c` and `crc::crc32c` — the trailer checksum over a SIMD implementation.

## How it is used

```rust
use bytes::Bytes;
use iris_core::time::WallUs;
use iris_mp4::fragment::{FragmentError, FragmentHeader, Sample, encode_fragment};
use iris_mp4::reader::parse_fragment;

fn main() -> Result<(), FragmentError> {
    let samples = [Sample {
        data: Bytes::from_static(&[0, 0, 0, 2, 0x26, 1]),
        duration: 3000,
        is_sync: true,
    }];
    let header = FragmentHeader {
        sequence: 1,
        decode_time: 0,
        wall: WallUs::new(0),
    };
    let encoded = encode_fragment(&header, &samples)?;
    // On disk: encoded.prefix, then each sample's data, then encoded.trailer.
    let mut bytes = encoded.prefix.to_vec();
    bytes.extend_from_slice(&samples[0].data);
    bytes.extend_from_slice(&encoded.trailer);
    assert_eq!(encoded.total_len() as usize, bytes.len());
    assert!(parse_fragment(&bytes).is_some_and(|info| info.starts_with_sync));
    Ok(())
}
```

The writer calls `encode_fragment` and writes `EncodedFragment::io_slices` with one
`pwritev`. Recovery (`reader::scan_with_limit`) and the daily self-check (`reader::scan`)
verify every CRC. The offline `irisd hls` command builds playlists with `hls::media_playlist`;
`iris-api` renders its own VOD playlists from sidecar records, and its export rewrites
fragments with `export::rebase_fragment` and `export::trim_fragment_end`.

## Invariants and limits

- Byte layout of a file, including the trailer (`free`, magic `IRX1`, CRC-32C over `prft`
  through the end of `mdat`, `fragment_len`): `docs/research/04-storage-format.md §4`.
- Sidecar index: 64-byte header (`IRIX`, version 1, 40-byte records) and record fields:
  `04 §5`. The index caches what the fragment boxes say plus facts only the writer knows
  (`raw_rtp`, `run_id`, and the discontinuity and new-parameter flags); a scan rebuilds the
  box-derived fields, leaves `raw_rtp` zero, and keeps the writer-only facts only from a prior
  sidecar whose records match. A trailing partial record is ignored on read. Complete records
  must describe contiguous ranges from `init_len`, longer than a trailer and with
  non-overflowing ends; invalid ranges reject the cache so consumers can scan the media.
- Persisted-cache readers apply the shared `MAX_PERSISTED_INDEX_RECORDS` ceiling of 131,072
  records (about 5 MiB on disk and 5 MiB decoded at 40 bytes per record). Daemon code opens the
  sidecar itself, without following links or blocking, and reads the descriptor with
  `read_index_file_bounded` or `read_index_file_bound_to_media`; the path-taking
  `read_index_bounded` follows symlinks and can block on a FIFO or device, so it serves offline
  tools and tests. `decode_index` is the pure codec under all three, for a caller that holds a
  bounded image. The bounded readers check metadata, use fallible reservation, and read at most
  the header plus the allowed complete records and one partial-record tail. Oversized caches are
  rejected before allocation so callers can scan authoritative media instead.
- Scan budget: `scan` accepts at most `MAX_PERSISTED_INDEX_RECORDS` fragments (about 13 MiB of
  `FragmentInfo` without sample tables; `collect_samples` adds 8 bytes per sample),
  `scan_with_limit` any other count. A complete fragment beyond the limit stops the scan with
  `ScanStop::FragmentLimit`; its `valid_len` is then not a truncation point, so recovery refuses
  such a file and adoption skips it. A damaged region right after the limit reports its own stop
  reason. A file that ends right after its initialization segment is a clean scan with no
  fragments.
- Scans expose a nonzero initialization length only after complete `ftyp` and `moov` boxes;
  recovery rejects incomplete initialization without changing media or sidecar bytes. An
  initialization segment over 4 GiB stops the scan at its `moov`.
- File and memory fragment readers require optional `prft` (24 to 64 bytes), one `moof` of at
  most `MAX_MOOF_LEN`, one adjacent `mdat` of at most `MAX_PAYLOAD_LEN`, and one trailer in that
  order. The native `default-base-is-moof` plus explicit `trun` data offset must address that
  payload, every declared table field must fit, the `trun` must hold a sample, the first sample
  must carry flags (from the `trun` or the `tfhd` default; the `trex` default is not consulted),
  and a `moof` or `traf` with undecodable trailing bytes is rejected. A fragment the memory
  reader accepts also scans.
- Media timescale 90 kHz; `tfdt` is 64-bit and continuous within a run: `04 §7`.
- Samples hold length-prefixed NAL units without parameter sets; the sample entry is `hvc1`
  with parameter sets in `hvcC`: `04 §4`.
- Bounds: a `moof` is at most `MAX_MOOF_LEN` (1 MiB), a payload at most `MAX_PAYLOAD_LEN`
  (1 GiB), a fragment at most 65 535 samples.
- The CRC is CRC-32C (iSCSI polynomial); the implementation may change, the value may not.
- Export rebasing verifies the original trailer checksum before changing bytes; payload
  corruption is rejected rather than given a fresh checksum.
- Edit-list insertion rejects extended-size `moov` or `trak` parents.
- Trailer-free HLS uses one range per fragment, including continuations. Media playlists
  declare independence only when all emitted segments start at sync; the master leaves
  that declaration to the media playlist.

## Tests and measurements

Unit tests per module: box nesting and size patching, large and open-ended headers,
bounds-checked field reads, fragment layout, encoder rejections (empty, too many samples, payload
over the maximum), trailer round trips and rejections, scanner behaviour on clean files,
initialization-only files, truncation, zero fill, corruption, the fragment limit, and an
initialization segment over 4 GiB, index header rejections and record round trips with a partial
tail, playlist grouping, continuation trailer exclusion, and refused playlist text,
malformed-cache fallback, completed-init recovery guards, fragment order, addressing,
optional-field, missing-flag, empty-`trun`, container-padding, and bound rejections, edit-parent
and media-time restrictions, and a CRC cross-check against the reference crate at every start
offset and after every chunk. `tests/properties.rs` holds seeded property tests over generated
recordings: the scanner, the memory parser, and the sidecar record read back what the encoder
wrote, a file cut inside a fragment is valid up to it, and a flipped bit anywhere in a fragment
ends the valid prefix before it. The doctest in `lib.rs` runs in the suite. Fuzz targets under
`fuzz/fuzz_targets/` (`scan`, `box_header`, `index`, `trailer`, and `export_rebase` for the export
rewrites) cover the parsers. Container overhead and the checksum's cost are measured in
`docs/research/04-storage-format.md §4` and `12-ingest-cost.md §5`.

## Further reading

- `docs/research/04-storage-format.md` §3 to §5, §9, §10
- `docs/research/07-architecture.md` §2 and §8
- `docs/research/12-ingest-cost.md` §5
- `STANDARDS.md`
