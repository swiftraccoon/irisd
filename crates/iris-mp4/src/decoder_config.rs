//! What a decoder needs from an init segment before the first sample: the
//! parameter sets and picture size of an HEVC track, or the
//! `AudioSpecificConfig`, sampling rate, and channel count of an AAC track.
//!
//! [`DecoderConfig::parse`] reads either from the sample entry of the first
//! track; [`parse_parameter_sets`] and [`parse_hvcc`] read `hvcC` alone. The
//! example of [`DecoderConfig::parse`] reads an init segment of each kind.
//!
//! The readers take untrusted bytes: they refuse what they cannot use, copy
//! out no more than a declared maximum (96 parameter sets, an
//! `AudioSpecificConfig` of 64 bytes), and never panic (fuzz target
//! `decoder_config`).
//!
//! The readers take the layouts of `hvcC` and `esds` that the vendored
//! retina writes for the daemon and that ffmpeg writes; the tests read both.
//! `docs/research/15-apple-clients.md` §1 records the ISO/IEC 14496 texts
//! that define the layouts as not read.

use thiserror::Error;

use crate::boxes::{self, BoxHeader};
use crate::init::sample_entry_bytes;

/// Why an init segment's `hvcC` yields no usable parameter sets: the refusal
/// of [`parse_parameter_sets`] and [`parse_hvcc`], and the cause of a
/// [`DecoderConfigError::Hvcc`]. The example of [`parse_hvcc`] shows one.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Error)]
#[non_exhaustive]
pub enum HvccError {
    /// A box on the path to the sample entry's `hvcC`, or the fields before
    /// it, is missing, open-ended, or does not fit its parent; the value
    /// names the box or the fields.
    #[error("missing or malformed {0}")]
    Missing(&'static str),
    /// The sample entry is neither `hvc1` nor `hev1`; the value is its box
    /// type. The text shows the type as [`escape_ascii`](slice::escape_ascii)
    /// does. A tab, a carriage return, and a line feed show as `\t`, `\r`,
    /// and `\n`. A double quote, a single quote, and a backslash show as
    /// `\"`, `\'`, and `\\`. Any other byte outside printable ASCII shows as
    /// `\xNN`.
    #[error("the sample entry \"{}\" is not HEVC", .0.escape_ascii())]
    NotHevc([u8; 4]),
    /// The configuration version is not 1.
    #[error("unsupported hvcC configuration version")]
    Version,
    /// NAL units carry length prefixes other than four bytes.
    #[error("hvcC does not use four-byte NAL length prefixes")]
    LengthSize,
    /// The record ends inside its fixed fields, an array header, a NAL
    /// unit's length, or a NAL unit.
    #[error("hvcC is truncated")]
    Truncated,
    /// An array header has its reserved bit set.
    #[error("hvcC array reserved bit is set")]
    Reserved,
    /// A NAL unit is shorter than its two-byte header.
    #[error("hvcC NAL unit is shorter than its two-byte header")]
    ShortNal,
    /// A NAL unit's type differs from its array's.
    #[error("hvcC array type {array_type} contains NAL unit type {nal_type}")]
    TypeMismatch {
        /// The NAL unit type the array's header gives, 0 to 63.
        array_type: u8,
        /// The type the NAL unit's header gives, 0 to 63 and other than
        /// `array_type`.
        nal_type: u8,
    },
    /// The record holds more parameter sets of one type than ITU-T H.265
    /// defines identifiers for: more than 16 VPSs, 16 SPSs, or 64 PPSs
    /// ([`parse_hvcc`]). The set past the bound is refused before it is
    /// copied.
    #[error("hvcC holds more than {max} parameter sets of type {nal_type}")]
    TooManyParameterSets {
        /// The sets' NAL unit type: 32 (VPS), 33 (SPS), or 34 (PPS).
        nal_type: u8,
        /// The most sets of that type a record may hold: 16 VPSs, 16 SPSs,
        /// or 64 PPSs.
        max: usize,
    },
    /// Bytes follow the last array.
    #[error("hvcC has trailing bytes")]
    Trailing,
    /// The record holds no set of one of the three parameter-set types.
    #[error("hvcC is missing parameter-set type {nal_type}")]
    MissingParameterSet {
        /// The first type missing, in the order 32 (VPS), 33 (SPS), and 34
        /// (PPS).
        nal_type: u8,
    },
}

/// Why an `mp4a` sample entry yields no usable AAC configuration: the cause
/// of a [`DecoderConfigError::Esds`], which [`DecoderConfig::parse`] returns.
/// The example of [`DecoderConfig::parse`] shows one.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Error)]
#[non_exhaustive]
pub enum EsdsError {
    /// The entry's fixed fields or its `esds` box is missing, open-ended, or
    /// does not fit its parent; the `channelcount` is zero; or a descriptor
    /// lacks the tag its place requires. The value names the part.
    #[error("missing or malformed {0}")]
    Missing(&'static str),
    /// The `esds` ends before its version and flags, or a descriptor or a
    /// field inside one runs past what holds it.
    #[error("esds is truncated")]
    Truncated,
    /// A descriptor's length runs on past four bytes.
    #[error("an esds descriptor length runs past four bytes")]
    DescriptorLength,
    /// The object type indication is not MPEG-4 audio (`0x40`); the value is
    /// the indication.
    #[error("esds object type {0:#04x} is not MPEG-4 audio")]
    ObjectType(u8),
    /// The `AudioSpecificConfig` is shorter than 2 bytes or longer than 64;
    /// the value is its length.
    #[error("an AudioSpecificConfig length of {0} is outside 2 to 64 bytes")]
    ConfigLength(usize),
    /// The sampling frequency index is reserved (13 or 14), or is 15 with an
    /// explicit rate of zero; the value is the index.
    #[error("sampling frequency index {0} of the AudioSpecificConfig gives no rate")]
    SamplingIndex(u8),
    /// The audio object type is the escape value 31.
    #[error("the AudioSpecificConfig uses the escape audio object type")]
    AudioObjectType,
}

/// Why an init segment yields no decoder configuration: the refusal of
/// [`DecoderConfig::parse`], whose example shows one.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Error)]
#[non_exhaustive]
pub enum DecoderConfigError {
    /// The init segment holds no whole sample entry at
    /// `moov/trak/mdia/minf/stbl/stsd` ([`sample_entry_bytes`]).
    #[error("the init segment has no sample entry")]
    NoSampleEntry,
    /// The sample entry is neither HEVC (`hvc1`, `hev1`) nor AAC (`mp4a`);
    /// the value is its box type. The text shows the type with the escapes
    /// of [`HvccError::NotHevc`].
    #[error("the sample entry \"{}\" is neither HEVC nor AAC", .0.escape_ascii())]
    Unsupported([u8; 4]),
    /// The fields or the `hvcC` of an HEVC sample entry are missing or
    /// unusable.
    #[error("reading the HEVC configuration: {0}")]
    Hvcc(#[from] HvccError),
    /// The fields or the `esds` of an AAC sample entry are missing or
    /// unusable.
    #[error("reading the AAC configuration: {0}")]
    Esds(#[from] EsdsError),
}

/// What an HEVC decoder needs from an `hvc1` or `hev1` sample entry: the
/// value of [`DecoderConfig::Hevc`]. Only [`DecoderConfig::parse`] makes one,
/// so every value holds what its accessors promise; the example of
/// [`DecoderConfig::parse`] reads one.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct HevcConfig {
    parameter_sets: Vec<Vec<u8>>,
    width: u16,
    height: u16,
}

impl HevcConfig {
    /// The VPS, SPS, and PPS NAL units of `hvcC` in the record's order,
    /// without length prefixes, as [`parse_hvcc`] returns them. They hold at
    /// least one set of each type and at most 16 VPSs, 16 SPSs, and 64 PPSs,
    /// one for each identifier that ITU-T H.265 defines for the type. That
    /// makes at most 96 sets of at most 65,535 bytes each. The NAL units of
    /// the samples carry four-byte length prefixes, the one size
    /// [`parse_hvcc`] admits. The example of [`DecoderConfig::parse`] reads
    /// them.
    #[must_use]
    pub fn parameter_sets(&self) -> &[Vec<u8>] {
        &self.parameter_sets
    }

    /// The width field of the visual sample entry, in pixels. The example of
    /// [`DecoderConfig::parse`] reads it.
    #[must_use]
    pub fn width(&self) -> u16 {
        self.width
    }

    /// The height field of the visual sample entry, in pixels. The example
    /// of [`DecoderConfig::parse`] reads it.
    #[must_use]
    pub fn height(&self) -> u16 {
        self.height
    }
}

/// What an AAC decoder needs from an `mp4a` sample entry: the value of
/// [`DecoderConfig::Aac`]. Only [`DecoderConfig::parse`] makes one, so every
/// value holds what its accessors promise; the example of
/// [`DecoderConfig::parse`] reads one.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct AacConfig {
    audio_specific_config: Vec<u8>,
    sample_rate: u32,
    channels: u16,
}

impl AacConfig {
    /// The `AudioSpecificConfig` of the entry's `esds`, 2 to 64 bytes, as it
    /// stands. The example of [`DecoderConfig::parse`] reads it.
    #[must_use]
    pub fn audio_specific_config(&self) -> &[u8] {
        &self.audio_specific_config
    }

