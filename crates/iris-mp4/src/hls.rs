//! Static HLS playlists over a single recording file, using byte ranges.

use std::fmt::Write as _;

use thiserror::Error;

use crate::fragment::TRAILER_LEN;
use crate::index::IndexRecord;
use iris_core::time::TIMESCALE;

/// Playlist generation options.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct PlaylistOptions {
    /// URI of the media file as seen by the player (relative to the playlist),
    /// already percent-encoded: it is written into the playlist as given.
    pub media_uri: String,
    /// The least number of fragments per segment when trailers are included:
    /// a segment opens at a sync fragment and extends through at least this
    /// many fragments, then through every continuation fragment up to the
    /// next sync fragment. Without trailers every segment is one fragment.
    pub fragments_per_segment: usize,
    /// Whether segment byte ranges include the 24-byte trailers. When false,
    /// every segment is exactly one fragment ending before its trailer.
    pub include_trailers: bool,
}

/// The bit rates a multivariant playlist declares for one variant, in bits
/// per second (RFC 8216 §4.1): `peak` is the largest bit rate of any run of
/// consecutive segments whose total duration lies between half and one and a
/// half target durations (`BANDWIDTH`), and `average` is the whole playlist's
/// segment bytes over its duration (`AVERAGE-BANDWIDTH`). Both count segment
/// bytes only; the initialization section is not a media segment.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct VariantBitRates {
    /// The peak segment bit rate.
    pub peak: u64,
    /// The average segment bit rate.
    pub average: u64,
}

/// A URI or codecs string that would break the playlist syntax.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Error)]
#[error("playlist text must not contain quotation marks, line breaks, or control characters")]
pub struct InvalidPlaylistText;

/// Checks text that is written verbatim into a playlist line or a quoted
/// attribute value.
fn check_text(text: &str) -> Result<(), InvalidPlaylistText> {
    if text.chars().any(|c| c == '"' || c.is_control()) {
        Err(InvalidPlaylistText)
    } else {
        Ok(())
    }
}

/// One media segment: a byte range of the recording and its duration in ticks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Segment {
    offset: u64,
    len: u64,
    duration: u64,
}

/// Groups records into segments per `options` and reports whether every
/// segment starts at a sync fragment.
fn segments(records: &[IndexRecord], options: &PlaylistOptions) -> (Vec<Segment>, bool) {
    let per_segment = if options.include_trailers {
        options.fragments_per_segment.max(1)
    } else {
        1
    };
    let trailer = TRAILER_LEN as u64;
    let mut segments = Vec::new();
    let mut current: Option<(Segment, usize)> = None;
    let mut independent = true;
    for r in records {
        let r_end = if options.include_trailers {
            r.end()
        } else {
            r.end().saturating_sub(trailer)
        };
        match current.as_mut() {
            Some((segment, n))
                if options.include_trailers && !(r.starts_with_sync() && *n >= per_segment) =>
            {
                segment.len = r_end.saturating_sub(segment.offset);
                segment.duration += u64::from(r.duration);
                *n += 1;
            }
            _ => {
                if let Some((segment, _)) = current.take() {
                    segments.push(segment);
                }
                current = Some((
                    Segment {
                        offset: r.offset,
                        len: r_end.saturating_sub(r.offset),
                        duration: u64::from(r.duration),
                    },
                    1,
                ));
                independent &= r.starts_with_sync();
            }
        }
    }
    if let Some((segment, _)) = current {
        segments.push(segment);
    }
    (segments, independent)
}

/// The target duration in whole seconds: the longest segment rounded up, at
/// least one.
fn target_duration_secs(segments: &[Segment]) -> u64 {
    segments
        .iter()
        .map(|segment| segment.duration)
        .max()
        .unwrap_or(0)
        .div_ceil(u64::from(TIMESCALE))
        .max(1)
}

/// Bits per second of `bytes` over `ticks`; zero over an empty duration.
fn bit_rate(bytes: u128, ticks: u128) -> u64 {
    (bytes * 8 * u128::from(TIMESCALE))
        .checked_div(ticks)
        .map_or(0, |rate| u64::try_from(rate).unwrap_or(u64::MAX))
}

