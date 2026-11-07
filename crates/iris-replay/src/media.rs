//! A recording loaded into memory with its parameter sets and frame table.

use std::path::Path;

use anyhow::{Context, bail};
use bytes::Bytes;
use iris_h265::inspect_access_unit;
use iris_mp4::boxes::BoxHeader;
use iris_mp4::reader::{ScanOptions, scan};

/// Largest decode time whose 90 kHz-to-microsecond conversion cannot overflow.
pub const MAX_REPLAY_TICKS: u64 = u64::MAX / 100;

/// One access unit in the file: a length-prefixed sample.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Frame {
    /// Byte offset of the sample in the file.
    pub offset: usize,
    /// Sample length in bytes.
    pub len: usize,
    /// Duration in 90 kHz ticks.
    pub duration: u32,
    /// Decode time in 90 kHz ticks from the start of the file.
    pub decode_time: u64,
    /// Whether the sample is a random access point.
    pub is_sync: bool,
}

/// A recording ready to replay.
#[derive(Debug)]
pub struct Media {
    /// The whole file.
    pub data: Bytes,
    /// VPS, SPS, and PPS from the sample entry, without length prefixes.
    pub parameter_sets: Vec<Vec<u8>>,
    /// Frames in decode order.
    pub frames: Vec<Frame>,
    /// Total duration in ticks.
    pub total_ticks: u64,
}

impl Media {
    /// Reads and indexes a recording.
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let data = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
        Self::from_bytes(Bytes::from(data)).with_context(|| format!("indexing {}", path.display()))
    }

    /// Indexes a recording held in memory.
    pub fn from_bytes(data: Bytes) -> anyhow::Result<Self> {
        let report = scan(
            std::io::Cursor::new(&data[..]),
            ScanOptions {
                verify_crc: true,
                collect_samples: true,
            },
        )
        .context("scanning")?;
        if let Some(stop) = &report.stop {
            bail!("recording is not CRC-clean through exact EOF: {stop:?}");
        }
        if report.valid_len != data.len() as u64 {
            bail!(
                "recording scan accepted {} of {} bytes",
                report.valid_len,
                data.len()
            );
        }
        if report.fragments.is_empty() {
            bail!("no fragments");
        }
        let init_len = usize::try_from(report.init_len).context("init length")?;
        let parameter_sets = parameter_sets(&data[..init_len]).context("parameter sets")?;
        // Stored decode times are continuous within a recording run, so a file
        // that is not the first of its run starts far from zero; playback times
        // are relative to the file's first fragment.
        let base = report.fragments[0].decode_time;
        let mut frames = Vec::new();
        let mut previous_end = 0u64;
        for fragment in &report.fragments {
            let mut offset = usize::try_from(fragment.payload_offset).context("offset")?;
            let mut decode_time = fragment
                .decode_time
                .checked_sub(base)
                .context("fragment decode time precedes the first fragment")?;
            if decode_time < previous_end {
                bail!("fragment decode time {decode_time} overlaps preceding end {previous_end}");
            }
            for (index, (&size, &duration)) in fragment
                .sample_sizes
                .iter()
                .zip(&fragment.sample_durations)
                .enumerate()
            {
                if duration == 0 {
                    bail!("sample duration is zero");
                }
                let len = usize::try_from(size).context("sample size")?;
                let end = offset.checked_add(len).context("sample offset overflow")?;
                let sample = data.get(offset..end).context("sample past end of file")?;
                let info = inspect_access_unit(sample)
                    .with_context(|| format!("invalid access unit at byte {offset}"))?;
                if info.nal_count == 0 {
                    bail!("empty access unit at byte {offset}");
                }
                if index == 0 && fragment.starts_with_sync != info.is_irap {
                    bail!("fragment sync flag disagrees with its first access unit");
                }
                frames.push(Frame {
                    offset,
                    len,
                    duration,
                    decode_time,
                    is_sync: info.is_irap,
                });
                offset = end;
                decode_time = decode_time
                    .checked_add(u64::from(duration))
                    .context("sample decode time overflow")?;
                if decode_time > MAX_REPLAY_TICKS {
                    bail!("media timeline exceeds the replay clock range");
                }
            }
            previous_end = decode_time;
        }
        let Some(first) = frames.first() else {
            bail!("recording carries no samples");
        };
        if !first.is_sync {
            bail!("recording does not begin with a sync sample");
        }
        let total_ticks = previous_end;
        if total_ticks == 0 {
            bail!("recording timeline does not advance");
        }
        Ok(Self {
            data,
            parameter_sets,
            frames,
            total_ticks,
        })
    }

    /// The bytes of a frame.
    #[must_use]
    pub fn sample(&self, frame: &Frame) -> &[u8] {
        &self.data[frame.offset..frame.offset + frame.len]
    }
}