    /// The sampling rate the `AudioSpecificConfig` declares, in hertz, never
    /// zero: by its sampling frequency index, or in the 24 bits after index
    /// 15. The example of [`DecoderConfig::parse`] reads it.
    #[must_use]
    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    /// The entry's `channelcount` field, never zero. The example of
    /// [`DecoderConfig::parse`] reads it.
    #[must_use]
    pub fn channels(&self) -> u16 {
        self.channels
    }
}

/// The configuration a decoder needs before the first sample, by the kind of
/// sample entry. [`DecoderConfig::parse`] reads one, and its example reads
/// both kinds.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum DecoderConfig {
    /// An `hvc1` or `hev1` entry.
    Hevc(HevcConfig),
    /// An `mp4a` entry of MPEG-4 audio.
    Aac(AacConfig),
}

impl DecoderConfig {
    /// Reads the decoder configuration of the sample entry of an init
    /// segment's first track. For `hvc1` and `hev1` it is a [`HevcConfig`]:
    /// the parameter sets of `hvcC` and the picture size. For `mp4a` it is an
    /// [`AacConfig`]: the `AudioSpecificConfig` of `esds`, its sampling rate,
    /// and the channel count. [`TryFrom<&[u8]>`](TryFrom) does the same.
    ///
    /// After its version and flags, an `esds` holds an `ES_Descriptor` (tag
    /// 3), whose flags announce the fields to step over. Inside it is a
    /// `DecoderConfigDescriptor` (tag 4) of MPEG-4 audio. Inside that is a
    /// `DecoderSpecificInfo` (tag 5): the `AudioSpecificConfig`. A
    /// descriptor's length takes one to four bytes, high bits first, in the
    /// form that ffmpeg writes (`crates/iris-mp4/tests/ffmpeg_mp4a.rs`). The
    /// vendored retina writes a length under 128 in one byte. For an
    /// `AudioSpecificConfig` of at most 64 bytes, every descriptor length of
    /// the `esds` that retina writes is under 128; the longest, 87, is that
    /// of the `ES_Descriptor`. Retina writes a longer length low bits first,
    /// which this reader does not take.
    ///
    /// # Errors
    ///
    /// - [`DecoderConfigError::NoSampleEntry`] when the init segment holds no
    ///   whole sample entry.
    /// - [`DecoderConfigError::Unsupported`] for an entry of another kind.
    /// - [`DecoderConfigError::Hvcc`] for an HEVC entry whose fields or
    ///   `hvcC` are missing or which [`parse_hvcc`] refuses.
    /// - [`DecoderConfigError::Esds`] for an AAC entry whose fields, `esds`,
    ///   or descriptors are missing or cut short, whose descriptor length
    ///   runs past four bytes, or whose channel count is zero.
    /// - [`DecoderConfigError::Esds`] for an AAC entry whose object type is
    ///   not MPEG-4 audio, or whose `AudioSpecificConfig` is outside 2 to 64
    ///   bytes, uses the escape audio object type, or gives no sampling rate.
    ///
    /// A box of size 0 on the way to the configuration, which would run to
    /// the end of its parent, counts as missing.
    ///
    /// # Example
    ///
    /// ```
    /// use iris_mp4::decoder_config::{
    ///     DecoderConfig, DecoderConfigError, EsdsError,
    /// };
    /// # use std::error::Error;
    /// #
    /// # use iris_mp4::boxes::BoxWriter;
    /// # use iris_mp4::init::{SampleEntry, build_init_segment};
    ///
    /// // `audio` is the init segment of an `mp4a` entry of AAC-LC at 48 kHz in
    /// // one channel. `video` is that of a 1920×1080 `hvc1` entry whose
    /// // `hvcC` holds a VPS, an SPS, and a PPS, each of them only its
    /// // two-byte NAL unit header.
    /// # let mut w = BoxWriter::default();
    /// # let mp4a = w.begin(b"mp4a");
    /// # w.zeros(6);
    /// # w.u16(1); // data_reference_index
    /// # w.zeros(8);
    /// # w.u16(1); // channelcount
    /// # w.u16(16); // samplesize
    /// # w.zeros(4);
    /// # w.u32(48_000 << 16); // samplerate
    /// # let esds = w.begin_full(b"esds", 0, 0);
    /// # // An ES_Descriptor of ES_ID 1 whose flags announce no field.
    /// # w.bytes(&[0x03, 22, 0, 1, 0]);
    /// # // A DecoderConfigDescriptor of MPEG-4 audio.
    /// # w.bytes(&[0x04, 17, 0x40, 0x15]);
    /// # w.zeros(11); // bufferSizeDB, maxBitrate, avgBitrate
    /// # w.bytes(&[0x05, 2, 0x11, 0x88]); // DecoderSpecificInfo
    /// # w.end(esds);
    /// # w.end(mp4a);
    /// # let entry = SampleEntry::audio(w.into_bytes(), "mp4a.40.2".into());
    /// # let audio = build_init_segment(&entry);
    /// # let mut w = BoxWriter::default();
    /// # let hvc1 = w.begin(b"hvc1");
    /// # w.zeros(6);
    /// # w.u16(1); // data_reference_index
    /// # w.zeros(16);
    /// # w.u16(1920);
    /// # w.u16(1080);
    /// # // horizresolution, vertresolution, reserved, frame_count,
    /// # // compressorname, depth, and pre_defined.
    /// # w.zeros(50);
    /// # let hvcc = w.begin(b"hvcC");
    /// # w.u8(1); // configurationVersion
    /// # w.zeros(20); // profile, tier, level, and the fields after them
    /// # w.u8(0x0f); // lengthSizeMinusOne: four-byte NAL lengths
    /// # w.u8(3); // numOfArrays
    /// # for nal_type in [32, 33, 34] {
    /// #     w.bytes(&[0x80 | nal_type, 0, 1, 0, 2, nal_type << 1, 1]);
    /// # }
    /// # w.end(hvcc);
    /// # w.end(hvc1);
    /// # let entry = SampleEntry::new(w.into_bytes(), "hvc1".into(), 1920, 1080);
    /// # let video = build_init_segment(&entry);
    ///
    /// // An audio decoder takes the AudioSpecificConfig as it stands.
    /// let DecoderConfig::Aac(aac) = DecoderConfig::parse(&audio)? else {
    ///     return Err("an mp4a entry gives AAC".into());
    /// };
    /// assert_eq!(aac.audio_specific_config(), [0x11, 0x88]);
    /// assert_eq!((aac.sample_rate(), aac.channels()), (48_000, 1));
    ///
    /// // A video decoder takes the parameter sets and the picture size; the
    /// // samples carry four-byte NAL unit lengths.
    /// let DecoderConfig::Hevc(hevc) = DecoderConfig::parse(&video)? else {
    ///     return Err("an hvc1 entry gives HEVC".into());
    /// };
    /// let sets = hevc.parameter_sets();
    /// assert_eq!(sets, [[32 << 1, 1], [33 << 1, 1], [34 << 1, 1]]);
    /// assert_eq!((hevc.width(), hevc.height()), (1920, 1080));
    ///
    /// // An init segment cut short holds no whole sample entry.
    /// let refused = DecoderConfig::parse(&video[..video.len() - 1]);
    /// assert_eq!(refused, Err(DecoderConfigError::NoSampleEntry));
    ///
    /// // An `mp4a` entry whose `esds` names MPEG-1 audio, object type `0x6b`,
    /// // holds no AAC configuration.
    /// # let mut mpeg1 = audio.to_vec();
    /// # let object_type = mpeg1
    /// #     .windows(3)
    /// #     .position(|window| window == [0x04, 17, 0x40])
    /// #     .ok_or("the DecoderConfigDescriptor")?
    /// #     + 2;
    /// # mpeg1[object_type] = 0x6b;
    /// let refused = DecoderConfig::parse(&mpeg1);
    /// assert_eq!(
    ///     refused,
    ///     Err(DecoderConfigError::Esds(EsdsError::ObjectType(0x6b)))
    /// );
    /// # Ok::<(), Box<dyn Error>>(())
    /// ```
    pub fn parse(init: &[u8]) -> Result<Self, DecoderConfigError> {
        let entry = sample_entry_bytes(init).ok_or(DecoderConfigError::NoSampleEntry)?;
        let found = first_box(entry).ok_or(DecoderConfigError::NoSampleEntry)?;
        match &found.kind {
            b"hvc1" | b"hev1" => Ok(Self::Hevc(hevc_config(found.body)?)),
            b"mp4a" => Ok(Self::Aac(aac_config(found.body)?)),
            _ => Err(DecoderConfigError::Unsupported(found.kind)),
        }
    }
}

impl TryFrom<&[u8]> for DecoderConfig {
    type Error = DecoderConfigError;

