//! A recording loaded into memory with its parameter sets and frame table.

use std::path::Path;

use anyhow::{Context, bail};
use bytes::Bytes;
use iris_h265::inspect_access_unit;
use iris_mp4::boxes::BoxHeader;
use iris_mp4::reader::{ScanOptions, scan};

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
                verify_crc: false,
                collect_samples: true,
            },
        )
        .context("scanning")?;
        if report.fragments.is_empty() {
            bail!(
                "no fragments{}",
                report.stop.map_or(String::new(), |s| format!(" ({s:?})"))
            );
        }
        let init_len = usize::try_from(report.init_len).context("init length")?;
        let parameter_sets = parameter_sets(&data[..init_len]).context("parameter sets")?;
        // Stored decode times are continuous within a recording run, so a file
        // that is not the first of its run starts far from zero; playback times
        // are relative to the file's first fragment.
        let base = report.fragments[0].decode_time;
        let mut frames = Vec::new();
        for fragment in &report.fragments {
            let mut offset = usize::try_from(fragment.payload_offset).context("offset")?;
            let mut decode_time = fragment.decode_time.saturating_sub(base);
            for (index, (&size, &duration)) in fragment
                .sample_sizes
                .iter()
                .zip(&fragment.sample_durations)
                .enumerate()
            {
                let len = usize::try_from(size).context("sample size")?;
                let sample = data
                    .get(offset..offset + len)
                    .context("sample past end of file")?;
                let is_sync = if index == 0 {
                    fragment.starts_with_sync
                } else {
                    inspect_access_unit(sample).is_ok_and(|info| info.is_irap)
                };
                frames.push(Frame {
                    offset,
                    len,
                    duration,
                    decode_time,
                    is_sync,
                });
                offset += len;
                decode_time += u64::from(duration);
            }
        }
        let total_ticks = frames
            .last()
            .map_or(0, |f| f.decode_time + u64::from(f.duration));
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
    let count = usize::from(*body.get(22).context("hvcC too short")?);
    let mut pos = 23;
    let mut sets = Vec::new();
    for _ in 0..count {
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
            let nal = body.get(pos..pos + len).context("hvcC nalu truncated")?;
            if nal.len() >= 2 {
                sets.push(nal.to_vec());
            }
            pos += len;
        }
    }
    if sets.is_empty() {
        bail!("hvcC carries no parameter sets");
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
    use super::*;
    use crate::synth::{self, TEST_PPS, TEST_SPS, TEST_VPS};

    #[test]
    fn parameter_sets_come_from_hvcc() {
        let entry = synth::sample_entry(&[TEST_VPS.to_vec(), TEST_SPS.to_vec(), TEST_PPS.to_vec()]);
        let init = iris_mp4::init::build_init_segment(&entry);
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
        });
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
        });
        let media = Media::from_bytes(bytes).unwrap();
        assert_eq!(media.frames.len(), 6);
        assert!(media.frames[0].is_sync && media.frames[3].is_sync);
        assert!(!media.frames[1].is_sync);
        assert_eq!(media.frames[1].decode_time, 3_000);
        assert_eq!(media.total_ticks, 18_000);
        assert_eq!(media.sample(&media.frames[0]).len(), media.frames[0].len);
        assert_eq!(media.parameter_sets.len(), 3);
    }
}
