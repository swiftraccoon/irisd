# Fuzz targets

Coverage-guided fuzzing for the parsers that face untrusted bytes. The crate is excluded
from the workspace so that stable builds do not compile it.

| Target | Parser |
|---|---|
| `scan` | the recording scanner (`iris_mp4::reader::scan`), with and without CRC verification |
| `box_header` | ISO BMFF box headers |
| `index` | the sidecar index codec |
| `trailer` | the fragment trailer decoder |
| `export_rebase` | the export rewrites: `parse_fragment`, `rebase_fragment` (a rewritten fragment must parse with the new values), `with_edit_list` |
| `access_unit` | H.265 length-prefixed access-unit framing and parameter-set stripping |
| `multipart` | the multipart event-feed reader, any boundary and any chunking |
| `digest_challenge` | HTTP digest challenge parsing and the header built from it |
| `dahua_part` | the vendor event-part parser and normaliser |
| `onvif_utc` | the ONVIF timestamp parser and topic mapping |
| `rtsp_request` | the replay server's RTSP request parser |

Requirements: a nightly toolchain and `cargo-fuzz`.

```sh
rustup toolchain install nightly
cargo install cargo-fuzz
cd fuzz
cargo +nightly fuzz run scan                       # any target name from the table
cargo +nightly fuzz run multipart -- -max_total_time=3600
```

Seed corpora go under `fuzz/corpus/<target>/`; captured camera data (never committed) makes
good seeds for `scan`, `access_unit`, and `multipart`. A crash reproducer lands in
`fuzz/artifacts/` and should become a unit test in the owning crate before the fix is merged.
The phase-2 exit criterion is 24 hours per target without findings (docs/research/08-roadmap.md).