/// The bit rates [`master_playlist`] declares for the media playlist that
/// [`media_playlist`] builds from the same records and options. When no run
/// of segments reaches half the target duration, the peak is the average.
///
/// # Example
///
/// ```
/// use iris_mp4::hls::{PlaylistOptions, variant_bit_rates};
/// use iris_mp4::index::{FLAG_STARTS_WITH_SYNC, IndexRecord};
///
/// let records = [
///     IndexRecord { offset: 700, len: 1_000, duration: 180_000, flags: FLAG_STARTS_WITH_SYNC, ..IndexRecord::default() },
///     IndexRecord { offset: 1_700, len: 3_000, duration: 180_000, flags: FLAG_STARTS_WITH_SYNC, ..IndexRecord::default() },
/// ];
/// let options = PlaylistOptions { media_uri: "a.mp4".into(), fragments_per_segment: 1, include_trailers: true };
/// let rates = variant_bit_rates(&records, &options);
/// assert_eq!((rates.peak, rates.average), (12_000, 8_000));
/// ```
#[must_use]
pub fn variant_bit_rates(records: &[IndexRecord], options: &PlaylistOptions) -> VariantBitRates {
    let (segments, _) = segments(records, options);
    let total_bytes: u128 = segments.iter().map(|s| u128::from(s.len)).sum();
    let total_ticks: u128 = segments.iter().map(|s| u128::from(s.duration)).sum();
    let average = bit_rate(total_bytes, total_ticks);
    let target_ticks = u128::from(target_duration_secs(&segments)) * u128::from(TIMESCALE);
    let (shortest, longest) = (target_ticks / 2, target_ticks * 3 / 2);
    let mut peak = 0;
    for start in 0..segments.len() {
        let mut bytes = 0u128;
        let mut ticks = 0u128;
        for segment in &segments[start..] {
            bytes += u128::from(segment.len);
            ticks += u128::from(segment.duration);
            if ticks > longest {
                break;
            }
            if ticks >= shortest {
                peak = peak.max(bit_rate(bytes, ticks));
            }
        }
    }
    VariantBitRates {
        peak: if peak == 0 { average } else { peak },
        average,
    }
}

/// Builds a VOD media playlist.
///
/// Records come from a validated sidecar or scan: contiguous and each longer
/// than a trailer. Other records yield byte ranges that do not describe the
/// file, never a panic. Trailer exclusion emits one range per fragment;
/// independence is declared only when every segment starts at sync.
///
/// # Errors
///
/// Returns [`InvalidPlaylistText`] when the media URI holds a quotation
/// mark, a line break, or another control character.
pub fn media_playlist(
    init_len: u32,
    records: &[IndexRecord],
    options: &PlaylistOptions,
) -> Result<String, InvalidPlaylistText> {
    check_text(&options.media_uri)?;
    let (segments, independent) = segments(records, options);
    let target = target_duration_secs(&segments);

    let mut out = String::with_capacity(segments.len() * 64 + 256);
    out.push_str("#EXTM3U\n#EXT-X-VERSION:7\n");
    let _ = writeln!(out, "#EXT-X-TARGETDURATION:{target}");
    out.push_str("#EXT-X-MEDIA-SEQUENCE:0\n#EXT-X-PLAYLIST-TYPE:VOD\n");
    if independent {
        out.push_str("#EXT-X-INDEPENDENT-SEGMENTS\n");
    }
    let _ = writeln!(
        out,
        "#EXT-X-MAP:URI=\"{}\",BYTERANGE=\"{init_len}@0\"",
        options.media_uri
    );
    for segment in segments {
        let _ = writeln!(
            out,
            "#EXTINF:{:.5},",
            segment.duration as f64 / f64::from(TIMESCALE)
        );
        let _ = writeln!(out, "#EXT-X-BYTERANGE:{}@{}", segment.len, segment.offset);
        out.push_str(&options.media_uri);
        out.push('\n');
    }
    out.push_str("#EXT-X-ENDLIST\n");
    Ok(out)
}

