//! Rewrites that turn stored fragments into a stand-alone export: a continuous
//! timeline from zero, fresh sequence numbers, a trailer over the rewritten
//! bytes, and an edit list in the init segment that trims the first and last
//! fragments to the requested range.
//!
//! Stored decode times run on across the files of a recording run and
//! sequence numbers restart per file, so an export that copied fragments
//! verbatim would start hours into its own timeline and jump at every file
//! boundary. These functions patch the boxes in place; sample data is never
//! touched.

use crate::boxes::{BoxHeader, BoxWriter, be_u32};
use crate::crc::Crc32c;
use crate::fragment::{TRAILER_LEN, encode_trailer};

/// Movie timescale written by [`crate::init::build_init_segment`] (`mvhd`).
pub const MOVIE_TIMESCALE: u32 = 1000;

/// Why a fragment or init segment could not be rewritten.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ExportError {
    /// A box the rewrite needs is missing or malformed.
    #[error("malformed {0}")]
    Malformed(&'static str),
}

/// Rewrites one complete fragment in place (`prft`, `moof`, `mdat`, trailer):
/// the `mfhd` sequence number becomes `sequence`, the `tfdt` decode time and
/// the `prft` media time become `decode_time`, and the trailer is recomputed
/// over the rewritten bytes.
pub fn rebase_fragment(
    fragment: &mut [u8],
    sequence: u32,
    decode_time: u64,
) -> Result<(), ExportError> {
    // The whole fragment must parse first (`moof`, `mdat` whose size agrees
    // with the sample table, a trailer whose length matches), or a corrupt
    // file would be rewritten into another corrupt file with a valid trailer.
    if crate::reader::parse_fragment(fragment).is_none() {
        return Err(ExportError::Malformed("fragment"));
    }
    let mut pos = 0usize;
    let mut saw_moof = false;
    let body_end = fragment.len() - TRAILER_LEN;
    while pos < body_end {
        let (kind, header_len, size) = header(&fragment[pos..body_end], "fragment")?;
        match &kind {
            b"prft" => {
                // version/flags(4) track(4) ntp(8) media_time(8)
                put_u64(
                    &mut fragment[pos + header_len..pos + size],
                    16,
                    decode_time,
                    "prft",
                )?;
            }
            b"moof" => {
                rebase_moof(
                    &mut fragment[pos + header_len..pos + size],
                    sequence,
                    decode_time,
                )?;
                saw_moof = true;
            }
            _ => {}
        }
        pos += size;
    }
    if !saw_moof {
        return Err(ExportError::Malformed("moof"));
    }
    let mut crc = Crc32c::new();
    crc.update(&fragment[..body_end]);
    let len = u32::try_from(fragment.len()).map_err(|_| ExportError::Malformed("length"))?;
    fragment[body_end..].copy_from_slice(&encode_trailer(crc.finish(), len));
    Ok(())
}

fn rebase_moof(body: &mut [u8], sequence: u32, decode_time: u64) -> Result<(), ExportError> {
    let mut pos = 0usize;
    while pos < body.len() {
        let (kind, header_len, size) = header(&body[pos..], "moof")?;
        match &kind {
            b"mfhd" => put_u32(&mut body[pos + header_len..pos + size], 4, sequence, "mfhd")?,
            b"traf" => rebase_traf(&mut body[pos + header_len..pos + size], decode_time)?,
            _ => {}
        }
        pos += size;
    }
    Ok(())
}

fn rebase_traf(body: &mut [u8], decode_time: u64) -> Result<(), ExportError> {
    let mut pos = 0usize;
    while pos < body.len() {
        let (kind, header_len, size) = header(&body[pos..], "traf")?;
        if &kind == b"tfdt" {
            let tfdt = &mut body[pos + header_len..pos + size];
            match tfdt.first() {
                Some(1) => put_u64(tfdt, 4, decode_time, "tfdt")?,
                Some(0) => {
                    let small =
                        u32::try_from(decode_time).map_err(|_| ExportError::Malformed("tfdt"))?;
                    put_u32(tfdt, 4, small, "tfdt")?;
                }
                _ => return Err(ExportError::Malformed("tfdt")),
            }
        }
        pos += size;
    }
    Ok(())
}

