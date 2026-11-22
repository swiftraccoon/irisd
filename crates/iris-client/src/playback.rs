//! A recorded window as a viewer plays it, apart from how the viewer hands it
//! to its player. The module holds the playlists of one selection and the
//! bodies a player is handed under fixed names. It also gives the times of a
//! strip of thumbnails along the window and the file name of an export.
//!
//! A viewer fetches a window's VOD playlist once, with the audio playlist of a
//! main stream beside it, and checks each
//! ([`answer::vod_playlist`](crate::api::answer::vod_playlist)). A
//! [`PlaybackSelection`] holds them: the fragments its timeline maps through
//! and the bodies its player plays, so the two read one selection. Its
//! [`PlaybackSnapshot`] names each body. The player opens
//! [`PlaybackSnapshot::ENTRY`]: the video playlist, or, with audio, a
//! multivariant playlist that names the two media playlists beside it,
//! [`vod::VIDEO_PLAYLIST`] and [`vod::AUDIO_PLAYLIST`]. The Linux viewer serves
//! each body under its name on a loopback endpoint; a viewer whose player asks
//! a resource loader for each name answers it with [`PlaybackSnapshot::body`].
//!
//! # Example
//!
//! ```
//! use std::error::Error;
//!
//! use iris_client::api::{Answer, Segment, VodPlaylist, Window, answer};
//! use iris_client::origin::{ApiOrigin, PlainHttp};
//! use iris_client::playback::{self, PlaybackSelection, ThumbnailTime};
//! use iris_client::vod;
//! use iris_core::time::WallUs;
//!
//! /// The `kind` playlist of `window`, two seconds of segment `segment`, as
//! /// the check of its answer gives it.
//! fn checked(
//!     origin: &ApiOrigin,
//!     window: Window,
//!     kind: &str,
//!     segment: u32,
//! ) -> Result<VodPlaylist, Box<dyn Error>> {
//!     let path = format!("/api/streams/cam1/{kind}/hls/vod.m3u8");
//!     let url = origin.url().join(&path)?;
//!     let body = format!(
//!         "#EXTM3U\n\
//!          #IRIS-FRAGMENT:WALL-US=0,DURATION-TICKS=180000\n\
//!          #EXTINF:2.00000,\n\
//!          /api/segments/{segment}/file.mp4\n#EXT-X-ENDLIST\n"
//!     );
//!     let received = Answer {
//!         status: 200,
//!         content_type: Some("application/vnd.apple.mpegurl"),
//!         final_url: &url,
//!         body: Some(body.as_bytes()),
//!     };
//!     Ok(answer::vod_playlist(origin, &received, window)?)
//! }
//!
//! let api = "https://nvr.example";
//! let origin = ApiOrigin::parse(api, PlainHttp::LoopbackOnly)?;
//! let window = Window::try_new(WallUs::new(0), WallUs::new(60_000_000))?;
//! let video = checked(&origin, window, "main", 1)?;
//! let audio = checked(&origin, window, "audio", 2)?;
//! let selection = PlaybackSelection::new(video, Some(audio));
//! let (fragments, snapshot) = selection.into_parts();
//! // The timeline maps through the video's fragments.
//! assert_eq!(fragments.len(), 1);
//! assert!(snapshot.has_audio());
//! // The player opens the entry, `vod.m3u8`, which names the audio playlist
//! // beside it.
//! let entry = snapshot.body("vod.m3u8").ok_or("the entry")?;
//! assert!(entry.contains("URI=\"audio.m3u8\""));
//! let audio = snapshot.body(vod::AUDIO_PLAYLIST).ok_or("the audio")?;
//! assert!(audio.contains("https://nvr.example/api/segments/2/"));
//! assert_eq!(snapshot.body("../audio.m3u8"), None);
//! let names: Vec<&str> = snapshot.iter().map(|(name, _)| name).collect();
//! assert_eq!(names, ["vod.m3u8", "video.m3u8", "audio.m3u8"]);
//! // A viewer that serves the bodies takes each one out of the snapshot.
//! for (name, body) in snapshot {
//!     assert!(body.starts_with("#EXTM3U\n"), "{name}");
//! }
//!
//! // Four thumbnails over a minute of which the first half is recorded.
//! let recorded = [Segment {
//!     start: WallUs::new(0),
//!     end: WallUs::new(30_000_000),
//!     sealed: true,
//! }];
//! let times = playback::thumbnail_times(window, &recorded, 4);
//! let middles = [
//!     ThumbnailTime {
//!         slot: 0,
//!         time: WallUs::new(7_500_000),
//!     },
//!     ThumbnailTime {
//!         slot: 1,
//!         time: WallUs::new(22_500_000),
//!     },
//! ];
//! assert_eq!(times, middles);
//! # Ok::<(), Box<dyn Error>>(())
//! ```

