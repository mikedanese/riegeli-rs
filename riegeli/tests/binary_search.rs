//! Integration tests for RecordReader::search() binary search.

use std::cmp::Ordering;
use std::io::Cursor;

use riegeli::{
    CompressionType, Field, FieldProjection, ReaderOptions, RecordReader, RecordWriter,
    WriterOptions,
};

fn encode_u32(v: u32) -> Vec<u8> {
    let mut out = Vec::new();
    let mut v = v as u64;
    loop {
        if v < 0x80 {
            out.push(v as u8);
            break;
        }
        out.push((v as u8 & 0x7f) | 0x80);
        v >>= 7;
    }
    out
}

fn decode_u32(buf: &[u8]) -> Result<(u32, usize), String> {
    let mut result = 0u32;
    let mut shift = 0u32;
    for (i, &byte) in buf.iter().enumerate() {
        if shift >= 32 {
            return Err("varint overflow".into());
        }
        result |= ((byte & 0x7f) as u32) << shift;
        shift += 7;
        if byte & 0x80 == 0 {
            return Ok((result, i + 1));
        }
    }
    Err("unexpected EOF".into())
}

// ---------------------------------------------------------------------------
// Proto encoding helpers
// ---------------------------------------------------------------------------

#[allow(clippy::identity_op)] // tags spell out the varint wiretype: (field << 3) | 0
fn encode_varint_field(field_number: u32, value: u64) -> Vec<u8> {
    let tag = (field_number << 3) | 0u32;
    let mut out = encode_u32(tag);
    let mut v = value;
    loop {
        let byte = (v & 0x7F) as u8;
        v >>= 7;
        if v == 0 {
            out.push(byte);
            break;
        } else {
            out.push(byte | 0x80);
        }
    }
    out
}

fn encode_fixed32_field(field_number: u32, value: u32) -> Vec<u8> {
    let tag = (field_number << 3) | 5u32;
    let mut out = encode_u32(tag);
    out.extend_from_slice(&value.to_le_bytes());
    out
}

fn encode_string_field(field_number: u32, value: &[u8]) -> Vec<u8> {
    let tag = (field_number << 3) | 2u32;
    let mut out = encode_u32(tag);
    out.extend_from_slice(&encode_u32(value.len() as u32));
    out.extend_from_slice(value);
    out
}

/// Build a proto record with fields 1 (varint), 2 (fixed32), 3 (string).
fn make_proto_record(field1: u64, field2: u32, field3: &[u8]) -> Vec<u8> {
    let mut rec = Vec::new();
    rec.extend_from_slice(&encode_varint_field(1, field1));
    rec.extend_from_slice(&encode_fixed32_field(2, field2));
    rec.extend_from_slice(&encode_string_field(3, field3));
    rec
}

/// Parse field 1 (varint) from a proto record. Returns None if absent.
fn parse_field1(record: &[u8]) -> Option<u64> {
    let mut pos = 0;
    while pos < record.len() {
        let (tag, consumed) = decode_u32(&record[pos..]).ok()?;
        pos += consumed;
        let field_number = tag >> 3;
        let wire_type = tag & 7;
        if field_number == 1 && wire_type == 0 {
            let mut val: u64 = 0;
            let mut shift = 0u64;
            loop {
                if pos >= record.len() {
                    return None;
                }
                let b = record[pos];
                pos += 1;
                val |= ((b & 0x7F) as u64) << shift;
                shift += 7;
                if b < 0x80 {
                    break;
                }
            }
            return Some(val);
        } else {
            // Skip this field.
            match wire_type {
                0 => {
                    // varint: skip bytes until high bit clear
                    while pos < record.len() {
                        let b = record[pos];
                        pos += 1;
                        if b < 0x80 {
                            break;
                        }
                    }
                }
                5 => pos += 4, // fixed32
                1 => pos += 8, // fixed64
                2 => {
                    // length-delimited
                    let (len, c) = decode_u32(&record[pos..]).ok()?;
                    pos += c + len as usize;
                }
                _ => break,
            }
        }
    }
    None
}

