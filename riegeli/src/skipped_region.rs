//! A contiguous region of the file skipped during recovery.

use std::fmt;

/// A region of invalid file content skipped by the recovery mechanism.
///
/// Mirrors the C++ `riegeli::SkippedRegion`: a half-open byte range
/// `[begin, end)` in canonical file positions, plus a human-readable message
/// explaining why the region was invalid.
///
/// `end` is exactly where the reader resumes after the region is skipped:
/// the region reported to the recovery callback and the actual resync
/// position are the same value by construction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkippedRegion {
    begin: u64,
    end: u64,
    message: String,
}

impl SkippedRegion {
    /// Creates a `SkippedRegion` over `[begin, end)` with a message
    /// explaining why the region is invalid.
    pub(crate) fn new(begin: u64, end: u64, message: String) -> Self {
        Self {
            begin,
            end,
            message,
        }
    }

    /// File position of the beginning of the skipped region, inclusive.
    pub fn begin(&self) -> u64 {
        self.begin
    }

    /// File position of the end of the skipped region, exclusive. This is
    /// where the reader resumes.
    pub fn end(&self) -> u64 {
        self.end
    }

    /// Length of the skipped region, in bytes.
    pub fn length(&self) -> u64 {
        self.end - self.begin
    }

    /// Message explaining why the region is invalid.
    pub fn message(&self) -> &str {
        &self.message
    }
}

impl fmt::Display for SkippedRegion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "[{}..{}): {}", self.begin, self.end, self.message)
    }
}