/// Builds a multivariant (master) playlist with one variant declaring
/// `rates` (RFC 8216 §4.3.4.2). Independence is declared by the media
/// playlist when its segments all start at sync.
///
/// # Errors
///
/// Returns [`InvalidPlaylistText`] when the playlist URI or the codecs
/// string holds a quotation mark, a line break, or another control
/// character.
pub fn master_playlist(
    media_playlist_uri: &str,
    codecs: &str,
    rates: VariantBitRates,
    width: u16,
    height: u16,
) -> Result<String, InvalidPlaylistText> {
    check_text(media_playlist_uri)?;
    check_text(codecs)?;
    let mut out = String::from("#EXTM3U\n#EXT-X-VERSION:7\n");
    let _ = writeln!(
        out,
        "#EXT-X-STREAM-INF:BANDWIDTH={},AVERAGE-BANDWIDTH={},CODECS=\"{codecs}\",RESOLUTION={width}x{height}",
        rates.peak, rates.average
    );
    out.push_str(media_playlist_uri);
    out.push('\n');
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::FLAG_STARTS_WITH_SYNC;

    fn record(offset: u64, len: u32, sync: bool) -> IndexRecord {
        IndexRecord {
            offset,
            len,
            duration: 180_000,
            sample_count: 60,
            flags: if sync { FLAG_STARTS_WITH_SYNC } else { 0 },
            ..IndexRecord::default()
        }
    }

    fn continuation_records() -> [IndexRecord; 4] {
        [
            record(700, 1000, true),
            record(1700, 1000, false),
            record(2700, 1000, false),
            record(3700, 1000, true),
        ]
    }

    fn options(fragments_per_segment: usize, include_trailers: bool) -> PlaylistOptions {
        PlaylistOptions {
            media_uri: "a.mp4".into(),
            fragments_per_segment,
            include_trailers,
        }
    }

    #[test]
    fn groups_fragments_into_segments_starting_at_sync() {
        let records = [
            record(700, 1000, true),
            record(1700, 1000, false),
            record(2700, 1000, true),
            record(3700, 1000, true),
        ];
        let text = media_playlist(700, &records, &options(2, true)).unwrap();
        assert!(text.contains("#EXT-X-MAP:URI=\"a.mp4\",BYTERANGE=\"700@0\""));
        assert!(text.contains("#EXT-X-BYTERANGE:2000@700\n"));
        assert!(text.contains("#EXT-X-BYTERANGE:2000@2700\n"));
        assert!(text.contains("#EXT-X-TARGETDURATION:4\n"));
        assert!(text.ends_with("#EXT-X-ENDLIST\n"));
    }

    #[test]
    fn excludes_trailers_when_asked() {
        let records = [record(700, 1000, true), record(1700, 1000, true)];
        let text = media_playlist(700, &records, &options(3, false)).unwrap();
        assert!(text.contains("#EXT-X-BYTERANGE:976@700\n"));
        assert!(text.contains("#EXT-X-BYTERANGE:976@1700\n"));
    }

    #[test]
    fn trailer_exclusion_keeps_continuations_separate_and_does_not_claim_independence() {
        let records = continuation_records();
        let text = media_playlist(700, &records, &options(3, false)).unwrap();
        let ranges: Vec<_> = text
            .lines()
            .filter_map(|line| line.strip_prefix("#EXT-X-BYTERANGE:"))
            .collect();
        assert_eq!(ranges, ["976@700", "976@1700", "976@2700", "976@3700"]);
        assert_eq!(text.matches("#EXTINF:2.00000,").count(), 4);
        assert!(!text.contains("#EXT-X-INDEPENDENT-SEGMENTS"));
        for (range, record) in ranges.iter().zip(&records) {
            let (len, offset) = range.split_once('@').unwrap();
            let start = offset.parse::<u64>().unwrap();
            let end = start + len.parse::<u64>().unwrap();
            assert_eq!(end, record.end() - 24);
            for other in &records {
                assert!(end <= other.end() - 24 || start >= other.end());
            }
        }
    }

    #[test]
    fn trailer_inclusion_groups_continuations_and_claims_independence() {
        let text = media_playlist(700, &continuation_records(), &options(3, true)).unwrap();
        assert!(text.contains("#EXT-X-BYTERANGE:3000@700\n"));
        assert!(text.contains("#EXT-X-BYTERANGE:1000@3700\n"));
        assert!(text.contains("#EXT-X-INDEPENDENT-SEGMENTS"));
    }

    #[test]
    fn the_master_playlist_makes_no_independence_claim() {
        let rates = VariantBitRates {
            peak: 12_000,
            average: 8_000,
        };
        let text = master_playlist("index.m3u8", "hvc1.1.6.L150.B0", rates, 16, 16).unwrap();
        assert!(!text.contains("#EXT-X-INDEPENDENT-SEGMENTS"));
        assert!(text.ends_with(
            "#EXT-X-STREAM-INF:BANDWIDTH=12000,AVERAGE-BANDWIDTH=8000,\
             CODECS=\"hvc1.1.6.L150.B0\",RESOLUTION=16x16\nindex.m3u8\n"
        ));
    }

    #[test]
    fn text_that_breaks_the_syntax_is_refused() {
        for bad in ["a\"b.mp4", "a\nb.mp4", "a\rb", "a\u{7f}b"] {
            let mut opts = options(1, true);
            opts.media_uri = bad.to_owned();
            assert_eq!(
                media_playlist(700, &[record(700, 1000, true)], &opts),
                Err(InvalidPlaylistText),
                "{bad:?}"
            );
            assert_eq!(
                master_playlist(bad, "hvc1", VariantBitRates::default(), 16, 16),
                Err(InvalidPlaylistText),
                "{bad:?}"
            );
            assert_eq!(
                master_playlist("index.m3u8", bad, VariantBitRates::default(), 16, 16),
                Err(InvalidPlaylistText),
                "{bad:?}"
            );
        }
        assert!(
            master_playlist(
                "a%20b.m3u8",
                "hvc1.1.6.L150.B0",
                VariantBitRates::default(),
                16,
                16
            )
            .is_ok()
        );
    }

    fn timed_record(offset: u64, len: u32, duration: u32) -> IndexRecord {
        IndexRecord {
            offset,
            len,
            duration,
            sample_count: 1,
            flags: FLAG_STARTS_WITH_SYNC,
            ..IndexRecord::default()
        }
    }

    #[test]
    fn the_peak_bit_rate_is_the_largest_qualifying_segment_window() {
        // One two-second segment (the target duration) and four 0.3 s segments:
        // the four short ones together form the 1.2 s window with the highest rate.
        let records = [
            timed_record(700, 1_000, 180_000),
            timed_record(1_700, 900, 27_000),
            timed_record(2_600, 900, 27_000),
            timed_record(3_500, 900, 27_000),
            timed_record(4_400, 900, 27_000),
        ];
        let rates = variant_bit_rates(&records, &options(1, true));
        assert_eq!(rates.peak, 24_000);
        assert_eq!(rates.average, 11_500);
    }

    #[test]
    fn a_window_shorter_than_half_the_target_does_not_set_the_peak() {
        // A 0.3 s segment of 9,000 bytes between two-second segments runs at
        // 240,000 bit/s by itself, but the shortest window that counts is
        // one second: with a neighbour it is 10,000 bytes over 2.3 s.
        let records = [
            timed_record(700, 1_000, 180_000),
            timed_record(1_700, 9_000, 27_000),
            timed_record(10_700, 1_000, 180_000),
        ];
        let rates = variant_bit_rates(&records, &options(1, true));
        assert_eq!(rates.peak, 34_782);
    }

    #[test]
    fn a_single_large_segment_sets_the_peak() {
        let records = [
            timed_record(700, 1_000, 180_000),
            timed_record(1_700, 1_000, 180_000),
            timed_record(2_700, 1_000, 180_000),
            timed_record(3_700, 5_000, 180_000),
        ];
        let rates = variant_bit_rates(&records, &options(1, true));
        assert_eq!(
            rates.peak, 20_000,
            "the 5,000-byte segment over two seconds"
        );
        assert_eq!(rates.average, 8_000);
    }

    #[test]
    fn without_a_qualifying_window_the_peak_is_the_average() {
        let records = [timed_record(700, 300, 27_000)];
        let rates = variant_bit_rates(&records, &options(1, true));
        assert_eq!(rates.average, 8_000);
        assert_eq!(rates.peak, rates.average);
        assert_eq!(
            variant_bit_rates(&[], &options(1, true)),
            VariantBitRates::default()
        );
    }
}