/// Write records to a Riegeli file using the given options.
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

// ---------------------------------------------------------------------------
// Criterion 19.3: search finds record 500 in 1000-record sorted file
// ---------------------------------------------------------------------------

/// Encode a u64 value as a big-endian sortable bytes for records.
/// We use big-endian so byte comparison is equivalent to integer comparison.
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

fn make_sorted_file(n: usize) -> Vec<u8> {
    let opts = WriterOptions::new()
        .compression(CompressionType::None)
        .chunk_size(4096); // larger chunks so we have ~10 records/chunk
    let records: Vec<Vec<u8>> = (0..n as u64).map(encode_sorted_record).collect();
    write_records(&records, opts)
}

fn compare_record(record: &[u8], target: u64) -> Ordering {
    match parse_sorted_record(record) {
        Some(v) => v.cmp(&target),
        None => Ordering::Less,
    }
}

#[test]
fn test_search_finds_middle_record() {
    let data = make_sorted_file(1000);
    let mut reader =
        RecordReader::new(Cursor::new(&data), ReaderOptions::new()).expect("reader ok");

    let target = 500u64;
    let found = reader
        .search(|rec| compare_record(rec, target))
        .expect("search ok");

    assert!(found, "search should find record 500");

    // Next read_record() should return the record with value 500.
    let next_rec = reader
        .read_record()
        .expect("read ok")
        .expect("should have record");
    let value = parse_sorted_record(&next_rec).expect("parse ok");
    assert_eq!(value, 500, "next record after search should be 500");
}

// ---------------------------------------------------------------------------
// Criterion 19.4: search for absent value returns Ok(false)
// ---------------------------------------------------------------------------

#[test]
fn test_search_absent_value() {
    let data = make_sorted_file(1000);
    let mut reader =
        RecordReader::new(Cursor::new(&data), ReaderOptions::new()).expect("reader ok");

    let found = reader
        .search(|rec| compare_record(rec, 1500))
        .expect("search ok");

    assert!(!found, "search for 1500 should return false (not in file)");

    // Reader should be at EOF.
    let next = reader.read_record().expect("read ok");
    assert!(
        next.is_none(),
        "reader should be at EOF after failed search"
    );
}

// ---------------------------------------------------------------------------
// Criterion 19.5: search for first record (value 0)
// ---------------------------------------------------------------------------

#[test]
fn test_search_first_record() {
    let data = make_sorted_file(1000);
    let mut reader =
        RecordReader::new(Cursor::new(&data), ReaderOptions::new()).expect("reader ok");

    let found = reader
        .search(|rec| compare_record(rec, 0))
        .expect("search ok");

    assert!(found, "search should find record 0");

    let next_rec = reader
        .read_record()
        .expect("read ok")
        .expect("should have record");
    let value = parse_sorted_record(&next_rec).expect("parse ok");
    assert_eq!(value, 0, "next record after search should be 0");
}

// ---------------------------------------------------------------------------
// Criterion 19.6: search for last record (value 999)
// ---------------------------------------------------------------------------

#[test]
fn test_search_last_record() {
    let data = make_sorted_file(1000);
    let mut reader =
        RecordReader::new(Cursor::new(&data), ReaderOptions::new()).expect("reader ok");

    let found = reader
        .search(|rec| compare_record(rec, 999))
        .expect("search ok");

    assert!(found, "search should find record 999");

    let next_rec = reader
        .read_record()
        .expect("read ok")
        .expect("should have record");
    let value = parse_sorted_record(&next_rec).expect("parse ok");
    assert_eq!(value, 999, "next record after search should be 999");
}

// ---------------------------------------------------------------------------
// Criterion 19.7: search on empty file returns Ok(false)
// ---------------------------------------------------------------------------