use std::{fmt, iter, vec};

use iris_core::id::{CameraId, StreamKind};
use iris_core::time::WallUs;

use crate::api::{Segment, VodPlaylist, Window};
use crate::vod::{self, PlaybackFragment};

/// One recorded window's playlists as a viewer fetched them, once. The
/// selection holds the video playlist's fragments, which its timeline maps
/// through, and the bodies its player plays. Those are the video playlist's
/// and, for a main stream with recorded audio, the audio playlist's. The
/// timeline and the player read one selection, so a recording appended or
/// removed between two fetches cannot set them apart. Its `Debug` output shows
/// the count of fragments and the length of each body, never a body, whose
/// references carry media capabilities. The example of the [module](self)
/// makes one.
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct PlaybackSelection {
    /// The video playlist's fragments in served order.
    fragments: Vec<PlaybackFragment>,
    /// The video playlist's body.
    video: String,
    /// The audio playlist's body, for a window that plays with sound.
    audio: Option<String>,
}

impl PlaybackSelection {
    /// The selection of `video`, played with `audio` when there is one: each
    /// as the check of its answer gives it, its timing read and its references
    /// absolute on the API origin. The example of the [module](self) makes
    /// one.
    #[must_use]
    pub fn new(video: VodPlaylist, audio: Option<VodPlaylist>) -> Self {
        let (fragments, video) = video.into_parts();
        Self {
            fragments,
            video,
            audio: audio.map(|audio| audio.into_parts().1),
        }
    }

    /// The video playlist's fragments in served order, which the timeline
    /// maps through, and the bodies a player is handed, each under its name,
    /// all moved out of the selection. The audio playlist's timing is not
    /// kept: its sound lies on the video's timeline. Without audio the video
    /// playlist is the entry, [`PlaybackSnapshot::ENTRY`]; with audio the
    /// entry is the multivariant playlist of the two
    /// ([`vod::multivariant_playlist`]), the video playlist is
    /// [`vod::VIDEO_PLAYLIST`], and the audio playlist [`vod::AUDIO_PLAYLIST`].
    /// The example of the [module](self) takes both.
    #[must_use]
    pub fn into_parts(self) -> (Vec<PlaybackFragment>, PlaybackSnapshot) {
        let snapshot = match self.audio {
            None => PlaybackSnapshot {
                entry: self.video,
                media: None,
            },
            Some(audio) => PlaybackSnapshot {
                entry: vod::multivariant_playlist(&self.video, &audio),
                media: Some(MediaPlaylists {
                    video: self.video,
                    audio,
                }),
            },
        };
        (self.fragments, snapshot)
    }
}

impl fmt::Debug for PlaybackSelection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PlaybackSelection")
            .field("fragments", &self.fragments.len())
            .field("video_bytes", &self.video.len())
            .field("audio_bytes", &self.audio.as_ref().map(String::len))
            .finish()
    }
}

