//! Initialization segment (`ftyp` + `moov`) for a single video or audio track.

use bytes::Bytes;

use crate::boxes::{BoxHeader, BoxWriter};
use crate::fragment::{NON_SYNC_SAMPLE_FLAGS, SYNC_SAMPLE_FLAGS};
use iris_core::time::TIMESCALE;

/// The track id used for video in every irisd file.
pub const VIDEO_TRACK_ID: u32 = 1;

/// Movie timescale written to `mvhd` by [`build_init_segment`]: edit-list
/// durations are expressed in it (milliseconds).
pub const MOVIE_TIMESCALE: u32 = 1000;

/// A codec configuration: the complete sample entry box (`hvc1` for video,
/// `mp4a` for audio) plus the facts playlists and clients need. The box's own
/// type says which of the two a track is ([`SampleEntry::is_audio`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SampleEntry {
    /// The serialized sample entry box, parameter sets included (`hvcC`, or
    /// `esds` for audio).
    pub bytes: Bytes,
    /// RFC 6381 codecs string, for example `hvc1.1.6.L150.B0` or `mp4a.40.2`.
    pub codecs: String,
    /// Display width in pixels; zero for audio.
    pub width: u16,
    /// Display height in pixels; zero for audio.
    pub height: u16,
    /// CRC-32C of `bytes` when built with [`SampleEntry::new`]: a hint for a
    /// parameter-set epoch, not an identity. The store and the catalog
    /// compare entries by bytes, and the catalog keys them by hash and bytes
    /// together, so two entries with one hash stay distinct.
    pub hash: u32,
}

impl SampleEntry {
    /// Wraps a serialized sample entry and computes its hash.
    #[must_use]
    pub fn new(bytes: Bytes, codecs: String, width: u16, height: u16) -> Self {
        let hash = crate::crc::crc32c(&bytes);
        Self {
            bytes,
            codecs,
            width,
            height,
            hash,
        }
    }

    /// Wraps a serialized audio sample entry: no picture size.
    #[must_use]
    pub fn audio(bytes: Bytes, codecs: String) -> Self {
        Self::new(bytes, codecs, 0, 0)
    }

    /// Whether the entry describes audio: its box type is `mp4a`.
    #[must_use]
    pub fn is_audio(&self) -> bool {
        self.bytes.get(4..8) == Some(b"mp4a")
    }
}

/// The duration in 90 kHz media ticks of an audio frame of `frame_samples`
/// samples at `sample_rate` hertz, when it is a whole number of ticks.
///
/// Audio files use the 90 kHz media timescale of the video files, so one
/// time model serves both; that is exact for the rates whose frames are whole
/// ticks (an AAC frame of 1,024 samples at 8, 16, 32, 48, 64, or 96 kHz) and
/// refused for the others (the 44.1 kHz family), whose timeline would drift.
///
/// ```
/// use iris_mp4::init::audio_frame_ticks;
///
/// assert_eq!(audio_frame_ticks(64_000, 1_024), Some(1_440));
/// assert_eq!(audio_frame_ticks(44_100, 1_024), None);
/// ```
#[must_use]
pub fn audio_frame_ticks(sample_rate: u32, frame_samples: u32) -> Option<u32> {
    let ticks = u64::from(frame_samples) * u64::from(TIMESCALE);
    let rate = u64::from(sample_rate);
    if rate == 0 || ticks == 0 || !ticks.is_multiple_of(rate) {
        return None;
    }
    u32::try_from(ticks / rate).ok()
}

/// Builds the initialization segment for the one track `entry` describes.
///
/// An audio track ([`SampleEntry::is_audio`]) has the layout of a video one
/// with a sound handler and media header, no picture size, full volume, and
/// `trex` defaults that mark every sample a sync sample, since audio frames
/// do not depend on each other.
#[must_use]
pub fn build_init_segment(entry: &SampleEntry) -> Bytes {
    let kind = if entry.is_audio() {
        TrackKind::Audio
    } else {
        TrackKind::Video {
            width: entry.width,
            height: entry.height,
        }
    };
    build(&[(&entry.bytes, kind, VIDEO_TRACK_ID)])
}

