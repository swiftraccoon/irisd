# Fuzz targets

Coverage-guided fuzzing for the parsers that face untrusted bytes. The crate is excluded
from the workspace so that stable builds do not compile it.

| Target | Parser |
|---|---|
| `api_time` | API query and retention timestamps, including non-ASCII input and fractional seconds |
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
| `onvif_soap` | bounded SOAP/XML structure parsing and destruction |
| `rtsp_request` | the replay server's RTSP request parser |
| `rtsp_message` | the vendored retina RTSP message parser for camera replies and interleaved-data headers: contiguous against chunked ring-buffer input, under the production 1 MiB limit and a fuzz-chosen limit |
| `tools_protocol` | the iris-tools probe parsers: RTP payload bounds, RTSP response bounds, and camera UTC |
| `describe_diagnostics` | retina's `DESCRIBE` interpretation: every error and every log record at `info` or above stays under 2 KiB, the SDP parser's are escaped single lines, and it warns at most once per response |

Retina's SDP parsing, RTP depacketization, and RTSP session state have no target here; the
vendored crate's own tests (`just vendor-test`) and the `iris-ingest` integration tests
exercise them.

Requirements: `cargo-fuzz` and the nightly toolchain that `rust-toolchain.toml` in this
directory pins; `rustup toolchain install`, run here, installs it. `cargo +nightly fuzz` runs
the same commands on the floating nightly instead.

```sh
cargo install cargo-fuzz --version 0.13.2 --locked
cd fuzz
rustup toolchain install
cargo fuzz run scan                       # any target name from the table
cargo fuzz run multipart -- -max_total_time=3600
```

`just fuzz-build` builds every target with the pinned toolchain, as CI does on every push and
pull request.

Seed corpora go under `fuzz/corpus/<target>/`; captured camera data (never committed) makes
good seeds for `scan`, `access_unit`, and `multipart`, and the RTSP transcripts in
`third_party/retina/src/client/testdata/`, each prefixed with the bytes `ff ff 00` (a
65,535-byte size limit and no chunking control bytes), seed `rtsp_message`. A crash reproducer
lands in `fuzz/artifacts/` and should become a unit test in the owning crate before the fix is
merged. The phase-2 exit criterion is 24 hours per target without findings
(docs/research/08-roadmap.md).
