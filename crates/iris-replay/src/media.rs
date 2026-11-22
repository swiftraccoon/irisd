//! A recording loaded into memory with its parameter sets and frame table.

use std::fmt;
use std::io::{self, Cursor};
use std::path::{Path, PathBuf};
use std::time::Duration;

use bytes::Bytes;
use iris_h265::{FramingError, inspect_access_unit};
use iris_mp4::decoder_config::{self, HvccError};
use iris_mp4::reader::{self, ScanOptions, ScanReport, ScanStop};

/// A count of the recording's 90 kHz clock. Unsigned, as the recording's decode
/// times are.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Ticks(u64);

impl Ticks {
    /// No time.
    pub const ZERO: Self = Self(0);

    /// Wraps a tick count.
    #[must_use]
    pub const fn new(ticks: u64) -> Self {
        Self(ticks)
    }

    /// The tick count.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }

    /// `self + other`, or `None` when the sum overflows.
    #[must_use]
    pub fn checked_add(self, other: Self) -> Option<Self> {
        self.0.checked_add(other.0).map(Self)
    }

    /// The ticks as a duration, or `None` when the microsecond count overflows,
    /// which it cannot at or below [`MAX_REPLAY_TICKS`].
    #[must_use]
    pub fn to_duration(self) -> Option<Duration> {
        self.0
            .checked_mul(100)
            .map(|micros| Duration::from_micros(micros / 9))
    }
}

/// Largest decode time whose 90 kHz-to-microsecond conversion cannot overflow.
pub const MAX_REPLAY_TICKS: Ticks = Ticks(u64::MAX / 100);

/// One access unit in the file: a length-prefixed sample.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Frame {
    /// Byte offset of the sample in the file.
    pub offset: usize,
    /// Sample length in bytes.
    pub len: usize,
    /// Duration; positive in an admitted recording.
    pub duration: Ticks,
    /// Decode time from the start of the file.
    pub decode_time: Ticks,
    /// Whether the sample is a random access point.
    pub is_sync: bool,
}

/// Why a recording cannot be replayed.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum MediaError {
    /// The file could not be read.
    #[error("reading {}", path.display())]
    Read {
        /// The file.
        path: PathBuf,
        /// The read failure.
        #[source]
        source: io::Error,
    },
    /// The scanner could not read the recording's bytes.
    #[error("scanning the recording")]
    Scan(#[source] io::Error),
    /// The scan stopped before the end of the file: damage, a CRC mismatch, or a
    /// truncated fragment.
    #[error("the recording is not CRC-clean through exact EOF: {0}")]
    Stopped(ScanStop),
    /// The scan accepted fewer bytes than the file holds.
    #[error("the recording scan accepted {accepted} of {len} bytes")]
    Unscanned {
        /// Bytes the scan accepted.
        accepted: u64,
        /// Bytes in the file.
        len: u64,
    },
    /// The file holds no fragment.
    #[error("the recording has no fragments")]
    NoFragments,
    /// A fragment carries no sample, so nothing bounds its time.
    #[error("fragment {index} carries no samples")]
    EmptyFragment {
        /// The fragment's position in the file, from zero.
        index: usize,
    },
    /// A fragment starts before the file's first fragment.
    #[error("fragment {index} starts before the first fragment")]
    Regression {
        /// The fragment's position in the file, from zero.
        index: usize,
    },
    /// A fragment starts before the preceding one ends.
    #[error("fragment {index} starts at tick {start}, before the preceding end {previous_end}")]
    Overlap {
        /// The fragment's position in the file, from zero.
        index: usize,
        /// Its start, relative to the first fragment.
        start: u64,
        /// The preceding fragment's end.
        previous_end: u64,
    },
    /// A sample has a zero duration.
    #[error("sample duration is zero")]
    ZeroDuration,
    /// The timeline runs past [`MAX_REPLAY_TICKS`].
    #[error("the media timeline exceeds the replay clock range")]
    ClockRange,
    /// A sample's bytes lie outside the file.
    #[error("the sample at byte {offset} lies past the end of the file")]
    SampleBounds {
        /// The sample's offset as the fragment states it.
        offset: u64,
    },
    /// A sample is not a completely framed access unit.
    #[error("invalid access unit at byte {offset}")]
    AccessUnit {
        /// The sample's offset.
        offset: usize,
        /// The framing failure.
        #[source]
        source: FramingError,
    },
    /// A sample holds no NAL unit.
    #[error("empty access unit at byte {offset}")]
    EmptyAccessUnit {
        /// The sample's offset.
        offset: usize,
    },
    /// A fragment's sync flag disagrees with its first access unit.
    #[error("fragment sync flag disagrees with its first access unit")]
    SyncMismatch,
    /// The first sample is not a sync sample, so a client could not start.
    #[error("the recording does not begin with a sync sample")]
    NoInitialSync,
    /// The init segment yields no usable parameter sets.
    #[error("reading the parameter sets")]
    ParameterSets(#[source] HvccError),
}

/// A recording ready to replay.
///
/// [`Media::from_bytes`] admits a file only when it is CRC-clean through exact
/// EOF, every fragment carries samples, every sample is a nonempty, completely
/// framed access unit inside the file, the first sample is a sync sample, and
/// the fragments form a nonoverlapping timeline within [`MAX_REPLAY_TICKS`].
/// The frame table indexes those samples in decode order. Its `Debug` output
/// summarises the recording instead of printing its bytes.
pub struct Media {
    data: Bytes,
    parameter_sets: Vec<Vec<u8>>,
    frames: Vec<Frame>,
    total_ticks: Ticks,
}

impl fmt::Debug for Media {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Media")
            .field("bytes", &self.data.len())
            .field("parameter_sets", &self.parameter_sets.len())
            .field("frames", &self.frames.len())
            .field("total_ticks", &self.total_ticks)
            .finish_non_exhaustive()
    }
}

