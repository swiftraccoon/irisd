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
    /// The requested duration is zero or negative, or leaves no sample.
    #[error("export duration must be positive")]
    InvalidDuration,
    /// A fragment that must lose its start has a sample that is not a sync
    /// sample.
    #[error("a fragment with a sample that is not a sync sample cannot lose its start")]
    NotAllSync,
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

/// Moves one complete fragment to track `track_id`, in place: the `tfhd`
/// track id and the `prft` reference track id become `track_id`, and the
/// trailer is recomputed. A stored fragment names track 1; the audio track of
/// a file with two tracks is [`crate::init::AV_AUDIO_TRACK_ID`]. The original
/// checksum is verified before any bytes are changed.
///
/// # Errors
///
/// [`ExportError::Malformed`] when the fragment does not parse, its checksum
/// does not match, or it has no `tfhd`.
pub fn retrack_fragment(fragment: &mut [u8], track_id: u32) -> Result<(), ExportError> {
    validated_fragment(fragment)?;
    let body_end = fragment.len() - TRAILER_LEN;
    let mut pos = 0usize;
    let mut saw_tfhd = false;
    while pos < body_end {
        let (kind, header_len, size) = header(&fragment[pos..body_end], "fragment")?;
        match &kind {
            // version/flags(4) reference_track_ID(4)
            b"prft" => put_u32(
                &mut fragment[pos + header_len..pos + size],
                4,
                track_id,
                "prft",
            )?,
            b"moof" => {
                let moof = find_box(fragment, pos + header_len, pos + size, *b"traf", "traf")?;
                let tfhd = find_box(fragment, moof.body, moof.end, *b"tfhd", "tfhd")?;
                // version/flags(4) track_ID(4)
                put_u32(&mut fragment[tfhd.body..tfhd.end], 4, track_id, "tfhd")?;
                saw_tfhd = true;
            }
            _ => {}
        }
        pos += size;
    }
    if !saw_tfhd {
        return Err(ExportError::Malformed("tfhd"));
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
            is_sync: info.all_sync || (index == 0 && info.starts_with_sync),
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

/// What [`trim_fragment_start`] leaves of a fragment.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TrimmedStart {
    /// The fragment from its first retained sample on.
    pub fragment: Vec<u8>,
    /// The time the omitted samples covered, in 90 kHz ticks.
    pub omitted_ticks: u64,
}

/// Rebuilds a checksum-verified fragment from its first sample that starts
/// at or after `skip` ticks into it, and returns it with the ticks the
/// omitted samples covered. The remainder keeps the fragment's sequence
/// number and starts that many ticks later in decode time and in wall time;
/// retained samples are unchanged. Only a fragment whose samples are all
/// sync samples, as an audio fragment's are, can lose its start and still
/// decode.
///
/// An edit list would hide the same samples without rewriting the fragment,
/// but the `ffmpeg` demuxer reads the media time of an AAC track's edit as a
/// count of audio samples, which it is only when the track's timescale is
/// its sampling rate (`docs/research/13-audio.md` §9).
///
/// # Errors
///
/// [`ExportError::Malformed`] for invalid structure, a checksum mismatch, or
/// a fragment without a wall time; [`ExportError::NotAllSync`] for a
/// fragment with a sample that is not a sync sample;
/// [`ExportError::InvalidDuration`] when `skip` is negative or no sample
/// starts at or after it; and [`ExportError::Encode`] for a remainder the
/// recording fragment encoder cannot represent.
pub fn trim_fragment_start(
    fragment: Vec<u8>,
    skip: MediaTicks,
) -> Result<TrimmedStart, ExportError> {
    let info = validated_fragment(&fragment)?;
    if !info.all_sync {
        return Err(ExportError::NotAllSync);
    }
    let wall = info.wall.ok_or(ExportError::Malformed("prft"))?;
    let skip = u64::try_from(skip.get()).map_err(|_| ExportError::InvalidDuration)?;
    let data = Bytes::from(fragment);
    let mut offset = usize::try_from(info.payload_offset)
        .map_err(|_| ExportError::Malformed("payload offset"))?;
    let mut omitted = 0u64;
    let mut samples = Vec::with_capacity(usize::from(info.sample_count));
    for (&size, &duration) in info.sample_sizes.iter().zip(&info.sample_durations) {
        let end = offset
            .checked_add(size as usize)
            .ok_or(ExportError::Malformed("sample size"))?;
        if end > data.len() {
            return Err(ExportError::Malformed("sample data"));
        }
        if samples.is_empty() && omitted < skip {
            omitted += u64::from(duration);
        } else {
            samples.push(Sample {
                data: data.slice(offset..end),
                duration,
                is_sync: true,
            });
        }
        offset = end;
    }
    if samples.is_empty() {
        return Err(ExportError::InvalidDuration);
    }
    let later = i64::try_from(omitted).map_err(|_| ExportError::Malformed("sample duration"))?;
    let header = FragmentHeader {
        sequence: info.sequence,
        decode_time: info
            .decode_time
            .checked_add(omitted)
            .ok_or(ExportError::Malformed("tfdt"))?,
        wall: wall.offset(MediaTicks::new(later).as_micros()),
    };
    let encoded = encode_fragment(&header, &samples)?;
    let mut out = Vec::with_capacity(
        usize::try_from(encoded.total_len()).map_err(|_| ExportError::Malformed("length"))?,
    );
    for slice in encoded.io_slices(&samples) {
        out.extend_from_slice(&slice);
    }
    Ok(TrimmedStart {
        fragment: out,
        omitted_ticks: omitted,
    })
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
    with_edit_lists(
        init,
        &[TrackEdit {
            media_time,
            duration_ms,
        }],
    )
}

/// The one edit of a track's edit list, field for field as `elst` holds it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TrackEdit {
    /// Where presentation starts in the track's media, in 90 kHz ticks.
    pub media_time: u64,
    /// How long the presentation lasts, in milliseconds
    /// ([`MOVIE_TIMESCALE`]).
    pub duration_ms: u64,
}