/// The bodies a player is handed for one [`PlaybackSelection`], each under a
/// fixed name: the entry, [`PlaybackSnapshot::ENTRY`], which the player opens,
/// and with audio the two media playlists the entry names relative to itself.
/// A viewer hands the player each body under its name beside the entry and
/// answers any other name as absent; the bodies stay the same however often
/// the player asks. A viewer that serves the bodies itself takes them out of
/// the snapshot with [`IntoIterator`], each with its name. Its `Debug` output
/// shows each name with the length of its body, never a body, whose
/// references carry media capabilities. The example of the [module](self)
/// reads one.
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct PlaybackSnapshot {
    /// The body named [`PlaybackSnapshot::ENTRY`].
    entry: String,
    /// The media playlists the entry names, when it is a multivariant
    /// playlist.
    media: Option<MediaPlaylists>,
}

/// The two media playlists beside a multivariant entry.
#[derive(Clone, PartialEq, Eq, Hash)]
struct MediaPlaylists {
    /// The body named [`vod::VIDEO_PLAYLIST`].
    video: String,
    /// The body named [`vod::AUDIO_PLAYLIST`].
    audio: String,
}

impl PlaybackSnapshot {
    /// The name of the body a player opens: the video playlist, or the
    /// multivariant playlist over the video and the audio playlist. The
    /// example of the [module](self) opens the body of that name.
    pub const ENTRY: &'static str = "vod.m3u8";

    /// The body named `name`, or `None` for a name the snapshot does not
    /// hold. A name matches whole and in its letter case, so one with a path
    /// or a query around it is absent, and so are the media playlists' names
    /// in a snapshot without audio. The example of the [module](self) asks
    /// for three names.
    #[must_use]
    pub fn body(&self, name: &str) -> Option<&str> {
        self.iter()
            .find_map(|(held, body)| (held == name).then_some(body))
    }

    /// Each name the snapshot holds, with its body: the entry first, and with
    /// audio the video and then the audio playlist. The example of the
    /// [module](self) lists the names.
    pub fn iter(&self) -> impl Iterator<Item = (&'static str, &str)> {
        let media = self.media.iter().flat_map(|media| {
            [
                (vod::VIDEO_PLAYLIST, media.video.as_str()),
                (vod::AUDIO_PLAYLIST, media.audio.as_str()),
            ]
        });
        iter::once((Self::ENTRY, self.entry.as_str())).chain(media)
    }

    /// Whether the entry is a multivariant playlist with the window's recorded
    /// audio as its rendition, so that the player plays sound. The example of
    /// the [module](self) reads it.
    #[must_use]
    pub fn has_audio(&self) -> bool {
        self.media.is_some()
    }
}

/// Each name the snapshot holds, with its body moved out of it, in the order
/// of [`PlaybackSnapshot::iter`].
impl IntoIterator for PlaybackSnapshot {
    type Item = (&'static str, String);
    type IntoIter = vec::IntoIter<(&'static str, String)>;

    fn into_iter(self) -> Self::IntoIter {
        let mut bodies = vec![(Self::ENTRY, self.entry)];
        if let Some(media) = self.media {
            bodies.push((vod::VIDEO_PLAYLIST, media.video));
            bodies.push((vod::AUDIO_PLAYLIST, media.audio));
        }
        bodies.into_iter()
    }
}

impl fmt::Debug for PlaybackSnapshot {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PlaybackSnapshot")
            .field("body_bytes", &BodyBytes(self))
            .finish()
    }
}

/// A snapshot's names with the length of each body, shown as a map.
struct BodyBytes<'snapshot>(&'snapshot PlaybackSnapshot);

impl fmt::Debug for BodyBytes<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_map()
            .entries(self.0.iter().map(|(name, body)| (name, body.len())))
            .finish()
    }
}

/// A slot of a strip of thumbnails that shows a recording, with its time, as
/// [`thumbnail_times`] gives it. The example of the [module](self) compares
/// two.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ThumbnailTime {
    /// The slot, counted from 0 at the window's start. A slot that
    /// [`thumbnail_times`] gives is less than the `slots` it was asked for.
    pub slot: usize,
    /// The time at the middle of the slot's part of the window. A time that
    /// [`thumbnail_times`] gives is held by one of the `segments` it was given.
    pub time: WallUs,
}

