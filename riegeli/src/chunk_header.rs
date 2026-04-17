//! Chunk header serialization, deserialization, and hash verification.
//!
//! A Riegeli chunk header is 40 bytes:
//! ```text
//! bytes [ 0.. 8] — header_hash               (LE u64): HighwayHash of bytes[8..40]
//! bytes [ 8..16] — data_size                  (LE u64): length of the chunk data in bytes
//! bytes [16..24] — data_hash                  (LE u64): HighwayHash of the chunk data
//! bytes [24..32] — chunk_type_and_num_records (LE u64): low 8 bits = chunk_type, high 56 bits = num_records
//! bytes [32..40] — decoded_data_size          (LE u64): uncompressed data size
//! ```

use crate::error::RiegeliError;
use crate::hash::highway_hash_64;

/// The type byte that identifies what kind of payload a chunk carries.
///
/// The discriminant values match the C++ wire format exactly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub(crate) enum ChunkType {
    /// File signature chunk — the first chunk in every file (type byte `'s'`).
    FileSignature = b's',
    /// File metadata chunk (type byte `'m'`).
    FileMetadata = b'm',
    /// Padding chunk — used to align to a block boundary (type byte `'p'`).
    Padding = b'p',
    /// Simple chunk — contains a sequence of records (type byte `'r'`).
    Simple = b'r',
    /// Transposed chunk — field-columnar layout for better proto compression (type byte `'t'`).
    Transposed = b't',
}

impl TryFrom<u8> for ChunkType {
    type Error = RiegeliError;

    fn try_from(b: u8) -> Result<Self, Self::Error> {
        match b {
            b's' => Ok(ChunkType::FileSignature),
            b'm' => Ok(ChunkType::FileMetadata),
            b'p' => Ok(ChunkType::Padding),
            b'r' => Ok(ChunkType::Simple),
            b't' => Ok(ChunkType::Transposed),
            _ => Err(RiegeliError::UnknownChunkType(b)),
        }
    }
}

/// A 40-byte chunk header describing the payload that immediately follows it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChunkHeader {
    /// HighwayHash of bytes[8..40] of the serialized header.
    pub(crate) header_hash: u64,
    /// Length of the chunk data in bytes.
    pub(crate) data_size: u64,
    /// HighwayHash of the chunk data.
    pub(crate) data_hash: u64,
    /// Packed field: `(num_records << 8) | (chunk_type as u64)`.
    pub(crate) chunk_type_and_num_records: u64,
    /// Uncompressed size of the record data.
    pub(crate) decoded_data_size: u64,
}

impl ChunkHeader {
    /// Construct a `ChunkHeader` from its payload data and metadata, computing all hashes.
    ///
    /// - `data`: the raw chunk data bytes (used to compute `data_size` and `data_hash`)
    /// - `chunk_type`: the type byte identifying this chunk's encoding
    /// - `num_records`: number of records stored in this chunk
    /// - `decoded_data_size`: total uncompressed size of all records
    pub fn from_parts(
        data: &[u8],
        chunk_type: ChunkType,
        num_records: u64,
        decoded_data_size: u64,
    ) -> Self {
        let data_size = data.len() as u64;
        let data_hash = highway_hash_64(data);
        let chunk_type_and_num_records = (num_records << 8) | (chunk_type as u64);

        // Build bytes[8..40] to compute header_hash.
        let mut body = [0u8; 32];
        body[0..8].copy_from_slice(&data_size.to_le_bytes());
        body[8..16].copy_from_slice(&data_hash.to_le_bytes());
        body[16..24].copy_from_slice(&chunk_type_and_num_records.to_le_bytes());
        body[24..32].copy_from_slice(&decoded_data_size.to_le_bytes());
        let header_hash = highway_hash_64(&body);

        Self {
            header_hash,
            data_size,
            data_hash,
            chunk_type_and_num_records,
            decoded_data_size,
        }
    }

    /// Deserialize a `ChunkHeader` from its raw 40-byte wire representation.
    ///
    /// The stored hashes are taken as-is; use [`ChunkHeader::is_header_valid`] and [`ChunkHeader::is_data_valid`] to verify.
    pub fn from_bytes(bytes: [u8; 40]) -> Self {
        let header_hash = u64::from_le_bytes(bytes[0..8].try_into().unwrap());
        let data_size = u64::from_le_bytes(bytes[8..16].try_into().unwrap());
        let data_hash = u64::from_le_bytes(bytes[16..24].try_into().unwrap());
        let chunk_type_and_num_records = u64::from_le_bytes(bytes[24..32].try_into().unwrap());
        let decoded_data_size = u64::from_le_bytes(bytes[32..40].try_into().unwrap());
        Self {
            header_hash,
            data_size,
            data_hash,
            chunk_type_and_num_records,
            decoded_data_size,
        }
    }

