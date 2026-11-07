//! Scanning and validating recording files.
//!
//! The scanner walks `ftyp`, `moov`, then fragments, verifying box structure,
//! sample accounting, and trailers. It stops at the first inconsistency and
//! reports how many bytes are valid, which is exactly what crash recovery
//! needs to truncate a file that was open when power failed.
//!
//! A scan accepts at most a fixed number of fragments, by default
//! [`MAX_PERSISTED_INDEX_RECORDS`], the same ceiling every sidecar reader
//! applies, so the memory a scan holds is bounded whatever the file contains.
//! A complete fragment beyond that limit ends the scan with
//! [`ScanStop::FragmentLimit`].

use std::io::{self, Read, Seek, SeekFrom};

use crate::boxes::{BoxHeader, be_u32, be_u64};
use crate::crc::Crc32c;
use crate::fragment::{MAX_MOOF_LEN, MAX_PAYLOAD_LEN, TRAILER_LEN, decode_trailer};
use crate::index::MAX_PERSISTED_INDEX_RECORDS;
use iris_core::time::WallUs;

/// Facts about one fragment, as read from its boxes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FragmentInfo {
    /// Byte offset of the fragment (its `prft`, or `moof` if absent).
    pub offset: u64,
    /// Length through the end of the trailer.
    pub len: u32,
    /// Byte offset of the first sample's data inside `mdat`.
    pub payload_offset: u64,
    /// `mfhd` sequence number.
    pub sequence: u32,
    /// `tfdt` decode time.
    pub decode_time: u64,
    /// Sum of sample durations.
    pub duration: u32,
    /// Number of samples.
    pub sample_count: u16,
    /// Whether the first sample is a sync sample.
    pub starts_with_sync: bool,
    /// Wall time from `prft`, if present.
    pub wall: Option<WallUs>,
    /// Per-sample durations, when requested.
    pub sample_durations: Vec<u32>,
    /// Per-sample sizes, when requested.
    pub sample_sizes: Vec<u32>,
}

/// Why a scan stopped before the end of the file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ScanStop {
    /// The file ended inside a fragment.
    Truncated {
        /// Offset of the incomplete fragment.
        offset: u64,
    },
    /// The next box header is all zeros: preallocated, never written space.
    ZeroFill {
        /// Offset of the zero region.
        offset: u64,
    },
    /// A box of an unexpected type or size.
    BadBox {
        /// Offset of the box.
        offset: u64,
        /// What was found.
        kind: [u8; 4],
    },
    /// The `moof` was malformed or inconsistent with its `mdat`.
    BadFragment {
        /// Offset of the fragment.
        offset: u64,
        /// Explanation.
        reason: &'static str,
    },
    /// The trailer is missing, has the wrong magic, or a wrong length.
    BadTrailer {
        /// Offset of the fragment.
        offset: u64,
    },
    /// The CRC-32C did not match.
    CrcMismatch {
        /// Offset of the fragment.
        offset: u64,
    },
    /// A complete, valid fragment follows the last fragment the scan may
    /// accept. The file is not damaged at `offset`; it holds more fragments
    /// than one sidecar index can describe, so `valid_len` is not a
    /// truncation point.
    FragmentLimit {
        /// Offset of the first fragment beyond the limit.
        offset: u64,
        /// The number of fragments the scan accepted.
        limit: usize,
    },
}

/// What a scan found: the init segment length, every valid fragment, how many
/// bytes from the start are valid, and why the scan stopped if it did not reach
/// a clean end of file. Unless the scan stopped at
/// [`ScanStop::FragmentLimit`], `valid_len` is what crash recovery truncates to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScanReport {
    /// Length of completely accepted `ftyp` + `moov`, or zero for a rejected init.
    pub init_len: u32,
    /// Valid fragments in file order.
    pub fragments: Vec<FragmentInfo>,
    /// Bytes from the start of the file that are valid.
    pub valid_len: u64,
    /// Why the scan stopped, if not at a clean end of file.
    pub stop: Option<ScanStop>,
}

/// What a scan reads. The default checks structure only, which is enough to
/// index a file; recovery and self-checks also verify every trailer CRC.
#[derive(Clone, Copy, Debug, Default)]
pub struct ScanOptions {
    /// Read every payload and verify the trailer CRC (slower; used for recovery).
    pub verify_crc: bool,
    /// Keep per-sample durations and sizes in [`FragmentInfo`].
    pub collect_samples: bool,
}

/// Scans a recording, accepting at most [`MAX_PERSISTED_INDEX_RECORDS`]
/// fragments.
///
/// # Errors
///
/// Propagates reader failures; malformed content ends the scan with a
/// [`ScanStop`] instead.
pub fn scan<R: Read + Seek>(reader: R, options: ScanOptions) -> io::Result<ScanReport> {
    scan_with_limit(reader, options, MAX_PERSISTED_INDEX_RECORDS)
}

