//! Synthetic recordings for tests and benchmarks: real parameter sets, dummy
//! slices with the right NAL headers, and the exact on-disk format irisd writes.

use std::collections::TryReserveError;
use std::fmt;
use std::io;
use std::path::Path;

use bytes::Bytes;
use iris_core::time::WallUs;
use iris_mp4::fragment::{FragmentError, FragmentHeader, Sample, encode_fragment};
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

/// Largest number of fragments accepted by [`recording`].
pub const MAX_SYNTH_GOPS: usize = 100_000;
/// Largest number of frames accepted by [`recording`].
pub const MAX_SYNTH_FRAMES: usize = 1_000_000;
/// Largest number of frames accepted in one synthetic fragment.
pub const MAX_SYNTH_FRAMES_PER_GOP: usize = u16::MAX as usize;
/// Largest combined sample payload accepted by [`recording`].
pub const MAX_SYNTH_PAYLOAD_BYTES: usize = 1 << 30;

/// Error while building or writing synthetic replay media.
#[derive(Debug)]
#[non_exhaustive]
pub enum SynthError {
    /// A parameter-set collection cannot form a usable `hvcC`.
    InvalidParameterSets(&'static str),
    /// A NAL-unit length is outside the supported range.
    InvalidNalLength {
        /// Rejected byte length.
        len: usize,
        /// Largest accepted byte length for this NAL-unit use.
        maximum: usize,
    },
    /// A plan field or combination is outside the supported bounds.
    InvalidPlan(&'static str),
    /// Checked plan arithmetic overflowed.
    ArithmeticOverflow(&'static str),
    /// The planned sample payload exceeds the synthetic recording ceiling.
    PayloadTooLarge {
        /// Planned payload bytes.
        bytes: u64,
    },
    /// Memory reservation failed before mutation.
    Allocation(TryReserveError),
    /// Fragment encoding rejected the validated samples.
    Fragment(FragmentError),
    /// Writing the completed recording failed.
    Io(io::Error),
}

impl fmt::Display for SynthError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidParameterSets(reason) => {
                write!(formatter, "invalid parameter sets: {reason}")
            }
            Self::InvalidNalLength { len, maximum } => {
                write!(formatter, "NAL-unit length {len} is outside 2..={maximum}")
            }
            Self::InvalidPlan(reason) => write!(formatter, "invalid synthetic plan: {reason}"),
            Self::ArithmeticOverflow(field) => {
                write!(formatter, "synthetic plan overflows {field}")
            }
            Self::PayloadTooLarge { bytes } => write!(
                formatter,
                "synthetic payload has {bytes} bytes; at most {MAX_SYNTH_PAYLOAD_BYTES} allowed"
            ),
            Self::Allocation(error) => write!(formatter, "allocating synthetic recording: {error}"),
            Self::Fragment(error) => write!(formatter, "encoding synthetic fragment: {error}"),
            Self::Io(error) => write!(formatter, "writing synthetic recording: {error}"),
        }
    }
}

impl std::error::Error for SynthError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Allocation(error) => Some(error),
            Self::Fragment(error) => Some(error),
            Self::Io(error) => Some(error),
            _ => None,
        }
    }
}

impl From<TryReserveError> for SynthError {
    fn from(error: TryReserveError) -> Self {
        Self::Allocation(error)
    }
}

impl From<FragmentError> for SynthError {
    fn from(error: FragmentError) -> Self {
        Self::Fragment(error)
    }
}

impl From<io::Error> for SynthError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

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
///
/// # Errors
///
/// Returns an error unless the bounded collection contains at least one VPS,
/// SPS, and PPS, with every NAL unit representable by `hvcC`.
pub fn sample_entry(sets: &[Vec<u8>]) -> Result<SampleEntry, SynthError> {
    if sets.is_empty() || sets.len() > usize::from(u8::MAX) {
        return Err(SynthError::InvalidParameterSets(
            "the collection must contain 1..=255 NAL units",
        ));
    }
    for nal in sets {
        validate_nal_len(nal.len(), usize::from(u16::MAX))?;
    }
    for required in 32..=34 {
        if !sets.iter().any(|nal| ((nal[0] >> 1) & 0x3f) == required) {
            return Err(SynthError::InvalidParameterSets(
                "VPS, SPS, and PPS are all required",
            ));
        }
    }
    let hvcc_len = sets.iter().try_fold(23usize, |len, nal| {
        len.checked_add(5)
            .and_then(|len| len.checked_add(nal.len()))
            .ok_or(SynthError::ArithmeticOverflow("hvcC length"))
    })?;
    let mut hvcc = Vec::new();
    hvcc.try_reserve_exact(hvcc_len)?;
    hvcc.extend_from_slice(&[
        1, 0x01, 0x60, 0, 0, 0, 0x90, 0, 0, 0, 0, 0, 0x5a, 0xf0, 0x00, 0xfc, 0xfd, 0xf8, 0xf8, 0,
        0, 0x0f,
    ]);
    hvcc.push(sets.len() as u8);
    for nal in sets {
        hvcc.push(0x80 | ((nal[0] >> 1) & 0x3f));
        hvcc.extend_from_slice(&1u16.to_be_bytes());
        hvcc.extend_from_slice(&(nal.len() as u16).to_be_bytes());
        hvcc.extend_from_slice(nal);
    }
    let body_len = 86usize
        .checked_add(hvcc.len())
        .ok_or(SynthError::ArithmeticOverflow("sample-entry length"))?;
    let mut body = Vec::new();
    body.try_reserve_exact(body_len)?;
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
    body.extend_from_slice(&boxed(*b"hvcC", &hvcc)?);
    Ok(SampleEntry::new(
        Bytes::from(boxed(*b"hvc1", &body)?),
        "hvc1.1.6.L90.90".to_owned(),
        1920,
        1080,
    ))
}