impl TryFrom<Bytes> for Media {
    type Error = MediaError;

    fn try_from(data: Bytes) -> Result<Self, Self::Error> {
        Self::from_bytes(data)
    }
}

impl Media {
    /// Reads and indexes a recording.
    ///
    /// # Errors
    ///
    /// [`MediaError::Read`] when the file cannot be read, and every error of
    /// [`Media::from_bytes`].
    pub fn load(path: &Path) -> Result<Self, MediaError> {
        let data = std::fs::read(path).map_err(|source| MediaError::Read {
            path: path.to_owned(),
            source,
        })?;
        Self::from_bytes(Bytes::from(data))
    }

    /// Indexes a recording held in memory; [`TryFrom<Bytes>`] does the same.
    ///
    /// # Errors
    ///
    /// A [`MediaError`] naming the first admission rule the bytes break: a scan
    /// failure or stop (damage or a CRC mismatch), bytes past the accepted
    /// length, no fragments, a fragment without samples, a timeline that
    /// regresses, overlaps, or exceeds [`MAX_REPLAY_TICKS`], a zero sample
    /// duration, a sample outside the file, an empty or malformed access unit, a
    /// sync flag that disagrees with its access unit, a first sample that is not
    /// a sync sample, or an init segment that
    /// [`decoder_config::parse_parameter_sets`] refuses. Among others, that
    /// reader refuses an `hvcC` without version 1, four-byte lengths, and a
    /// VPS, SPS, and PPS, and one that holds more than 16 VPSs, 16 SPSs, or
    /// 64 PPSs ([`HvccError::TooManyParameterSets`]).
    pub fn from_bytes(data: Bytes) -> Result<Self, MediaError> {
        let report = reader::scan(
            Cursor::new(&data[..]),
            ScanOptions {
                verify_crc: true,
                collect_samples: true,
            },
        )
        .map_err(MediaError::Scan)?;
        Self::index(data, report)
    }

