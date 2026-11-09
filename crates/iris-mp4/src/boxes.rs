//! Minimal ISO BMFF box serialization and header parsing.

use bytes::Bytes;

/// A growable buffer that writes nested boxes and patches their sizes.
#[derive(Debug, Default)]
pub struct BoxWriter {
    buf: Vec<u8>,
}

impl BoxWriter {
    /// Creates a writer with the given initial capacity.
    #[must_use]
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            buf: Vec::with_capacity(capacity),
        }
    }

    /// Starts a box; returns the position to pass to [`BoxWriter::end`]. A
    /// box never ended keeps a zero size, which every reader takes as "to
    /// the end of the file".
    #[must_use = "pass the position to BoxWriter::end"]
    pub fn begin(&mut self, kind: &[u8; 4]) -> usize {
        let start = self.buf.len();
        self.buf.extend_from_slice(&[0, 0, 0, 0]);
        self.buf.extend_from_slice(kind);
        start
    }

    /// Starts a full box (version and 24-bit flags); returns the position to
    /// pass to [`BoxWriter::end`].
    #[must_use = "pass the position to BoxWriter::end"]
    pub fn begin_full(&mut self, kind: &[u8; 4], version: u8, flags: u32) -> usize {
        let start = self.begin(kind);
        self.u8(version);
        self.buf.extend_from_slice(&flags.to_be_bytes()[1..]);
        start
    }

    /// Finishes the box started at `start` by patching its size.
    ///
    /// # Panics
    ///
    /// Panics if the box exceeds `u32::MAX` bytes, which no irisd box approaches.
    pub fn end(&mut self, start: usize) {
        let size = u32::try_from(self.buf.len() - start).expect("box larger than 4 GiB");
        self.buf[start..start + 4].copy_from_slice(&size.to_be_bytes());
    }

    /// Appends one byte.
    pub fn u8(&mut self, v: u8) {
        self.buf.push(v);
    }

    /// Appends a big-endian `u16`.
    pub fn u16(&mut self, v: u16) {
        self.buf.extend_from_slice(&v.to_be_bytes());
    }

    /// Appends a big-endian `i16`.
    pub fn i16(&mut self, v: i16) {
        self.buf.extend_from_slice(&v.to_be_bytes());
    }

    /// Appends a big-endian `u32`.
    pub fn u32(&mut self, v: u32) {
        self.buf.extend_from_slice(&v.to_be_bytes());
    }

    /// Appends a big-endian `i32`.
    pub fn i32(&mut self, v: i32) {
        self.buf.extend_from_slice(&v.to_be_bytes());
    }

    /// Appends a big-endian `u64`.
    pub fn u64(&mut self, v: u64) {
        self.buf.extend_from_slice(&v.to_be_bytes());
    }

    /// Appends raw bytes.
    pub fn bytes(&mut self, v: &[u8]) {
        self.buf.extend_from_slice(v);
    }

    /// Appends `n` zero bytes.
    pub fn zeros(&mut self, n: usize) {
        self.buf.resize(self.buf.len() + n, 0);
    }

    /// Current length in bytes.
    #[must_use]
    pub fn len(&self) -> usize {
        self.buf.len()
    }

    /// Whether nothing has been written.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    /// Consumes the writer.
    #[must_use]
    pub fn into_bytes(self) -> Bytes {
        Bytes::from(self.buf)
    }

    /// Consumes the writer into a `Vec`.
    #[must_use]
    pub fn into_vec(self) -> Vec<u8> {
        self.buf
    }
}

/// A parsed box header.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BoxHeader {
    /// Four-character type.
    pub kind: [u8; 4],
    /// Total box size including the header; `None` means "to end of file".
    pub size: Option<u64>,
    /// Header length: 8, or 16 with a 64-bit `largesize`.
    pub header_len: u8,
}

