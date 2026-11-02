# iris-h265

H.265 (HEVC) NAL unit helpers for a recorder that never decodes. The crate classifies NAL
units from their two-byte headers, walks 4-byte-length-prefixed access units, and strips
parameter sets out of samples so that files use the `hvc1` sample entry with parameter
sets in `hvcC`. It sits between the RTSP client (which delivers assembled access units)
and the fragment encoder on the record path (RTSP in → fMP4 on disk → byte-range serving).

## What it provides

- `NalType` and `nal_type` — the NAL unit types irisd distinguishes (ITU-T H.265 Table 7-1)
  with `is_irap`, `is_vcl`, `is_parameter_set`, and `code`.
- `LengthPrefixedNals` — an iterator over the NAL units of a length-prefixed access unit
  that reports truncation and too-short units instead of panicking.
- `inspect_access_unit` and `AccessUnitInfo` — NAL count, IRAP presence, and in-band
  parameter-set bytes of one access unit.
- `strip_parameter_sets` — a copy of the access unit without VPS, SPS, and PPS, or `None`
  when there is nothing to strip.
- `FramingError` — the error type of the walkers.

## How it is used

```rust
use iris_h265::{NalType, inspect_access_unit, nal_type, strip_parameter_sets};

// A length-prefixed access unit: a VPS (type 32) followed by an IDR_W_RADL slice (type 19).
let au = [0, 0, 0, 2, 32 << 1, 1, 0, 0, 0, 3, 19 << 1, 1, 0xaa];
assert_eq!(nal_type(au[4]), NalType::Vps);
let info = inspect_access_unit(&au)?;
assert!(info.is_irap && info.has_parameter_sets);
let stripped = strip_parameter_sets(&au)?.expect("parameter sets were present");
assert_eq!(stripped, [0, 0, 0, 3, 19 << 1, 1, 0xaa]);
# Ok::<(), iris_h265::FramingError>(())
```

Ingest calls `inspect_access_unit` on every frame to learn whether it is a random access
point and `strip_parameter_sets` before the frame becomes a sample.

## Invariants and limits

- Access units are 4-byte big-endian length-prefixed NAL units, the `hvc1` sample format;
  Annex B start codes never appear on disk (`docs/research/04-storage-format.md §4`).
- A NAL unit is at least two bytes (its header); shorter units are a `FramingError`.
- Parameter sets (VPS, SPS, PPS) live only in `hvcC`; the recorder strips in-band copies
  and seals a file when they change (`04 §4`, `05-ingest-events.md §2`).
- Every function accepts arbitrary bytes without panicking; the fuzz target
  `fuzz/fuzz_targets/access_unit.rs` enforces it (root `AGENTS.md` rule 8).

## Tests and measurements

Unit tests cover type classification, stripping with slices kept, and truncation
reporting; the crate-level doctest exercises `nal_type`. The fuzz target `access_unit`
feeds arbitrary bytes through the walkers. The elementary-stream facts the helpers rely on
(FU-only packetisation, parameter sets before every IDR, no AUD) are measured in
`docs/research/01-camera-probe.md §3`.

## Further reading

- `docs/research/01-camera-probe.md` §3 (elementary-stream structure)
- `docs/research/04-storage-format.md` §4 (sample format)
- `docs/research/05-ingest-events.md` §2 (per-stream pipeline)
- `STANDARDS.md`
