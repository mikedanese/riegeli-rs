//! Arithmetic for navigating the Riegeli block/chunk layout.
//!
//! The file is divided into 65536-byte blocks. Each block starts with a 24-byte
//! [`BlockHeader`](crate::block_header::BlockHeader). The remaining 65512 bytes carry
//! chunk data. Chunks span blocks; block headers are interleaved transparently.

use crate::constants::{BLOCK_HEADER_SIZE, BLOCK_SIZE};

/// Returns `true` if `pos` is the first byte of a block (i.e. `pos % BLOCK_SIZE == 0`).
pub fn is_block_boundary(pos: u64) -> bool {
    pos.is_multiple_of(BLOCK_SIZE)
}

/// Round `pos` down to the nearest block boundary.
pub fn round_down_to_block_boundary(pos: u64) -> u64 {
    pos - (pos % BLOCK_SIZE)
}

/// Number of data bytes remaining in the current block before the next block boundary.
///
/// Returns `0` when `pos` is exactly at a block boundary (including `pos == 0`).
pub fn remaining_in_block(pos: u64) -> u64 {
    let offset = pos % BLOCK_SIZE;
    if offset == 0 { 0 } else { BLOCK_SIZE - offset }
}

/// The usable (non-block-header) bytes per block.
pub const USABLE_BLOCK_SIZE: u64 = BLOCK_SIZE - BLOCK_HEADER_SIZE;

/// Returns `true` if `pos` is a position where a chunk could legally start.
///
/// Mirrors the C++ `IsPossibleChunkBoundary`: a chunk position is valid when
/// it is not inside nor immediately after a block header. Per block the valid
/// offsets are `{0} ∪ {25..65535}`: a chunk whose header physically follows a
/// block header is addressed AT the block boundary (offset 0), and offset 24
/// — the first physical byte after the header — is excluded as an alias of
/// that same position.
#[cfg_attr(not(test), allow(dead_code))]
pub fn is_possible_chunk_boundary(pos: u64) -> bool {
    remaining_in_block(pos) < USABLE_BLOCK_SIZE
}

/// Round `pos` up to the nearest position where a chunk could legally start.
///
/// Mirrors the C++ `RoundUpToPossibleChunkBoundary`:
/// `pos + saturating_sub(remaining_in_block(pos), USABLE_BLOCK_SIZE - 1)`.
/// A block boundary is returned unchanged (it is a valid chunk position);
/// offsets 1..=24 advance to boundary + 25.
pub fn round_up_to_possible_chunk_boundary(pos: u64) -> u64 {
    pos.saturating_add(remaining_in_block(pos).saturating_sub(USABLE_BLOCK_SIZE - 1))
}

/// Canonicalize a chunk address: a chunk whose header physically follows a
/// block header can be referred to either by the block boundary (canonical)
/// or by the first header byte (boundary + 24, the alias). Positions are
/// stored and compared in canonical form.
pub fn canonical_chunk_address(pos: u64) -> u64 {
    if pos % BLOCK_SIZE == BLOCK_HEADER_SIZE {
        pos - BLOCK_HEADER_SIZE
    } else {
        pos
    }
}

/// The file position immediately after a chunk that begins at `chunk_begin`
/// with `data_size` data bytes and `num_records` records.
///
/// Mirrors the C++ `ChunkEnd` formula:
/// `max(AddWithOverhead(begin, header_size + data_size),
///      RoundUpToPossibleChunkBoundary(begin + num_records))`.
///
/// The second term accounts for the zero-padding the writer appends so every
/// record occupies at least one file byte (enabling recovery scanning).
/// `chunk_begin` may be given in either canonical or alias form; both
/// addresses describe the same chunk and yield the same end position.
pub fn chunk_end(chunk_begin: u64, data_size: u64, num_records: u64) -> u64 {
    // Saturating arithmetic: callers are responsible for bounding
    // data_size/num_records against the physical stream (the reader does so
    // in read_chunk_header_at; the writer controls its own sizes), but a
    // hostile value must never wrap — a wrapped end position moves scan
    // loops backward, which turns a malformed file into an infinite loop.
    // Saturation instead yields an end past any real stream, which readers
    // fail cleanly on at the next read.
    let begin = canonical_chunk_address(chunk_begin);
    let end_from_data = add_with_overhead(
        begin,
        crate::constants::CHUNK_HEADER_SIZE.saturating_add(data_size),
    );
    let end_from_records = round_up_to_possible_chunk_boundary(begin.saturating_add(num_records));
    end_from_data.max(end_from_records)
}