/// Scans a recording, accepting at most `max_fragments` fragments.
///
/// When another complete, valid fragment follows the last accepted one, the
/// report stops with [`ScanStop::FragmentLimit`] at that fragment's offset. A
/// damaged region right after the limit reports its own stop reason instead,
/// so a file holding exactly `max_fragments` fragments and a torn tail is
/// reported like any other torn file.
///
/// # Errors
///
/// Propagates reader failures; malformed content ends the scan with a
/// [`ScanStop`] instead.
pub fn scan_with_limit<R: Read + Seek>(
    mut reader: R,
    options: ScanOptions,
    max_fragments: usize,
) -> io::Result<ScanReport> {
    let file_len = reader.seek(SeekFrom::End(0))?;
    reader.seek(SeekFrom::Start(0))?;

    let init_len = match scan_initialization(&mut reader, file_len)? {
        Ok(init_len) => init_len,
        Err(stop) => {
            return Ok(ScanReport {
                init_len: 0,
                fragments: Vec::new(),
                valid_len: 0,
                stop: Some(stop),
            });
        }
    };
    scan_fragments(
        &mut reader,
        file_len,
        init_len,
        u64::from(init_len),
        options,
        max_fragments,
    )
}

/// Validates the initialization and scans fragments beginning at `start`.
///
/// Bytes between the initialization and `start` are not read. The caller must
/// establish that `start` is a trusted fragment boundary, for example from a
/// contiguous sidecar index whose last record is then validated by this scan.
/// Like [`scan`], it accepts at most [`MAX_PERSISTED_INDEX_RECORDS`]
/// fragments, counted from `start`.
///
/// # Errors
///
/// Returns an invalid-input error when `start` is before the end of the
/// initialization or past the file length, and propagates reader failures.
pub fn scan_from_offset<R: Read + Seek>(
    mut reader: R,
    start: u64,
    options: ScanOptions,
) -> io::Result<ScanReport> {
    let file_len = reader.seek(SeekFrom::End(0))?;
    reader.seek(SeekFrom::Start(0))?;
    let init_len = match scan_initialization(&mut reader, file_len)? {
        Ok(init_len) => init_len,
        Err(stop) => {
            return Ok(ScanReport {
                init_len: 0,
                fragments: Vec::new(),
                valid_len: 0,
                stop: Some(stop),
            });
        }
    };
    if start < u64::from(init_len) || start > file_len {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "scan offset is outside the fragment region",
        ));
    }
    scan_fragments(
        &mut reader,
        file_len,
        init_len,
        start,
        options,
        MAX_PERSISTED_INDEX_RECORDS,
    )
}

/// Reads and validates exactly one fragment at `start` without following it
/// through the remainder of the file.
///
/// This is intended for binding a bounded sidecar cache to selected physical
/// media facts. It does not validate the initialization segment; callers that
/// trust initialization facts must also validate those independently.
pub fn scan_fragment_at<R: Read + Seek>(
    mut reader: R,
    start: u64,
    options: ScanOptions,
) -> io::Result<Option<FragmentInfo>> {
    let file_len = reader.seek(SeekFrom::End(0))?;
    match scan_fragment(&mut reader, start, file_len, options)? {
        Ok(info) => Ok(Some(info)),
        Err(_) => Ok(None),
    }
}

fn scan_initialization<R: Read + Seek>(
    reader: &mut R,
    file_len: u64,
) -> io::Result<Result<u32, ScanStop>> {
    let mut offset = 0u64;
    for expected in [*b"ftyp", *b"moov"] {
        let header = match read_header(reader, offset, file_len)? {
            Some(header) if header.kind == expected => header,
            Some(header) => {
                return Ok(Err(ScanStop::BadBox {
                    offset,
                    kind: header.kind,
                }));
            }
            None => return Ok(Err(ScanStop::Truncated { offset })),
        };
        let size = header.size.unwrap_or(file_len - offset);
        // A declared size past the end of the file (a 64-bit `largesize` among them)
        // must stop the scan, not overflow the offset.
        if size < u64::from(header.header_len) || size > file_len - offset {
            return Ok(Err(ScanStop::Truncated { offset }));
        }
        offset += size;
    }
    let init_len = u32::try_from(offset).map_err(|_| io::Error::other("init segment too large"))?;
    Ok(Ok(init_len))
}

fn scan_fragments<R: Read + Seek>(
    reader: &mut R,
    file_len: u64,
    init_len: u32,
    mut offset: u64,
    options: ScanOptions,
    max_fragments: usize,
) -> io::Result<ScanReport> {
    let mut report = ScanReport {
        init_len,
        fragments: Vec::new(),
        valid_len: offset,
        stop: None,
    };
    loop {
        match scan_fragment(reader, offset, file_len, options)? {
            Ok(_) if report.fragments.len() >= max_fragments => {
                report.stop = Some(ScanStop::FragmentLimit {
                    offset,
                    limit: max_fragments,
                });
                return Ok(report);
            }
            Ok(info) => {
                offset += u64::from(info.len);
                report.valid_len = offset;
                report.fragments.push(info);
                if offset == file_len {
                    return Ok(report);
                }
            }
            Err(stop) => {
                report.stop = Some(stop);
                return Ok(report);
            }
        }
    }
}

fn read_exact_at<R: Read + Seek>(reader: &mut R, offset: u64, buf: &mut [u8]) -> io::Result<()> {
    reader.seek(SeekFrom::Start(offset))?;
    reader.read_exact(buf)
}