/// The body of the first child box of type `kind` in `buf`, or `None` when
/// there is none or a box claims more than `buf` holds.
fn child(buf: &[u8], kind: [u8; 4]) -> Option<&[u8]> {
    let mut rest = buf;
    while !rest.is_empty() {
        let header = BoxHeader::parse(rest)?;
        let size = usize::try_from(header.size?).ok()?;
        if size > rest.len() {
            return None;
        }
        if header.kind == kind {
            return rest.get(usize::from(header.header_len)..size);
        }
        rest = &rest[size..];
    }
    None
}

/// The sample entry box of the first track of an initialization segment
/// (`moov/trak/mdia/minf/stbl/stsd`), whole, as [`SampleEntry::bytes`] holds
/// it. `None` when a box on the way is missing or does not fit.
///
/// ```
/// use bytes::Bytes;
/// use iris_mp4::init::{SampleEntry, build_init_segment, sample_entry_bytes};
///
/// let entry = SampleEntry::audio(Bytes::from_static(b"\0\0\0\x08mp4a"), "mp4a.40.2".into());
/// let init = build_init_segment(&entry);
/// assert_eq!(sample_entry_bytes(&init), Some(&entry.bytes[..]));
/// ```
#[must_use]
pub fn sample_entry_bytes(init: &[u8]) -> Option<&[u8]> {
    let mut body = init;
    for kind in [b"moov", b"trak", b"mdia", b"minf", b"stbl", b"stsd"] {
        body = child(body, *kind)?;
    }
    // A full box: version and flags, then the entry count, then the entries.
    let entries = body.get(8..)?;
    let header = BoxHeader::parse(entries)?;
    entries.get(..usize::try_from(header.size?).ok()?)
}

/// The channel count an `mp4a` sample entry declares in its `channelcount`
/// field, or `None` when `entry` is not an `mp4a` box that reaches the field
/// or the field is zero. The entry irisd records for an AAC stream carries
/// the count its configuration declares (`docs/research/13-audio.md` §8).
///
/// ```
/// use iris_mp4::init::audio_channel_count;
///
/// // size, type, reserved, data_reference_index, reserved, channelcount.
/// let mut entry = b"\0\0\0\x1amp4a\0\0\0\0\0\0\0\x01\0\0\0\0\0\0\0\0".to_vec();
/// entry.extend_from_slice(&[0, 2]);
/// assert_eq!(audio_channel_count(&entry), Some(2));
/// assert_eq!(audio_channel_count(&entry[..25]), None);
/// ```
#[must_use]
pub fn audio_channel_count(entry: &[u8]) -> Option<u16> {
    if entry.get(4..8)? != b"mp4a" {
        return None;
    }
    let count = u16::from_be_bytes(*entry.get(24..26)?.first_chunk::<2>()?);
    (count != 0).then_some(count)
}

/// The track id of the audio track in a file that holds a video track and an
/// audio track; the video track is [`VIDEO_TRACK_ID`].
pub const AV_AUDIO_TRACK_ID: u32 = 2;

/// Builds the initialization segment of a file with two tracks: `video` as
/// track [`VIDEO_TRACK_ID`] and `audio` as track [`AV_AUDIO_TRACK_ID`], each
/// as [`build_init_segment`] writes it alone, with one `trex` for each
/// (`docs/research/13-audio.md` §9). A fragment of the audio track names
/// that track id in its `tfhd`.
#[must_use]
pub fn build_av_init_segment(video: &SampleEntry, audio: &SampleEntry) -> Bytes {
    build(&[
        (
            &video.bytes,
            TrackKind::Video {
                width: video.width,
                height: video.height,
            },
            VIDEO_TRACK_ID,
        ),
        (&audio.bytes, TrackKind::Audio, AV_AUDIO_TRACK_ID),
    ])
}

/// What a track carries, for the boxes that differ between the two.
#[derive(Clone, Copy)]
enum TrackKind {
    Video { width: u16, height: u16 },
    Audio,
}