    /// Indexes `data` from its scan, applying the admission rules of
    /// [`Media::from_bytes`].
    fn index(data: Bytes, report: ScanReport) -> Result<Self, MediaError> {
        if let Some(stop) = report.stop {
            return Err(MediaError::Stopped(stop));
        }
        if report.valid_len != data.len() as u64 {
            return Err(MediaError::Unscanned {
                accepted: report.valid_len,
                len: data.len() as u64,
            });
        }
        let Some(first) = report.fragments.first() else {
            return Err(MediaError::NoFragments);
        };
        // The scan read all of `data`, so the init segment it reports lies
        // inside it. A length past the end, which no scan reports, is refused
        // as bytes the scan did not accept.
        let init = usize::try_from(report.init_len)
            .ok()
            .and_then(|init_len| data.get(..init_len))
            .ok_or(MediaError::Unscanned {
                accepted: report.valid_len,
                len: data.len() as u64,
            })?;
        let parameter_sets =
            decoder_config::parse_parameter_sets(init).map_err(MediaError::ParameterSets)?;
        // Stored decode times are continuous within a recording run, so a file
        // that is not the first of its run starts far from zero; playback times
        // are relative to the file's first fragment.
        let base = first.decode_time;
        let mut frames = Vec::new();
        let mut previous_end = Ticks::ZERO;
        for (index, fragment) in report.fragments.iter().enumerate() {
            if fragment.sample_sizes.is_empty() {
                return Err(MediaError::EmptyFragment { index });
            }
            let mut decode_time = Ticks::new(
                fragment
                    .decode_time
                    .checked_sub(base)
                    .ok_or(MediaError::Regression { index })?,
            );
            if decode_time < previous_end {
                return Err(MediaError::Overlap {
                    index,
                    start: decode_time.get(),
                    previous_end: previous_end.get(),
                });
            }
            let mut offset = fragment.payload_offset;
            for (sample, (&size, &duration)) in fragment
                .sample_sizes
                .iter()
                .zip(&fragment.sample_durations)
                .enumerate()
            {
                if duration == 0 {
                    return Err(MediaError::ZeroDuration);
                }
                let bounds = MediaError::SampleBounds { offset };
                let start = usize::try_from(offset).map_err(|_| bounds)?;
                let len = usize::try_from(size).map_err(|_| MediaError::SampleBounds { offset })?;
                let bytes = start
                    .checked_add(len)
                    .and_then(|end| data.get(start..end))
                    .ok_or(MediaError::SampleBounds { offset })?;
                let info = inspect_access_unit(bytes).map_err(|source| MediaError::AccessUnit {
                    offset: start,
                    source,
                })?;
                if info.nal_count == 0 {
                    return Err(MediaError::EmptyAccessUnit { offset: start });
                }
                if sample == 0 && fragment.starts_with_sync != info.is_irap {
                    return Err(MediaError::SyncMismatch);
                }
                let duration = Ticks::new(u64::from(duration));
                frames.push(Frame {
                    offset: start,
                    len,
                    duration,
                    decode_time,
                    is_sync: info.is_irap,
                });
                offset += u64::from(size);
                decode_time = decode_time
                    .checked_add(duration)
                    .filter(|end| *end <= MAX_REPLAY_TICKS)
                    .ok_or(MediaError::ClockRange)?;
            }
            previous_end = decode_time;
        }
        if !frames.first().is_some_and(|frame| frame.is_sync) {
            return Err(MediaError::NoInitialSync);
        }
        Ok(Self {
            data,
            parameter_sets,
            frames,
            total_ticks: previous_end,
        })
    }

    /// A recording made of its parts, for tests that need media `from_bytes`
    /// would refuse.
    #[cfg(test)]
    pub(crate) fn for_tests(
        data: Bytes,
        parameter_sets: Vec<Vec<u8>>,
        frames: Vec<Frame>,
        total_ticks: Ticks,
    ) -> Self {
        Self {
            data,
            parameter_sets,
            frames,
            total_ticks,
        }
    }

    /// The frames in decode order.
    #[must_use]
    pub fn frames(&self) -> &[Frame] {
        &self.frames
    }

    /// VPS, SPS, and PPS from the sample entry, without length prefixes.
    #[must_use]
    pub fn parameter_sets(&self) -> &[Vec<u8>] {
        &self.parameter_sets
    }

    /// The duration of one pass through the recording.
    #[must_use]
    pub fn total_ticks(&self) -> Ticks {
        self.total_ticks
    }

    /// The bytes of `frame`, a length-prefixed access unit, or `None` when the
    /// frame does not lie inside this recording; every frame of
    /// [`Media::frames`] does.
    #[must_use]
    pub fn sample(&self, frame: &Frame) -> Option<&[u8]> {
        self.data
            .get(frame.offset..frame.offset.checked_add(frame.len)?)
    }
}

#[cfg(test)]
mod tests {
    use iris_core::time::WallUs;
    use iris_mp4::fragment::{FragmentHeader, Sample, encode_fragment};
    use iris_mp4::init::build_init_segment;
    use iris_mp4::reader::FragmentInfo;

    use super::*;
    use crate::synth::{self, Plan, SliceKind, TEST_PPS, TEST_SPS, TEST_VPS};

    fn sample(index: u64, is_sync: bool, duration: u32) -> Sample {
        let kind = if is_sync {
            SliceKind::Idr
        } else {
            SliceKind::Trail
        };
        let nal = synth::slice(index, kind, 8).unwrap();
        let mut data = Vec::with_capacity(4 + nal.len());
        data.extend_from_slice(&u32::try_from(nal.len()).unwrap().to_be_bytes());
        data.extend_from_slice(&nal);
        Sample {
            data: Bytes::from(data),
            duration,
            is_sync,
        }
    }