/// The slots of a strip of `slots` thumbnails along `window` that show a
/// recording, in slot order, each with its time. The window is cut into
/// `slots` equal parts. Slot `n`, counted from 0, is at the middle of its part,
/// `from + (to − from) × (2n + 1) / (2 × slots)` in whole microseconds rounded
/// down. The slot is kept when one of `segments` holds
/// that time, from its start up to and not including its end. A slot whose
/// time falls in a gap is left out, so that its thumbnail stays empty rather
/// than showing a frame from beside the gap. The arithmetic runs in `i128`,
/// where no window and no slot count overflows. The example of the
/// [module](self) asks for four slots over a minute recorded for its first
/// half, and gets the two slots in that half.
#[must_use]
pub fn thumbnail_times(window: Window, segments: &[Segment], slots: usize) -> Vec<ThumbnailTime> {
    let (from, to) = (window.from(), window.to());
    let span = i128::from(to.get()) - i128::from(from.get());
    // A `usize` holds at most 64 bits on every target, so the casts are exact.
    let parts = 2 * slots as i128;
    let mut kept = Vec::new();
    for slot in 0..slots {
        let middle = i128::from(from.get()) + span * (2 * slot as i128 + 1) / parts;
        // The middle lies inside the window, so it fits `i64`; the window's
        // end stands for one that would not.
        let time = i64::try_from(middle).map_or(to, WallUs::new);
        let recorded = segments
            .iter()
            .any(|segment| segment.start <= time && time < segment.end);
        if recorded {
            kept.push(ThumbnailTime { slot, time });
        }
    }
    kept
}

/// A file name for an export of the `kind` stream of `camera`, starting at
/// `from`: the camera, the kind, and the start in UTC to the second, as in
/// `cam1-main-20261101T000000Z.mp4`.
///
/// # Example
///
/// ```
/// # use std::error::Error;
/// #
/// use iris_client::playback;
/// use iris_core::id::{CameraId, StreamKind};
/// use iris_core::time::WallUs;
///
/// let camera = CameraId::try_new("cam1")?;
/// // 2026-11-01 00:00:00.5 UTC; the name drops the fraction of a second.
/// let from = WallUs::new(1_793_491_200_500_000);
/// let name = playback::export_name(&camera, StreamKind::Main, from);
/// assert_eq!(name, "cam1-main-20261101T000000Z.mp4");
/// # Ok::<(), Box<dyn Error>>(())
/// ```
#[must_use]
pub fn export_name(camera: &CameraId, kind: StreamKind, from: WallUs) -> String {
    let civil = from.to_civil();
    format!(
        "{camera}-{kind}-{:04}{:02}{:02}T{:02}{:02}{:02}Z.mp4",
        civil.year, civil.month, civil.day, civil.hour, civil.minute, civil.second
    )
}

#[cfg(test)]
mod tests {
    use iris_core::time::MediaTicks;

    use super::*;
    use crate::api::MAX_RANGE_US;

    fn at(micros: i64) -> WallUs {
        WallUs::new(micros)
    }

    fn camera(id: &str) -> CameraId {
        CameraId::try_new(id).unwrap()
    }

    fn window(from: i64, to: i64) -> Window {
        Window::try_new(at(from), at(to)).unwrap()
    }

    fn segment(start: i64, end: i64) -> Segment {
        Segment {
            start: at(start),
            end: at(end),
            sealed: true,
        }
    }

    /// The slot `slot` of a strip, kept at `micros`.
    fn kept(slot: usize, micros: i64) -> ThumbnailTime {
        ThumbnailTime {
            slot,
            time: at(micros),
        }
    }