fn boxed(kind: [u8; 4], body: &[u8]) -> Result<Vec<u8>, SynthError> {
    let len = 8usize
        .checked_add(body.len())
        .ok_or(SynthError::ArithmeticOverflow("box length"))?;
    let box_len = u32::try_from(len).map_err(|_| SynthError::ArithmeticOverflow("box length"))?;
    let mut out = Vec::new();
    out.try_reserve_exact(len)?;
    out.extend_from_slice(&box_len.to_be_bytes());
    out.extend_from_slice(&kind);
    out.extend_from_slice(body);
    Ok(out)
}

/// A dummy slice NAL unit of `len` bytes: type 19 (`IDR_W_RADL`) for sync
/// frames, type 1 (`TRAIL_R`) otherwise, with a deterministic pseudo-random body.
///
/// # Errors
///
/// Returns an error when `len` cannot hold an H.265 NAL-unit header, exceeds
/// the synthetic payload ceiling, or memory reservation fails.
pub fn slice(index: u64, is_sync: bool, len: usize) -> Result<Vec<u8>, SynthError> {
    validate_nal_len(len, MAX_SYNTH_PAYLOAD_BYTES)?;
    let mut nal = Vec::new();
    nal.try_reserve_exact(len)?;
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
    Ok(nal)
}

/// Builds a recording in memory.
///
/// # Errors
///
/// Returns an error when `plan` has zero or unrepresentable fields, exceeds
/// the documented cardinality or payload ceilings, allocation fails, or a
/// validated fragment cannot be encoded.
pub fn recording(plan: Plan) -> Result<Bytes, SynthError> {
    let bounds = validate_plan(plan)?;
    let entry = std::sync::Arc::new(sample_entry(&[
        TEST_VPS.to_vec(),
        TEST_SPS.to_vec(),
        TEST_PPS.to_vec(),
    ])?);
    let mut out = build_init_segment(&entry).to_vec();
    let mut decode_time = plan.start_ticks;
    let mut index = 0u64;
    for gop in 0..plan.gops {
        let mut samples = Vec::new();
        samples.try_reserve_exact(plan.frames_per_gop)?;
        for frame in 0..plan.frames_per_gop {
            let is_sync = frame == 0;
            let nal = slice(
                index,
                is_sync,
                if is_sync {
                    plan.idr_len
                } else {
                    plan.trail_len
                },
            )?;
            index = index
                .checked_add(1)
                .ok_or(SynthError::ArithmeticOverflow("frame index"))?;
            let data_len = 4usize
                .checked_add(nal.len())
                .ok_or(SynthError::ArithmeticOverflow("sample length"))?;
            let mut data = Vec::new();
            data.try_reserve_exact(data_len)?;
            data.extend_from_slice(&(nal.len() as u32).to_be_bytes());
            data.extend_from_slice(&nal);
            samples.push(Sample {
                data: Bytes::from(data),
                duration: plan.frame_ticks,
                is_sync,
            });
        }
        let header = FragmentHeader {
            sequence: (gop + 1) as u32,
            decode_time,
            wall: wall_for_decode_time(decode_time)?,
        };
        let encoded = encode_fragment(&header, &samples)?;
        let append_len = encoded
            .prefix
            .len()
            .checked_add(bounds.payload_per_gop)
            .and_then(|len| len.checked_add(encoded.trailer.len()))
            .ok_or(SynthError::ArithmeticOverflow("recording length"))?;
        out.try_reserve(append_len)?;
        out.extend_from_slice(&encoded.prefix);
        for sample in &samples {
            out.extend_from_slice(&sample.data);
        }
        out.extend_from_slice(&encoded.trailer);
        decode_time = decode_time
            .checked_add(bounds.ticks_per_gop)
            .ok_or(SynthError::ArithmeticOverflow("decode time"))?;
    }
    Ok(Bytes::from(out))
}

