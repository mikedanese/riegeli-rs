//! Integration tests for RecordReader edge cases: empty files, metadata, missing metadata.

// Some imports are used only by feature-gated tests; in reduced-feature
// builds they would otherwise trip unused_imports.
#![cfg_attr(
    not(all(feature = "brotli", feature = "zstd", feature = "snappy")),
    allow(unused_imports)
)]
use std::io::Cursor;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};

use riegeli::{CompressionType, ReaderOptions, RecordReader, RecordWriter, WriterOptions};

fn write_records(records: &[&[u8]], opts: WriterOptions) -> Vec<u8> {
    let mut buf = Cursor::new(Vec::<u8>::new());
    {
        let mut w = RecordWriter::new(&mut buf, opts).expect("writer new");
        for rec in records {
            w.write_record(rec).expect("write_record");
        }
        w.close().expect("close");
    }
    buf.into_inner()
}

fn open_reader(data: Vec<u8>) -> RecordReader<Cursor<Vec<u8>>> {
    RecordReader::new(Cursor::new(data), ReaderOptions::new()).expect("reader new")
}

// ── A: read_metadata() on a file with no metadata returns None ──────────────
#[test]
fn adv_a_read_metadata_returns_none_when_absent() {
    let data = write_records(&[b"hello", b"world"], WriterOptions::new());
    let mut reader = open_reader(data);
    let meta = reader
        .read_serialized_metadata()
        .expect("read_metadata should not error");
    assert_eq!(
        meta, None,
        "read_metadata on no-metadata file should return None"
    );
}

// ── B: size() on file with metadata chunk counts only data records ────────────
#[test]
fn adv_b_size_excludes_metadata_chunk() {
    // Write 5 records with metadata; size() should still return 5.
    let records: Vec<&[u8]> = vec![b"a", b"b", b"c", b"d", b"e"];
    let data = write_records(
        &records,
        WriterOptions::new().set_serialized_metadata(b"schema".to_vec()),
    );
    let mut reader = open_reader(data);
    let count = reader.size().expect("size");
    assert_eq!(
        count, 5,
        "size() should count only data records, not metadata"
    );
}

// ── C: check_file_format() called mid-read doesn't break subsequent reads ────
#[test]
fn adv_c_check_file_format_mid_read_preserves_state() {
    let records: Vec<Vec<u8>> = (0u32..5).map(|i| i.to_le_bytes().to_vec()).collect();
    let record_refs: Vec<&[u8]> = records.iter().map(|v| v.as_slice()).collect();
    let data = write_records(&record_refs, WriterOptions::new());
    let mut reader = open_reader(data);

    // Read 2 records.
    let rec0 = reader.read_record().expect("read 0").expect("got record 0");
    let rec1 = reader.read_record().expect("read 1").expect("got record 1");
    assert_eq!(rec0, records[0]);
    assert_eq!(rec1, records[1]);

    // Call check_file_format() mid-read.
    reader
        .check_file_format()
        .expect("check_file_format should succeed");

    // The next read should return record 2 (not 0 or None).
    let rec2 = reader.read_record().expect("read 2").expect("got record 2");
    assert_eq!(
        rec2, records[2],
        "check_file_format should not disrupt subsequent reads"
    );

    // Remaining records should read correctly.
    let rec3 = reader.read_record().expect("read 3").expect("got record 3");
    let rec4 = reader.read_record().expect("read 4").expect("got record 4");
    assert_eq!(rec3, records[3]);
    assert_eq!(rec4, records[4]);
    assert_eq!(reader.read_record().expect("eof"), None);
}

// ── D: seek_back() twice in a row stays on the last-read record ──────────────
#[test]
fn adv_d_seek_back_twice_stays_on_same_record() {
    let records: Vec<Vec<u8>> = (0u32..5).map(|i| i.to_le_bytes().to_vec()).collect();
    let record_refs: Vec<&[u8]> = records.iter().map(|v| v.as_slice()).collect();
    let data = write_records(&record_refs, WriterOptions::new());
    let mut reader = open_reader(data);

    // Read 3 records (records 0, 1, 2).
    for _ in 0..3 {
        reader.read_record().expect("read").expect("got record");
    }

    // seek_back() once: repositions at record 2.
    let r1 = reader.seek_back().expect("seek_back 1");
    assert!(r1, "first seek_back should return true");

    // seek_back() again: last_pos after the first seek is still record 2.
    let r2 = reader.seek_back().expect("seek_back 2");
    assert!(r2, "second seek_back should return true");

    // The next read should return record 2 again.
    let re_read = reader.read_record().expect("re-read").expect("got record");
    assert_eq!(
        re_read, records[2],
        "double seek_back should re-read record 2"
    );
}

// ── E: seek_back() after reading the first record returns Ok(true) ────────────
#[test]
fn adv_e_seek_back_after_first_record() {
    // seek_back() positions at the last record read; after reading record 0,
    // seek_back() returns Ok(true) and re-reads record 0.
    let records: Vec<Vec<u8>> = (0u32..3).map(|i| i.to_le_bytes().to_vec()).collect();
    let record_refs: Vec<&[u8]> = records.iter().map(|v| v.as_slice()).collect();
    let data = write_records(&record_refs, WriterOptions::new());
    let mut reader = open_reader(data);

    // Read the first record.
    let first = reader
        .read_record()
        .expect("read first")
        .expect("got first");
    assert_eq!(first, records[0]);

    // seek_back() after the first record should return Ok(true) and re-read record 0.
    let result = reader.seek_back().expect("seek_back");
    assert!(
        result,
        "seek_back after first record should return Ok(true)"
    );

    let re_read = reader
        .read_record()
        .expect("re-read first")
        .expect("got re-read");
    assert_eq!(
        re_read, records[0],
        "seek_back after first record re-reads record 0"
    );
}

