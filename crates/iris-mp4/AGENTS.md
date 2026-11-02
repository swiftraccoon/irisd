# AGENTS.md — iris-mp4

Scope: this crate only. Read the root `AGENTS.md` first.

## Invariants

- The byte layout in `docs/research/04-storage-format.md` §4 (boxes, versions, flags,
  trailer) and §5 (sidecar header and record) is the format; a change there is a format
  version change and updates the design document first. Files written before the change
  must still scan.
- The trailer CRC is CRC-32C over `prft` through the end of `mdat`; `crc.rs` may swap
  implementations only with the cross-check test green.
- `reader::scan` stops at the first inconsistency and reports the valid length; it never
  panics on any input (fuzz targets `scan`, `box_header`, `index`, `trailer`).
- Timescale 90 kHz, 64-bit `tfdt`, 32-bit per-sample durations (`04 §4`, §7).

## Where things are

- `boxes.rs` — `BoxWriter` (nested boxes with patched sizes), `BoxHeader`, `be_u32`,
  `be_u64`.
- `init.rs` — `SampleEntry`, `build_init_segment`, `VIDEO_TRACK_ID`.
- `fragment.rs` — `Sample`, `Fragment`, `FragmentHeader`, `encode_fragment`,
  `EncodedFragment`, the trailer codec and the format constants.
- `reader.rs` — `scan` and its report types.
- `index.rs` — the sidecar codec, `IndexWriter`, `read_index`, flag constants.
- `hls.rs` — `media_playlist`, `master_playlist`, `PlaylistOptions`.
- `crc.rs` — `Crc32c`, `crc32c`.

## How to verify a change

```sh
cargo fmt -p iris-mp4
cargo clippy -p iris-mp4 --all-targets -- -D warnings
cargo test -p iris-mp4
RUSTDOCFLAGS="-D warnings" cargo doc -p iris-mp4 --no-deps
cd fuzz && cargo +nightly fuzz run scan -- -max_total_time=60   # after a parser change
cargo run -p irisd -- inspect <file.mp4>                          # against a real recording
```

## Pitfalls

- `encode_fragment` rejects empty sample lists, more than 65 535 samples, and payloads
  over `MAX_PAYLOAD_LEN`; callers must handle `FragmentError`.
- `EncodedFragment::io_slices` must be written in order with one call; the trailer's
  `fragment_len` and CRC assume the prefix, samples, and trailer are contiguous.
- The scanner tolerates a missing `prft` but requires `moof` then `mdat` then the trailer;
  an open-ended `mdat` is a `BadFragment`.
- `IndexRecord::decode` takes exactly 40 bytes; readers drop a trailing partial record.
- Playlist byte ranges are contiguous per file; a change to grouping must keep
  `include_trailers = false` segments ending before each trailer (players reject unknown
  bytes inside a segment less than they reject gaps).
- `FragmentInfo::payload_offset` points at the first sample byte; the fragment `offset`
  points at `prft` (or `moof` without one).

## Related documents

- `docs/research/04-storage-format.md`, `07-architecture.md` §2 and §8, `12-ingest-cost.md` §5
- Dependents: `iris-ingest`, `iris-store`, `iris-catalog`, `iris-api`, `iris-replay`,
  `irisd`.