fn read_header<R: Read + Seek>(
    reader: &mut R,
    offset: u64,
    file_len: u64,
) -> io::Result<Option<BoxHeader>> {
    if offset + 8 > file_len {
        return Ok(None);
    }
    let mut buf = [0u8; 16];
    let n = usize::try_from((file_len - offset).min(16)).expect("bounded");
    read_exact_at(reader, offset, &mut buf[..n])?;
    Ok(BoxHeader::parse(&buf[..n]))
}

#[allow(clippy::too_many_lines)]
fn scan_fragment<R: Read + Seek>(
    reader: &mut R,
    start: u64,
    file_len: u64,
    options: ScanOptions,
) -> io::Result<Result<FragmentInfo, ScanStop>> {
    let mut offset = start;
    let mut header_buf = [0u8; 8];
    if offset + 8 > file_len {
        return Ok(Err(ScanStop::Truncated { offset }));
    }
    read_exact_at(reader, offset, &mut header_buf)?;
    if header_buf == [0u8; 8] {
        return Ok(Err(ScanStop::ZeroFill { offset }));
    }

    // Optional prft.
    let mut wall = None;
    let Some(first) = BoxHeader::parse(&header_buf) else {
        return Ok(Err(ScanStop::BadBox {
            offset,
            kind: [header_buf[4], header_buf[5], header_buf[6], header_buf[7]],
        }));
    };
    let mut crc_start = offset;
    if first.kind == *b"prft" {
        let Some(size) = first.size.filter(|s| (24..=64).contains(s)) else {
            return Ok(Err(ScanStop::BadBox {
                offset,
                kind: first.kind,
            }));
        };
        if offset + size > file_len {
            return Ok(Err(ScanStop::Truncated { offset }));
        }
        let mut prft = vec![0u8; size as usize];
        read_exact_at(reader, offset, &mut prft)?;
        let version = prft[8];
        wall = if version == 1 {
            be_u64(&prft, 16).map(WallUs::from_ntp64)
        } else {
            be_u32(&prft, 16).map(|_| WallUs::from_ntp64(be_u64(&prft, 16).unwrap_or(0)))
        };
        offset += size;
        crc_start = start;
        if offset + 8 > file_len {
            return Ok(Err(ScanStop::Truncated { offset: start }));
        }
        read_exact_at(reader, offset, &mut header_buf)?;
    }

    // moof
    let Some(moof) = BoxHeader::parse(&header_buf).filter(|h| h.kind == *b"moof") else {
        return Ok(Err(ScanStop::BadBox {
            offset,
            kind: [header_buf[4], header_buf[5], header_buf[6], header_buf[7]],
        }));
    };
    let Some(moof_size) = moof.size.filter(|s| *s <= MAX_MOOF_LEN as u64) else {
        return Ok(Err(ScanStop::BadFragment {
            offset: start,
            reason: "moof size out of range",
        }));
    };
    if offset + moof_size > file_len {
        return Ok(Err(ScanStop::Truncated { offset: start }));
    }
    let mut moof_buf = vec![0u8; moof_size as usize];
    read_exact_at(reader, offset, &mut moof_buf)?;
    let parsed = match parse_moof(&moof_buf, options.collect_samples) {
        Ok(p) => p,
        Err(reason) => {
            return Ok(Err(ScanStop::BadFragment {
                offset: start,
                reason,
            }));
        }
    };
    offset += moof_size;

    // mdat
    if offset + 8 > file_len {
        return Ok(Err(ScanStop::Truncated { offset: start }));
    }
    read_exact_at(reader, offset, &mut header_buf)?;
    let Some(mdat) = BoxHeader::parse(&header_buf).filter(|h| h.kind == *b"mdat") else {
        return Ok(Err(ScanStop::BadFragment {
            offset: start,
            reason: "mdat missing",
        }));
    };
    let Some(mdat_size) = mdat.size else {
        return Ok(Err(ScanStop::BadFragment {
            offset: start,
            reason: "open-ended mdat",
        }));
    };
    let payload_len = mdat_size.saturating_sub(u64::from(mdat.header_len));
    if payload_len != parsed.total_size || payload_len > MAX_PAYLOAD_LEN {
        return Ok(Err(ScanStop::BadFragment {
            offset: start,
            reason: "mdat size disagrees with trun",
        }));
    }
    let payload_offset = offset + u64::from(mdat.header_len);
    if u64::try_from(parsed.data_offset).ok() != Some(moof_size + u64::from(mdat.header_len)) {
        return Ok(Err(ScanStop::BadFragment {
            offset: start,
            reason: "trun data offset does not address mdat payload",
        }));
    }
    offset += mdat_size;

    // trailer
    if offset + TRAILER_LEN as u64 > file_len {
        return Ok(Err(ScanStop::Truncated { offset: start }));
    }
    let mut trailer_buf = [0u8; TRAILER_LEN];
    read_exact_at(reader, offset, &mut trailer_buf)?;
    let Some(trailer) = decode_trailer(&trailer_buf) else {
        return Ok(Err(ScanStop::BadTrailer { offset: start }));
    };
    offset += TRAILER_LEN as u64;
    if u64::from(trailer.fragment_len) != offset - start {
        return Ok(Err(ScanStop::BadTrailer { offset: start }));
    }

    if options.verify_crc {
        let mut crc = Crc32c::new();
        let mut remaining = payload_offset - crc_start + payload_len;
        let mut pos = crc_start;
        let mut buf = vec![0u8; 1 << 16];
        while remaining > 0 {
            let n = usize::try_from(remaining.min(buf.len() as u64)).expect("bounded");
            read_exact_at(reader, pos, &mut buf[..n])?;
            crc.update(&buf[..n]);
            pos += n as u64;
            remaining -= n as u64;
        }
        if crc.finish() != trailer.crc {
            return Ok(Err(ScanStop::CrcMismatch { offset: start }));
        }
    }

    Ok(Ok(FragmentInfo {
        offset: start,
        len: trailer.fragment_len,
        payload_offset,
        sequence: parsed.sequence,
        decode_time: parsed.decode_time,
        duration: parsed.total_duration,
        sample_count: parsed.sample_count,
        starts_with_sync: parsed.starts_with_sync,
        wall,
        sample_durations: parsed.durations,
        sample_sizes: parsed.sizes,
    }))
}

