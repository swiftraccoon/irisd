//! CRC-32C (the iSCSI polynomial), the trailer checksum.
//!
//! The value follows the conventions of the `crc32c` crate (a test checks
//! them); the computation is `crc-fast`'s carry-less-multiply folding. The
//! trailer checksum covers every fragment byte, so its cost is the writer's
//! largest item: `docs/research/12-ingest-cost.md` §5 records it.

use crc_fast::{CrcAlgorithm, Digest};

/// A running CRC-32C. Copying one forks the checksum at that point.
#[derive(Clone, Copy)]
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

    fn pseudo_random(len: usize) -> Vec<u8> {
        (0..len as u32)
            .map(|i| (i.wrapping_mul(2_654_435_761) >> 13) as u8)
            .collect()
    }

    #[test]
    fn matches_the_reference_implementation_at_every_alignment() {
        // Every start residue of a 64-byte block and lengths around the
        // folding widths, so a head loop over misaligned input is compared too.
        let data = pseudo_random(64 + 10_000);
        for start in 0..64 {
            for len in [
                0, 1, 3, 7, 8, 15, 16, 63, 64, 65, 255, 256, 1_000, 4_099, 10_000,
            ] {
                let slice = &data[start..start + len];
                assert_eq!(
                    crc32c(slice),
                    crc32c::crc32c(slice),
                    "start {start} len {len}"
                );
            }
        }
    }

    #[test]
    fn a_running_checksum_matches_the_reference_after_every_chunk() {
        let data = pseudo_random(10_000);
        let mut running = Crc32c::new();
        let mut fed = 0;
        for chunk in data.chunks(777) {
            running.update(chunk);
            fed += chunk.len();
            assert_eq!(
                running.finish(),
                crc32c::crc32c(&data[..fed]),
                "{fed} bytes"
            );
        }
        let forked = running;
        running.update(b"more");
        assert_eq!(forked.finish(), crc32c::crc32c(&data));
        assert_ne!(running.finish(), forked.finish());
    }

    #[test]
    fn matches_the_published_check_value() {
        assert_eq!(crc32c(b"123456789"), 0xe306_9283);
    }
}
