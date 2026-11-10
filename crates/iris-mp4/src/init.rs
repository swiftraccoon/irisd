//! Initialization segment (`ftyp` + `moov`) for a single video track.

use bytes::Bytes;

use crate::boxes::BoxWriter;
use crate::fragment::NON_SYNC_SAMPLE_FLAGS;
use iris_core::time::TIMESCALE;

/// The track id used for video in every irisd file.
pub const VIDEO_TRACK_ID: u32 = 1;

/// Movie timescale written to `mvhd` by [`build_init_segment`]: edit-list
/// durations are expressed in it (milliseconds).
pub const MOVIE_TIMESCALE: u32 = 1000;

/// A codec configuration: the complete `hvc1` sample entry box plus the facts
/// playlists and clients need.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SampleEntry {
    /// The serialized sample entry box, parameter sets included (`hvcC`).
    pub bytes: Bytes,
    /// RFC 6381 codecs string, for example `hvc1.1.6.L150.B0`.
    pub codecs: String,
    /// Display width in pixels.
    pub width: u16,
    /// Display height in pixels.
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
}

/// Builds the initialization segment for one video track.
#[must_use]
pub fn build_init_segment(entry: &SampleEntry) -> Bytes {
    let mut w = BoxWriter::with_capacity(entry.bytes.len() + 512);

    let ftyp = w.begin(b"ftyp");
    w.bytes(b"iso6");
    w.u32(0);
    w.bytes(b"iso6");
    w.bytes(b"cmfc");
    w.bytes(b"mp41");
    w.end(ftyp);

    let moov = w.begin(b"moov");
    write_mvhd(&mut w);
    write_trak(&mut w, entry);
    let mvex = w.begin(b"mvex");
    let trex = w.begin_full(b"trex", 0, 0);
    w.u32(VIDEO_TRACK_ID);
    w.u32(1); // default_sample_description_index
    w.u32(0); // default_sample_duration
    w.u32(0); // default_sample_size
    w.u32(NON_SYNC_SAMPLE_FLAGS);
    w.end(trex);
    w.end(mvex);
    w.end(moov);

    w.into_bytes()
}

const UNITY_MATRIX: [i32; 9] = [0x0001_0000, 0, 0, 0, 0x0001_0000, 0, 0, 0, 0x4000_0000];

fn write_mvhd(w: &mut BoxWriter) {
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
    w.u32(VIDEO_TRACK_ID + 1); // next_track_ID
    w.end(mvhd);
}

fn write_trak(w: &mut BoxWriter, entry: &SampleEntry) {
    let trak = w.begin(b"trak");

    let tkhd = w.begin_full(b"tkhd", 1, 0x7); // enabled | in_movie | in_preview
    w.u64(0); // creation_time
    w.u64(0); // modification_time
    w.u32(VIDEO_TRACK_ID);
    w.u32(0); // reserved
    w.u64(0); // duration
    w.zeros(8); // reserved
    w.i16(0); // layer
    w.i16(0); // alternate_group
    w.i16(0); // volume (video)
    w.u16(0); // reserved
    for m in UNITY_MATRIX {
        w.i32(m);
    }
    w.u32(u32::from(entry.width) << 16);
    w.u32(u32::from(entry.height) << 16);
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
    w.bytes(b"vide");
    w.zeros(12); // reserved
    w.bytes(b"irisd video\0");
    w.end(hdlr);

    let minf = w.begin(b"minf");
    let vmhd = w.begin_full(b"vmhd", 0, 1);
    w.u16(0); // graphicsmode
    w.zeros(6); // opcolor
    w.end(vmhd);

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
    w.bytes(&entry.bytes);
    w.end(sample_description);
    for kind in [b"stts", b"stsc"] {
        let b = w.begin_full(kind, 0, 0);
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
}
