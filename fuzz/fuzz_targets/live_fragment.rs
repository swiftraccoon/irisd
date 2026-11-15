//! Fuzzes what the viewer reads from a live payload before the demuxer has
//! it (`crates/iris-gui/src/fragment.rs`): any bytes give a media time or
//! none, never a panic, and a media time only for a payload that starts
//! with a version-1 `prft` box, whose ticks it then is.
#![no_main]

use libfuzzer_sys::fuzz_target;

#[path = "../../crates/iris-gui/src/fragment.rs"]
mod fragment;

fuzz_target!(|data: &[u8]| {
    let Some(nanos) = fragment::media_time(data) else {
        return;
    };
    assert!(data.len() >= 32, "a media time from {} bytes", data.len());
    assert_eq!(&data[4..8], b"prft");
    assert_eq!(data[8], 1);
    let ticks = u64::from_be_bytes(data[24..32].try_into().expect("eight bytes"));
    // 90 kHz ticks as nanoseconds, rounded down.
    assert_eq!(
        u128::try_from(nanos).ok(),
        Some(u128::from(ticks) * 100_000 / 9)
    );
});
