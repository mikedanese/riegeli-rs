//! Pure-Rust implementation of the Riegeli/records file format.
//!
//! Riegeli is a high-performance, seekable, compressed record store used in
//! machine learning and data pipelines. This crate provides byte-level
//! interoperability with the C++ reference implementation.

// ── Public API modules ────────────────────────────────────────────────────────
pub(crate) mod compression;
pub(crate) mod error;
pub(crate) mod field_projection;
pub(crate) mod record_position;
pub(crate) mod record_reader;
pub(crate) mod record_writer;

// ── Implementation detail modules ─────────────────────────────────────────────
pub(crate) mod block_arithmetic;
pub(crate) mod block_header;
pub(crate) mod chunk_header;
pub(crate) mod constants;
pub(crate) mod hash;
pub mod proto;
pub(crate) mod simple_chunk;
pub(crate) mod skipped_region;
pub(crate) mod transpose;
pub mod varint;

// ── Generated protobuf ────────────────────────────────────────────────────────
// Checked-in protoc output — see scripts/regen-proto.sh to regenerate.
#[allow(clippy::all)]
pub mod proto_generated {
    include!("proto_generated/google/protobuf/generated.rs");
}

// ── Fuzz entry points (cargo-fuzz builds only) ────────────────────────────────
// cargo-fuzz passes `--cfg fuzzing`; this module exposes crate-private
// decoder paths to the fuzz targets in fuzz/. It is not part of the public
// API and cannot be enabled by a downstream crate.
#[cfg(fuzzing)]
pub mod fuzz {
    /// Decode an arbitrary transposed chunk and drain it. The first four
    /// input bytes pick the header's claimed num_records and
    /// decoded_data_size so the fuzzer can explore their interaction with
    /// the chunk body.
    pub fn transpose_decode(data: &[u8]) {
        if data.len() < 4 {
            return;
        }
        let num_records = u16::from_le_bytes([data[0], data[1]]) as u64;
        let decoded_data_size = u16::from_le_bytes([data[2], data[3]]) as u64;
        let body = &data[4..];
        let header = crate::chunk_header::ChunkHeader::from_parts(
            body,
            crate::chunk_header::ChunkType::Transposed,
            num_records,
            decoded_data_size,
        );
        let chunk = crate::simple_chunk::Chunk { header, data: body.to_vec() };
        if let Ok(mut decoder) =
            crate::transpose::decoder::TransposeChunkDecoder::new_with_projection(chunk, None)
        {
            while let Ok(Some(_)) = decoder.read_record() {}
        }
    }
}

// ── Public re-exports — the complete public API ───────────────────────────────
pub use compression::{CompressOptions, CompressionType};
pub use error::RiegeliError;
pub use field_projection::{Field, FieldProjection};
pub use proto_generated::RecordsMetadata;
pub use record_position::RecordPosition;
pub use skipped_region::SkippedRegion;
pub use record_reader::{ReaderOptions, RecordReader};
pub use record_writer::{RecordWriter, WriterOptions};