    fn try_from(init: &[u8]) -> Result<Self, Self::Error> {
        Self::parse(init)
    }
}

/// Extracts the VPS, SPS, and PPS NAL units from the `hvcC` of the sample
/// entry of an init segment's first track.
///
/// # Errors
///
/// - [`HvccError::Missing`] when a box on the path
///   `moov`/`trak`/`mdia`/`minf`/`stbl`/`stsd` is absent, open-ended (a size
///   of 0), or does not fit its parent. The same error covers the entry count
///   of `stsd`, the sample entry, its fixed fields, and its `hvcC`.
/// - [`HvccError::NotHevc`] for a sample entry other than `hvc1` or `hev1`.
/// - Every error of [`parse_hvcc`].
///
/// # Example
///
/// ```
/// use iris_mp4::boxes::BoxWriter;
/// use iris_mp4::decoder_config::{HvccError, parse_parameter_sets};
/// use iris_mp4::init::{SampleEntry, build_init_segment};
///
/// // An `hvc1` entry of 1920×1080 whose `hvcC` holds a VPS, an SPS, and a
/// // PPS, each of them only its two-byte NAL unit header.
/// let mut w = BoxWriter::default();
/// let entry = w.begin(b"hvc1");
/// w.zeros(6);
/// w.u16(1); // data_reference_index
/// w.zeros(16);
/// w.u16(1920);
/// w.u16(1080);
/// // horizresolution, vertresolution, reserved, frame_count, compressorname,
/// // depth, and pre_defined.
/// w.zeros(50);
/// let hvcc = w.begin(b"hvcC");
/// w.u8(1); // configurationVersion
/// w.zeros(20); // profile, tier, level, and the fields after them
/// w.u8(0x0f); // lengthSizeMinusOne: four-byte NAL lengths
/// w.u8(3); // numOfArrays
/// for nal_type in [32, 33, 34] {
///     w.bytes(&[0x80 | nal_type, 0, 1, 0, 2, nal_type << 1, 1]);
/// }
/// w.end(hvcc);
/// w.end(entry);
/// let entry = SampleEntry::new(w.into_bytes(), "hvc1".into(), 1920, 1080);
/// let init = build_init_segment(&entry);
///
/// let sets = parse_parameter_sets(&init)?;
/// assert_eq!(sets, [[32 << 1, 1], [33 << 1, 1], [34 << 1, 1]]);
/// # Ok::<(), HvccError>(())
/// ```
pub fn parse_parameter_sets(init: &[u8]) -> Result<Vec<Vec<u8>>, HvccError> {
    let moov = boxes::child(init, *b"moov").ok_or(HvccError::Missing("moov"))?;
    let trak = boxes::child(moov, *b"trak").ok_or(HvccError::Missing("trak"))?;
    let mdia = boxes::child(trak, *b"mdia").ok_or(HvccError::Missing("mdia"))?;
    let minf = boxes::child(mdia, *b"minf").ok_or(HvccError::Missing("minf"))?;
    let stbl = boxes::child(minf, *b"stbl").ok_or(HvccError::Missing("stbl"))?;
    let stsd = boxes::child(stbl, *b"stsd").ok_or(HvccError::Missing("stsd"))?;
    // stsd is a full box: version/flags then entry_count, then the entries.
    let entries = stsd.get(8..).ok_or(HvccError::Missing("stsd entries"))?;
    let entry = first_box(entries).ok_or(HvccError::Missing("sample entry"))?;
    if &entry.kind != b"hvc1" && &entry.kind != b"hev1" {
        return Err(HvccError::NotHevc(entry.kind));
    }
    entry_parameter_sets(entry.body)
}

/// Reads the NAL unit arrays of an `hvcC` body and returns its VPS, SPS, and
/// PPS NAL units in order. It reads at most 16 VPSs, 16 SPSs, and 64 PPSs,
/// one for each identifier that ITU-T H.265 defines for the type
/// ([`HvccError::TooManyParameterSets`]). The set past the bound of its type
/// is refused before it is copied, so a record of any length yields at most
/// 96 sets of at most 65,535 bytes each.
///
/// The function takes the record layout that the vendored retina writes for
/// the daemon (`third_party/retina/src/codec/h265/record.rs`) and that
/// ffmpeg writes (`docs/research/15-apple-clients.md` §11). ffmpeg decoded
/// the test daemon's streams with the sets this reader gives (§3.6 of that
/// document).
///
/// # Errors
///
/// An [`HvccError`] for:
///
/// - a configuration version other than 1, or length prefixes other than
///   four bytes;
/// - a record cut short, or bytes after the last array;
/// - an array header whose reserved bit is set;
/// - a NAL unit shorter than its header or of another type than its array;
/// - a parameter set past the bound of its type
///   ([`HvccError::TooManyParameterSets`]);
/// - no VPS, no SPS, or no PPS.
///
/// # Example
///
/// ```
/// use iris_mp4::decoder_config::{HvccError, parse_hvcc};
///
/// // A record of version 1 with four-byte NAL lengths and an array of each
/// // parameter set, each set its NAL unit header and a byte.
/// let mut record = vec![1];
/// record.extend([0; 20]); // profile, tier, level, and the fields after them
/// record.extend([0x0f, 3]); // lengthSizeMinusOne, numOfArrays
/// for nal_type in [32, 33, 34] {
///     record.extend([0x80 | nal_type, 0, 1, 0, 3, nal_type << 1, 1, 0]);
/// }
/// let sets = parse_hvcc(&record)?;
/// assert_eq!(sets, [[32 << 1, 1, 0], [33 << 1, 1, 0], [34 << 1, 1, 0]]);
///
/// // Cut inside its last NAL unit, the record is refused.
/// let cut = &record[..record.len() - 1];
/// assert_eq!(parse_hvcc(cut), Err(HvccError::Truncated));
/// # Ok::<(), HvccError>(())
/// ```
pub fn parse_hvcc(body: &[u8]) -> Result<Vec<Vec<u8>>, HvccError> {
    if body.first() != Some(&1) {
        return Err(HvccError::Version);
    }
    if body.get(21).ok_or(HvccError::Truncated)? & 0x03 != 3 {
        return Err(HvccError::LengthSize);
    }
    let count = usize::from(*body.get(22).ok_or(HvccError::Truncated)?);
    let byte = |pos: usize| body.get(pos).copied().ok_or(HvccError::Truncated);
    let mut pos = 23;
    let mut sets = Vec::new();
    // Each parameter-set type with its bound and the count of its sets read.
    let mut held = PARAMETER_SET_BOUNDS.map(|(nal_type, max)| (nal_type, max, 0_usize));
    for _ in 0..count {
        let array_header = byte(pos)?;
        if array_header & 0x40 != 0 {
            return Err(HvccError::Reserved);
        }
        let array_type = array_header & 0x3f;
        let nalus = usize::from(u16::from_be_bytes([byte(pos + 1)?, byte(pos + 2)?]));
        pos += 3;
        for _ in 0..nalus {
            let len = usize::from(u16::from_be_bytes([byte(pos)?, byte(pos + 1)?]));
            pos += 2;
            let end = pos.checked_add(len).ok_or(HvccError::Truncated)?;
            let nal = body.get(pos..end).ok_or(HvccError::Truncated)?;
            let &[first, _, ..] = nal else {
                return Err(HvccError::ShortNal);
            };
            let nal_type = (first >> 1) & 0x3f;
            if nal_type != array_type {
                return Err(HvccError::TypeMismatch {
                    array_type,
                    nal_type,
                });
            }
            // A NAL unit that is not a VPS, an SPS, or a PPS is passed over.
            if let Some((_, max, read)) = held.iter_mut().find(|(kind, ..)| *kind == nal_type) {
                // The set past the bound is refused before it is copied, so
                // no record holds more than `max` copies of a type.
                if *read == *max {
                    return Err(HvccError::TooManyParameterSets {
                        nal_type,
                        max: *max,
                    });
                }
                *read += 1;
                sets.push(nal.to_vec());
            }
            pos = end;
        }
    }
    if pos != body.len() {
        return Err(HvccError::Trailing);
    }
    for (nal_type, _, read) in held {
        if read == 0 {
            return Err(HvccError::MissingParameterSet { nal_type });
        }
    }
    Ok(sets)
}

/// The NAL unit types of the parameter sets, VPS (32), SPS (33), and PPS
/// (34), each with the most sets of it that [`parse_hvcc`] reads from one
/// record. The bound is one set for each identifier that ITU-T H.265
/// defines for the type. `vps_video_parameter_set_id` is u(4) (§7.3.2.1),
/// so 16 VPSs. "The value of `sps_seq_parameter_set_id` shall be in the
/// range of 0 to 15, inclusive" (§7.4.3.2.1), so 16 SPSs. "The value of
/// `pps_pic_parameter_set_id` shall be in the range of 0 to 63, inclusive"
/// (§7.4.3.3.1), so 64 PPSs. A slice segment, a set, or an SEI message
/// refers to a set by its identifier (§7.4.2.4.2), so a decoder tells apart
/// at most that many sets of a type. `docs/research/15-apple-clients.md` §1
/// names the edition of ITU-T H.265 that these clauses come from. Under
/// these bounds one record allocates at most 96 sets of at most 65,535
/// bytes each, whatever its length.
const PARAMETER_SET_BOUNDS: [(u8, usize); 3] = [(32, 16), (33, 16), (34, 64)];

/// The configuration of an `hvc1` or `hev1` entry from the `body` after its
/// box header.
fn hevc_config(body: &[u8]) -> Result<HevcConfig, HvccError> {
    // Six reserved bytes, the data reference index, and sixteen bytes of
    // predefined and reserved fields come before the width and the height.
    let Some(&[w0, w1, h0, h1]) = body.get(24..28) else {
        return Err(HvccError::Missing("sample entry fields"));
    };
    Ok(HevcConfig {
        parameter_sets: entry_parameter_sets(body)?,
        width: u16::from_be_bytes([w0, w1]),
        height: u16::from_be_bytes([h0, h1]),
    })
}

/// The parameter sets of the `hvcC` among the children of a visual sample
/// entry, from the `body` after its box header.
fn entry_parameter_sets(body: &[u8]) -> Result<Vec<Vec<u8>>, HvccError> {
    // A visual sample entry has 78 bytes of fixed fields after the box header.
    let children = body
        .get(78..)
        .ok_or(HvccError::Missing("sample entry fields"))?;
    let hvcc = boxes::child(children, *b"hvcC").ok_or(HvccError::Missing("hvcC"))?;
    parse_hvcc(hvcc)
}

/// The configuration of an `mp4a` entry from the `body` after its box
/// header, whether that header is 8 bytes or 16.
fn aac_config(body: &[u8]) -> Result<AacConfig, EsdsError> {
    // An audio sample entry has 28 bytes of fixed fields after the box header.
    // Six reserved bytes, the data reference index, and eight reserved bytes
    // come before the channel count.
    let (Some(&[high, low]), Some(children)) = (body.get(16..18), body.get(28..)) else {
        return Err(EsdsError::Missing("sample entry fields"));
    };
    let esds = boxes::child(children, *b"esds").ok_or(EsdsError::Missing("esds"))?;
    let config = audio_specific_config(esds)?;
    let sample_rate = sampling_rate(config)?;
    let channels = u16::from_be_bytes([high, low]);
    if channels == 0 {
        return Err(EsdsError::Missing("channelcount"));
    }
    Ok(AacConfig {
        audio_specific_config: config.to_vec(),
        sample_rate,
        channels,
    })
}

/// The `AudioSpecificConfig` in the body of an `esds` box: past a version
/// and flags, the `DecoderSpecificInfo` inside the `DecoderConfigDescriptor`
/// inside the `ES_Descriptor`.
fn audio_specific_config(esds: &[u8]) -> Result<&[u8], EsdsError> {
    let descriptors = esds.get(4..).ok_or(EsdsError::Truncated)?;
    let es = descriptor(descriptors, 0x03, "ES_Descriptor")?;
    // ES_ID, then the flags; each of their top three bits announces a field
    // before the next descriptor, in this order.
    let (&flags, mut rest) = es
        .get(2..)
        .and_then(<[u8]>::split_first)
        .ok_or(EsdsError::Truncated)?;
    if flags & 0x80 != 0 {
        // streamDependenceFlag: dependsOn_ES_ID.
        rest = rest.get(2..).ok_or(EsdsError::Truncated)?;
    }
    if flags & 0x40 != 0 {
        // URL_Flag: URLlength, then the URL.
        let (&len, url) = rest.split_first().ok_or(EsdsError::Truncated)?;
        rest = url.get(usize::from(len)..).ok_or(EsdsError::Truncated)?;
    }
    if flags & 0x20 != 0 {
        // OCRstreamFlag: OCR_ES_Id.
        rest = rest.get(2..).ok_or(EsdsError::Truncated)?;
    }
    let decoder = descriptor(rest, 0x04, "DecoderConfigDescriptor")?;
    let &object_type = decoder.first().ok_or(EsdsError::Truncated)?;
    if object_type != 0x40 {
        return Err(EsdsError::ObjectType(object_type));
    }
    // The object type, the stream type, bufferSizeDB, maxBitrate, and
    // avgBitrate come before the decoder's specific information.
    let specific = decoder.get(13..).ok_or(EsdsError::Truncated)?;
    descriptor(specific, 0x05, "DecoderSpecificInfo")
}

/// The body of the descriptor at the start of `buf`, which must carry `tag`;
/// `name` names it in a refusal. A descriptor is its tag, a length in one to
/// four bytes of seven bits each, high bits first, whose top bit says another
/// byte follows, and that many bytes. ffmpeg writes every length in that
/// form, in four bytes (`crates/iris-mp4/tests/ffmpeg_mp4a.rs`).
///
/// The vendored retina writes a length under 128 in one byte, and a longer
/// one with its low bits first (`set_iso14496_length` in
/// `third_party/retina/src/codec/mod.rs`), which this reader misreads. Every
/// length of the `esds` that retina writes for an `AudioSpecificConfig` of at
/// most 64 bytes is under 128; the longest, 87, is that of the
/// `ES_Descriptor`.
fn descriptor<'a>(buf: &'a [u8], tag: u8, name: &'static str) -> Result<&'a [u8], EsdsError> {
    let (&found, mut rest) = buf.split_first().ok_or(EsdsError::Truncated)?;
    if found != tag {
        return Err(EsdsError::Missing(name));
    }
    let mut len = 0;
    for _ in 0..4 {
        let (&byte, after) = rest.split_first().ok_or(EsdsError::Truncated)?;
        rest = after;
        len = (len << 7) | usize::from(byte & 0x7f);
        if byte & 0x80 == 0 {
            return rest.get(..len).ok_or(EsdsError::Truncated);
        }
    }
    Err(EsdsError::DescriptorLength)
}

/// The rates in hertz of sampling frequency indices 0 to 12 of an
/// `AudioSpecificConfig`: the table of the vendored retina's reader
/// (`AudioSpecificConfig::parse` in `third_party/retina/src/codec/aac.rs`).
/// ffmpeg decoded the test daemon's audio at 64,000 Hz, the rate of index 2,
/// and reports 48,000 Hz, the rate of index 3, for an init segment it wrote
/// (`docs/research/15-apple-clients.md` §3.6 and §11).
const SAMPLING_RATES: [u32; 13] = [
    96_000, 88_200, 64_000, 48_000, 44_100, 32_000, 24_000, 22_050, 16_000, 12_000, 11_025, 8_000,
    7_350,
];

/// The longest `AudioSpecificConfig` read, in bytes. The parser sets this
/// bound to limit what it copies; the vendored retina writes a configuration
/// of any length. The test daemon's audio carried a configuration of 2
/// bytes, and ffmpeg's AAC encoder writes one of 5
/// (`docs/research/15-apple-clients.md` §3.1 and §11).
const MAX_AUDIO_SPECIFIC_CONFIG_LEN: usize = 64;

/// The sampling rate that an `AudioSpecificConfig` of 2 to 64 bytes
/// declares. It starts with five bits of audio object type and four of
/// sampling frequency index into `SAMPLING_RATES`; index 15 says the rate
/// follows in 24 bits.
fn sampling_rate(config: &[u8]) -> Result<u32, EsdsError> {
    let &[first, second, ..] = config else {
        return Err(EsdsError::ConfigLength(config.len()));
    };
    if config.len() > MAX_AUDIO_SPECIFIC_CONFIG_LEN {
        return Err(EsdsError::ConfigLength(config.len()));
    }
    if first >> 3 == 31 {
        return Err(EsdsError::AudioObjectType);
    }
    let index = ((first & 0x07) << 1) | (second >> 7);
    if let Some(&rate) = SAMPLING_RATES.get(usize::from(index)) {
        return Ok(rate);
    }
    if index != 15 {
        return Err(EsdsError::SamplingIndex(index));
    }
    // The explicit rate is bits 9 to 32 of the configuration.
    let &[_, b1, b2, b3, b4, ..] = config else {
        return Err(EsdsError::Truncated);
    };
    match (u32::from_be_bytes([b1, b2, b3, b4]) >> 7) & 0x00ff_ffff {
        0 => Err(EsdsError::SamplingIndex(15)),
        rate => Ok(rate),
    }
}

/// The type of a box and its body, the bytes after its header.
struct FoundBox<'buffer> {
    kind: [u8; 4],
    body: &'buffer [u8],
}