/// Inserts an `edts`/`elst` (version 1, one entry) into the init segment's
/// `trak`: presentation starts `media_time` ticks into the media and lasts
/// `duration_ms` in the movie timescale. Returns the rewritten init segment.
pub fn with_edit_list(
    init: &[u8],
    media_time: u64,
    duration_ms: u64,
) -> Result<Vec<u8>, ExportError> {
    let mut w = BoxWriter::with_capacity(48);
    let edts = w.begin(b"edts");
    let elst = w.begin_full(b"elst", 1, 0);
    w.u32(1);
    w.u64(duration_ms);
    w.u64(media_time);
    w.i16(1);
    w.i16(0);
    w.end(elst);
    w.end(edts);
    let edts = w.into_vec();

    let moov = find_box(init, 0, init.len(), *b"moov", "moov")?;
    let trak = find_box(init, moov.body, moov.end, *b"trak", "trak")?;
    let tkhd = find_box(init, trak.body, trak.end, *b"tkhd", "tkhd")?;
    let grow = u32::try_from(edts.len()).map_err(|_| ExportError::Malformed("edts"))?;
    let mut out = Vec::with_capacity(init.len() + edts.len());
    out.extend_from_slice(&init[..tkhd.end]);
    out.extend_from_slice(&edts);
    out.extend_from_slice(&init[tkhd.end..]);
    for start in [moov.start, trak.start] {
        let size = be_u32(&out, start).ok_or(ExportError::Malformed("size"))?;
        let grown = size
            .checked_add(grow)
            .ok_or(ExportError::Malformed("size"))?;
        out[start..start + 4].copy_from_slice(&grown.to_be_bytes());
    }
    Ok(out)
}

struct Found {
    start: usize,
    body: usize,
    end: usize,
}

fn find_box(
    buf: &[u8],
    from: usize,
    to: usize,
    kind: [u8; 4],
    what: &'static str,
) -> Result<Found, ExportError> {
    let mut pos = from;
    while pos < to {
        let (k, header_len, size) = header(&buf[pos..to], what)?;
        if k == kind {
            return Ok(Found {
                start: pos,
                body: pos + header_len,
                end: pos + size,
            });
        }
        pos += size;
    }
    Err(ExportError::Malformed(what))
}

fn header(buf: &[u8], what: &'static str) -> Result<([u8; 4], usize, usize), ExportError> {
    let h = BoxHeader::parse(buf).ok_or(ExportError::Malformed(what))?;
    let size = usize::try_from(h.size.ok_or(ExportError::Malformed(what))?)
        .map_err(|_| ExportError::Malformed(what))?;
    let header_len = usize::from(h.header_len);
    if size < header_len || size > buf.len() {
        return Err(ExportError::Malformed(what));
    }
    Ok((h.kind, header_len, size))
}

fn put_u32(body: &mut [u8], at: usize, value: u32, what: &'static str) -> Result<(), ExportError> {
    body.get_mut(at..at + 4)
        .ok_or(ExportError::Malformed(what))?
        .copy_from_slice(&value.to_be_bytes());
    Ok(())
}

