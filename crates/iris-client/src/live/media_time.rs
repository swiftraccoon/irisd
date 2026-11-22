//! What a viewer reads from a live fragment itself, before its demuxer has
//! it: the media time, which places a feed on the clock. The bytes come from
//! the network; nothing here panics on them, and the fuzz target
//! `live_fragment` checks that.

use iris_core::time::MediaTicks;

/// The size of the `prft` box a live fragment starts with, as
/// `iris_mp4::fragment::encode_fragment` writes it: version 1, with a 64-bit
/// media time.
const PRFT_BYTES: u32 = 32;

/// The media time of a live fragment: the media time of the `prft` box it
/// starts with, in the 90 kHz ticks the box counts in. The box must be the 32
/// bytes of a version-1 `prft`, as the daemon writes it. `None` for an init
/// segment, for any other box, for a `prft` of another size or version, and
/// for ticks above `i64::MAX`.
///
/// # Example
///
/// ```
/// # use std::error::Error;
/// #
/// use iris_client::live;
/// use iris_core::time::MediaTicks;
///
/// // A live fragment that starts 1,000 s into its run.
/// let mut payload = Vec::new();
/// payload.extend_from_slice(&32_u32.to_be_bytes());
/// payload.extend_from_slice(b"prft");
/// // The version, the flags, and the track.
/// payload.extend_from_slice(&[1, 0, 0, 0x18, 0, 0, 0, 1]);
/// // The wall time, then the media time.
/// payload.extend_from_slice(&0_u64.to_be_bytes());
/// payload.extend_from_slice(&90_000_000_u64.to_be_bytes());
/// let media = live::media_time(&payload).ok_or("a live fragment")?;
/// assert_eq!(media, MediaTicks::from_secs(1_000));
/// // An init segment has no media time.
/// assert_eq!(live::media_time(b"\0\0\0\x1cftypiso6"), None);
/// # Ok::<(), Box<dyn Error>>(())
/// ```
#[must_use]
pub fn media_time(payload: &[u8]) -> Option<MediaTicks> {
    // The box's size is bytes 0 to 4, its type 4 to 8, its version byte 8,
    // and its flags 9 to 12. The track is bytes 12 to 16, the wall time 16 to
    // 24, and the media time 24 to 32.
    if payload.get(0..4)? != PRFT_BYTES.to_be_bytes().as_slice()
        || payload.get(4..8)? != b"prft"
        || *payload.get(8)? != 1
    {
        return None;
    }
    let ticks = u64::from_be_bytes(*payload.get(24..32)?.first_chunk::<8>()?);
    i64::try_from(ticks).ok().map(MediaTicks::new)
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
        assert_eq!(
            media_time(&prft(23_585_436)),
            Some(MediaTicks::new(23_585_436))
        );
    }

    #[test]
    fn a_prft_cut_short_gives_no_media_time() {
        let fragment = prft(23_585_436);
        for cut in 0..fragment.len() {
            assert_eq!(media_time(&fragment[..cut]), None, "cut at {cut}");
        }
    }

    /// The reader takes the media time of a version-1 `prft` alone, the
    /// version the daemon writes.
    #[test]
    fn a_version_0_prft_gives_no_media_time() {
        let mut version_zero = prft(23_585_436);
        version_zero[8] = 0;
        assert_eq!(media_time(&version_zero), None);
    }

    /// A `prft` laid out as the daemon writes it in every byte but its size.
    /// One that declares fewer bytes ends before its media time. One that
    /// declares more is not the box the daemon writes.
    #[test]
    fn a_prft_of_another_size_gives_no_media_time() {
        for size in [0_u32, 24, 31, 33, 40] {
            let mut resized = prft(23_585_436);
            resized[0..4].copy_from_slice(&size.to_be_bytes());
            assert_eq!(media_time(&resized), None, "{size} bytes");
        }
    }

    /// A box of another type, laid out as a version-1 `prft` in every other
    /// byte, so that its type alone refuses it.
    #[test]
    fn a_payload_that_starts_with_another_box_gives_no_media_time() {
        let mut other = prft(23_585_436);
        other[4..8].copy_from_slice(b"moof");
        assert_eq!(media_time(&other), None);
    }

    /// One tick past the most a signed 64-bit count holds, and the most a
    /// `u64` holds.
    #[test]
    fn ticks_past_the_signed_range_give_no_media_time() {
        let most = u64::try_from(i64::MAX).unwrap();
        assert_eq!(media_time(&prft(most + 1)), None);
        assert_eq!(media_time(&prft(u64::MAX)), None);
    }

    #[test]
    fn ticks_of_i64_max_give_a_media_time() {
        let most = u64::try_from(i64::MAX).unwrap();
        assert_eq!(media_time(&prft(most)), Some(MediaTicks::new(i64::MAX)));
    }
}