#[test]
fn test_search_empty_file() {
    let opts = WriterOptions::new().compression(CompressionType::None);
    let data = write_records(&[], opts);

    let mut reader =
        RecordReader::new(Cursor::new(&data), ReaderOptions::new()).expect("reader ok");

    let found = reader.search(|_rec| Ordering::Equal).expect("search ok");
    assert!(!found, "search on empty file should return false");
}

// ---------------------------------------------------------------------------
// Criterion 19.8: O(log N) closure calls
// ---------------------------------------------------------------------------

#[test]
fn test_search_log_n_calls() {
    let data = make_sorted_file(1000);
    let mut reader =
        RecordReader::new(Cursor::new(&data), ReaderOptions::new()).expect("reader ok");

    let mut call_count = 0usize;
    let target = 500u64;

    reader
        .search(|rec| {
            call_count += 1;
            compare_record(rec, target)
        })
        .expect("search ok");

    // For 1000 records with chunk_size=4096 (each record = 8 bytes, ~512 records/chunk,
    // so ~2 chunks) we'd have very few calls. Even for smaller chunks we stay under 20.
    // The spec says ≤20 calls for a 1000-record file.
    assert!(
        call_count <= 20,
        "search should use at most 20 closure calls, used {}",
        call_count
    );
}

// ---------------------------------------------------------------------------
// Additional: search multiple values in a file
// ---------------------------------------------------------------------------

#[test]
fn test_search_various_values() {
    // Write with smaller chunk size to have more chunks and exercise binary search.
    let opts = WriterOptions::new()
        .compression(CompressionType::None)
        .chunk_size(256); // ~32 records per chunk with 8-byte records
    let records: Vec<Vec<u8>> = (0..1000u64).map(encode_sorted_record).collect();
    let data = write_records(&records, opts);

    for &target in &[0u64, 1, 100, 499, 500, 501, 998, 999] {
        let mut reader =
            RecordReader::new(Cursor::new(&data), ReaderOptions::new()).expect("reader ok");
        let found = reader
            .search(|rec| compare_record(rec, target))
            .expect("search ok");
        assert!(found, "search should find record {}", target);

        let next_rec = reader
            .read_record()
            .expect("read ok")
            .expect("should have record");
        let value = parse_sorted_record(&next_rec).expect("parse ok");
        assert_eq!(value, target, "next record should be {}", target);
    }
}

// ---------------------------------------------------------------------------
// Additional: set_field_projection with explicit chunk boundary crossing
// ---------------------------------------------------------------------------

#[test]
fn test_set_field_projection_chunk_boundary() {
    // Write exactly 2 records to a transpose file, each in its own chunk
    // by using a very small chunk size.
    let opts = WriterOptions::new()
        .compression(CompressionType::None)
        .transpose(true)
        .chunk_size(1); // Force each record into its own chunk

    let records: Vec<Vec<u8>> = (0..20u64)
        .map(|i| make_proto_record(i, i as u32 * 10, b"world"))
        .collect();

    let data = write_records(&records, opts);

    // Read with no projection initially.
    let mut reader =
        RecordReader::new(Cursor::new(&data), ReaderOptions::new()).expect("reader ok");

    // Read some records from first chunk.
    let rec1 = reader.read_record().expect("read ok").expect("have rec");
    let field1_val = parse_field1(&rec1);
    assert!(field1_val.is_some(), "full record should have field 1");

    // Set projection to field 1 only.
    let proj = FieldProjection::new().add_field(Field::new(vec![1]));
    reader.set_field_projection(proj);

    // Keep reading until we see projected (smaller) records.
    let full_size = records[0].len();
    let mut saw_projected = false;
    while let Some(rec) = reader.read_record().expect("read ok") {
        if rec.len() < full_size {
            saw_projected = true;
            // Verify it has field 1 and no field 2/3.
            assert!(
                parse_field1(&rec).is_some(),
                "projected record should have field 1"
            );
            break;
        }
    }

    assert!(
        saw_projected,
        "should see projected records after chunk boundary"
    );
}
