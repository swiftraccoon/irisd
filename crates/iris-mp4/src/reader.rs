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

use std::fmt;
use std::io::{self, Read, Seek, SeekFrom};

use crate::boxes::{BoxHeader, be_u32, be_u64};
use crate::crc::Crc32c;
use crate::fragment::{
    MAX_MOOF_LEN, MAX_PAYLOAD_LEN, MAX_PRFT_LEN, MIN_PRFT_LEN, SAMPLE_IS_NON_SYNC,
    TFHD_BASE_DATA_OFFSET_PRESENT, TFHD_DEFAULT_BASE_IS_MOOF, TFHD_DEFAULT_SAMPLE_DURATION_PRESENT,
    TFHD_DEFAULT_SAMPLE_FLAGS_PRESENT, TFHD_DEFAULT_SAMPLE_SIZE_PRESENT,
    TFHD_SAMPLE_DESCRIPTION_INDEX_PRESENT, TRAILER_LEN, TRUN_COMPOSITION_OFFSETS_PRESENT,
    TRUN_DATA_OFFSET_PRESENT, TRUN_FIRST_SAMPLE_FLAGS_PRESENT, TRUN_SAMPLE_DURATION_PRESENT,
    TRUN_SAMPLE_FLAGS_PRESENT, TRUN_SAMPLE_SIZE_PRESENT, Trailer, decode_trailer,
};
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
    /// `tfdt` decode time in 90 kHz ticks.
    pub decode_time: u64,
    /// Sum of sample durations in 90 kHz ticks.
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
#[non_exhaustive]
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
    /// A box of an unexpected type or size, or an initialization segment
    /// longer than 4 GiB.
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

impl fmt::Display for ScanStop {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Truncated { offset } => {
                write!(formatter, "the file ends inside the fragment at {offset}")
            }
            Self::ZeroFill { offset } => write!(formatter, "zero-filled space at {offset}"),
            Self::BadBox { offset, kind } => {
                write!(
                    formatter,
                    "unexpected box {} at {offset}",
                    kind.escape_ascii()
                )
            }
            Self::BadFragment { offset, reason } => {
                write!(formatter, "malformed fragment at {offset}: {reason}")
            }
            Self::BadTrailer { offset } => {
                write!(formatter, "bad trailer on the fragment at {offset}")
            }
            Self::CrcMismatch { offset } => {
                write!(formatter, "CRC mismatch on the fragment at {offset}")
            }
            Self::FragmentLimit { offset, limit } => write!(
                formatter,
                "more than {limit} fragments; the first beyond the limit starts at {offset}"
            ),
        }
    }
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
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct ScanOptions {
    /// Read every payload and verify the trailer CRC (slower; used for recovery).
    pub verify_crc: bool,
    /// Keep per-sample durations and sizes in [`FragmentInfo`]: 8 bytes per
    /// sample, up to 65,535 samples per fragment, beyond the fragment table
    /// itself.
    pub collect_samples: bool,
}

/// Why a stage of a scan ended: the reader failed, or the content stops the
/// scan. Stages use `?` on both; [`scanned`] separates them again.
enum Stop {
    Io(io::Error),
    Scan(ScanStop),
}

