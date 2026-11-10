//! Seeded property tests over generated recordings: what the encoder writes,
//! the scanner, the memory parser, and the sidecar record read back, and a
//! damaged file is valid exactly up to the damage.

use std::io::Cursor;

use bytes::Bytes;
use iris_core::time::{MediaTicks, WallUs};
use iris_mp4::export::{rebase_fragment, trim_fragment_end};
use iris_mp4::fragment::{FragmentHeader, Sample, encode_fragment};
use iris_mp4::index::IndexRecord;
use iris_mp4::init::{SampleEntry, build_init_segment};
use iris_mp4::reader::{self, FragmentInfo, ScanOptions, ScanReport, parse_fragment};

/// A small deterministic generator (xorshift64*), so a failure reproduces
/// from its seed.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }

    /// A value in `0..n`.
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

/// What the generator asked the encoder to write for one fragment.
struct Written {
    header: FragmentHeader,
    offset: u64,
    len: u64,
    durations: Vec<u32>,
    sizes: Vec<u32>,
    starts_with_sync: bool,
}

/// A recording of one to six fragments with one to forty samples each, and
/// the facts it was built from.
fn recording(rng: &mut Rng) -> (Vec<u8>, Vec<Written>) {
    let entry = SampleEntry::new(
        Bytes::from_static(b"\0\0\0\x10hvc1\0\0\0\0\0\0\0\0"),
        "hvc1.1.6.L150.B0".into(),
        640,
        480,
    );
    let mut file = build_init_segment(&entry).to_vec();
    let mut written = Vec::new();
    for _ in 0..=rng.below(6) {
        let starts_with_sync = rng.below(4) != 0;
        let samples: Vec<Sample> = (0..=rng.below(40))
            .map(|index| {
                // Mostly small samples, with an occasional large one.
                let len = if rng.below(10) == 0 {
                    1 + rng.below(70_000)
                } else {
                    1 + rng.below(900)
                };
                let byte = u8::try_from(rng.below(256)).unwrap();
                Sample {
                    data: Bytes::from(vec![byte; usize::try_from(len).unwrap()]),
                    duration: 1 + u32::try_from(rng.below(9000)).unwrap(),
                    is_sync: index == 0 && starts_with_sync,
                }
            })
            .collect();
        let header = FragmentHeader {
            sequence: u32::try_from(rng.below(1 << 32)).unwrap(),
            decode_time: rng.below(1 << 40),
            // 2000-01-01 through 2099, to the microsecond.
            wall: WallUs::new(
                946_684_800_000_000 + i64::try_from(rng.below(3_155_000_000_000_000)).unwrap(),
            ),
        };
        let encoded = encode_fragment(&header, &samples).unwrap();
        let offset = file.len() as u64;
        for slice in encoded.io_slices(&samples) {
            file.extend_from_slice(&slice);
        }
        written.push(Written {
            header,
            offset,
            len: encoded.total_len(),
            durations: samples.iter().map(|s| s.duration).collect(),
            sizes: samples
                .iter()
                .map(|s| u32::try_from(s.data.len()).unwrap())
                .collect(),
            starts_with_sync,
        });
    }
    (file, written)
}

fn scanned(file: &[u8]) -> ScanReport {
    reader::scan(
        Cursor::new(file),
        ScanOptions {
            verify_crc: true,
            collect_samples: true,
        },
    )
    .unwrap()
}

fn assert_matches(info: &FragmentInfo, written: &Written, seed: u64) {
    assert_eq!(u64::from(info.len), written.len, "seed {seed}");
    assert_eq!(info.sequence, written.header.sequence, "seed {seed}");
    assert_eq!(info.decode_time, written.header.decode_time, "seed {seed}");
    assert_eq!(info.wall, Some(written.header.wall), "seed {seed}");
    assert_eq!(
        info.duration,
        written.durations.iter().sum::<u32>(),
        "seed {seed}"
    );
    assert_eq!(
        usize::from(info.sample_count),
        written.sizes.len(),
        "seed {seed}"
    );
    assert_eq!(
        info.starts_with_sync, written.starts_with_sync,
        "seed {seed}"
    );
    assert_eq!(info.sample_durations, written.durations, "seed {seed}");
    assert_eq!(info.sample_sizes, written.sizes, "seed {seed}");
}