    /// A VOD playlist of one two-second fragment recorded at `micros`, as the
    /// check of an answer gives it: its references absolute and carrying the
    /// media capability `token`.
    fn playlist(token: &str, micros: i64) -> VodPlaylist {
        VodPlaylist::new(
            vec![PlaybackFragment {
                wall_start: at(micros),
                duration: MediaTicks::new(180_000),
            }],
            format!(
                "#EXTM3U\n#EXT-X-VERSION:7\n#EXT-X-TARGETDURATION:2\n#EXT-X-PLAYLIST-TYPE:VOD\n\
                 #EXT-X-MAP:URI=\"https://nvr.example/api/segments/7/file.mp4?token={token}\",\
                 BYTERANGE=\"700@0\"\n\
                 #IRIS-FRAGMENT:WALL-US={micros},DURATION-TICKS=180000\n#EXTINF:2.00000,\n\
                 #EXT-X-BYTERANGE:1000@700\n\
                 https://nvr.example/api/segments/7/file.mp4?token={token}\n#EXT-X-ENDLIST\n"
            ),
        )
    }

    /// The snapshot of a selection of the video playlist alone.
    fn alone() -> PlaybackSnapshot {
        PlaybackSelection::new(playlist("video", 0), None)
            .into_parts()
            .1
    }

    /// The snapshot of a selection of the video playlist with the audio one.
    fn paired() -> PlaybackSnapshot {
        PlaybackSelection::new(playlist("video", 0), Some(playlist("audio", 0)))
            .into_parts()
            .1
    }

    /// The timeline maps through the video playlist's fragments, with audio
    /// or without.
    #[test]
    fn a_selection_gives_up_the_video_playlists_fragments() {
        let fragments = playlist("video", 1_000_000).fragments().to_vec();
        for audio in [None, Some(playlist("audio", 1_010_500))] {
            let selection = PlaybackSelection::new(playlist("video", 1_000_000), audio);
            assert_eq!(selection.into_parts().0, fragments);
        }
    }

    #[test]
    fn the_entry_is_named_vod_m3u8() {
        assert_eq!(PlaybackSnapshot::ENTRY, "vod.m3u8");
    }

    #[test]
    fn a_selection_without_audio_serves_the_video_playlist_at_the_entry() {
        let body = playlist("video", 0).body().to_owned();
        assert_eq!(alone().body(PlaybackSnapshot::ENTRY), Some(body.as_str()));
    }

    #[test]
    fn a_snapshot_has_audio_only_with_an_audio_playlist() {
        assert!(!alone().has_audio());
        assert!(paired().has_audio());
    }

    #[test]
    fn a_snapshot_without_audio_holds_no_media_playlist() {
        let snapshot = alone();
        for name in [vod::VIDEO_PLAYLIST, vod::AUDIO_PLAYLIST] {
            assert_eq!(snapshot.body(name), None, "{name}");
        }
    }

    #[test]
    fn a_selection_with_audio_serves_a_multivariant_playlist_at_the_entry() {
        let (video, audio) = (playlist("video", 0), playlist("audio", 0));
        let multivariant = vod::multivariant_playlist(video.body(), audio.body());
        assert_eq!(paired().body("vod.m3u8"), Some(multivariant.as_str()));
    }

    #[test]
    fn a_selection_with_audio_serves_both_media_playlists_beside_the_entry() {
        let (video, audio) = (playlist("video", 0), playlist("audio", 0));
        let snapshot = paired();
        assert_eq!(snapshot.body("video.m3u8"), Some(video.body()));
        assert_eq!(snapshot.body("audio.m3u8"), Some(audio.body()));
    }

    /// The entry comes first, and with audio the video and then the audio
    /// playlist.
    #[test]
    fn a_snapshot_lists_each_name_in_order() {
        for (snapshot, names) in [
            (alone(), vec!["vod.m3u8"]),
            (paired(), vec!["vod.m3u8", "video.m3u8", "audio.m3u8"]),
        ] {
            let listed: Vec<&str> = snapshot.iter().map(|(name, _)| name).collect();
            assert_eq!(listed, names);
        }
    }

    /// A viewer that serves the bodies takes out the ones the snapshot
    /// lists, under the same names and in the same order.
    #[test]
    fn a_snapshot_gives_up_each_body_with_its_name_in_order() {
        for snapshot in [alone(), paired()] {
            let listed: Vec<(&str, String)> = snapshot
                .iter()
                .map(|(name, body)| (name, body.to_owned()))
                .collect();
            let taken: Vec<(&str, String)> = snapshot.into_iter().collect();
            assert_eq!(taken, listed);
        }
    }

