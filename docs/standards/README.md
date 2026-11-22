# Standards documents

This directory holds the IETF documents that the record path, the serving path, and the rules of
`crates/iris-client` follow. They are kept verbatim so that reviews and design discussions cite
local text. They define the protocols, and two of them, RFC 1918 and RFC 6598, define address
ranges. The last section records the ISO/IEC and ITU-T texts that the tree holds no copy of. The
engineering standards (Rust style, API guidelines, Google's Rust and documentation guidance,
Conventional Commits, Semantic Versioning) live in the reference pack under `.rust-reference/`;
`STANDARDS.md` at the repository root is the digest of what irisd adopts from all of them.

| Directory | Documents | License |
|---|---|---|
| `ietf/` | RFC 2326 (RTSP 1.0), RFC 3550 (RTP), RFC 3551 (the audio and video RTP profile: L16, PCMA), RFC 3640 (MPEG-4 elementary streams over RTP, the `mpeg4-generic` payload of AAC), RFC 7798 (HEVC RTP payload), RFC 8216 (HLS) and draft-pantos-hls-rfc8216bis-22, RFC 6381 (codecs parameter), RFC 6455 (WebSocket), RFC 1918 (the private IPv4 address ranges), RFC 6598 (the shared IPv4 address range) | IETF Trust |

The files are the published text from `https://www.rfc-editor.org/rfc/rfcNNNN.txt` and
`https://www.ietf.org/archive/id/draft-pantos-hls-rfc8216bis-22.txt`; a published RFC does
not change, so there is nothing to refresh. Do not edit them.

## ISO/IEC and ITU-T texts without a local copy

The tree holds no copy of ISO/IEC 14496, of ISO/IEC 23000-19 (CMAF), or of ITU-T H.265. ISO/IEC
14496 and ITU-T H.265 define boxes and bitstream fields that the code reads, and
`docs/research/13-audio.md` names CMAF for the timescale of an audio track. Cite a clause of one of
these texts only where the clause was read. Where none was read, cite the check that was made
against a program or a file from outside the code under test. Mark a decision that no such check
settles **unverified** (`AGENTS.md` rule 11).

| Standard | What is recorded of the text | What stands in for it |
|---|---|---|
| ISO/IEC 14496-12 (the box syntax of `moov`, `moof`, `tfhd`, `tfdt`, `trun`, and `prft`) | Not read (`docs/research/04-storage-format.md` §4, `docs/research/13-audio.md` §9.1, `docs/research/15-apple-clients.md` §1). Whether ISO offers it at no charge is **unverified**. ISO's page "Publicly Available Standards", read on 2026-11-18 at <https://standards.iso.org/ittf/PubliclyAvailableStandards/index.html>, says: "The deliverables previously available on this site are now available at no charge on the ISO and IEC webstores". The page lists no standard, and the store's page for this part was not read. The vendored retina links the 2015 edition at an address under that site (`third_party/retina/src/codec/aac.rs`). | The files that ffmpeg writes and reads. The daemon writes the `prft` flags that ffmpeg writes (`04-storage-format.md` §4), and ffmpeg and GStreamer decode the daemon's exports (`13-audio.md` §9.1). On macOS 27, AVPlayer played the daemon's playlists and their byte ranges (`15-apple-clients.md` §3.2, a dated observation). |
| ISO/IEC 14496-15 (`hvcC` and the `hvc1` and `hev1` sample entries) | As for 14496-12: not read, and whether ISO offers it at no charge is **unverified**. | The writer of the daemon's `hvcC`, the vendored retina (`third_party/retina/src/codec/h265/record.rs`). An `hev1` init segment that ffmpeg wrote, held in `crates/iris-mp4/tests/ffmpeg_hev1.rs`, with the parameter sets that ffmpeg's `hevc_mp4toannexb` filter read from the same record. ffmpeg decoding the daemon's video by the parameter sets that `iris_mp4::decoder_config` reads (`15-apple-clients.md` §3.6 and §11). |
| ISO/IEC 14496-14 (`esds`), ISO/IEC 14496-1 (the descriptors inside `esds`), and ISO/IEC 14496-3 (AudioSpecificConfig) | As for 14496-12: not read, and whether ISO offers them at no charge is **unverified**. | The writer of the daemon's `esds`, the vendored retina, whose comments cite the clauses it follows (`third_party/retina/src/codec/aac.rs`). An `mp4a` init segment that ffmpeg wrote, held in `crates/iris-mp4/tests/ffmpeg_mp4a.rs`, with the configuration, rate, and channel count that ffprobe reports for it. ffmpeg decoding the daemon's audio by the AudioSpecificConfig that `iris_mp4::decoder_config` reads (`15-apple-clients.md` §3.6 and §11). |
| ISO/IEC 23000-19 (CMAF), which `docs/research/13-audio.md` §1 names for the timescale of an audio track | As for 14496-12: not read, and whether ISO offers it at no charge is **unverified**. | Nothing stands in for its text: whether it constrains the timescale of an audio track is **unverified** (`13-audio.md` §1 and §4). FFmpeg and GStreamer decode the daemon's audio from a track with a 90 kHz timescale (`13-audio.md` §3). On macOS 27, AVFoundation opened an export whose audio track has that timescale (`15-apple-clients.md` §3.2, a dated observation). |
| ITU-T H.265 (the bitstream, its NAL unit types, and the identifiers of its parameter sets) | Read from ITU's download. ITU's page for Recommendation H.265 (01/26), <https://www.itu.int/rec/T-REC-H.265-202601-I>, lists the text under "Freely available items". The page "ITU Copyright Information", <https://www.itu.int/home/copyright/index.html>, obliges a user "not to duplicate the document or parts thereof for distribution or sale external to the user's organization". Both pages were read on 2026-11-18. | The clauses themselves, read from ITU's download and cited by number with the edition (`15-apple-clients.md` §1), and RFC 7798 under `ietf/`, which restates the NAL unit header and types. |
