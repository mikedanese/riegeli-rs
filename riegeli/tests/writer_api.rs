//! Integration tests for RecordWriter API and SetFieldProjection edge cases.

use std::cmp::Ordering;
use std::io::Cursor;

use riegeli::{
    CompressionType, Field, FieldProjection, ReaderOptions, RecordReader, RecordWriter,
    WriterOptions,
};

fn write_records(records: &[Vec<u8>], opts: WriterOptions) -> Vec<u8> {
    let mut buf = Cursor::new(Vec::<u8>::new());
    {
        let mut w = RecordWriter::new(&mut buf, opts).expect("writer new ok");
        for rec in records {
            w.write_record(rec).expect("write ok");
        }
        w.flush().expect("flush ok");
    }
    buf.into_inner()
}

fn encode_sorted_record(value: u64) -> Vec<u8> {
    value.to_be_bytes().to_vec()
}

fn parse_sorted_record(record: &[u8]) -> Option<u64> {
    if record.len() == 8 {
        Some(u64::from_be_bytes(record.try_into().unwrap()))
    } else {
        None
    }
}

fn compare_record(record: &[u8], target: u64) -> Ordering {
    match parse_sorted_record(record) {
        Some(v) => v.cmp(&target),
        None => Ordering::Less,
    }
}

// ---------------------------------------------------------------------------
// Adversarial: single-record file
// ---------------------------------------------------------------------------

/// Search in a file with exactly 1 record. The target exists.
#[test]
fn adv_search_single_record_found() {
    let data = write_records(
        &[encode_sorted_record(42)],
        WriterOptions::new().compression(CompressionType::None),
    );

    let mut reader = RecordReader::new(Cursor::new(&data), ReaderOptions::new()).expect("ok");
    let found = reader.search(|rec| compare_record(rec, 42)).expect("ok");
    assert!(found, "single-record file: should find record 42");

    let next = reader
        .read_record()
        .expect("ok")
        .expect("should have record");
    assert_eq!(
        parse_sorted_record(&next),
        Some(42),
        "next read after search should return 42"
    );
}

/// Search in a file with exactly 1 record. The target does NOT exist (too small).
#[test]
fn adv_search_single_record_not_found_less() {
    let data = write_records(
        &[encode_sorted_record(42)],
        WriterOptions::new().compression(CompressionType::None),
    );

    let mut reader = RecordReader::new(Cursor::new(&data), ReaderOptions::new()).expect("ok");
    // Target 10 is less than the only record (42), so search should return false.
    let found = reader.search(|rec| compare_record(rec, 10)).expect("ok");
    assert!(
        !found,
        "single-record file: should not find record 10 (only record is 42)"
    );

    let next = reader.read_record().expect("ok");
    assert!(
        next.is_none(),
        "reader should be at EOF after failed search"
    );
}

/// Search in a file with exactly 1 record. The target does NOT exist (too large).
#[test]
fn adv_search_single_record_not_found_greater() {
    let data = write_records(
        &[encode_sorted_record(42)],
        WriterOptions::new().compression(CompressionType::None),
    );

    let mut reader = RecordReader::new(Cursor::new(&data), ReaderOptions::new()).expect("ok");
    // Target 100 is greater than the only record (42), so search should return false.
    let found = reader.search(|rec| compare_record(rec, 100)).expect("ok");
    assert!(
        !found,
        "single-record file: should not find record 100 (only record is 42)"
    );

    let next = reader.read_record().expect("ok");
    assert!(
        next.is_none(),
        "reader should be at EOF after failed search"
    );
}

// ---------------------------------------------------------------------------
// Adversarial: search after having already read some records
// ---------------------------------------------------------------------------

/// search must re-scan the entire file even if we've already read past some records.
#[test]
fn adv_search_after_partial_read() {
    let opts = WriterOptions::new()
        .compression(CompressionType::None)
        .chunk_size(256);
    let records: Vec<Vec<u8>> = (0..100u64).map(encode_sorted_record).collect();
    let data = write_records(&records, opts);

    let mut reader = RecordReader::new(Cursor::new(&data), ReaderOptions::new()).expect("ok");

    // Read the first 10 records, advancing the reader well past the start.
    for _ in 0..10 {
        reader
            .read_record()
            .expect("ok")
            .expect("should have record");
    }

    // Now search for record 5 — which is BEFORE the current position.
    // search must go back to the beginning.
    let found = reader.search(|rec| compare_record(rec, 5)).expect("ok");
    assert!(
        found,
        "search should find record 5 even after reading past it"
    );

    let next = reader
        .read_record()
        .expect("ok")
        .expect("should have record");
    assert_eq!(
        parse_sorted_record(&next),
        Some(5),
        "next read after search(5) should return 5"
    );
}

