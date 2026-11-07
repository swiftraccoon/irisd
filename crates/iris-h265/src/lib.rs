//! H.265 (HEVC) NAL unit helpers for a recorder that never decodes.
//!
//! irisd only needs to classify NAL units, walk length-prefixed access units,
//! and strip parameter sets out of samples so that files use the `hvc1` sample
//! entry (parameter sets live in `hvcC`). Nothing here parses a bitstream past
//! the two-byte NAL header, and nothing here panics on arbitrary bytes: the
//! inputs come from cameras and the crate has a fuzz target.
//!
//! # Items
//!
//! - [`NalType`] and [`nal_type`]: the NAL unit types irisd distinguishes, with
//!   [`NalType::is_irap`], [`NalType::is_vcl`], and [`NalType::is_parameter_set`].
//! - [`LengthPrefixedNals`]: an iterator over the units of a 4-byte
//!   length-prefixed access unit that yields a [`FramingError`] instead of
//!   reading past the end.
//! - [`inspect_access_unit`] and [`AccessUnitInfo`]: NAL count, random-access
//!   point, and in-band parameter sets of one access unit.
//! - [`strip_parameter_sets`]: the access unit without VPS, SPS, and PPS.
//!
//! # Framing
//!
//! Access units are 4-byte big-endian length-prefixed NAL units, the `hvc1`
//! sample format; Annex B start codes never reach this crate. A NAL unit is at
//! least its two header bytes.
//!
//! ```
//! use iris_h265::{NalType, inspect_access_unit, nal_type, strip_parameter_sets};
//!
//! // First byte of an IDR_W_RADL NAL header: type 19 in bits 1..7.
//! assert_eq!(nal_type(19 << 1), NalType::IdrWRadl);
//! assert!(NalType::IdrWRadl.is_irap());
//!
//! // A VPS (type 32) followed by that IDR slice, length-prefixed.
//! let au = [0, 0, 0, 2, 32 << 1, 1, 0, 0, 0, 3, 19 << 1, 1, 0xaa];
//! let info = inspect_access_unit(&au)?;
//! assert!(info.is_irap && info.has_parameter_sets);
//! assert_eq!(strip_parameter_sets(&au)?.unwrap(), [0, 0, 0, 3, 19 << 1, 1, 0xaa]);
//! # Ok::<(), iris_h265::FramingError>(())
//! ```

use thiserror::Error;

/// NAL unit types from ITU-T H.265 Table 7-1 that irisd distinguishes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum NalType {
    /// Non-reference trailing picture (type 0).
    TrailN,
    /// Reference trailing picture (type 1).
    TrailR,
    /// Other VCL NAL in 2..=15 or 24..=31 (TSA, STSA, RADL, RASL, reserved).
    OtherVcl(u8),
    /// Broken link access picture (types 16, 17, 18).
    Bla(u8),
    /// Instantaneous decoding refresh with leading pictures (type 19).
    IdrWRadl,
    /// Instantaneous decoding refresh without leading pictures (type 20).
    IdrNLp,
    /// Clean random access picture (type 21).
    Cra,
    /// Reserved IRAP types 22 and 23.
    ReservedIrap(u8),
    /// Video parameter set (type 32).
    Vps,
    /// Sequence parameter set (type 33).
    Sps,
    /// Picture parameter set (type 34).
    Pps,
    /// Access unit delimiter (type 35).
    Aud,
    /// Prefix SEI (type 39).
    SeiPrefix,
    /// Suffix SEI (type 40).
    SeiSuffix,
    /// Any other non-VCL type.
    Other(u8),
}

impl NalType {
    /// Returns true for intra random access point pictures (types 16..=23).
    #[must_use]
    pub const fn is_irap(self) -> bool {
        matches!(
            self,
            Self::Bla(_) | Self::IdrWRadl | Self::IdrNLp | Self::Cra | Self::ReservedIrap(_)
        )
    }

    /// Returns true for VPS, SPS, and PPS.
    #[must_use]
    pub const fn is_parameter_set(self) -> bool {
        matches!(self, Self::Vps | Self::Sps | Self::Pps)
    }

    /// Returns true for VCL (slice-carrying) NAL units, types 0..=31.
    #[must_use]
    pub const fn is_vcl(self) -> bool {
        matches!(
            self,
            Self::TrailN
                | Self::TrailR
                | Self::OtherVcl(_)
                | Self::Bla(_)
                | Self::IdrWRadl
                | Self::IdrNLp
                | Self::Cra
                | Self::ReservedIrap(_)
        )
    }

