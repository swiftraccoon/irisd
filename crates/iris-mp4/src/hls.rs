//! Static HLS playlists over a single recording file, using byte ranges.

use std::fmt::Write as _;

use crate::index::IndexRecord;
use iris_core::time::TIMESCALE;

/// Playlist generation options.
#[derive(Clone, Debug)]
pub struct PlaylistOptions {
    /// URI of the media file as seen by the player (relative to the playlist).
    pub media_uri: String,
    /// How many fragments to group per segment; grouping starts at sync fragments.
    pub fragments_per_segment: usize,
    /// Whether segment byte ranges include the 24-byte trailers. When false,
    /// every segment is exactly one fragment ending before its trailer.
    pub include_trailers: bool,
}

/// Builds a VOD media playlist.
/// Records come from a validated sidecar or scan. Trailer exclusion emits one
/// range per fragment; independence is declared only when every segment starts at sync.
#[must_use]
pub fn media_playlist(init_len: u32, records: &[IndexRecord], options: &PlaylistOptions) -> String {
    let per_segment = if options.include_trailers {
        options.fragments_per_segment.max(1)
    } else {
        1
    };
    let mut segments: Vec<(u64, u64, u64)> = Vec::new(); // (offset, len, duration ticks)
    let mut current: Option<(u64, u64, u64, usize)> = None;
    let mut independent = true;
    for r in records {
        let r_len = if options.include_trailers {
            u64::from(r.len)
        } else {
            u64::from(r.len) - 24
        };
        match current.as_mut() {
            Some((offset, len, dur, n))
                if options.include_trailers && !(r.starts_with_sync() && *n >= per_segment) =>
            {
                *len = (r.offset + r_len) - *offset;
                *dur += u64::from(r.duration);
                *n += 1;
            }
            _ => {
                if let Some((o, l, d, _)) = current.take() {
                    segments.push((o, l, d));
                }
                current = Some((r.offset, r_len, u64::from(r.duration), 1));
                independent &= r.starts_with_sync();
            }
        }
    }
    if let Some((o, l, d, _)) = current {
        segments.push((o, l, d));
    }
    let target = segments
        .iter()
        .map(|s| s.2)
        .max()
        .unwrap_or(0)
        .div_ceil(u64::from(TIMESCALE))
        .max(1);

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
    for (offset, len, dur) in segments {
        let _ = writeln!(out, "#EXTINF:{:.5},", dur as f64 / f64::from(TIMESCALE));
        let _ = writeln!(out, "#EXT-X-BYTERANGE:{len}@{offset}");
        out.push_str(&options.media_uri);
        out.push('\n');
    }
    out.push_str("#EXT-X-ENDLIST\n");
    out
}

/// Builds a multivariant (master) playlist with one variant.
/// Independence is declared by the media playlist when its segments all start at sync.
#[must_use]
pub fn master_playlist(
    media_playlist_uri: &str,
    codecs: &str,
    bandwidth: u64,
    width: u16,
    height: u16,
) -> String {
    format!(
        "#EXTM3U\n#EXT-X-VERSION:7\n#EXT-X-STREAM-INF:BANDWIDTH={bandwidth},CODECS=\"{codecs}\",RESOLUTION={width}x{height}\n{media_playlist_uri}\n"
    )
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

    #[test]
    fn groups_fragments_into_segments_starting_at_sync() {
        let records = [
            record(700, 1000, true),
            record(1700, 1000, false),
            record(2700, 1000, true),
            record(3700, 1000, true),
        ];
        let opts = PlaylistOptions {
            media_uri: "a.mp4".into(),
            fragments_per_segment: 2,
            include_trailers: true,
        };
        let text = media_playlist(700, &records, &opts);
        assert!(text.contains("#EXT-X-MAP:URI=\"a.mp4\",BYTERANGE=\"700@0\""));
        assert!(text.contains("#EXT-X-BYTERANGE:2000@700\n"));
        assert!(text.contains("#EXT-X-BYTERANGE:2000@2700\n"));
        assert!(text.contains("#EXT-X-TARGETDURATION:4\n"));
        assert!(text.ends_with("#EXT-X-ENDLIST\n"));
    }

    #[test]
    fn excludes_trailers_when_asked() {
        let records = [record(700, 1000, true), record(1700, 1000, true)];
        let opts = PlaylistOptions {
            media_uri: "a.mp4".into(),
            fragments_per_segment: 3,
            include_trailers: false,
        };
        let text = media_playlist(700, &records, &opts);
        assert!(text.contains("#EXT-X-BYTERANGE:976@700\n"));
        assert!(text.contains("#EXT-X-BYTERANGE:976@1700\n"));
    }

    #[test]
    fn trailer_exclusion_keeps_continuations_separate_and_does_not_claim_independence() {
        let records = [
            record(700, 1000, true),
            record(1700, 1000, false),
            record(2700, 1000, false),
            record(3700, 1000, true),
        ];
        let mut opts = PlaylistOptions {
            media_uri: "a.mp4".into(),
            fragments_per_segment: 3,
            include_trailers: false,
        };
        let text = media_playlist(700, &records, &opts);
        let ranges: Vec<_> = text
            .lines()
            .filter_map(|line| line.strip_prefix("#EXT-X-BYTERANGE:"))
            .collect();
        assert_eq!(ranges, ["976@700", "976@1700", "976@2700", "976@3700"]);
        assert_eq!(text.matches("#EXTINF:2.00000,").count(), 4);
        assert!(!text.contains("#EXT-X-INDEPENDENT-SEGMENTS"));
        assert!(
            !master_playlist("index.m3u8", "hvc1", 1, 16, 16)
                .contains("#EXT-X-INDEPENDENT-SEGMENTS")
        );
        for (range, record) in ranges.iter().zip(&records) {
            let (len, offset) = range.split_once('@').unwrap();
            let start = offset.parse::<u64>().unwrap();
            let end = start + len.parse::<u64>().unwrap();
            assert_eq!(end, record.end() - 24);
            for other in &records {
                assert!(end <= other.end() - 24 || start >= other.end());
            }
        }
        opts.include_trailers = true;
        let included = media_playlist(700, &records, &opts);
        assert!(included.contains("#EXT-X-BYTERANGE:3000@700\n"));
        assert!(included.contains("#EXT-X-BYTERANGE:1000@3700\n"));
        assert!(included.contains("#EXT-X-INDEPENDENT-SEGMENTS"));
    }
}