/// Parses one complete fragment held in memory (optional `prft`, one `moof`,
/// one `mdat`, final trailer) with its sample tables filled, without verifying the CRC. `offset`
/// is 0. Returns `None` for anything malformed, including a trailer whose
/// length disagrees with the buffer. For a serving path that has read one
/// fragment out of a file; the scanner is for whole files.
/// Both readers require `default-base-is-moof` and an explicit `trun` data
/// offset to the adjacent `mdat` payload, with every declared table field present.
#[must_use]
pub fn parse_fragment(buf: &[u8]) -> Option<FragmentInfo> {
    let mut pos = 0usize;
    let mut wall = None;
    let mut parsed = None;
    let mut payload = None;
    let mut moof_offset = 0;
    let mut trailer = None;
    while pos < buf.len() {
        let header = BoxHeader::parse(&buf[pos..])?;
        let size = usize::try_from(header.size?).ok()?;
        let header_len = usize::from(header.header_len);
        if size < header_len || size > buf.len() - pos {
            return None;
        }
        let full = &buf[pos..pos + size];
        let body = &full[header_len..];
        match &header.kind {
            b"prft" if pos == 0 => wall = Some(WallUs::from_ntp64(be_u64(body, 8)?)),
            b"moof" if parsed.is_none() && payload.is_none() => {
                moof_offset = pos;
                parsed = Some(parse_moof(full, true).ok()?);
            }
            b"mdat" if parsed.is_some() && payload.is_none() => {
                payload = Some((pos + header_len, body.len() as u64));
            }
            b"free" if payload.is_some() && size == TRAILER_LEN && pos + size == buf.len() => {
                trailer = Some(decode_trailer(full)?);
            }
            _ => return None,
        }
        pos += size;
    }
    let parsed = parsed?;
    let (payload_offset, payload_len) = payload?;
    if payload_len != parsed.total_size
        || usize::try_from(parsed.data_offset).ok()? != payload_offset - moof_offset
    {
        return None;
    }
    let trailer = trailer?;
    if usize::try_from(trailer.fragment_len).ok()? != buf.len() {
        return None;
    }
    Some(FragmentInfo {
        offset: 0,
        len: trailer.fragment_len,
        payload_offset: payload_offset as u64,
        sequence: parsed.sequence,
        decode_time: parsed.decode_time,
        duration: parsed.total_duration,
        sample_count: parsed.sample_count,
        starts_with_sync: parsed.starts_with_sync,
        wall,
        sample_durations: parsed.durations,
        sample_sizes: parsed.sizes,
    })
}

struct ParsedMoof {
    data_offset: i32,
    sequence: u32,
    decode_time: u64,
    total_duration: u32,
    total_size: u64,
    sample_count: u16,
    starts_with_sync: bool,
    durations: Vec<u32>,
    sizes: Vec<u32>,
}

fn children(buf: &[u8]) -> impl Iterator<Item = (BoxHeader, &[u8])> {
    let mut pos = 0usize;
    std::iter::from_fn(move || {
        let header = BoxHeader::parse(&buf[pos..])?;
        let size = usize::try_from(header.size?).ok()?;
        // `size > buf.len() - pos` rather than `pos + size > buf.len()`: a
        // declared size near `usize::MAX` must not overflow the addition.
        if size < usize::from(header.header_len) || size > buf.len() - pos {
            return None;
        }
        let body = &buf[pos + usize::from(header.header_len)..pos + size];
        pos += size;
        Some((header, body))
    })
}

