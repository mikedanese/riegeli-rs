//! Integration tests for FieldProjection: column pruning in transpose-encoded files.

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
// Proto encoding helpers (duplicated to keep this file self-contained)
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

fn write_records(records: &[&[u8]], opts: WriterOptions) -> Vec<u8> {
    let mut buf = Cursor::new(Vec::<u8>::new());
    {
        let mut w = RecordWriter::new(&mut buf, opts).expect("new ok");
        for rec in records {
            w.write_record(rec).expect("write ok");
        }
        w.flush().expect("flush ok");
    }
    buf.into_inner()
}

fn read_all(data: &[u8], opts: ReaderOptions) -> Vec<Vec<u8>> {
    let cursor = Cursor::new(data);
    let mut reader = RecordReader::new(cursor, opts).expect("reader new ok");
    let mut out = Vec::new();
    while let Some(rec) = reader.read_record().expect("read ok") {
        out.push(rec);
    }
    out
}

// ---------------------------------------------------------------------------
// Adversarial Test 1: Empty projection (no fields added) returns empty records
// ---------------------------------------------------------------------------

#[test]
fn test_empty_projection_returns_empty_proto_records() {
    // Build proto records with field 1 and field 2.
    let records: Vec<Vec<u8>> = (0..5u64)
        .map(|i| {
            let mut r = Vec::new();
            r.extend_from_slice(&encode_varint_field(1, i + 1));
            r.extend_from_slice(&encode_varint_field(2, i * 10));
            r
        })
        .collect();
    let record_refs: Vec<&[u8]> = records.iter().map(|r| r.as_slice()).collect();

    let opts = WriterOptions::new()
        .transpose(true)
        .compression(CompressionType::None);
    let data = write_records(&record_refs, opts);

    // Empty projection — no fields included.
    let proj = FieldProjection::new(); // no add_field calls
    let results = read_all(&data, ReaderOptions::new().field_projection(proj));

    assert_eq!(
        results.len(),
        5,
        "should read 5 records even with empty projection"
    );
    for (i, rec) in results.iter().enumerate() {
        assert!(
            rec.is_empty(),
            "record {i} should be empty with empty projection, got {} bytes",
            rec.len()
        );
    }
}

// ---------------------------------------------------------------------------
// Adversarial Test 2: Multi-field projection {1, 3} from a 3-field record
// ---------------------------------------------------------------------------

#[test]
fn test_multi_field_projection_fields_1_and_3() {
    let records: Vec<Vec<u8>> = (0..5u64)
        .map(|i| {
            let mut r = Vec::new();
            r.extend_from_slice(&encode_varint_field(1, i + 1));
            r.extend_from_slice(&encode_varint_field(2, i * 7)); // excluded
            r.extend_from_slice(&encode_string_field(3, b"hello"));
            r
        })
        .collect();
    let record_refs: Vec<&[u8]> = records.iter().map(|r| r.as_slice()).collect();

    let opts = WriterOptions::new()
        .transpose(true)
        .compression(CompressionType::None);
    let data = write_records(&record_refs, opts);

    // Project {1, 3} — field 2 must be absent.
    let proj = FieldProjection::new()
        .add_field(Field::new(vec![1]))
        .add_field(Field::new(vec![3]));
    let results = read_all(&data, ReaderOptions::new().field_projection(proj));

    assert_eq!(results.len(), 5, "should read 5 records");
    for (i, rec) in results.iter().enumerate() {
        // Field 1 must be present.
        let mut has_field1 = false;
        let mut has_field3 = false;
        let mut has_field2 = false;
        let mut pos = 0;
        while pos < rec.len() {
            let (tag, c) = decode_u32(&rec[pos..]).expect("tag decode");
            pos += c;
            let fn_num = tag >> 3;
            let wt = tag & 7;
            if fn_num == 1 {
                has_field1 = true;
            }
            if fn_num == 2 {
                has_field2 = true;
            }
            if fn_num == 3 {
                has_field3 = true;
            }
            // Skip value.
            match wt {
                0 => {
                    while pos < rec.len() {
                        let b = rec[pos];
                        pos += 1;
                        if b < 0x80 {
                            break;
                        }
                    }
                }
                5 => pos += 4,
                1 => pos += 8,
                2 => {
                    let (l, c) = decode_u32(&rec[pos..]).expect("len");
                    pos += c + l as usize;
                }
                _ => break,
            }
        }
        assert!(has_field1, "record {i}: field 1 should be present");
        assert!(has_field3, "record {i}: field 3 should be present");
        assert!(!has_field2, "record {i}: field 2 should be absent");
    }
}

// ---------------------------------------------------------------------------
// Adversarial Test 4: Projection on a field absent from all records
// ---------------------------------------------------------------------------

