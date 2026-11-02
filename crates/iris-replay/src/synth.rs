//! Synthetic recordings for tests and benchmarks: real parameter sets, dummy
//! slices with the right NAL headers, and the exact on-disk format irisd writes.

use std::path::Path;

use bytes::Bytes;
use iris_core::time::WallUs;
use iris_mp4::fragment::{FragmentHeader, Sample, encode_fragment};
use iris_mp4::init::{SampleEntry, build_init_segment};

/// A video parameter set for a 1920x1080 Main profile stream.
pub const TEST_VPS: &[u8] = &[
    0x40, 0x01, 0x0c, 0x01, 0xff, 0xff, 0x01, 0x60, 0x00, 0x00, 0x03, 0x00, 0xb0, 0x00, 0x00, 0x03,
    0x00, 0x00, 0x03, 0x00, 0x5a, 0xac, 0x0c, 0x00, 0x00, 0x03, 0x00, 0x04, 0x00, 0x00, 0x03, 0x00,
    0x32, 0xa8,
];
/// The matching sequence parameter set.
pub const TEST_SPS: &[u8] = &[
    0x42, 0x01, 0x01, 0x01, 0x60, 0x00, 0x00, 0x03, 0x00, 0xb0, 0x00, 0x00, 0x03, 0x00, 0x00, 0x03,
    0x00, 0x5a, 0xa0, 0x05, 0x82, 0x01, 0xe1, 0x63, 0x6b, 0x92, 0x45, 0x2f, 0xcd, 0xc1, 0x41, 0x81,
    0x41, 0x00, 0x00, 0x03, 0x00, 0x01, 0x00, 0x00, 0x03, 0x00, 0x0c, 0xa1,
];
/// The matching picture parameter set.
pub const TEST_PPS: &[u8] = &[0x44, 0x01, 0xc0, 0xf2, 0xc6, 0x8d, 0x03, 0xb3, 0x40];

/// Shape of a synthetic recording.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Plan {
    /// Groups of pictures, one fragment each.
    pub gops: usize,
    /// Frames per group; the first is a sync sample.
    pub frames_per_gop: usize,
    /// Frame duration in 90 kHz ticks.
    pub frame_ticks: u32,
    /// Slice bytes of a sync frame.
    pub idr_len: usize,
    /// Slice bytes of every other frame.
    pub trail_len: usize,
    /// Decode time of the first sample in ticks, as a file that is not the
    /// first of its run has.
    pub start_ticks: u64,
}

impl Default for Plan {
    /// Thirty 60-frame groups at 30 fps, about 1.6 Mbit/s.
    fn default() -> Self {
        Self {
            gops: 30,
            frames_per_gop: 60,
            frame_ticks: 3_000,
            idr_len: 60_000,
            trail_len: 6_000,
            start_ticks: 0,
        }
    }
}

/// An `hvc1` sample entry carrying `sets` in its `hvcC`.
#[must_use]
pub fn sample_entry(sets: &[Vec<u8>]) -> SampleEntry {
    let mut hvcc = vec![
        1, 0x01, 0x60, 0, 0, 0, 0x90, 0, 0, 0, 0, 0, 0x5a, 0xf0, 0x00, 0xfc, 0xfd, 0xf8, 0xf8, 0,
        0, 0x0f,
    ];
    hvcc.push(u8::try_from(sets.len()).expect("few parameter sets"));
    for nal in sets {
        hvcc.push(0x80 | ((nal[0] >> 1) & 0x3f));
        hvcc.extend_from_slice(&1u16.to_be_bytes());
        hvcc.extend_from_slice(
            &u16::try_from(nal.len())
                .expect("short parameter set")
                .to_be_bytes(),
        );
        hvcc.extend_from_slice(nal);
    }
    let mut body = Vec::with_capacity(78 + hvcc.len() + 8);
    body.extend_from_slice(&[0; 6]);
    body.extend_from_slice(&1u16.to_be_bytes()); // data_reference_index
    body.extend_from_slice(&[0; 16]); // pre_defined and reserved
    body.extend_from_slice(&1920u16.to_be_bytes());
    body.extend_from_slice(&1080u16.to_be_bytes());
    body.extend_from_slice(&0x0048_0000u32.to_be_bytes());
    body.extend_from_slice(&0x0048_0000u32.to_be_bytes());
    body.extend_from_slice(&[0; 4]);
    body.extend_from_slice(&1u16.to_be_bytes()); // frame_count
    body.extend_from_slice(&[0; 32]); // compressorname
    body.extend_from_slice(&0x0018u16.to_be_bytes()); // depth
    body.extend_from_slice(&(-1i16).to_be_bytes());
    body.extend_from_slice(&boxed(*b"hvcC", &hvcc));
    SampleEntry::new(
        Bytes::from(boxed(*b"hvc1", &body)),
        "hvc1.1.6.L90.90".to_owned(),
        1920,
        1080,
    )
}

