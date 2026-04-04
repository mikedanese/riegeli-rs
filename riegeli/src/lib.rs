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
pub(crate) mod transpose;
pub mod varint;

// ── Generated protobuf ────────────────────────────────────────────────────────
#[allow(clippy::all)]
pub mod proto_generated {
    include!(concat!(
        env!("OUT_DIR"),
        "/protobuf_generated/google/protobuf/generated.rs"
    ));
}

// ── Public re-exports — the complete public API ───────────────────────────────
pub use compression::{CompressOptions, CompressionType};
pub use error::RiegeliError;
pub use field_projection::{Field, FieldProjection};
pub use proto_generated::RecordsMetadata;
pub use record_position::RecordPosition;
pub use record_reader::{ReaderOptions, RecordReader};
pub use record_writer::{RecordWriter, WriterOptions};