impl From<io::Error> for Stop {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<ScanStop> for Stop {
    fn from(stop: ScanStop) -> Self {
        Self::Scan(stop)
    }
}

/// Splits a stage result into the reader failure and the scan outcome.
fn scanned<T>(result: Result<T, Stop>) -> io::Result<Result<T, ScanStop>> {
    match result {
        Ok(value) => Ok(Ok(value)),
        Err(Stop::Io(error)) => Err(error),
        Err(Stop::Scan(stop)) => Ok(Err(stop)),
    }
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
/// reported like any other torn file. A file that ends right after its
/// initialization segment is a clean scan with no fragments.
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

    let init_len = match scanned(scan_initialization(&mut reader, file_len))? {
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
    let mut report = ScanReport {
        init_len,
        fragments: Vec::new(),
        valid_len: u64::from(init_len),
        stop: None,
    };
    let mut offset = u64::from(init_len);
    loop {
        if offset == file_len {
            return Ok(report);
        }
        match scan_fragment(&mut reader, offset, file_len, options)? {
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
            }
            Err(stop) => {
                report.stop = Some(stop);
                return Ok(report);
            }
        }
    }
}

/// Accepts `ftyp` then `moov` and returns their combined length.
fn scan_initialization<R: Read + Seek>(reader: &mut R, file_len: u64) -> Result<u32, Stop> {
    let mut offset = 0u64;
    let mut moov_offset = 0u64;
    for expected in [*b"ftyp", *b"moov"] {
        let header = match read_header(reader, offset, file_len)? {
            Some(header) if header.kind == expected => header,
            Some(header) => {
                return Err(ScanStop::BadBox {
                    offset,
                    kind: header.kind,
                }
                .into());
            }
            None => return Err(ScanStop::Truncated { offset }.into()),
        };
        let size = header.size.unwrap_or(file_len - offset);
        // A declared size past the end of the file (a 64-bit `largesize` among them)
        // must stop the scan, not overflow the offset.
        if size < u64::from(header.header_len) || size > file_len - offset {
            return Err(ScanStop::Truncated { offset }.into());
        }
        moov_offset = offset;
        offset += size;
    }
    // The sidecar and the fragment addressing hold the init length in 32 bits.
    u32::try_from(offset).map_err(|_| {
        ScanStop::BadBox {
            offset: moov_offset,
            kind: *b"moov",
        }
        .into()
    })
}

fn read_exact_at<R: Read + Seek>(reader: &mut R, offset: u64, buf: &mut [u8]) -> io::Result<()> {
    reader.seek(SeekFrom::Start(offset))?;
    reader.read_exact(buf)
}

/// The box header at `offset`, or `None` when fewer than 8 bytes remain.
fn read_header<R: Read + Seek>(
    reader: &mut R,
    offset: u64,
    file_len: u64,
) -> io::Result<Option<BoxHeader>> {
    if offset.saturating_add(8) > file_len {
        return Ok(None);
    }
    let mut buf = [0u8; 16];
    let n = usize::try_from((file_len - offset).min(16)).expect("at most 16 bytes");
    read_exact_at(reader, offset, &mut buf[..n])?;
    Ok(BoxHeader::parse(&buf[..n]))
}

/// Reads and validates the fragment at `start`.
fn scan_fragment<R: Read + Seek>(
    reader: &mut R,
    start: u64,
    file_len: u64,
    options: ScanOptions,
) -> io::Result<Result<FragmentInfo, ScanStop>> {
    scanned(fragment_at(reader, start, file_len, options))
}

fn fragment_at<R: Read + Seek>(
    reader: &mut R,
    start: u64,
    file_len: u64,
    options: ScanOptions,
) -> Result<FragmentInfo, Stop> {
    let mut offset = start;
    let mut header = read_fragment_box_header(reader, offset, start, file_len)?;
    let mut wall = None;
    if header.kind == *b"prft" {
        let (prft_wall, size) = read_prft(reader, offset, header, file_len)?;
        wall = prft_wall;
        offset += size;
        header = read_fragment_box_header(reader, offset, start, file_len)?;
    }
    if header.kind != *b"moof" {
        return Err(ScanStop::BadBox {
            offset,
            kind: header.kind,
        }
        .into());
    }
    let moof_size = header
        .size
        .filter(|size| *size <= MAX_MOOF_LEN as u64)
        .ok_or(ScanStop::BadFragment {
            offset: start,
            reason: "moof size out of range",
        })?;
    if moof_size > file_len - offset {
        return Err(ScanStop::Truncated { offset: start }.into());
    }
    let mut moof_buf = vec![0u8; moof_size as usize];
    read_exact_at(reader, offset, &mut moof_buf)?;
    let parsed = parse_moof(
        &moof_buf[usize::from(header.header_len)..],
        options.collect_samples,
    )
    .map_err(|reason| ScanStop::BadFragment {
        offset: start,
        reason,
    })?;
    offset += moof_size;

    let (payload_offset, trailer_offset) =
        read_mdat(reader, offset, start, file_len, &parsed, moof_size)?;
    let (trailer, end) = read_trailer(reader, trailer_offset, start, file_len)?;

    if options.verify_crc && !crc_matches(reader, start, trailer_offset, trailer.crc)? {
        return Err(ScanStop::CrcMismatch { offset: start }.into());
    }

    Ok(FragmentInfo {
        offset: start,
        len: u32::try_from(end - start).expect("the trailer's length field matched"),
        payload_offset,
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

/// The 8-byte header of a top-level fragment box at `offset`. At the
/// fragment's `start`, eight zero bytes are preallocated space.
fn read_fragment_box_header<R: Read + Seek>(
    reader: &mut R,
    offset: u64,
    start: u64,
    file_len: u64,
) -> Result<BoxHeader, Stop> {
    if offset.saturating_add(8) > file_len {
        return Err(ScanStop::Truncated { offset: start }.into());
    }
    let mut buf = [0u8; 8];
    read_exact_at(reader, offset, &mut buf)?;
    if offset == start && buf == [0u8; 8] {
        return Err(ScanStop::ZeroFill { offset }.into());
    }
    BoxHeader::parse(&buf).ok_or_else(|| {
        ScanStop::BadBox {
            offset,
            kind: [buf[4], buf[5], buf[6], buf[7]],
        }
        .into()
    })
}

/// Reads a `prft` box whose header is `header`: its wall time and its size.
fn read_prft<R: Read + Seek>(
    reader: &mut R,
    offset: u64,
    header: BoxHeader,
    file_len: u64,
) -> Result<(Option<WallUs>, u64), Stop> {
    let size = header
        .size
        .filter(|size| (MIN_PRFT_LEN..=MAX_PRFT_LEN).contains(size))
        .ok_or(ScanStop::BadBox {
            offset,
            kind: header.kind,
        })?;
    if size > file_len - offset {
        return Err(ScanStop::Truncated { offset }.into());
    }
    let mut prft = vec![0u8; size as usize];
    read_exact_at(reader, offset, &mut prft)?;
    // version/flags(4) track(4) ntp(8) media_time(4 or 8): the NTP timestamp is
    // 64 bits in both versions.
    Ok((be_u64(&prft, 16).map(WallUs::from_ntp64), size))
}

/// Checks the `mdat` header at `offset` against the parsed `moof`; returns the
/// payload offset and the trailer offset.
fn read_mdat<R: Read + Seek>(
    reader: &mut R,
    offset: u64,
    start: u64,
    file_len: u64,
    parsed: &ParsedMoof,
    moof_size: u64,
) -> Result<(u64, u64), Stop> {
    if offset.saturating_add(8) > file_len {
        return Err(ScanStop::Truncated { offset: start }.into());
    }
    let mut buf = [0u8; 8];
    read_exact_at(reader, offset, &mut buf)?;
    let mdat = BoxHeader::parse(&buf)
        .filter(|header| header.kind == *b"mdat")
        .ok_or(ScanStop::BadFragment {
            offset: start,
            reason: "mdat missing",
        })?;
    let mdat_size = mdat.size.ok_or(ScanStop::BadFragment {
        offset: start,
        reason: "open-ended mdat",
    })?;
    let payload_len = mdat_size.saturating_sub(u64::from(mdat.header_len));
    if payload_len != parsed.total_size || payload_len > MAX_PAYLOAD_LEN {
        return Err(ScanStop::BadFragment {
            offset: start,
            reason: "mdat size disagrees with trun",
        }
        .into());
    }
    if u64::try_from(parsed.data_offset).ok() != Some(moof_size + u64::from(mdat.header_len)) {
        return Err(ScanStop::BadFragment {
            offset: start,
            reason: "trun data offset does not address mdat payload",
        }
        .into());
    }
    let payload_offset = offset + u64::from(mdat.header_len);
    let trailer_offset = offset
        .checked_add(mdat_size)
        .ok_or(ScanStop::Truncated { offset: start })?;
    Ok((payload_offset, trailer_offset))
}

/// Reads the trailer at `offset` and checks its length against the fragment
/// that began at `start`; returns the trailer and the fragment's end.
fn read_trailer<R: Read + Seek>(
    reader: &mut R,
    offset: u64,
    start: u64,
    file_len: u64,
) -> Result<(Trailer, u64), Stop> {
    if offset.saturating_add(TRAILER_LEN as u64) > file_len {
        return Err(ScanStop::Truncated { offset: start }.into());
    }
    let mut buf = [0u8; TRAILER_LEN];
    read_exact_at(reader, offset, &mut buf)?;
    let trailer = decode_trailer(&buf).ok_or(ScanStop::BadTrailer { offset: start })?;
    let end = offset + TRAILER_LEN as u64;
    if u64::from(trailer.fragment_len) != end - start {
        return Err(ScanStop::BadTrailer { offset: start }.into());
    }
    Ok((trailer, end))
}

/// Whether the CRC-32C of `[start, end)` equals `expected`.
fn crc_matches<R: Read + Seek>(
    reader: &mut R,
    start: u64,
    end: u64,
    expected: u32,
) -> io::Result<bool> {
    let mut crc = Crc32c::new();
    let mut buf = vec![0u8; 1 << 16];
    let mut pos = start;
    while pos < end {
        let n =
            usize::try_from((end - pos).min(buf.len() as u64)).expect("at most the 64 KiB buffer");
        read_exact_at(reader, pos, &mut buf[..n])?;
        crc.update(&buf[..n]);
        pos += n as u64;
    }
    Ok(crc.finish() == expected)
}

/// Parses one complete fragment held in memory (optional `prft`, one `moof`,
/// one `mdat`, final trailer) with its sample tables filled, without verifying
/// the CRC. `offset` is 0. Returns `None` for anything malformed, including a
/// trailer whose length disagrees with the buffer, and for a fragment beyond
/// the scanner's bounds (`prft` outside 24 to 64 bytes, `moof` over
/// [`MAX_MOOF_LEN`], payload over [`MAX_PAYLOAD_LEN`]), so a fragment this
/// function accepts also scans. For a serving path that has read one fragment
/// out of a file; the scanner is for whole files.
///
/// Both readers require `default-base-is-moof` and an explicit `trun` data
/// offset to the adjacent `mdat` payload, with every declared table field
/// present.
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
            b"prft" if pos == 0 && (MIN_PRFT_LEN..=MAX_PRFT_LEN).contains(&(size as u64)) => {
                wall = Some(WallUs::from_ntp64(be_u64(body, 8)?));
            }
            b"moof" if parsed.is_none() && payload.is_none() && size <= MAX_MOOF_LEN => {
                moof_offset = pos;
                parsed = Some(parse_moof(body, /* collect_samples */ true).ok()?);
            }
            b"mdat"
                if parsed.is_some()
                    && payload.is_none()
                    && body.len() as u64 <= MAX_PAYLOAD_LEN =>
            {
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

/// The sample table of one `trun`.
struct Trun {
    data_offset: i32,
    /// The flags of the first sample, from `first_sample_flags` or the table.
    first_flags: Option<u32>,
    total_duration: u32,
    total_size: u64,
    sample_count: u16,
    durations: Vec<u32>,
    sizes: Vec<u32>,
}

/// The child boxes of a container body in order. A child whose header or
/// declared size does not fit ends the walk with an error, so undecodable
/// bytes inside `moof` or `traf` reject the fragment in both readers.
fn children(buf: &[u8]) -> impl Iterator<Item = Result<(BoxHeader, &[u8]), &'static str>> {
    let mut pos = 0usize;
    let mut failed = false;
    std::iter::from_fn(move || {
        if failed || pos >= buf.len() {
            return None;
        }
        let header = BoxHeader::parse(&buf[pos..]);
        let size = header.and_then(|header| usize::try_from(header.size?).ok());
        // `size > buf.len() - pos` rather than `pos + size > buf.len()`: a
        // declared size near `usize::MAX` must not overflow the addition.
        match (header, size) {
            (Some(header), Some(size))
                if size >= usize::from(header.header_len) && size <= buf.len() - pos =>
            {
                let body = &buf[pos + usize::from(header.header_len)..pos + size];
                pos += size;
                Some(Ok((header, body)))
            }
            _ => {
                failed = true;
                Some(Err("malformed child box"))
            }
        }
    })
}

/// Parses a `moof` body: `mfhd`, and one `traf` with `tfhd`, `tfdt`, and `trun`.
fn parse_moof(body: &[u8], collect_samples: bool) -> Result<ParsedMoof, &'static str> {
    let mut sequence = None;
    let mut decode_time = None;
    let mut tfhd = None;
    let mut trun = None;
    for child in children(body) {
        let (header, child_body) = child?;
        match &header.kind {
            b"mfhd" => sequence = be_u32(child_body, 4),
            b"traf" => {
                for child in children(child_body) {
                    let (header, child_body) = child?;
                    match &header.kind {
                        b"tfhd" => tfhd = Some(parse_tfhd(child_body)?),
                        b"tfdt" => decode_time = parse_tfdt(child_body),
                        b"trun" => trun = Some(parse_trun(child_body, collect_samples)?),
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }
    let default_flags = tfhd.ok_or("tfhd missing")?;
    let trun = trun.ok_or("trun missing")?;
    // irisd writes a tfhd default, so a fragment without any sample flags is
    // not one of its own; the `trex` default is not consulted.
    let first_flags = trun
        .first_flags
        .or(default_flags)
        .ok_or("sample flags missing")?;
    Ok(ParsedMoof {
        data_offset: trun.data_offset,
        sequence: sequence.ok_or("mfhd missing")?,
        decode_time: decode_time.ok_or("tfdt missing")?,
        total_duration: trun.total_duration,
        total_size: trun.total_size,
        sample_count: trun.sample_count,
        starts_with_sync: first_flags & SAMPLE_IS_NON_SYNC == 0,
        durations: trun.durations,
        sizes: trun.sizes,
    })
}

/// Parses a `tfhd` body: requires `default-base-is-moof` without a base data
/// offset, and returns the default sample flags when declared. Every declared
/// field must fit.
fn parse_tfhd(body: &[u8]) -> Result<Option<u32>, &'static str> {
    let flags = be_u32(body, 0).ok_or("short tfhd")? & 0x00ff_ffff;
    if flags & TFHD_DEFAULT_BASE_IS_MOOF == 0 || flags & TFHD_BASE_DATA_OFFSET_PRESENT != 0 {
        return Err("unsupported trun data base");
    }
    let mut pos = 8; // version/flags, track_ID
    for optional in [
        TFHD_SAMPLE_DESCRIPTION_INDEX_PRESENT,
        TFHD_DEFAULT_SAMPLE_DURATION_PRESENT,
        TFHD_DEFAULT_SAMPLE_SIZE_PRESENT,
    ] {
        if flags & optional != 0 {
            pos += 4;
        }
    }
    let default_flags = if flags & TFHD_DEFAULT_SAMPLE_FLAGS_PRESENT != 0 {
        let value = be_u32(body, pos).ok_or("tfhd truncated")?;
        pos += 4;
        Some(value)
    } else {
        None
    };
    if body.len() < pos {
        return Err("tfhd truncated");
    }
    Ok(default_flags)
}

/// The decode time of a `tfdt` body, 64 bits in version 1 and 32 in version 0.
fn parse_tfdt(body: &[u8]) -> Option<u64> {
    if body.first() == Some(&1) {
        be_u64(body, 4)
    } else {
        be_u32(body, 4).map(u64::from)
    }
}

/// Parses a `trun` body: a data offset, per-sample durations and sizes, and
/// at least one sample.
fn parse_trun(body: &[u8], collect_samples: bool) -> Result<Trun, &'static str> {
    let flags = be_u32(body, 0).ok_or("short trun")? & 0x00ff_ffff;
    let count = be_u32(body, 4).ok_or("short trun")?;
    let sample_count = u16::try_from(count).map_err(|_| "too many samples")?;
    if sample_count == 0 {
        return Err("trun has no samples");
    }
    if flags & TRUN_DATA_OFFSET_PRESENT == 0 {
        return Err("trun data offset missing");
    }
    let mut pos = 8;
    let data_offset = be_u32(body, pos).ok_or("trun truncated")? as i32;
    pos += 4;
    let mut first_flags = None;
    if flags & TRUN_FIRST_SAMPLE_FLAGS_PRESENT != 0 {
        first_flags = Some(be_u32(body, pos).ok_or("trun truncated")?);
        pos += 4;
    }
    if flags & TRUN_SAMPLE_DURATION_PRESENT == 0 || flags & TRUN_SAMPLE_SIZE_PRESENT == 0 {
        return Err("trun lacks per-sample duration or size");
    }
    let mut total_duration = 0u32;
    let mut total_size = 0u64;
    let mut durations = Vec::new();
    let mut sizes = Vec::new();
    for index in 0..count {
        let duration = be_u32(body, pos).ok_or("trun truncated")?;
        pos += 4;
        let size = be_u32(body, pos).ok_or("trun truncated")?;
        pos += 4;
        if flags & TRUN_SAMPLE_FLAGS_PRESENT != 0 {
            let sample_flags = be_u32(body, pos).ok_or("trun truncated")?;
            if index == 0 && first_flags.is_none() {
                first_flags = Some(sample_flags);
            }
            pos += 4;
        }
        if flags & TRUN_COMPOSITION_OFFSETS_PRESENT != 0 {
            be_u32(body, pos).ok_or("trun truncated")?;
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
    Ok(Trun {
        data_offset,
        first_flags,
        total_duration,
        total_size,
        sample_count,
        durations,
        sizes,
    })
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use bytes::Bytes;

    use super::*;
    use crate::crc::crc32c;
    use crate::export::rebase_fragment;
    use crate::fragment::{
        FragmentHeader, NON_SYNC_SAMPLE_FLAGS, SYNC_SAMPLE_FLAGS, Sample, encode_fragment,
        encode_trailer,
    };
    use crate::init::build_init_segment;
    use crate::init::tests::fake_sample_entry;

    /// A two-sample fragment: a sync sample of six bytes and a dependent
    /// sample of seven.
    fn two_sample_fragment() -> ([Sample; 2], FragmentHeader, Vec<u8>) {
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
        for sample in &samples {
            bytes.extend_from_slice(&sample.data);
        }
        bytes.extend_from_slice(&encoded.trailer);
        (samples, header, bytes)
    }

    #[test]
    fn parse_fragment_reads_an_encoded_fragment_back() {
        let (samples, header, bytes) = two_sample_fragment();
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
    }

    #[test]
    fn parse_fragment_rejects_a_fragment_cut_at_either_end() {
        let (_, _, bytes) = two_sample_fragment();
        assert!(parse_fragment(&bytes[..bytes.len() - 1]).is_none());
        assert!(parse_fragment(&bytes[1..]).is_none());
    }

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
        let trailer = encode_trailer(crc32c(&bytes[..end]), bytes.len() as u32);
        bytes[end..].copy_from_slice(&trailer);
    }

    /// Adds `delta` to the size field of the box of `kind`.
    fn grow_box(bytes: &mut [u8], kind: [u8; 4], delta: i64) {
        let at = box_at(bytes, kind);
        let size = u32::try_from(i64::from(be_u32(bytes, at).unwrap()) + delta).unwrap();
        bytes[at..at + 4].copy_from_slice(&size.to_be_bytes());
    }

    fn assert_fragment_rejected(bytes: &[u8]) {
        assert!(
            parse_fragment(bytes).is_none(),
            "the memory parser accepted it"
        );
        assert!(
            rebase_fragment(&mut bytes.to_vec(), 2, 3000).is_err(),
            "the export rewrite accepted it"
        );
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
        assert!(report.stop.is_some(), "the scanner accepted it");
    }

    fn assert_fragment_accepted(bytes: &[u8]) {
        assert!(
            parse_fragment(bytes).is_some(),
            "the memory parser rejected it"
        );
        assert!(
            rebase_fragment(&mut bytes.to_vec(), 2, 3000).is_ok(),
            "the export rewrite rejected it"
        );
        let mut file = build_init_segment(&fake_sample_entry()).to_vec();
        file.extend_from_slice(bytes);
        let report = scan(
            Cursor::new(file),
            ScanOptions {
                verify_crc: true,
                collect_samples: true,
            },
        )
        .unwrap();
        assert_eq!(report.stop, None);
        assert_eq!(report.fragments.len(), 1);
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
    }

    #[test]
    fn an_initialization_only_file_is_a_clean_scan_without_fragments() {
        let init = build_init_segment(&fake_sample_entry());
        let report = scan(Cursor::new(init.to_vec()), ScanOptions::default()).unwrap();
        assert_eq!(report.init_len as usize, init.len());
        assert_eq!(report.valid_len as usize, init.len());
        assert_eq!(report.fragments, Vec::<FragmentInfo>::new());
        assert_eq!(report.stop, None);
    }

    #[test]
    fn a_partial_fragment_header_after_the_initialization_is_truncation() {
        let init = build_init_segment(&fake_sample_entry());
        let mut file = init.to_vec();
        file.extend([0, 0, 0, 32, b'p', b'r', b'f', b't']);
        let report = scan(Cursor::new(file), ScanOptions::default()).unwrap();
        assert_eq!(report.valid_len as usize, init.len());
        assert_eq!(
            report.stop,
            Some(ScanStop::Truncated {
                offset: init.len() as u64
            })
        );
    }

    /// A reader over `head` followed by zeros up to `len` bytes, for a
    /// declared box size no test file could hold.
    struct Sparse {
        head: Vec<u8>,
        len: u64,
        pos: u64,
    }

    impl Read for Sparse {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            let remaining = self.len.saturating_sub(self.pos);
            let n = buf
                .len()
                .min(usize::try_from(remaining).unwrap_or(usize::MAX));
            for (i, byte) in buf[..n].iter_mut().enumerate() {
                *byte = usize::try_from(self.pos + i as u64)
                    .ok()
                    .and_then(|at| self.head.get(at))
                    .copied()
                    .unwrap_or(0);
            }
            self.pos += n as u64;
            Ok(n)
        }
    }

    impl Seek for Sparse {
        fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
            let unreachable = || io::Error::other("seek outside the file");
            self.pos = match pos {
                SeekFrom::Start(at) => at,
                SeekFrom::End(delta) => {
                    self.len.checked_add_signed(delta).ok_or_else(unreachable)?
                }
                SeekFrom::Current(delta) => {
                    self.pos.checked_add_signed(delta).ok_or_else(unreachable)?
                }
            };
            Ok(self.pos)
        }
    }

    #[test]
    fn an_initialization_segment_over_4_gib_stops_the_scan_at_the_moov() {
        let mut head = vec![
            0, 0, 0, 8, b'f', b't', b'y', b'p', 0, 0, 0, 1, b'm', b'o', b'o', b'v',
        ];
        head.extend((1u64 << 32).to_be_bytes());
        let reader = Sparse {
            head,
            len: 8 + (1u64 << 32),
            pos: 0,
        };
        let report = scan(reader, ScanOptions::default()).unwrap();
        assert_eq!(report.init_len, 0);
        assert_eq!(
            report.stop,
            Some(ScanStop::BadBox {
                offset: 8,
                kind: *b"moov"
            })
        );
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
        assert!(rebase_fragment(&mut extra_trailer, 2, 3000).is_err());
        assert_fragment_rejected(&normal[..end]);
        let mut no_prft = normal[moof..].to_vec();
        repair_crc(&mut no_prft);
        assert_fragment_accepted(&normal);
        assert_fragment_accepted(&no_prft);
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
            (*b"trun", TRUN_DATA_OFFSET_PRESENT, 0),
            (*b"tfhd", TFHD_DEFAULT_BASE_IS_MOOF, 0),
            (*b"tfhd", 0, TFHD_BASE_DATA_OFFSET_PRESENT),
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
        for flag in [TRUN_SAMPLE_FLAGS_PRESENT, TRUN_COMPOSITION_OFFSETS_PRESENT] {
            for extra in 0..=4 {
                let mut bytes = fragment_bytes(false);
                let trun = box_at(&bytes, *b"trun");
                let flags = be_u32(&bytes, trun + 8).unwrap() | flag;
                bytes[trun + 8..trun + 12].copy_from_slice(&flags.to_be_bytes());
                let table_end = trun + be_u32(&bytes, trun).unwrap() as usize;
                bytes.splice(table_end..table_end, vec![0; extra]);
                for kind in [*b"moof", *b"traf", *b"trun"] {
                    grow_box(&mut bytes, kind, i64::try_from(extra).unwrap());
                }
                let data_offset = be_u32(&bytes, trun + 16).unwrap() + extra as u32;
                bytes[trun + 16..trun + 20].copy_from_slice(&data_offset.to_be_bytes());
                repair_crc(&mut bytes);
                if extra < 4 {
                    assert_fragment_rejected(&bytes);
                } else {
                    assert_fragment_accepted(&bytes);
                }
            }
        }
    }

    #[test]
    fn a_declared_tfhd_field_that_does_not_fit_is_rejected() {
        let mut bytes = fragment_bytes(false);
        let at = box_at(&bytes, *b"tfhd") + 8;
        let flags = be_u32(&bytes, at).unwrap() | TFHD_DEFAULT_SAMPLE_DURATION_PRESENT;
        bytes[at..at + 4].copy_from_slice(&flags.to_be_bytes());
        repair_crc(&mut bytes);
        assert_fragment_rejected(&bytes);
    }

    /// How a hand-built fragment lays out its `tfhd` and `trun`, for the
    /// layouts another muxer writes and the recorder's encoder does not.
    #[derive(Clone, Copy, Default)]
    struct ForeignLayout<'a> {
        /// `tfhd` flags beside default-base-is-moof; each declared optional
        /// field is written as four bytes of `0xee`.
        tfhd_flags: u32,
        /// The default sample flags, written when `tfhd_flags` declares them.
        default_flags: u32,
        /// `trun` flags beside the data offset.
        trun_flags: u32,
        first_flags: u32,
        /// Duration, size, and flags of each sample.
        samples: &'a [(u32, u32, u32)],
    }

    fn boxed(kind: [u8; 4], body: &[u8]) -> Vec<u8> {
        let mut out = u32::try_from(8 + body.len())
            .unwrap()
            .to_be_bytes()
            .to_vec();
        out.extend_from_slice(&kind);
        out.extend_from_slice(body);
        out
    }

    /// A fragment in `layout`: `moof`, an `mdat` of the declared sizes, and
    /// the trailer.
    fn foreign_fragment(layout: ForeignLayout<'_>) -> Vec<u8> {
        let mut tfhd = (TFHD_DEFAULT_BASE_IS_MOOF | layout.tfhd_flags)
            .to_be_bytes()
            .to_vec();
        tfhd.extend_from_slice(&1u32.to_be_bytes());
        for optional in [
            TFHD_SAMPLE_DESCRIPTION_INDEX_PRESENT,
            TFHD_DEFAULT_SAMPLE_DURATION_PRESENT,
            TFHD_DEFAULT_SAMPLE_SIZE_PRESENT,
        ] {
            if layout.tfhd_flags & optional != 0 {
                tfhd.extend_from_slice(&[0xee; 4]);
            }
        }
        if layout.tfhd_flags & TFHD_DEFAULT_SAMPLE_FLAGS_PRESENT != 0 {
            tfhd.extend_from_slice(&layout.default_flags.to_be_bytes());
        }
        let mut tfdt = vec![1, 0, 0, 0];
        tfdt.extend_from_slice(&7_000u64.to_be_bytes());
        let trun_flags = TRUN_DATA_OFFSET_PRESENT | layout.trun_flags;
        let mut trun = trun_flags.to_be_bytes().to_vec();
        trun.extend_from_slice(&u32::try_from(layout.samples.len()).unwrap().to_be_bytes());
        let data_offset_at = trun.len();
        trun.extend_from_slice(&[0; 4]);
        if trun_flags & TRUN_FIRST_SAMPLE_FLAGS_PRESENT != 0 {
            trun.extend_from_slice(&layout.first_flags.to_be_bytes());
        }
        for (duration, size, flags) in layout.samples {
            if trun_flags & TRUN_SAMPLE_DURATION_PRESENT != 0 {
                trun.extend_from_slice(&duration.to_be_bytes());
            }
            if trun_flags & TRUN_SAMPLE_SIZE_PRESENT != 0 {
                trun.extend_from_slice(&size.to_be_bytes());
            }
            if trun_flags & TRUN_SAMPLE_FLAGS_PRESENT != 0 {
                trun.extend_from_slice(&flags.to_be_bytes());
            }
            if trun_flags & TRUN_COMPOSITION_OFFSETS_PRESENT != 0 {
                trun.extend_from_slice(&[0xee; 4]);
            }
        }
        let moof_len = 8 + 16 + 8 + (8 + tfhd.len()) + (8 + tfdt.len()) + (8 + trun.len());
        let data_offset = u32::try_from(moof_len + 8).unwrap();
        trun[data_offset_at..data_offset_at + 4].copy_from_slice(&data_offset.to_be_bytes());
        let mut traf = boxed(*b"tfhd", &tfhd);
        traf.extend(boxed(*b"tfdt", &tfdt));
        traf.extend(boxed(*b"trun", &trun));
        let mut moof = boxed(*b"mfhd", &[0, 0, 0, 0, 0, 0, 0, 9]);
        moof.extend(boxed(*b"traf", &traf));
        let mut bytes = boxed(*b"moof", &moof);
        assert_eq!(bytes.len(), moof_len);
        let payload: usize = layout.samples.iter().map(|s| s.1 as usize).sum();
        bytes.extend(boxed(*b"mdat", &vec![0x5a; payload]));
        bytes.extend_from_slice(&[0; TRAILER_LEN]);
        repair_crc(&mut bytes);
        bytes
    }

    const TIMED: u32 = TRUN_SAMPLE_DURATION_PRESENT | TRUN_SAMPLE_SIZE_PRESENT;

    #[test]
    fn optional_tfhd_fields_shift_the_default_sample_flags() {
        for (default_flags, sync) in [(SYNC_SAMPLE_FLAGS, true), (NON_SYNC_SAMPLE_FLAGS, false)] {
            for optional in [
                0,
                TFHD_SAMPLE_DESCRIPTION_INDEX_PRESENT,
                TFHD_DEFAULT_SAMPLE_DURATION_PRESENT | TFHD_DEFAULT_SAMPLE_SIZE_PRESENT,
                TFHD_SAMPLE_DESCRIPTION_INDEX_PRESENT
                    | TFHD_DEFAULT_SAMPLE_DURATION_PRESENT
                    | TFHD_DEFAULT_SAMPLE_SIZE_PRESENT,
            ] {
                let bytes = foreign_fragment(ForeignLayout {
                    tfhd_flags: optional | TFHD_DEFAULT_SAMPLE_FLAGS_PRESENT,
                    default_flags,
                    trun_flags: TIMED,
                    samples: &[(3000, 6, 0), (2990, 7, 0)],
                    ..ForeignLayout::default()
                });
                assert_fragment_accepted(&bytes);
                let info = parse_fragment(&bytes).unwrap();
                assert_eq!(info.starts_with_sync, sync, "tfhd flags {optional:#x}");
                assert_eq!(info.decode_time, 7_000);
                assert_eq!(info.sequence, 9);
            }
        }
    }

    #[test]
    fn a_tfhd_without_default_flags_must_still_hold_its_declared_fields() {
        let layout = ForeignLayout {
            tfhd_flags: TFHD_DEFAULT_SAMPLE_SIZE_PRESENT,
            trun_flags: TIMED | TRUN_FIRST_SAMPLE_FLAGS_PRESENT,
            first_flags: SYNC_SAMPLE_FLAGS,
            samples: &[(3000, 6, 0)],
            ..ForeignLayout::default()
        };
        let complete = foreign_fragment(layout);
        assert_fragment_accepted(&complete);
        assert!(parse_fragment(&complete).unwrap().starts_with_sync);

        // The same header with the declared field cut off.
        let mut short = complete;
        let tfhd = box_at(&short, *b"tfhd");
        short.drain(tfhd + 16..tfhd + 20);
        for kind in [*b"moof", *b"traf", *b"tfhd"] {
            grow_box(&mut short, kind, -4);
        }
        let trun = box_at(&short, *b"trun");
        let data_offset = be_u32(&short, trun + 16).unwrap() - 4;
        short[trun + 16..trun + 20].copy_from_slice(&data_offset.to_be_bytes());
        repair_crc(&mut short);
        assert_fragment_rejected(&short);
    }

    #[test]
    fn per_sample_flags_and_composition_offsets_keep_the_sample_table_aligned() {
        for (first, sync) in [(SYNC_SAMPLE_FLAGS, true), (NON_SYNC_SAMPLE_FLAGS, false)] {
            for extra in [
                TRUN_SAMPLE_FLAGS_PRESENT,
                TRUN_SAMPLE_FLAGS_PRESENT | TRUN_COMPOSITION_OFFSETS_PRESENT,
            ] {
                // No first-sample flags and no default: the first sample's
                // own flags decide, and the later samples' do not.
                let bytes = foreign_fragment(ForeignLayout {
                    trun_flags: TIMED | extra,
                    samples: &[
                        (3000, 6, first),
                        (2990, 7, SYNC_SAMPLE_FLAGS),
                        (3010, 5, NON_SYNC_SAMPLE_FLAGS),
                    ],
                    ..ForeignLayout::default()
                });
                assert_fragment_accepted(&bytes);
                let info = parse_fragment(&bytes).unwrap();
                assert_eq!(info.starts_with_sync, sync, "trun flags {extra:#x}");
                assert_eq!(info.sample_durations, [3000, 2990, 3010]);
                assert_eq!(info.sample_sizes, [6, 7, 5]);
                assert_eq!(info.duration, 9000);
            }
        }
        // Declared first-sample flags win over the first sample's own.
        let bytes = foreign_fragment(ForeignLayout {
            trun_flags: TIMED | TRUN_FIRST_SAMPLE_FLAGS_PRESENT | TRUN_SAMPLE_FLAGS_PRESENT,
            first_flags: NON_SYNC_SAMPLE_FLAGS,
            samples: &[(3000, 6, SYNC_SAMPLE_FLAGS)],
            ..ForeignLayout::default()
        });
        assert!(!parse_fragment(&bytes).unwrap().starts_with_sync);
    }

    #[test]
    fn a_trun_without_per_sample_durations_or_sizes_is_rejected() {
        for trun_flags in [TRUN_SAMPLE_DURATION_PRESENT, TRUN_SAMPLE_SIZE_PRESENT, 0] {
            let bytes = foreign_fragment(ForeignLayout {
                tfhd_flags: TFHD_DEFAULT_SAMPLE_FLAGS_PRESENT,
                default_flags: SYNC_SAMPLE_FLAGS,
                trun_flags,
                samples: &[(3000, 0, 0)],
                ..ForeignLayout::default()
            });
            assert_fragment_rejected(&bytes);
        }
    }

    #[test]
    fn a_fragment_without_any_sample_flags_is_rejected() {
        // Clearing the tfhd default leaves a dependent fragment with no flags
        // at all; reading it as sync would let a segment open on it.
        let mut bytes = fragment_bytes(false);
        let at = box_at(&bytes, *b"tfhd") + 8;
        let flags = be_u32(&bytes, at).unwrap() & !TFHD_DEFAULT_SAMPLE_FLAGS_PRESENT;
        bytes[at..at + 4].copy_from_slice(&flags.to_be_bytes());
        bytes.drain(at + 8..at + 12);
        for kind in [*b"moof", *b"traf", *b"tfhd"] {
            grow_box(&mut bytes, kind, -4);
        }
        let trun = box_at(&bytes, *b"trun");
        let data_offset = be_u32(&bytes, trun + 16).unwrap() - 4;
        bytes[trun + 16..trun + 20].copy_from_slice(&data_offset.to_be_bytes());
        repair_crc(&mut bytes);
        assert_fragment_rejected(&bytes);
    }

    #[test]
    fn undecodable_bytes_inside_a_container_box_are_rejected() {
        for container in [*b"moof", *b"traf"] {
            let mut bytes = fragment_bytes(true);
            let at = box_at(&bytes, container);
            let end = at + be_u32(&bytes, at).unwrap() as usize;
            bytes.splice(end..end, [0xaa; 4]);
            grow_box(&mut bytes, *b"moof", 4);
            if container == *b"traf" {
                grow_box(&mut bytes, *b"traf", 4);
            }
            let trun = box_at(&bytes, *b"trun");
            let data_offset = be_u32(&bytes, trun + 16).unwrap() + 4;
            bytes[trun + 16..trun + 20].copy_from_slice(&data_offset.to_be_bytes());
            repair_crc(&mut bytes);
            assert_fragment_rejected(&bytes);
        }
    }

    #[test]
    fn a_fragment_without_samples_is_rejected() {
        let mut bytes = fragment_bytes(true);
        let trun = box_at(&bytes, *b"trun");
        // version/flags(4) count(4) data_offset(4) first_flags(4), then the one
        // duration and size entry.
        let entry = trun + 8 + 16;
        bytes.drain(entry..entry + 8);
        for kind in [*b"moof", *b"traf", *b"trun"] {
            grow_box(&mut bytes, kind, -8);
        }
        bytes[trun + 12..trun + 16].copy_from_slice(&0u32.to_be_bytes());
        let data_offset = be_u32(&bytes, trun + 16).unwrap() - 8;
        bytes[trun + 16..trun + 20].copy_from_slice(&data_offset.to_be_bytes());
        let mdat = box_at(&bytes, *b"mdat");
        bytes.drain(mdat + 8..mdat + 8 + 6);
        bytes[mdat..mdat + 4].copy_from_slice(&8u32.to_be_bytes());
        repair_crc(&mut bytes);
        assert_fragment_rejected(&bytes);
    }

    #[test]
    fn the_memory_parser_applies_the_scanner_bounds() {
        let normal = fragment_bytes(true);
        assert!(parse_fragment(&normal).is_some());
        let mut long_prft = normal.clone();
        long_prft.splice(32..32, [0u8; 40]);
        grow_box(&mut long_prft, *b"prft", 40);
        repair_crc(&mut long_prft);
        assert!(parse_fragment(&long_prft).is_none());
        let mut huge_moof = normal.clone();
        let moof = box_at(&huge_moof, *b"moof");
        huge_moof[moof..moof + 4].copy_from_slice(&((MAX_MOOF_LEN as u32) + 1).to_be_bytes());
        assert!(parse_fragment(&huge_moof).is_none());
    }

    #[test]
    fn scan_stops_at_a_trailer_with_the_wrong_length() {
        let mut bytes = fragment_bytes(true);
        let len = bytes.len();
        bytes[len - 8..len - 4].copy_from_slice(&((len as u32) + 1).to_be_bytes());
        assert!(parse_fragment(&bytes).is_none());
        let mut file = file_with(1);
        let valid_len = file.len() as u64;
        file.extend_from_slice(&bytes);
        let report = scan(Cursor::new(file), ScanOptions::default()).unwrap();
        assert_eq!(
            report.stop,
            Some(ScanStop::BadTrailer { offset: valid_len })
        );
        assert_eq!(report.valid_len, valid_len);
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
        // The payload offset is a file offset: the samples start there and
        // the trailer follows them.
        let payload = usize::try_from(f.payload_offset).unwrap();
        assert_eq!(file[payload..payload + 6], [0, 0, 0, 2, 0x26, 1]);
        assert_eq!(
            f.payload_offset + 13 + TRAILER_LEN as u64,
            f.offset + u64::from(f.len)
        );
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
    fn stops_at_truncation() {
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
    }

    #[test]
    fn stops_at_zero_fill() {
        let full = file_with(2);
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

    #[test]
    fn scan_stops_describe_themselves() {
        assert_eq!(
            ScanStop::BadBox {
                offset: 8,
                kind: *b"ab\x00\xff",
            }
            .to_string(),
            "unexpected box ab\\x00\\xff at 8"
        );
        assert_eq!(
            ScanStop::FragmentLimit {
                offset: 4_096,
                limit: 2,
            }
            .to_string(),
            "more than 2 fragments; the first beyond the limit starts at 4096"
        );
        assert_eq!(
            ScanStop::CrcMismatch { offset: 700 }.to_string(),
            "CRC mismatch on the fragment at 700"
        );
    }
}