fn boxed(kind: [u8; 4], body: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(8 + body.len());
    out.extend_from_slice(
        &u32::try_from(8 + body.len())
            .expect("box fits")
            .to_be_bytes(),
    );
    out.extend_from_slice(&kind);
    out.extend_from_slice(body);
    out
}

/// A dummy slice NAL unit of `len` bytes: type 19 (`IDR_W_RADL`) for sync
/// frames, type 1 (`TRAIL_R`) otherwise, with a deterministic pseudo-random body.
#[must_use]
pub fn slice(index: u64, is_sync: bool, len: usize) -> Vec<u8> {
    let mut nal = Vec::with_capacity(len);
    nal.push(if is_sync { 0x26 } else { 0x02 });
    nal.push(0x01);
    let mut state = index
        .wrapping_mul(6_364_136_223_846_793_005)
        .wrapping_add(1_442_695_040_888_963_407);
    while nal.len() < len {
        state = state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1);
        // Avoid emulation-prevention concerns in tests by keeping bytes above 0x03.
        nal.push(4 + ((state >> 33) % 251) as u8);
    }
    nal
}

/// Builds a recording in memory.
#[must_use]
pub fn recording(plan: Plan) -> Bytes {
    let entry = std::sync::Arc::new(sample_entry(&[
        TEST_VPS.to_vec(),
        TEST_SPS.to_vec(),
        TEST_PPS.to_vec(),
    ]));
    let mut out = build_init_segment(&entry).to_vec();
    let mut decode_time = plan.start_ticks;
    let mut index = 0u64;
    for gop in 0..plan.gops {
        let samples: Vec<Sample> = (0..plan.frames_per_gop)
            .map(|i| {
                let is_sync = i == 0;
                let nal = slice(
                    index,
                    is_sync,
                    if is_sync {
                        plan.idr_len
                    } else {
                        plan.trail_len
                    },
                );
                index += 1;
                let mut data = Vec::with_capacity(4 + nal.len());
                data.extend_from_slice(
                    &u32::try_from(nal.len()).expect("slice fits").to_be_bytes(),
                );
                data.extend_from_slice(&nal);
                Sample {
                    data: Bytes::from(data),
                    duration: plan.frame_ticks,
                    is_sync,
                }
            })
            .collect();
        let header = FragmentHeader {
            sequence: u32::try_from(gop + 1).expect("few fragments"),
            decode_time,
            wall: WallUs::new(
                1_700_000_000_000_000 + i64::try_from(decode_time).expect("fits") * 100 / 9,
            ),
        };
        let encoded = encode_fragment(&header, &samples).expect("valid fragment");
        out.extend_from_slice(&encoded.prefix);
        for sample in &samples {
            out.extend_from_slice(&sample.data);
        }
        out.extend_from_slice(&encoded.trailer);
        decode_time += u64::from(plan.frame_ticks) * plan.frames_per_gop as u64;
    }
    Bytes::from(out)
}

/// Writes a recording to `path`.
pub fn write_recording(path: &Path, plan: Plan) -> std::io::Result<()> {
    std::fs::write(path, recording(plan))
}

#[cfg(test)]
mod tests {
    use super::*;
    use iris_mp4::reader::{ScanOptions, scan};

    #[test]
    fn recording_scans_cleanly_with_crc_verification() {
        let bytes = recording(Plan {
            gops: 2,
            frames_per_gop: 3,
            ..Plan::default()
        });
        let report = scan(
            std::io::Cursor::new(&bytes[..]),
            ScanOptions {
                verify_crc: true,
                collect_samples: true,
            },
        )
        .unwrap();
        assert_eq!(report.stop, None);
        assert_eq!(report.fragments.len(), 2);
        assert_eq!(report.fragments[1].decode_time, 9_000);
        assert_eq!(report.fragments[0].sample_sizes[0], 60_004);
        assert!(report.fragments[0].starts_with_sync);
    }
}
