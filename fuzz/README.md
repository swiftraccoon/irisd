# Fuzz targets

Coverage-guided fuzzing for the parsers that face untrusted bytes. The crate is excluded
from the workspace so that stable builds do not compile it.

| Target | Parser |
|---|---|
| `api_time` | API query and retention timestamps, including non-ASCII input and fractional seconds |
| `api_auth` | the bearer middleware's pre-authentication parsers: the `token` query parameter and the encoded capability, which round-trips through its canonical form |
| `api_range` | the raw recording route's `Range` header parser: clamped ranges, refusals, ignored units, and the bounded-class length bound |
| `scan` | the recording scanner (`iris_mp4::reader::scan` and `scan_with_limit`), with and without CRC verification and under a small fragment limit |
| `box_header` | ISO BMFF box headers and the big-endian field readers at arbitrary offsets |
| `index` | the sidecar index codec: the header and record codecs, and `decode_index` over a whole file image (accepted records are contiguous and within the budget) |
| `trailer` | the fragment trailer decoder |
| `export_rebase` | the export rewrites: `parse_fragment`, `rebase_fragment` (a rewritten fragment parses with the given values), `retrack_fragment`, `trim_fragment_end` (a trimmed fragment parses and fits the limit), `trim_fragment_start` (the remainder is all sync samples and starts later by what was left out), `with_edit_list` and `with_edit_lists`, and the sample entry and channel count read from an init segment; a fragment encoded from the input reaches the rewrites past the checksum |
| `decoder_config` | the decoder configuration of an init segment (`iris_mp4::decoder_config`): `DecoderConfig::parse` and `parse_parameter_sets` over an init segment, and `parse_hvcc` over an `hvcC` body. Accepted HEVC holds a VPS, an SPS, and a PPS, at most 16, 16, and 64 of them, none empty and none of another NAL type. `DecoderConfig::parse` gives HEVC exactly when `parse_parameter_sets` gives parameter sets, and then the same sets. Accepted AAC has an AudioSpecificConfig of 2 to 64 bytes, a nonzero sampling rate, and a nonzero channel count. The input taken as the `esds` of an `mp4a` entry reaches the descriptor reader, and gives AAC or an `esds` refusal |
| `access_unit` | H.265 length-prefixed access-unit framing and parameter-set stripping |
| `multipart` | the multipart event-feed reader and `boundary_from_content_type`, any boundary and any chunking: a fragmented feed yields the parts of the whole feed, and scan work stays linear |
| `digest_challenge` | HTTP digest challenge parsing and the header built from it |
| `dahua_part` | the vendor event-part parser and normaliser |
| `onvif_utc` | the ONVIF timestamp parser and topic mapping |
| `onvif_notification` | the ONVIF notification normaliser: any topic, operation, time, and items give a bounded event with valid JSON data, or none |
| `onvif_soap` | bounded SOAP/XML structure parsing and destruction |
| `discovery_parts` | mapping of WS-Discovery answers (endpoint, scopes, service URLs) to discovered cameras |
| `rtsp_request` | the replay server's RTSP request parser and the header parsers it applies: mount-name extraction, the `Transport` channel negotiation, and the `Session` id |
| `rtsp_message` | the vendored retina RTSP message parser for camera replies and interleaved-data headers: contiguous against chunked ring-buffer input, under the production 1 MiB limit and a fuzz-chosen limit |
| `tools_protocol` | the camera probes' parsers (`crates/iris-tools/src/probe_parser.rs` with `time.rs`, included by path): the interleaved stream, whose items stay inside the buffer and always consume bytes, RTSP response framing, RTP and RTCP packets, H.265 payload units, timestamp unwrapping, and the digest challenge, SDP control, transport, header, and camera UTC parsers |
| `tools_logs` | the report tools' log parsers (`crates/iris-tools/src/logs.rs` with `time.rs`, included by path): the line bound, escape removal that leaves no control or bidirectional formatting character, line splitting, statistics classification, and pair parsing |
| `describe_diagnostics` | retina's `DESCRIBE` interpretation, SDP parsing included: every error and every log record at `info` or above stays within 2 KiB, the SDP parser's are escaped single lines, and it warns at most once per response |
| `depacketize_h265` | retina's H.265 depacketizer under arbitrary payloads, sequence numbers, timestamps, and marker bits: every frame stays within the 64 MiB access-unit bound |
| `rtp_packet` | retina's RTP header validation over a contiguous slice and the two halves of a ring buffer, which must agree |
| `vod_playlist` | the VOD playlist rules a viewer applies (`iris_client::vod`). Accepted fragment timing holds 1 to `MAX_PLAYBACK_FRAGMENTS` fragments, each of a positive duration whose end fits the wall-clock range. An accepted rewrite and its input are each at most `MAX_VOD_PLAYLIST_BYTES`. The rewrite holds only the daemon's tags and absolute references on the API origin without userinfo, with the input's timing. The multivariant playlist built over an accepted body has five lines, which name the two media playlists and a positive bit rate |
| `live_fragment` | what a viewer reads from a live payload before its demuxer has it (`iris_client::live::media_time`): a media time only for a payload that starts with a 32-byte version-1 `prft` box, and then the box's ticks |
| `live_message` | the framing of the live feed's messages (`iris_client::live`). An init segment or a fragment gives the payload after its type byte. Any other message is refused as empty or for its type byte, and the refusal names that byte |
| `live_sample` | the live decoder (`iris_client::live::LiveDecoder`) over a run of messages, each a big-endian `u16` header (the length, and a top bit that resets the decoder first) and its bytes. Every message gives a configuration, a sample of at least one byte that lies inside the message past its type byte, or the refusal that a model of the decoder's rules expects. A sample comes only while the decoder has a configuration, and the first after each configuration or refused fragment is a sync sample |
| `client_answers` | how a viewer reads an answer (`iris_client::api`), over any status, content type, and body, held whole or not, from the API origin or another. `HttpRequest::body_to_read` of every call says what to read of the body, and every check gives the call's value or a refusal whose text the target builds. Neither panics |

Retina's RTCP compound validation and its `SETUP` and `PLAY` interpretation have no target
here; the vendored crate's own tests (`just vendor-test`) and the `iris-ingest` integration
tests exercise them.

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

`just fuzz-build` builds every target with the pinned toolchain and `just fuzz-clippy` lints
them, as CI does on every push to `main` and every pull request. `cargo fmt --all` and the
workspace's lint tables apply to the targets too; `fuzz/Cargo.toml` repeats the tables because
the crate is not a workspace member.

Seed corpora go under `fuzz/corpus/<target>/`:

- `scan`, `access_unit`, and `multipart`: captured camera media, which is never committed;
- `vod_playlist`: a playlist the daemon served;
- `client_answers`: an answer the daemon served, behind the head that the target's documentation
  lays out;
- `decoder_config`: the init segment of a recording;
- `live_sample`: the messages under 32 KiB of a live session, such as an audio or a sub-stream
  session, each behind its `u16` length with the top bit clear. The header holds 15 bits of
  length, so a message of 32 KiB or more cannot be framed;
- `rtsp_message`: the RTSP transcripts in `third_party/retina/src/client/testdata/`, each
  prefixed with the bytes `ff ff 00` (a 65,535-byte size limit and no chunking control bytes).

A crash reproducer lands in `fuzz/artifacts/` and should become a unit test in the owning crate
before the fix is merged. The phase-2 exit criterion is 24 hours per target without findings
(`docs/research/08-roadmap.md`); completed runs are recorded in
`docs/research/10-phase2-results.md`.
