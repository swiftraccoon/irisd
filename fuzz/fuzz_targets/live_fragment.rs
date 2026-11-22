//! Fuzzes what a viewer reads from a live payload before its demuxer has it
//! (`iris_client::live::media_time`). Any bytes give a media time or none,
//! never a panic. A media time comes only from a payload that starts with
//! the 32 bytes of a version-1 `prft` box, and it is that box's ticks.
#![no_main]

use iris_client::live;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Some(media) = live::media_time(data) else {
        return;
    };
    assert!(data.len() >= 32, "a media time from {} bytes", data.len());
    assert_eq!(
        data[0..4],
        32_u32.to_be_bytes(),
        "a media time from a prft that is not 32 bytes"
    );
    assert_eq!(
        &data[4..8],
        b"prft",
        "a media time from a box that is no prft"
    );
    assert_eq!(
        data[8], 1,
        "a media time from a prft of version {}",
        data[8]
    );
    let ticks = u64::from_be_bytes(data[24..32].try_into().expect("eight bytes"));
    // The media time is the box's 90 kHz ticks.
    assert_eq!(
        u64::try_from(media.get()).ok(),
        Some(ticks),
        "a media time other than the box's ticks"
    );
});
