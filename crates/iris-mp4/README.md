# iris-mp4

The on-disk format of irisd recordings and the readers over it. A recording is an
initialization segment (`ftyp` + `moov`) followed by self-describing fragments, each
`prft` + `moof` + `mdat` + a 24-byte trailer carrying a CRC-32C and the fragment length,
with a sidecar index of fixed-size records. This crate encodes those bytes for the writer,
scans and validates them for recovery and self-checks, rewrites them for export, and builds
byte-range HLS playlists for offline serving. It also reads the decoder configuration of an
init segment, for a client and the replay server. It sits at the centre of the record path
(RTSP in → fMP4 on disk → byte-range serving).

## What it provides

- `init::SampleEntry` and `init::build_init_segment` — the `hvc1` sample entry and the
  `ftyp` + `moov` bytes of a file. An entry whose box is `mp4a` (`SampleEntry::audio`) gives
  the init segment of one audio track, in the same 90 kHz media timescale;
  `init::audio_frame_ticks` gives an audio frame's duration in it or refuses a sampling rate
  whose frames are not whole ticks. `init::build_av_init_segment` gives the init segment of
  a file with a video track and an audio track (track 2, `init::AV_AUDIO_TRACK_ID`), and
  `init::sample_entry_bytes` reads the sample entry back from an init segment.
- `decoder_config::DecoderConfig::parse` — what a decoder needs from the sample entry of an init
  segment, a `decoder_config::DecoderConfig`. For `hvc1` or `hev1` it is a
  `decoder_config::HevcConfig`: the parameter sets of `hvcC` and the picture size. For `mp4a` it is
  a `decoder_config::AacConfig`: the AudioSpecificConfig of `esds`, its sampling rate, and the
  channel count. `TryFrom<&[u8]>` does the same. `decoder_config::parse_parameter_sets` and
  `decoder_config::parse_hvcc` read `hvcC` alone. Refusals are `DecoderConfigError`, `HvccError`,
  and `EsdsError` variants.
- `fragment::Sample`, `fragment::Fragment`, `fragment::FragmentHeader`, and
  `fragment::encode_fragment` — one access unit, one assembled GOP, and the `prft`,
  `moof`, `mdat` header, and trailer around its samples (`EncodedFragment`).
  `fragment::FrameSteps` marks a fragment that holds a step of its stream: frames left out
  or recorded twice (`docs/research/13-audio.md` §5.3).
- `fragment::encode_trailer`, `decode_trailer`, and `Trailer` — the 24-byte `free` box with
  magic, CRC-32C, fragment length, and reserved bytes that must be zero.
- `export::rebase_fragment`, `export::trim_fragment_end`, and `export::with_edit_list` — the
  rewrites that make an export a stand-alone recording: a fragment's sequence, decode time, and
  `prft` media time with a recomputed trailer, a tail prefix containing only samples before the
  cutoff, and an `edts`/`elst` spliced into an init segment for start preroll. Tail clipping
  preserves retained sample bytes and shortens a crossing sample's duration.
  `export::retrack_fragment` moves a fragment to another track id (`tfhd`, `prft`, trailer),
  and `export::with_edit_lists` splices an edit list into each track of an init segment; the
  two make the audio track of a two-track export. `export::trim_fragment_start` cuts the
  start of a fragment whose samples are all sync samples at a sample boundary, which is how
  an export starts its sound (`docs/research/13-audio.md` §9).
- `reader::parse_fragment` — one fragment held in memory, with its sample tables, for a
  serving path that has read it out of a file (the keyframe resource).
- `reader::scan`, `reader::scan_with_limit`, `ScanOptions`, `ScanReport`, `FragmentInfo`, and
  `ScanStop` — a forward walk of a file that reports the valid length and why it stopped.
- `index::IndexHeader`, `IndexRecord` with its `FLAG_*` bits and `WRITER_ONLY_FLAGS`,
  `IndexWriter`, `decode_index`, `read_index_bounded`,
  `read_index_file_bounded`, and `read_index_file_bound_to_media` — the sidecar `.idx` codec,
  the writer, and the bounded readers, which reject a file whose metadata can hold more than the
  caller's record budget before allocating; `IndexWriter::from_claimed_file` initializes an
  exclusively claimed file.
- `hls::media_playlist`, `hls::master_playlist`, `hls::master_playlist_with_audio` (one
  variant and an audio rendition, `AudioRendition`), `hls::variant_bit_rates`, `PlaylistOptions`,
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
fragments with `export::rebase_fragment` and `export::trim_fragment_end`. `iris-replay` takes a
recording's parameter sets with `decoder_config::parse_parameter_sets`, and
`DecoderConfig::parse` gives a client the configuration of the decoder for a stream's init
segment (`docs/research/15-apple-clients.md` §4 and §5).

## Invariants and limits

- Byte layout of a file, including the trailer (`free`, magic `IRX1`, CRC-32C over `prft`
  through the end of `mdat`, `fragment_len`): `docs/research/04-storage-format.md §4`.
