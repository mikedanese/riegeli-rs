//! Integration tests for RecordWriter and RecordReader: flush, empty records, multi-block files.

// Some imports are used only by feature-gated tests; in reduced-feature
// builds they would otherwise trip unused_imports.
#![cfg_attr(
    not(all(feature = "brotli", feature = "zstd", feature = "snappy")),
    allow(unused_imports)
)]
use riegeli::{CompressionType, ReaderOptions, RecordReader, RecordWriter, WriterOptions};

use std::io::{Cursor, Seek, SeekFrom, Write};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// A Write wrapper around Vec<u8>.
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

/// Build a file from records with given options, returning the raw bytes.
fn write_file(records: &[&[u8]], options: WriterOptions) -> Vec<u8> {
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

/// Read all records from a byte slice using the public RecordReader API.
fn read_all_records(file_data: Vec<u8>) -> Vec<Vec<u8>> {
    let mut reader =
        RecordReader::new(Cursor::new(file_data), ReaderOptions::new()).expect("reader ok");
    let mut records = Vec::new();
    while let Some(rec) = reader.read_record().expect("read_record") {
        records.push(rec);
    }
    records
}

// ---------------------------------------------------------------------------
// Adversarial probe: flush, write more, close
// ---------------------------------------------------------------------------

/// After flush(), write more records and close(): all records must be present.
#[test]
fn flush_write_more_close() {
    let mut w = VecWriter::new();
    {
        let mut writer = RecordWriter::new(&mut w, WriterOptions::new()).expect("new ok");
        writer.write_record(b"batch1_rec1").expect("ok");
        writer.write_record(b"batch1_rec2").expect("ok");
        writer.flush().expect("flush ok");

        writer.write_record(b"batch2_rec1").expect("ok");
        writer.write_record(b"batch2_rec2").expect("ok");
        writer.write_record(b"batch2_rec3").expect("ok");

        writer.close().expect("close ok");
    }
    let data_bytes = w.data.clone();

    let decoded = read_all_records(data_bytes);
    assert_eq!(decoded.len(), 5, "all 5 records must be present");
    assert_eq!(decoded[0], b"batch1_rec1");
    assert_eq!(decoded[1], b"batch1_rec2");
    assert_eq!(decoded[2], b"batch2_rec1");
    assert_eq!(decoded[3], b"batch2_rec2");
    assert_eq!(decoded[4], b"batch2_rec3");
}

// ---------------------------------------------------------------------------
// Adversarial probe: 10,000 Brotli records all decodable
// ---------------------------------------------------------------------------

/// Criterion 5.8 requires all records to be readable, not just block headers valid.
#[test]
#[cfg(feature = "brotli")]
fn ten_thousand_brotli_records_all_decodable() {
    let records: Vec<Vec<u8>> = (0..10_000u32)
        .map(|i| format!("record-{i:05}").into_bytes())
        .collect();
    let record_refs: Vec<&[u8]> = records.iter().map(|r| r.as_slice()).collect();

    let opts = WriterOptions::new().compression(CompressionType::Brotli);
    let data = write_file(&record_refs, opts);

    // Decode all records and verify content
    let mut reader = RecordReader::new(Cursor::new(&data), ReaderOptions::new()).expect("reader");
    let mut count = 0usize;
    while let Some(rec) = reader.read_record().expect("read_record") {
        let expected = format!("record-{count:05}");
        assert_eq!(rec, expected.as_bytes(), "record {count} content mismatch");
        count += 1;
    }
    assert_eq!(count, 10_000, "must decode all 10,000 records");

    // Verify non-zero data (compressed data should exist)
    assert!(data.len() > 64, "file must have data beyond header");
}

// ---------------------------------------------------------------------------
// Adversarial probe: single empty record (BUG FINDER)
// ---------------------------------------------------------------------------

/// Writing a single empty record b"" should produce a file with 1 record.
#[test]
fn single_empty_record() {
    let data = write_file(&[b""], WriterOptions::new());
    let decoded = read_all_records(data);
    assert_eq!(
        decoded.len(),
        1,
        "empty record must not be silently dropped"
    );
    assert_eq!(decoded[0], b"");
}

// ---------------------------------------------------------------------------
// Adversarial probe: multiple empty records (BUG FINDER)
// ---------------------------------------------------------------------------

/// Multiple empty records should all be present.
#[test]
fn multiple_empty_records() {
    let data = write_file(&[b"", b"", b""], WriterOptions::new());
    let decoded = read_all_records(data);
    assert_eq!(
        decoded.len(),
        3,
        "empty records must not be silently dropped"
    );
}

// ---------------------------------------------------------------------------
// Adversarial probe: mix of empty and non-empty records (BUG FINDER)
// ---------------------------------------------------------------------------

/// Empty records interleaved with non-empty should all be preserved.
#[test]
fn mixed_empty_and_nonempty_records() {
    let data = write_file(&[b"", b"hello", b"", b"world", b""], WriterOptions::new());
    let decoded = read_all_records(data);
    assert_eq!(decoded.len(), 5);
    assert_eq!(decoded[0], b"");
    assert_eq!(decoded[1], b"hello");
    assert_eq!(decoded[2], b"");
    assert_eq!(decoded[3], b"world");
    assert_eq!(decoded[4], b"");
}

// ---------------------------------------------------------------------------
// Adversarial probe: very small chunk_size forces many chunks
// ---------------------------------------------------------------------------

#[test]
fn tiny_chunk_size_many_chunks() {
    let records: Vec<&[u8]> = (0..50).map(|_| b"x" as &[u8]).collect();
    let opts = WriterOptions::new().chunk_size(1);
    let data = write_file(&records, opts);

    let decoded = read_all_records(data);
    assert_eq!(decoded.len(), 50);
    for rec in &decoded {
        assert_eq!(rec, b"x");
    }
}

// ---------------------------------------------------------------------------
// Adversarial probe: multiple flushes produce consistent file
// ---------------------------------------------------------------------------

#[test]
fn multiple_flushes() {
    let mut w = VecWriter::new();
    {
        let mut writer = RecordWriter::new(&mut w, WriterOptions::new()).expect("new ok");
        writer.write_record(b"a").expect("ok");
        writer.flush().expect("ok");
        writer.write_record(b"b").expect("ok");
        writer.flush().expect("ok");
        writer.write_record(b"c").expect("ok");
        writer.flush().expect("ok");
    }
    let decoded = read_all_records(w.data);
    assert_eq!(decoded.len(), 3);
    assert_eq!(decoded[0], b"a");
    assert_eq!(decoded[1], b"b");
    assert_eq!(decoded[2], b"c");
}

// ---------------------------------------------------------------------------
// Adversarial probe: flush with no pending data is a no-op
// ---------------------------------------------------------------------------

#[test]
fn flush_with_no_data_is_noop() {
    let mut w = VecWriter::new();
    {
        let mut writer = RecordWriter::new(&mut w, WriterOptions::new()).expect("new ok");
        writer.flush().expect("ok");
        writer.flush().expect("ok");
        writer.write_record(b"hello").expect("ok");
        writer.flush().expect("ok");
        writer.flush().expect("ok");
    }
    let decoded = read_all_records(w.data);
    assert_eq!(decoded.len(), 1);
    assert_eq!(decoded[0], b"hello");
}

// ---------------------------------------------------------------------------
// Adversarial probe: only empty records, flushed explicitly
// ---------------------------------------------------------------------------

/// If the only records written are empty, flush() should still produce them.
#[test]
fn only_empty_records_flushed() {
    let mut w = VecWriter::new();
    {
        let mut writer = RecordWriter::new(&mut w, WriterOptions::new()).expect("new ok");
        writer.write_record(b"").expect("ok");
        writer.write_record(b"").expect("ok");
        writer.flush().expect("ok");
    }
    let decoded = read_all_records(w.data);
    assert_eq!(decoded.len(), 2, "empty records must survive flush");
}
