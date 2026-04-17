//! Error types for the Riegeli crate.

use std::borrow::Cow;

/// A decoded error from chunk operations.
#[derive(Debug)]
pub enum RiegeliError {
    /// The chunk data hash did not match the stored hash in the header.
    DataHashMismatch,
    /// The chunk data is malformed or truncated.
    MalformedData(Cow<'static, str>),
    /// The compression type byte is not supported (feature not enabled or unknown).
    UnsupportedCompression(u8),
    /// The writer has been closed; no further writes are accepted.
    WriterClosed,
    /// An I/O error occurred.
    IoError(std::io::Error),
    /// An unrecognized `ChunkType` byte was encountered.
    UnknownChunkType(u8),
    /// An unrecognized `CompressionType` byte was encountered.
    UnknownCompressionType(u8),
}

impl std::fmt::Display for RiegeliError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RiegeliError::DataHashMismatch => {
                write!(f, "chunk data hash mismatch: data is corrupted")
            }
            RiegeliError::MalformedData(msg) => write!(f, "malformed chunk data: {msg}"),
            RiegeliError::UnsupportedCompression(byte) => {
                write!(f, "unsupported compression type byte: {byte:#04x}")
            }
            RiegeliError::WriterClosed => write!(f, "writer has been closed"),
            RiegeliError::IoError(e) => write!(f, "I/O error: {e}"),
            RiegeliError::UnknownChunkType(byte) => {
                write!(f, "unknown chunk type byte: {byte:#04x}")
            }
            RiegeliError::UnknownCompressionType(byte) => {
                write!(f, "unknown compression type byte: {byte:#04x}")
            }
        }
    }
}

impl std::error::Error for RiegeliError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            RiegeliError::IoError(e) => Some(e),
            _ => None,
        }
    }
}

impl From<std::io::Error> for RiegeliError {
    fn from(e: std::io::Error) -> Self {
        RiegeliError::IoError(e)
    }
}