/// The box at the start of `buf`. `None` when `buf` is shorter than the box
/// header, when the size field is smaller than the header, or when the box
/// is open-ended (a size of 0) or claims more than `buf` holds.
/// `boxes::child` refuses such a box too.
fn first_box(buf: &[u8]) -> Option<FoundBox<'_>> {
    let header = BoxHeader::parse(buf)?;
    let size = usize::try_from(header.size?).ok()?;
    let body = buf.get(usize::from(header.header_len)..size)?;
    Some(FoundBox {
        kind: header.kind,
        body,
    })
}

#[cfg(test)]
mod tests {
    use std::fmt;

    use bytes::Bytes;

    use super::*;
    use crate::boxes::BoxWriter;
    use crate::init::{SampleEntry, build_init_segment};

    /// The width and height fields of the HEVC fixtures' visual sample entry.
    const W: u16 = 1920;
    const H: u16 = 1080;

    /// A VPS, an SPS, and a PPS: each a NAL unit header of its type and a
    /// byte of payload, which is all the `hvcC` reader looks at.
    const VPS: [u8; 3] = [32 << 1, 1, 0x0c];
    const SPS: [u8; 3] = [33 << 1, 1, 0x01];
    const PPS: [u8; 3] = [34 << 1, 1, 0xc1];

    fn hvcc(arrays: &[(u8, Vec<Vec<u8>>)]) -> Vec<u8> {
        let mut body = vec![
            1, 0x01, 0x60, 0, 0, 0, 0x90, 0, 0, 0, 0, 0, 0x5a, 0xf0, 0x00, 0xfc, 0xfd, 0xf8, 0xf8,
            0, 0, 0x0f,
        ];
        body.push(u8::try_from(arrays.len()).unwrap());
        for (kind, nals) in arrays {
            body.push(0x80 | kind);
            body.extend_from_slice(&u16::try_from(nals.len()).unwrap().to_be_bytes());
            for nal in nals {
                body.extend_from_slice(&u16::try_from(nal.len()).unwrap().to_be_bytes());
                body.extend_from_slice(nal);
            }
        }
        body
    }

    fn required_hvcc_arrays() -> Vec<(u8, Vec<Vec<u8>>)> {
        vec![
            (32, vec![VPS.to_vec()]),
            (33, vec![SPS.to_vec()]),
            (34, vec![PPS.to_vec()]),
        ]
    }

    /// A sample entry of box type `kind` with the fields of a visual sample
    /// entry of `W` by `H` and an `hvcC` box whose body is `hvcc`.
    fn visual_entry(kind: [u8; 4], hvcc: &[u8]) -> SampleEntry {
        let mut w = BoxWriter::default();
        let entry = w.begin(&kind);
        w.zeros(6);
        w.u16(1); // data_reference_index
        w.zeros(16); // pre_defined and reserved
        w.u16(W);
        w.u16(H);
        w.u32(0x0048_0000); // horizresolution, 72 dpi
        w.u32(0x0048_0000); // vertresolution
        w.u32(0); // reserved
        w.u16(1); // frame_count
        w.zeros(32); // compressorname
        w.u16(0x18); // depth
        w.i16(-1); // pre_defined
        let config = w.begin(b"hvcC");
        w.bytes(hvcc);
        w.end(config);
        w.end(entry);
        SampleEntry::new(w.into_bytes(), "hvc1.1.6.L120.90".into(), W, H)
    }

    /// An init segment whose sample entry has the box type `kind` and the
    /// layout of a visual sample entry of `W` by `H`, with an `hvcC` that
    /// holds a VPS, an SPS, and a PPS.
    fn init_with_entry(kind: [u8; 4]) -> Bytes {
        build_init_segment(&visual_entry(kind, &hvcc(&required_hvcc_arrays())))
    }

    /// How a descriptor's length is written: in one byte, as retina writes a
    /// length under 128, or padded to four, as ffmpeg writes every length
    /// (the `esds` of `tests/ffmpeg_mp4a.rs`).
    #[derive(Clone, Copy, Debug)]
    enum LengthForm {
        OneByte,
        FourBytes,
    }

    /// An MPEG-4 descriptor: `tag`, the length of `body` in `form`, and `body`.
    fn encode_descriptor(tag: u8, body: &[u8], form: LengthForm) -> Vec<u8> {
        let len = body.len();
        let mut out = vec![tag];
        match form {
            LengthForm::OneByte => {
                assert!(len < 0x80, "{len} bytes need a longer length");
                out.push(u8::try_from(len).unwrap());
            }
            LengthForm::FourBytes => {
                for shift in [21, 14, 7] {
                    out.push(0x80 | u8::try_from((len >> shift) & 0x7f).unwrap());
                }
                out.push(u8::try_from(len & 0x7f).unwrap());
            }
        }
        out.extend_from_slice(body);
        out
    }

    /// The `ES_ID` and the flags of an `ES_Descriptor` whose flags announce
    /// no further field.
    const ES_FIELDS: [u8; 3] = [0, 0, 0];

    /// The body of an `esds` box as retina writes it: a version and flags,
    /// then an `ES_Descriptor`. That descriptor holds `es_fields` (`ES_ID`,
    /// the flags, and the fields they announce), a `DecoderConfigDescriptor`
    /// of `object_type` around a `DecoderSpecificInfo` that holds `config`,
    /// and an `SLConfigDescriptor`. The buffer size and the bit rates are
    /// those that retina writes for one channel at 48 kHz.
    fn esds(es_fields: &[u8], object_type: u8, config: &[u8], form: LengthForm) -> Vec<u8> {
        let mut decoder = vec![object_type, 0x15]; // an audio stream
        decoder.extend_from_slice(&[0, 0x03, 0]); // bufferSizeDB: 768 bytes
        decoder.extend_from_slice(&288_000u32.to_be_bytes()); // maxBitrate
        decoder.extend_from_slice(&0u32.to_be_bytes()); // avgBitrate: variable
        decoder.extend(encode_descriptor(0x05, config, form));
        let mut es = es_fields.to_vec();
        es.extend(encode_descriptor(0x04, &decoder, form));
        es.extend(encode_descriptor(0x06, &[2], form)); // predefined for MP4 files
        let mut body = vec![0; 4]; // version and flags
        body.extend(encode_descriptor(0x03, &es, form));
        body
    }

    /// An `mp4a` sample entry laid out as retina lays one out, with `body`
    /// as the body of its `esds` box.
    fn mp4a(channels: u16, rate: u16, body: &[u8]) -> SampleEntry {
        let mut w = BoxWriter::default();
        let entry = w.begin(b"mp4a");
        w.zeros(6);
        w.u16(1); // data_reference_index
        w.zeros(8);
        w.u16(channels);
        w.u16(16); // samplesize
        w.zeros(4); // pre_defined and reserved
        w.u32(u32::from(rate) << 16);
        let esds = w.begin(b"esds");
        w.bytes(body);
        w.end(esds);
        w.end(entry);
        SampleEntry::audio(w.into_bytes(), "mp4a.40.2".into())
    }

    /// The `mp4a` entry that `make_sample_entry` of
    /// `third_party/retina/src/codec/aac.rs` writes, with its descriptor
    /// lengths in `form`. The buffer size and the bit rates are those that
    /// retina writes for one channel at 48 kHz, whatever `channels` and
    /// `rate` are.
    fn mp4a_entry(channels: u16, rate: u16, config: &[u8], form: LengthForm) -> SampleEntry {
        mp4a(channels, rate, &esds(&ES_FIELDS, 0x40, config, form))
    }

