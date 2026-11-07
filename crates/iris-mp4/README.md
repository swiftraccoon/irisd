# iris-mp4

The on-disk format of irisd recordings and the readers over it. A recording is an
initialization segment (`ftyp` + `moov`) followed by self-describing fragments, each
`prft` + `moof` + `mdat` + a 24-byte trailer carrying a CRC-32C and the fragment length,
with a sidecar index of fixed-size records. This crate encodes those bytes for the writer,
scans and validates them for recovery and self-checks, and builds the byte-range HLS
playlists that serve them. It sits at the centre of the record path (RTSP in → fMP4 on
disk → byte-range serving).

## What it provides

- `init::SampleEntry` and `init::build_init_segment` — the `hvc1` sample entry and the
  `ftyp` + `moov` bytes of a file.
- `fragment::Sample`, `fragment::Fragment`, `fragment::FragmentHeader`, and
  `fragment::encode_fragment` — one access unit, one assembled GOP, and the `prft`,
  `moof`, `mdat` header, and trailer around its samples (`EncodedFragment`).
- `fragment::encode_trailer`, `decode_trailer`, and `Trailer` — the 24-byte `free` box with
  magic, CRC-32C, and fragment length.
- `export::rebase_fragment`, `export::trim_fragment_end`, and `export::with_edit_list` — the rewrites that make an
  export a stand-alone recording: a fragment's sequence, decode time, and `prft` media time
  with a recomputed trailer, a tail prefix containing only samples before the cutoff,
  and an `edts`/`elst` spliced into an init segment for start preroll. Tail clipping
  preserves retained sample bytes and shortens a crossing sample's duration.
- `reader::parse_fragment` — one fragment held in memory, with its sample tables, for a
  serving path that has read it out of a file (the keyframe resource).
- `reader::scan`, `reader::scan_from_offset`, `ScanOptions`, `ScanReport`, `FragmentInfo`,
  and `ScanStop` — a forward walk of a file, or of a tail after validating the
  initialization, that reports the valid length and why it stopped.
- `index::IndexHeader`, `IndexRecord`, `IndexWriter`, `read_index`, and
  `read_index_bounded` — the sidecar `.idx` codec and file; the bounded reader rejects a file whose
  metadata can contain more than the caller's record budget before allocating, and
  `IndexWriter::from_claimed_file` initializes an exclusively claimed file.
- `hls::media_playlist`, `hls::master_playlist`, and `PlaylistOptions` — VOD playlists as
  byte ranges over one file.
- `boxes::BoxWriter` and `boxes::BoxHeader` — minimal ISO BMFF serialization and header
  parsing.
- `crc::Crc32c` and `crc::crc32c` — the trailer checksum over a SIMD implementation.

## How it is used

```rust
use bytes::Bytes;
use iris_core::time::WallUs;
use iris_mp4::fragment::{FragmentHeader, Sample, encode_fragment};
use iris_mp4::reader::{ScanOptions, scan};

let samples = [Sample { data: Bytes::from_static(&[0, 0, 0, 2, 0x26, 1]), duration: 3000, is_sync: true }];
let header = FragmentHeader { sequence: 1, decode_time: 0, wall: WallUs::new(0) };
let encoded = encode_fragment(&header, &samples)?;
// On disk: encoded.prefix, then each sample's data, then encoded.trailer.
assert_eq!(encoded.total_len(), encoded.prefix.len() as u64 + 6 + 24);
# Ok::<(), iris_mp4::fragment::FragmentError>(())
```

The writer calls `encode_fragment` and writes `EncodedFragment::io_slices` with one
`pwritev`; recovery and the daily self-check call `reader::scan` with `verify_crc`; the
API builds playlists from the sidecar records with `hls::media_playlist`.

## Invariants and limits

- Byte layout of a file, including the trailer (`free`, magic `IRX1`, CRC-32C over `prft`
  through the end of `mdat`, `fragment_len`): `docs/research/04-storage-format.md §4`.
- Sidecar index: 64-byte header (`IRIX`, version 1, 40-byte records) and record fields:
  `04 §5`. A trailing partial record is ignored on read. Complete records must describe
  contiguous ranges from `init_len`, longer than a trailer and with non-overflowing ends;
  invalid ranges reject the cache so consumers can scan the media.
- Daemon and offline persisted-cache entrypoints use `read_index_bounded` with the shared
  `MAX_PERSISTED_INDEX_RECORDS` ceiling of 131,072 records (about 5 MiB). The low-level
  `read_index` codec requires its caller to impose an equivalent bound. The bounded reader checks
  metadata, uses fallible reservation, and reads at most the header plus the allowed complete
  records and one partial-record tail. Oversized caches are rejected before allocation so callers
  can scan authoritative media instead.
- Scans expose a nonzero initialization length only after complete `ftyp` and `moov` boxes;
  recovery rejects incomplete initialization without changing media or sidecar bytes.
  `scan_from_offset` still validates that initialization but does not read the trusted prefix
  before its fragment boundary; callers must validate that boundary independently.
- File and memory fragment readers require optional `prft`, one `moof`, one adjacent `mdat`,
  and one trailer in that order. The native `default-base-is-moof` plus explicit `trun`
  data offset must address that payload, and every declared optional table field must fit.
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
fragment layout, trailer round trips, scanner behaviour on clean files, truncation,
zero fill, and corruption, index header and record round trips with a partial tail,
playlist grouping and continuation trailer exclusion, malformed-cache fallback, completed-init
recovery guards, fragment order/addressing/optional-field rejection, edit-parent restrictions,
tail scanning from a validated offset, and a CRC cross-check against the reference crate
at every alignment. The doctest in `lib.rs` runs in the suite. Fuzz targets under
`fuzz/fuzz_targets/` (`scan`, `box_header`, `index`, `trailer`) cover the parsers. Container
overhead and the checksum's cost are measured in `docs/research/04-storage-format.md §4`
and `12-ingest-cost.md §5`.

## Further reading

- `docs/research/04-storage-format.md` §3 to §5, §9, §10
- `docs/research/07-architecture.md` §2 and §8
- `docs/research/12-ingest-cost.md` §5
- `STANDARDS.md`