#[test]
fn a_generated_recording_scans_back_to_what_was_encoded() {
    for seed in 1..=300 {
        let (file, written) = recording(&mut Rng(seed));
        let report = scanned(&file);
        assert_eq!(report.stop, None, "seed {seed}");
        assert_eq!(report.valid_len, file.len() as u64, "seed {seed}");
        assert_eq!(report.fragments.len(), written.len(), "seed {seed}");
        for (info, written) in report.fragments.iter().zip(&written) {
            assert_eq!(info.offset, written.offset, "seed {seed}");
            assert_matches(info, written, seed);
        }
    }
}

#[test]
fn the_memory_parser_agrees_with_the_scanner_on_every_generated_fragment() {
    for seed in 1..=300 {
        let (file, written) = recording(&mut Rng(seed));
        for written in &written {
            let start = usize::try_from(written.offset).unwrap();
            let end = start + usize::try_from(written.len).unwrap();
            let info = parse_fragment(&file[start..end])
                .unwrap_or_else(|| panic!("seed {seed}: the fragment did not parse"));
            assert_eq!(info.offset, 0, "seed {seed}");
            assert_matches(&info, written, seed);
        }
    }
}

#[test]
fn a_sidecar_record_built_from_a_scan_round_trips_through_its_codec() {
    for seed in 1..=300 {
        let (file, _) = recording(&mut Rng(seed));
        for info in &scanned(&file).fragments {
            let record = IndexRecord::from_scan(info);
            assert_eq!(IndexRecord::decode(&record.encode()), record, "seed {seed}");
            assert_eq!(record.offset, info.offset, "seed {seed}");
            assert_eq!(
                record.end(),
                info.offset + u64::from(info.len),
                "seed {seed}"
            );
            assert_eq!(
                record.starts_with_sync(),
                info.starts_with_sync,
                "seed {seed}"
            );
        }
    }
}

#[test]
fn a_recording_cut_inside_a_fragment_is_valid_up_to_that_fragment() {
    for seed in 1..=300 {
        let mut rng = Rng(seed);
        let (file, written) = recording(&mut rng);
        let cut_in = usize::try_from(rng.below(written.len() as u64)).unwrap();
        let victim = &written[cut_in];
        let cut = victim.offset + 1 + rng.below(victim.len - 1);
        let report = scanned(&file[..usize::try_from(cut).unwrap()]);
        assert!(report.stop.is_some(), "seed {seed}");
        assert_eq!(report.fragments.len(), cut_in, "seed {seed}");
        assert_eq!(report.valid_len, victim.offset, "seed {seed}");
    }
}

#[test]
fn a_flipped_payload_bit_ends_the_valid_prefix_before_its_fragment() {
    for seed in 1..=300 {
        let mut rng = Rng(seed);
        let (mut file, written) = recording(&mut rng);
        let clean = scanned(&file);
        let hit = usize::try_from(rng.below(written.len() as u64)).unwrap();
        let info = &clean.fragments[hit];
        let payload: u64 = info.sample_sizes.iter().map(|size| u64::from(*size)).sum();
        let at = usize::try_from(info.payload_offset + rng.below(payload)).unwrap();
        file[at] ^= 1 << rng.below(8);
        let report = scanned(&file);
        assert!(report.stop.is_some(), "seed {seed}");
        assert_eq!(report.fragments.len(), hit, "seed {seed}");
        assert_eq!(report.valid_len, written[hit].offset, "seed {seed}");
    }
}

#[test]
fn a_flipped_bit_anywhere_in_a_fragment_ends_the_valid_prefix_before_it() {
    // Headers, payload, and trailer alike: seed 1214 flips a reserved
    // trailer byte, which lies outside the CRC.
    for seed in 1..=2000 {
        let mut rng = Rng(seed);
        let (mut file, written) = recording(&mut rng);
        let hit = usize::try_from(rng.below(written.len() as u64)).unwrap();
        let victim = &written[hit];
        let at = usize::try_from(victim.offset + rng.below(victim.len)).unwrap();
        file[at] ^= 1 << rng.below(8);
        let report = scanned(&file);
        assert_eq!(report.fragments.len(), hit, "seed {seed}");
        assert_eq!(report.valid_len, victim.offset, "seed {seed}");
    }
}

/// The bytes of generated fragment `written` in `file`.
fn fragment_bytes(file: &[u8], written: &Written) -> Vec<u8> {
    let start = usize::try_from(written.offset).unwrap();
    file[start..start + usize::try_from(written.len).unwrap()].to_vec()
}

/// The payload of a fragment: its bytes from `payload_offset` for the sum of
/// the sample sizes.
fn payload<'a>(bytes: &'a [u8], info: &FragmentInfo) -> &'a [u8] {
    let start = usize::try_from(info.payload_offset).unwrap();
    let len: usize = info.sample_sizes.iter().map(|size| *size as usize).sum();
    &bytes[start..start + len]
}