// ---------------------------------------------------------------------------
// Adversarial: search on file with metadata chunk
// ---------------------------------------------------------------------------

/// search must skip the FileMetadata chunk and only find record data.
#[test]
fn adv_search_with_metadata_chunk() {
    let opts = WriterOptions::new()
        .compression(CompressionType::None)
        .set_serialized_metadata(b"schema-v1".to_vec());

    let records: Vec<Vec<u8>> = (0..50u64).map(encode_sorted_record).collect();
    let data = write_records(&records, opts);

    let mut reader = RecordReader::new(Cursor::new(&data), ReaderOptions::new()).expect("ok");

    let found = reader.search(|rec| compare_record(rec, 25)).expect("ok");
    assert!(found, "search should find record 25 despite metadata chunk");

    let next = reader
        .read_record()
        .expect("ok")
        .expect("should have record");
    assert_eq!(
        parse_sorted_record(&next),
        Some(25),
        "next read after search should return 25"
    );
}

// ---------------------------------------------------------------------------
// Adversarial: two-chunk file — value in second chunk
// ---------------------------------------------------------------------------

/// With two chunks and target in the second chunk, the outer binary search
/// must correctly fall through to the within-chunk search of chunk[1].
#[test]
fn adv_search_two_chunk_target_in_second() {
    // Each chunk holds ~4 records (chunk_size = 32 bytes, 8 bytes/record).
    let opts = WriterOptions::new()
        .compression(CompressionType::None)
        .chunk_size(32);
    let records: Vec<Vec<u8>> = (0..8u64).map(encode_sorted_record).collect();
    let data = write_records(&records, opts);

    let mut reader = RecordReader::new(Cursor::new(&data), ReaderOptions::new()).expect("ok");

    // Search for the last record — it should be in the second (or later) chunk.
    let found = reader.search(|rec| compare_record(rec, 7)).expect("ok");
    assert!(found, "should find record 7");

    let next = reader
        .read_record()
        .expect("ok")
        .expect("should have record");
    assert_eq!(parse_sorted_record(&next), Some(7), "next read should be 7");
}

/// Search for a value between first-records of two adjacent chunks.
#[test]
fn adv_search_value_between_chunk_pivots() {
    // Force 3 records per chunk → chunks contain [0,1,2], [3,4,5], [6,7,8,9].
    let opts = WriterOptions::new()
        .compression(CompressionType::None)
        .chunk_size(24); // ~3 records/chunk at 8 bytes each
    let records: Vec<Vec<u8>> = (0..10u64).map(encode_sorted_record).collect();
    let data = write_records(&records, opts);

    for target in 0u64..10u64 {
        let mut reader = RecordReader::new(Cursor::new(&data), ReaderOptions::new()).expect("ok");
        let found = reader
            .search(|rec| compare_record(rec, target))
            .expect("ok");
        assert!(found, "should find record {}", target);

        let next = reader
            .read_record()
            .expect("ok")
            .expect("should have record");
        assert_eq!(
            parse_sorted_record(&next),
            Some(target),
            "next read should be {}",
            target
        );
    }
}

// ---------------------------------------------------------------------------
// Adversarial: set_field_projection before any reads
// ---------------------------------------------------------------------------

