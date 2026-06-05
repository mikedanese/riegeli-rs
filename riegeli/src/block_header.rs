//! Block header serialization, deserialization, and hash verification.
//!
//! Every Riegeli block starts with a 24-byte `BlockHeader`:
//! ```text
//! bytes [ 0.. 8] — header_hash     (LE u64): HighwayHash of bytes[8..24]
//! bytes [ 8..16] — previous_chunk  (LE u64): distance from this block boundary
//!                   back to the start of the chunk in progress at it (0 if a
//!                   chunk starts exactly here)
//! bytes [16..24] — next_chunk      (LE u64): distance from this block boundary
//!                   forward to the next chunk start
//! ```

use crate::hash::highway_hash_64;

/// A 24-byte block header found at every 65536-byte block boundary in a Riegeli file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BlockHeader {
    /// HighwayHash of bytes[8..24] of the serialized header.
    pub(crate) header_hash: u64,
    /// Distance from the block boundary back to the in-progress chunk's start.
    pub(crate) previous_chunk: u64,
    /// Distance from the block boundary forward to the next chunk's start.
    pub(crate) next_chunk: u64,
}

impl BlockHeader {
    /// Construct a `BlockHeader` from its two payload fields, computing `header_hash`.
    pub fn from_parts(previous_chunk: u64, next_chunk: u64) -> Self {
        // Build bytes[8..24] to hash.
        let mut body = [0u8; 16];
        body[0..8].copy_from_slice(&previous_chunk.to_le_bytes());
        body[8..16].copy_from_slice(&next_chunk.to_le_bytes());
        let header_hash = highway_hash_64(&body);
        Self {
            header_hash,
            previous_chunk,
            next_chunk,
        }
    }

    /// Deserialize a `BlockHeader` from its raw 24-byte wire representation.
    ///
    /// The stored `header_hash` is taken as-is; use [`BlockHeader::is_valid`] to verify it.
    pub fn from_bytes(bytes: [u8; 24]) -> Self {
        let header_hash = u64::from_le_bytes(bytes[0..8].try_into().unwrap());
        let previous_chunk = u64::from_le_bytes(bytes[8..16].try_into().unwrap());
        let next_chunk = u64::from_le_bytes(bytes[16..24].try_into().unwrap());
        Self {
            header_hash,
            previous_chunk,
            next_chunk,
        }
    }

    /// Serialize this `BlockHeader` into its 24-byte wire representation.
    pub fn to_bytes(self) -> [u8; 24] {
        let mut bytes = [0u8; 24];
        bytes[0..8].copy_from_slice(&self.header_hash.to_le_bytes());
        bytes[8..16].copy_from_slice(&self.previous_chunk.to_le_bytes());
        bytes[16..24].copy_from_slice(&self.next_chunk.to_le_bytes());
        bytes
    }

    /// The `previous_chunk` field: distance back to the in-progress chunk's start.
    #[allow(dead_code)]
    pub fn previous_chunk(&self) -> u64 {
        self.previous_chunk
    }

    /// The `next_chunk` field: distance forward to the next chunk's start.
    #[allow(dead_code)]
    pub fn next_chunk(&self) -> u64 {
        self.next_chunk
    }

    /// The `header_hash` as stored in this header (may be invalid if deserialized from corrupt data).
    #[allow(dead_code)]
    pub fn stored_hash(&self) -> u64 {
        self.header_hash
    }

    /// Compute the expected hash from the current `previous_chunk` and `next_chunk` fields.
    pub fn computed_hash(&self) -> u64 {
        let bytes = self.to_bytes();
        highway_hash_64(&bytes[8..24])
    }

