//! Shared test helpers for cross-language round-trip testing.
//!
//! Shared helpers used by recovery and transpose roundtrip tests.

use std::io::Cursor;

use riegeli::{ReaderOptions, RecordReader, RecordWriter, WriterOptions as RustWriterOptions};
use riegeli_ffi::{
    RecordReader as FfiReader, RecordWriter as FfiWriter, WriterOptions as FfiWriterOptions,
};

// ---------------------------------------------------------------------------
// Record generation helpers
// ---------------------------------------------------------------------------

/// Generate `n` small ASCII records of the form `"record-000000"`.
pub fn make_small_records(n: usize) -> Vec<Vec<u8>> {
    (0..n)
        .map(|i| format!("record-{i:06}").into_bytes())
        .collect()
}

/// Generate `n` records of exactly `size` bytes filled with repeating `(i % 256) as u8`.
pub fn make_large_records(n: usize, size: usize) -> Vec<Vec<u8>> {
    (0..n).map(|i| vec![(i % 256) as u8; size]).collect()
}

// ---------------------------------------------------------------------------
// Core helper
// ---------------------------------------------------------------------------

/// Write `records` using `write_fn`, then read them back using `read_fn` and
/// assert byte-for-byte equality.
///
/// Both closures operate on raw `Vec<u8>` riegeli file bytes so the helper is
/// agnostic to whether the writer or reader is Rust or C++, and whether the
/// encoding is simple or transpose.
pub fn cross_lang_roundtrip<W, R>(label: &str, records: &[Vec<u8>], write_fn: W, read_fn: R)
where
    W: FnOnce(&[Vec<u8>]) -> Vec<u8>,
    R: FnOnce(&[u8]) -> Vec<Vec<u8>>,
{
    let file_bytes = write_fn(records);
    assert!(
        !file_bytes.is_empty(),
        "{label}: writer produced empty output"
    );

    let got = read_fn(&file_bytes);
    assert_eq!(
        got.len(),
        records.len(),
        "{label}: expected {} records, got {}",
        records.len(),
        got.len()
    );
    for (idx, (expected, actual)) in records.iter().zip(got.iter()).enumerate() {
        assert_eq!(actual, expected, "{label}: record {idx} mismatch");
    }
}

// ---------------------------------------------------------------------------
// Write-side closures
// ---------------------------------------------------------------------------

/// Rust writer with arbitrary options.
pub fn rust_write(records: &[Vec<u8>], opts: RustWriterOptions) -> Vec<u8> {
    let mut buf = Cursor::new(Vec::<u8>::new());
    {
        let mut w = RecordWriter::new(&mut buf, opts).expect("rust writer ok");
        for rec in records {
            w.write_record(rec).expect("rust write_record ok");
        }
        w.flush().expect("rust flush ok");
    }
    buf.into_inner()
}

/// C++ writer (via FFI) with arbitrary options.
pub fn cpp_write(records: &[Vec<u8>], opts: FfiWriterOptions) -> Vec<u8> {
    let mut w = FfiWriter::new(opts).expect("cpp writer ok");
    for rec in records {
        w.write_record(rec).expect("cpp write_record ok");
    }
    w.close().expect("cpp writer close ok")
}

// ---------------------------------------------------------------------------
// Read-side closures
// ---------------------------------------------------------------------------

/// Rust reader.
pub fn rust_read(data: &[u8]) -> Vec<Vec<u8>> {
    let mut reader =
        RecordReader::new(Cursor::new(data), ReaderOptions::new()).expect("rust reader ok");
    let mut out = Vec::new();
    while let Some(rec) = reader.read_record().expect("rust read_record ok") {
        out.push(rec);
    }
    out
}

/// C++ reader (via FFI).
pub fn cpp_read(data: &[u8]) -> Vec<Vec<u8>> {
    let mut reader = FfiReader::new(data).expect("cpp reader ok");
    let mut out = Vec::new();
    while let Some(rec) = reader.read_record().expect("cpp read_record ok") {
        out.push(rec);
    }
    reader.close().expect("cpp reader close ok");
    out
}
