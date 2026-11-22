# AGENTS.md — iris-mp4

Scope: this crate only. Read the root `AGENTS.md` first.

## Invariants

- The byte layout in `docs/research/04-storage-format.md` §4 (boxes, versions, flags,
  trailer) and §5 (sidecar header and record) is the format; a change there is a format
  version change and updates the design document first. Files written before the change
  must still scan.
- The trailer CRC is CRC-32C over `prft` through the end of `mdat`; `crc.rs` may swap
  implementations only with the cross-check test green.
- Export rebasing verifies the input checksum before modifying any bytes.
- Edit insertion rejects extended-size `moov`/`trak` parents instead of patching their sentinel.
- A fragment whose samples are all sync samples says so in its `tfhd` default (an audio
  fragment, a lone keyframe); any other marks its first sample alone. `FragmentInfo::all_sync`
  reports it, and only such a fragment may lose its start (`trim_fragment_start`), at a sample
  boundary. An end trim keeps the flags it found.
- A recording has one track, id 1. Track 2 exists only in an export's init segment
  (`build_av_init_segment`) and in fragments moved to it (`retrack_fragment`); the edit of an
  audio track has no media time, because a player in wide use reads it as a count of samples
  (`docs/research/13-audio.md` §9).
- Export tail trimming verifies the original checksum, preserves retained sample bytes,
  omits samples starting at or after the cutoff, and shortens a crossing sample's duration.
  Keep sync preroll at the start; end clipping must not depend on edit-list support.
- `reader::scan` stops at the first inconsistency and reports the valid length; it never
  panics on any input (fuzz targets `scan`, `box_header`, `index`, `trailer`; `export_rebase`
  covers `parse_fragment` and the export rewrites).
- `ScanReport::init_len` is zero until both required initialization boxes are complete;
  recovery relies on this guard before modifying files. An initialization-only file is a clean
  scan with no fragments, not a truncation.
- `parse_fragment` applies the scanner's bounds and structure rules, so a fragment the keyframe
  and export paths accept is one the scanner accepts; keep the two readers in step.
- `IndexWriter::from_claimed_file` writes only to a file whose exclusive ownership remains
  with the caller, so store initialization can roll back the exact pair without clobbering it.
- Timescale 90 kHz, 64-bit `tfdt`, 32-bit per-sample durations (`04 §4`, §7). Codec structs
  that mirror a box or record (`Sample`, `FragmentHeader`, `Fragment`, `FragmentInfo`,
  `IndexHeader`, `IndexRecord`) carry the field's integer type with its unit documented
  (`STANDARDS.md §5`).
- Persisted-sidecar readers use the shared 131,072-record ceiling. Daemon code passes its own
  nonblocking, no-follow descriptor to `read_index_file_bounded` or
  `read_index_file_bound_to_media`; the path-taking `read_index_bounded` follows symlinks and can
  block on special files, so it is for offline tools and tests. `decode_index` is the pure codec
  under all three and takes the same record budget. The bounded reader checks metadata before
  allocation, rejects growth beyond the ceiling, and retains the format's
  trailing-partial-record ignore rule.
- The sidecar's `raw_rtp`, `run_id`, and discontinuity, new-parameter, and step flags exist
  nowhere in the media; a rebuild from a scan loses them unless a prior sidecar's records
  match. A record flag that only the writer knows belongs in `index::WRITER_ONLY_FLAGS`, which
  is what a rebuild keeps.
- `scan` accepts at most the same 131,072 fragments; `ScanStop::FragmentLimit` reports a complete
  fragment beyond the limit and must never be treated as a truncation point.
- `decoder_config` faces the network (root `AGENTS.md` rule 8): `DecoderConfig::parse`,
  `parse_parameter_sets`, and `parse_hvcc` never panic (fuzz target `decoder_config`). An
  accepted `hvcC` has four-byte NAL length prefixes, since that is what the samples carry, and
  an accepted AudioSpecificConfig is 2 to 64 bytes with a nonzero sampling rate.
  `HevcConfig::parameter_sets()` are NAL units without length prefixes, in the record's order:
  at most 16 VPSs, 16 SPSs, and 64 PPSs, one for each identifier that ITU-T H.265 defines for
  the type (`PARAMETER_SET_BOUNDS`). The set past a type's bound is refused before it is
  copied. So a record of any length allocates at most 96 sets of at most 65,535 bytes. Keep
  every copy behind a declared maximum (`STANDARDS.md` §6). The `decoder_config` readers and
  `init::sample_entry_bytes` walk boxes with the one walker `boxes::child`, which refuses a box
  of size 0 at any depth, so they refuse the same boxes on the path to the sample entry.