// ── F: size() preserves last_record_is_valid flag ────────────────────────────
#[test]
fn adv_f_size_preserves_last_record_is_valid() {
    // Use a file with a corrupted chunk to trigger recovery.
    let records: Vec<Vec<u8>> = (0u32..5).map(|i| i.to_le_bytes().to_vec()).collect();
    let record_refs: Vec<&[u8]> = records.iter().map(|v| v.as_slice()).collect();

    let mut raw = write_records(&record_refs, WriterOptions::new().chunk_size(64));
    // Corrupt a byte in the chunk data (first data chunk at 64, header is 40 bytes).
    let corrupt_at = 64 + 40 + 1;
    if corrupt_at < raw.len() {
        raw[corrupt_at] ^= 0xFF;
    }

    let recovery_fired = Arc::new(AtomicBool::new(false));
    let recovery_clone = recovery_fired.clone();
    let opts = ReaderOptions::new().recovery(move |_region| {
        recovery_clone.store(true, Ordering::SeqCst);
        true
    });

    let mut reader = RecordReader::new(Cursor::new(raw), opts).expect("reader new");

    // Attempt to read — recovery fires.
    let _ = reader.read_record();

    // If recovery fired, last_record_is_valid should be false.
    if recovery_fired.load(Ordering::SeqCst) {
        assert!(
            !reader.last_record_is_valid(),
            "last_record_is_valid should be false after recovery"
        );

        // Call size() — it should preserve last_record_is_valid.
        let _ = reader.size();
        assert!(
            !reader.last_record_is_valid(),
            "size() should not change last_record_is_valid"
        );
    }
}

// ── G: check_file_format() on a file with corrupted block header fails at open ──
#[test]
fn adv_g_check_file_format_detects_bad_block_header() {
    // Corrupt the initial block header hash (bytes [0..8]).
    let data = write_records(&[b"test"], WriterOptions::new());
    let mut corrupted = data.clone();
    corrupted[0] ^= 0xFF;

    // Opening a reader validates the block header — should fail at construction.
    let reader_result = RecordReader::new(Cursor::new(corrupted), ReaderOptions::new());
    assert!(
        reader_result.is_err(),
        "opening reader on corrupted block header should fail"
    );
}

// ── H: read_metadata() works with Brotli-compressed data file ────────────────
#[test]
#[cfg(feature = "brotli")]
fn adv_h_read_metadata_with_brotli_records() {
    let meta = b"brotli-schema-v1".to_vec();
    let data = write_records(
        &[b"compressed record"],
        WriterOptions::new()
            .set_serialized_metadata(meta.clone())
            .compression(CompressionType::Brotli),
    );
    let mut reader = open_reader(data);
    let got = reader.read_serialized_metadata().expect("read_metadata");
    assert_eq!(
        got,
        Some(meta),
        "read_metadata should work with Brotli-compressed files"
    );

    // Records should also be readable.
    let rec = reader
        .read_record()
        .expect("read record")
        .expect("got record");
    assert_eq!(rec, b"compressed record");
}

// ── I: size() called before any read — BUG: corrupts reader state ────────────
//
// This test documents a real bug in the size() implementation:
// When size() is called before any reads, it restores position by calling
// seek(initial_pos) where initial_pos = {chunk_begin: 24, record_index: 0}.
// seek() calls load_chunk_at(24) which loads the FileSignature chunk and
// returns Ok(None), causing seek() to set at_eof = true.
// After size(), all subsequent read_record() calls return None (EOF).
//
// Expected: size() should not change the read position.
// Actual: size() corrupts the reader by setting at_eof = true.
#[test]
fn adv_i_size_before_any_read_bug() {
    let records: Vec<Vec<u8>> = (0u32..42).map(|i| i.to_le_bytes().to_vec()).collect();
    let record_refs: Vec<&[u8]> = records.iter().map(|v| v.as_slice()).collect();
    let data = write_records(&record_refs, WriterOptions::new());
    let mut reader = open_reader(data);

    // Call size() before reading anything — should return 42.
    let count = reader.size().expect("size");
    assert_eq!(count, 42, "size() before any read should return 42");

    // After size(), reads should start from the beginning.
    // BUG: This currently returns None because size() sets at_eof = true.
    let first = reader.read_record().expect("read");
    assert_eq!(
        first,
        Some(records[0].clone()),
        "first read after size() should return record 0, not EOF (size() must preserve reader state)"
    );
}

// ── J: read_metadata() does not advance the read position ────────────────────
#[test]
fn adv_j_read_metadata_does_not_advance_position() {
    let meta = b"schema".to_vec();
    let records = vec![b"rec0".as_ref(), b"rec1".as_ref(), b"rec2".as_ref()];
    let data = write_records(
        &records,
        WriterOptions::new().set_serialized_metadata(meta.clone()),
    );
    let mut reader = open_reader(data);

    // Call read_metadata() twice — should return same thing each time.
    let m1 = reader
        .read_serialized_metadata()
        .expect("first read_metadata");
    let m2 = reader
        .read_serialized_metadata()
        .expect("second read_metadata");
    assert_eq!(m1, Some(meta.clone()));
    assert_eq!(m2, Some(meta));

    // Records should still be readable in order from the beginning.
    for (i, expected) in records.iter().enumerate() {
        let got = reader
            .read_record()
            .unwrap_or_else(|_| panic!("read {i}"))
            .expect("got record");
        assert_eq!(
            got, *expected,
            "record {i} should match after repeated read_metadata"
        );
    }
    assert_eq!(reader.read_record().expect("eof"), None);
}