    /// Returns the numeric type from the bitstream.
    #[must_use]
    pub const fn code(self) -> u8 {
        match self {
            Self::TrailN => 0,
            Self::TrailR => 1,
            Self::OtherVcl(t) | Self::Bla(t) | Self::ReservedIrap(t) | Self::Other(t) => t,
            Self::IdrWRadl => 19,
            Self::IdrNLp => 20,
            Self::Cra => 21,
            Self::Vps => 32,
            Self::Sps => 33,
            Self::Pps => 34,
            Self::Aud => 35,
            Self::SeiPrefix => 39,
            Self::SeiSuffix => 40,
        }
    }
}

/// Classifies a NAL unit from the first byte of its two-byte header.
#[must_use]
pub const fn nal_type(header_byte0: u8) -> NalType {
    match (header_byte0 >> 1) & 0x3f {
        0 => NalType::TrailN,
        1 => NalType::TrailR,
        t @ (2..=15 | 24..=31) => NalType::OtherVcl(t),
        t @ 16..=18 => NalType::Bla(t),
        19 => NalType::IdrWRadl,
        20 => NalType::IdrNLp,
        21 => NalType::Cra,
        t @ 22..=23 => NalType::ReservedIrap(t),
        32 => NalType::Vps,
        33 => NalType::Sps,
        34 => NalType::Pps,
        35 => NalType::Aud,
        39 => NalType::SeiPrefix,
        40 => NalType::SeiSuffix,
        t => NalType::Other(t),
    }
}

/// Error from walking a length-prefixed access unit.
#[derive(Clone, Debug, PartialEq, Eq, Error)]
pub enum FramingError {
    /// A length prefix points past the end of the buffer.
    #[error("NAL length {len} at offset {offset} exceeds remaining {remaining} bytes")]
    Truncated {
        /// Declared NAL length, or zero for an incomplete prefix.
        len: usize,
        /// Offset of the length prefix.
        offset: usize,
        /// Bytes left after a complete prefix, or bytes of an incomplete prefix.
        remaining: usize,
    },
    /// A NAL unit shorter than its two-byte header.
    #[error("NAL at offset {0} is shorter than its header")]
    TooShort(usize),
}

/// Iterates over the NAL units of an access unit framed with 4-byte
/// big-endian length prefixes (the `hvc1` sample format and retina's default).
#[derive(Clone, Debug)]
pub struct LengthPrefixedNals<'a> {
    data: &'a [u8],
    offset: usize,
}

impl<'a> LengthPrefixedNals<'a> {
    /// Creates an iterator over `data`.
    #[must_use]
    pub const fn new(data: &'a [u8]) -> Self {
        Self { data, offset: 0 }
    }
}

impl<'a> Iterator for LengthPrefixedNals<'a> {
    type Item = Result<&'a [u8], FramingError>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.offset >= self.data.len() {
            return None;
        }
        let remaining = self.data.len() - self.offset;
        if remaining < 4 {
            let offset = self.offset;
            self.offset = self.data.len();
            return Some(Err(FramingError::Truncated {
                len: 0,
                offset,
                remaining,
            }));
        }
        let prefix = &self.data[self.offset..self.offset + 4];
        let len = u32::from_be_bytes([prefix[0], prefix[1], prefix[2], prefix[3]]) as usize;
        let start = self.offset + 4;
        if len > self.data.len() - start {
            let offset = self.offset;
            self.offset = self.data.len();
            return Some(Err(FramingError::Truncated {
                len,
                offset,
                remaining: remaining - 4,
            }));
        }
        if len < 2 {
            let offset = self.offset;
            self.offset = self.data.len();
            return Some(Err(FramingError::TooShort(offset)));
        }
        self.offset = start + len;
        Some(Ok(&self.data[start..start + len]))
    }
}

/// Summary of an access unit's NAL units.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AccessUnitInfo {
    /// Number of NAL units.
    pub nal_count: usize,
    /// Whether any VCL NAL is an IRAP picture.
    pub is_irap: bool,
    /// Whether the unit carries VPS, SPS, or PPS in-band.
    pub has_parameter_sets: bool,
    /// Bytes occupied by in-band parameter sets, including their prefixes.
    pub parameter_set_bytes: usize,
}

/// Inspects a length-prefixed access unit.
pub fn inspect_access_unit(data: &[u8]) -> Result<AccessUnitInfo, FramingError> {
    let mut info = AccessUnitInfo::default();
    for nal in LengthPrefixedNals::new(data) {
        let nal = nal?;
        let kind = nal_type(nal[0]);
        info.nal_count += 1;
        info.is_irap |= kind.is_irap();
        if kind.is_parameter_set() {
            info.has_parameter_sets = true;
            info.parameter_set_bytes += 4 + nal.len();
        }
    }
    Ok(info)
}