fn put_u64(body: &mut [u8], at: usize, value: u64, what: &'static str) -> Result<(), ExportError> {
    body.get_mut(at..at + 8)
        .ok_or(ExportError::Malformed(what))?
        .copy_from_slice(&value.to_be_bytes());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fragment::{FragmentHeader, Sample, encode_fragment};
    use crate::init::build_init_segment;
    use crate::reader::{ScanOptions, parse_fragment, scan};
    use bytes::Bytes;
    use iris_core::time::WallUs;

    fn fragment(sequence: u32, decode_time: u64) -> Vec<u8> {
        let samples = [
            Sample {
                data: Bytes::from_static(&[0, 0, 0, 2, 0x26, 1]),
                duration: 3000,
                is_sync: true,
            },
            Sample {
                data: Bytes::from_static(&[0, 0, 0, 3, 0x02, 1, 2]),
                duration: 3000,
                is_sync: false,
            },
        ];
        let header = FragmentHeader {
            sequence,
            decode_time,
            wall: WallUs::new(1_700_000_000_000_000),
        };
        let encoded = encode_fragment(&header, &samples).unwrap();
        let mut bytes = encoded.prefix.to_vec();
        for s in &samples {
            bytes.extend_from_slice(&s.data);
        }
        bytes.extend_from_slice(&encoded.trailer);
        bytes
    }

    #[test]
    fn rebased_fragments_carry_new_times_and_a_valid_trailer() {
        let mut bytes = fragment(900, 90_000 * 3_600);
        rebase_fragment(&mut bytes, 1, 6000).unwrap();
        let info = parse_fragment(&bytes).unwrap();
        assert_eq!(info.sequence, 1);
        assert_eq!(info.decode_time, 6000);
        assert_eq!(info.sample_sizes, vec![6, 7]);
        // The prft media time follows: scan the fragment behind an init segment with CRC checks.
        let mut file = build_init_segment(&crate::init::tests::fake_sample_entry()).to_vec();
        file.extend_from_slice(&bytes);
        let report = scan(
            std::io::Cursor::new(file),
            ScanOptions {
                verify_crc: true,
                collect_samples: false,
            },
        )
        .unwrap();
        assert_eq!(report.fragments.len(), 1);
        assert_eq!(report.stop, None);
        assert_eq!(report.fragments[0].decode_time, 6000);
        let mut short = bytes[..bytes.len() - 1].to_vec();
        assert_eq!(
            rebase_fragment(&mut short, 1, 0),
            Err(ExportError::Malformed("fragment"))
        );
    }

    #[test]
    fn the_edit_list_lands_after_tkhd_and_the_sizes_grow() {
        let init = build_init_segment(&crate::init::tests::fake_sample_entry());
        let out = with_edit_list(&init, 45_000, 5_500).unwrap();
        // edts header 8 + elst header 8 + version/flags 4 + count 4 + duration 8 + media time 8 + rate 4.
        assert_eq!(out.len(), init.len() + 44);
        let before = find_box(&init, 0, init.len(), *b"moov", "moov").unwrap();
        let before_trak = find_box(&init, before.body, before.end, *b"trak", "trak").unwrap();
        let moov = find_box(&out, 0, out.len(), *b"moov", "moov").unwrap();
        let trak = find_box(&out, moov.body, moov.end, *b"trak", "trak").unwrap();
        let tkhd = find_box(&out, trak.body, trak.end, *b"tkhd", "tkhd").unwrap();
        let edts = find_box(&out, trak.body, trak.end, *b"edts", "edts").unwrap();
        assert_eq!(edts.start, tkhd.end);
        let elst = find_box(&out, edts.body, edts.end, *b"elst", "elst").unwrap();
        let body = &out[elst.body..elst.end];
        assert_eq!(body[0], 1);
        assert_eq!(be_u32(body, 4), Some(1));
        assert_eq!(u64::from_be_bytes(body[8..16].try_into().unwrap()), 5_500);
        assert_eq!(u64::from_be_bytes(body[16..24].try_into().unwrap()), 45_000);
        assert_eq!(moov.end, out.len());
        assert_eq!(moov.end - moov.start, before.end - before.start + 44);
        assert_eq!(
            trak.end - trak.start,
            before_trak.end - before_trak.start + 44
        );
        // The result still scans as an init segment.
        let report = scan(
            std::io::Cursor::new(out.clone()),
            ScanOptions {
                verify_crc: false,
                collect_samples: false,
            },
        )
        .unwrap();
        assert_eq!(report.init_len as usize, out.len());
    }
}