## Where things are

- `boxes.rs` — `BoxWriter` (nested boxes with patched sizes), `BoxHeader`, `be_u32`,
  `be_u64`, and `child`, the walker that `init::sample_entry_bytes` and the `decoder_config`
  readers share.
- `init.rs` — `SampleEntry`, `build_init_segment`, `VIDEO_TRACK_ID`.
- `decoder_config.rs` — `DecoderConfig` with its reader `DecoderConfig::parse`, `HevcConfig`,
  and `AacConfig`; the `hvcC` parser (`parse_parameter_sets`, `parse_hvcc`); the `esds` reader;
  and their errors `DecoderConfigError`, `HvccError`, and `EsdsError`.
- `fragment.rs` — `Sample`, `Fragment`, `FragmentHeader`, `encode_fragment`,
  `EncodedFragment`, the trailer codec, and the format constants (box flag bits included).
- `reader.rs` — `scan`, `scan_with_limit`, `parse_fragment`, their report types, and the
  `moof` table parsers both readers share.
- `index.rs` — the sidecar codec, `decode_index`, `IndexWriter`, the bounded readers, flag
  constants.
- `export.rs` — `rebase_fragment`, `trim_fragment_end`, `with_edit_list`, `ExportError`; it
  re-exports `MOVIE_TIMESCALE` from `init.rs`.
- `hls.rs` — `media_playlist`, `master_playlist`, `variant_bit_rates` (RFC 8216 peak and
  average segment bit rates), `PlaylistOptions`, `VariantBitRates`, `InvalidPlaylistText`.
- `crc.rs` — `Crc32c`, `crc32c`.

## How to verify a change

```sh
cargo fmt -p iris-mp4
cargo clippy -p iris-mp4 --all-targets -- -D warnings
cargo test -p iris-mp4
RUSTDOCFLAGS="-D warnings" cargo doc -p iris-mp4 --no-deps
just fuzz-build                                                   # every target still builds
cd fuzz && cargo fuzz run scan -- -max_total_time=60              # pinned nightly; after a parser
                                                                  # change: scan, box_header, index,
                                                                  # trailer, export_rebase, or
                                                                  # decoder_config
cargo run -p irisd -- inspect <file.mp4>                          # against a real recording
```

## Pitfalls

- `encode_fragment` rejects empty sample lists, more than 65 535 samples, and payloads
  over `MAX_PAYLOAD_LEN`; callers must handle `FragmentError`.
- `EncodedFragment::io_slices` must be written in order with one call; the trailer's
  `fragment_len` and CRC assume the prefix, samples, and trailer are contiguous.
- The scanner tolerates a missing `prft` but requires `moof` then `mdat` then the trailer;
  an open-ended `mdat` is a `BadFragment`.
- The memory parser enforces the same top-level order, bounds, and one exact final trailer.
  `decode_trailer` refuses a set reserved byte, so with the CRC every byte of a fragment is
  checked; `tests/properties.rs` holds that property and fails with the seed that broke it.
  Both readers require native moof-relative sample addressing, complete declared table fields,
  at least one sample, flags for the first sample, and containers without trailing bytes.
- `IndexRecord::decode` takes exactly 40 bytes and accepts any; `IndexRecord::end` saturates
  for a hand-built record. Readers drop a trailing partial record, and `decode_index` rejects
  impossible or noncontiguous fragment ranges.
- `media_playlist` trusts its records to come from a validated sidecar or scan; other records
  give ranges that do not describe the file, never a panic.
- Playlist byte ranges are contiguous per file; a change to grouping must keep
  `include_trailers = false` segments ending before each trailer (players reject unknown
  bytes inside a segment less than they reject gaps).
- Excluded-trailer playlists keep continuation fragments separate and claim independence
  only when every emitted segment starts at sync; the master makes no independence claim.
- `FragmentInfo::payload_offset` points at the first sample byte; the fragment `offset`
  points at `prft` (or `moof` without one).

## Related documents

- `docs/research/04-storage-format.md`, `07-architecture.md` §2 and §8, `12-ingest-cost.md` §5,
  `15-apple-clients.md` §4 and §5 (decoder configuration)
- Dependents: `iris-ingest`, `iris-store`, `iris-catalog`, `iris-api`, `iris-replay`, `irisd`,
  `iris-client`, whose live decoder reads a viewer's messages with `DecoderConfig::parse` and
  `reader::parse_fragment`, `iris-gui` (its tests), and the fuzz targets (`fuzz/Cargo.toml`).