#[test]
fn test_projection_on_absent_field_returns_empty_records() {
    let records: Vec<Vec<u8>> = (0..5u64)
        .map(|i| {
            let mut r = Vec::new();
            r.extend_from_slice(&encode_varint_field(1, i));
            r.extend_from_slice(&encode_varint_field(2, i * 2));
            r
        })
        .collect();
    let record_refs: Vec<&[u8]> = records.iter().map(|r| r.as_slice()).collect();

    let opts = WriterOptions::new()
        .transpose(true)
        .compression(CompressionType::None);
    let data = write_records(&record_refs, opts);

    // Project field 99, which is never present in the records.
    let proj = FieldProjection::new().add_field(Field::new(vec![99]));
    let results = read_all(&data, ReaderOptions::new().field_projection(proj));

    assert_eq!(results.len(), 5, "should still read 5 records");
    for (i, rec) in results.iter().enumerate() {
        assert!(
            rec.is_empty(),
            "record {i} should be empty since field 99 is absent, got {} bytes",
            rec.len()
        );
    }
}

// ---------------------------------------------------------------------------
// Adversarial Test 10: Projection applied to a transpose chunk with
// multiple records, verify exact byte-for-byte projection correctness
// ---------------------------------------------------------------------------

#[test]
fn test_projection_byte_exact_field1_from_multifield_transpose() {
    // Build records: field 1 = i, field 2 = i*100, field 3 = "str_<i>".
    // Write as transpose. Read with projection = {field 1}.
    // Verify each returned record is EXACTLY the encoding of field 1 alone,
    // not just "contains field 1" but "is only field 1".
    let n = 10u64;
    let records: Vec<Vec<u8>> = (0..n)
        .map(|i| {
            let mut r = Vec::new();
            r.extend_from_slice(&encode_varint_field(1, i + 1));
            r.extend_from_slice(&encode_fixed32_field(2, (i * 100) as u32));
            r.extend_from_slice(&encode_string_field(3, format!("str_{i}").as_bytes()));
            r
        })
        .collect();
    let record_refs: Vec<&[u8]> = records.iter().map(|r| r.as_slice()).collect();

    let opts = WriterOptions::new()
        .transpose(true)
        .compression(CompressionType::None);
    let data = write_records(&record_refs, opts);

    let proj = FieldProjection::new().add_field(Field::new(vec![1]));
    let results = read_all(&data, ReaderOptions::new().field_projection(proj));

    assert_eq!(results.len(), n as usize);
    for (i, rec) in results.iter().enumerate() {
        let expected = encode_varint_field(1, i as u64 + 1);
        assert_eq!(
            rec, &expected,
            "record {i}: expected exactly field-1 encoding, got different bytes"
        );
    }
}

// ---------------------------------------------------------------------------
// Adversarial Test 11: size() preserves projection state
// ---------------------------------------------------------------------------

#[test]
fn test_size_does_not_discard_projection() {
    // Write 10 records with fields 1, 2, 3 using transpose encoding.
    let records: Vec<Vec<u8>> = (0..10u64)
        .map(|i| {
            let mut r = Vec::new();
            r.extend_from_slice(&encode_varint_field(1, i + 1));
            r.extend_from_slice(&encode_varint_field(2, i * 5));
            r
        })
        .collect();
    let record_refs: Vec<&[u8]> = records.iter().map(|r| r.as_slice()).collect();

    let opts = WriterOptions::new()
        .transpose(true)
        .compression(CompressionType::None);
    let data = write_records(&record_refs, opts);

    let proj = FieldProjection::new().add_field(Field::new(vec![1]));
    let cursor = Cursor::new(data);
    let mut reader =
        RecordReader::new(cursor, ReaderOptions::new().field_projection(proj)).expect("new ok");

    // Call size() before reads — should not lose the projection.
    let sz = reader.size().expect("size() ok");
    assert_eq!(sz, 10);

    // Now reads should still apply the projection.
    let mut count = 0;
    while let Some(rec) = reader.read_record().expect("read ok") {
        // Field 2 should be absent (projection is still active).
        let mut has_field2 = false;
        let mut pos = 0;
        while pos < rec.len() {
            let (tag, c) = decode_u32(&rec[pos..]).expect("tag");
            pos += c;
            let fn_num = tag >> 3;
            let wt = tag & 7;
            if fn_num == 2 {
                has_field2 = true;
            }
            match wt {
                0 => {
                    while pos < rec.len() {
                        let b = rec[pos];
                        pos += 1;
                        if b < 0x80 {
                            break;
                        }
                    }
                }
                _ => break,
            }
        }
        assert!(
            !has_field2,
            "record {count}: field 2 should still be absent after size()"
        );
        count += 1;
    }
    assert_eq!(
        count, 10,
        "should read all 10 records with projection intact"
    );
}
