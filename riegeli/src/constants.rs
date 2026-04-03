//! Format constants for the Riegeli/records file format.

/// Size of a Riegeli block in bytes.
pub(crate) const BLOCK_SIZE: u64 = 65536;

/// Size of a block header in bytes.
pub(crate) const BLOCK_HEADER_SIZE: u64 = 24;

/// Size of a chunk header in bytes.
pub(crate) const CHUNK_HEADER_SIZE: u64 = 40;
