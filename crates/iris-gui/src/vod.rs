//! The VOD playlist rules the viewer applies before GStreamer sees a body: the
//! `#IRIS-FRAGMENT` timing the timeline maps through, and the rewrite that makes
//! every init and media reference an absolute URL on the API origin. The module
//! depends on `anyhow`, `url`, and `iris-core` alone, so the `vod_playlist` fuzz
//! target compiles it unchanged.

use anyhow::Context;
use iris_core::time::{MediaTicks, WallUs};
use url::{Origin, Url};

/// Bounds playlist metadata received before playback starts, and the resolved
/// body handed to the player.
pub const MAX_VOD_PLAYLIST_BYTES: usize = 64 << 20;
/// Matches the API's selected-fragment budget.
pub const MAX_PLAYBACK_FRAGMENTS: usize = 100_000;
/// The tags the daemon's VOD writer emits. Any other `#EXT` tag rejects a
/// playlist, in particular the URI-bearing ones (`EXT-X-KEY`, `EXT-X-MEDIA`,
/// `EXT-X-PART`, `EXT-X-STREAM-INF`, ...) that would make the player fetch
/// from elsewhere. Other comments, such as `#IRIS-FRAGMENT`, pass through.
const VOD_TAGS: [&str; 11] = [
    "#EXTM3U",
    "#EXT-X-VERSION",
    "#EXT-X-TARGETDURATION",
    "#EXT-X-PLAYLIST-TYPE",
    "#EXT-X-MEDIA-SEQUENCE",
    "#EXT-X-INDEPENDENT-SEGMENTS",
    "#EXT-X-MAP",
    "#EXTINF",
    "#EXT-X-BYTERANGE",
    "#EXT-X-DISCONTINUITY",
    "#EXT-X-ENDLIST",
];

/// One complete fragment selected by the VOD playlist, in served order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PlaybackFragment {
    /// Recorder time of the fragment's first sample.
    pub wall_start: WallUs,
    /// The fragment's duration in the API's 90 kHz media timescale; positive.
    pub duration: MediaTicks,
}

/// Whether a playlist line is an HLS tag. The comparison ignores case so that
/// no spelling of a tag escapes [`VOD_TAGS`].
fn is_tag(line: &str) -> bool {
    line.as_bytes()
        .get(..4)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case(b"#EXT"))
}

/// Resolves a playlist reference against `base` and requires the result to be
/// on `origin` (scheme, host, and effective port) without userinfo. Opaque
/// origins such as `file:` and `data:` never match.
fn same_origin(base: &Url, reference: &str, origin: &Origin) -> anyhow::Result<Url> {
    let absolute = base.join(reference)?;
    anyhow::ensure!(
        absolute.origin() == *origin
            && absolute.username().is_empty()
            && absolute.password().is_none(),
        "VOD playlist references another origin"
    );
    Ok(absolute)
}

/// Rewrites the daemon's VOD body for the player: init and media references
/// resolve against `base`, the URL the body came from, and become absolute URLs
/// on `origin` with their queries; the body may hold only the tags in
/// [`VOD_TAGS`], and the result at most [`MAX_VOD_PLAYLIST_BYTES`].
pub fn absolute_playlist(text: &str, base: &Url, origin: &Origin) -> anyhow::Result<String> {
    let mut body = String::new();
    for line in text.lines() {
        if is_tag(line) {
            let name = line.split_once(':').map_or(line, |(name, _)| name);
            anyhow::ensure!(
                VOD_TAGS.contains(&name),
                "VOD playlist has a tag the daemon does not emit"
            );
        }
        if let Some(attributes) = line.strip_prefix("#EXT-X-MAP:") {
            let value = attributes
                .strip_prefix("URI=\"")
                .context("VOD init map must start with its URI")?;
            let (uri, suffix) = value.split_once('"').context("invalid VOD init URI")?;
            anyhow::ensure!(!suffix.contains("URI="), "VOD init map has a second URI");
            let absolute = same_origin(base, uri, origin).context("invalid VOD init URI")?;
            body.push_str("#EXT-X-MAP:URI=\"");
            body.push_str(absolute.as_str());
            body.push('"');
            body.push_str(suffix);
        } else if line.is_empty() || line.starts_with('#') {
            body.push_str(line);
        } else {
            let absolute = same_origin(base, line, origin).context("invalid VOD media URI")?;
            body.push_str(absolute.as_str());
        }
        body.push('\n');
        anyhow::ensure!(
            body.len() <= MAX_VOD_PLAYLIST_BYTES,
            "resolved VOD playlist exceeds its byte limit"
        );
    }
    Ok(body)
}