- Sidecar index: 64-byte header (`IRIX`, version 1, 40-byte records) and record fields:
  `04 §5`. The index caches what the fragment boxes say plus facts only the writer knows
  (`raw_rtp`, `run_id`, and the discontinuity, new-parameter, and step flags); a scan rebuilds the
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
- Decoder configuration: an `hvcC` is version 1 with four-byte NAL length prefixes and holds a VPS,
  an SPS, and a PPS, and at most 16 VPSs, 16 SPSs, and 64 PPSs. That is one set for each
  identifier that ITU-T H.265 defines for the type. `vps_video_parameter_set_id` is u(4)
  (§7.3.2.1), `sps_seq_parameter_set_id` is 0 to 15 (§7.4.3.2.1), and `pps_pic_parameter_set_id`
  is 0 to 63 (§7.4.3.3.1). A set is referred to by its identifier (§7.4.2.4.2).
  `docs/research/15-apple-clients.md` §1 names the edition these clauses come from. The set past
  the bound of its type is refused (`HvccError::TooManyParameterSets`) before it is copied, so a
  record of any length yields at most 96 sets of at most 65,535 bytes. An `esds` holds an
  ES_Descriptor, whose flags announce fields that are skipped. Inside it is a
  DecoderConfigDescriptor of MPEG-4 audio (object type `0x40`), and inside that the
  DecoderSpecificInfo. Each has a length of one to four bytes, high bits first. The
  AudioSpecificConfig is 2 to 64 bytes, and its audio object type is not the escape value 31. Its
  sampling rate comes from frequency index 0 to 12, or from index 15 and a nonzero 24-bit rate.
  The channel count is the entry's `channelcount`, never zero. A box of size 0 anywhere on the
  path to the configuration, which would run to the end of its parent, is refused.
- Bounds: a `moof` is at most `MAX_MOOF_LEN` (1 MiB), a payload at most `MAX_PAYLOAD_LEN` (1 GiB),
  and a fragment at most 65 535 samples.
- The CRC is CRC-32C (iSCSI polynomial); the implementation may change, the value may not.
- Export rebasing verifies the original trailer checksum before changing bytes; payload
  corruption is rejected rather than given a fresh checksum.
- Edit-list insertion rejects extended-size `moov` or `trak` parents.
- Trailer-free HLS uses one range per fragment, including continuations. Media playlists
  declare independence only when all emitted segments start at sync; the master leaves
  that declaration to the media playlist.

## Tests and measurements

The unit tests are in the `mod tests` of each module:

- `boxes`: box nesting and size patching, large and open-ended headers, bounds-checked field
  reads, and the payload length of open-ended and undersized headers.
- `init`: every field of the init segment, box by box, and its `ftyp` before its `moov`. Also the
  audio init segment against the video one, the two-track init segment against its single
  tracks, audio frame durations in ticks, the channel count of an `mp4a` entry, and the sample
  entry read back.
- `fragment`: the fragment layout and the encoder's rejections (empty, too many samples, a payload
  over the maximum). Also the payload duration and sync start a fragment reports, the default
  flags that say whether every sample is a sync sample, and trailer round trips and rejections.
- `reader`: the scanner on clean files, initialization-only files, an incomplete initialization
  segment, a partial fragment header, truncation, zero fill, corruption, and a trailer of the wrong
  length. Also boxes whose size runs past the file, the fragment limit, an initialization segment
  over 4 GiB, and the texts of three stops. Also the memory parser reading an encoded fragment back,
  and the rejections of both readers: fragment order, addressing, optional fields, missing flags,
  an empty `trun`, container padding, and the bounds.
- `index`: header rejections, header and record round trips, a record from a scanned fragment, the
  saturating end of a record built by hand, and a file read back with a partial tail. Also the
  rejections of a malformed cache, the bounded readers at their limit and one byte past it, and
  the refusal of a sidecar that does not match its media.
- `hls`: playlist grouping at sync samples, trailer exclusion and inclusion with continuations and
  the independence each declares, the master playlist and its audio rendition, and refused
  playlist text. Also the peak bit rate over the segment windows that qualify, and the average
  where none does.
- `export`: rebasing, with its refusals of payload corruption and of a fragment cut short. Tail
  trimming keeps the sample bytes and the sync samples, shortens the crossing duration, and
  refuses payload corruption and a duration that is not positive. Also head trimming, retracking,
  and edit lists for one track and for two, with the edit-parent and media-time restrictions.
- `crc`: a cross-check against the reference crate at every start offset and after every chunk,
  and the published check value.
- `decoder_config`: the tests read init segments built with the crate's writers. They check the
  configuration read from an HEVC and from an AAC entry, and through boxes with a 64-bit size.
  They check each refusal of the `hvcC` and `esds` readers in a separate test, with its text. They
  check each bound at its limit and one past it, and the input cut at every length.

`tests/properties.rs` holds seeded property tests over generated recordings. The scanner, the
memory parser, and the sidecar record read back what the encoder wrote. A file cut inside a
fragment is valid up to it, and a flipped bit anywhere in a fragment ends the valid prefix before
it. `tests/ffmpeg_hev1.rs` reads an `hev1` init segment that ffmpeg wrote from a synthetic frame.
It records how the file was made, and checks its picture size and the parameter sets that
ffmpeg's Annex B rewrite gives. `tests/ffmpeg_mp4a.rs` reads an `mp4a` init segment that ffmpeg
wrote from a synthetic tone, whose `esds` gives every descriptor length in four bytes. It
records how the file was made, and checks the AudioSpecificConfig, sampling rate, and channel
count that ffprobe reports. The doctest in `lib.rs` runs in the suite. Fuzz targets under
`fuzz/fuzz_targets/` (`scan`, `box_header`, `index`, `trailer`, `export_rebase` for the export
rewrites, and `decoder_config`) cover the parsers. Container overhead and the checksum's cost
are measured in `docs/research/04-storage-format.md §4` and `12-ingest-cost.md §5`.

## Further reading

- `docs/research/04-storage-format.md` §3 to §5, §9, §10
- `docs/research/07-architecture.md` §2 and §8
- `docs/research/12-ingest-cost.md` §5
- `docs/research/15-apple-clients.md` §4 and §5
- `STANDARDS.md`