/// Extracts the VPS, SPS, and PPS NAL units from an init segment's `hvcC`.
pub fn parameter_sets(init: &[u8]) -> anyhow::Result<Vec<Vec<u8>>> {
    let moov = child(init, *b"moov").context("moov missing")?;
    let trak = child(moov, *b"trak").context("trak missing")?;
    let mdia = child(trak, *b"mdia").context("mdia missing")?;
    let minf = child(mdia, *b"minf").context("minf missing")?;
    let stbl = child(minf, *b"stbl").context("stbl missing")?;
    let stsd = child(stbl, *b"stsd").context("stsd missing")?;
    // stsd is a full box: version/flags then entry_count, then the entries.
    let entries = stsd.get(8..).context("stsd too short")?;
    let entry = first_box(entries).context("sample entry missing")?;
    if &entry.kind != b"hvc1" && &entry.kind != b"hev1" {
        bail!(
            "sample entry is not HEVC: {:?}",
            String::from_utf8_lossy(&entry.kind)
        );
    }
    // A visual sample entry has 78 bytes of fixed fields after the box header.
    let children = entry.body.get(78..).context("sample entry too short")?;
    let hvcc = child(children, *b"hvcC").context("hvcC missing")?;
    parse_hvcc(hvcc)
}

/// Reads the NAL unit arrays of an `hvcC` body (ISO/IEC 14496-15 §8.3.3.1).
pub fn parse_hvcc(body: &[u8]) -> anyhow::Result<Vec<Vec<u8>>> {
    if body.first() != Some(&1) {
        bail!("unsupported hvcC configuration version");
    }
    if body.get(21).context("hvcC too short")? & 0x03 != 3 {
        bail!("hvcC does not use four-byte NAL length prefixes");
    }
    let count = usize::from(*body.get(22).context("hvcC too short")?);
    let mut pos = 23;
    let mut sets = Vec::new();
    for _ in 0..count {
        let array_header = *body.get(pos).context("hvcC array truncated")?;
        if array_header & 0x40 != 0 {
            bail!("hvcC array reserved bit is set");
        }
        let array_type = array_header & 0x3f;
        let nalus = usize::from(u16::from_be_bytes([
            *body.get(pos + 1).context("hvcC array truncated")?,
            *body.get(pos + 2).context("hvcC array truncated")?,
        ]));
        pos += 3;
        for _ in 0..nalus {
            let len = usize::from(u16::from_be_bytes([
                *body.get(pos).context("hvcC nalu truncated")?,
                *body.get(pos + 1).context("hvcC nalu truncated")?,
            ]));
            pos += 2;
            let end = pos.checked_add(len).context("hvcC nalu length overflow")?;
            let nal = body.get(pos..end).context("hvcC nalu truncated")?;
            if nal.len() < 2 {
                bail!("hvcC NAL unit is shorter than its two-byte header");
            }
            let actual_type = (nal[0] >> 1) & 0x3f;
            if actual_type != array_type {
                bail!("hvcC array type {array_type} contains NAL unit type {actual_type}");
            }
            if matches!(array_type, 32..=34) {
                sets.push(nal.to_vec());
            }
            pos = end;
        }
    }
    if pos != body.len() {
        bail!("hvcC has trailing bytes");
    }
    for required in 32..=34 {
        if !sets.iter().any(|nal| ((nal[0] >> 1) & 0x3f) == required) {
            bail!("hvcC is missing parameter-set type {required}");
        }
    }
    Ok(sets)
}

/// A box found inside a buffer.
struct FoundBox<'a> {
    kind: [u8; 4],
    body: &'a [u8],
}