/// The `#IRIS-FRAGMENT:WALL-US=<start>,DURATION-TICKS=<ticks>` timing of a VOD
/// body in served order: at least one fragment and at most
/// [`MAX_PLAYBACK_FRAGMENTS`], each with a positive duration whose end fits the
/// wall-clock range.
pub fn playlist_fragments(text: &str) -> anyhow::Result<Vec<PlaybackFragment>> {
    anyhow::ensure!(
        text.len() <= MAX_VOD_PLAYLIST_BYTES,
        "VOD playlist exceeds its byte limit"
    );
    let mut fragments = Vec::new();
    for line in text.lines() {
        let Some(fields) = line.strip_prefix("#IRIS-FRAGMENT:WALL-US=") else {
            continue;
        };
        let (wall, ticks) = fields
            .split_once(",DURATION-TICKS=")
            .context("invalid VOD fragment timing")?;
        let wall_start: i64 = wall.parse().context("invalid VOD fragment wall time")?;
        let ticks: u32 = ticks.parse().context("invalid VOD fragment duration")?;
        anyhow::ensure!(ticks > 0, "zero VOD fragment duration");
        let duration = MediaTicks::new(i64::from(ticks));
        duration
            .checked_as_micros()
            .and_then(|micros| wall_start.checked_add(micros))
            .context("VOD fragment wall time overflows")?;
        anyhow::ensure!(
            fragments.len() < MAX_PLAYBACK_FRAGMENTS,
            "VOD playlist has too many fragments"
        );
        fragments.push(PlaybackFragment {
            wall_start: WallUs::new(wall_start),
            duration,
        });
    }
    anyhow::ensure!(
        !fragments.is_empty(),
        "VOD playlist has no fragment timing metadata"
    );
    Ok(fragments)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The response URL of a fixture VOD request, and the API origin it belongs to.
    fn vod_base() -> (Url, Origin) {
        let base =
            Url::parse("https://192.0.2.10/api/streams/fixture/main/hls/vod.m3u8?token=unused")
                .unwrap();
        let origin = Url::parse("https://192.0.2.10:443/").unwrap().origin();
        (base, origin)
    }

    fn fragment(wall_start: i64, ticks: i64) -> PlaybackFragment {
        PlaybackFragment {
            wall_start: WallUs::new(wall_start),
            duration: MediaTicks::new(ticks),
        }
    }

    #[test]
    fn fragment_timing_keeps_served_order_and_whole_boundary_gops() {
        let fragments = playlist_fragments(concat!(
            "#EXTM3U\n",
            "#IRIS-FRAGMENT:WALL-US=100,DURATION-TICKS=180000\n",
            "#IRIS-FRAGMENT:WALL-US=9000000,DURATION-TICKS=3001\n",
        ))
        .unwrap();
        assert_eq!(
            fragments,
            [fragment(100, 180_000), fragment(9_000_000, 3001)]
        );
    }

    #[test]
    fn missing_zero_or_overflowing_fragment_timing_is_refused() {
        for invalid in [
            "#EXTM3U\n",
            "#IRIS-FRAGMENT:WALL-US=0,DURATION-TICKS=0",
            "#IRIS-FRAGMENT:WALL-US=9223372036854775807,DURATION-TICKS=90000",
            "#IRIS-FRAGMENT:WALL-US=x,DURATION-TICKS=90000",
            "#IRIS-FRAGMENT:WALL-US=0,DURATION-TICKS=4294967296",
        ] {
            assert!(playlist_fragments(invalid).is_err(), "{invalid}");
        }
    }

    #[test]
    fn references_resolve_against_the_response_url_and_keep_queries_and_byte_ranges() {
        let (base, origin) = vod_base();
        let playlist = concat!(
            "#EXTM3U\n#EXT-X-VERSION:7\n#EXT-X-TARGETDURATION:1\n#EXT-X-MEDIA-SEQUENCE:0\n",
            "#EXT-X-PLAYLIST-TYPE:VOD\n#EXT-X-INDEPENDENT-SEGMENTS\n",
            "#EXT-X-MAP:URI=\"/api/segments/1/file.mp4?token=encoded%26token\",",
            "BYTERANGE=\"100@0\"\n",
            "#IRIS-FRAGMENT:WALL-US=10,DURATION-TICKS=90000\n",
            "#EXTINF:1.00000,\n#EXT-X-BYTERANGE:200@100\n",
            "../media/file.mp4?token=encoded%26token\n",
            "#EXT-X-DISCONTINUITY\n#EXTINF:1.00000,\n",
            "https://192.0.2.10:443/api/segments/2/file.mp4?token=same\n#EXT-X-ENDLIST\n"
        );
        let body = absolute_playlist(playlist, &base, &origin).unwrap();
        assert!(body.contains(concat!(
            "#EXT-X-MAP:URI=\"https://192.0.2.10/api/segments/1/file.mp4?token=encoded%26token\",",
            "BYTERANGE=\"100@0\"\n"
        )));
        assert!(body.contains(concat!(
            "#EXT-X-BYTERANGE:200@100\n",
            "https://192.0.2.10/api/streams/fixture/main/media/file.mp4?token=encoded%26token\n"
        )));
        assert!(body.contains("\nhttps://192.0.2.10/api/segments/2/file.mp4?token=same\n"));
    }

    #[test]
    fn the_rewrite_keeps_the_fragment_timing() {
        let (base, origin) = vod_base();
        let playlist = concat!(
            "#EXTM3U\n#EXT-X-MAP:URI=\"/api/segments/1/file.mp4\"\n",
            "#IRIS-FRAGMENT:WALL-US=10,DURATION-TICKS=90000\n#EXTINF:1.00000,\n",
            "/api/segments/1/file.mp4\n#EXT-X-ENDLIST\n"
        );
        let body = absolute_playlist(playlist, &base, &origin).unwrap();
        assert_eq!(
            playlist_fragments(playlist).unwrap(),
            playlist_fragments(&body).unwrap()
        );
    }

    #[test]
    fn references_to_another_origin_or_with_userinfo_are_refused() {
        let (base, origin) = vod_base();
        let with_media = |uri: &str| {
            format!(
                "#EXTM3U\n#EXT-X-MAP:URI=\"/api/segments/1/file.mp4\"\n#EXTINF:1.0,\n{uri}\n\
                 #EXT-X-ENDLIST\n"
            )
        };
        let with_map = |uri: &str| {
            format!("#EXTM3U\n#EXT-X-MAP:URI=\"{uri}\"\n#EXTINF:1.0,\n/api/segments/1/file.mp4\n")
        };
        for foreign in [
            "https://192.0.2.20/file.mp4?token=other",
            "//192.0.2.20/file.mp4",
            "https://192.0.2.10:8443/file.mp4",
            "http://192.0.2.10/file.mp4",
            "file:///etc/hostname",
            "data:video/mp4;base64,AAAA",
            "https://user@192.0.2.10/file.mp4",
            "https://user:secret@192.0.2.10/file.mp4",
        ] {
            assert!(
                absolute_playlist(&with_media(foreign), &base, &origin).is_err(),
                "media {foreign}"
            );
            assert!(
                absolute_playlist(&with_map(foreign), &base, &origin).is_err(),
                "map {foreign}"
            );
        }
        assert!(absolute_playlist(&with_media("/api/segments/2/file.mp4"), &base, &origin).is_ok());
    }

    #[test]
    fn an_init_map_must_carry_exactly_one_quoted_uri_first() {
        let (base, origin) = vod_base();
        for map in [
            "#EXT-X-MAP:BYTERANGE=\"1@0\",URI=\"/api/segments/1/file.mp4\"",
            "#EXT-X-MAP:URI=\"/api/segments/1/file.mp4\",URI=\"https://192.0.2.20/x\"",
            "#EXT-X-MAP:URI=/api/segments/1/file.mp4",
        ] {
            assert!(
                absolute_playlist(&format!("#EXTM3U\n{map}\n"), &base, &origin).is_err(),
                "{map}"
            );
        }
    }

    #[test]
    fn playlists_carry_only_the_tags_the_daemon_emits() {
        let (base, origin) = vod_base();
        for tag in [
            "#EXT-X-KEY:METHOD=AES-128,URI=\"https://192.0.2.20/key.bin\"",
            "#EXT-X-KEY:METHOD=NONE",
            "#EXT-X-SESSION-KEY:METHOD=AES-128,URI=\"/key.bin\"",
            "#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"a\",NAME=\"a\",URI=\"https://192.0.2.20/a.m3u8\"",
            "#EXT-X-PART:DURATION=0.5,URI=\"https://192.0.2.20/part.m4s\"",
            "#EXT-X-PRELOAD-HINT:TYPE=PART,URI=\"https://192.0.2.20/part.m4s\"",
            "#EXT-X-RENDITION-REPORT:URI=\"https://192.0.2.20/other.m3u8\"",
            "#EXT-X-I-FRAME-STREAM-INF:BANDWIDTH=1,URI=\"https://192.0.2.20/i.m3u8\"",
            "#EXT-X-SESSION-DATA:DATA-ID=\"x\",URI=\"https://192.0.2.20/data.json\"",
            "#EXT-X-STREAM-INF:BANDWIDTH=1",
            "#EXT-X-PROGRAM-DATE-TIME:2026-11-01T00:00:00Z",
            "#EXT-X-DISCONTINUITY-SEQUENCE:0",
            "#ext-x-key:METHOD=AES-128,URI=\"https://192.0.2.20/key.bin\"",
            "#EXTM3U8",
        ] {
            let playlist =
                format!("#EXTM3U\n{tag}\n#EXTINF:1.0,\n/api/segments/1/file.mp4\n#EXT-X-ENDLIST\n");
            assert!(
                absolute_playlist(&playlist, &base, &origin).is_err(),
                "{tag}"
            );
        }
    }

    #[test]
    fn comments_other_than_tags_pass_through() {
        let (base, origin) = vod_base();
        let commented = "#EXTM3U\n# a comment\n#IRIS-FRAGMENT:WALL-US=1,DURATION-TICKS=9\n";
        assert_eq!(
            absolute_playlist(commented, &base, &origin).unwrap(),
            commented
        );
    }
}
