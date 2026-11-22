//! The VOD playlist rules a viewer applies before its player sees a body. The
//! `#IRIS-FRAGMENT` timing gives what a timeline maps through. The rewrite
//! makes every init and media reference an absolute URL on the API origin. The
//! bodies come from the network; nothing here panics on them, and the fuzz
//! target `vod_playlist` checks that. [`playlist_fragments`],
//! [`absolute_playlist`], and [`multivariant_playlist`] each show their use
//! in an example.

use std::num::ParseIntError;
use std::str::Utf8Error;

use iris_core::time::{MediaTicks, WallUs};
use thiserror::Error;
use url::{Origin, ParseError, Url};

/// The most bytes of a VOD playlist's body as a viewer receives it, and of
/// the body [`absolute_playlist`] rewrites it into: 64 MiB. The viewer sets
/// this bound above the daemon's, which renders a VOD playlist of at most its
/// `MAX_PLAYLIST_BYTES` (`crates/iris-api/src/hls.rs`), 16 MiB.
pub const MAX_VOD_PLAYLIST_BYTES: usize = 64 << 20;
/// The most fragments a VOD playlist's timing holds: the daemon's
/// `MAX_SELECTED_RECORDS` (`crates/iris-api/src/hls.rs`), since each record
/// the daemon selects for a playlist is one `#IRIS-FRAGMENT` line.
pub const MAX_PLAYBACK_FRAGMENTS: usize = 100_000;
/// The tags the daemon's VOD writer emits. The rewrite refuses a playlist with
/// any other `#EXT` tag, in particular those that name a URI (`EXT-X-KEY`,
/// `EXT-X-MEDIA`, `EXT-X-PART`) or precede one (`EXT-X-STREAM-INF`). Comments,
/// such as `#IRIS-FRAGMENT`, pass through.
pub const VOD_TAGS: [&str; 11] = [
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

/// One complete fragment of a VOD playlist, as its `#IRIS-FRAGMENT` line
/// gives it. The fragments [`playlist_fragments`] returns are in served
/// order, and each has a positive duration whose end fits the wall-clock
/// range. The fields are public, so a fragment built elsewhere may have any
/// duration. The example of [`playlist_fragments`] reads one.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct PlaybackFragment {
    /// Recorder time of the fragment's first sample.
    pub wall_start: WallUs,
    /// The fragment's duration in the API's 90 kHz media timescale.
    pub duration: MediaTicks,
}

