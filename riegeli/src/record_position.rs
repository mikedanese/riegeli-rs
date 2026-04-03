//! `RecordPosition` — a logical position within a Riegeli file.
//!
//! A position combines a chunk's file offset with a record index within that chunk,
//! uniquely identifying any record in the file.

use std::fmt;
use std::str::FromStr;

use crate::error::RiegeliError;

/// A logical position within a Riegeli file identifying a specific record.
///
/// The position is represented as the file offset of the chunk that contains
/// the record (`chunk_begin`) and the zero-based index of the record within
/// that chunk (`record_index`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct RecordPosition {
    /// File offset of the chunk containing this record.
    pub chunk_begin: u64,
    /// Zero-based index of the record within its chunk.
    pub record_index: u64,
}

impl RecordPosition {
    /// Create a new `RecordPosition`.
    pub fn new(chunk_begin: u64, record_index: u64) -> Self {
        Self {
            chunk_begin,
            record_index,
        }
    }

    /// Returns a single numeric position suitable for ordering and seeking.
    ///
    /// Computed as `chunk_begin + record_index`.
    pub fn numeric(&self) -> u64 {
        self.chunk_begin + self.record_index
    }
}

impl fmt::Display for RecordPosition {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.chunk_begin, self.record_index)
    }
}

impl FromStr for RecordPosition {
    type Err = RiegeliError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let (chunk_str, index_str) = s.split_once('/').ok_or_else(|| {
            RiegeliError::MalformedData(format!(
                "invalid RecordPosition format (expected 'chunk/index'): {s}"
            ))
        })?;
        let chunk_begin = chunk_str.parse::<u64>().map_err(|e| {
            RiegeliError::MalformedData(format!("invalid chunk_begin in RecordPosition: {e}"))
        })?;
        let record_index = index_str.parse::<u64>().map_err(|e| {
            RiegeliError::MalformedData(format!("invalid record_index in RecordPosition: {e}"))
        })?;
        Ok(Self {
            chunk_begin,
            record_index,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numeric() {
        let pos = RecordPosition::new(100, 5);
        assert_eq!(pos.numeric(), 105);
    }

    #[test]
    fn display() {
        let pos = RecordPosition::new(24, 0);
        assert_eq!(pos.to_string(), "24/0");
        let pos2 = RecordPosition::new(65560, 42);
        assert_eq!(pos2.to_string(), "65560/42");
    }

    #[test]
    fn from_str_roundtrip() {
        let pos = RecordPosition::new(12345, 67);
        let s = pos.to_string();
        let parsed: RecordPosition = s.parse().expect("parse ok");
        assert_eq!(parsed, pos);
    }

    #[test]
    fn from_str_invalid() {
        assert!("no-slash".parse::<RecordPosition>().is_err());
        assert!("abc/0".parse::<RecordPosition>().is_err());
        assert!("0/abc".parse::<RecordPosition>().is_err());
    }
}
