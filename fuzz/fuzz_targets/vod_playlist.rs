//! Fuzzes the viewer's VOD playlist rules (`crates/iris-gui/src/vod.rs`): any
//! body gives fragment timing or a refusal, never a panic, and an accepted
//! rewrite holds only `#EXT` tags the daemon emits and absolute references on
//! the API origin without userinfo, with the same fragment timing as its input.
#![no_main]

use libfuzzer_sys::fuzz_target;
use url::{Origin, Url};

#[path = "../../crates/iris-gui/src/vod.rs"]
mod vod;

/// The tags `vod::absolute_playlist` lets through, as `crates/iris-api/src/hls.rs`
/// writes them.
const DAEMON_TAGS: [&str; 11] = [
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

/// Asserts that a rewritten reference is an absolute URL on `origin` without
/// userinfo.
fn assert_on_origin(reference: &str, origin: &Origin) {
    let url = Url::parse(reference).expect("a rewritten reference is absolute");
    assert_eq!(&url.origin(), origin, "{reference}");
    assert!(
        url.username().is_empty() && url.password().is_none(),
        "{reference}"
    );
}

fuzz_target!(|data: &[u8]| {
    let Ok(text) = std::str::from_utf8(data) else {
        return;
    };
    let base = Url::parse("https://192.0.2.10/api/streams/cam1/main/hls/vod.m3u8?from=0&to=1")
        .expect("a fixed base URL");
    let origin = base.origin();
    let timing = vod::playlist_fragments(text);
    if let Ok(fragments) = &timing {
        assert!(!fragments.is_empty() && fragments.len() <= vod::MAX_PLAYBACK_FRAGMENTS);
        for fragment in fragments {
            assert!(fragment.duration.get() > 0, "{fragment:?}");
            assert!(
                fragment
                    .wall_start
                    .get()
                    .checked_add(fragment.duration.as_micros())
                    .is_some(),
                "{fragment:?}"
            );
        }
    }
    let Ok(body) = vod::absolute_playlist(text, &base, &origin) else {
        return;
    };
    assert!(body.len() <= vod::MAX_VOD_PLAYLIST_BYTES);
    for line in body.lines() {
        if let Some(attributes) = line.strip_prefix("#EXT-X-MAP:URI=\"") {
            let (uri, _) = attributes
                .split_once('"')
                .expect("a rewritten map keeps its quoted URI");
            assert_on_origin(uri, &origin);
        } else if line
            .as_bytes()
            .get(..4)
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case(b"#EXT"))
        {
            let name = line.split_once(':').map_or(line, |(name, _)| name);
            assert!(DAEMON_TAGS.contains(&name), "{line}");
        } else if !line.is_empty() && !line.starts_with('#') {
            assert_on_origin(line, &origin);
        }
    }
    assert_eq!(
        vod::playlist_fragments(&body).ok(),
        timing.ok(),
        "the rewrite keeps the timing comments"
    );
});