    /// What `DecoderConfig::parse` reads from the init segment of `entry`:
    /// its AAC configuration, or the refusal of its `esds`.
    fn read_aac(entry: &SampleEntry) -> Result<AacConfig, EsdsError> {
        match DecoderConfig::parse(&build_init_segment(entry)) {
            Ok(DecoderConfig::Aac(config)) => Ok(config),
            Err(DecoderConfigError::Esds(error)) => Err(error),
            other => panic!("neither AAC nor an esds refusal: {other:?}"),
        }
    }

    /// The reading of a mono 48 kHz entry whose `AudioSpecificConfig` is
    /// `config`.
    fn aac(config: &[u8]) -> Result<AacConfig, EsdsError> {
        read_aac(&mp4a_entry(1, 48_000, config, LengthForm::OneByte))
    }

    /// The reading of an entry whose object type indication is `object_type`.
    fn aac_with_object_type(object_type: u8) -> Result<AacConfig, EsdsError> {
        let esds = esds(&ES_FIELDS, object_type, &[0x11, 0x88], LengthForm::OneByte);
        read_aac(&mp4a(1, 48_000, &esds))
    }

    /// `bytes` with the first box type `kind` in it renamed `free`, a box
    /// no reader looks for.
    fn renamed(bytes: &[u8], kind: [u8; 4]) -> Vec<u8> {
        let at = bytes
            .windows(4)
            .position(|window| window == kind)
            .expect("the fixture holds the box");
        let mut renamed = bytes.to_vec();
        renamed[at..at + 4].copy_from_slice(b"free");
        renamed
    }

    /// `bytes` with the size field of the first box of type `kind` in it set
    /// to 0, which says that the box runs to the end of what holds it.
    fn open_ended(bytes: &[u8], kind: [u8; 4]) -> Vec<u8> {
        let at = bytes
            .windows(4)
            .position(|window| window == kind)
            .expect("the fixture holds the box");
        let mut open = bytes.to_vec();
        open[at - 4..at].copy_from_slice(&[0; 4]);
        open
    }

    /// The box `whole`, which has a 32-bit size, written with a 64-bit
    /// `largesize`: a size field of 1, the type, the size in eight bytes,
    /// and the body.
    fn with_large_size(whole: &[u8]) -> Vec<u8> {
        let size = u32::from_be_bytes(whole[..4].try_into().unwrap());
        let mut large = 1u32.to_be_bytes().to_vec();
        large.extend_from_slice(&whole[4..8]);
        large.extend_from_slice(&(u64::from(size) + 8).to_be_bytes());
        large.extend_from_slice(&whole[8..]);
        large
    }

    /// An init segment whose `stsd` box holds `body` after its header, on
    /// the path `moov`/`trak`/`mdia`/`minf`/`stbl`.
    fn init_with_stsd(body: &[u8]) -> Vec<u8> {
        let mut w = BoxWriter::default();
        let path = [b"moov", b"trak", b"mdia", b"minf", b"stbl", b"stsd"].map(|kind| w.begin(kind));
        w.bytes(body);
        for open in path.into_iter().rev() {
            w.end(open);
        }
        w.into_vec()
    }

    /// An `stsd` body of one entry: a version and flags, an entry count of
    /// one, and an `hvc1` box whose body is `fields`.
    fn stsd_of_hvc1(fields: &[u8]) -> Vec<u8> {
        let mut body = vec![0, 0, 0, 0, 0, 0, 0, 1];
        body.extend_from_slice(&u32::try_from(8 + fields.len()).unwrap().to_be_bytes());
        body.extend_from_slice(b"hvc1");
        body.extend_from_slice(fields);
        body
    }

    /// The init segment of an `hvc1` sample entry whose body is `fields`.
    fn init_with_hvc1_fields(fields: &[u8]) -> Bytes {
        let mut entry = u32::try_from(8 + fields.len())
            .unwrap()
            .to_be_bytes()
            .to_vec();
        entry.extend_from_slice(b"hvc1");
        entry.extend_from_slice(fields);
        build_init_segment(&SampleEntry::new(Bytes::from(entry), String::new(), W, H))
    }

    /// The body of an `esds` box whose `ES_Descriptor` holds `es`, with a
    /// one-byte length.
    fn esds_around(es: &[u8]) -> Vec<u8> {
        let mut body = vec![0; 4]; // version and flags
        body.extend(encode_descriptor(0x03, es, LengthForm::OneByte));
        body
    }

    /// What `DecoderConfig::parse` reads from a mono 48 kHz `mp4a` entry
    /// whose `ES_Descriptor` holds `es`.
    fn read_es(es: &[u8]) -> Result<AacConfig, EsdsError> {
        read_aac(&mp4a(1, 48_000, &esds_around(es)))
    }

    /// The body of an `ES_Descriptor`: `ES_FIELDS`, then a
    /// `DecoderConfigDescriptor` whose body is `decoder`.
    fn es_with_decoder(decoder: &[u8]) -> Vec<u8> {
        let mut es = ES_FIELDS.to_vec();
        es.extend(encode_descriptor(0x04, decoder, LengthForm::OneByte));
        es
    }

    /// The HEVC configuration `DecoderConfig::parse` reads from `init`.
    fn read_hevc(init: &[u8]) -> HevcConfig {
        match DecoderConfig::parse(init) {
            Ok(DecoderConfig::Hevc(config)) => config,
            other => panic!("not HEVC: {other:?}"),
        }
    }

    /// `read` with a refusal paired with its text, so that one comparison
    /// checks the variant and the text of a refusal.
    fn with_text<T, E: fmt::Display>(read: Result<T, E>) -> Result<T, (E, String)> {
        read.map_err(|refusal| {
            let text = refusal.to_string();
            (refusal, text)
        })
    }

    /// The text of [`EsdsError::Truncated`].
    const ESDS_TRUNCATED: &str = "esds is truncated";

    #[test]
    fn decoder_config_reads_the_parameter_sets_of_hvcc() {
        let config = read_hevc(&init_with_entry(*b"hvc1"));
        assert_eq!(config.parameter_sets(), [VPS, SPS, PPS]);
    }

    #[test]
    fn decoder_config_reads_the_picture_size_of_the_entry() {
        let config = read_hevc(&init_with_entry(*b"hvc1"));
        assert_eq!((config.width(), config.height()), (W, H));
    }

    #[test]
    fn an_init_segment_converts_to_its_decoder_configuration() {
        let hevc = HevcConfig {
            parameter_sets: vec![VPS.to_vec(), SPS.to_vec(), PPS.to_vec()],
            width: W,
            height: H,
        };
        assert_eq!(
            DecoderConfig::try_from(&init_with_entry(*b"hvc1")[..]),
            Ok(DecoderConfig::Hevc(hevc))
        );
    }

    /// Both entries give HEVC, and the same configuration.
    #[test]
    fn an_hev1_entry_gives_the_configuration_of_its_hvc1_twin() {
        assert_eq!(
            read_hevc(&init_with_entry(*b"hev1")),
            read_hevc(&init_with_entry(*b"hvc1"))
        );
    }

    /// An `hev1` entry whose `hvcC` leaves every parameter set to the
    /// samples gives no configuration: the reader takes the sets from the
    /// record alone.
    #[test]
    fn an_hev1_entry_whose_hvcc_holds_no_parameter_set_is_refused() {
        let in_band_only = visual_entry(*b"hev1", &hvcc(&[]));
        assert_eq!(
            with_text(DecoderConfig::parse(&build_init_segment(&in_band_only))),
            Err((
                DecoderConfigError::Hvcc(HvccError::MissingParameterSet { nal_type: 32 }),
                "reading the HEVC configuration: hvcC is missing parameter-set type 32".into()
            ))
        );
    }

    #[test]
    fn decoder_config_reads_aac() {
        let init = build_init_segment(&mp4a_entry(1, 64_000, &[0x11, 0x08], LengthForm::OneByte));
        assert_eq!(
            DecoderConfig::parse(&init).unwrap(),
            DecoderConfig::Aac(AacConfig {
                audio_specific_config: vec![0x11, 0x08],
                sample_rate: 64_000,
                channels: 1,
            })
        );
    }

    #[test]
    fn a_configuration_of_64_bytes_is_read_whole() {
        let longest = [0x11; 64];
        assert_eq!(
            aac(&longest).map(|config| config.audio_specific_config),
            Ok(longest.to_vec())
        );
    }

    #[test]
    fn an_esds_descriptor_length_padded_to_four_bytes_parses_alike() {
        let one = build_init_segment(&mp4a_entry(1, 48_000, &[0x11, 0x88], LengthForm::OneByte));
        let four = build_init_segment(&mp4a_entry(1, 48_000, &[0x11, 0x88], LengthForm::FourBytes));
        assert_eq!(
            DecoderConfig::parse(&one).unwrap(),
            DecoderConfig::parse(&four).unwrap()
        );
    }

    /// A length past 127 continues into a second group of seven bits: here
    /// an `ES_Descriptor` of 235 bytes, made long by a URL of 200.
    #[test]
    fn a_descriptor_length_past_127_reads_whole() {
        let mut es_fields = vec![0, 1, 0x40, 200];
        es_fields.extend([b'u'; 200]);
        let esds = esds(&es_fields, 0x40, &[0x11, 0x88], LengthForm::FourBytes);
        assert_eq!(esds[4..9], [0x03, 0x80, 0x80, 0x81, 0x6b]);
        assert_eq!(read_aac(&mp4a(1, 48_000, &esds)), aac(&[0x11, 0x88]));
    }

    /// The length 200 in two bytes, high bits first, is `81 48`. Retina
    /// writes it low bits first, `c8 01`, which this reader takes as 9,217.
    #[test]
    fn a_two_byte_descriptor_length_is_read_high_bits_first() {
        let high_first = [&[0x03, 0x81, 0x48][..], &[0xaa; 200]].concat();
        let body = descriptor(&high_first, 0x03, "ES_Descriptor");
        assert_eq!(body.map(<[u8]>::len), Ok(200));
        let low_first = [&[0x03, 0xc8, 0x01][..], &[0xaa; 9_217]].concat();
        let body = descriptor(&low_first, 0x03, "ES_Descriptor");
        assert_eq!(body.map(<[u8]>::len), Ok(9_217));
    }

    /// The fields that the flags of an `ES_Descriptor` announce are stepped over:
    /// a dependency, a URL, and an OCR stream, alone or together, in that
    /// order; the stream priority announces nothing.
    #[test]
    fn es_descriptor_flags_announce_fields_that_are_skipped() {
        let read = |es_fields: &[u8]| {
            let esds = esds(es_fields, 0x40, &[0x11, 0x88], LengthForm::OneByte);
            read_aac(&mp4a(1, 48_000, &esds))
        };
        let plain = read(&ES_FIELDS).unwrap();
        for es_fields in [
            &[0, 1, 0x80, 0, 2][..],            // streamDependenceFlag, dependsOn_ES_ID
            &[0, 1, 0x40, 3, b'u', b'r', b'l'], // URL_Flag, URLlength, URLstring
            &[0, 1, 0x20, 0, 3],                // OCRstreamFlag, OCR_ES_Id
            &[0, 1, 0xe0, 0, 2, 3, b'u', b'r', b'l', 0, 3], // all three
            &[0, 1, 0x1f],                      // streamPriority alone
        ] {
            assert_eq!(read(es_fields), Ok(plain.clone()), "{es_fields:?}");
        }
    }