/// Why a VOD playlist is refused. The text of each is the one a viewer
/// shows; a refused reference carries its reason as its source
/// ([`ReferenceError`]), so a viewer that prints the chain of sources prints
/// both. The examples of [`absolute_playlist`] and [`playlist_fragments`]
/// each give one.
#[derive(Clone, Debug, PartialEq, Eq, Error)]
#[non_exhaustive]
pub enum PlaylistError {
    /// The body is not UTF-8, for the reason the value gives. The functions
    /// here take text; the answer check of a VOD playlist
    /// ([`crate::api::answer::vod_playlist`]) refuses such a body before they
    /// read it.
    #[error("VOD playlist is not UTF-8")]
    NotUtf8(#[source] Utf8Error),
    /// A carriage return stands inside a line instead of before its line
    /// feed.
    #[error("VOD playlist has a carriage return inside a line")]
    CarriageReturn,
    /// A line that starts with `#EXT` in any letter case and names no tag of
    /// [`VOD_TAGS`]. RFC 8216 §4.1 makes tags case sensitive, so by its text
    /// `#ext-x-key` is a comment. The rewrite refuses it all the same, so that
    /// a player that reads tags in any case meets none outside [`VOD_TAGS`].
    #[error("VOD playlist has a tag the daemon does not emit")]
    UnknownTag,
    /// The attributes of an `EXT-X-MAP` tag do not start with `URI="`.
    #[error("VOD init map must start with its URI")]
    MapWithoutUri,
    /// The URI of an `EXT-X-MAP` tag has no closing quote.
    #[error("VOD init URI has no closing quote")]
    UnterminatedInitUri,
    /// An `EXT-X-MAP` tag holds a second `URI=` after its URI.
    #[error("VOD init map has a second URI")]
    SecondInitUri,
    /// The URI of an `EXT-X-MAP` tag is refused, for the reason the value
    /// gives.
    #[error("invalid VOD init URI")]
    InitUri(#[source] ReferenceError),
    /// A media URI is refused, for the reason the value gives.
    #[error("invalid VOD media URI")]
    MediaUri(#[source] ReferenceError),
    /// The rewritten body would exceed [`MAX_VOD_PLAYLIST_BYTES`].
    #[error("resolved VOD playlist exceeds its byte limit")]
    ResolvedOversized,
    /// The body exceeds [`MAX_VOD_PLAYLIST_BYTES`].
    #[error("VOD playlist exceeds its byte limit")]
    Oversized,
    /// An `#IRIS-FRAGMENT` line has no `DURATION-TICKS` field after its wall
    /// time.
    #[error("invalid VOD fragment timing")]
    FragmentTiming,
    /// An `#IRIS-FRAGMENT` wall time is not a signed 64-bit count of
    /// microseconds.
    #[error("invalid VOD fragment wall time")]
    FragmentWallTime(#[source] ParseIntError),
    /// An `#IRIS-FRAGMENT` duration is not an unsigned 32-bit count of ticks.
    #[error("invalid VOD fragment duration")]
    FragmentDuration(#[source] ParseIntError),
    /// An `#IRIS-FRAGMENT` duration is zero ticks.
    #[error("zero VOD fragment duration")]
    ZeroDuration,
    /// A fragment's end does not fit the wall-clock range.
    #[error("VOD fragment wall time overflows")]
    WallTimeOverflow,
    /// The body holds more than [`MAX_PLAYBACK_FRAGMENTS`] fragments.
    #[error("VOD playlist has too many fragments")]
    TooManyFragments,
    /// The body holds no `#IRIS-FRAGMENT` line.
    #[error("VOD playlist has no fragment timing metadata")]
    NoFragmentTiming,
}

/// Why a reference of a VOD playlist is refused: the source of
/// [`PlaylistError::InitUri`] and [`PlaylistError::MediaUri`]. The example of
/// [`absolute_playlist`] gives one.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Error)]
#[non_exhaustive]
pub enum ReferenceError {
    /// The reference does not resolve against the URL the body came from;
    /// the value says why, in the words of the `url` crate.
    #[error(transparent)]
    Unresolved(#[from] ParseError),
    /// The resolved URL is on another origin: another scheme, host, or
    /// effective port, or an opaque origin such as `file:` or `data:`.
    #[error("VOD playlist references another origin")]
    OtherOrigin,
    /// The resolved URL is on the API origin and carries userinfo: a user
    /// name, a password, or both.
    #[error("VOD playlist reference carries userinfo")]
    Userinfo,
}

/// Whether a line starts with `#EXT` in any letter case. RFC 8216 §4.1 makes
/// tags case sensitive; the check ignores case so that a player that reads a
/// tag in another case meets none outside [`VOD_TAGS`].
fn is_tag(line: &str) -> bool {
    line.as_bytes()
        .get(..4)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case(b"#EXT"))
}

/// Resolves a playlist reference against `base` and requires the result to be
/// on `origin` (scheme, host, and effective port) without userinfo. Opaque
/// origins such as `file:` and `data:` never match. A reference on another
/// origin is refused as such, with or without userinfo.
fn same_origin(base: &Url, reference: &str, origin: &Origin) -> Result<Url, ReferenceError> {
    let absolute = base.join(reference)?;
    if absolute.origin() != *origin {
        return Err(ReferenceError::OtherOrigin);
    }
    if !absolute.username().is_empty() || absolute.password().is_some() {
        return Err(ReferenceError::Userinfo);
    }
    Ok(absolute)
}

/// Rewrites the daemon's VOD body for the player. Init and media references
/// resolve against `base`, the URL the body came from, and become absolute
/// URLs on `origin` with their queries. The body may hold only the tags in
/// [`VOD_TAGS`], and the body and the result each hold at most
/// [`MAX_VOD_PLAYLIST_BYTES`]. A line ends in a line feed, with or without a
/// carriage return before it, as RFC 8216 §4.1 has it. A carriage return
/// anywhere else is refused: the daemon's writer puts none in a line, and a
/// reader that ended a line at it would read lines this function did not
/// check.
///
/// # Errors
///
/// [`PlaylistError::Oversized`] for a body over [`MAX_VOD_PLAYLIST_BYTES`],
/// before any line is read. Otherwise the first line that breaks a rule
/// refuses the body: [`PlaylistError::CarriageReturn`],
/// [`PlaylistError::UnknownTag`],
/// [`PlaylistError::MapWithoutUri`], [`PlaylistError::UnterminatedInitUri`],
/// [`PlaylistError::SecondInitUri`], [`PlaylistError::InitUri`] or
/// [`PlaylistError::MediaUri`] for a reference that does not resolve onto
/// `origin` without userinfo, and [`PlaylistError::ResolvedOversized`].
///
/// # Example
///
/// ```
/// # use std::error::Error;
/// #
/// use iris_client::vod::{self, PlaylistError, ReferenceError};
/// use url::Url;
///
/// // The URL the body came from, on the API origin.
/// let hls = Url::parse("https://nvr.example/api/streams/cam1/main/hls/")?;
/// let base = hls.join("vod.m3u8")?;
/// let origin = base.origin();
/// let body = "#EXTM3U\n#EXTINF:2.0,\n/api/segments/7/file.mp4\n";
/// let resolved = vod::absolute_playlist(body, &base, &origin)?;
/// let media = "\nhttps://nvr.example/api/segments/7/file.mp4\n";
/// assert!(resolved.contains(media));
///
/// // A reference to another origin refuses the playlist.
/// let foreign = "#EXTM3U\n#EXTINF:2.0,\nhttps://elsewhere.example/v.mp4\n";
/// assert_eq!(
///     vod::absolute_playlist(foreign, &base, &origin),
///     Err(PlaylistError::MediaUri(ReferenceError::OtherOrigin))
/// );
/// # Ok::<(), Box<dyn Error>>(())
/// ```
pub fn absolute_playlist(text: &str, base: &Url, origin: &Origin) -> Result<String, PlaylistError> {
    use std::fmt::Write as _;

    // A body within the bound has lines within it, so the rewrite never
    // holds more than the bound and the one line that passes it.
    if text.len() > MAX_VOD_PLAYLIST_BYTES {
        return Err(PlaylistError::Oversized);
    }
    let mut body = String::new();
    for line in text.lines() {
        if line.contains('\r') {
            return Err(PlaylistError::CarriageReturn);
        }
        if is_tag(line) {
            let name = line.split_once(':').map_or(line, |(name, _)| name);
            if !VOD_TAGS.contains(&name) {
                return Err(PlaylistError::UnknownTag);
            }
        }
        if let Some(attributes) = line.strip_prefix("#EXT-X-MAP:") {
            let value = attributes
                .strip_prefix("URI=\"")
                .ok_or(PlaylistError::MapWithoutUri)?;
            let (uri, suffix) = value
                .split_once('"')
                .ok_or(PlaylistError::UnterminatedInitUri)?;
            if suffix.contains("URI=") {
                return Err(PlaylistError::SecondInitUri);
            }
            let absolute = same_origin(base, uri, origin).map_err(PlaylistError::InitUri)?;
            // Writing to a `String` cannot fail.
            let _ = write!(body, "#EXT-X-MAP:URI=\"{absolute}\"{suffix}");
        } else if line.is_empty() || line.starts_with('#') {
            body.push_str(line);
        } else {
            let absolute = same_origin(base, line, origin).map_err(PlaylistError::MediaUri)?;
            body.push_str(absolute.as_str());
        }
        body.push('\n');
        if body.len() > MAX_VOD_PLAYLIST_BYTES {
            return Err(PlaylistError::ResolvedOversized);
        }
    }
    Ok(body)
}

/// The name of the video media playlist beside a multivariant playlist built
/// by [`multivariant_playlist`].
pub const VIDEO_PLAYLIST: &str = "video.m3u8";
/// The name of the audio media playlist beside a multivariant playlist built
/// by [`multivariant_playlist`].
pub const AUDIO_PLAYLIST: &str = "audio.m3u8";

/// The bit rate of a resolved VOD body: the bytes its `EXT-X-BYTERANGE` lines
/// name over the seconds its `EXTINF` lines name, at least one.
fn bits_per_second(body: &str) -> u64 {
    let mut bytes = 0_u64;
    let mut seconds = 0.0_f64;
    for line in body.lines() {
        if let Some(range) = line.strip_prefix("#EXT-X-BYTERANGE:") {
            let len = range.split('@').next().unwrap_or(range);
            let size: u64 = len.parse().unwrap_or(0);
            bytes = bytes.saturating_add(size);
        } else if let Some(duration) = line.strip_prefix("#EXTINF:") {
            let value = duration.split(',').next().unwrap_or(duration);
            let duration_secs: f64 = value.parse().unwrap_or(0.0);
            if duration_secs.is_finite() {
                seconds += duration_secs;
            }
        }
    }
    if seconds <= 0.0 {
        return 1;
    }
    // The counts come from the body, so the rate the `BANDWIDTH` attribute
    // names is clamped to at least one bit a second and at most
    // `u64::MAX as f64`. That top is 2^64, one past the most a `u64` holds.
    // The cast truncates a rate below it and saturates 2^64 itself to
    // `u64::MAX`. `seconds` is positive here, so the rate is never NaN.
    (bytes as f64 * 8.0 / seconds).clamp(1.0, u64::MAX as f64) as u64
}

/// A multivariant playlist that plays the resolved VOD body `video` with the
/// resolved VOD body `audio` as its one audio rendition
/// (`docs/research/13-audio.md` §8). The two media playlists are named
/// [`VIDEO_PLAYLIST`] and [`AUDIO_PLAYLIST`], relative to wherever the
/// multivariant playlist is served.
///
/// The `BANDWIDTH` it writes is the sum of the average bit rates of the two
/// bodies, and it writes no `CODECS` and no `CHANNELS`. RFC 8216 §4.3.4.2
/// says that once every media segment exists, "the BANDWIDTH value MUST be
/// the largest sum of peak segment bit rates that is produced by any playable
/// combination of Renditions". It also says that every `EXT-X-STREAM-INF` tag
/// SHOULD include a `CODECS` attribute. The Linux viewer plays this playlist
/// (`docs/research/13-audio.md` §6). Whether any other player plays it is
/// **unverified**.
///
/// # Example
///
/// ```
/// use iris_client::vod;
///
/// // Two seconds of video in 250,000 bytes, and of audio in 16,000.
/// let video = "#EXTINF:2.0,\n#EXT-X-BYTERANGE:250000@0\nhttps://h.example/\n";
/// let audio = "#EXTINF:2.0,\n#EXT-X-BYTERANGE:16000@0\nhttps://h.example/\n";
/// let entry = vod::multivariant_playlist(video, audio);
/// // The player fetches the two by these names, beside the entry.
/// assert!(entry.contains("URI=\"audio.m3u8\""));
/// assert!(entry.ends_with("\nvideo.m3u8\n"));
/// // 266,000 bytes over two seconds: 1,064,000 bits a second.
/// assert!(entry.contains("BANDWIDTH=1064000,"));
/// ```
#[must_use]
pub fn multivariant_playlist(video: &str, audio: &str) -> String {
    format!(
        "#EXTM3U\n#EXT-X-VERSION:7\n\
         #EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"audio\",NAME=\"camera\",LANGUAGE=\"und\",\
         DEFAULT=YES,AUTOSELECT=YES,URI=\"{AUDIO_PLAYLIST}\"\n\
         #EXT-X-STREAM-INF:BANDWIDTH={},AUDIO=\"audio\"\n{VIDEO_PLAYLIST}\n",
        bits_per_second(video).saturating_add(bits_per_second(audio))
    )
}

/// The `#IRIS-FRAGMENT:WALL-US=<start>,DURATION-TICKS=<ticks>` timing of a VOD
/// body in served order: at least one fragment and at most
/// [`MAX_PLAYBACK_FRAGMENTS`], each with a positive duration whose end fits the
/// wall-clock range.
///
/// # Errors
///
/// [`PlaylistError::Oversized`] for a body over [`MAX_VOD_PLAYLIST_BYTES`];
/// for the first timing line that breaks a rule,
/// [`PlaylistError::FragmentTiming`], [`PlaylistError::FragmentWallTime`],
/// [`PlaylistError::FragmentDuration`], [`PlaylistError::ZeroDuration`],
/// [`PlaylistError::WallTimeOverflow`], or
/// [`PlaylistError::TooManyFragments`]; [`PlaylistError::NoFragmentTiming`]
/// for a body without timing.
///
/// # Example
///
/// ```
/// use iris_client::vod::{self, PlaylistError};
/// use iris_core::time::{MediaTicks, WallUs};
///
/// let body = "#EXTM3U\n#IRIS-FRAGMENT:WALL-US=1000,DURATION-TICKS=180000\n";
/// let fragments = vod::playlist_fragments(body)?;
/// assert_eq!(fragments[0].wall_start, WallUs::new(1000));
/// // Two seconds of the 90 kHz media timescale.
/// assert_eq!(fragments[0].duration, MediaTicks::new(180_000));
///
/// // A body without timing gives no fragments to map through.
/// let untimed = vod::playlist_fragments("#EXTM3U\n");
/// assert_eq!(untimed, Err(PlaylistError::NoFragmentTiming));
/// # Ok::<(), PlaylistError>(())
/// ```
pub fn playlist_fragments(text: &str) -> Result<Vec<PlaybackFragment>, PlaylistError> {
    if text.len() > MAX_VOD_PLAYLIST_BYTES {
        return Err(PlaylistError::Oversized);
    }
    let mut fragments = Vec::new();
    for line in text.lines() {
        let Some(fields) = line.strip_prefix("#IRIS-FRAGMENT:WALL-US=") else {
            continue;
        };
        let (wall, ticks) = fields
            .split_once(",DURATION-TICKS=")
            .ok_or(PlaylistError::FragmentTiming)?;
        let wall_start: i64 = wall.parse().map_err(PlaylistError::FragmentWallTime)?;
        let ticks: u32 = ticks.parse().map_err(PlaylistError::FragmentDuration)?;
        if ticks == 0 {
            return Err(PlaylistError::ZeroDuration);
        }
        let duration = MediaTicks::new(i64::from(ticks));
        duration
            .checked_as_micros()
            .and_then(|micros| wall_start.checked_add(micros))
            .ok_or(PlaylistError::WallTimeOverflow)?;
        if fragments.len() >= MAX_PLAYBACK_FRAGMENTS {
            return Err(PlaylistError::TooManyFragments);
        }
        fragments.push(PlaybackFragment {
            wall_start: WallUs::new(wall_start),
            duration,
        });
    }
    if fragments.is_empty() {
        return Err(PlaylistError::NoFragmentTiming);
    }
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

    /// The first 80 characters of `playlist`, which a failure names: a body
    /// at the byte limit would print whole otherwise.
    fn shown(playlist: &str) -> String {
        playlist.chars().take(80).collect()
    }

    /// Checks that the rewrite refuses `playlist` with `error`, whose text is
    /// `text`; a failure names the line that calls it.
    #[track_caller]
    fn assert_rewrite_refuses(playlist: &str, error: &PlaylistError, text: &str) {
        let (base, origin) = vod_base();
        let refused = absolute_playlist(playlist, &base, &origin).err();
        assert_eq!(refused.as_ref(), Some(error), "{:?}", shown(playlist));
        assert_eq!(
            refused.map(|refusal| refusal.to_string()).as_deref(),
            Some(text),
            "{:?}",
            shown(playlist)
        );
    }

    /// Checks that the timing reader refuses `playlist` with `error`, whose
    /// text is `text`; a failure names the line that calls it.
    #[track_caller]
    fn assert_timing_refuses(playlist: &str, error: &PlaylistError, text: &str) {
        let refused = playlist_fragments(playlist).err();
        assert_eq!(refused.as_ref(), Some(error), "{:?}", shown(playlist));
        assert_eq!(
            refused.map(|refusal| refusal.to_string()).as_deref(),
            Some(text),
            "{:?}",
            shown(playlist)
        );
    }

    /// The timing line of a fragment one tick long.
    const TIMING_LINE: &str = "#IRIS-FRAGMENT:WALL-US=0,DURATION-TICKS=1\n";

    /// `first`, then a comment that fills the body to `len` bytes, which the
    /// rewrite passes through as it stands.
    fn filled(first: &str, len: usize) -> String {
        let filler = "-".repeat(len - first.len() - 2);
        let body = format!("{first}#{filler}\n");
        assert_eq!(body.len(), len, "the fixture fills {len} bytes");
        body
    }

    /// A body of exactly `MAX_VOD_PLAYLIST_BYTES`: a fragment's timing and a
    /// comment that fills the rest.
    fn at_the_byte_limit() -> String {
        filled(TIMING_LINE, MAX_VOD_PLAYLIST_BYTES)
    }

    /// A body one byte past `MAX_VOD_PLAYLIST_BYTES`.
    fn past_the_byte_limit() -> String {
        filled(TIMING_LINE, MAX_VOD_PLAYLIST_BYTES + 1)
    }

    /// References on another origin than the fixture's: another host, port,
    /// or scheme, an opaque `file:` or `data:` URL, and another host with
    /// userinfo.
    const FOREIGN: [&str; 8] = [
        "//192.0.2.20",
        "https://192.0.2.20/file.mp4?token=other",
        "//192.0.2.20/file.mp4",
        "https://192.0.2.10:8443/file.mp4",
        "http://192.0.2.10/file.mp4",
        "file:///etc/hostname",
        "data:video/mp4;base64,AAAA",
        "https://user:secret@192.0.2.20/file.mp4",
    ];

    /// References on the fixture's origin with userinfo: a user name, and a
    /// user name with a password.
    const WITH_USERINFO: [&str; 2] = [
        "https://user@192.0.2.10/file.mp4",
        "https://user:secret@192.0.2.10/file.mp4",
    ];

    /// A boundary GOP's whole two seconds, as the daemon serves them.
    #[test]
    fn a_timing_line_gives_a_fragment_at_its_wall_start_lasting_its_ticks() {
        assert_eq!(
            playlist_fragments("#IRIS-FRAGMENT:WALL-US=100,DURATION-TICKS=180000\n"),
            Ok(vec![fragment(100, 180_000)])
        );
    }

    /// The later fragment listed first stays first.
    #[test]
    fn fragment_timing_keeps_the_served_order() {
        let fragments = playlist_fragments(concat!(
            "#EXTM3U\n",
            "#IRIS-FRAGMENT:WALL-US=9000000,DURATION-TICKS=3001\n",
            "#IRIS-FRAGMENT:WALL-US=100,DURATION-TICKS=180000\n",
        ));
        assert_eq!(
            fragments,
            Ok(vec![fragment(9_000_000, 3001), fragment(100, 180_000)])
        );
    }

    #[test]
    fn resolving_an_init_reference_adds_the_origin_alone() {
        let (base, origin) = vod_base();
        let map = concat!(
            "#EXT-X-MAP:URI=\"/api/segments/1/file.mp4?token=encoded%26token\",",
            "BYTERANGE=\"100@0\"\n"
        );
        let resolved = concat!(
            "#EXT-X-MAP:URI=\"https://192.0.2.10/api/segments/1/file.mp4?token=encoded%26token\",",
            "BYTERANGE=\"100@0\"\n"
        );
        assert_eq!(
            absolute_playlist(map, &base, &origin).as_deref(),
            Ok(resolved)
        );
    }

    #[test]
    fn a_relative_media_reference_resolves_against_the_response_url_with_its_query() {
        let (base, origin) = vod_base();
        let reference = "../media/file.mp4?token=encoded%26token\n";
        let resolved =
            "https://192.0.2.10/api/streams/fixture/main/media/file.mp4?token=encoded%26token\n";
        assert_eq!(
            absolute_playlist(reference, &base, &origin).as_deref(),
            Ok(resolved)
        );
    }

    /// A reference on the origin that names the default port.
    #[test]
    fn an_absolute_media_reference_on_the_origin_keeps_its_query() {
        let (base, origin) = vod_base();
        let reference = "https://192.0.2.10:443/api/segments/2/file.mp4?token=same\n";
        assert_eq!(
            absolute_playlist(reference, &base, &origin).as_deref(),
            Ok("https://192.0.2.10/api/segments/2/file.mp4?token=same\n")
        );
    }

    /// The tags the daemon emits, the byte range among them, and the timing
    /// comment. The map, which carries a URI, is left out.
    #[test]
    fn the_tags_the_daemon_emits_pass_through_as_they_stand() {
        let (base, origin) = vod_base();
        let tags = concat!(
            "#EXTM3U\n#EXT-X-VERSION:7\n#EXT-X-TARGETDURATION:1\n#EXT-X-MEDIA-SEQUENCE:0\n",
            "#EXT-X-PLAYLIST-TYPE:VOD\n#EXT-X-INDEPENDENT-SEGMENTS\n",
            "#IRIS-FRAGMENT:WALL-US=10,DURATION-TICKS=90000\n#EXTINF:1.00000,\n",
            "#EXT-X-BYTERANGE:200@100\n#EXT-X-DISCONTINUITY\n#EXT-X-ENDLIST\n"
        );
        assert_eq!(absolute_playlist(tags, &base, &origin).as_deref(), Ok(tags));
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

    /// A carriage return alone, one between two tags, and one at the end of
    /// the body's last line, where no line feed follows it. A reader that
    /// ended a line at each would read lines the rewrite did not check.
    #[test]
    fn a_carriage_return_inside_a_line_is_refused() {
        for playlist in [
            "\r",
            "#EXTM3U\r#EXT-X-ENDLIST\n",
            "#2-\n#IRIS-FRAGMENT:WALL-US=17903,DURATION-TICKS=3636\r",
        ] {
            assert_rewrite_refuses(
                playlist,
                &PlaylistError::CarriageReturn,
                "VOD playlist has a carriage return inside a line",
            );
        }
    }

    /// A carriage return left at the end of the body's last line is part of
    /// that line, whose duration then does not parse: what the rewrite
    /// would hide by ending the line there.
    #[test]
    fn a_carriage_return_after_the_last_duration_is_part_of_it() {
        let reason = "3636\r".parse::<u32>().unwrap_err();
        assert_eq!(
            playlist_fragments("#2-\n#IRIS-FRAGMENT:WALL-US=17903,DURATION-TICKS=3636\r"),
            Err(PlaylistError::FragmentDuration(reason))
        );
    }

    #[test]
    fn a_crlf_line_ending_is_rewritten_as_a_line_feed() {
        let (base, origin) = vod_base();
        let crlf = "#EXTM3U\r\n#IRIS-FRAGMENT:WALL-US=17903,DURATION-TICKS=3636\r\n";
        assert_eq!(
            absolute_playlist(crlf, &base, &origin).unwrap(),
            "#EXTM3U\n#IRIS-FRAGMENT:WALL-US=17903,DURATION-TICKS=3636\n"
        );
    }

    #[test]
    fn the_timing_of_a_crlf_line_is_read() {
        let crlf = "#EXTM3U\r\n#IRIS-FRAGMENT:WALL-US=17903,DURATION-TICKS=3636\r\n";
        assert_eq!(playlist_fragments(crlf), Ok(vec![fragment(17_903, 3636)]));
    }

    /// The shortest line taken for a tag, `#EXT`, and tags of HLS the daemon
    /// does not write, in any letter case, those that carry a URI among them.
    /// Each is tried as a playlist of its line alone, and inside a playlist.
    #[test]
    fn playlists_carry_only_the_tags_the_daemon_emits() {
        for tag in [
            "#EXT",
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
            let inside =
                format!("#EXTM3U\n{tag}\n#EXTINF:1.0,\n/api/segments/1/file.mp4\n#EXT-X-ENDLIST\n");
            for playlist in [tag, inside.as_str()] {
                assert_rewrite_refuses(
                    playlist,
                    &PlaylistError::UnknownTag,
                    "VOD playlist has a tag the daemon does not emit",
                );
            }
        }
    }

    /// A map without attributes, one whose first attribute is not its URI,
    /// and one whose URI is not quoted.
    #[test]
    fn an_init_map_that_does_not_start_with_its_uri_is_refused() {
        for map in [
            "#EXT-X-MAP:",
            "#EXT-X-MAP:BYTERANGE=\"1@0\",URI=\"/api/segments/1/file.mp4\"",
            "#EXT-X-MAP:URI=/api/segments/1/file.mp4",
        ] {
            // The line alone, and the line after the playlist's header.
            for playlist in [map.to_owned(), format!("#EXTM3U\n{map}\n")] {
                assert_rewrite_refuses(
                    &playlist,
                    &PlaylistError::MapWithoutUri,
                    "VOD init map must start with its URI",
                );
            }
        }
    }

    #[test]
    fn an_init_uri_without_its_closing_quote_is_refused() {
        for map in [
            "#EXT-X-MAP:URI=\"",
            "#EXT-X-MAP:URI=\"/api/segments/1/file.mp4",
        ] {
            // The line alone, and the line after the playlist's header.
            for playlist in [map.to_owned(), format!("#EXTM3U\n{map}\n")] {
                assert_rewrite_refuses(
                    &playlist,
                    &PlaylistError::UnterminatedInitUri,
                    "VOD init URI has no closing quote",
                );
            }
        }
    }

    #[test]
    fn an_init_map_with_a_second_uri_is_refused() {
        for map in [
            "#EXT-X-MAP:URI=\"\"URI=",
            "#EXT-X-MAP:URI=\"/api/segments/1/file.mp4\",URI=\"https://192.0.2.20/x\"",
        ] {
            // The line alone, and the line after the playlist's header.
            for playlist in [map.to_owned(), format!("#EXTM3U\n{map}\n")] {
                assert_rewrite_refuses(
                    &playlist,
                    &PlaylistError::SecondInitUri,
                    "VOD init map has a second URI",
                );
            }
        }
    }

    #[test]
    fn an_init_uri_on_another_origin_is_refused() {
        for uri in FOREIGN {
            assert_rewrite_refuses(
                &format!("#EXT-X-MAP:URI=\"{uri}\""),
                &PlaylistError::InitUri(ReferenceError::OtherOrigin),
                "invalid VOD init URI",
            );
        }
    }

    #[test]
    fn an_init_uri_with_userinfo_is_refused() {
        for uri in WITH_USERINFO {
            assert_rewrite_refuses(
                &format!("#EXT-X-MAP:URI=\"{uri}\""),
                &PlaylistError::InitUri(ReferenceError::Userinfo),
                "invalid VOD init URI",
            );
        }
    }

    #[test]
    fn an_init_uri_that_does_not_resolve_is_refused() {
        assert_rewrite_refuses(
            "#EXT-X-MAP:URI=\"//192.0.2.10:99999\"",
            &PlaylistError::InitUri(ReferenceError::Unresolved(ParseError::InvalidPort)),
            "invalid VOD init URI",
        );
    }

    #[test]
    fn a_media_uri_on_another_origin_is_refused() {
        for uri in FOREIGN {
            assert_rewrite_refuses(
                uri,
                &PlaylistError::MediaUri(ReferenceError::OtherOrigin),
                "invalid VOD media URI",
            );
        }
    }

    #[test]
    fn a_media_uri_with_userinfo_is_refused() {
        for uri in WITH_USERINFO {
            assert_rewrite_refuses(
                uri,
                &PlaylistError::MediaUri(ReferenceError::Userinfo),
                "invalid VOD media URI",
            );
        }
    }

    /// The fixture's origin with a password and no user name.
    #[test]
    fn a_reference_whose_userinfo_is_a_password_alone_is_refused() {
        assert_rewrite_refuses(
            "https://:secret@192.0.2.10/file.mp4",
            &PlaylistError::MediaUri(ReferenceError::Userinfo),
            "invalid VOD media URI",
        );
    }

    #[test]
    fn a_media_uri_that_does_not_resolve_is_refused() {
        assert_rewrite_refuses(
            "//192.0.2.10:99999",
            &PlaylistError::MediaUri(ReferenceError::Unresolved(ParseError::InvalidPort)),
            "invalid VOD media URI",
        );
    }

    #[test]
    fn a_rewrite_that_fills_the_byte_limit_is_kept_whole() {
        let (base, origin) = vod_base();
        assert_eq!(
            absolute_playlist(&at_the_byte_limit(), &base, &origin).map(|body| body.len()),
            Ok(MAX_VOD_PLAYLIST_BYTES)
        );
    }

    /// A body one byte past the limit whose first line is a tag the daemon
    /// does not emit: the bound refuses it before that line is read.
    #[test]
    fn a_body_past_the_byte_limit_is_refused_before_any_line_is_rewritten() {
        assert_rewrite_refuses(
            &filled("#EXT-X-KEY:METHOD=NONE\n", MAX_VOD_PLAYLIST_BYTES + 1),
            &PlaylistError::Oversized,
            "VOD playlist exceeds its byte limit",
        );
    }

    /// A body at the limit whose last line is a relative reference, which
    /// the rewrite makes longer.
    #[test]
    fn a_rewrite_that_grows_past_the_byte_limit_is_refused() {
        let comment = filled("", MAX_VOD_PLAYLIST_BYTES - "a\n".len());
        assert_rewrite_refuses(
            &format!("{comment}a\n"),
            &PlaylistError::ResolvedOversized,
            "resolved VOD playlist exceeds its byte limit",
        );
    }

    /// A body within the limit whose last line is a relative reference. The
    /// rewrite of that line and its line feed end one byte past the limit.
    #[test]
    fn a_rewrite_one_byte_past_the_byte_limit_is_refused() {
        let resolved = vod_base().0.join("a").unwrap();
        let comment = filled("", MAX_VOD_PLAYLIST_BYTES - resolved.as_str().len());
        assert_rewrite_refuses(
            &format!("{comment}a\n"),
            &PlaylistError::ResolvedOversized,
            "resolved VOD playlist exceeds its byte limit",
        );
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

    #[test]
    fn the_timing_of_a_body_that_fills_the_byte_limit_is_read() {
        assert_eq!(
            playlist_fragments(&at_the_byte_limit()).map(|fragments| fragments.len()),
            Ok(1)
        );
    }

    #[test]
    fn a_body_past_the_byte_limit_is_refused_before_its_timing_is_read() {
        assert_timing_refuses(
            &past_the_byte_limit(),
            &PlaylistError::Oversized,
            "VOD playlist exceeds its byte limit",
        );
    }

    #[test]
    fn a_timing_line_without_its_duration_is_refused() {
        assert_timing_refuses(
            "#IRIS-FRAGMENT:WALL-US=",
            &PlaylistError::FragmentTiming,
            "invalid VOD fragment timing",
        );
    }

    /// A wall time without digits, and one of a character that is no digit:
    /// neither is a signed 64-bit count of microseconds.
    #[test]
    fn a_wall_time_that_is_not_a_count_of_microseconds_is_refused() {
        for wall in ["", "x"] {
            let reason = wall.parse::<i64>().unwrap_err();
            assert_timing_refuses(
                &format!("#IRIS-FRAGMENT:WALL-US={wall},DURATION-TICKS=1"),
                &PlaylistError::FragmentWallTime(reason),
                "invalid VOD fragment wall time",
            );
        }
    }

    /// A duration without digits, one below zero, and one past the range of
    /// `u32`.
    #[test]
    fn a_duration_that_is_not_an_unsigned_32_bit_count_of_ticks_is_refused() {
        for ticks in ["", "-1", "4294967296"] {
            let reason = ticks.parse::<u32>().unwrap_err();
            assert_timing_refuses(
                &format!("#IRIS-FRAGMENT:WALL-US=0,DURATION-TICKS={ticks}"),
                &PlaylistError::FragmentDuration(reason),
                "invalid VOD fragment duration",
            );
        }
    }

    #[test]
    fn a_duration_of_the_most_ticks_a_u32_holds_is_read() {
        assert_eq!(
            playlist_fragments("#IRIS-FRAGMENT:WALL-US=0,DURATION-TICKS=4294967295"),
            Ok(vec![fragment(0, 4_294_967_295)])
        );
    }

    #[test]
    fn a_duration_of_zero_ticks_is_refused() {
        assert_timing_refuses(
            "#IRIS-FRAGMENT:WALL-US=0,DURATION-TICKS=0",
            &PlaylistError::ZeroDuration,
            "zero VOD fragment duration",
        );
    }

    /// A fragment that starts 11 µs before the end of the wall clock and
    /// lasts one tick, 11.1 µs rounded down: it ends at the last microsecond.
    #[test]
    fn a_fragment_that_ends_at_the_last_microsecond_is_read() {
        let start = i64::MAX - 11;
        assert_eq!(
            playlist_fragments(&format!("#IRIS-FRAGMENT:WALL-US={start},DURATION-TICKS=1")),
            Ok(vec![fragment(start, 1)])
        );
    }

    /// A fragment that starts at the last microsecond of the wall clock and
    /// lasts a tick, or a second.
    #[test]
    fn a_fragment_that_ends_past_the_wall_clock_is_refused() {
        for ticks in [1, 90_000] {
            assert_timing_refuses(
                &format!("#IRIS-FRAGMENT:WALL-US={},DURATION-TICKS={ticks}", i64::MAX),
                &PlaylistError::WallTimeOverflow,
                "VOD fragment wall time overflows",
            );
        }
    }

    #[test]
    fn as_many_fragments_as_the_limit_are_read() {
        assert_eq!(
            playlist_fragments(&TIMING_LINE.repeat(MAX_PLAYBACK_FRAGMENTS))
                .map(|fragments| fragments.len()),
            Ok(MAX_PLAYBACK_FRAGMENTS)
        );
    }

    #[test]
    fn more_fragments_than_the_limit_are_refused() {
        assert_timing_refuses(
            &TIMING_LINE.repeat(MAX_PLAYBACK_FRAGMENTS + 1),
            &PlaylistError::TooManyFragments,
            "VOD playlist has too many fragments",
        );
    }

    /// An empty body, and a playlist without an `#IRIS-FRAGMENT` line. The
    /// text of `NotUtf8`, which the answer check gives, is checked by
    /// `a_playlist_that_is_not_utf8_is_refused_with_its_text`
    /// (`src/api/answer.rs`).
    #[test]
    fn a_body_without_fragment_timing_is_refused() {
        for body in ["", "#EXTM3U\n#EXT-X-ENDLIST\n"] {
            assert_timing_refuses(
                body,
                &PlaylistError::NoFragmentTiming,
                "VOD playlist has no fragment timing metadata",
            );
        }
    }

    /// The five lines of the local multivariant playlist. Its bit rate is
    /// that of both bodies: 2,000 bytes over 4 s of video and 500 bytes over
    /// 4 s of audio, 5,000 bits a second.
    #[test]
    fn the_multivariant_playlist_has_its_five_lines() {
        let video = concat!(
            "#EXTM3U\n#EXTINF:2.00000,\n#EXT-X-BYTERANGE:1500@700\nhttp://192.0.2.10/v.mp4\n",
            "#EXTINF:2.00000,\n#EXT-X-BYTERANGE:500@2200\nhttp://192.0.2.10/v.mp4\n",
        );
        let audio = concat!(
            "#EXTM3U\n#EXTINF:4.00000,\n#EXT-X-BYTERANGE:500@600\n",
            "http://192.0.2.10/a.mp4\n",
        );
        assert_eq!(
            multivariant_playlist(video, audio),
            "#EXTM3U\n#EXT-X-VERSION:7\n\
             #EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"audio\",NAME=\"camera\",LANGUAGE=\"und\",\
             DEFAULT=YES,AUTOSELECT=YES,URI=\"audio.m3u8\"\n\
             #EXT-X-STREAM-INF:BANDWIDTH=5000,AUDIO=\"audio\"\nvideo.m3u8\n"
        );
    }

    /// Each body gives a rate of one bit a second at least, so the two give
    /// two.
    #[test]
    fn bodies_without_durations_or_sizes_still_declare_a_positive_rate() {
        assert!(multivariant_playlist("#EXTM3U\n", "#EXTINF:x,\n").contains("BANDWIDTH=2,"));
    }

    /// The most bytes a `u64` counts over a microsecond: the clamp's top,
    /// 2^64, saturates to `u64::MAX` in the cast.
    #[test]
    fn a_bit_rate_past_the_most_a_u64_holds_is_u64_max() {
        let fast = format!("#EXTINF:0.000001,\n#EXT-X-BYTERANGE:{}@0\n", u64::MAX);
        assert_eq!(bits_per_second(&fast), u64::MAX);
    }

    /// A duration that is not a number, or is infinite, beside two seconds
    /// over 100 bytes: the rate is that of the two seconds alone.
    #[test]
    fn a_duration_that_is_not_finite_adds_no_seconds() {
        for duration in ["NaN", "inf", "-inf"] {
            let body = format!("#EXTINF:{duration},\n#EXTINF:2.0,\n#EXT-X-BYTERANGE:100@0\n");
            assert_eq!(bits_per_second(&body), 400, "{duration}");
        }
    }

    /// No byte over four seconds, and one byte over a hundred.
    #[test]
    fn a_bit_rate_below_one_bit_a_second_is_one() {
        assert_eq!(bits_per_second("#EXTINF:4.0,\n#EXT-X-BYTERANGE:0@0\n"), 1);
        assert_eq!(bits_per_second("#EXTINF:100.0,\n#EXT-X-BYTERANGE:1@0\n"), 1);
    }

    /// A refusal as a viewer prints it: its text, then each source's after a
    /// colon.
    fn chain(error: &PlaylistError) -> String {
        use std::error::Error as _;

        let mut text = error.to_string();
        let mut source = error.source();
        while let Some(cause) = source {
            text = format!("{text}: {cause}");
            source = cause.source();
        }
        text
    }

    /// A refused reference shows its line's message and then its reason.
    #[test]
    fn a_refused_reference_shows_its_message_then_its_reason() {
        let (base, origin) = vod_base();
        for (playlist, text) in [
            (
                "#EXT-X-MAP:URI=\"https://192.0.2.20/init.mp4\"\n",
                "invalid VOD init URI: VOD playlist references another origin",
            ),
            (
                "file:///etc/hostname\n",
                "invalid VOD media URI: VOD playlist references another origin",
            ),
            (
                "https://user@192.0.2.10/file.mp4\n",
                "invalid VOD media URI: VOD playlist reference carries userinfo",
            ),
            (
                "https://192.0.2.10:99999/file.mp4\n",
                "invalid VOD media URI: invalid port number",
            ),
        ] {
            let refused = absolute_playlist(playlist, &base, &origin).unwrap_err();
            assert_eq!(chain(&refused), text, "{playlist}");
        }
    }

    #[test]
    fn a_refused_timing_line_shows_its_message_then_its_reason() {
        let timing = playlist_fragments("#IRIS-FRAGMENT:WALL-US=x,DURATION-TICKS=1").unwrap_err();
        assert_eq!(
            chain(&timing),
            "invalid VOD fragment wall time: invalid digit found in string"
        );
    }

    /// A refusal without a reason shows the message of the rule it breaks
    /// alone.
    #[test]
    fn a_refusal_without_a_reason_shows_its_message_alone() {
        let (base, origin) = vod_base();
        for (playlist, text) in [
            (
                "#EXT-X-MAP:URI=\"/api/segments/1/file.mp4\n",
                "VOD init URI has no closing quote",
            ),
            (
                "#EXT-X-KEY:METHOD=NONE\n",
                "VOD playlist has a tag the daemon does not emit",
            ),
        ] {
            let refused = absolute_playlist(playlist, &base, &origin).unwrap_err();
            assert_eq!(chain(&refused), text, "{playlist}");
        }
    }
}