/// Inserts an edit list, as [`with_edit_list`] does, into each of the init
/// segment's first `tracks.len()` tracks, in the order of the `trak` boxes: one edit
/// for each.
///
/// # Errors
///
/// As [`with_edit_list`]; also [`ExportError::Malformed`] when the init
/// segment has fewer tracks than `tracks`.
pub fn with_edit_lists(init: &[u8], tracks: &[TrackEdit]) -> Result<Vec<u8>, ExportError> {
    let moov = find_box(init, 0, init.len(), *b"moov", "moov")?;
    if moov.body - moov.start != 8 {
        return Err(ExportError::Malformed("extended init parent"));
    }
    let mut out = Vec::with_capacity(init.len() + 48 * tracks.len());
    let mut copied = 0;
    let mut from = moov.body;
    let mut grown_by = 0u32;
    // Where each rewritten `trak` starts in `out`, and by how much it grew.
    let mut grown = Vec::with_capacity(tracks.len());
    for &TrackEdit {
        media_time,
        duration_ms,
    } in tracks
    {
        if i64::try_from(media_time).is_err() {
            return Err(ExportError::InvalidMediaTime);
        }
        let trak = find_box(init, from, moov.end, *b"trak", "trak")?;
        if trak.body - trak.start != 8 {
            return Err(ExportError::Malformed("extended init parent"));
        }
        let tkhd = find_box(init, trak.body, trak.end, *b"tkhd", "tkhd")?;
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
        let grow = u32::try_from(edts.len()).map_err(|_| ExportError::Malformed("edts"))?;
        grown.push((trak.start + grown_by as usize, grow));
        out.extend_from_slice(&init[copied..tkhd.end]);
        out.extend_from_slice(&edts);
        copied = tkhd.end;
        from = trak.end;
        grown_by = grown_by
            .checked_add(grow)
            .ok_or(ExportError::Malformed("size"))?;
    }
    out.extend_from_slice(&init[copied..]);
    for (start, grow) in grown.into_iter().chain([(moov.start, grown_by)]) {
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
        // The prft media time follows the decode time: it sits behind the
        // box header, version and flags, track id, and NTP time.
        assert_eq!(&bytes[4..8], b"prft");
        assert_eq!(bytes[24..32], 6000u64.to_be_bytes());
        // Scan the fragment behind an init segment with CRC checks.
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

    /// The offset of the first box of `kind` at any depth of `buf`.
    fn box_offset(buf: &[u8], kind: [u8; 4]) -> usize {
        buf.windows(4).position(|window| window == kind).unwrap() - 4
    }

    /// Moving a fragment to track 2 changes the two track ids and the
    /// trailer and nothing else, and the result still parses with its
    /// checksum; a corrupt fragment is refused untouched.
    #[test]
    fn retracking_changes_the_track_ids_and_the_trailer_alone() {
        let original = fragment(7, 90_000);
        let mut moved = original.clone();
        retrack_fragment(&mut moved, 2).unwrap();
        assert_eq!(moved.len(), original.len());
        let tfhd = box_offset(&original, *b"tfhd") + 12;
        let prft = box_offset(&original, *b"prft") + 12;
        assert_eq!(original[tfhd..tfhd + 4], [0, 0, 0, 1]);
        assert_eq!(moved[tfhd..tfhd + 4], [0, 0, 0, 2]);
        assert_eq!(moved[prft..prft + 4], [0, 0, 0, 2]);
        let body_end = original.len() - TRAILER_LEN;
        let changed: Vec<usize> = (0..body_end)
            .filter(|&at| original[at] != moved[at])
            .collect();
        assert_eq!(changed, [prft + 3, tfhd + 3]);
        assert_ne!(original[body_end..], moved[body_end..], "the checksum");
        let info = parse_fragment(&moved).expect("the moved fragment parses");
        assert_eq!(
            (info.sequence, info.decode_time, info.sample_count),
            (7, 90_000, 2)
        );
        validated_fragment(&moved).expect("its checksum holds");
        // Moving it back restores the original bytes.
        retrack_fragment(&mut moved, 1).unwrap();
        assert_eq!(moved, original);

        let mut corrupt = original.clone();
        corrupt[body_end - 1] ^= 1;
        let before = corrupt.clone();
        assert_eq!(
            retrack_fragment(&mut corrupt, 2),
            Err(ExportError::Malformed("checksum"))
        );
        assert_eq!(corrupt, before);
    }

    /// Each track of a two-track init segment gets its own edit list after
    /// its `tkhd`, and the sizes of both tracks and of the movie grow by them.
    #[test]
    fn every_track_of_a_two_track_init_gets_its_edit_list() {
        use crate::init::{SampleEntry, build_av_init_segment};
        let video = SampleEntry::new(
            Bytes::from_static(b"\0\0\0\x10hvc1\0\0\0\0\0\0\0\0"),
            "hvc1".into(),
            16,
            16,
        );
        let audio = SampleEntry::audio(
            Bytes::from_static(b"\0\0\0\x10mp4a\0\0\0\0\0\0\0\0"),
            "mp4a.40.2".into(),
        );
        let init = build_av_init_segment(&video, &audio);
        let edit = |media_time, duration_ms| TrackEdit {
            media_time,
            duration_ms,
        };
        let edited = with_edit_lists(&init, &[edit(3_000, 2_000), edit(1_440, 1_984)]).unwrap();
        assert_eq!(edited.len(), init.len() + 2 * 44);
        let moov = find_box(&edited, 0, edited.len(), *b"moov", "moov").unwrap();
        assert_eq!(moov.end, edited.len());
        let mut from = moov.body;
        for (media_time, duration_ms) in [(3_000_u64, 2_000_u64), (1_440, 1_984)] {
            let trak = find_box(&edited, from, moov.end, *b"trak", "trak").unwrap();
            let tkhd = find_box(&edited, trak.body, trak.end, *b"tkhd", "tkhd").unwrap();
            let edts = find_box(&edited, trak.body, trak.end, *b"edts", "edts").unwrap();
            assert_eq!(
                edts.start, tkhd.end,
                "the edit list follows the track header"
            );
            let elst = &edited[edts.body + 8..edts.end];
            // version 1, one entry: duration, media time, rate 1.0.
            assert_eq!(elst[..8], [1, 0, 0, 0, 0, 0, 0, 1]);
            assert_eq!(elst[8..16], duration_ms.to_be_bytes());
            assert_eq!(elst[16..24], media_time.to_be_bytes());
            assert_eq!(elst[24..], [0, 1, 0, 0]);
            find_box(&edited, trak.body, trak.end, *b"mdia", "mdia").unwrap();
            from = trak.end;
        }
        find_box(&edited, from, moov.end, *b"mvex", "mvex").unwrap();
        // One edit touches the first track alone; three are one too many.
        let one = with_edit_lists(&init, &[edit(3_000, 2_000)]).unwrap();
        assert_eq!(one, with_edit_list(&init, 3_000, 2_000).unwrap());
        assert_eq!(
            with_edit_lists(&init, &[edit(0, 1), edit(0, 1), edit(0, 1)]),
            Err(ExportError::Malformed("trak"))
        );
    }

    /// A fragment of `count` sync samples of 1,440 ticks, each one byte
    /// holding its index, as an audio fragment is.
    fn sound(count: u8) -> (Vec<u8>, FragmentHeader) {
        let samples: Vec<Sample> = (0..count)
            .map(|index| Sample {
                data: Bytes::copy_from_slice(&[index]),
                duration: 1440,
                is_sync: true,
            })
            .collect();
        let header = FragmentHeader {
            sequence: 9,
            decode_time: 90_000,
            wall: WallUs::new(1_700_000_000_000_000),
        };
        let encoded = encode_fragment(&header, &samples).unwrap();
        let mut bytes = encoded.prefix.to_vec();
        for sample in &samples {
            bytes.extend_from_slice(&sample.data);
        }
        bytes.extend_from_slice(&encoded.trailer);
        (bytes, header)
    }

    /// The sample bytes of a fragment whose samples are one byte each.
    fn payload(fragment: &[u8]) -> Vec<u8> {
        let info = parse_fragment(fragment).unwrap();
        let from = usize::try_from(info.payload_offset).unwrap();
        fragment[from..from + usize::from(info.sample_count)].to_vec()
    }

    /// The start of an all-sync fragment is cut at a sample boundary: the
    /// first sample kept is the first that starts at or after the skip, and
    /// the remainder starts later by what was left out, in decode time and
    /// in wall time, with every sample still a sync sample.
    #[test]
    fn head_trimming_keeps_the_samples_that_start_at_or_after_the_skip() {
        let (bytes, header) = sound(5);
        for (skip, omitted, kept) in [
            (0, 0, &[0u8, 1, 2, 3, 4][..]),
            (1, 1440, &[1, 2, 3, 4]),
            (1440, 1440, &[1, 2, 3, 4]),
            (1441, 2880, &[2, 3, 4]),
            (5760, 5760, &[4]),
        ] {
            let TrimmedStart {
                fragment: rest,
                omitted_ticks: left_out,
            } = trim_fragment_start(bytes.clone(), MediaTicks::new(skip)).unwrap();
            assert_eq!(left_out, omitted, "skip {skip}");
            assert_eq!(payload(&rest), kept, "skip {skip}");
            let info = parse_fragment(&rest).unwrap();
            assert_eq!(info.sequence, header.sequence);
            assert_eq!(info.decode_time, header.decode_time + omitted);
            assert_eq!(
                info.wall,
                Some(
                    header
                        .wall
                        .offset(i64::try_from(omitted).unwrap() * 100 / 9)
                ),
                "skip {skip}"
            );
            assert_eq!(info.duration, 7200 - u32::try_from(omitted).unwrap());
            assert!(info.all_sync && info.starts_with_sync, "skip {skip}");
            validated_fragment(&rest).expect("the remainder's checksum holds");
        }
        // No sample starts at or after the last one's start plus a tick.
        for skip in [5761, 7200, 90_000] {
            assert_eq!(
                trim_fragment_start(bytes.clone(), MediaTicks::new(skip)),
                Err(ExportError::InvalidDuration),
                "skip {skip}"
            );
        }
        assert_eq!(
            trim_fragment_start(bytes.clone(), MediaTicks::new(-1)),
            Err(ExportError::InvalidDuration)
        );
        // A decode time that the cut would carry past the field is refused.
        let mut late = bytes.clone();
        rebase_fragment(&mut late, 9, u64::MAX - 1_000).unwrap();
        assert_eq!(
            trim_fragment_start(late.clone(), MediaTicks::new(1)),
            Err(ExportError::Malformed("tfdt"))
        );
        assert!(trim_fragment_start(late, MediaTicks::new(0)).is_ok());
        // A fragment that starts a group of pictures cannot lose its start.
        assert_eq!(
            trim_fragment_start(fragment(1, 0), MediaTicks::new(3000)),
            Err(ExportError::NotAllSync)
        );
        let mut corrupt = bytes;
        let last = corrupt.len() - TRAILER_LEN - 1;
        corrupt[last] ^= 1;
        assert_eq!(
            trim_fragment_start(corrupt, MediaTicks::new(1440)),
            Err(ExportError::Malformed("checksum"))
        );
    }

    /// Cutting the end of an all-sync fragment leaves every sample a sync
    /// sample; cutting the end of a group of pictures leaves the first alone.
    #[test]
    fn tail_trimming_keeps_the_sync_samples_of_the_fragment() {
        let (bytes, header) = sound(5);
        let cut = trim_fragment_end(bytes, header, MediaTicks::new(4000)).unwrap();
        let info = parse_fragment(&cut).unwrap();
        assert_eq!((info.sample_count, info.duration), (3, 4000));
        assert!(info.all_sync);
        let pictures = trim_fragment_end(fragment(1, 0), header, MediaTicks::new(6000)).unwrap();
        let info = parse_fragment(&pictures).unwrap();
        assert_eq!(info.sample_count, 2);
        assert!(info.starts_with_sync && !info.all_sync);
    }
}