/// A one-fragment file around `fragment` scans clean, CRC included.
fn assert_scans_clean(fragment: &[u8], seed: u64) {
    let entry = SampleEntry::new(
        Bytes::from_static(b"\0\0\0\x10hvc1\0\0\0\0\0\0\0\0"),
        "hvc1.1.6.L150.B0".into(),
        640,
        480,
    );
    let mut file = build_init_segment(&entry).to_vec();
    file.extend_from_slice(fragment);
    let report = scanned(&file);
    assert_eq!(report.stop, None, "seed {seed}");
    assert_eq!(report.fragments.len(), 1, "seed {seed}");
}

#[test]
fn rebasing_changes_the_sequence_and_decode_time_and_nothing_else() {
    for seed in 1..=300 {
        let mut rng = Rng(seed);
        let (file, written) = recording(&mut rng);
        for written in &written {
            let original = fragment_bytes(&file, written);
            let before = parse_fragment(&original).unwrap();
            let sequence = u32::try_from(rng.below(1 << 32)).unwrap();
            let decode_time = rng.below(1 << 40);
            let mut rebased = original.clone();
            rebase_fragment(&mut rebased, sequence, decode_time)
                .unwrap_or_else(|error| panic!("seed {seed}: {error}"));
            assert_eq!(rebased.len(), original.len(), "seed {seed}");
            let after = parse_fragment(&rebased).unwrap();
            assert_eq!(after.sequence, sequence, "seed {seed}");
            assert_eq!(after.decode_time, decode_time, "seed {seed}");
            assert_eq!(after.wall, before.wall, "seed {seed}");
            assert_eq!(
                after.sample_durations, before.sample_durations,
                "seed {seed}"
            );
            assert_eq!(after.sample_sizes, before.sample_sizes, "seed {seed}");
            assert_eq!(
                after.starts_with_sync, before.starts_with_sync,
                "seed {seed}"
            );
            assert_eq!(
                payload(&rebased, &after),
                payload(&original, &before),
                "seed {seed}"
            );
            assert_scans_clean(&rebased, seed);
        }
    }
}

#[test]
fn trimming_keeps_the_samples_that_start_before_the_cut_with_their_bytes() {
    for seed in 1..=300 {
        let mut rng = Rng(seed);
        let (file, written) = recording(&mut rng);
        for written in &written {
            let original = fragment_bytes(&file, written);
            let before = parse_fragment(&original).unwrap();
            let total: u64 = before.sample_durations.iter().map(|d| u64::from(*d)).sum();
            // A cut inside the fragment, at its end, or past it.
            let cut = 1 + rng.below(total + 2_000);
            let header = FragmentHeader {
                sequence: u32::try_from(rng.below(1 << 32)).unwrap(),
                decode_time: rng.below(1 << 40),
                wall: written.header.wall,
            };
            let trimmed = trim_fragment_end(
                original.clone(),
                header,
                MediaTicks::new(i64::try_from(cut).unwrap()),
            )
            .unwrap_or_else(|error| panic!("seed {seed}: {error}"));
            let after = parse_fragment(&trimmed).unwrap();

            // Samples that start before the cut stay; the one that crosses
            // it ends at it.
            let mut kept = Vec::new();
            let mut start = 0u64;
            for duration in &before.sample_durations {
                if start >= cut {
                    break;
                }
                let end = (start + u64::from(*duration)).min(cut);
                kept.push(u32::try_from(end - start).unwrap());
                start += u64::from(*duration);
            }
            assert_eq!(
                after.sample_durations, kept,
                "seed {seed}: cut {cut} of {total}"
            );
            assert_eq!(u64::from(after.duration), cut.min(total), "seed {seed}");
            assert_eq!(
                after.sample_sizes,
                before.sample_sizes[..kept.len()],
                "seed {seed}"
            );
            assert_eq!(after.sequence, header.sequence, "seed {seed}");
            assert_eq!(after.decode_time, header.decode_time, "seed {seed}");
            assert_eq!(
                after.starts_with_sync, before.starts_with_sync,
                "seed {seed}"
            );
            let after_payload = payload(&trimmed, &after);
            assert_eq!(
                after_payload,
                &payload(&original, &before)[..after_payload.len()],
                "seed {seed}"
            );
            assert_scans_clean(&trimmed, seed);
        }
    }
}
