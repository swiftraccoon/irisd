//! Rewrites that turn stored fragments into a stand-alone export: a continuous
//! timeline from zero, fresh sequence numbers, a trailer over the rewritten
//! bytes, physical tail sample trimming, and an edit list in the init segment
//! that hides the first fragment's sync preroll.
//!
//! Stored decode times run on across the files of a recording run and
//! sequence numbers restart per file, so an export that copied fragments
//! verbatim would start hours into its own timeline and jump at every file
//! boundary. These functions rewrite boxes; retained sample data is never changed.

use bytes::Bytes;

use crate::boxes::{BoxHeader, BoxWriter, be_u32};
use crate::crc::Crc32c;
use crate::fragment::{
    FragmentError, FragmentHeader, Sample, TRAILER_LEN, decode_trailer, encode_fragment,
    encode_trailer,
};
pub use crate::init::MOVIE_TIMESCALE;
use crate::reader::FragmentInfo;
use iris_core::time::MediaTicks;

/// Why a fragment or init segment could not be rewritten.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum ExportError {
    /// A box the rewrite needs is missing or malformed, or the checksum does
    /// not match.
    #[error("malformed {0}")]
    Malformed(&'static str),
    /// The requested duration is zero or negative.
    #[error("export duration must be positive")]
    InvalidDuration,
    /// The edit list's media time does not fit the signed 64-bit field.
    #[error("edit list media time must fit a signed 64-bit field")]
    InvalidMediaTime,
    /// The trimmed prefix cannot be encoded as a recording fragment.
    #[error("encoding the trimmed fragment: {0}")]
    Encode(#[from] FragmentError),
}

/// Rewrites one complete fragment in place (`prft`, `moof`, `mdat`, trailer):
/// the `mfhd` sequence number becomes `sequence`, the `tfdt` decode time and
/// the `prft` media time become `decode_time` (90 kHz ticks, the `tfdt`
/// value), and the trailer is recomputed over the rewritten bytes. The
/// original checksum is verified before any bytes are changed; corrupt input
/// is rejected.
///
/// # Errors
///
/// [`ExportError::Malformed`] when the fragment does not parse, its checksum
/// does not match, or a box the rewrite needs is missing; a version-0 `tfdt`
/// that cannot hold `decode_time` is malformed too.
pub fn rebase_fragment(
    fragment: &mut [u8],
    sequence: u32,
    decode_time: u64,
) -> Result<(), ExportError> {
    validated_fragment(fragment)?;
    let body_end = fragment.len() - TRAILER_LEN;
    let mut pos = 0usize;
    let mut saw_moof = false;
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

fn validated_fragment(fragment: &[u8]) -> Result<FragmentInfo, ExportError> {
    // The whole fragment must parse first (`moof`, `mdat` whose size agrees
    // with the sample table, a trailer whose length matches), or a corrupt
    // file would be rewritten into another corrupt file with a valid trailer.
    let info = crate::reader::parse_fragment(fragment).ok_or(ExportError::Malformed("fragment"))?;
    let body_end = fragment.len() - TRAILER_LEN;
    let trailer = decode_trailer(&fragment[body_end..]).ok_or(ExportError::Malformed("trailer"))?;
    let mut original_crc = Crc32c::new();
    original_crc.update(&fragment[..body_end]);
    if original_crc.finish() != trailer.crc {
        return Err(ExportError::Malformed("checksum"));
    }
    Ok(info)
}

/// Rebuilds a checksum-verified fragment's prefix through `duration`, rebased
/// to `header`. Samples starting at or after that duration are omitted; a
/// crossing sample's duration is shortened. Retained sample bytes are unchanged.
///
/// The input allocation backs sample slices while the output is assembled;
/// sample count and payload remain bounded by the fragment parser and encoder.
///
/// # Errors
///
/// [`ExportError::Malformed`] for invalid structure or a checksum mismatch,
/// [`ExportError::InvalidDuration`] for a duration that is not positive, and
/// [`ExportError::Encode`] for a prefix the recording fragment encoder cannot
/// represent.
pub fn trim_fragment_end(
    fragment: Vec<u8>,
    header: FragmentHeader,
    duration: MediaTicks,
) -> Result<Vec<u8>, ExportError> {
    let info = validated_fragment(&fragment)?;
    let mut remaining = u64::try_from(duration.get()).map_err(|_| ExportError::InvalidDuration)?;
    if remaining == 0 {
        return Err(ExportError::InvalidDuration);
    }
    let data = Bytes::from(fragment);
    let mut offset = usize::try_from(info.payload_offset)
        .map_err(|_| ExportError::Malformed("payload offset"))?;
    let mut samples = Vec::with_capacity(usize::from(info.sample_count));
    for (index, (&size, &duration)) in info
        .sample_sizes
        .iter()
        .zip(&info.sample_durations)
        .enumerate()
    {
        if remaining == 0 {
            break;
        }
        let end = offset
            .checked_add(size as usize)
            .ok_or(ExportError::Malformed("sample size"))?;
        if end > data.len() {
            return Err(ExportError::Malformed("sample data"));
        }
        let duration = u32::try_from(remaining.min(u64::from(duration)))
            .map_err(|_| ExportError::Malformed("sample duration"))?;
        samples.push(Sample {
            data: data.slice(offset..end),
            duration,
            is_sync: index == 0 && info.starts_with_sync,
        });
        offset = end;
        remaining -= u64::from(duration);
    }
    let encoded = encode_fragment(&header, &samples)?;
    let mut out = Vec::with_capacity(
        usize::try_from(encoded.total_len()).map_err(|_| ExportError::Malformed("length"))?,
    );
    for slice in encoded.io_slices(&samples) {
        out.extend_from_slice(&slice);
    }
    Ok(out)
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
/// `trak`: presentation starts `media_time` ticks (90 kHz) into the media and
/// lasts `duration_ms` milliseconds, the movie timescale irisd writes
/// ([`MOVIE_TIMESCALE`]). Returns the rewritten init segment. The init segment
/// must be one [`crate::init::build_init_segment`] produced: its `mvhd`
/// timescale is not read. Extended-size `moov` or `trak` parents are rejected.
///
/// # Errors
///
/// [`ExportError::InvalidMediaTime`] when `media_time` does not fit the
/// signed `elst` field; [`ExportError::Malformed`] when `moov`, `trak`, or
/// `tkhd` is missing, a parent has an extended size, or a size would
/// overflow.
pub fn with_edit_list(
    init: &[u8],
    media_time: u64,
    duration_ms: u64,
) -> Result<Vec<u8>, ExportError> {
    if i64::try_from(media_time).is_err() {
        return Err(ExportError::InvalidMediaTime);
    }
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
    if moov.body - moov.start != 8 || trak.body - trak.start != 8 {
        return Err(ExportError::Malformed("extended init parent"));
    }
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
    fn rejects_payload_corruption_before_rewriting_the_checksum() {
        let mut bytes = fragment(7, 90_000);
        let payload_end = bytes.len() - TRAILER_LEN;
        bytes[payload_end - 1] ^= 0x80;
        let original = bytes.clone();
        assert!(rebase_fragment(&mut bytes, 1, 0).is_err());
        assert_eq!(bytes, original, "a failed rewrite leaves the input intact");
    }

    /// The rebased header every trimming test uses.
    fn trimmed_header(original: &FragmentInfo) -> FragmentHeader {
        FragmentHeader {
            sequence: 1,
            decode_time: 0,
            wall: original.wall.unwrap(),
        }
    }

    #[test]
    fn tail_trimming_preserves_sample_bytes_and_shortens_only_the_crossing_duration() {
        let bytes = fragment(7, 90_000);
        let original = parse_fragment(&bytes).unwrap();
        let payload = &bytes[original.payload_offset as usize..bytes.len() - TRAILER_LEN];
        let header = trimmed_header(&original);
        for (limit, sizes, durations) in [
            (3000, vec![6], vec![3000]),
            (4500, vec![6, 7], vec![3000, 1500]),
            (9000, vec![6, 7], vec![3000, 3000]),
        ] {
            let trimmed = trim_fragment_end(bytes.clone(), header, MediaTicks::new(limit)).unwrap();
            let info = parse_fragment(&trimmed).unwrap();
            assert_eq!(info.sample_sizes, sizes);
            assert_eq!(info.sample_durations, durations);
            assert_eq!(info.sequence, 1);
            assert_eq!(info.decode_time, 0);
            let kept = info.sample_sizes.iter().sum::<u32>() as usize;
            assert_eq!(
                &trimmed[info.payload_offset as usize..trimmed.len() - TRAILER_LEN],
                &payload[..kept]
            );
            let mut file = build_init_segment(&crate::init::tests::fake_sample_entry()).to_vec();
            file.extend_from_slice(&trimmed);
            let scan = scan(
                std::io::Cursor::new(file),
                ScanOptions {
                    verify_crc: true,
                    collect_samples: false,
                },
            )
            .unwrap();
            assert_eq!(scan.stop, None);
        }
    }

    #[test]
    fn tail_trimming_rejects_a_duration_that_is_not_positive() {
        let bytes = fragment(7, 90_000);
        let header = trimmed_header(&parse_fragment(&bytes).unwrap());
        for limit in [0, -1] {
            assert_eq!(
                trim_fragment_end(bytes.clone(), header, MediaTicks::new(limit)),
                Err(ExportError::InvalidDuration),
                "limit {limit}"
            );
        }
    }

    #[test]
    fn tail_trimming_rejects_payload_corruption() {
        let mut bytes = fragment(7, 90_000);
        let original = parse_fragment(&bytes).unwrap();
        bytes[original.payload_offset as usize] ^= 1;
        assert_eq!(
            trim_fragment_end(bytes, trimmed_header(&original), MediaTicks::new(3000)),
            Err(ExportError::Malformed("checksum"))
        );
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
    }

    #[test]
    fn rebasing_rejects_a_fragment_missing_its_last_byte() {
        let bytes = fragment(900, 90_000 * 3_600);
        let mut short = bytes[..bytes.len() - 1].to_vec();
        assert_eq!(
            rebase_fragment(&mut short, 1, 0),
            Err(ExportError::Malformed("fragment"))
        );
    }

    /// An `edts` box holding a version-1 `elst` with one entry: the two
    /// headers (8 + 8), version and flags (4), entry count (4), duration (8),
    /// media time (8), and rate (4).
    const EDIT_LIST_LEN: usize = 44;

    #[test]
    fn the_edit_list_lands_after_tkhd_and_the_sizes_grow() {
        let init = build_init_segment(&crate::init::tests::fake_sample_entry());
        let out = with_edit_list(&init, 45_000, 5_500).unwrap();
        assert_eq!(out.len(), init.len() + EDIT_LIST_LEN);
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
        assert_eq!(
            moov.end - moov.start,
            before.end - before.start + EDIT_LIST_LEN
        );
        assert_eq!(
            trak.end - trak.start,
            before_trak.end - before_trak.start + EDIT_LIST_LEN
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

    fn init_tree_valid(mut bytes: &[u8]) -> bool {
        while !bytes.is_empty() {
            let Some(header) = BoxHeader::parse(bytes) else {
                return false;
            };
            let Some(size) = header.size.and_then(|n| usize::try_from(n).ok()) else {
                return false;
            };
            if size < usize::from(header.header_len) || size > bytes.len() {
                return false;
            }
            if [*b"moov", *b"trak"].contains(&header.kind)
                && !init_tree_valid(&bytes[usize::from(header.header_len)..size])
            {
                return false;
            }
            bytes = &bytes[size..];
        }
        true
    }

    fn extend_parent(init: &[u8], kind: [u8; 4]) -> Vec<u8> {
        let moov = find_box(init, 0, init.len(), *b"moov", "moov").unwrap();
        let parent = if kind == *b"moov" {
            find_box(init, 0, init.len(), kind, "moov").unwrap()
        } else {
            find_box(init, moov.body, moov.end, kind, "trak").unwrap()
        };
        let mut out = init[..parent.start].to_vec();
        out.extend(1u32.to_be_bytes());
        out.extend(kind);
        out.extend(((parent.end - parent.start + 8) as u64).to_be_bytes());
        out.extend_from_slice(&init[parent.body..]);
        if kind == *b"trak" {
            let size = be_u32(&out, moov.start).unwrap() + 8;
            out[moov.start..moov.start + 4].copy_from_slice(&size.to_be_bytes());
        }
        out
    }

    #[test]
    fn edit_insertion_rejects_a_media_time_beyond_the_signed_field() {
        let init = build_init_segment(&crate::init::tests::fake_sample_entry());
        assert_eq!(
            with_edit_list(&init, u64::MAX, 33),
            Err(ExportError::InvalidMediaTime)
        );
        assert!(with_edit_list(&init, i64::MAX as u64, 33).is_ok());
    }

    #[test]
    fn edit_insertion_rejects_extended_parents_without_corrupt_output() {
        let init = build_init_segment(&crate::init::tests::fake_sample_entry());
        assert!(init_tree_valid(&with_edit_list(&init, 0, 33).unwrap()));
        let extended_trak = extend_parent(&init, *b"trak");
        for bytes in [
            extend_parent(&init, *b"moov"),
            extended_trak.clone(),
            extend_parent(&extended_trak, *b"moov"),
        ] {
            assert!(init_tree_valid(&bytes));
            assert!(with_edit_list(&bytes, 0, 33).is_err());
        }
    }
}
