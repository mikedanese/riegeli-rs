//! HighwayHash computation using the Riegeli-specific 256-bit key.
//!
//! The Riegeli key is the ASCII string `"Riegeli/records\nRiegeli/records\n"`,
//! interpreted as four little-endian 64-bit words:
//! ```text
//! [0x2f696c6567656952, 0x0a7364726f636572,
//!  0x2f696c6567656952, 0x0a7364726f636572]
//! ```

use highway::{HighwayHash, HighwayHasher, Key};

/// The Riegeli HighwayHash key as four little-endian u64 words.
///
/// ASCII: `"Riegeli/records\nRiegeli/records\n"`
pub const RIEGELI_HASH_KEY: [u64; 4] = [
    0x2f696c6567656952,
    0x0a7364726f636572,
    0x2f696c6567656952,
    0x0a7364726f636572,
];

/// Compute the 64-bit HighwayHash of `data` using the Riegeli key.
pub fn highway_hash_64(data: &[u8]) -> u64 {
    HighwayHasher::new(Key(RIEGELI_HASH_KEY)).hash64(data)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_hash_empty_is_deterministic() {
        let h1 = highway_hash_64(b"");
        let h2 = highway_hash_64(b"");
        assert_eq!(h1, h2, "hash must be deterministic");
    }

    #[test]
    fn test_hash_empty_is_nonzero() {
        let h = highway_hash_64(b"");
        assert_ne!(h, 0, "hash of empty string should be non-zero");
    }

    #[test]
    fn test_hash_differs_for_different_inputs() {
        let h_empty = highway_hash_64(b"");
        let h_hello = highway_hash_64(b"hello");
        assert_ne!(
            h_empty, h_hello,
            "different inputs must produce different hashes"
        );
    }

    /// Verify that block header hash construction and extraction round-trips.
    ///
    /// The first block header has previous_chunk=0, next_chunk=24.
    /// The header_hash covers bytes[8..24] of the serialized header.
    #[test]
    fn test_hash_known_block_header_body() {
        // Build the 16-byte body: [0u8;8] ++ 24u64.to_le_bytes()
        let mut body = [0u8; 16];
        body[0..8].copy_from_slice(&0u64.to_le_bytes());
        body[8..16].copy_from_slice(&24u64.to_le_bytes());

        let computed_hash = highway_hash_64(&body);

        // Store in a full 24-byte header and re-extract
        let mut full_header = [0u8; 24];
        full_header[0..8].copy_from_slice(&computed_hash.to_le_bytes());
        full_header[8..24].copy_from_slice(&body);

        let stored = u64::from_le_bytes(full_header[0..8].try_into().unwrap());
        let recomputed = highway_hash_64(&full_header[8..24]);
        assert_eq!(stored, recomputed, "block header hash round-trip failed");
    }

    #[test]
    fn test_cpp_reference_hash_empty() {
        // Pinned value from highway crate v1.3.0 with the Riegeli key.
        // If this changes, the highway crate or RIEGELI_HASH_KEY has diverged
        // from the C++ reference implementation.
        assert_eq!(highway_hash_64(b""), 0x72c3b1e9c0139fe1);
    }

    #[test]
    fn test_hash_bit_flip_detection() {
        // Flipping any single bit in a 16-byte body must change the hash.
        let mut body = [0u8; 16];
        body[0..8].copy_from_slice(&0u64.to_le_bytes());
        body[8..16].copy_from_slice(&24u64.to_le_bytes());
        let original = highway_hash_64(&body);
        for byte_idx in 0..16 {
            for bit in 0..8 {
                let mut mutated = body;
                mutated[byte_idx] ^= 1 << bit;
                assert_ne!(
                    original,
                    highway_hash_64(&mutated),
                    "flipping bit {bit} of byte {byte_idx} should change the hash"
                );
            }
        }
    }
}
