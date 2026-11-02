//! Initialization segment (`ftyp` + `moov`) for a single video track.

use bytes::Bytes;

use crate::boxes::BoxWriter;
use crate::fragment::NON_SYNC_SAMPLE_FLAGS;
use iris_core::time::TIMESCALE;

/// The track id used for video in every irisd file.
pub const VIDEO_TRACK_ID: u32 = 1;

/// A codec configuration: the complete `hvc1`/`avc1` sample entry box plus
/// the facts playlists and clients need.
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
    /// CRC-32C of `bytes`; identifies a parameter-set epoch.
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
    w.u32(1000); // timescale
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
