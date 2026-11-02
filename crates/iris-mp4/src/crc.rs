//! CRC-32C (the iSCSI polynomial) over a SIMD implementation.
//!
//! The on-disk format is unchanged: the same polynomial and conventions as the
//! `crc32c` crate, checked by a test. The implementation folds with carry-less
//! multiplies and runs an order of magnitude faster than the table-and-`crc32q`
//! loop, which mattered because the trailer checksum re-reads every fragment.

use crc_fast::{CrcAlgorithm, Digest};

/// A running CRC-32C.
pub struct Crc32c(Digest);

impl Default for Crc32c {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for Crc32c {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Crc32c").finish_non_exhaustive()
    }
}

impl Crc32c {
    /// Starts a checksum.
    #[must_use]
    pub fn new() -> Self {
        Self(Digest::new(CrcAlgorithm::Crc32Iscsi))
    }

    /// Feeds bytes.
    pub fn update(&mut self, data: &[u8]) {
        self.0.update(data);
    }

    /// The checksum of everything fed so far.
    #[must_use]
    pub fn finish(&self) -> u32 {
        // CRC-32C fits in 32 bits; the digest returns it widened.
        self.0.finalize() as u32
    }
}

/// The CRC-32C of `data`.
#[must_use]
pub fn crc32c(data: &[u8]) -> u32 {
    crc_fast::crc32_iscsi(data)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_the_reference_implementation_at_every_alignment() {
        let data: Vec<u8> = (0..10_000u32)
            .map(|i| (i.wrapping_mul(2_654_435_761) >> 13) as u8)
            .collect();
        for len in [
            0, 1, 3, 7, 8, 15, 16, 63, 64, 65, 255, 256, 1_000, 4_099, 10_000,
        ] {
            assert_eq!(
                crc32c(&data[..len]),
                crc32c::crc32c(&data[..len]),
                "len {len}"
            );
        }
        let mut running = Crc32c::new();
        for chunk in data.chunks(777) {
            running.update(chunk);
        }
        assert_eq!(running.finish(), crc32c::crc32c(&data));
        assert_eq!(crc32c(b"123456789"), 0xe306_9283);
    }
}
