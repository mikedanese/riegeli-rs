//! Drain a RecordReader over arbitrary bytes — the surface where the
//! unknown-chunk infinite loop, the straddling-header read failures, and
//! the hostile-header allocation/overflow bombs all lived.
//!
//! Run with `-max_len=262144` (or larger): block headers and straddling
//! chunk headers — the interesting region — start at the 64 KiB boundary.
//! Hangs are caught by libFuzzer's -timeout.
#![no_main]
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if data.is_empty() {
        return;
    }
    // The LAST input byte selects reader options, so the file prefix stays
    // byte-aligned with real riegeli files (corpus seeds keep their
    // meaning). Recovery mode matters: its boundary-skip scan loop is
    // itself hostile-input surface the default options never reach.
    let (file, opt_byte) = data.split_at(data.len() - 1);
    let mut options = riegeli::ReaderOptions::new();
    if opt_byte[0] & 1 != 0 {
        // Assert the callback invariants the coupled design guarantees:
        // nonempty regions that only move forward.
        let last_end = std::cell::Cell::new(0u64);
        options = options.recovery(move |region| {
            assert!(region.begin() < region.end(), "empty skipped region");
            assert!(region.begin() >= last_end.get(), "regions moved backward");
            last_end.set(region.end());
            true
        });
    }
    let cursor = std::io::Cursor::new(file.to_vec());
    let Ok(mut reader) = riegeli::RecordReader::new(cursor, options) else {
        return;
    };
    // With recovery enabled the reader skips corruption instead of
    // erroring, so bound the drain by the input size (each record consumes
    // at least one file byte) rather than trusting termination.
    let mut budget = data.len() + 1;
    while let Ok(Some(_)) = reader.read_record() {
        budget -= 1;
        if budget == 0 {
            break;
        }
    }
});