    #[test]
    fn each_sampling_index_gives_its_rate() {
        let rates = [
            96_000, 88_200, 64_000, 48_000, 44_100, 32_000, 24_000, 22_050, 16_000, 12_000, 11_025,
            8_000, 7_350,
        ];
        for (index, rate) in (0u8..).zip(rates) {
            // AAC-LC, the index, and one channel.
            let bytes = [0x10 | (index >> 1), ((index & 1) << 7) | 0x08];
            assert_eq!(
                aac(&bytes).map(|config| config.sample_rate),
                Ok(rate),
                "index {index}"
            );
        }
    }

    /// Index 15 says the rate follows in 24 bits, here 50,000 Hz.
    #[test]
    fn sampling_index_15_gives_the_rate_that_follows_it() {
        assert_eq!(
            aac(&[0x17, 0x80, 0x61, 0xa8, 0x08]).map(|config| config.sample_rate),
            Ok(50_000)
        );
    }

    #[test]
    fn sampling_index_15_with_a_rate_of_zero_is_refused() {
        assert_eq!(
            aac(&[0x17, 0x80, 0, 0, 0x08]),
            Err(EsdsError::SamplingIndex(15))
        );
    }

    /// A configuration of index 15 cut before the end of its 24-bit rate,
    /// whose last bit lies in the fifth byte.
    #[test]
    fn sampling_index_15_without_its_whole_rate_is_truncated() {
        for cut in [
            &[0x17, 0x80][..],
            &[0x17, 0x80, 0x61],
            &[0x17, 0x80, 0x61, 0xa8],
        ] {
            assert_eq!(
                with_text(aac(cut)),
                Err((EsdsError::Truncated, ESDS_TRUNCATED.into())),
                "{cut:02x?}"
            );
        }
    }

    #[test]
    fn a_reserved_sampling_index_is_refused() {
        assert_eq!(aac(&[0x16, 0x88]), Err(EsdsError::SamplingIndex(13)));
        assert_eq!(aac(&[0x17, 0x08]), Err(EsdsError::SamplingIndex(14)));
        assert_eq!(
            EsdsError::SamplingIndex(13).to_string(),
            "sampling frequency index 13 of the AudioSpecificConfig gives no rate"
        );
    }

    /// The configuration says one channel; the entry's `channelcount` is the
    /// count.
    #[test]
    fn the_channel_count_comes_from_the_entry() {
        let stereo = read_aac(&mp4a_entry(2, 48_000, &[0x11, 0x88], LengthForm::OneByte));
        assert_eq!(stereo.map(|config| config.channels), Ok(2));
    }

    /// The fields of an entry whose header is 16 bytes long start 8 bytes
    /// later in the box than those of an entry whose header is 8 bytes long.
    #[test]
    fn an_mp4a_entry_with_a_64_bit_size_gives_its_channel_count() {
        let stereo = mp4a_entry(2, 48_000, &[0x11, 0x90], LengthForm::OneByte);
        let large = SampleEntry::audio(Bytes::from(with_large_size(&stereo.bytes)), stereo.codecs);
        assert_eq!(read_aac(&large).map(|config| config.channels), Ok(2));
    }

    #[test]
    fn a_channel_count_of_zero_is_refused() {
        assert_eq!(
            read_aac(&mp4a_entry(0, 48_000, &[0x11, 0x88], LengthForm::OneByte)),
            Err(EsdsError::Missing("channelcount"))
        );
        assert_eq!(
            EsdsError::Missing("channelcount").to_string(),
            "missing or malformed channelcount"
        );
    }

    #[test]
    fn an_audio_specific_config_shorter_than_two_bytes_is_refused() {
        assert_eq!(
            with_text(aac(&[])),
            Err((
                EsdsError::ConfigLength(0),
                "an AudioSpecificConfig length of 0 is outside 2 to 64 bytes".into()
            ))
        );
        assert_eq!(
            with_text(aac(&[0x11])),
            Err((
                EsdsError::ConfigLength(1),
                "an AudioSpecificConfig length of 1 is outside 2 to 64 bytes".into()
            ))
        );
    }

    #[test]
    fn an_audio_specific_config_longer_than_64_bytes_is_refused() {
        assert_eq!(
            with_text(aac(&[0x11; 65])),
            Err((
                EsdsError::ConfigLength(65),
                "an AudioSpecificConfig length of 65 is outside 2 to 64 bytes".into()
            ))
        );
    }

    #[test]
    fn the_escape_audio_object_type_is_refused() {
        assert_eq!(aac(&[0xf9, 0x08]), Err(EsdsError::AudioObjectType));
        assert_eq!(
            EsdsError::AudioObjectType.to_string(),
            "the AudioSpecificConfig uses the escape audio object type"
        );
    }

    /// MPEG-1 audio, object type `0x6b`, is not MPEG-4 audio.
    #[test]
    fn an_object_type_other_than_mpeg4_audio_is_refused() {
        assert_eq!(
            with_text(aac_with_object_type(0x6b)),
            Err((
                EsdsError::ObjectType(0x6b),
                "esds object type 0x6b is not MPEG-4 audio".into()
            ))
        );
    }

    /// A length whose fourth byte says a fifth follows.
    #[test]
    fn a_descriptor_length_past_four_bytes_is_refused() {
        let long = [0, 0, 0, 0, 0x03, 0x80, 0x80, 0x80, 0x80, 0];
        assert_eq!(
            with_text(read_aac(&mp4a(1, 48_000, &long))),
            Err((
                EsdsError::DescriptorLength,
                "an esds descriptor length runs past four bytes".into()
            ))
        );
    }

    /// An `esds` cut anywhere is refused as truncated, never a panic. Cut
    /// inside its version and flags, it holds no descriptor; cut later, its
    /// outermost descriptor runs past the box.
    #[test]
    fn an_esds_cut_at_any_length_is_truncated() {
        for form in [LengthForm::OneByte, LengthForm::FourBytes] {
            let entry = esds(&ES_FIELDS, 0x40, &[0x11, 0x88], form);
            for cut in 0..entry.len() {
                assert_eq!(
                    with_text(read_aac(&mp4a(1, 48_000, &entry[..cut]))),
                    Err((EsdsError::Truncated, ESDS_TRUNCATED.into())),
                    "{form:?} cut at {cut}"
                );
            }
        }
    }

    /// An `ES_Descriptor` of fewer than three bytes holds no `ES_ID` and
    /// flags.
    #[test]
    fn an_es_descriptor_without_its_flags_is_truncated() {
        for es in [&[][..], &[0], &[0, 1]] {
            assert_eq!(
                with_text(read_es(es)),
                Err((EsdsError::Truncated, ESDS_TRUNCATED.into())),
                "{es:02x?}"
            );
        }
    }

    /// The dependency, the URL's length, the URL, or the OCR stream that the
    /// flags announce, absent or cut short.
    #[test]
    fn a_field_the_flags_announce_that_is_absent_is_truncated() {
        for es in [
            &[0, 1, 0x80][..],
            &[0, 1, 0x80, 0],
            &[0, 1, 0x40],
            &[0, 1, 0x40, 3, b'u', b'r'],
            &[0, 1, 0x20],
            &[0, 1, 0x20, 0],
        ] {
            assert_eq!(
                with_text(read_es(es)),
                Err((EsdsError::Truncated, ESDS_TRUNCATED.into())),
                "{es:02x?}"
            );
        }
    }

    #[test]
    fn an_es_descriptor_without_a_decoder_config_descriptor_is_truncated() {
        assert_eq!(
            with_text(read_es(&ES_FIELDS)),
            Err((EsdsError::Truncated, ESDS_TRUNCATED.into()))
        );
    }

    /// The 13 bytes of fields of a `DecoderConfigDescriptor` of MPEG-4 audio,
    /// the object type and the rest zero, followed by `specific`: what the
    /// descriptor holds after its fields, its `DecoderSpecificInfo` or a
    /// part of one. The reader reads no descriptor after the
    /// `DecoderSpecificInfo`, so when that descriptor is cut, the refusal
    /// comes from it.
    fn decoder_fields_then(specific: &[u8]) -> Vec<u8> {
        let mut decoder = vec![0; 13];
        decoder[0] = 0x40;
        decoder.extend_from_slice(specific);
        decoder
    }

    /// A `DecoderSpecificInfo` whose tag has no length after it, and one
    /// whose length says another byte follows that is not there.
    #[test]
    fn a_decoder_specific_info_cut_inside_its_length_is_truncated() {
        for specific in [&[0x05][..], &[0x05, 0x80]] {
            assert_eq!(
                with_text(read_es(&es_with_decoder(&decoder_fields_then(specific)))),
                Err((EsdsError::Truncated, ESDS_TRUNCATED.into())),
                "{specific:02x?}"
            );
        }
    }

    /// A `DecoderSpecificInfo` that says it holds 2 bytes and holds one.
    #[test]
    fn a_decoder_specific_info_shorter_than_its_length_is_truncated() {
        assert_eq!(
            with_text(read_es(&es_with_decoder(&decoder_fields_then(&[
                0x05, 2, 0x11
            ])))),
            Err((EsdsError::Truncated, ESDS_TRUNCATED.into()))
        );
    }

    #[test]
    fn a_decoder_config_descriptor_without_a_decoder_specific_info_is_truncated() {
        assert_eq!(
            with_text(read_es(&es_with_decoder(&decoder_fields_then(&[])))),
            Err((EsdsError::Truncated, ESDS_TRUNCATED.into()))
        );
    }

    #[test]
    fn an_empty_decoder_config_descriptor_is_truncated() {
        assert_eq!(
            with_text(read_es(&es_with_decoder(&[]))),
            Err((EsdsError::Truncated, ESDS_TRUNCATED.into()))
        );
    }

    /// A `DecoderConfigDescriptor` of MPEG-4 audio that ends before its 13
    /// bytes of fields.
    #[test]
    fn a_decoder_config_descriptor_cut_inside_its_fields_is_truncated() {
        for len in 1..13 {
            let mut decoder = vec![0; len];
            decoder[0] = 0x40;
            assert_eq!(
                with_text(read_es(&es_with_decoder(&decoder))),
                Err((EsdsError::Truncated, ESDS_TRUNCATED.into())),
                "{len} bytes"
            );
        }
    }