/// Number of block-header bytes that still need to be laid down before `pos` reaches usable data.
///
/// Returns `0` if `pos` is already in the usable data area.
/// Returns a positive value if `pos` is within the 24-byte header region at the start of a block.
pub fn remaining_in_block_header(pos: u64) -> usize {
    let offset = pos % BLOCK_SIZE;
    if offset < BLOCK_HEADER_SIZE {
        (BLOCK_HEADER_SIZE - offset) as usize
    } else {
        0
    }
}

/// Given a chunk starting at `chunk_begin`, return the file position after laying out
/// `length` bytes of chunk data, skipping over 24-byte block headers at every
/// 65536-byte block boundary encountered along the way.
pub fn add_with_overhead(chunk_begin: u64, length: u64) -> u64 {
    let mut pos = chunk_begin;
    let mut remaining = length;

    loop {
        if remaining == 0 {
            break;
        }

        // Skip any block header we're currently inside.
        let header_skip = remaining_in_block_header(pos) as u64;
        pos += header_skip;

        // How many data bytes are available until the next block boundary?
        let avail = remaining_in_block(pos);
        if avail == 0 {
            // We're exactly at a block boundary (just crossed one after a header skip
            // that landed us exactly at the next block start, or we started here).
            // Skip the block header and continue.
            pos += BLOCK_HEADER_SIZE;
            continue;
        }

        if remaining <= avail {
            pos += remaining;
            remaining = 0;
        } else {
            remaining -= avail;
            pos += avail;
            // pos is now at the next block boundary; next iteration will skip the header.
        }
    }

    pos
}