/// Returns a copy of the access unit without VPS/SPS/PPS NAL units, or `None`
/// when there is nothing to strip (so callers can keep the original buffer).
pub fn strip_parameter_sets(data: &[u8]) -> Result<Option<Vec<u8>>, FramingError> {
    let info = inspect_access_unit(data)?;
    if !info.has_parameter_sets {
        return Ok(None);
    }
    let mut out = Vec::with_capacity(data.len() - info.parameter_set_bytes);
    for nal in LengthPrefixedNals::new(data) {
        let nal = nal?;
        if nal_type(nal[0]).is_parameter_set() {
            continue;
        }
        out.extend_from_slice(&(nal.len() as u32).to_be_bytes());
        out.extend_from_slice(nal);
    }
    Ok(Some(out))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nal(kind: u8, payload: &[u8]) -> Vec<u8> {
        let mut v = Vec::new();
        v.extend_from_slice(&((payload.len() + 2) as u32).to_be_bytes());
        v.push(kind << 1);
        v.push(1);
        v.extend_from_slice(payload);
        v
    }

    #[test]
    fn classifies_types() {
        assert_eq!(nal_type(32 << 1), NalType::Vps);
        assert_eq!(nal_type(1 << 1 | 1), NalType::TrailR);
        assert!(nal_type(21 << 1).is_irap());
        assert!(!nal_type(1 << 1).is_irap());
        assert_eq!(NalType::Cra.code(), 21);
    }

    #[test]
    fn classifies_every_first_header_byte() {
        for byte in 0..=u8::MAX {
            let code = (byte >> 1) & 0x3f;
            let kind = nal_type(byte);
            assert_eq!(kind.code(), code, "header byte {byte:#04x}");
            assert_eq!(kind.is_vcl(), code < 32, "NAL type {code}");
            assert_eq!(kind.is_irap(), (16..=23).contains(&code), "NAL type {code}");
            assert_eq!(
                kind.is_parameter_set(),
                (32..=34).contains(&code),
                "NAL type {code}"
            );
        }
    }

    #[test]
    fn partial_prefix_errors_point_to_the_prefix_start() {
        for prefix in [Vec::new(), nal(19, b"")] {
            for remaining in 1..=3 {
                let mut data = prefix.clone();
                data.extend_from_slice(&[0xff; 3][..remaining]);
                let expected = FramingError::Truncated {
                    len: 0,
                    offset: prefix.len(),
                    remaining,
                };
                let mut walk = LengthPrefixedNals::new(&data);
                if !prefix.is_empty() {
                    assert_eq!(walk.next(), Some(Ok(&prefix[4..])));
                }
                assert_eq!(walk.next(), Some(Err(expected.clone())));
                assert_eq!(walk.next(), None);
                assert_eq!(walk.next(), None);
                assert_eq!(inspect_access_unit(&data), Err(expected.clone()));
                assert_eq!(strip_parameter_sets(&data), Err(expected));
            }
        }
    }

    #[test]
    fn complete_prefix_errors_keep_their_fields_and_terminate() {
        for (data, expected) in [
            (
                vec![0, 0, 0, 4, 2, 1],
                FramingError::Truncated {
                    len: 4,
                    offset: 0,
                    remaining: 2,
                },
            ),
            (vec![0, 0, 0, 0], FramingError::TooShort(0)),
            (vec![0, 0, 0, 1, 2], FramingError::TooShort(0)),
        ] {
            let mut walk = LengthPrefixedNals::new(&data);
            assert_eq!(walk.next(), Some(Err(expected.clone())));
            assert_eq!(walk.next(), None);
            assert_eq!(walk.next(), None);
            assert_eq!(inspect_access_unit(&data), Err(expected.clone()));
            assert_eq!(strip_parameter_sets(&data), Err(expected));
        }
    }

    #[test]
    fn strips_parameter_sets_and_keeps_slices() {
        let mut au = nal(32, b"v");
        au.extend(nal(33, b"s"));
        au.extend(nal(34, b"p"));
        au.extend(nal(19, b"idr-slice"));
        let info = inspect_access_unit(&au).unwrap();
        assert_eq!(info.nal_count, 4);
        assert!(info.is_irap && info.has_parameter_sets);
        let stripped = strip_parameter_sets(&au).unwrap().unwrap();
        assert_eq!(stripped, nal(19, b"idr-slice"));
        assert!(strip_parameter_sets(&stripped).unwrap().is_none());
    }

    #[test]
    fn reports_truncation() {
        let mut au = nal(1, b"abc");
        au.truncate(au.len() - 1);
        assert!(matches!(
            inspect_access_unit(&au),
            Err(FramingError::Truncated { .. })
        ));
    }
}