fn first_box(buf: &[u8]) -> Option<FoundBox<'_>> {
    let header = BoxHeader::parse(buf)?;
    let size = usize::try_from(header.size.unwrap_or(buf.len() as u64)).ok()?;
    let body = buf.get(usize::from(header.header_len)..size)?;
    Some(FoundBox {
        kind: header.kind,
        body,
    })
}

/// The body of the first child box of type `kind`.
fn child(buf: &[u8], kind: [u8; 4]) -> Option<&[u8]> {
    let mut rest = buf;
    while !rest.is_empty() {
        let header = BoxHeader::parse(rest)?;
        let size = usize::try_from(header.size.unwrap_or(rest.len() as u64)).ok()?;
        if size < usize::from(header.header_len) || size > rest.len() {
            return None;
        }
        if header.kind == kind {
            return Some(&rest[usize::from(header.header_len)..size]);
        }
        rest = &rest[size..];
    }
    None
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;
    use std::sync::Arc;

    use iris_core::time::WallUs;
    use iris_mp4::fragment::{FragmentHeader, Sample, encode_fragment};
    use iris_mp4::init::build_init_segment;
    use iris_mp4::reader::{ScanOptions, scan};

    use super::*;
    use crate::synth::{self, TEST_PPS, TEST_SPS, TEST_VPS};

    fn sample(index: u64, is_sync: bool, duration: u32) -> Sample {
        let nal = synth::slice(index, is_sync, 8).unwrap();
        let mut data = Vec::with_capacity(4 + nal.len());
        data.extend_from_slice(&(nal.len() as u32).to_be_bytes());
        data.extend_from_slice(&nal);
        Sample {
            data: Bytes::from(data),
            duration,
            is_sync,
        }
    }

    fn recording_with_fragments(fragments: &[(u64, Vec<Sample>)]) -> Bytes {
        let entry = Arc::new(
            synth::sample_entry(&[TEST_VPS.to_vec(), TEST_SPS.to_vec(), TEST_PPS.to_vec()])
                .unwrap(),
        );
        let mut out = build_init_segment(&entry).to_vec();
        for (index, (decode_time, samples)) in fragments.iter().enumerate() {
            let encoded = encode_fragment(
                &FragmentHeader {
                    sequence: u32::try_from(index + 1).unwrap(),
                    decode_time: *decode_time,
                    wall: WallUs::new(1_700_000_000_000_000),
                },
                samples,
            )
            .unwrap();
            out.extend_from_slice(&encoded.prefix);
            for sample in samples {
                out.extend_from_slice(&sample.data);
            }
            out.extend_from_slice(&encoded.trailer);
        }
        Bytes::from(out)
    }

    fn hvcc(arrays: &[(u8, Vec<Vec<u8>>)]) -> Vec<u8> {
        let mut body = vec![
            1, 0x01, 0x60, 0, 0, 0, 0x90, 0, 0, 0, 0, 0, 0x5a, 0xf0, 0x00, 0xfc, 0xfd, 0xf8, 0xf8,
            0, 0, 0x0f,
        ];
        body.push(u8::try_from(arrays.len()).unwrap());
        for (kind, nals) in arrays {
            body.push(0x80 | kind);
            body.extend_from_slice(&u16::try_from(nals.len()).unwrap().to_be_bytes());
            for nal in nals {
                body.extend_from_slice(&u16::try_from(nal.len()).unwrap().to_be_bytes());
                body.extend_from_slice(nal);
            }
        }
        body
    }

    fn required_hvcc_arrays() -> Vec<(u8, Vec<Vec<u8>>)> {
        vec![
            (32, vec![TEST_VPS.to_vec()]),
            (33, vec![TEST_SPS.to_vec()]),
            (34, vec![TEST_PPS.to_vec()]),
        ]
    }

    #[test]
    fn parameter_sets_come_from_hvcc() {
        let entry = synth::sample_entry(&[TEST_VPS.to_vec(), TEST_SPS.to_vec(), TEST_PPS.to_vec()])
            .unwrap();
        let init = build_init_segment(&entry);
        let sets = parameter_sets(&init).unwrap();
        assert_eq!(
            sets,
            vec![TEST_VPS.to_vec(), TEST_SPS.to_vec(), TEST_PPS.to_vec()]
        );
    }

    #[test]
    fn decode_times_are_relative_to_the_first_fragment() {
        let bytes = synth::recording(synth::Plan {
            gops: 2,
            frames_per_gop: 3,
            start_ticks: 90_000 * 3_600,
            ..synth::Plan::default()
        })
        .unwrap();
        let media = Media::from_bytes(bytes).unwrap();
        assert_eq!(media.frames[0].decode_time, 0);
        assert_eq!(media.frames[1].decode_time, 3_000);
        assert_eq!(media.frames[3].decode_time, 9_000);
        assert_eq!(media.total_ticks, 18_000);
    }

    #[test]
    fn frames_are_indexed_with_sync_flags_and_decode_times() {
        let bytes = synth::recording(synth::Plan {
            gops: 2,
            frames_per_gop: 3,
            ..synth::Plan::default()
        })
        .unwrap();
        let media = Media::from_bytes(bytes).unwrap();
        assert_eq!(media.frames.len(), 6);
        assert!(media.frames[0].is_sync && media.frames[3].is_sync);
        assert!(!media.frames[1].is_sync);
        assert_eq!(media.frames[1].decode_time, 3_000);
        assert_eq!(media.total_ticks, 18_000);
        assert_eq!(media.sample(&media.frames[0]).len(), media.frames[0].len);
        assert_eq!(media.parameter_sets.len(), 3);
    }

    #[test]
    fn timelines_reject_zero_regression_and_overlap_but_preserve_gaps() {
        let zero = recording_with_fragments(&[(0, vec![sample(0, true, 0)])]);
        assert!(
            Media::from_bytes(zero)
                .unwrap_err()
                .to_string()
                .contains("duration is zero")
        );

        let regressing = recording_with_fragments(&[
            (10, vec![sample(0, true, 1)]),
            (9, vec![sample(1, true, 1)]),
        ]);
        assert!(
            Media::from_bytes(regressing)
                .unwrap_err()
                .to_string()
                .contains("precedes the first")
        );

        let overlap = recording_with_fragments(&[
            (0, vec![sample(0, true, 3_000)]),
            (2_999, vec![sample(1, true, 3_000)]),
        ]);
        assert!(
            Media::from_bytes(overlap)
                .unwrap_err()
                .to_string()
                .contains("overlaps preceding end")
        );

        let gap = recording_with_fragments(&[
            (0, vec![sample(0, true, 3_000)]),
            (3_001, vec![sample(1, true, 3_000)]),
        ]);
        let gap = Media::from_bytes(gap).unwrap();
        assert_eq!(gap.frames[1].decode_time, 3_001);
        assert_eq!(gap.total_ticks, 6_001);
    }

    #[test]
    fn timeline_accepts_the_exact_replay_limit_and_rejects_one_over() {
        let exact = recording_with_fragments(&[
            (0, vec![sample(0, true, 1)]),
            (MAX_REPLAY_TICKS - 1, vec![sample(1, true, 1)]),
        ]);
        assert_eq!(
            Media::from_bytes(exact).unwrap().total_ticks,
            MAX_REPLAY_TICKS
        );

        let over = recording_with_fragments(&[
            (0, vec![sample(0, true, 1)]),
            (MAX_REPLAY_TICKS, vec![sample(1, true, 1)]),
        ]);
        assert!(
            Media::from_bytes(over)
                .unwrap_err()
                .to_string()
                .contains("exceeds the replay clock range")
        );
    }

    #[test]
    fn damaged_partial_and_trailing_recordings_are_rejected() {
        let bytes = synth::recording(synth::Plan {
            gops: 2,
            frames_per_gop: 3,
            idr_len: 32,
            trail_len: 16,
            ..synth::Plan::default()
        })
        .unwrap();
        let report = scan(
            Cursor::new(&bytes[..]),
            ScanOptions {
                verify_crc: true,
                collect_samples: true,
            },
        )
        .unwrap();

        let mut damaged = bytes.to_vec();
        damaged[report.fragments[0].payload_offset as usize + 5] ^= 1;
        assert!(Media::from_bytes(Bytes::from(damaged)).is_err());

        let mut partial = bytes.to_vec();
        partial.truncate(partial.len() - 1);
        assert!(Media::from_bytes(Bytes::from(partial)).is_err());

        let mut trailing = bytes.to_vec();
        trailing.extend_from_slice(b"trailing");
        assert!(Media::from_bytes(Bytes::from(trailing)).is_err());
    }

    #[test]
    fn every_access_unit_must_be_nonempty_and_completely_framed() {
        for bad_index in 0..3 {
            let mut samples = vec![
                sample(0, true, 3_000),
                sample(1, false, 3_000),
                sample(2, false, 3_000),
            ];
            samples[bad_index].data = Bytes::from_static(&[0, 0, 0, 10, 0x26, 0x01]);
            let error = Media::from_bytes(recording_with_fragments(&[(0, samples)]))
                .unwrap_err()
                .to_string();
            assert!(
                error.contains("invalid access unit"),
                "index {bad_index}: {error}"
            );
        }

        let empty = recording_with_fragments(&[(
            0,
            vec![
                sample(0, true, 3_000),
                Sample {
                    data: Bytes::new(),
                    duration: 3_000,
                    is_sync: false,
                },
            ],
        )]);
        assert!(
            Media::from_bytes(empty)
                .unwrap_err()
                .to_string()
                .contains("empty access unit")
        );
    }

    #[test]
    fn recording_must_start_with_a_truthful_sync_sample() {
        let nonsync = recording_with_fragments(&[(0, vec![sample(0, false, 3_000)])]);
        assert!(
            Media::from_bytes(nonsync)
                .unwrap_err()
                .to_string()
                .contains("does not begin with a sync sample")
        );

        let mut mislabeled = sample(0, true, 3_000);
        mislabeled.is_sync = false;
        assert!(
            Media::from_bytes(recording_with_fragments(&[(0, vec![mislabeled])]))
                .unwrap_err()
                .to_string()
                .contains("sync flag disagrees")
        );
    }

    #[test]
    fn hvcc_requires_supported_version_length_and_parameter_sets() {
        let valid = hvcc(&required_hvcc_arrays());
        assert_eq!(parse_hvcc(&valid).unwrap().len(), 3);

        let mut wrong_version = valid.clone();
        wrong_version[0] = 0;
        assert!(parse_hvcc(&wrong_version).is_err());

        for length_size_minus_one in 0..3 {
            let mut wrong_length = valid.clone();
            wrong_length[21] = (wrong_length[21] & !3) | length_size_minus_one;
            assert!(parse_hvcc(&wrong_length).is_err());
        }

        for missing in 32..=34 {
            let arrays = required_hvcc_arrays()
                .into_iter()
                .filter(|(kind, _)| *kind != missing)
                .collect::<Vec<_>>();
            assert!(parse_hvcc(&hvcc(&arrays)).is_err(), "missing {missing}");
        }
    }

    #[test]
    fn hvcc_validates_array_contents_and_ignores_well_formed_unrelated_arrays() {
        let mut arrays = required_hvcc_arrays();
        arrays.insert(1, (39, vec![vec![39 << 1, 1, 0xaa]]));
        arrays.push((33, vec![TEST_SPS.to_vec()]));
        let sets = parse_hvcc(&hvcc(&arrays)).unwrap();
        assert_eq!(sets.len(), 4);
        assert_eq!(
            sets.iter().filter(|nal| nal[0] >> 1 & 0x3f == 33).count(),
            2
        );

        let mut mismatch = required_hvcc_arrays();
        mismatch[0] = (32, vec![TEST_SPS.to_vec()]);
        assert!(parse_hvcc(&hvcc(&mismatch)).is_err());

        let mut short = required_hvcc_arrays();
        short[0] = (32, vec![vec![32 << 1]]);
        assert!(parse_hvcc(&hvcc(&short)).is_err());

        let mut reserved = hvcc(&required_hvcc_arrays());
        reserved[23] |= 0x40;
        assert!(parse_hvcc(&reserved).is_err());

        let mut trailing = hvcc(&required_hvcc_arrays());
        trailing.push(0);
        assert!(parse_hvcc(&trailing).is_err());
    }
}