/// Return the number of chunk-data bytes between `chunk_begin` and `pos`,
/// excluding any block headers that fall in that range.
///
/// This is the inverse of [`add_with_overhead`].
pub fn distance_without_overhead(chunk_begin: u64, pos: u64) -> u64 {
    let mut cur = chunk_begin;
    let mut data_bytes = 0u64;

    while cur < pos {
        // Skip any block header at current position.
        let header_skip = remaining_in_block_header(cur) as u64;
        cur += header_skip;

        if cur >= pos {
            break;
        }

        // How many data bytes until the next block boundary (or pos)?
        let avail = remaining_in_block(cur);
        let avail = if avail == 0 {
            // At block boundary after header — shouldn't normally occur here
            // since we just skipped the header. Treat as full usable block.
            BLOCK_SIZE - BLOCK_HEADER_SIZE
        } else {
            avail
        };

        let step = avail.min(pos - cur);
        data_bytes += step;
        cur += step;
    }

    data_bytes
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_block_boundary() {
        assert!(is_block_boundary(0));
        assert!(is_block_boundary(65536));
        assert!(is_block_boundary(131072));
        assert!(!is_block_boundary(1));
        assert!(!is_block_boundary(65535));
        assert!(!is_block_boundary(65537));
    }

    #[test]
    fn test_round_down_to_block_boundary() {
        assert_eq!(round_down_to_block_boundary(0), 0);
        assert_eq!(round_down_to_block_boundary(1), 0);
        assert_eq!(round_down_to_block_boundary(65535), 0);
        assert_eq!(round_down_to_block_boundary(65536), 65536);
        assert_eq!(round_down_to_block_boundary(65537), 65536);
    }

    #[test]
    fn test_remaining_in_block() {
        assert_eq!(remaining_in_block(0), 0);
        assert_eq!(remaining_in_block(1), 65535);
        assert_eq!(remaining_in_block(65535), 1);
        assert_eq!(remaining_in_block(65536), 0);
        assert_eq!(remaining_in_block(65537), 65535);
    }

    #[test]
    fn test_is_possible_chunk_boundary() {
        // C++ convention: valid offsets per block are {0} ∪ {25..65535}.
        // A block boundary IS a chunk position (the chunk's header bytes
        // physically follow the block header); offsets 1..=24 are not —
        // 1..=23 are inside the block header and 24 is the alias of the
        // boundary address.
        assert!(is_possible_chunk_boundary(0));
        assert!(!is_possible_chunk_boundary(1));
        assert!(!is_possible_chunk_boundary(23));
        assert!(!is_possible_chunk_boundary(24));
        assert!(is_possible_chunk_boundary(25));
        assert!(is_possible_chunk_boundary(65535));
        assert!(is_possible_chunk_boundary(65536));
        assert!(!is_possible_chunk_boundary(65536 + 24));
        assert!(is_possible_chunk_boundary(65536 + 25));
    }

    #[test]
    fn test_round_up_to_possible_chunk_boundary() {
        // A boundary is already valid; offsets 1..=24 advance to boundary+25.
        assert_eq!(round_up_to_possible_chunk_boundary(0), 0);
        assert_eq!(round_up_to_possible_chunk_boundary(1), 25);
        assert_eq!(round_up_to_possible_chunk_boundary(23), 25);
        assert_eq!(round_up_to_possible_chunk_boundary(24), 25);
        assert_eq!(round_up_to_possible_chunk_boundary(25), 25);
        assert_eq!(round_up_to_possible_chunk_boundary(100), 100);
        assert_eq!(round_up_to_possible_chunk_boundary(65536), 65536);
        assert_eq!(round_up_to_possible_chunk_boundary(65536 + 1), 65536 + 25);
        assert_eq!(round_up_to_possible_chunk_boundary(65536 + 24), 65536 + 25);
        assert_eq!(round_up_to_possible_chunk_boundary(65536 + 25), 65536 + 25);
        // Acceptance data point verified against the real C++ implementation:
        // 64 + 131019 = 131072 + 11 → 131097.
        assert_eq!(round_up_to_possible_chunk_boundary(131084), 131097);
    }

    #[test]
    fn test_canonical_chunk_address() {
        assert_eq!(canonical_chunk_address(0), 0);
        assert_eq!(canonical_chunk_address(24), 0);
        assert_eq!(canonical_chunk_address(25), 25);
        assert_eq!(canonical_chunk_address(64), 64);
        assert_eq!(canonical_chunk_address(65536), 65536);
        assert_eq!(canonical_chunk_address(65536 + 24), 65536);
        assert_eq!(canonical_chunk_address(65536 + 25), 65536 + 25);
    }

    #[test]
    fn test_chunk_end_equal_addressing_invariant() {
        // chunk_end(boundary, ...) == chunk_end(boundary + 24, ...) — both
        // addresses describe the same chunk.
        for (data_size, num_records) in [
            (0u64, 0u64),
            (53, 131019),
            (100, 1),
            (70_000, 3),
            (10, 70_000),
        ] {
            for boundary in [0u64, 65536, 131072] {
                assert_eq!(
                    chunk_end(boundary, data_size, num_records),
                    chunk_end(boundary + 24, data_size, num_records),
                    "boundary={boundary} data_size={data_size} num_records={num_records}"
                );
            }
        }
    }

    #[test]
    fn test_chunk_end_acceptance_points() {
        // The signature chunk: begins at 0 (canonical), 40-byte header,
        // no data, no records → ends at 64.
        assert_eq!(chunk_end(0, 0, 0), 64);
        // 131k-empties zstd repro verified against C++:
        // record chunk at 64, data_size=53, num_records=131019 → 131097.
        assert_eq!(chunk_end(64, 53, 131019), 131097);
    }

    #[test]
    fn test_add_with_overhead_matches_cpp_closed_form() {
        // The C++ AddWithOverhead is a closed form; ours walks positions.
        // They must agree for every canonical chunk_begin.
        fn cpp_add_with_overhead(chunk_begin: u64, length: u64) -> u64 {
            let usable = BLOCK_SIZE - BLOCK_HEADER_SIZE;
            let num_overhead_blocks = (length + (chunk_begin + usable - 1) % BLOCK_SIZE) / usable;
            chunk_begin + length + num_overhead_blocks * BLOCK_HEADER_SIZE
        }
        let begins = [0u64, 25, 64, 1000, 65535, 65536, 65536 + 25, 131072, 131097];
        let lengths = [
            0u64, 1, 40, 93, 65471, 65472, 65473, 65512, 65513, 131024, 200_000,
        ];
        for &b in &begins {
            assert!(
                is_possible_chunk_boundary(b),
                "test input {b} not canonical"
            );
            for &l in &lengths {
                assert_eq!(
                    add_with_overhead(b, l),
                    cpp_add_with_overhead(b, l),
                    "begin={b} len={l}"
                );
            }
        }
    }

    #[test]
    fn test_remaining_in_block_header() {
        assert_eq!(remaining_in_block_header(0), 24);
        assert_eq!(remaining_in_block_header(1), 23);
        assert_eq!(remaining_in_block_header(23), 1);
        assert_eq!(remaining_in_block_header(24), 0);
        assert_eq!(remaining_in_block_header(65536), 24);
        assert_eq!(remaining_in_block_header(65536 + 24), 0);
    }

    #[test]
    fn test_add_with_overhead_simple() {
        // Chunk at 24, 100 bytes — fits entirely in first block.
        assert_eq!(add_with_overhead(24, 100), 124);
    }

    #[test]
    fn test_add_with_overhead_zero_length() {
        assert_eq!(add_with_overhead(24, 0), 24);
        assert_eq!(add_with_overhead(100, 0), 100);
    }

    #[test]
    fn test_add_with_overhead_crosses_block_boundary() {
        // Start at offset 24, lay out enough data to cross the first block boundary.
        // Usable bytes in first block from offset 24: 65536 - 24 = 65512 bytes.
        // After 65512 bytes of data, we reach offset 65536 (block boundary).
        // Then there's a 24-byte block header, then more data.
        let start = 24u64;
        let length = 65512 + 100; // cross into second block by 100 bytes
        let result = add_with_overhead(start, length);
        // Expected: 65536 (block boundary) + 24 (header) + 100 = 65660
        assert_eq!(result, 65536 + 24 + 100);
    }

    #[test]
    fn test_add_with_overhead_exact_block_end() {
        // Lay out exactly 65512 bytes from offset 24 — land exactly at block boundary.
        let start = 24u64;
        let length = 65512u64;
        let result = add_with_overhead(start, length);
        assert_eq!(result, 65536);
    }

    #[test]
    fn test_distance_without_overhead_simple() {
        // From 24 to 124, no block header crossed — distance is 100.
        assert_eq!(distance_without_overhead(24, 124), 100);
    }

    #[test]
    fn test_distance_without_overhead_crosses_block() {
        // From 24 to 65536 + 24 + 100: data = 65512 + 100 = 65612.
        let end = 65536 + 24 + 100;
        assert_eq!(distance_without_overhead(24, end as u64), 65512 + 100);
    }

    #[test]
    fn test_add_distance_roundtrip() {
        // add_with_overhead and distance_without_overhead should be inverses.
        let starts = [24u64, 100, 65536 + 24, 131072 + 24];
        let lengths = [0u64, 1, 65512, 65513, 200_000];
        for &start in &starts {
            for &len in &lengths {
                let end = add_with_overhead(start, len);
                let recovered = distance_without_overhead(start, end);
                assert_eq!(
                    recovered, len,
                    "round-trip failed for start={start}, len={len}"
                );
            }
        }
    }
}