    /// An `mp4a` box of its header alone holds none of the 28 bytes of fixed
    /// fields of an audio sample entry.
    #[test]
    fn an_audio_entry_without_its_fixed_fields_is_refused() {
        let without_fields =
            SampleEntry::audio(Bytes::from_static(b"\0\0\0\x08mp4a"), String::new());
        assert_eq!(
            read_aac(&without_fields),
            Err(EsdsError::Missing("sample entry fields"))
        );
    }

    #[test]
    fn an_audio_entry_without_an_esds_is_refused() {
        let entry = mp4a_entry(1, 48_000, &[0x11, 0x88], LengthForm::OneByte);
        assert_eq!(
            read_aac(&SampleEntry::audio(
                Bytes::from(renamed(&entry.bytes, *b"esds")),
                String::new()
            )),
            Err(EsdsError::Missing("esds"))
        );
    }

    /// Each descriptor on the way to the configuration must carry the tag of
    /// its place, or the refusal names the descriptor.
    #[test]
    fn a_descriptor_out_of_its_place_is_named() {
        let valid = esds(&ES_FIELDS, 0x40, &[0x11, 0x88], LengthForm::OneByte);
        for (at, tag, name) in [
            (4, 0x03, "ES_Descriptor"),
            (9, 0x04, "DecoderConfigDescriptor"),
            (24, 0x05, "DecoderSpecificInfo"),
        ] {
            assert_eq!(valid[at], tag, "{name}");
            let mut displaced = valid.clone();
            displaced[at] = 0x06;
            assert_eq!(
                read_aac(&mp4a(1, 48_000, &displaced)),
                Err(EsdsError::Missing(name))
            );
        }
    }

    #[test]
    fn a_sample_entry_of_another_kind_is_refused() {
        assert_eq!(
            DecoderConfig::parse(&init_with_entry(*b"avc1")),
            Err(DecoderConfigError::Unsupported(*b"avc1"))
        );
    }

    #[test]
    fn an_unsupported_box_type_shows_its_bytes_outside_printable_ascii_escaped() {
        assert_eq!(
            DecoderConfigError::Unsupported(*b"av\x001").to_string(),
            "the sample entry \"av\\x001\" is neither HEVC nor AAC"
        );
    }

    #[test]
    fn an_init_segment_without_a_whole_sample_entry_is_refused() {
        let init = init_with_entry(*b"hvc1");
        for cut in [&b""[..], &init[..init.len() - 1]] {
            assert_eq!(
                with_text(DecoderConfig::parse(cut)),
                Err((
                    DecoderConfigError::NoSampleEntry,
                    "the init segment has no sample entry".into()
                )),
                "{} bytes",
                cut.len()
            );
        }
    }

    #[test]
    fn an_hevc_entry_whose_hvcc_lacks_a_parameter_set_is_refused() {
        let without_pps = visual_entry(*b"hvc1", &hvcc(&required_hvcc_arrays()[..2]));
        assert_eq!(
            with_text(DecoderConfig::parse(&build_init_segment(&without_pps))),
            Err((
                DecoderConfigError::Hvcc(HvccError::MissingParameterSet { nal_type: 34 }),
                "reading the HEVC configuration: hvcC is missing parameter-set type 34".into()
            ))
        );
    }

    /// The width and the height end 28 bytes into the body of a visual
    /// sample entry: an entry with no fields at all, and one cut inside the
    /// height.
    #[test]
    fn an_hevc_entry_without_its_picture_size_is_refused() {
        for len in [0, 27] {
            assert_eq!(
                with_text(DecoderConfig::parse(&init_with_hvc1_fields(&vec![0; len]))),
                Err((
                    DecoderConfigError::Hvcc(HvccError::Missing("sample entry fields")),
                    "reading the HEVC configuration: missing or malformed sample entry fields"
                        .into()
                )),
                "{len} bytes"
            );
        }
    }

    /// The children of a visual sample entry start 78 bytes into its body:
    /// an entry that holds its picture size and ends before them.
    #[test]
    fn an_hevc_entry_cut_before_its_children_gives_no_configuration() {
        for len in [28, 77] {
            assert_eq!(
                with_text(DecoderConfig::parse(&init_with_hvc1_fields(&vec![0; len]))),
                Err((
                    DecoderConfigError::Hvcc(HvccError::Missing("sample entry fields")),
                    "reading the HEVC configuration: missing or malformed sample entry fields"
                        .into()
                )),
                "{len} bytes"
            );
        }
    }

    /// A visual sample entry that holds its picture size and ends before its
    /// children, which start 78 bytes into its body.
    #[test]
    fn an_hevc_entry_cut_before_its_children_gives_no_parameter_sets() {
        for len in [28, 77] {
            assert_eq!(
                parse_parameter_sets(&init_with_stsd(&stsd_of_hvc1(&vec![0; len]))),
                Err(HvccError::Missing("sample entry fields")),
                "{len} bytes"
            );
        }
    }

    #[test]
    fn parameter_sets_come_from_hvcc() {
        let init = init_with_entry(*b"hvc1");
        let sets = parse_parameter_sets(&init).unwrap();
        assert_eq!(sets, vec![VPS.to_vec(), SPS.to_vec(), PPS.to_vec()]);
    }

    #[test]
    fn parameter_sets_are_read_from_an_hev1_entry() {
        assert_eq!(
            parse_parameter_sets(&init_with_entry(*b"hev1")),
            Ok(vec![VPS.to_vec(), SPS.to_vec(), PPS.to_vec()])
        );
    }

    #[test]
    fn another_codecs_sample_entry_is_refused_by_name() {
        assert_eq!(
            parse_parameter_sets(&init_with_entry(*b"avc1")),
            Err(HvccError::NotHevc(*b"avc1"))
        );
    }

    #[test]
    fn a_box_type_that_is_not_hevc_shows_its_bytes_outside_printable_ascii_escaped() {
        assert_eq!(
            HvccError::NotHevc(*b"av\x001").to_string(),
            "the sample entry \"av\\x001\" is not HEVC"
        );
    }

    /// Each box on the path to the sample entry, absent, is named in the
    /// refusal.
    #[test]
    fn a_box_missing_from_the_path_to_the_sample_entry_is_named() {
        let init = init_with_entry(*b"hvc1");
        for (kind, name) in [
            (b"moov", "moov"),
            (b"trak", "trak"),
            (b"mdia", "mdia"),
            (b"minf", "minf"),
            (b"stbl", "stbl"),
            (b"stsd", "stsd"),
        ] {
            assert_eq!(
                parse_parameter_sets(&renamed(&init, *kind)),
                Err(HvccError::Missing(name)),
                "{name}"
            );
        }
    }

    /// An `stsd` of fewer than eight bytes holds no version, flags, and
    /// entry count.
    #[test]
    fn an_stsd_without_its_entry_count_is_refused() {
        assert_eq!(
            parse_parameter_sets(&init_with_stsd(&[0, 0, 0, 0, 0, 0, 0])),
            Err(HvccError::Missing("stsd entries"))
        );
        assert_eq!(
            HvccError::Missing("stsd entries").to_string(),
            "missing or malformed stsd entries"
        );
    }

    /// An `stsd` that holds no entry after its count, and one whose entry
    /// claims more bytes than the `stsd` holds.
    #[test]
    fn an_stsd_without_a_whole_sample_entry_is_refused() {
        let mut claims_more = vec![0, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0, 100];
        claims_more.extend_from_slice(b"hvc1");
        for stsd in [&[0, 0, 0, 0, 0, 0, 0, 1][..], &claims_more] {
            assert_eq!(
                parse_parameter_sets(&init_with_stsd(stsd)),
                Err(HvccError::Missing("sample entry")),
                "{stsd:02x?}"
            );
        }
    }

    #[test]
    fn an_hevc_entry_without_an_hvcc_gives_no_parameter_sets() {
        let init = renamed(&init_with_entry(*b"hvc1"), *b"hvcC");
        assert_eq!(parse_parameter_sets(&init), Err(HvccError::Missing("hvcC")));
    }

    #[test]
    fn an_hevc_entry_without_an_hvcc_gives_no_configuration() {
        let init = renamed(&init_with_entry(*b"hvc1"), *b"hvcC");
        assert_eq!(
            with_text(DecoderConfig::parse(&init)),
            Err((
                DecoderConfigError::Hvcc(HvccError::Missing("hvcC")),
                "reading the HEVC configuration: missing or malformed hvcC".into()
            ))
        );
    }

    /// A box of size 0 runs to the end of its parent. On the way to the
    /// `hvcC`, each such box is refused by name.
    #[test]
    fn a_box_of_size_zero_on_the_path_to_the_hvcc_gives_no_parameter_sets() {
        let init = init_with_entry(*b"hvc1");
        for (kind, name) in [
            (b"moov", "moov"),
            (b"trak", "trak"),
            (b"mdia", "mdia"),
            (b"minf", "minf"),
            (b"stbl", "stbl"),
            (b"stsd", "stsd"),
            (b"hvc1", "sample entry"),
            (b"hvcC", "hvcC"),
        ] {
            assert_eq!(
                parse_parameter_sets(&open_ended(&init, *kind)),
                Err(HvccError::Missing(name)),
                "{name}"
            );
        }
    }

    /// A box of size 0 on the path to the sample entry leaves no whole
    /// sample entry.
    #[test]
    fn a_box_of_size_zero_on_the_path_to_the_sample_entry_gives_no_configuration() {
        let init = init_with_entry(*b"hvc1");
        for kind in [
            b"moov", b"trak", b"mdia", b"minf", b"stbl", b"stsd", b"hvc1",
        ] {
            assert_eq!(
                with_text(DecoderConfig::parse(&open_ended(&init, *kind))),
                Err((
                    DecoderConfigError::NoSampleEntry,
                    "the init segment has no sample entry".into()
                )),
                "{}",
                kind.escape_ascii()
            );
        }
    }

    #[test]
    fn an_hvcc_of_size_zero_gives_no_configuration() {
        let init = open_ended(&init_with_entry(*b"hvc1"), *b"hvcC");
        assert_eq!(
            with_text(DecoderConfig::parse(&init)),
            Err((
                DecoderConfigError::Hvcc(HvccError::Missing("hvcC")),
                "reading the HEVC configuration: missing or malformed hvcC".into()
            ))
        );
    }

    #[test]
    fn an_esds_of_size_zero_gives_no_configuration() {
        let entry = mp4a_entry(1, 48_000, &[0x11, 0x88], LengthForm::OneByte);
        let init = open_ended(&build_init_segment(&entry), *b"esds");
        assert_eq!(
            with_text(DecoderConfig::parse(&init)),
            Err((
                DecoderConfigError::Esds(EsdsError::Missing("esds")),
                "reading the AAC configuration: missing or malformed esds".into()
            ))
        );
    }