    /// Serialize this `ChunkHeader` into its 40-byte wire representation.
    pub fn to_bytes(self) -> [u8; 40] {
        let mut bytes = [0u8; 40];
        bytes[0..8].copy_from_slice(&self.header_hash.to_le_bytes());
        bytes[8..16].copy_from_slice(&self.data_size.to_le_bytes());
        bytes[16..24].copy_from_slice(&self.data_hash.to_le_bytes());
        bytes[24..32].copy_from_slice(&self.chunk_type_and_num_records.to_le_bytes());
        bytes[32..40].copy_from_slice(&self.decoded_data_size.to_le_bytes());
        bytes
    }

    /// The `data_size` field: length of the chunk data in bytes.
    pub fn data_size(&self) -> u64 {
        self.data_size
    }

    /// The `data_hash` field: HighwayHash of the chunk data as stored in the header.
    #[allow(dead_code)]
    pub fn data_hash(&self) -> u64 {
        self.data_hash
    }

    /// The chunk type extracted from the low 8 bits of `chunk_type_and_num_records`.
    ///
    /// Returns `Err` if the stored byte is not a known `ChunkType` discriminant.
    pub fn chunk_type(&self) -> Result<ChunkType, RiegeliError> {
        let byte = (self.chunk_type_and_num_records & 0xff) as u8;
        match byte {
            b's' => Ok(ChunkType::FileSignature),
            b'm' => Ok(ChunkType::FileMetadata),
            b'p' => Ok(ChunkType::Padding),
            b'r' => Ok(ChunkType::Simple),
            b't' => Ok(ChunkType::Transposed),
            _ => Err(RiegeliError::MalformedData(format!(
                "unknown chunk type byte: {byte:#04x}"
            ).into())),
        }
    }

    /// The `num_records` field extracted from the high 56 bits of `chunk_type_and_num_records`.
    pub fn num_records(&self) -> u64 {
        self.chunk_type_and_num_records >> 8
    }

    /// The `decoded_data_size` field: total uncompressed size of all records.
    pub fn decoded_data_size(&self) -> u64 {
        self.decoded_data_size
    }

    /// The `header_hash` as stored in this header.
    #[allow(dead_code)]
    pub fn stored_hash(&self) -> u64 {
        self.header_hash
    }

    /// Returns `true` if `stored_hash == highway_hash_64(bytes[8..40])`.
    pub fn is_header_valid(&self) -> bool {
        let bytes = self.to_bytes();
        let computed = highway_hash_64(&bytes[8..40]);
        self.header_hash == computed
    }

