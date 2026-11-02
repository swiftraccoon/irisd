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
#[must_use]
pub fn media_playlist(init_len: u32, records: &[IndexRecord], options: &PlaylistOptions) -> String {
    let per_segment = if options.include_trailers {
        options.fragments_per_segment.max(1)
    } else {
        1
    };
    let mut segments: Vec<(u64, u64, u64)> = Vec::new(); // (offset, len, duration ticks)
    let mut current: Option<(u64, u64, u64, usize)> = None;
    for r in records {
        let r_len = if options.include_trailers {
            u64::from(r.len)
        } else {
            u64::from(r.len) - 24
        };
        match current.as_mut() {
            Some((offset, len, dur, n)) if !(r.starts_with_sync() && *n >= per_segment) => {
                *len = (r.offset + r_len) - *offset;
                *dur += u64::from(r.duration);
                *n += 1;
            }
            _ => {
                if let Some((o, l, d, _)) = current.take() {
                    segments.push((o, l, d));
                }
                current = Some((r.offset, r_len, u64::from(r.duration), 1));
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
    out.push_str(
        "#EXT-X-MEDIA-SEQUENCE:0\n#EXT-X-PLAYLIST-TYPE:VOD\n#EXT-X-INDEPENDENT-SEGMENTS\n",
    );
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
#[must_use]
pub fn master_playlist(
    media_playlist_uri: &str,
    codecs: &str,
    bandwidth: u64,
    width: u16,
    height: u16,
) -> String {
    format!(
        "#EXTM3U\n#EXT-X-VERSION:7\n#EXT-X-INDEPENDENT-SEGMENTS\n#EXT-X-STREAM-INF:BANDWIDTH={bandwidth},CODECS=\"{codecs}\",RESOLUTION={width}x{height}\n{media_playlist_uri}\n"
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
}
