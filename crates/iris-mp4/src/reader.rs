//! Scanning and validating recording files.
//!
//! The scanner walks `ftyp`, `moov`, then fragments, verifying box structure,
//! sample accounting, and trailers. It stops at the first inconsistency and
//! reports how many bytes are valid, which is exactly what crash recovery
//! needs to truncate a file that was open when power failed.

use std::io::{self, Read, Seek, SeekFrom};

use crate::boxes::{BoxHeader, be_u32, be_u64};
use crate::crc::Crc32c;
use crate::fragment::{MAX_MOOF_LEN, MAX_PAYLOAD_LEN, TRAILER_LEN, decode_trailer};
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
}

/// What a scan found: the init segment length, every valid fragment, how many
/// bytes from the start are valid, and why the scan stopped if it did not reach
/// a clean end of file. `valid_len` is what crash recovery truncates to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScanReport {
    /// Length of `ftyp` + `moov`.
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

/// Scans a recording.
pub fn scan<R: Read + Seek>(mut reader: R, options: ScanOptions) -> io::Result<ScanReport> {
    let file_len = reader.seek(SeekFrom::End(0))?;
    reader.seek(SeekFrom::Start(0))?;

    let mut offset = 0u64;
    let mut init_len = 0u32;
    for expected in [*b"ftyp", *b"moov"] {
        let header = match read_header(&mut reader, offset, file_len)? {
            Some(h) if h.kind == expected => h,
            Some(h) => {
                return Ok(ScanReport {
                    init_len,
                    fragments: Vec::new(),
                    valid_len: 0,
                    stop: Some(ScanStop::BadBox {
                        offset,
                        kind: h.kind,
                    }),
                });
            }
            None => {
                return Ok(ScanReport {
                    init_len,
                    fragments: Vec::new(),
                    valid_len: 0,
                    stop: Some(ScanStop::Truncated { offset }),
                });
            }
        };
        let size = header.size.unwrap_or(file_len - offset);
        // A declared size past the end of the file (a 64-bit `largesize` among them)
        // must stop the scan, not overflow the offset.
        if size < u64::from(header.header_len) || size > file_len - offset {
            return Ok(ScanReport {
                init_len,
                fragments: Vec::new(),
                valid_len: 0,
                stop: Some(ScanStop::Truncated { offset }),
            });
        }
        offset += size;
        init_len = u32::try_from(offset).map_err(|_| io::Error::other("init segment too large"))?;
    }

    let mut report = ScanReport {
        init_len,
        fragments: Vec::new(),
        valid_len: offset,
        stop: None,
    };
    loop {
        match scan_fragment(&mut reader, offset, file_len, options)? {
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

/// Parses one complete fragment held in memory (`prft`, `moof`, `mdat`,
/// trailer) with its sample tables filled, without verifying the CRC. `offset`
/// is 0. Returns `None` for anything malformed, including a trailer whose
/// length disagrees with the buffer. For a serving path that has read one
/// fragment out of a file; the scanner is for whole files.
#[must_use]
pub fn parse_fragment(buf: &[u8]) -> Option<FragmentInfo> {
    let mut pos = 0usize;
    let mut wall = None;
    let mut parsed = None;
    let mut payload = None;
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
            b"prft" => wall = Some(WallUs::from_ntp64(be_u64(body, 8)?)),
            b"moof" => parsed = Some(parse_moof(full, true).ok()?),
            b"mdat" => payload = Some((pos + header_len, body.len() as u64)),
            _ => {}
        }
        pos += size;
    }
    let parsed = parsed?;
    let (payload_offset, payload_len) = payload?;
    if payload_len != parsed.total_size || buf.len() < TRAILER_LEN {
        return None;
    }
    let trailer = decode_trailer(&buf[buf.len() - TRAILER_LEN..])?;
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
    let mut trun = None;
    for (h, b) in children(body) {
        match &h.kind {
            b"mfhd" => sequence = be_u32(b, 4),
            b"traf" => {
                for (th, tb) in children(b) {
                    match &th.kind {
                        b"tfhd" => {
                            let flags = be_u32(tb, 0).ok_or("short tfhd")? & 0x00ff_ffff;
                            let mut pos = 8;
                            if flags & 0x1 != 0 {
                                pos += 8;
                            }
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
    let trun = trun.ok_or("trun missing")?;
    let flags = be_u32(trun, 0).ok_or("short trun")? & 0x00ff_ffff;
    let count = be_u32(trun, 4).ok_or("short trun")?;
    let sample_count = u16::try_from(count).map_err(|_| "too many samples")?;
    let mut pos = 8;
    if flags & 0x1 != 0 {
        pos += 4;
    }
    let mut first_flags = None;
    if flags & 0x4 != 0 {
        first_flags = be_u32(trun, pos);
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
            if i == 0 {
                first_sample_flags_in_table = be_u32(trun, pos);
            }
            pos += 4;
        }
        if flags & 0x800 != 0 {
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