    /// Returns `true` if `stored_hash == highway_hash_64(bytes[8..24])`.
    pub fn is_valid(&self) -> bool {
        self.header_hash == self.computed_hash()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_parts_is_valid() {
        let h = BlockHeader::from_parts(0, 24);
        assert!(h.is_valid(), "from_parts must produce a valid header");
    }

    #[test]
    fn from_parts_accessors() {
        let h = BlockHeader::from_parts(100, 200);
        assert_eq!(h.previous_chunk(), 100);
        assert_eq!(h.next_chunk(), 200);
    }

    #[test]
    fn round_trip() {
        let original = BlockHeader::from_parts(0, 24);
        let bytes = original.to_bytes();
        let restored = BlockHeader::from_bytes(bytes);
        assert_eq!(original, restored);
        assert!(restored.is_valid());
    }

    #[test]
    fn bit_flip_in_body_invalidates() {
        let h = BlockHeader::from_parts(0, 24);
        let mut bytes = h.to_bytes();
        // Flip every bit in bytes[8..24] individually and check invalidation.
        for byte_idx in 8..24usize {
            for bit in 0..8u8 {
                let mut corrupt = bytes;
                corrupt[byte_idx] ^= 1 << bit;
                let corrupted = BlockHeader::from_bytes(corrupt);
                assert!(
                    !corrupted.is_valid(),
                    "bit flip at byte {byte_idx} bit {bit} should invalidate"
                );
            }
        }
        // Flipping bits in bytes[0..8] (the hash field itself) should also invalidate.
        for byte_idx in 0..8usize {
            for bit in 0..8u8 {
                bytes[byte_idx] ^= 1 << bit;
                let corrupted = BlockHeader::from_bytes(bytes);
                assert!(
                    !corrupted.is_valid(),
                    "bit flip in hash field at byte {byte_idx} bit {bit} should invalidate"
                );
                bytes[byte_idx] ^= 1 << bit; // restore
            }
        }
    }

    // -------------------------------------------------------------------------
    // Sprint 5 block boundary tests (moved from integration tests)
    // -------------------------------------------------------------------------

    use crate::block_arithmetic::add_with_overhead;
    use crate::chunk_header::ChunkHeader;
    use crate::constants::{BLOCK_HEADER_SIZE, BLOCK_SIZE, CHUNK_HEADER_SIZE};
    use crate::record_writer::{RecordWriter, WriterOptions};
    use crate::simple_chunk::{Chunk, SimpleChunkDecoder};
    use std::io::Write;

    struct VecWriter {
        data: Vec<u8>,
    }

    impl VecWriter {
        fn new() -> Self {
            Self { data: Vec::new() }
        }
    }

    impl Write for VecWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.data.extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn write_file_unit(records: &[&[u8]], options: WriterOptions) -> Vec<u8> {
        let mut w = VecWriter::new();
        {
            let mut writer = RecordWriter::new(&mut w, options).expect("new ok");
            for rec in records {
                writer.write_record(rec).expect("write ok");
            }
            writer.flush().expect("flush ok");
        }
        w.data
    }

    /// Walk the file and extract all records from Simple chunks, handling block
    /// headers at block boundaries.
    fn decode_all_records_unit(file_data: &[u8]) -> Vec<Vec<u8>> {
        use crate::chunk_header::ChunkType;
        let mut pos = 64usize; // after block header (24) + signature chunk header (40)
        let mut all_records: Vec<Vec<u8>> = Vec::new();

        while pos < file_data.len() {
            // Skip block header if at block boundary
            if pos % BLOCK_SIZE as usize == 0 {
                pos += BLOCK_HEADER_SIZE as usize;
                if pos >= file_data.len() {
                    break;
                }
            }

            // Read chunk header bytes, skipping any block boundaries within the header
            if pos + CHUNK_HEADER_SIZE as usize > file_data.len() {
                break;
            }
            let mut ch_raw = [0u8; 40];
            let mut ch_offset = 0;
            while ch_offset < 40 {
                if pos % BLOCK_SIZE as usize == 0 {
                    pos += BLOCK_HEADER_SIZE as usize;
                }
                let space = (BLOCK_SIZE as usize - (pos % BLOCK_SIZE as usize)).min(40 - ch_offset);
                if pos + space > file_data.len() {
                    return all_records;
                }
                ch_raw[ch_offset..ch_offset + space].copy_from_slice(&file_data[pos..pos + space]);
                pos += space;
                ch_offset += space;
            }

            let ch = ChunkHeader::from_bytes(ch_raw);
            if !ch.is_header_valid() {
                panic!("invalid chunk header");
            }

            let data_size = ch.data_size() as usize;
            let mut chunk_data = Vec::with_capacity(data_size);
            let mut remaining = data_size;
            while remaining > 0 {
                if pos % BLOCK_SIZE as usize == 0 {
                    pos += BLOCK_HEADER_SIZE as usize;
                }
                let space = (BLOCK_SIZE as usize - (pos % BLOCK_SIZE as usize)).min(remaining);
                if pos + space > file_data.len() {
                    return all_records;
                }
                chunk_data.extend_from_slice(&file_data[pos..pos + space]);
                pos += space;
                remaining -= space;
            }

            if ch.chunk_type().unwrap() == ChunkType::Simple {
                let chunk = Chunk {
                    header: ch,
                    data: chunk_data,
                };
                let mut decoder = SimpleChunkDecoder::new(chunk).expect("decoder ok");
                while let Some(rec) = decoder.read_record().expect("read ok") {
                    all_records.push(rec);
                }
            }
        }

        all_records
    }

    /// Collect all chunk start positions from the file (by walking block headers).
    fn collect_chunk_starts_unit(file_data: &[u8]) -> Vec<u64> {
        let mut starts = Vec::new();
        let mut pos = 64usize; // after sig chunk

        while pos < file_data.len() {
            if pos % BLOCK_SIZE as usize == 0 {
                pos += BLOCK_HEADER_SIZE as usize;
                if pos >= file_data.len() {
                    break;
                }
            }

            let chunk_start = pos as u64;
            starts.push(chunk_start);

            // Read the chunk header to find data_size
            if pos + 40 > file_data.len() {
                break;
            }
            let mut ch_raw = [0u8; 40];
            let mut temp_pos = pos;
            let mut ch_offset = 0;
            while ch_offset < 40 {
                if temp_pos % BLOCK_SIZE as usize == 0 {
                    temp_pos += BLOCK_HEADER_SIZE as usize;
                }
                let space =
                    (BLOCK_SIZE as usize - (temp_pos % BLOCK_SIZE as usize)).min(40 - ch_offset);
                if temp_pos + space > file_data.len() {
                    return starts;
                }
                ch_raw[ch_offset..ch_offset + space]
                    .copy_from_slice(&file_data[temp_pos..temp_pos + space]);
                temp_pos += space;
                ch_offset += space;
            }

            let ch = ChunkHeader::from_bytes(ch_raw);
            let total = CHUNK_HEADER_SIZE as u64 + ch.data_size();
            let end = add_with_overhead(chunk_start, total);
            pos = end as usize;
        }

        starts
    }

    /// Write a record that exactly fills the first block (65536 bytes total).
    #[test]
    fn exact_first_block_fill() {
        let record_size = 65427; // fills first block exactly
        let record = vec![0xABu8; record_size];
        let opts = WriterOptions::new().chunk_size(record_size as u64 + 1);
        let data = write_file_unit(&[&record], opts);

        assert_eq!(
            data.len(),
            BLOCK_SIZE as usize,
            "file should be exactly one block; got {} bytes",
            data.len()
        );

        let decoded = decode_all_records_unit(&data);
        assert_eq!(decoded.len(), 1);
        assert_eq!(decoded[0].len(), record_size);
    }

    /// One byte more than the exact fill: should spill into second block.
    #[test]
    fn one_byte_past_first_block() {
        let record_size = 65428; // one byte more than exact fill
        let record = vec![0xABu8; record_size];
        let opts = WriterOptions::new().chunk_size(record_size as u64 + 1);
        let data = write_file_unit(&[&record], opts);

        assert!(
            data.len() > BLOCK_SIZE as usize,
            "file should span into second block; got {} bytes",
            data.len()
        );

        // Verify second block header is valid
        if data.len() >= BLOCK_SIZE as usize + BLOCK_HEADER_SIZE as usize {
            let bh = BlockHeader::from_bytes(
                data[BLOCK_SIZE as usize..BLOCK_SIZE as usize + 24]
                    .try_into()
                    .unwrap(),
            );
            assert!(bh.is_valid(), "second block header must be valid");
        }

        let decoded = decode_all_records_unit(&data);
        assert_eq!(decoded.len(), 1);
        assert_eq!(decoded[0].len(), record_size);
    }

    /// Write records that cause a chunk to straddle a block boundary.
    #[test]
    fn chunk_straddles_block_boundary() {
        let record = vec![0xCD; 2000];
        let num_records = 40; // 40 * 2000 = 80000 bytes > 65536
        let records: Vec<&[u8]> = (0..num_records).map(|_| record.as_slice()).collect();

        let opts = WriterOptions::new().chunk_size(100_000);
        let data = write_file_unit(&records, opts);

        assert!(
            data.len() > BLOCK_SIZE as usize,
            "file must span multiple blocks"
        );

        // Check all block headers are valid
        let mut pos = 0usize;
        while pos + 24 <= data.len() {
            let bh = BlockHeader::from_bytes(data[pos..pos + 24].try_into().unwrap());
            assert!(bh.is_valid(), "block header at {pos} must be valid");
            pos += BLOCK_SIZE as usize;
        }

        // Verify all records decode correctly
        let decoded = decode_all_records_unit(&data);
        assert_eq!(decoded.len(), num_records, "all records should decode");
        for (i, rec) in decoded.iter().enumerate() {
            assert_eq!(rec.len(), 2000, "record {i} length");
            assert!(rec.iter().all(|&b| b == 0xCD), "record {i} content");
        }
    }

    /// Verify that previous_chunk and next_chunk in block headers point to valid
    /// chunk boundaries.
    #[test]
    fn block_header_pointers_are_valid_chunk_boundaries() {
        let record = vec![0x42; 500];
        let records: Vec<&[u8]> = (0..300).map(|_| record.as_slice()).collect();
        let opts = WriterOptions::new().chunk_size(2048);
        let data = write_file_unit(&records, opts);

        assert!(data.len() > 2 * BLOCK_SIZE as usize, "need multiple blocks");

        let chunk_starts = collect_chunk_starts_unit(&data);

        let mut block_pos = 0u64;
        while (block_pos as usize) + 24 <= data.len() {
            let bh = BlockHeader::from_bytes(
                data[block_pos as usize..(block_pos as usize) + 24]
                    .try_into()
                    .unwrap(),
            );
            assert!(bh.is_valid(), "block header at {block_pos} must be valid");

            let prev = bh.previous_chunk();
            let next = bh.next_chunk();

            if block_pos == 0 {
                assert_eq!(prev, 0, "first block header previous_chunk must be 0");
                assert_eq!(next, 64, "first block header next_chunk must be 64");
            } else {
                let chunk_begin = block_pos - prev;
                let all_chunk_positions: Vec<u64> = std::iter::once(0u64)
                    .chain(std::iter::once(64u64))
                    .chain(chunk_starts.iter().copied())
                    .collect();
                assert!(
                    all_chunk_positions.contains(&chunk_begin),
                    "block header at {block_pos}: chunk_begin={chunk_begin} (from prev={prev}) is not a known chunk start. Known: {all_chunk_positions:?}"
                );
                assert!(
                    next > 0,
                    "block header at {block_pos}: next_chunk={next} should be > 0"
                );
            }

            block_pos += BLOCK_SIZE;
        }
    }

    /// empty file has valid structure (block header + signature chunk).
    #[test]
    fn empty_file_has_valid_structure() {
        use crate::chunk_header::ChunkType;
        let data = write_file_unit(&[], WriterOptions::new());

        assert!(data.len() >= 64);

        let bh = BlockHeader::from_bytes(data[..24].try_into().unwrap());
        assert!(bh.is_valid());
        assert_eq!(bh.previous_chunk(), 0);
        assert_eq!(bh.next_chunk(), 64);

        let sig = ChunkHeader::from_bytes(data[24..64].try_into().unwrap());
        assert!(sig.is_header_valid());
        assert_eq!(sig.chunk_type().unwrap(), ChunkType::FileSignature);
        assert_eq!(sig.data_size(), 0);
    }
}
