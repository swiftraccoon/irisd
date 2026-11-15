//! Fuzzes the export rewrites: parsing a fragment held in memory, rebasing it
//! in place, moving it to another track, trimming its end and its start, and,
//! with arbitrary bytes taken as an init segment, splicing edit lists into it
//! and reading its sample entry back. All of them read files the daemon
//! wrote, but a corrupt file must yield an error, never a panic.
//!
//! No mutation of a stored fragment keeps its checksum, so the target also
//! encodes a fragment from the input and cuts and moves that one: the paths
//! past the checksum are then reached whatever the corpus holds.
#![no_main]

use bytes::Bytes;
use iris_core::time::{MediaTicks, WallUs};
use iris_mp4::export::{
    ExportError, TrackEdit, TrimmedStart, rebase_fragment, retrack_fragment, trim_fragment_end,
    trim_fragment_start, with_edit_list, with_edit_lists,
};
use iris_mp4::fragment::{FragmentHeader, Sample, encode_fragment};
use iris_mp4::init::{audio_channel_count, sample_entry_bytes};
use iris_mp4::reader::parse_fragment;
use libfuzzer_sys::fuzz_target;

/// An `edts` box holding a version-1 `elst` with one entry: 8 + 8 + 4 + 4 + 20
/// bytes.
const EDIT_LIST_BYTES: usize = 44;

/// Encodes a fragment from `data` and checks the cut of its start and its
/// move to another track. The first byte chooses whether every sample is a
/// sync sample, how far in the cut is, and how long the samples are.
fn cut_an_encoded_fragment(data: &[u8]) {
    let Some((&shape, rest)) = data.split_first() else {
        return;
    };
    let every_sample_sync = shape & 1 == 0;
    let samples: Vec<Sample> = rest
        .chunks(usize::from(shape >> 4) + 1)
        .take(64)
        .enumerate()
        .map(|(index, chunk)| Sample {
            data: Bytes::copy_from_slice(chunk),
            duration: 1 + u32::from(chunk[0]) * 16,
            is_sync: every_sample_sync || index == 0,
        })
        .collect();
    let header = FragmentHeader {
        sequence: 3,
        decode_time: 1_000,
        wall: WallUs::new(1_000_000),
    };
    let Ok(encoded) = encode_fragment(&header, &samples) else {
        return;
    };
    let mut fragment = Vec::new();
    for slice in encoded.io_slices(&samples) {
        fragment.extend_from_slice(&slice);
    }
    let all_sync = every_sample_sync || samples.len() == 1;
    let total: u64 = samples.iter().map(|s| u64::from(s.duration)).sum();
    let last_start = total - u64::from(samples[samples.len() - 1].duration);
    let skip = total * u64::from((shape >> 1) & 7) / 7;
    match trim_fragment_start(fragment.clone(), MediaTicks::new(skip.cast_signed())) {
        Ok(TrimmedStart {
            fragment: rest,
            omitted_ticks: omitted,
        }) => {
            assert!(all_sync);
            assert!(omitted >= skip && skip <= last_start);
            let info = parse_fragment(&rest).expect("the remainder parses");
            assert!(info.all_sync);
            assert_eq!(info.sequence, header.sequence);
            assert_eq!(info.decode_time, header.decode_time + omitted);
            assert_eq!(u64::from(info.duration) + omitted, total);
        }
        Err(ExportError::NotAllSync) => assert!(!all_sync),
        Err(ExportError::InvalidDuration) => assert!(all_sync && skip > last_start),
        Err(error) => panic!("an encoded fragment was refused: {error}"),
    }
    let mut moved = fragment.clone();
    retrack_fragment(&mut moved, 2).expect("an encoded fragment moves");
    let info = parse_fragment(&moved).expect("the moved fragment parses");
    assert_eq!((info.track_id, info.all_sync), (2, all_sync));
    retrack_fragment(&mut moved, 1).expect("it moves back");
    assert_eq!(moved, fragment);
}

fuzz_target!(|data: &[u8]| {
    cut_an_encoded_fragment(data);
    let _ = parse_fragment(data);
    let mut fragment = data.to_vec();
    if rebase_fragment(&mut fragment, 7, 90_000).is_ok() {
        // A rewritten fragment parses with the new values.
        let info = parse_fragment(&fragment).expect("rebased fragment parses");
        assert_eq!(info.sequence, 7);
        assert_eq!(info.decode_time, 90_000);
    }
    let mut moved = data.to_vec();
    if retrack_fragment(&mut moved, 2).is_ok() {
        let info = parse_fragment(&moved).expect("moved fragment parses");
        assert_eq!(info.track_id, 2);
        assert_eq!(moved.len(), data.len());
    }
    if let Ok(init) = with_edit_list(data, 45_000, 5_500) {
        assert_eq!(init.len(), data.len() + EDIT_LIST_BYTES);
    }
    let edit = |media_time| TrackEdit {
        media_time,
        duration_ms: 5_500,
    };
    if let Ok(init) = with_edit_lists(data, &[edit(45_000), edit(0)]) {
        assert_eq!(init.len(), data.len() + 2 * EDIT_LIST_BYTES);
    }
    if let Some(entry) = sample_entry_bytes(data) {
        let _ = audio_channel_count(entry);
    }
    let _ = audio_channel_count(data);
    let limit = parse_fragment(data).map_or(1, |info| i64::from(info.duration) / 2 + 1);
    if let Ok(TrimmedStart {
        fragment: rest,
        omitted_ticks: omitted,
    }) = trim_fragment_start(data.to_vec(), MediaTicks::new(limit))
    {
        let whole = parse_fragment(data).expect("a fragment that was cut parses");
        let info = parse_fragment(&rest).expect("the remainder parses");
        assert!(info.all_sync);
        assert_eq!(info.sequence, whole.sequence);
        assert_eq!(info.decode_time, whole.decode_time + omitted);
        assert_eq!(
            u64::from(info.duration) + omitted,
            u64::from(whole.duration)
        );
    }
    if let Ok(trimmed) = trim_fragment_end(
        data.to_vec(),
        FragmentHeader {
            sequence: 7,
            decode_time: 0,
            wall: WallUs::new(0),
        },
        MediaTicks::new(limit),
    ) {
        let info = parse_fragment(&trimmed).expect("trimmed fragment parses");
        assert_eq!(info.sequence, 7);
        assert_eq!(info.decode_time, 0);
        assert!(
            i64::from(info.duration) <= limit,
            "trimmed to {} ticks over a limit of {limit}",
            info.duration
        );
    }
});