    /// Returns `true` if the stored `data_hash` matches `highway_hash_64(data)`.
    pub fn is_data_valid(&self, data: &[u8]) -> bool {
        self.data_hash == highway_hash_64(data)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_header() -> (ChunkHeader, Vec<u8>) {
        let data = b"hello, riegeli!".to_vec();
        let h = ChunkHeader::from_parts(&data, ChunkType::Simple, 1, data.len() as u64);
        (h, data)
    }

    #[test]
    fn from_parts_header_valid() {
        let (h, _) = make_header();
        assert!(h.is_header_valid());
    }

    #[test]
    fn from_parts_data_valid() {
        let (h, data) = make_header();
        assert!(h.is_data_valid(&data));
    }

    #[test]
    fn pack_unpack_chunk_type_and_num_records() {
        let data = b"test";
        let chunk_type = ChunkType::Simple;
        let num_records = 42u64;
        let h = ChunkHeader::from_parts(data, chunk_type, num_records, data.len() as u64);
        assert_eq!(h.chunk_type().unwrap(), chunk_type);
        assert_eq!(h.num_records(), num_records);
    }

    #[test]
    fn pack_unpack_large_num_records() {
        let data = b"";
        let num_records = (1u64 << 48) - 1;
        let h = ChunkHeader::from_parts(data, ChunkType::FileSignature, num_records, 0);
        assert_eq!(h.num_records(), num_records);
        assert_eq!(h.chunk_type().unwrap(), ChunkType::FileSignature);
    }

    #[test]
    fn round_trip() {
        let (original, _) = make_header();
        let bytes = original.to_bytes();
        let restored = ChunkHeader::from_bytes(bytes);
        assert_eq!(original, restored);
        assert!(restored.is_header_valid());
    }

    #[test]
    fn accessors() {
        let data = b"some data";
        let h = ChunkHeader::from_parts(data, ChunkType::Simple, 5, 100);
        assert_eq!(h.data_size(), data.len() as u64);
        assert_eq!(h.decoded_data_size(), 100);
        assert_eq!(h.num_records(), 5);
    }

    // -------------------------------------------------------------------------
    // Sprint 7 ChunkType TryFrom tests (moved from integration tests)
    // -------------------------------------------------------------------------

    #[test]
    fn chunk_type_try_from_unknown_returns_err() {
        use crate::error::RiegeliError;
        let unknown_bytes: &[u8] = &[0x00, 0x01, 0x7f, 0xfe, 0xff, b'x', b'a'];
        for &b in unknown_bytes {
            let result = ChunkType::try_from(b);
            assert!(
                matches!(result, Err(RiegeliError::UnknownChunkType(_))),
                "expected UnknownChunkType for byte {b:#04x}, got {result:?}"
            );
        }
    }

    // -------------------------------------------------------------------------
    // Sprint 17 ChunkHeader internal tests (moved from integration tests)
    // -------------------------------------------------------------------------

    #[test]
    fn chunk_type_file_metadata_recognized() {
        let h = ChunkHeader::from_parts(&[], ChunkType::FileMetadata, 0, 0);
        assert_eq!(h.chunk_type().unwrap(), ChunkType::FileMetadata);
    }

    /// criterion_2: A file written with set_metadata has a FileMetadata chunk at byte offset 64.
    #[test]
    fn criterion_2_metadata_chunk_at_offset_64() {
        use crate::constants::{BLOCK_HEADER_SIZE, CHUNK_HEADER_SIZE};
        use crate::record_writer::{RecordWriter, WriterOptions};
        use std::io::{Seek, SeekFrom, Write};

        struct VecWriter {
            data: Vec<u8>,
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
        impl Seek for VecWriter {
            fn seek(&mut self, _: SeekFrom) -> std::io::Result<u64> {
                Ok(self.data.len() as u64)
            }
        }

        let payload = b"schema-v1".to_vec();
        let mut w = VecWriter { data: Vec::new() };
        {
            let mut writer = RecordWriter::new(
                &mut w,
                WriterOptions::new().set_serialized_metadata(payload.clone()),
            )
            .expect("writer");
            writer.write_record(b"hello").expect("write");
            writer.close().expect("close");
        }
        let data = w.data;

        let metadata_chunk_pos = BLOCK_HEADER_SIZE + CHUNK_HEADER_SIZE; // = 64
        assert!(
            data.len() > metadata_chunk_pos as usize + 40,
            "file too short to contain metadata chunk"
        );

        let ch_bytes: [u8; 40] = data
            [metadata_chunk_pos as usize..metadata_chunk_pos as usize + 40]
            .try_into()
            .unwrap();
        let ch = ChunkHeader::from_bytes(ch_bytes);

        assert!(ch.is_header_valid(), "chunk header hash invalid");
        assert_eq!(
            ch.chunk_type().unwrap(),
            ChunkType::FileMetadata,
            "expected FileMetadata chunk at offset 64"
        );

        let data_start = metadata_chunk_pos as usize + 40;
        let data_end = data_start + ch.data_size() as usize;
        assert!(data.len() >= data_end, "file truncated");
        assert_eq!(&data[data_start..data_end], payload.as_slice());
    }

    /// criterion_8: check_file_format() does not decompress — only validates hashes.
    #[test]
    fn criterion_8_check_file_format_does_not_decompress() {
        use crate::hash::highway_hash_64;
        use crate::record_reader::{ReaderOptions, RecordReader};
        use crate::record_writer::{RecordWriter, WriterOptions};
        use std::io::{Cursor, Seek, SeekFrom, Write};

        struct VecWriter {
            data: Vec<u8>,
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
        impl Seek for VecWriter {
            fn seek(&mut self, _: SeekFrom) -> std::io::Result<u64> {
                Ok(self.data.len() as u64)
            }
        }

        let mut w = VecWriter { data: Vec::new() };
        {
            let mut writer = RecordWriter::new(&mut w, WriterOptions::new()).expect("writer");
            writer.write_record(b"hello").expect("write");
            writer.write_record(b"world").expect("write");
            writer.close().expect("close");
        }
        let mut tampered = w.data;

        let ch_start = 64usize;
        let ch_bytes: [u8; 40] = tampered[ch_start..ch_start + 40].try_into().unwrap();
        let ch = ChunkHeader::from_bytes(ch_bytes);

        let data_size = ch.data_size() as usize;
        let data_start = ch_start + 40;

        // Replace the chunk data with garbage bytes.
        let garbage: Vec<u8> = (0..data_size)
            .map(|i| (i as u8).wrapping_mul(17) ^ 0xAB)
            .collect();
        tampered[data_start..data_start + data_size].copy_from_slice(&garbage);

        // Recompute data_hash over the garbage.
        let new_data_hash = highway_hash_64(&garbage);
        let new_dh_bytes = new_data_hash.to_le_bytes();
        tampered[ch_start + 16..ch_start + 24].copy_from_slice(&new_dh_bytes);

        // Recompute header_hash over bytes [8..40] of the updated header.
        let header_body: [u8; 32] = tampered[ch_start + 8..ch_start + 40].try_into().unwrap();
        let new_header_hash = highway_hash_64(&header_body);
        let new_hh_bytes = new_header_hash.to_le_bytes();
        tampered[ch_start..ch_start + 8].copy_from_slice(&new_hh_bytes);

        // check_file_format() should succeed — it only validates hashes, not decompression.
        let mut reader =
            RecordReader::new(Cursor::new(tampered), ReaderOptions::new()).expect("reader");
        reader
            .check_file_format()
            .expect("check_file_format should succeed even with garbage-but-hash-valid data");
    }
}