/// Builds `ftyp` and `moov` for `tracks`: each a sample entry, what it
/// carries, and its track id, in ascending order of id.
fn build(tracks: &[(&[u8], TrackKind, u32)]) -> Bytes {
    let entries: usize = tracks.iter().map(|(entry, _, _)| entry.len()).sum();
    let mut w = BoxWriter::with_capacity(entries + 512 * tracks.len());

    let ftyp = w.begin(b"ftyp");
    w.bytes(b"iso6");
    w.u32(0);
    w.bytes(b"iso6");
    w.bytes(b"cmfc");
    w.bytes(b"mp41");
    w.end(ftyp);

    let moov = w.begin(b"moov");
    let next_track_id = tracks.iter().map(|(_, _, id)| *id).max().unwrap_or(0) + 1;
    write_mvhd(&mut w, next_track_id);
    for &(entry, kind, track_id) in tracks {
        write_trak(&mut w, entry, kind, track_id);
    }
    let mvex = w.begin(b"mvex");
    for &(_, kind, track_id) in tracks {
        let trex = w.begin_full(b"trex", 0, 0);
        w.u32(track_id);
        w.u32(1); // default_sample_description_index
        w.u32(0); // default_sample_duration
        w.u32(0); // default_sample_size
        w.u32(match kind {
            TrackKind::Video { .. } => NON_SYNC_SAMPLE_FLAGS,
            TrackKind::Audio => SYNC_SAMPLE_FLAGS,
        });
        w.end(trex);
    }
    w.end(mvex);
    w.end(moov);

    w.into_bytes()
}

const UNITY_MATRIX: [i32; 9] = [0x0001_0000, 0, 0, 0, 0x0001_0000, 0, 0, 0, 0x4000_0000];

fn write_mvhd(w: &mut BoxWriter, next_track_id: u32) {
    let mvhd = w.begin_full(b"mvhd", 1, 0);
    w.u64(0); // creation_time
    w.u64(0); // modification_time
    w.u32(MOVIE_TIMESCALE);
    w.u64(0); // duration (unknown; fragmented)
    w.i32(0x0001_0000); // rate 1.0
    w.i16(0x0100); // volume 1.0
    w.zeros(2 + 8); // reserved
    for m in UNITY_MATRIX {
        w.i32(m);
    }
    w.zeros(6 * 4); // pre_defined
    w.u32(next_track_id);
    w.end(mvhd);
}

