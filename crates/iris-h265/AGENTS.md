# AGENTS.md — iris-h265

Scope: this crate only. Read the root `AGENTS.md` first.

## Invariants

- The framing is 4-byte big-endian length prefixes; the walkers never accept Annex B
  (`docs/research/04-storage-format.md` §4).
- No function panics on any input: this crate faces camera bytes and has a fuzz target
  (`fuzz/fuzz_targets/access_unit.rs`); a crash reproducer becomes a unit test here.
- `NalType` is `#[non_exhaustive]`; an added variant keeps `code` exact and breaks no caller.
- Classification treats every type code below 32 as VCL, including reserved types 24..=31.
- Framing errors report the prefix-start offset; an incomplete prefix is its own variant
  (`FramingError::IncompletePrefix`), never a zero-length truncation.
- The crate stays decode-free: no bitstream parsing beyond the NAL header. Dimensions and
  profiles come from the RTSP client's parameters, not from here.

## Where things are

- `lib.rs` — everything: `NalType`, `nal_type`, `FramingError`, `LengthPrefixedNals`,
  `AccessUnitInfo`, `inspect_access_unit`, `strip_parameter_sets`, and the tests.

## How to verify a change

```sh
cargo fmt -p iris-h265
cargo clippy -p iris-h265 --all-targets -- -D warnings
cargo test -p iris-h265
RUSTDOCFLAGS="-D warnings" cargo doc -p iris-h265 --no-deps
cd fuzz && cargo fuzz run access_unit -- -max_total_time=60   # pinned nightly; after a parser change
```

## Pitfalls

- `nal_type` takes the first header byte only; the type is bits 1 to 6 (`(b >> 1) & 0x3f`).
- `LengthPrefixedNals` stops at the first framing error and yields it once; later units are
  not visited.
- `strip_parameter_sets` returns `None` when nothing was stripped so callers keep the
  original buffer; do not turn that into an empty copy.
- SEI units stay in samples; only VPS, SPS, and PPS are stripped (`04 §4`).

## Related documents

- `docs/research/01-camera-probe.md` §3, `04-storage-format.md` §4, `05-ingest-events.md` §2
- Dependents: `iris-ingest`, `iris-replay`.
