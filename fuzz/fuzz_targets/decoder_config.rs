//! Fuzzes the decoder configuration readers: `DecoderConfig::parse` and
//! `parse_parameter_sets` with arbitrary bytes taken as an init segment, and
//! `parse_hvcc` with them taken as an `hvcC` body. A client reads the init
//! segment it is sent, so a corrupt one must yield an error, never a panic,
//! and what is accepted must be what a decoder can take.
//!
//! `DecoderConfig::parse` gives HEVC exactly when `parse_parameter_sets` gives
//! parameter sets, and then the same sets.
//!
//! The `esds` descriptors lie inside eight nested boxes, so the target also
//! takes the input as the body of the `esds` of an `mp4a` entry: the
//! descriptor reader is then reached whatever the corpus holds.
#![no_main]

use bytes::Bytes;
use iris_mp4::boxes::BoxWriter;
use iris_mp4::decoder_config::{self, DecoderConfig, DecoderConfigError};
use iris_mp4::init::{SampleEntry, build_init_segment};
use libfuzzer_sys::fuzz_target;

/// Parameter sets a decoder can take: at least a VPS, an SPS, and a PPS,
/// none of them empty, and no NAL unit of another type. They hold at most
/// 16 VPSs, 16 SPSs, and 64 PPSs, one for each identifier that ITU-T H.265
/// defines for the type.
fn check_parameter_sets(sets: &[Vec<u8>]) {
    for nal in sets {
        let Some(&header) = nal.first() else {
            panic!("an empty parameter set");
        };
        let nal_type = (header >> 1) & 0x3f;
        assert!(
            (32..=34).contains(&nal_type),
            "a NAL unit of type {nal_type} among the parameter sets"
        );
    }
    for (nal_type, max) in [(32, 16), (33, 16), (34, 64)] {
        let held = sets
            .iter()
            .filter(|nal| {
                nal.first()
                    .is_some_and(|header| ((header >> 1) & 0x3f) == nal_type)
            })
            .count();
        assert!(
            (1..=max).contains(&held),
            "{held} parameter sets of type {nal_type}"
        );
    }
}

/// A configuration a decoder can take, if one was read.
fn check(read: &Result<DecoderConfig, DecoderConfigError>) {
    match read {
        Ok(DecoderConfig::Hevc(config)) => check_parameter_sets(config.parameter_sets()),
        Ok(DecoderConfig::Aac(config)) => {
            let len = config.audio_specific_config().len();
            assert!(
                (2..=64).contains(&len),
                "an AudioSpecificConfig of {len} bytes"
            );
            assert_ne!(config.sample_rate(), 0, "a sampling rate of zero");
            assert_ne!(config.channels(), 0, "a channel count of zero");
        }
        _ => {}
    }
}

/// The init segment of one `mp4a` track of one channel whose `esds` box has
/// `body` as its body.
fn audio_init(body: &[u8]) -> Bytes {
    let mut w = BoxWriter::default();
    let entry = w.begin(b"mp4a");
    w.zeros(6);
    w.u16(1); // data_reference_index
    w.zeros(8);
    w.u16(1); // channelcount
    w.u16(16); // samplesize
    w.zeros(4);
    w.u32(48_000 << 16); // samplerate
    let esds = w.begin(b"esds");
    w.bytes(body);
    w.end(esds);
    w.end(entry);
    build_init_segment(&SampleEntry::audio(w.into_bytes(), "mp4a.40.2".into()))
}

fuzz_target!(|data: &[u8]| {
    let read = DecoderConfig::parse(data);
    check(&read);
    let sets = decoder_config::parse_parameter_sets(data);
    if let Ok(sets) = &sets {
        check_parameter_sets(sets);
    }
    // Both readers walk the same boxes to the first track's sample entry and
    // read its `hvcC` alike, so one gives HEVC exactly when the other gives
    // parameter sets, and the sets are the same.
    match (&read, &sets) {
        (Ok(DecoderConfig::Hevc(config)), Ok(sets)) => {
            assert_eq!(config.parameter_sets(), sets, "the readers give other sets");
        }
        (Ok(DecoderConfig::Hevc(_)), Err(refused)) => {
            panic!("parse_parameter_sets refuses an init segment that gives HEVC: {refused}")
        }
        (_, Ok(_)) => panic!("parse gives no HEVC where parse_parameter_sets gives sets: {read:?}"),
        _ => {}
    }
    if let Ok(sets) = decoder_config::parse_hvcc(data) {
        check_parameter_sets(&sets);
    }
    // An audio entry gives AAC or a refusal of its `esds`, nothing else.
    let read = DecoderConfig::parse(&audio_init(data));
    assert!(
        matches!(
            read,
            Ok(DecoderConfig::Aac(_)) | Err(DecoderConfigError::Esds(_))
        ),
        "{read:?}"
    );
    check(&read);
});