fn write_trak(w: &mut BoxWriter, entry: &[u8], kind: TrackKind, track_id: u32) {
    let (width, height, volume, handler, name) = match kind {
        TrackKind::Video { width, height } => (width, height, 0, b"vide", &b"irisd video\0"[..]),
        TrackKind::Audio => (0, 0, 0x0100, b"soun", &b"irisd audio\0"[..]),
    };
    let trak = w.begin(b"trak");

    let tkhd = w.begin_full(b"tkhd", 1, 0x7); // enabled | in_movie | in_preview
    w.u64(0); // creation_time
    w.u64(0); // modification_time
    w.u32(track_id);
    w.u32(0); // reserved
    w.u64(0); // duration
    w.zeros(8); // reserved
    w.i16(0); // layer
    w.i16(0); // alternate_group
    w.i16(volume); // 8.8 fixed point: zero for video, 1.0 for audio
    w.u16(0); // reserved
    for m in UNITY_MATRIX {
        w.i32(m);
    }
    w.u32(u32::from(width) << 16);
    w.u32(u32::from(height) << 16);
    w.end(tkhd);

    let mdia = w.begin(b"mdia");
    let mdhd = w.begin_full(b"mdhd", 1, 0);
    w.u64(0);
    w.u64(0);
    w.u32(TIMESCALE);
    w.u64(0);
    w.u16(0x55c4); // language 'und'
    w.u16(0); // pre_defined
    w.end(mdhd);

    let hdlr = w.begin_full(b"hdlr", 0, 0);
    w.u32(0); // pre_defined
    w.bytes(handler);
    w.zeros(12); // reserved
    w.bytes(name);
    w.end(hdlr);

    let minf = w.begin(b"minf");
    match kind {
        TrackKind::Video { .. } => {
            let vmhd = w.begin_full(b"vmhd", 0, 1);
            w.u16(0); // graphicsmode
            w.zeros(6); // opcolor
            w.end(vmhd);
        }
        TrackKind::Audio => {
            let smhd = w.begin_full(b"smhd", 0, 0);
            w.i16(0); // balance
            w.u16(0); // reserved
            w.end(smhd);
        }
    }

    let dinf = w.begin(b"dinf");
    let dref = w.begin_full(b"dref", 0, 0);
    w.u32(1);
    let url = w.begin_full(b"url ", 0, 1); // self-contained
    w.end(url);
    w.end(dref);
    w.end(dinf);

    let stbl = w.begin(b"stbl");
    let sample_description = w.begin_full(b"stsd", 0, 0);
    w.u32(1);
    w.bytes(entry);
    w.end(sample_description);
    for table in [b"stts", b"stsc"] {
        let b = w.begin_full(table, 0, 0);
        w.u32(0);
        w.end(b);
    }
    let sample_sizes = w.begin_full(b"stsz", 0, 0);
    w.u32(0); // sample_size
    w.u32(0); // sample_count
    w.end(sample_sizes);
    let stco = w.begin_full(b"stco", 0, 0);
    w.u32(0);
    w.end(stco);
    w.end(stbl);
    w.end(minf);
    w.end(mdia);
    w.end(trak);
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::boxes::BoxHeader;

    /// A syntactically valid but content-free `hvc1` sample entry.
    pub(crate) fn fake_sample_entry() -> SampleEntry {
        let mut w = BoxWriter::default();
        let b = w.begin(b"hvc1");
        w.zeros(6);
        w.u16(1);
        w.zeros(16);
        w.u16(3840);
        w.u16(2160);
        w.u32(0x0048_0000);
        w.u32(0x0048_0000);
        w.u32(0);
        w.u16(1);
        w.zeros(32);
        w.u16(0x18);
        w.i16(-1);
        let c = w.begin(b"hvcC");
        w.bytes(&[
            1, 1, 0x60, 0, 0, 0, 0x90, 0, 0, 0, 0, 0, 150, 0xf0, 0, 0xfc, 0xfd, 0xf8, 0xf8, 0, 0,
            0x0f, 0,
        ]);
        w.end(c);
        w.end(b);
        SampleEntry::new(w.into_bytes(), "hvc1.1.6.L150.B0".into(), 3840, 2160)
    }

    /// The boxes of `buf` that hold fields, each with its path through the
    /// containers and its body. `dref` counts as one: its body holds the
    /// entry count and the `url ` box.
    fn field_boxes(buf: &[u8], prefix: &str, out: &mut Vec<(String, Vec<u8>)>) {
        let mut rest = buf;
        while !rest.is_empty() {
            let header = BoxHeader::parse(rest).expect("a box header");
            let size = usize::try_from(header.size.expect("a sized box")).unwrap();
            let body = &rest[usize::from(header.header_len)..size];
            let path = format!("{prefix}{}", std::str::from_utf8(&header.kind).unwrap());
            if [
                *b"moov", *b"trak", *b"mdia", *b"minf", *b"dinf", *b"stbl", *b"mvex",
            ]
            .contains(&header.kind)
            {
                field_boxes(body, &format!("{path}/"), out);
            } else {
                out.push((path, body.to_vec()));
            }
            rest = &rest[size..];
        }
    }

    /// Big-endian bytes of a list of fields, each `(value, width in bytes)`.
    fn fields(list: &[(u64, usize)]) -> Vec<u8> {
        list.iter()
            .flat_map(|&(value, width)| value.to_be_bytes()[8 - width..].to_vec())
            .collect()
    }

    /// Every field of the initialization segment, box by box, written out
    /// here from ISO/IEC 14496-12: version 1 `mvhd`, `tkhd`, and `mdhd`
    /// with zero times and durations, the unity matrix, the track's size
    /// in 16.16 fixed point, a 90 kHz media timescale, the sample entry
    /// verbatim in `stsd`, empty sample tables, and `trex` defaults that
    /// mark samples as dependent.
    #[test]
    fn every_box_of_the_init_segment_carries_the_specified_fields() {
        let entry = fake_sample_entry();
        let init = build_init_segment(&entry);
        let mut boxes = Vec::new();
        field_boxes(&init, "", &mut boxes);
        let matrix = [
            (0x0001_0000, 4),
            (0, 4),
            (0, 4),
            (0, 4),
            (0x0001_0000, 4),
            (0, 4),
            (0, 4),
            (0, 4),
            (0x4000_0000, 4),
        ];
        let mut mvhd = fields(&[
            (0x0100_0000, 4), // version 1, no flags
            (0, 8),           // creation time
            (0, 8),           // modification time
            (1000, 4),        // movie timescale
            (0, 8),           // duration, unknown in a fragmented file
            (0x0001_0000, 4), // rate 1.0
            (0x0100, 2),      // volume 1.0
            (0, 2),           // reserved
            (0, 8),           // reserved
        ]);
        mvhd.extend(fields(&matrix));
        mvhd.extend(fields(&[(0, 8), (0, 8), (0, 8), (2, 4)])); // pre_defined, next track id
        let mut tkhd = fields(&[
            (0x0100_0007, 4), // version 1; enabled, in movie, in preview
            (0, 8),
            (0, 8),
            (1, 4), // track id
            (0, 4), // reserved
            (0, 8), // duration
            (0, 8), // reserved
            (0, 2), // layer
            (0, 2), // alternate group
            (0, 2), // volume of a video track
            (0, 2), // reserved
        ]);
        tkhd.extend(fields(&matrix));
        tkhd.extend(fields(&[(3840 << 16, 4), (2160 << 16, 4)]));
        let mut hdlr = fields(&[(0, 4), (0, 4)]);
        hdlr.extend_from_slice(b"vide");
        hdlr.extend_from_slice(&[0; 12]);
        hdlr.extend_from_slice(b"irisd video\0");
        let mut dref = fields(&[(0, 4), (1, 4), (12, 4)]);
        dref.extend_from_slice(b"url ");
        dref.extend(fields(&[(1, 4)])); // the media is in this file
        let mut stsd = fields(&[(0, 4), (1, 4)]);
        stsd.extend_from_slice(&entry.bytes);
        let empty_table = fields(&[(0, 4), (0, 4)]);
        let expected: Vec<(&str, Vec<u8>)> = vec![
            ("ftyp", b"iso6\0\0\0\0iso6cmfcmp41".to_vec()),
            ("moov/mvhd", mvhd),
            ("moov/trak/tkhd", tkhd),
            (
                "moov/trak/mdia/mdhd",
                fields(&[
                    (0x0100_0000, 4),
                    (0, 8),
                    (0, 8),
                    (90_000, 4), // media timescale
                    (0, 8),
                    (0x55c4, 2), // language "und"
                    (0, 2),
                ]),
            ),
            ("moov/trak/mdia/hdlr", hdlr),
            (
                "moov/trak/mdia/minf/vmhd",
                fields(&[(1, 4), (0, 2), (0, 6)]),
            ),
            ("moov/trak/mdia/minf/dinf/dref", dref),
            ("moov/trak/mdia/minf/stbl/stsd", stsd),
            ("moov/trak/mdia/minf/stbl/stts", empty_table.clone()),
            ("moov/trak/mdia/minf/stbl/stsc", empty_table.clone()),
            (
                "moov/trak/mdia/minf/stbl/stsz",
                fields(&[(0, 4), (0, 4), (0, 4)]),
            ),
            ("moov/trak/mdia/minf/stbl/stco", empty_table),
            (
                "moov/mvex/trex",
                // Track 1, sample description 1, no default duration or
                // size, and samples that depend on others and are not sync.
                fields(&[(0, 4), (1, 4), (1, 4), (0, 4), (0, 4), (0x0101_0000, 4)]),
            ),
        ];
        let paths: Vec<&str> = boxes.iter().map(|(path, _)| path.as_str()).collect();
        let expected_paths: Vec<&str> = expected.iter().map(|(path, _)| *path).collect();
        assert_eq!(paths, expected_paths);
        for ((path, body), (_, wanted)) in boxes.iter().zip(&expected) {
            assert_eq!(body, wanted, "{path}");
        }
    }

    #[test]
    fn init_segment_has_ftyp_then_moov() {
        let init = build_init_segment(&fake_sample_entry());
        let ftyp = BoxHeader::parse(&init).unwrap();
        assert_eq!(ftyp.kind, *b"ftyp");
        let moov = BoxHeader::parse(&init[ftyp.size.unwrap() as usize..]).unwrap();
        assert_eq!(moov.kind, *b"moov");
        assert_eq!(ftyp.size.unwrap() + moov.size.unwrap(), init.len() as u64);
        assert!(init.windows(4).any(|w| w == b"hvcC"));
        assert!(init.windows(4).any(|w| w == b"trex"));
    }

    /// A syntactically valid `mp4a` sample entry without its `esds`.
    fn fake_audio_entry() -> SampleEntry {
        let mut w = BoxWriter::default();
        let b = w.begin(b"mp4a");
        w.zeros(6);
        w.u16(1); // data_reference_index
        w.zeros(8);
        w.u16(1); // channelcount
        w.u16(16); // samplesize
        w.zeros(4);
        w.u32(64_000 << 16);
        w.end(b);
        SampleEntry::audio(w.into_bytes(), "mp4a.40.2".into())
    }

    /// The audio initialization segment differs from the video one in the
    /// boxes that describe the medium and in nothing else: no picture size
    /// and full volume in `tkhd`, a sound handler, `smhd` for `vmhd`, the
    /// audio sample entry, and `trex` defaults that mark samples as sync.
    #[test]
    fn the_audio_init_segment_differs_only_in_the_boxes_of_its_medium() {
        let audio_entry = fake_audio_entry();
        assert_eq!(audio_entry.hash, crate::crc::crc32c(&audio_entry.bytes));
        assert!(audio_entry.is_audio());
        assert_eq!((audio_entry.width, audio_entry.height), (0, 0));
        let mut audio = Vec::new();
        field_boxes(&build_init_segment(&audio_entry), "", &mut audio);
        let video_entry = fake_sample_entry();
        assert!(!video_entry.is_audio());
        let mut video = Vec::new();
        field_boxes(&build_init_segment(&video_entry), "", &mut video);

        let differing: Vec<(&str, &str)> = audio
            .iter()
            .zip(&video)
            .filter(|(a, v)| a != v)
            .map(|(a, v)| (a.0.as_str(), v.0.as_str()))
            .collect();
        assert_eq!(audio.len(), video.len());
        assert_eq!(
            differing,
            [
                ("moov/trak/tkhd", "moov/trak/tkhd"),
                ("moov/trak/mdia/hdlr", "moov/trak/mdia/hdlr"),
                ("moov/trak/mdia/minf/smhd", "moov/trak/mdia/minf/vmhd"),
                (
                    "moov/trak/mdia/minf/stbl/stsd",
                    "moov/trak/mdia/minf/stbl/stsd"
                ),
                ("moov/mvex/trex", "moov/mvex/trex"),
            ]
        );
        let body = |path: &str| &audio.iter().find(|(p, _)| p == path).unwrap().1;
        let tkhd = body("moov/trak/tkhd");
        // Volume 1.0 and its reserved field before the matrix; zero width and height last.
        assert_eq!(tkhd[tkhd.len() - 48..tkhd.len() - 44], [1, 0, 0, 0]);
        assert_eq!(tkhd[tkhd.len() - 8..], [0; 8]);
        let hdlr = body("moov/trak/mdia/hdlr");
        assert_eq!(&hdlr[8..12], b"soun");
        assert_eq!(&hdlr[24..], b"irisd audio\0");
        assert_eq!(body("moov/trak/mdia/minf/smhd"), &[0; 8]);
        let stsd = body("moov/trak/mdia/minf/stbl/stsd");
        assert_eq!(&stsd[8..], &audio_entry.bytes[..]);
        assert_eq!(
            body("moov/mvex/trex"),
            &fields(&[(0, 4), (1, 4), (1, 4), (0, 4), (0, 4), (0x0200_0000, 4)])
        );
    }

    #[test]
    fn an_audio_frame_is_a_whole_number_of_ticks_or_refused() {
        for (rate, ticks) in [
            (8_000, 11_520),
            (16_000, 5_760),
            (32_000, 2_880),
            (48_000, 1_920),
            (64_000, 1_440),
            (96_000, 960),
        ] {
            assert_eq!(audio_frame_ticks(rate, 1_024), Some(ticks), "{rate}");
        }
        for rate in [11_025, 22_050, 44_100, 88_200, 0] {
            assert_eq!(audio_frame_ticks(rate, 1_024), None, "{rate}");
        }
        assert_eq!(audio_frame_ticks(48_000, 0), None);
        assert_eq!(audio_frame_ticks(48_000, 960), Some(1_800));
        assert_eq!(audio_frame_ticks(1, u32::MAX), None, "past 32 bits");
    }

    /// The two-track initialization segment holds the video track as it is
    /// alone, the audio track as it is alone but for its track id, a `trex`
    /// for each, and the next free track id.
    #[test]
    fn the_two_track_init_segment_is_both_single_tracks_under_one_movie() {
        let (video_entry, audio_entry) = (fake_sample_entry(), fake_audio_entry());
        let boxes_of = |init: &Bytes| {
            let mut boxes = Vec::new();
            field_boxes(init, "", &mut boxes);
            boxes
        };
        let both = boxes_of(&build_av_init_segment(&video_entry, &audio_entry));
        let video = boxes_of(&build_init_segment(&video_entry));
        let audio = boxes_of(&build_init_segment(&audio_entry));
        let paths: Vec<&str> = both.iter().map(|(path, _)| path.as_str()).collect();
        let track = |boxes: &[(String, Vec<u8>)]| -> Vec<(String, Vec<u8>)> {
            boxes
                .iter()
                .filter(|(path, _)| path.starts_with("moov/trak/"))
                .cloned()
                .collect()
        };
        let (video_track, audio_track) = (track(&video), track(&audio));
        let tracks = track(&both);
        assert_eq!(paths[..2], ["ftyp", "moov/mvhd"]);
        assert_eq!(tracks.len(), video_track.len() + audio_track.len());
        assert_eq!(tracks[..video_track.len()], video_track[..]);
        // The audio track's `tkhd` names track 2; nothing else differs.
        let second = &tracks[video_track.len()..];
        for (index, ((path, body), (alone_path, alone_body))) in
            second.iter().zip(&audio_track).enumerate()
        {
            assert_eq!(path, alone_path);
            if index == 0 {
                assert_eq!(path, "moov/trak/tkhd");
                // Version and flags, two times of eight bytes, then the id.
                assert_eq!(body[20..24], [0, 0, 0, 2]);
                assert_eq!(alone_body[20..24], [0, 0, 0, 1]);
                assert_eq!(
                    (&body[..20], &body[24..]),
                    (&alone_body[..20], &alone_body[24..])
                );
            } else {
                assert_eq!(body, alone_body, "{path}");
            }
        }
        let mvhd = &both.iter().find(|(path, _)| path == "moov/mvhd").unwrap().1;
        assert_eq!(mvhd[mvhd.len() - 4..], [0, 0, 0, 3], "next_track_ID");
        let trex: Vec<&Vec<u8>> = both
            .iter()
            .filter(|(path, _)| path == "moov/mvex/trex")
            .map(|(_, body)| body)
            .collect();
        assert_eq!(
            trex,
            [
                &fields(&[(0, 4), (1, 4), (1, 4), (0, 4), (0, 4), (0x0101_0000, 4)]),
                &fields(&[(0, 4), (2, 4), (1, 4), (0, 4), (0, 4), (0x0200_0000, 4)]),
            ]
        );
    }

    /// The channel count is read from an `mp4a` entry alone, and only when
    /// the entry reaches the field and the field is not zero.
    #[test]
    fn the_channel_count_is_the_field_of_an_mp4a_entry() {
        let audio = fake_audio_entry();
        assert_eq!(audio_channel_count(&audio.bytes), Some(1));
        let mut six = audio.bytes.to_vec();
        six[24..26].copy_from_slice(&6u16.to_be_bytes());
        assert_eq!(audio_channel_count(&six), Some(6));
        six[24..26].copy_from_slice(&0u16.to_be_bytes());
        assert_eq!(audio_channel_count(&six), None);
        for cut in 0..26 {
            assert_eq!(
                audio_channel_count(&audio.bytes[..cut]),
                None,
                "cut at {cut}"
            );
        }
        assert_eq!(audio_channel_count(&audio.bytes[..26]), Some(1));
        // A video entry has other fields where an audio entry counts channels.
        let mut video = audio.bytes.to_vec();
        video[4..8].copy_from_slice(b"hvc1");
        assert_eq!(audio_channel_count(&video), None);
        // The count comes through an init segment's sample entry.
        let init = build_init_segment(&audio);
        assert_eq!(
            sample_entry_bytes(&init).and_then(audio_channel_count),
            Some(1)
        );
    }

    /// The sample entry of an init segment reads back whole, for either kind
    /// and for the first track of two; a segment cut anywhere has none.
    #[test]
    fn the_sample_entry_reads_back_from_an_init_segment() {
        let (video, audio) = (fake_sample_entry(), fake_audio_entry());
        for entry in [&video, &audio] {
            let init = build_init_segment(entry);
            assert_eq!(sample_entry_bytes(&init), Some(&entry.bytes[..]));
            for cut in 0..init.len() {
                assert_eq!(sample_entry_bytes(&init[..cut]), None, "cut at {cut}");
            }
        }
        let both = build_av_init_segment(&video, &audio);
        assert_eq!(sample_entry_bytes(&both), Some(&video.bytes[..]));
        assert_eq!(sample_entry_bytes(b""), None);
    }
}