    #[test]
    fn an_unknown_name_is_absent() {
        for snapshot in [&alone(), &paired()] {
            for name in [
                "../x",
                "other.m3u8",
                "",
                "/vod.m3u8",
                "vod.m3u8/",
                "./vod.m3u8",
                "../video.m3u8",
                "VOD.M3U8",
                "Audio.m3u8",
                "vod.m3u8?token=x",
                "vod.m3u8\n",
                " video.m3u8",
            ] {
                assert_eq!(snapshot.body(name), None, "{name:?}");
            }
        }
    }

    /// The `Debug` output of a selection: the count of fragments and the
    /// length of each body.
    #[test]
    fn a_selection_debug_shows_counts_in_place_of_its_bodies() {
        let (video, audio) = (
            playlist("video", 0).body().len(),
            playlist("audio", 0).body().len(),
        );
        let alone = PlaybackSelection::new(playlist("video", 0), None);
        let paired = PlaybackSelection::new(playlist("video", 0), Some(playlist("audio", 0)));
        assert_eq!(
            format!("{alone:?}"),
            format!(
                "PlaybackSelection {{ fragments: 1, video_bytes: {video}, audio_bytes: None }}"
            )
        );
        assert_eq!(
            format!("{paired:?}"),
            format!(
                "PlaybackSelection {{ fragments: 1, video_bytes: {video}, \
                 audio_bytes: Some({audio}) }}"
            )
        );
    }

    /// The `Debug` output of a snapshot: each name with the length of its
    /// body.
    #[test]
    fn a_snapshot_debug_shows_body_lengths_in_place_of_its_bodies() {
        let (video, audio) = (
            playlist("video", 0).body().len(),
            playlist("audio", 0).body().len(),
        );
        let entry = paired().body(PlaybackSnapshot::ENTRY).unwrap().len();
        assert_eq!(
            format!("{:?}", alone()),
            format!("PlaybackSnapshot {{ body_bytes: {{\"vod.m3u8\": {video}}} }}")
        );
        assert_eq!(
            format!("{:?}", paired()),
            format!(
                "PlaybackSnapshot {{ body_bytes: {{\"vod.m3u8\": {entry}, \
                 \"video.m3u8\": {video}, \"audio.m3u8\": {audio}}} }}"
            )
        );
    }

    /// The pretty `Debug` output of a selection and of a snapshot, with audio
    /// and without, names no capability and no media path.
    #[test]
    fn a_pretty_debug_output_holds_no_capability() {
        let selection = PlaybackSelection::new(playlist("video", 0), Some(playlist("audio", 0)));
        let alone_selection = PlaybackSelection::new(playlist("video", 0), None);
        for shown in [
            format!("{selection:#?}"),
            format!("{alone_selection:#?}"),
            format!("{:#?}", alone()),
            format!("{:#?}", paired()),
        ] {
            assert!(!shown.contains("token="), "{shown}");
            assert!(!shown.contains("/api/segments/"), "{shown}");
        }
    }

    /// Twelve slots over 24 s, the Linux viewer's strip, each at the middle of
    /// its two seconds; recorded are 0 to 4 s, 9 to 15 s, and 21 s on past the
    /// window's end.
    #[test]
    fn thumbnail_times_fall_inside_recorded_segments() {
        let from = 1_793_571_600_000_000;
        let second = 1_000_000;
        let segments = [
            segment(from, from + 4 * second),
            segment(from + 9 * second, from + 15 * second),
            segment(from + 21 * second, from + 30 * second),
        ];
        let times = thumbnail_times(window(from, from + 24 * second), &segments, 12);
        // A slot in a gap is left out, as is 15 s, where a segment ends; 21 s,
        // where one starts, is kept.
        let slots = [(0, 1), (1, 3), (4, 9), (5, 11), (6, 13), (10, 21), (11, 23)];
        let expected: Vec<ThumbnailTime> = slots
            .iter()
            .map(|&(slot, seconds)| kept(slot, from + seconds * second))
            .collect();
        assert_eq!(times, expected);
    }