/// set_field_projection called before the first read_record should take effect
/// from the very first chunk loaded.
#[test]
fn adv_set_field_projection_before_first_read() {
    // Write a transpose file with proto records containing fields 1 and 2.
    let encode_proto = |f1: u64, f2: u32| -> Vec<u8> {
        let mut rec = Vec::new();
        // field 1: varint
        let tag1 = 1u32 << 3; // field 1, wire type 0
        rec.push(tag1 as u8);
        rec.push(f1 as u8); // small value fits in 1 byte
        // field 2: varint
        let tag2 = 2u32 << 3; // field 2, wire type 0
        rec.push(tag2 as u8);
        rec.push(f2 as u8);
        rec
    };

    let opts = WriterOptions::new()
        .compression(CompressionType::None)
        .transpose(true);

    let records: Vec<Vec<u8>> = (0u8..10u8)
        .map(|i| encode_proto(i as u64, (i as u32) + 100))
        .collect();
    let data = write_records(&records, opts);

    // Open with no projection.
    let mut reader = RecordReader::new(Cursor::new(&data), ReaderOptions::new()).expect("ok");

    // Set projection BEFORE any reads.
    let proj = FieldProjection::new().add_field(Field::new(vec![1]));
    reader.set_field_projection(proj);

    // All records should use the new projection — field 2 should be absent.
    // (The projection takes effect at the next chunk boundary; since no chunk has been
    //  loaded yet, the very first chunk load uses the new projection.)
    let full_record_size = encode_proto(5, 105).len();

    // Read all records — at least those from chunks loaded AFTER the set_field_projection
    // call should be filtered.
    let mut all_records = Vec::new();
    while let Some(rec) = reader.read_record().expect("ok") {
        all_records.push(rec);
    }

    // All records that came from the new chunk (loaded after set_field_projection) should
    // be shorter. Since we called set_field_projection before any reads, all records
    // should come from chunks loaded after the call.
    assert!(!all_records.is_empty(), "should have read some records");
    // At least some records should be projected (shorter than full).
    let has_projected = all_records.iter().any(|r| r.len() < full_record_size);
    assert!(
        has_projected,
        "some records should be projected (shorter than full {} bytes)",
        full_record_size
    );
}

// ---------------------------------------------------------------------------
// Adversarial: search positions at the FIRST occurrence when there are duplicates
// ---------------------------------------------------------------------------

/// When there are duplicate values, search returns Ok(true) and positions at
/// some record with the target value (not necessarily the first, but the next
/// read_record() must return the target value).
#[test]
fn adv_search_duplicate_values() {
    // File: [0, 0, 5, 5, 10, 10] — each value appears twice.
    let opts = WriterOptions::new()
        .compression(CompressionType::None)
        .chunk_size(256);
    let records: Vec<Vec<u8>> = vec![0u64, 0, 5, 5, 10, 10]
        .into_iter()
        .map(encode_sorted_record)
        .collect();
    let data = write_records(&records, opts);

    for target in &[0u64, 5, 10] {
        let mut reader = RecordReader::new(Cursor::new(&data), ReaderOptions::new()).expect("ok");
        let found = reader
            .search(|rec| compare_record(rec, *target))
            .expect("ok");
        assert!(found, "should find target {}", target);

        let next = reader
            .read_record()
            .expect("ok")
            .expect("should have record");
        let val = parse_sorted_record(&next).expect("parse ok");
        assert_eq!(val, *target, "next read should return target {}", target);
    }
}

// ---------------------------------------------------------------------------
// Adversarial: search does not leave reader in broken state after Ok(false)
// ---------------------------------------------------------------------------

/// After a failed search, the reader should be at EOF.
/// A second search should also work correctly (no stale state).
#[test]
fn adv_search_twice_second_after_eof() {
    let opts = WriterOptions::new()
        .compression(CompressionType::None)
        .chunk_size(256);
    let records: Vec<Vec<u8>> = (0..20u64).map(encode_sorted_record).collect();
    let data = write_records(&records, opts);

    let mut reader = RecordReader::new(Cursor::new(&data), ReaderOptions::new()).expect("ok");

    // First search: fails (target 99 not in file).
    let found1 = reader.search(|rec| compare_record(rec, 99)).expect("ok");
    assert!(!found1, "search(99) should fail");

    // Second search: succeeds (target 10 IS in file).
    // search always re-scans from the beginning, so this should work.
    let found2 = reader.search(|rec| compare_record(rec, 10)).expect("ok");
    assert!(
        found2,
        "search(10) should succeed after a prior failed search"
    );

    let next = reader
        .read_record()
        .expect("ok")
        .expect("should have record");
    assert_eq!(
        parse_sorted_record(&next),
        Some(10),
        "next read should be 10"
    );
}