    #[test]
    fn an_empty_box_before_the_moov_is_stepped_over() {
        let mut led = b"\0\0\0\x08free".to_vec();
        led.extend_from_slice(&init_with_entry(*b"hvc1"));
        assert_eq!(
            parse_parameter_sets(&led),
            Ok(vec![VPS.to_vec(), SPS.to_vec(), PPS.to_vec()])
        );
    }

    /// The children of a `moov` whose header is 16 bytes long start after
    /// its `largesize`.
    #[test]
    fn a_moov_with_a_64_bit_size_gives_its_parameter_sets() {
        let init = init_with_entry(*b"hvc1");
        // The `moov` follows the `ftyp` and ends the init segment.
        let moov = init
            .windows(4)
            .position(|window| window == b"moov")
            .unwrap()
            - 4;
        let large = [&init[..moov], &with_large_size(&init[moov..])].concat();
        assert_eq!(
            parse_parameter_sets(&large),
            Ok(vec![VPS.to_vec(), SPS.to_vec(), PPS.to_vec()])
        );
    }

    /// The `moov` cut by a byte claims one more than is left, which ends
    /// the search for it.
    #[test]
    fn a_box_that_claims_more_bytes_than_are_left_ends_the_search() {
        let init = init_with_entry(*b"hvc1");
        assert_eq!(
            parse_parameter_sets(&init[..init.len() - 1]),
            Err(HvccError::Missing("moov"))
        );
    }

    #[test]
    fn a_valid_hvcc_yields_its_three_parameter_sets() {
        assert_eq!(parse_hvcc(&hvcc(&required_hvcc_arrays())).unwrap().len(), 3);
    }

    /// A record whose first byte is not 1, and a record of no byte.
    #[test]
    fn hvcc_requires_version_one() {
        let mut wrong_version = hvcc(&required_hvcc_arrays());
        wrong_version[0] = 0;
        for record in [&wrong_version[..], &[]] {
            assert_eq!(
                with_text(parse_hvcc(record)),
                Err((
                    HvccError::Version,
                    "unsupported hvcC configuration version".into()
                )),
                "{} bytes",
                record.len()
            );
        }
    }

    #[test]
    fn hvcc_requires_four_byte_lengths() {
        for length_size_minus_one in 0..3 {
            let mut wrong_length = hvcc(&required_hvcc_arrays());
            wrong_length[21] = (wrong_length[21] & !3) | length_size_minus_one;
            assert_eq!(
                with_text(parse_hvcc(&wrong_length)),
                Err((
                    HvccError::LengthSize,
                    "hvcC does not use four-byte NAL length prefixes".into()
                )),
                "{length_size_minus_one}"
            );
        }
    }

    /// A record cut anywhere after its version byte is refused as
    /// truncated: before its length size, before its count of arrays, and
    /// inside an array's header, a NAL unit's length, or a NAL unit.
    #[test]
    fn an_hvcc_cut_at_any_length_past_its_version_is_truncated() {
        let whole = hvcc(&required_hvcc_arrays());
        for cut in 1..whole.len() {
            assert_eq!(
                with_text(parse_hvcc(&whole[..cut])),
                Err((HvccError::Truncated, "hvcC is truncated".into())),
                "cut at {cut}"
            );
        }
    }

    #[test]
    fn hvcc_requires_each_parameter_set_type() {
        for (missing, text) in [
            (32, "hvcC is missing parameter-set type 32"),
            (33, "hvcC is missing parameter-set type 33"),
            (34, "hvcC is missing parameter-set type 34"),
        ] {
            let arrays: Vec<_> = required_hvcc_arrays()
                .into_iter()
                .filter(|(kind, _)| *kind != missing)
                .collect();
            assert_eq!(
                with_text(parse_hvcc(&hvcc(&arrays))),
                Err((
                    HvccError::MissingParameterSet { nal_type: missing },
                    text.into()
                ))
            );
        }
    }

    #[test]
    fn hvcc_keeps_a_repeated_set() {
        let mut arrays = required_hvcc_arrays();
        arrays.push((33, vec![SPS.to_vec()]));
        assert_eq!(
            parse_hvcc(&hvcc(&arrays)),
            Ok(vec![VPS.to_vec(), SPS.to_vec(), PPS.to_vec(), SPS.to_vec()])
        );
    }

    /// An array of SEI messages, NAL unit type 39, is read and left out.
    #[test]
    fn hvcc_passes_over_an_array_of_another_type() {
        let mut arrays = required_hvcc_arrays();
        arrays.insert(1, (39, vec![vec![39 << 1, 1, 0xaa]]));
        assert_eq!(
            parse_hvcc(&hvcc(&arrays)),
            Ok(vec![VPS.to_vec(), SPS.to_vec(), PPS.to_vec()])
        );
    }

    #[test]
    fn a_nal_unit_of_another_type_than_its_array_is_refused() {
        let mut mismatch = required_hvcc_arrays();
        mismatch[0] = (32, vec![SPS.to_vec()]);
        assert_eq!(
            with_text(parse_hvcc(&hvcc(&mismatch))),
            Err((
                HvccError::TypeMismatch {
                    array_type: 32,
                    nal_type: 33
                },
                "hvcC array type 32 contains NAL unit type 33".into()
            ))
        );
    }

    /// An array of SEI messages, NAL unit type 39, that holds an SPS.
    #[test]
    fn an_sps_in_an_array_of_sei_messages_is_refused() {
        let mut arrays = required_hvcc_arrays();
        arrays.insert(1, (39, vec![SPS.to_vec()]));
        assert_eq!(
            with_text(parse_hvcc(&hvcc(&arrays))),
            Err((
                HvccError::TypeMismatch {
                    array_type: 39,
                    nal_type: 33
                },
                "hvcC array type 39 contains NAL unit type 33".into()
            ))
        );
    }

    #[test]
    fn a_nal_unit_shorter_than_its_header_is_refused() {
        for short in [vec![], vec![32 << 1]] {
            let mut arrays = required_hvcc_arrays();
            arrays[0] = (32, vec![short.clone()]);
            assert_eq!(
                with_text(parse_hvcc(&hvcc(&arrays))),
                Err((
                    HvccError::ShortNal,
                    "hvcC NAL unit is shorter than its two-byte header".into()
                )),
                "{short:02x?}"
            );
        }
    }

    #[test]
    fn a_nal_unit_of_its_two_byte_header_alone_is_read() {
        let mut arrays = required_hvcc_arrays();
        arrays[0] = (32, vec![vec![32 << 1, 1]]);
        assert_eq!(parse_hvcc(&hvcc(&arrays)).unwrap()[0], [32 << 1, 1]);
    }

    #[test]
    fn an_array_with_its_reserved_bit_set_is_refused() {
        let mut reserved = hvcc(&required_hvcc_arrays());
        reserved[23] |= 0x40;
        assert_eq!(
            with_text(parse_hvcc(&reserved)),
            Err((HvccError::Reserved, "hvcC array reserved bit is set".into()))
        );
    }

    #[test]
    fn bytes_after_the_last_array_are_refused() {
        let mut trailing = hvcc(&required_hvcc_arrays());
        trailing.push(0);
        assert_eq!(
            with_text(parse_hvcc(&trailing)),
            Err((HvccError::Trailing, "hvcC has trailing bytes".into()))
        );
    }

    /// The arrays of a record that holds `vps` VPSs, `sps` SPSs, and `pps`
    /// PPSs, one array of each type.
    fn arrays_of(vps: usize, sps: usize, pps: usize) -> Vec<(u8, Vec<Vec<u8>>)> {
        vec![
            (32, vec![VPS.to_vec(); vps]),
            (33, vec![SPS.to_vec(); sps]),
            (34, vec![PPS.to_vec(); pps]),
        ]
    }

    /// A record may hold as many sets of a type as ITU-T H.265 defines
    /// identifiers for, and they are read whole and in order.
    #[test]
    fn hvcc_holds_the_most_parameter_sets_of_each_type() {
        let mut most = vec![VPS.to_vec(); 16];
        most.extend(vec![SPS.to_vec(); 16]);
        most.extend(vec![PPS.to_vec(); 64]);
        assert_eq!(parse_hvcc(&hvcc(&arrays_of(16, 16, 64))), Ok(most));
    }

    /// One set past the bound of its type is refused while the other types
    /// hold their most, and the refusal names the type and its bound.
    #[test]
    fn hvcc_refuses_a_parameter_set_past_the_bound_of_its_type() {
        for (arrays, nal_type, max, text) in [
            (
                arrays_of(17, 16, 64),
                32,
                16,
                "hvcC holds more than 16 parameter sets of type 32",
            ),
            (
                arrays_of(16, 17, 64),
                33,
                16,
                "hvcC holds more than 16 parameter sets of type 33",
            ),
            (
                arrays_of(16, 16, 65),
                34,
                64,
                "hvcC holds more than 64 parameter sets of type 34",
            ),
        ] {
            let refused = parse_hvcc(&hvcc(&arrays)).unwrap_err();
            assert_eq!(
                refused,
                HvccError::TooManyParameterSets { nal_type, max },
                "{text}"
            );
            assert_eq!(refused.to_string(), text);
        }
    }

    /// Sixteen SPSs, ten in one array and six in another, are read.
    #[test]
    fn the_most_sets_of_a_type_are_read_across_arrays() {
        let mut arrays = arrays_of(1, 10, 1);
        arrays.push((33, vec![SPS.to_vec(); 6]));
        assert_eq!(parse_hvcc(&hvcc(&arrays)).map(|sets| sets.len()), Ok(18));
    }

    /// The bound counts the sets of a type across all the arrays of the
    /// record: a seventeenth SPS in a third array is refused.
    #[test]
    fn a_set_past_the_bound_across_arrays_is_refused() {
        let mut arrays = arrays_of(1, 10, 1);
        arrays.push((33, vec![SPS.to_vec(); 6]));
        arrays.push((33, vec![SPS.to_vec()]));
        assert_eq!(
            parse_hvcc(&hvcc(&arrays)),
            Err(HvccError::TooManyParameterSets {
                nal_type: 33,
                max: 16
            })
        );
    }

    /// The parser stops at the first set past the bound: an array that
    /// announces 65,535 VPSs and holds 17 is refused at the seventeenth,
    /// and the refusal returns at once. A loop that read on would look for
    /// an eighteenth and refuse the record as truncated instead, so the
    /// refusal it gives shows where it stopped.
    #[test]
    fn the_parser_stops_at_the_first_set_past_the_bound() {
        let mut flood = hvcc(&[(32, vec![VPS.to_vec(); 17])]);
        // The array's NAL unit count follows its one-byte header.
        assert_eq!(flood[24..26], [0, 17]);
        flood[24..26].copy_from_slice(&[0xff, 0xff]);
        assert_eq!(
            parse_hvcc(&flood),
            Err(HvccError::TooManyParameterSets {
                nal_type: 32,
                max: 16
            })
        );
    }
}
