//! What the viewer reads from a live fragment itself, before the demuxer
//! has it: the media time, which places a feed on the clock
//! (`crate::sound`). The bytes come from the network; nothing here panics
//! on them, and `fuzz/fuzz_targets/live_fragment.rs` holds it to that.

/// The media time, in nanoseconds, of a live fragment: the media time of
/// the `prft` box it starts with (version 1), in 90 kHz ticks. `None` for
/// an init segment, for anything else, and for a time that does not fit.
pub fn media_time(payload: &[u8]) -> Option<i64> {
    // size, type, version and flags, the track, the wall time, the media time.
    if payload.get(4..8)? != b"prft" || *payload.get(8)? != 1 {
        return None;
    }
    let ticks = u64::from_be_bytes(*payload.get(24..32)?.first_chunk::<8>()?);
    i64::try_from(u128::from(ticks) * 100_000 / 9).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The `prft` box a live fragment starts with, for the media time
    /// `ticks`.
    fn prft(ticks: u64) -> Vec<u8> {
        let mut fragment = Vec::new();
        fragment.extend_from_slice(&32_u32.to_be_bytes());
        fragment.extend_from_slice(b"prft");
        fragment.extend_from_slice(&[1, 0, 0, 0x18]);
        fragment.extend_from_slice(&1_u32.to_be_bytes());
        fragment.extend_from_slice(&0_u64.to_be_bytes());
        fragment.extend_from_slice(&ticks.to_be_bytes());
        fragment
    }

    #[test]
    fn a_fragments_media_time_is_read_from_its_prft() {
        let fragment = prft(23_585_436);
        assert_eq!(media_time(&fragment), Some(262_060_400_000));
        for cut in 0..fragment.len() {
            assert_eq!(media_time(&fragment[..cut]), None, "cut at {cut}");
        }
        let mut version_zero = fragment.clone();
        version_zero[8] = 0;
        assert_eq!(media_time(&version_zero), None);
        assert_eq!(media_time(b"\0\0\0\x1cftypiso6"), None);
        let mut far = fragment;
        far[24..32].copy_from_slice(&u64::MAX.to_be_bytes());
        assert_eq!(media_time(&far), None);
    }
}