impl BoxHeader {
    /// Parses a header from the start of `buf`, which needs at least 8 bytes
    /// (16 for a large box). Returns `None` when `buf` is too short or the
    /// size field is smaller than the header.
    #[must_use]
    pub fn parse(buf: &[u8]) -> Option<Self> {
        if buf.len() < 8 {
            return None;
        }
        let size32 = u32::from_be_bytes([buf[0], buf[1], buf[2], buf[3]]);
        let kind = [buf[4], buf[5], buf[6], buf[7]];
        match size32 {
            0 => Some(Self {
                kind,
                size: None,
                header_len: 8,
            }),
            1 => {
                let large = u64::from_be_bytes(*buf.get(8..)?.first_chunk::<8>()?);
                (large >= 16).then_some(Self {
                    kind,
                    size: Some(large),
                    header_len: 16,
                })
            }
            s if s >= 8 => Some(Self {
                kind,
                size: Some(u64::from(s)),
                header_len: 8,
            }),
            _ => None,
        }
    }

    /// Returns the payload length when the size is known and covers the
    /// header; `None` for an open-ended box or a header built by hand whose
    /// size is smaller than its header.
    #[must_use]
    pub fn payload_len(&self) -> Option<u64> {
        self.size?.checked_sub(u64::from(self.header_len))
    }
}

/// Reads a big-endian `u32` at `offset`; `None` when the field does not fit.
#[must_use]
pub fn be_u32(buf: &[u8], offset: usize) -> Option<u32> {
    Some(u32::from_be_bytes(*buf.get(offset..)?.first_chunk::<4>()?))
}

/// Reads a big-endian `u64` at `offset`; `None` when the field does not fit.
#[must_use]
pub fn be_u64(buf: &[u8], offset: usize) -> Option<u64> {
    Some(u64::from_be_bytes(*buf.get(offset..)?.first_chunk::<8>()?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nests_and_patches_sizes() {
        let mut w = BoxWriter::default();
        let outer = w.begin(b"moov");
        let inner = w.begin_full(b"mvhd", 1, 0);
        w.u32(7);
        w.end(inner);
        w.end(outer);
        let bytes = w.into_vec();
        assert_eq!(bytes.len(), 8 + 12 + 4);
        let h = BoxHeader::parse(&bytes).unwrap();
        assert_eq!(h.kind, *b"moov");
        assert_eq!(h.size, Some(24));
        let inner = BoxHeader::parse(&bytes[8..]).unwrap();
        assert_eq!(inner.kind, *b"mvhd");
        assert_eq!(inner.size, Some(16));
    }

    #[test]
    fn parses_large_and_open_ended_headers() {
        let mut large = vec![0, 0, 0, 1, b'm', b'd', b'a', b't'];
        large.extend_from_slice(&(1u64 << 33).to_be_bytes());
        let h = BoxHeader::parse(&large).unwrap();
        assert_eq!((h.size, h.header_len), (Some(1 << 33), 16));
        let open = [0, 0, 0, 0, b'm', b'd', b'a', b't'];
        assert_eq!(BoxHeader::parse(&open).unwrap().size, None);
        assert!(BoxHeader::parse(&[0, 0, 0, 4, b'x', b'x', b'x', b'x']).is_none());
    }

    #[test]
    fn field_readers_refuse_offsets_that_do_not_fit() {
        let buf = [1, 2, 3, 4, 5, 6, 7, 8, 9];
        assert_eq!(be_u32(&buf, 0), Some(0x0102_0304));
        assert_eq!(be_u64(&buf, 1), Some(0x0203_0405_0607_0809));
        assert_eq!(be_u32(&buf, 6), None);
        assert_eq!(be_u64(&buf, 2), None);
        assert_eq!(be_u32(&buf, usize::MAX), None);
        assert_eq!(be_u64(&buf, usize::MAX - 4), None);
    }

    #[test]
    fn payload_len_is_none_for_open_ended_and_undersized_headers() {
        let open = BoxHeader {
            kind: *b"mdat",
            size: None,
            header_len: 8,
        };
        assert_eq!(open.payload_len(), None);
        let undersized = BoxHeader {
            kind: *b"mdat",
            size: Some(4),
            header_len: 8,
        };
        assert_eq!(undersized.payload_len(), None);
        assert_eq!(
            BoxHeader::parse(&[0, 0, 0, 24, b'f', b'r', b'e', b'e'])
                .unwrap()
                .payload_len(),
            Some(16)
        );
    }
}