    fn recording_with_fragments(fragments: &[(u64, Vec<Sample>)]) -> Bytes {
        let entry = synth::sample_entry(&[TEST_VPS.to_vec(), TEST_SPS.to_vec(), TEST_PPS.to_vec()])
            .unwrap();
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

    fn small_plan() -> Plan {
        Plan {
            gops: 2,
            frames_per_gop: 3,
            ..Plan::default()
        }
    }

    fn scan_of(bytes: &Bytes) -> ScanReport {
        reader::scan(
            Cursor::new(&bytes[..]),
            ScanOptions {
                verify_crc: true,
                collect_samples: true,
            },
        )
        .unwrap()
    }

    #[test]
    fn decode_times_are_relative_to_the_first_fragment() {
        let bytes = synth::recording(Plan {
            start_ticks: Ticks::new(90_000 * 3_600),
            ..small_plan()
        })
        .unwrap();
        let media = Media::from_bytes(bytes).unwrap();
        assert_eq!(media.frames()[0].decode_time, Ticks::ZERO);
        assert_eq!(media.frames()[1].decode_time, Ticks::new(3_000));
        assert_eq!(media.frames()[3].decode_time, Ticks::new(9_000));
        assert_eq!(media.total_ticks(), Ticks::new(18_000));
    }

    #[test]
    fn frames_are_indexed_with_sync_flags_and_samples() {
        let media = Media::from_bytes(synth::recording(small_plan()).unwrap()).unwrap();
        assert_eq!(media.frames().len(), 6);
        assert!(media.frames()[0].is_sync && media.frames()[3].is_sync);
        assert!(!media.frames()[1].is_sync);
        assert_eq!(
            media.sample(&media.frames()[0]).map(<[u8]>::len),
            Some(media.frames()[0].len)
        );
    }

    /// `Media::from_bytes` gives the parameter sets that `synth::recording`
    /// wrote, in their order.
    #[test]
    fn media_holds_the_parameter_sets_the_recording_carries() {
        let media = Media::from_bytes(synth::recording(small_plan()).unwrap()).unwrap();
        assert_eq!(media.parameter_sets(), [TEST_VPS, TEST_SPS, TEST_PPS]);
    }

    /// The configuration version is the first byte of the `hvcC` body. The
    /// fragment checksums leave the init segment out, so the scan accepts
    /// the file.
    #[test]
    fn a_recording_whose_hvcc_is_of_version_0_is_rejected() {
        let mut bytes = synth::recording(small_plan()).unwrap().to_vec();
        let hvcc = bytes
            .windows(4)
            .position(|window| window == b"hvcC")
            .unwrap();
        assert_eq!(bytes[hvcc + 4], 1, "configurationVersion");
        bytes[hvcc + 4] = 0;
        assert!(matches!(
            Media::from_bytes(Bytes::from(bytes)),
            Err(MediaError::ParameterSets(HvccError::Version))
        ));
    }

    #[test]
    fn a_frame_outside_the_recording_has_no_sample() {
        let media = Media::from_bytes(synth::recording(small_plan()).unwrap()).unwrap();
        let past_the_end = Frame {
            offset: usize::MAX - 1,
            len: 8,
            ..media.frames()[0]
        };
        assert_eq!(media.sample(&past_the_end), None);
    }

    #[test]
    fn the_debug_output_summarises_the_recording() {
        let media = Media::from_bytes(synth::recording(small_plan()).unwrap()).unwrap();
        let shown = format!("{media:?}");
        assert!(shown.contains("frames: 6") && shown.len() < 200, "{shown}");
    }

    #[test]
    fn zero_sample_durations_are_rejected() {
        let zero = recording_with_fragments(&[(0, vec![sample(0, true, 0)])]);
        assert!(matches!(
            Media::from_bytes(zero),
            Err(MediaError::ZeroDuration)
        ));
    }

    #[test]
    fn fragments_starting_before_the_first_are_rejected() {
        let regressing = recording_with_fragments(&[
            (10, vec![sample(0, true, 1)]),
            (9, vec![sample(1, true, 1)]),
        ]);
        assert!(matches!(
            Media::from_bytes(regressing),
            Err(MediaError::Regression { index: 1 })
        ));
    }

    #[test]
    fn overlapping_fragments_are_rejected() {
        let overlap = recording_with_fragments(&[
            (0, vec![sample(0, true, 3_000)]),
            (2_999, vec![sample(1, true, 3_000)]),
        ]);
        assert!(matches!(
            Media::from_bytes(overlap),
            Err(MediaError::Overlap {
                index: 1,
                start: 2_999,
                previous_end: 3_000
            })
        ));
    }

    #[test]
    fn forward_timeline_gaps_are_preserved() {
        let gap = recording_with_fragments(&[
            (0, vec![sample(0, true, 3_000)]),
            (3_001, vec![sample(1, true, 3_000)]),
        ]);
        let gap = Media::from_bytes(gap).unwrap();
        assert_eq!(gap.frames()[1].decode_time, Ticks::new(3_001));
        assert_eq!(gap.total_ticks(), Ticks::new(6_001));
    }

    #[test]
    fn a_fragment_without_samples_is_rejected() {
        // The scanner accepts a `trun` of zero samples; such a fragment's start
        // would otherwise set the timeline's end unchecked.
        let bytes = synth::recording(small_plan()).unwrap();
        let mut report = scan_of(&bytes);
        let last = report.fragments.last().unwrap().clone();
        report.fragments.push(FragmentInfo {
            decode_time: u64::MAX,
            sample_count: 0,
            sample_durations: Vec::new(),
            sample_sizes: Vec::new(),
            ..last
        });
        assert!(matches!(
            Media::index(bytes, report),
            Err(MediaError::EmptyFragment { index: 2 })
        ));
    }

    #[test]
    fn the_exact_replay_limit_is_accepted() {
        let exact = recording_with_fragments(&[
            (0, vec![sample(0, true, 1)]),
            (MAX_REPLAY_TICKS.get() - 1, vec![sample(1, true, 1)]),
        ]);
        assert_eq!(
            Media::from_bytes(exact).unwrap().total_ticks(),
            MAX_REPLAY_TICKS
        );
    }

    #[test]
    fn one_tick_past_the_replay_limit_is_rejected() {
        let over = recording_with_fragments(&[
            (0, vec![sample(0, true, 1)]),
            (MAX_REPLAY_TICKS.get(), vec![sample(1, true, 1)]),
        ]);
        assert!(matches!(
            Media::from_bytes(over),
            Err(MediaError::ClockRange)
        ));
    }

    #[test]
    fn damaged_partial_and_trailing_recordings_are_rejected() {
        let bytes = synth::recording(Plan {
            idr_len: 32,
            trail_len: 16,
            ..small_plan()
        })
        .unwrap();
        let report = scan_of(&bytes);

        let mut damaged = bytes.to_vec();
        damaged[usize::try_from(report.fragments[0].payload_offset).unwrap() + 5] ^= 1;
        assert!(Media::from_bytes(Bytes::from(damaged)).is_err(), "damaged");

        let mut partial = bytes.to_vec();
        partial.truncate(partial.len() - 1);
        assert!(Media::from_bytes(Bytes::from(partial)).is_err(), "partial");

        let mut trailing = bytes.to_vec();
        trailing.extend_from_slice(b"trailing");
        assert!(
            Media::from_bytes(Bytes::from(trailing)).is_err(),
            "trailing"
        );
    }

    #[test]
    fn every_access_unit_must_be_completely_framed() {
        for bad_index in 0..3 {
            let mut samples = vec![
                sample(0, true, 3_000),
                sample(1, false, 3_000),
                sample(2, false, 3_000),
            ];
            samples[bad_index].data = Bytes::from_static(&[0, 0, 0, 10, 0x26, 0x01]);
            let error = Media::from_bytes(recording_with_fragments(&[(0, samples)]));
            assert!(
                matches!(error, Err(MediaError::AccessUnit { .. })),
                "index {bad_index}: {error:?}"
            );
        }
    }

    #[test]
    fn empty_access_units_are_rejected() {
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
        assert!(matches!(
            Media::from_bytes(empty),
            Err(MediaError::EmptyAccessUnit { .. })
        ));
    }

    #[test]
    fn a_recording_must_start_with_a_sync_sample() {
        let nonsync = recording_with_fragments(&[(0, vec![sample(0, false, 3_000)])]);
        assert!(matches!(
            Media::from_bytes(nonsync),
            Err(MediaError::NoInitialSync)
        ));
    }

    #[test]
    fn a_sync_flag_must_match_its_access_unit() {
        let mut mislabeled = sample(0, true, 3_000);
        mislabeled.is_sync = false;
        assert!(matches!(
            Media::from_bytes(recording_with_fragments(&[(0, vec![mislabeled])])),
            Err(MediaError::SyncMismatch)
        ));
    }
}