    /// A 47 µs window: each slot's time is `47 × (2n + 1) / 24` rounded down,
    /// which is neither the nearest microsecond nor `47 / 24` steps of one.
    #[test]
    fn a_thumbnail_time_is_its_slots_middle_rounded_down() {
        let times = thumbnail_times(window(0, 47), &[segment(0, 47)], 12);
        let expected: Vec<ThumbnailTime> = [1, 5, 9, 13, 17, 21, 25, 29, 33, 37, 41, 45]
            .into_iter()
            .enumerate()
            .map(|(slot, micros)| kept(slot, micros))
            .collect();
        assert_eq!(times, expected);
    }

    #[test]
    fn a_segment_covers_a_thumbnail_time_from_its_start_to_before_its_end() {
        // One slot over 10 µs is at 5 µs.
        for (start, end, held) in [
            (5, 6, true),
            (4, 5, false),
            (0, 10, true),
            (6, 10, false),
            (0, 5, false),
            (5, 5, false),
        ] {
            let times = thumbnail_times(window(0, 10), &[segment(start, end)], 1);
            let expected = if held { vec![kept(0, 5)] } else { Vec::new() };
            assert_eq!(times, expected, "a segment from {start} to {end}");
        }
    }

    /// One slot over 10 µs is at 5 µs, between a segment that ends there
    /// and one that starts a microsecond after it.
    #[test]
    fn segments_on_either_side_of_a_thumbnail_time_do_not_hold_it() {
        let around = [segment(0, 5), segment(6, 10)];
        assert_eq!(thumbnail_times(window(0, 10), &around, 1), []);
    }

    #[test]
    fn no_slot_gives_no_thumbnail_time() {
        assert_eq!(thumbnail_times(window(0, 10), &[segment(0, 10)], 0), []);
    }

    #[test]
    fn the_slot_count_divides_the_window() {
        let recorded = [segment(0, 10)];
        assert_eq!(thumbnail_times(window(0, 10), &recorded, 1), [kept(0, 5)]);
        assert_eq!(
            thumbnail_times(window(0, 10), &recorded, 2),
            [kept(0, 2), kept(1, 7)]
        );
    }

    #[test]
    fn a_window_shorter_than_its_slots_gives_several_slots_one_time() {
        assert_eq!(
            thumbnail_times(window(0, 1), &[segment(0, 1)], 3),
            [kept(0, 0), kept(1, 0), kept(2, 0)]
        );
    }

    /// The widest window the API takes, at either end of the time range:
    /// seven days are 24 times 25,200 s.
    #[test]
    fn the_widest_window_at_either_end_of_time_keeps_every_slot() {
        let span = i64::try_from(MAX_RANGE_US).unwrap();
        let step = span / 24;
        assert_eq!(step, 25_200_000_000);
        for from in [i64::MIN, i64::MAX - span] {
            let times =
                thumbnail_times(window(from, from + span), &[segment(from, from + span)], 12);
            let expected: Vec<ThumbnailTime> = (0..12)
                .map(|slot| kept(slot, from + step * (2 * slot as i64 + 1)))
                .collect();
            assert_eq!(times, expected, "from {from}");
        }
    }

    #[test]
    fn an_export_name_names_its_stream_at_its_utc_start() {
        assert_eq!(
            export_name(&camera("cam1"), StreamKind::Main, at(1_793_571_648_056_937)),
            "cam1-main-20261101T222048Z.mp4"
        );
    }

    #[test]
    fn an_export_name_pads_each_field_to_its_width() {
        assert_eq!(
            export_name(&camera("cam_2-b"), StreamKind::Sub, at(0)),
            "cam_2-b-sub-19700101T000000Z.mp4"
        );
    }

    #[test]
    fn an_export_name_drops_the_microseconds() {
        assert_eq!(
            export_name(&camera("cam1"), StreamKind::Sub, at(999_999)),
            export_name(&camera("cam1"), StreamKind::Sub, at(0))
        );
    }
}