#[allow(clippy::too_many_lines)]
fn parse_moof(moof: &[u8], collect_samples: bool) -> Result<ParsedMoof, &'static str> {
    let body = &moof[8..];
    let mut sequence = None;
    let mut decode_time = None;
    let mut default_flags = None;
    let mut default_base_is_moof = false;
    let mut trun = None;
    for (h, b) in children(body) {
        match &h.kind {
            b"mfhd" => sequence = be_u32(b, 4),
            b"traf" => {
                for (th, tb) in children(b) {
                    match &th.kind {
                        b"tfhd" => {
                            let flags = be_u32(tb, 0).ok_or("short tfhd")? & 0x00ff_ffff;
                            if flags & 0x0002_0000 == 0 || flags & 0x1 != 0 {
                                return Err("unsupported trun data base");
                            }
                            default_base_is_moof = true;
                            let mut pos = 8;
                            if flags & 0x2 != 0 {
                                pos += 4;
                            }
                            if flags & 0x8 != 0 {
                                pos += 4;
                            }
                            if flags & 0x10 != 0 {
                                pos += 4;
                            }
                            if flags & 0x20 != 0 {
                                default_flags = be_u32(tb, pos);
                            }
                        }
                        b"tfdt" => {
                            decode_time = if tb.first() == Some(&1) {
                                be_u64(tb, 4)
                            } else {
                                be_u32(tb, 4).map(u64::from)
                            };
                        }
                        b"trun" => trun = Some(tb),
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }
    if !default_base_is_moof {
        return Err("tfhd missing");
    }
    let trun = trun.ok_or("trun missing")?;
    let flags = be_u32(trun, 0).ok_or("short trun")? & 0x00ff_ffff;
    let count = be_u32(trun, 4).ok_or("short trun")?;
    let sample_count = u16::try_from(count).map_err(|_| "too many samples")?;
    let mut pos = 8;
    if flags & 0x1 == 0 {
        return Err("trun data offset missing");
    }
    let data_offset = be_u32(trun, pos).ok_or("trun truncated")? as i32;
    pos += 4;
    let mut first_flags = None;
    if flags & 0x4 != 0 {
        first_flags = Some(be_u32(trun, pos).ok_or("trun truncated")?);
        pos += 4;
    }
    if flags & 0x100 == 0 || flags & 0x200 == 0 {
        return Err("trun lacks per-sample duration or size");
    }
    let mut total_duration = 0u32;
    let mut total_size = 0u64;
    let mut durations = Vec::new();
    let mut sizes = Vec::new();
    let mut first_sample_flags_in_table = None;
    for i in 0..count {
        let duration = be_u32(trun, pos).ok_or("trun truncated")?;
        pos += 4;
        let size = be_u32(trun, pos).ok_or("trun truncated")?;
        pos += 4;
        if flags & 0x400 != 0 {
            let sample_flags = be_u32(trun, pos).ok_or("trun truncated")?;
            if i == 0 {
                first_sample_flags_in_table = Some(sample_flags);
            }
            pos += 4;
        }
        if flags & 0x800 != 0 {
            be_u32(trun, pos).ok_or("trun truncated")?;
            pos += 4;
        }
        total_duration = total_duration
            .checked_add(duration)
            .ok_or("duration overflow")?;
        total_size += u64::from(size);
        if collect_samples {
            durations.push(duration);
            sizes.push(size);
        }
    }
    let effective = first_flags
        .or(first_sample_flags_in_table)
        .or(default_flags)
        .unwrap_or(0);
    let starts_with_sync = effective & 0x0001_0000 == 0;
    Ok(ParsedMoof {
        data_offset,
        sequence: sequence.ok_or("mfhd missing")?,
        decode_time: decode_time.ok_or("tfdt missing")?,
        total_duration,
        total_size,
        sample_count,
        starts_with_sync,
        durations,
        sizes,
    })
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use bytes::Bytes;

    use super::*;

    #[test]
    fn parse_fragment_reads_an_encoded_fragment_back_and_rejects_a_short_one() {
        use crate::fragment::{FragmentHeader, Sample, encode_fragment};
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
            sequence: 7,
            decode_time: 90_000,
            wall: WallUs::new(1_700_000_000_000_000),
        };
        let encoded = encode_fragment(&header, &samples).unwrap();
        let mut bytes = encoded.prefix.to_vec();
        for s in &samples {
            bytes.extend_from_slice(&s.data);
        }
        bytes.extend_from_slice(&encoded.trailer);
        let info = parse_fragment(&bytes).unwrap();
        assert_eq!(info.sequence, 7);
        assert_eq!(info.decode_time, 90_000);
        assert_eq!(info.sample_count, 2);
        assert!(info.starts_with_sync);
        assert_eq!(info.sample_sizes, vec![6, 7]);
        assert_eq!(info.sample_durations, vec![3000, 3000]);
        assert_eq!(info.wall, Some(header.wall));
        assert_eq!(
            &bytes[info.payload_offset as usize..][..6],
            &samples[0].data[..]
        );
        assert_eq!(info.len as usize, bytes.len());
        assert!(parse_fragment(&bytes[..bytes.len() - 1]).is_none());
        assert!(parse_fragment(&bytes[1..]).is_none());
    }

    use crate::fragment::{FragmentHeader, Sample, encode_fragment};
    use crate::init::build_init_segment;
    use crate::init::tests::fake_sample_entry;

    fn file_with(n: usize) -> Vec<u8> {
        let mut file = build_init_segment(&fake_sample_entry()).to_vec();
        for i in 0..n {
            let samples = vec![
                Sample {
                    data: Bytes::from(vec![0, 0, 0, 2, 0x26, i as u8]),
                    duration: 3000,
                    is_sync: true,
                },
                Sample {
                    data: Bytes::from(vec![0, 0, 0, 3, 0x02, 1, 7]),
                    duration: 2990,
                    is_sync: false,
                },
            ];
            let enc = encode_fragment(
                &FragmentHeader {
                    sequence: i as u32 + 1,
                    decode_time: i as u64 * 5990,
                    wall: WallUs::new(1_000_000 * i as i64),
                },
                &samples,
            )
            .unwrap();
            file.extend_from_slice(&enc.prefix);
            for s in &samples {
                file.extend_from_slice(&s.data);
            }
            file.extend_from_slice(&enc.trailer);
        }
        file
    }

    fn fragment_bytes(sync: bool) -> Vec<u8> {
        let samples = [Sample {
            data: Bytes::from_static(&[0, 0, 0, 2, 0x26, 1]),
            duration: 3000,
            is_sync: sync,
        }];
        let encoded = encode_fragment(
            &FragmentHeader {
                sequence: 1,
                decode_time: 0,
                wall: WallUs::new(0),
            },
            &samples,
        )
        .unwrap();
        let mut bytes = Vec::new();
        for slice in encoded.io_slices(&samples) {
            bytes.extend_from_slice(&slice);
        }
        bytes
    }

    fn box_at(bytes: &[u8], kind: [u8; 4]) -> usize {
        bytes.windows(4).position(|b| b == kind).unwrap() - 4
    }

    fn repair_crc(bytes: &mut [u8]) {
        let end = bytes.len() - TRAILER_LEN;
        let trailer =
            crate::fragment::encode_trailer(crate::crc::crc32c(&bytes[..end]), bytes.len() as u32);
        bytes[end..].copy_from_slice(&trailer);
    }

    fn assert_fragment_rejected(bytes: &[u8]) {
        assert!(parse_fragment(bytes).is_none());
        assert!(crate::export::rebase_fragment(&mut bytes.to_vec(), 2, 3000).is_err());
        let mut file = file_with(1);
        let valid_len = file.len() as u64;
        file.extend_from_slice(bytes);
        let report = scan(
            Cursor::new(file),
            ScanOptions {
                verify_crc: true,
                collect_samples: true,
            },
        )
        .unwrap();
        assert_eq!(report.fragments.len(), 1);
        assert_eq!(report.valid_len, valid_len);
        assert!(report.stop.is_some());
    }

    #[test]
    fn incomplete_initialization_has_no_recoverable_prefix() {
        let init = build_init_segment(&fake_sample_entry());
        let ftyp = BoxHeader::parse(&init).unwrap().size.unwrap() as usize;
        let mut oversized = init[..ftyp + 8].to_vec();
        oversized[ftyp..ftyp + 4].copy_from_slice(&u32::MAX.to_be_bytes());
        for bytes in [
            init[..ftyp].to_vec(),
            [init[..ftyp].to_vec(), b"\0\0\0\x08free".to_vec()].concat(),
            init[..ftyp + 12].to_vec(),
            oversized,
        ] {
            let report = scan(Cursor::new(bytes), ScanOptions::default()).unwrap();
            assert_eq!(report.init_len, 0);
            assert_eq!(report.valid_len, 0);
            assert!(report.stop.is_some());
        }
        for suffix in [vec![], vec![0, 0, 0, 32, b'p', b'r', b'f', b't']] {
            let mut file = init.to_vec();
            file.extend(suffix);
            let report = scan(Cursor::new(file), ScanOptions::default()).unwrap();
            assert_eq!(report.init_len as usize, init.len());
            assert_eq!(report.valid_len as usize, init.len());
        }
    }

    #[test]
    fn memory_parser_requires_the_supported_top_level_sequence() {
        let normal = fragment_bytes(true);
        let moof = box_at(&normal, *b"moof");
        let mdat = box_at(&normal, *b"mdat");
        let end = normal.len() - TRAILER_LEN;
        let prft = &normal[..moof];
        let moof_bytes = &normal[moof..mdat];
        let mdat_bytes = &normal[mdat..end];
        let trailer = &normal[end..];
        for parts in [
            vec![prft, mdat_bytes, moof_bytes, trailer],
            vec![prft, moof_bytes, moof_bytes, mdat_bytes, trailer],
            vec![prft, moof_bytes, prft, mdat_bytes, trailer],
            vec![prft, moof_bytes, mdat_bytes, mdat_bytes, trailer],
            vec![prft, moof_bytes, b"\0\0\0\x08skip", mdat_bytes, trailer],
            vec![prft, moof_bytes, trailer, mdat_bytes, trailer],
        ] {
            let mut bytes = parts.concat();
            repair_crc(&mut bytes);
            assert_fragment_rejected(&bytes);
        }
        let mut extra_trailer = [normal.as_slice(), trailer].concat();
        repair_crc(&mut extra_trailer);
        assert!(parse_fragment(&extra_trailer).is_none());
        assert!(crate::export::rebase_fragment(&mut extra_trailer, 2, 3000).is_err());
        assert_fragment_rejected(&normal[..end]);
        let mut no_prft = normal[moof..].to_vec();
        repair_crc(&mut no_prft);
        for bytes in [&normal, &no_prft] {
            assert!(parse_fragment(bytes).is_some());
            assert!(crate::export::rebase_fragment(&mut bytes.clone(), 2, 3000).is_ok());
            let mut file = build_init_segment(&fake_sample_entry()).to_vec();
            file.extend(bytes);
            let report = scan(
                Cursor::new(file),
                ScanOptions {
                    verify_crc: true,
                    collect_samples: true,
                },
            )
            .unwrap();
            assert!(report.stop.is_none());
        }
    }

    #[test]
    fn trun_addressing_must_resolve_to_the_adjacent_payload() {
        let normal = fragment_bytes(true);
        let trun = box_at(&normal, *b"trun");
        for offset in [0_i32, -1, 1, 1000] {
            let mut bytes = normal.clone();
            bytes[trun + 16..trun + 20].copy_from_slice(&offset.to_be_bytes());
            repair_crc(&mut bytes);
            assert_fragment_rejected(&bytes);
        }
        for (kind, clear, set) in [
            (*b"trun", 1, 0),
            (*b"tfhd", 0x0002_0000, 0),
            (*b"tfhd", 0, 1),
        ] {
            let mut bytes = normal.clone();
            let at = box_at(&bytes, kind) + 8;
            let flags = (be_u32(&bytes, at).unwrap() & !clear) | set;
            bytes[at..at + 4].copy_from_slice(&flags.to_be_bytes());
            repair_crc(&mut bytes);
            assert_fragment_rejected(&bytes);
        }
    }

    #[test]
    fn declared_optional_sample_fields_must_be_complete() {
        for flag in [0x400, 0x800] {
            for extra in 0..=4 {
                let mut bytes = fragment_bytes(false);
                let trun = box_at(&bytes, *b"trun");
                let flags = be_u32(&bytes, trun + 8).unwrap() | flag;
                bytes[trun + 8..trun + 12].copy_from_slice(&flags.to_be_bytes());
                let table_end = trun + be_u32(&bytes, trun).unwrap() as usize;
                bytes.splice(table_end..table_end, vec![0; extra]);
                for kind in [*b"moof", *b"traf", *b"trun"] {
                    let at = box_at(&bytes, kind);
                    let size = be_u32(&bytes, at).unwrap() + extra as u32;
                    bytes[at..at + 4].copy_from_slice(&size.to_be_bytes());
                }
                let data_offset = be_u32(&bytes, trun + 16).unwrap() + extra as u32;
                bytes[trun + 16..trun + 20].copy_from_slice(&data_offset.to_be_bytes());
                repair_crc(&mut bytes);
                if extra < 4 {
                    assert_fragment_rejected(&bytes);
                } else {
                    assert!(parse_fragment(&bytes).is_some());
                    assert!(crate::export::rebase_fragment(&mut bytes, 2, 3000).is_ok());
                    let mut file = build_init_segment(&fake_sample_entry()).to_vec();
                    file.extend(bytes);
                    assert!(
                        scan(Cursor::new(file), ScanOptions::default())
                            .unwrap()
                            .stop
                            .is_none()
                    );
                }
            }
        }
    }

    #[test]
    fn an_init_box_with_a_64_bit_size_past_the_file_stops_the_scan() {
        // A fuzz finding: `offset += size` overflowed on a `largesize` of all ones.
        let bytes: [u8; 33] = [
            0x00, 0x00, 0x00, 0x08, 0x66, 0x74, 0x79, 0x70, 0x00, 0x00, 0x00, 0x01, 0x6d, 0x6f,
            0x6f, 0x76, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
            0xff, 0xff, 0xff, 0xff, 0xff,
        ];
        let report = scan(Cursor::new(&bytes[..]), ScanOptions::default()).unwrap();
        assert!(matches!(report.stop, Some(ScanStop::Truncated { .. })));
    }

    #[test]
    fn a_child_box_with_a_huge_declared_size_stops_the_scan() {
        // A fuzz finding: the bound check added `pos + size` and overflowed.
        let bytes: [u8; 134] = [
            0x00, 0x00, 0x00, 0x08, 0x66, 0x74, 0x79, 0x70, 0x00, 0x00, 0x00, 0x08, 0x6d, 0x6f,
            0x6f, 0x76, 0x00, 0x00, 0x00, 0x46, 0x6d, 0x6f, 0x6f, 0x66, 0x00, 0x00, 0x00, 0x20,
            0x00, 0x00, 0x40, 0x00, 0x00, 0x41, 0x00, 0x00, 0x00, 0x70, 0x72, 0x66, 0x74, 0x00,
            0x00, 0x00, 0x08, 0x66, 0x74, 0x79, 0x6d, 0x6f, 0x6f, 0x08, 0x66, 0x74, 0x79, 0x70,
            0x00, 0x00, 0x00, 0x08, 0x6d, 0x6f, 0x6f, 0x76, 0x00, 0x00, 0x00, 0x01, 0x00, 0x80,
            0x00, 0x0b, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
            0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
            0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
            0xff, 0xff, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x66, 0x74, 0x01,
            0x00, 0xfe, 0xff, 0xff, 0xff, 0xff, 0xff, 0x1d,
        ];
        let report = scan(Cursor::new(&bytes[..]), ScanOptions::default()).unwrap();
        assert!(report.stop.is_some());
    }

    #[test]
    fn scans_a_clean_file() {
        let file = file_with(3);
        let report = scan(
            Cursor::new(&file),
            ScanOptions {
                verify_crc: true,
                collect_samples: true,
            },
        )
        .unwrap();
        assert_eq!(report.stop, None);
        assert_eq!(report.fragments.len(), 3);
        assert_eq!(report.valid_len, file.len() as u64);
        let f = &report.fragments[1];
        assert_eq!(
            (f.sequence, f.decode_time, f.duration, f.sample_count),
            (2, 5990, 5990, 2)
        );
        assert!(f.starts_with_sync);
        assert_eq!(f.wall, Some(WallUs::new(1_000_000)));
        assert_eq!(f.sample_durations, vec![3000, 2990]);
        assert_eq!(f.sample_sizes, vec![6, 7]);
    }

    #[test]
    fn stops_before_a_complete_fragment_beyond_the_limit() {
        let file = file_with(3);
        let options = ScanOptions {
            verify_crc: true,
            collect_samples: false,
        };
        let full = scan(Cursor::new(&file), options).unwrap();
        assert_eq!(full.stop, None);

        let limited = scan_with_limit(Cursor::new(&file), options, 2).unwrap();
        assert_eq!(limited.fragments, full.fragments[..2]);
        assert_eq!(limited.valid_len, full.fragments[2].offset);
        assert_eq!(
            limited.stop,
            Some(ScanStop::FragmentLimit {
                offset: full.fragments[2].offset,
                limit: 2,
            })
        );
        assert_eq!(
            scan_with_limit(Cursor::new(&file), options, 3).unwrap(),
            full
        );
        let none = scan_with_limit(Cursor::new(&file), options, 0).unwrap();
        assert_eq!(none.fragments, Vec::<FragmentInfo>::new());
        assert_eq!(
            none.stop,
            Some(ScanStop::FragmentLimit {
                offset: u64::from(full.init_len),
                limit: 0,
            })
        );
    }

    #[test]
    fn a_damaged_fragment_after_the_limit_reports_its_own_stop() {
        let file = file_with(3);
        let full = scan(Cursor::new(&file), ScanOptions::default()).unwrap();
        let torn_at = full.fragments[2].offset + 10;
        let torn = &file[..usize::try_from(torn_at).unwrap()];
        let report = scan_with_limit(Cursor::new(torn), ScanOptions::default(), 2).unwrap();
        assert_eq!(report.fragments, full.fragments[..2]);
        assert_eq!(
            report.stop,
            Some(ScanStop::Truncated {
                offset: full.fragments[2].offset,
            })
        );
    }

    #[test]
    fn scans_from_a_caller_validated_fragment_boundary() {
        let file = file_with(4);
        let options = ScanOptions {
            verify_crc: true,
            collect_samples: false,
        };
        let full = scan(Cursor::new(&file), options).unwrap();
        let start = full.fragments[2].offset;
        let tail = scan_from_offset(Cursor::new(&file), start, options).unwrap();
        assert_eq!(tail.init_len, full.init_len);
        assert_eq!(tail.fragments, full.fragments[2..]);
        assert_eq!(tail.valid_len, full.valid_len);
        assert_eq!(tail.stop, None);
        assert!(
            scan_from_offset(Cursor::new(&file), u64::from(full.init_len) - 1, options).is_err()
        );
        assert!(scan_from_offset(Cursor::new(&file), file.len() as u64 + 1, options).is_err());
    }

    #[test]
    fn stops_at_truncation_and_zero_fill() {
        let full = file_with(2);
        let mut truncated = full.clone();
        truncated.truncate(full.len() - 10);
        let report = scan(Cursor::new(&truncated), ScanOptions::default()).unwrap();
        assert_eq!(report.fragments.len(), 1);
        assert!(matches!(report.stop, Some(ScanStop::Truncated { .. })));
        assert_eq!(
            report.valid_len,
            report.fragments[0].offset + u64::from(report.fragments[0].len)
        );

        let mut padded = full.clone();
        padded.extend_from_slice(&[0u8; 4096]);
        let report = scan(Cursor::new(&padded), ScanOptions::default()).unwrap();
        assert_eq!(report.fragments.len(), 2);
        assert_eq!(
            report.stop,
            Some(ScanStop::ZeroFill {
                offset: full.len() as u64
            })
        );
        assert_eq!(report.valid_len, full.len() as u64);
    }

    #[test]
    fn detects_corruption_with_crc() {
        let mut file = file_with(2);
        let last = file.len() - TRAILER_LEN - 3;
        file[last] ^= 0xff;
        let report = scan(
            Cursor::new(&file),
            ScanOptions {
                verify_crc: true,
                collect_samples: false,
            },
        )
        .unwrap();
        assert_eq!(report.fragments.len(), 1);
        assert!(matches!(report.stop, Some(ScanStop::CrcMismatch { .. })));
        let lenient = scan(Cursor::new(&file), ScanOptions::default()).unwrap();
        assert_eq!(lenient.fragments.len(), 2);
    }
}