/// Writes a recording to `path`.
///
/// # Errors
///
/// Returns errors from [`recording`] or the filesystem write.
pub fn write_recording(path: &Path, plan: Plan) -> Result<(), SynthError> {
    std::fs::write(path, recording(plan)?)?;
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct PlanBounds {
    payload_per_gop: usize,
    ticks_per_gop: u64,
}

fn validate_nal_len(len: usize, maximum: usize) -> Result<(), SynthError> {
    if !(2..=maximum).contains(&len) {
        return Err(SynthError::InvalidNalLength { len, maximum });
    }
    Ok(())
}

fn validate_plan(plan: Plan) -> Result<PlanBounds, SynthError> {
    if !(1..=MAX_SYNTH_GOPS).contains(&plan.gops) {
        return Err(SynthError::InvalidPlan(
            "gops must be within 1..=MAX_SYNTH_GOPS",
        ));
    }
    if !(1..=MAX_SYNTH_FRAMES_PER_GOP).contains(&plan.frames_per_gop) {
        return Err(SynthError::InvalidPlan(
            "frames_per_gop must be within 1..=MAX_SYNTH_FRAMES_PER_GOP",
        ));
    }
    if plan.frame_ticks == 0 {
        return Err(SynthError::InvalidPlan(
            "frame_ticks must be greater than zero",
        ));
    }
    validate_nal_len(plan.idr_len, MAX_SYNTH_PAYLOAD_BYTES)?;
    validate_nal_len(plan.trail_len, MAX_SYNTH_PAYLOAD_BYTES)?;

    let total_frames = plan
        .gops
        .checked_mul(plan.frames_per_gop)
        .ok_or(SynthError::ArithmeticOverflow("frame count"))?;
    if total_frames > MAX_SYNTH_FRAMES {
        return Err(SynthError::InvalidPlan(
            "total frames exceed MAX_SYNTH_FRAMES",
        ));
    }
    let trailing_frames = plan.frames_per_gop - 1;
    let trailing_payload = plan
        .trail_len
        .checked_add(4)
        .and_then(|len| len.checked_mul(trailing_frames))
        .ok_or(SynthError::ArithmeticOverflow("group payload"))?;
    let payload_per_gop = plan
        .idr_len
        .checked_add(4)
        .and_then(|len| len.checked_add(trailing_payload))
        .ok_or(SynthError::ArithmeticOverflow("group payload"))?;
    let total_payload = payload_per_gop
        .checked_mul(plan.gops)
        .ok_or(SynthError::ArithmeticOverflow("recording payload"))?;
    if total_payload > MAX_SYNTH_PAYLOAD_BYTES {
        return Err(SynthError::PayloadTooLarge {
            bytes: total_payload as u64,
        });
    }

    let ticks_per_gop = u64::from(plan.frame_ticks)
        .checked_mul(plan.frames_per_gop as u64)
        .ok_or(SynthError::ArithmeticOverflow("group duration"))?;
    let total_ticks = ticks_per_gop
        .checked_mul(plan.gops as u64)
        .ok_or(SynthError::ArithmeticOverflow("recording duration"))?;
    let end_ticks = plan
        .start_ticks
        .checked_add(total_ticks)
        .ok_or(SynthError::ArithmeticOverflow("decode time"))?;
    if end_ticks > crate::media::MAX_REPLAY_TICKS {
        return Err(SynthError::InvalidPlan(
            "decode time exceeds the replay clock range",
        ));
    }
    wall_for_decode_time(end_ticks)?;
    Ok(PlanBounds {
        payload_per_gop,
        ticks_per_gop,
    })
}

fn wall_for_decode_time(decode_time: u64) -> Result<WallUs, SynthError> {
    const START_WALL_US: i128 = 1_700_000_000_000_000;
    let offset = i128::from(decode_time)
        .checked_mul(100)
        .ok_or(SynthError::ArithmeticOverflow("wall time"))?
        / 9;
    let wall = START_WALL_US
        .checked_add(offset)
        .and_then(|value| i64::try_from(value).ok())
        .ok_or(SynthError::ArithmeticOverflow("wall time"))?;
    Ok(WallUs::new(wall))
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
        })
        .unwrap();
        let report = scan(
            io::Cursor::new(&bytes[..]),
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

    #[test]
    fn public_builders_reject_short_nals_and_invalid_plans() {
        assert!(matches!(
            slice(0, true, 0),
            Err(SynthError::InvalidNalLength { .. })
        ));
        assert!(matches!(
            slice(0, true, 1),
            Err(SynthError::InvalidNalLength { .. })
        ));
        assert_eq!(slice(0, true, 2).unwrap(), vec![0x26, 0x01]);

        for plan in [
            Plan {
                gops: 0,
                ..Plan::default()
            },
            Plan {
                frames_per_gop: 0,
                ..Plan::default()
            },
            Plan {
                frame_ticks: 0,
                ..Plan::default()
            },
            Plan {
                idr_len: 1,
                ..Plan::default()
            },
        ] {
            assert!(recording(plan).is_err(), "{plan:?}");
        }
    }

    #[test]
    fn plan_limits_are_checked_without_allocating_the_recording() {
        validate_plan(Plan {
            gops: MAX_SYNTH_GOPS,
            frames_per_gop: MAX_SYNTH_FRAMES / MAX_SYNTH_GOPS,
            frame_ticks: 1,
            idr_len: 2,
            trail_len: 2,
            start_ticks: 0,
        })
        .unwrap();
        assert!(matches!(
            validate_plan(Plan {
                gops: MAX_SYNTH_GOPS + 1,
                ..Plan::default()
            }),
            Err(SynthError::InvalidPlan(_))
        ));
        validate_plan(Plan {
            gops: 1,
            frames_per_gop: MAX_SYNTH_FRAMES_PER_GOP,
            frame_ticks: 1,
            idr_len: 2,
            trail_len: 2,
            start_ticks: 0,
        })
        .unwrap();
        assert!(matches!(
            validate_plan(Plan {
                gops: 1,
                frames_per_gop: MAX_SYNTH_FRAMES_PER_GOP + 1,
                frame_ticks: 1,
                idr_len: 2,
                trail_len: 2,
                start_ticks: 0,
            }),
            Err(SynthError::InvalidPlan(_))
        ));
        validate_plan(Plan {
            gops: 100,
            frames_per_gop: 10_000,
            frame_ticks: 1,
            idr_len: 2,
            trail_len: 2,
            start_ticks: 0,
        })
        .unwrap();
        assert!(matches!(
            validate_plan(Plan {
                gops: 101,
                frames_per_gop: 9_901,
                frame_ticks: 1,
                idr_len: 2,
                trail_len: 2,
                start_ticks: 0,
            }),
            Err(SynthError::InvalidPlan(_))
        ));
        validate_plan(Plan {
            gops: 1,
            frames_per_gop: 1,
            frame_ticks: 1,
            idr_len: MAX_SYNTH_PAYLOAD_BYTES - 4,
            trail_len: 2,
            start_ticks: 0,
        })
        .unwrap();
        assert!(matches!(
            validate_plan(Plan {
                gops: 1,
                frames_per_gop: 1,
                idr_len: MAX_SYNTH_PAYLOAD_BYTES - 3,
                trail_len: 2,
                ..Plan::default()
            }),
            Err(SynthError::PayloadTooLarge { .. })
        ));
        validate_plan(Plan {
            gops: 1,
            frames_per_gop: 1,
            frame_ticks: 1,
            idr_len: 2,
            trail_len: 2,
            start_ticks: crate::media::MAX_REPLAY_TICKS - 1,
        })
        .unwrap();
        assert!(matches!(
            validate_plan(Plan {
                gops: 1,
                frames_per_gop: 1,
                frame_ticks: 1,
                idr_len: 2,
                trail_len: 2,
                start_ticks: crate::media::MAX_REPLAY_TICKS,
            }),
            Err(SynthError::InvalidPlan(_))
        ));
        assert!(matches!(
            slice(0, true, MAX_SYNTH_PAYLOAD_BYTES + 1),
            Err(SynthError::InvalidNalLength { .. })
        ));
    }

    #[test]
    fn sample_entry_checks_collection_and_nal_length_boundaries() {
        let mut exact_count = vec![TEST_VPS.to_vec(), TEST_SPS.to_vec(), TEST_PPS.to_vec()];
        exact_count.resize(usize::from(u8::MAX), TEST_PPS.to_vec());
        sample_entry(&exact_count).unwrap();
        exact_count.push(TEST_PPS.to_vec());
        assert!(matches!(
            sample_entry(&exact_count),
            Err(SynthError::InvalidParameterSets(_))
        ));

        let mut maximum_vps = vec![0; usize::from(u16::MAX)];
        maximum_vps[..2].copy_from_slice(&[0x40, 0x01]);
        sample_entry(&[maximum_vps.clone(), TEST_SPS.to_vec(), TEST_PPS.to_vec()]).unwrap();
        maximum_vps.push(0);
        assert!(matches!(
            sample_entry(&[maximum_vps, TEST_SPS.to_vec(), TEST_PPS.to_vec()]),
            Err(SynthError::InvalidNalLength { .. })
        ));

        assert!(matches!(
            sample_entry(&[TEST_VPS.to_vec(), TEST_SPS.to_vec()]),
            Err(SynthError::InvalidParameterSets(_))
        ));
    }
}
