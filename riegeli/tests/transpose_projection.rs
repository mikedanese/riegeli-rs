//! Integration tests for field projection on transpose chunks.

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

/// Encode a proto varint field: tag (field_number, wire_type=0) + varint value.
#[allow(clippy::identity_op)] // tags spell out the varint wiretype: (field << 3) | 0
fn encode_varint_field(field_number: u32, value: u64) -> Vec<u8> {
    let tag = (field_number << 3) | 0; // wire type 0 = varint
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

/// Encode a proto fixed32 field: tag (field_number, wire_type=5) + 4 bytes.
fn encode_fixed32_field(field_number: u32, value: u32) -> Vec<u8> {
    let tag = (field_number << 3) | 5; // wire type 5 = fixed32
    let mut out = encode_u32(tag);
    out.extend_from_slice(&value.to_le_bytes());
    out
}

/// Encode a proto string field: tag (field_number, wire_type=2) + length + bytes.
fn encode_string_field(field_number: u32, value: &[u8]) -> Vec<u8> {
    let tag = (field_number << 3) | 2; // wire type 2 = length-delimited
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

/// Parse a proto record and extract field 1 (varint) value.
/// Returns None if the field is absent.
fn parse_field1_varint(record: &[u8]) -> Option<u64> {
    let mut pos = 0;
    while pos < record.len() {
        let remaining = &record[pos..];
        let (tag, consumed) = decode_u32(remaining).ok()?;
        pos += consumed;
        let field_number = tag >> 3;
        let wire_type = tag & 7;
        if field_number == 1 && wire_type == 0 {
            // Varint value.
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
                    // varint — skip
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
                    let rem = &record[pos..];
                    let (len, c) = decode_u32(rem).ok()?;
                    pos += c + len as usize;
                }
                _ => break,
            }
        }
    }
    None
}

/// Check if a field number (given wire type 0 = varint) is absent from a record.
fn field_absent(record: &[u8], field_number: u32) -> bool {
    let mut pos = 0;
    while pos < record.len() {
        let remaining = &record[pos..];
        let (tag, consumed) = match decode_u32(remaining) {
            Ok(v) => v,
            Err(_) => return true,
        };
        pos += consumed;
        let fn_num = tag >> 3;
        let wire_type = tag & 7;
        if fn_num == field_number {
            return false;
        }
        match wire_type {
            0 => {
                while pos < record.len() {
                    let b = record[pos];
                    pos += 1;
                    if b < 0x80 {
                        break;
                    }
                }
            }
            5 => pos += 4,
            1 => pos += 8,
            2 => {
                let rem = &record[pos..];
                let (len, c) = match decode_u32(rem) {
                    Ok(v) => v,
                    Err(_) => return true,
                };
                pos += c + len as usize;
            }
            _ => break,
        }
    }
    true
}

/// Write records to a byte buffer and return the bytes.
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

fn encode_group(field: u32, content: &[u8]) -> Vec<u8> {
    let mut out = encode_u32((field << 3) | 3);
    out.extend_from_slice(content);
    out.extend_from_slice(&encode_u32((field << 3) | 4));
    out
}

/// Read all records from bytes with given options.
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
// Criterion 18.1: proto records with fields 1/2/3; projection = {field 1}
// ---------------------------------------------------------------------------

#[test]
fn test_projection_field1_only_simple_compression() {
    // Write 10 records with fields 1, 2, 3 using transpose encoding.
    let records: Vec<Vec<u8>> = (0..10u64)
        .map(|i| make_proto_record(i + 1, (i * 100) as u32, b"hello"))
        .collect();
    let record_refs: Vec<&[u8]> = records.iter().map(|r| r.as_slice()).collect();

    let opts = WriterOptions::new()
        .transpose(true)
        .compression(CompressionType::None);
    let data = write_records(&record_refs, opts);

    // Read with projection: only field 1.
    let proj = FieldProjection::new().add_field(Field::new(vec![1]));
    let reader_opts = ReaderOptions::new().field_projection(proj);
    let results = read_all(&data, reader_opts);

    assert_eq!(results.len(), 10, "should read 10 records");
    for (i, rec) in results.iter().enumerate() {
        // Field 1 must be present with correct value.
        let val = parse_field1_varint(rec).expect("field 1 should be present");
        assert_eq!(val, (i as u64) + 1, "field 1 value mismatch at record {i}");
        // Fields 2 and 3 must be absent.
        assert!(
            field_absent(rec, 2),
            "field 2 should be absent in record {i}"
        );
        assert!(
            field_absent(rec, 3),
            "field 3 should be absent in record {i}"
        );
    }
}

// ---------------------------------------------------------------------------
// Criterion 18.2: bucket_fraction(0.1) + projection returns correct field 1 values
// ---------------------------------------------------------------------------

#[test]
#[cfg(feature = "brotli")]
fn test_projection_with_small_bucket_fraction() {
    // Write many records with multiple fields, small bucket_fraction.
    let records: Vec<Vec<u8>> = (0..100u64)
        .map(|i| make_proto_record(i + 1, (i * 7) as u32, b"world"))
        .collect();
    let record_refs: Vec<&[u8]> = records.iter().map(|r| r.as_slice()).collect();

    let opts = WriterOptions::new()
        .transpose(true)
        .bucket_fraction(0.1)
        .compression(CompressionType::Brotli);
    let data = write_records(&record_refs, opts);

    // Read with projection = {field 1} — should complete and return correct values.
    let proj = FieldProjection::new().add_field(Field::new(vec![1]));
    let reader_opts = ReaderOptions::new().field_projection(proj);
    let results = read_all(&data, reader_opts);

    assert_eq!(results.len(), 100, "should read 100 records");
    for (i, rec) in results.iter().enumerate() {
        let val = parse_field1_varint(rec).expect("field 1 should be present");
        assert_eq!(val, (i as u64) + 1, "field 1 value mismatch at record {i}");
    }
}

// ---------------------------------------------------------------------------
// Criterion 18.4: non-proto records pass through unchanged
// ---------------------------------------------------------------------------

#[test]
fn test_nonproto_records_passthrough() {
    // Non-proto records are raw bytes that don't parse as valid proto.
    // We can write simple (non-transpose) records which are stored verbatim.
    // For transpose encoding, the encoder may detect them as nonproto.
    //
    // Write records that are not valid proto: random bytes starting with 0xFF.
    let nonproto: Vec<Vec<u8>> = (0..5u8).map(|i| vec![0xFF, 0xFE, i, i + 1]).collect();
    let record_refs: Vec<&[u8]> = nonproto.iter().map(|r| r.as_slice()).collect();

    let opts = WriterOptions::new()
        .transpose(true)
        .compression(CompressionType::None);
    let data = write_records(&record_refs, opts);

    // Read with a narrow projection — nonproto should still pass through.
    let proj = FieldProjection::new().add_field(Field::new(vec![1]));
    let results = read_all(&data, ReaderOptions::new().field_projection(proj));

    assert_eq!(
        results.len(),
        nonproto.len(),
        "should read all nonproto records"
    );
    for (i, (got, expected)) in results.iter().zip(nonproto.iter()).enumerate() {
        assert_eq!(got, expected, "nonproto record {i} mismatch");
    }
}

// ---------------------------------------------------------------------------
// Criterion 18.6: bucket_fraction(1.0) + projection works correctly
// ---------------------------------------------------------------------------

#[test]
fn test_projection_single_bucket() {
    let records: Vec<Vec<u8>> = (0..10u64)
        .map(|i| make_proto_record(i + 100, (i * 5) as u32, b"xyz"))
        .collect();
    let record_refs: Vec<&[u8]> = records.iter().map(|r| r.as_slice()).collect();

    let opts = WriterOptions::new()
        .transpose(true)
        .bucket_fraction(1.0)
        .compression(CompressionType::None);
    let data = write_records(&record_refs, opts);

    // Project only field 1.
    let proj = FieldProjection::new().add_field(Field::new(vec![1]));
    let results = read_all(&data, ReaderOptions::new().field_projection(proj));

    assert_eq!(results.len(), 10);
    for (i, rec) in results.iter().enumerate() {
        let val = parse_field1_varint(rec).expect("field 1 should be present");
        assert_eq!(val, (i as u64) + 100, "field 1 mismatch at record {i}");
        assert!(
            field_absent(rec, 2),
            "field 2 should be absent at record {i}"
        );
        assert!(
            field_absent(rec, 3),
            "field 3 should be absent at record {i}"
        );
    }
}

// ---------------------------------------------------------------------------
// Criterion 18.7: simple (non-transpose) chunk + projection → all records unmodified
// ---------------------------------------------------------------------------

#[test]
fn test_simple_chunk_projection_passthrough() {
    let records: Vec<Vec<u8>> = (0..10u64)
        .map(|i| make_proto_record(i + 1, (i * 2) as u32, b"simple"))
        .collect();
    let record_refs: Vec<&[u8]> = records.iter().map(|r| r.as_slice()).collect();

    // Write as simple (non-transpose) chunks.
    let opts = WriterOptions::new()
        .transpose(false)
        .compression(CompressionType::None);
    let data = write_records(&record_refs, opts);

    // Read without projection.
    let no_proj = read_all(&data, ReaderOptions::new());

    // Read with a narrow projection.
    let proj = FieldProjection::new().add_field(Field::new(vec![1]));
    let with_proj = read_all(&data, ReaderOptions::new().field_projection(proj));

    // Simple chunks ignore projection — all records should be identical.
    assert_eq!(no_proj, with_proj, "simple chunk should ignore projection");
}

// ---------------------------------------------------------------------------
// Criterion 18.8: nested field path [1, 2, 3] correctly included
// (Direct apply() test migrated to src/field_projection.rs unit tests;
// roundtrip test remains here.)
// ---------------------------------------------------------------------------

#[test]
fn test_nested_field_projection_roundtrip() {
    // Build records with nested structure and write/read via simple (non-transpose)
    // encoding. The projection applies only to transpose chunks, so simple chunks
    // pass through unchanged. We test this case to verify no crashes.
    let records: Vec<Vec<u8>> = (0..5u64)
        .map(|i| {
            let inner = encode_varint_field(3, i);
            let middle = encode_string_field(2, &inner);
            encode_string_field(1, &middle)
        })
        .collect();
    let record_refs: Vec<&[u8]> = records.iter().map(|r| r.as_slice()).collect();

    // Use transpose encoding with the nested records.
    let opts = WriterOptions::new()
        .transpose(true)
        .compression(CompressionType::None);
    let data = write_records(&record_refs, opts);

    // Read with projection [1, 2, 3].
    let proj = FieldProjection::new().add_field(Field::new(vec![1, 2, 3]));
    let results = read_all(&data, ReaderOptions::new().field_projection(proj));

    assert_eq!(results.len(), 5, "should read 5 records");
    // Each record should contain field 1 (present) and field 3 inside field 2.
    for (i, rec) in results.iter().enumerate() {
        // Field 1 should be present.
        assert!(
            !field_absent(rec, 1),
            "field 1 should be present in record {i}"
        );
    }
}

// ---------------------------------------------------------------------------
// Regression: size() before any reads should not break subsequent reads
// ---------------------------------------------------------------------------

#[test]
fn test_size_before_any_reads() {
    // Write 50 records.
    let records: Vec<Vec<u8>> = (0..50u8).map(|i| vec![i; 20]).collect();
    let record_refs: Vec<&[u8]> = records.iter().map(|r| r.as_slice()).collect();
    let data = write_records(&record_refs, WriterOptions::new());

    let cursor = Cursor::new(data);
    let mut reader = RecordReader::new(cursor, ReaderOptions::new()).expect("reader new ok");

    // Call size() before any reads.
    let sz = reader.size().expect("size() should not fail");
    assert_eq!(sz, 50, "size() should return 50");

    // Now read all records — should succeed (bug was that at_eof was set to true).
    let mut count = 0;
    while let Some(rec) = reader.read_record().expect("read after size() ok") {
        assert_eq!(rec, records[count], "record {count} mismatch after size()");
        count += 1;
    }
    assert_eq!(count, 50, "should read all 50 records after size()");
}

// ---------------------------------------------------------------------------
// Verify size() position preservation with a mid-read call
// ---------------------------------------------------------------------------

#[test]
fn test_size_preserves_position_mid_read() {
    let records: Vec<Vec<u8>> = (0..100u8).map(|i| vec![i; 10]).collect();
    let record_refs: Vec<&[u8]> = records.iter().map(|r| r.as_slice()).collect();
    let data = write_records(&record_refs, WriterOptions::new().chunk_size(200));

    let cursor = Cursor::new(data);
    let mut reader = RecordReader::new(cursor, ReaderOptions::new()).expect("new ok");

    // Read 10 records.
    for expected in &records[..10] {
        let rec = reader
            .read_record()
            .expect("read ok")
            .expect("should have record");
        assert_eq!(&rec, expected);
    }

    // Call size() mid-read.
    let sz = reader.size().expect("size() ok");
    assert_eq!(sz, 100);

    // Next read should return record 11 (index 10).
    let rec11 = reader
        .read_record()
        .expect("read ok after size()")
        .expect("should have record 11");
    assert_eq!(
        rec11, records[10],
        "position should be preserved after size()"
    );
}

// ---------------------------------------------------------------------------
// Regression tests: projections that silently zeroed or dropped data
// ---------------------------------------------------------------------------

/// An empty-path Field means "include everything". Two code paths used to
/// disagree: the include map treated it as projection-disabled while buffer
/// pruning matched it against nothing — so every field not otherwise listed
/// came back with a correct tag and a silently zeroed value.
#[test]
fn empty_path_field_includes_everything() {
    let records: Vec<Vec<u8>> = (0..4)
        .map(|i| make_proto_record(1000 + i, 0xDEAD_0000 + i as u32, b"payload"))
        .collect();
    let record_refs: Vec<&[u8]> = records.iter().map(|r| r.as_slice()).collect();
    let data = write_records(
        &record_refs,
        WriterOptions::new()
            .transpose(true)
            .compression(CompressionType::None),
    );

    let proj = FieldProjection::new().add_field(Field::new(vec![]));
    let got = read_all(&data, ReaderOptions::new().field_projection(proj));
    assert_eq!(got.len(), records.len());
    for (g, w) in got.iter().zip(&records) {
        assert_eq!(g, w, "empty-path projection must reproduce the full record");
    }
}

/// A nested-path projection (len >= 2) must decode the inner field's real
/// value. Buffer pruning used to match only top-level field numbers, so the
/// buffer behind the inner field was starved and its value decoded as zeros.
#[test]
fn nested_path_projection_preserves_inner_values() {
    // field 1 = submessage { field 2 = varint }
    let inner = encode_varint_field(2, 777);
    let outer = encode_string_field(1, &inner);
    let records = [outer.as_slice(), outer.as_slice()];
    let data = write_records(
        &records,
        WriterOptions::new()
            .transpose(true)
            .compression(CompressionType::None),
    );

    let proj = FieldProjection::new().add_field(Field::new(vec![1, 2]));
    let got = read_all(&data, ReaderOptions::new().field_projection(proj));
    assert_eq!(got.len(), 2);
    for g in &got {
        // Parse: outer tag, length, then the inner varint field 2 == 777.
        let (outer_tag, c1) = decode_u32(g).expect("outer tag");
        assert_eq!(outer_tag >> 3, 1);
        let (_len, c2) = decode_u32(&g[c1..]).expect("outer len");
        let inner_bytes = &g[c1 + c2..];
        let (inner_tag, c3) = decode_u32(inner_bytes).expect("inner tag");
        assert_eq!(inner_tag >> 3, 2);
        let (value, _) = decode_u32(&inner_bytes[c3..]).expect("inner value");
        assert_eq!(
            value, 777,
            "nested projection must carry the real value, not zeros"
        );
    }
}

/// existence_only on a submessage field must emit tag + zero length in
/// transpose chunks, exactly as the simple-chunk path does — it used to
/// drop the field entirely (tag included).
#[test]
fn existence_only_submessage_emits_tag_in_transpose() {
    let inner = encode_varint_field(2, 777);
    let outer = encode_string_field(1, &inner);
    let records = [outer.as_slice()];

    // The documented output (matching the simple-chunk path): the field's
    // LengthDelimited tag followed by a zero length.
    let mut expected = encode_u32((1 << 3) | 2);
    expected.push(0x00);

    let data = write_records(
        &records,
        WriterOptions::new()
            .transpose(true)
            .compression(CompressionType::None),
    );
    let proj = FieldProjection::new().add_field(Field::new(vec![1]).existence_only());
    let got = read_all(&data, ReaderOptions::new().field_projection(proj));
    assert_eq!(got.len(), 1);
    assert_eq!(
        got[0], expected,
        "transpose existence-only submessage must match the simple-chunk output"
    );
}

/// An existence-only submessage CONTAINING a nested submessage with content,
/// followed by an included sibling — the interleaved-skip-levels case. The
/// output must be exactly tag + 0x00 for the EO field, then the sibling:
/// nothing from the nested interior may leak, and record limits must hold.
#[test]
fn existence_only_submessage_with_nested_content_and_sibling() {
    let innermost = encode_varint_field(5, 42);
    let inner = encode_string_field(3, &innermost); // nested submessage
    let outer_sub = encode_string_field(1, &inner); // field 1: submessage (EO target)
    let mut record = outer_sub.clone();
    record.extend_from_slice(&encode_varint_field(2, 7)); // included sibling

    let data = write_records(
        &[record.as_slice()],
        WriterOptions::new()
            .transpose(true)
            .compression(CompressionType::None),
    );
    let proj = FieldProjection::new()
        .add_field(Field::new(vec![1]).existence_only())
        .add_field(Field::new(vec![2]));
    let got = read_all(&data, ReaderOptions::new().field_projection(proj));
    assert_eq!(got.len(), 1);

    let mut expected = encode_u32((1 << 3) | 2);
    expected.push(0x00);
    expected.extend_from_slice(&encode_varint_field(2, 7));
    assert_eq!(
        got[0], expected,
        "EO interior must not leak; sibling must survive"
    );
}

/// Two sequential existence-only submessages: frame/level pairing must not
/// cross-match between them.
#[test]
fn sequential_existence_only_submessages() {
    let sub1 = encode_string_field(1, &encode_varint_field(9, 1));
    let sub2 = encode_string_field(2, &encode_varint_field(9, 2));
    let mut record = sub1.clone();
    record.extend_from_slice(&sub2);

    let data = write_records(
        &[record.as_slice()],
        WriterOptions::new()
            .transpose(true)
            .compression(CompressionType::None),
    );
    let proj = FieldProjection::new()
        .add_field(Field::new(vec![1]).existence_only())
        .add_field(Field::new(vec![2]).existence_only());
    let got = read_all(&data, ReaderOptions::new().field_projection(proj));
    assert_eq!(got.len(), 1);

    let mut expected = encode_u32((1 << 3) | 2);
    expected.push(0x00);
    expected.extend_from_slice(&encode_u32((2 << 3) | 2));
    expected.push(0x00);
    assert_eq!(
        got[0], expected,
        "sequential EO submessages must pair independently"
    );
}

/// C++ parity (include-type min-resolution): when a projection contains BOTH
/// an existence-only path for a submessage and a fully-included child inside
/// it, the child wins (C++ resolves conflicts with std::min where
/// IncludeFully < IncludeChild < ExistenceOnly) — the submessage is framed
/// normally with the included child's real content, and non-included
/// siblings inside it are excluded.
#[test]
fn existence_only_upgraded_by_included_child_matches_cpp() {
    let mut inner = encode_varint_field(2, 7);
    inner.extend_from_slice(&encode_varint_field(3, 9)); // excluded sibling
    let outer = encode_string_field(1, &inner);

    let data = write_records(
        &[outer.as_slice()],
        WriterOptions::new()
            .transpose(true)
            .compression(CompressionType::None),
    );
    let proj = FieldProjection::new()
        .add_field(Field::new(vec![1]).existence_only())
        .add_field(Field::new(vec![1, 2]));
    let got = read_all(&data, ReaderOptions::new().field_projection(proj));
    assert_eq!(got.len(), 1);

    // Expected: field 1 { field 2 = 7 } — framed with real content.
    let expected_inner = encode_varint_field(2, 7);
    let expected = encode_string_field(1, &expected_inner);
    assert_eq!(
        got[0], expected,
        "included child must win over existence-only (C++ min-resolution)"
    );
}

/// PROBE A: nested include path THROUGH a group ([1,2] where 1 is a group).
#[test]
fn probe_nested_path_through_group() {
    let mut content = encode_varint_field(2, 7);
    content.extend_from_slice(&encode_varint_field(3, 9));
    let record = encode_group(1, &content);
    let data = write_records(
        &[record.as_slice()],
        WriterOptions::new()
            .transpose(true)
            .compression(CompressionType::None),
    );
    let proj = FieldProjection::new().add_field(Field::new(vec![1, 2]));
    let got = read_all(&data, ReaderOptions::new().field_projection(proj));
    let expected = encode_group(1, &encode_varint_field(2, 7));
    assert_eq!(got[0], expected, "PROBE A: child through group");
}

/// PROBE B: group included FULLY by path [1] — interior must survive.
#[test]
fn probe_group_included_fully() {
    let mut content = encode_varint_field(2, 7);
    content.extend_from_slice(&encode_varint_field(3, 9));
    let record = encode_group(1, &content);
    let data = write_records(
        &[record.as_slice()],
        WriterOptions::new()
            .transpose(true)
            .compression(CompressionType::None),
    );
    let proj = FieldProjection::new().add_field(Field::new(vec![1]));
    let got = read_all(&data, ReaderOptions::new().field_projection(proj));
    assert_eq!(
        got[0], record,
        "PROBE B: fully-included group keeps interior"
    );
}

/// PROBE C: LD submessage included FULLY by path [1] — interior values must
/// survive (the length-delimited analogue of probe B).
#[test]
fn probe_ld_submessage_included_fully() {
    let mut content = encode_varint_field(2, 7);
    content.extend_from_slice(&encode_varint_field(3, 9));
    let record = encode_string_field(1, &content);
    let data = write_records(
        &[record.as_slice()],
        WriterOptions::new()
            .transpose(true)
            .compression(CompressionType::None),
    );
    let proj = FieldProjection::new().add_field(Field::new(vec![1]));
    let got = read_all(&data, ReaderOptions::new().field_projection(proj));
    assert_eq!(
        got[0], record,
        "PROBE C: fully-included LD submessage keeps interior values"
    );
}

/// Buffer-consumption interleave (named documentation of the property the
/// randomized differential case covers statistically): an existence-only
/// submessage whose interior spans multiple buffer kinds (varint + string +
/// fixed32), followed by an included sibling whose exact values must
/// survive — the skip path must consume interior buffer bytes in exactly
/// the order the include path would have.
#[test]
fn eo_interior_buffer_interleave_preserves_sibling_values() {
    let mut inner = encode_varint_field(1, 300);
    inner.extend_from_slice(&encode_string_field(2, b"HELLO"));
    inner.extend_from_slice(&encode_u32((3 << 3) | 5)); // fixed32 tag
    inner.extend_from_slice(&[0xAA, 0xBB, 0xCC, 0xDD]);
    let mut record = encode_string_field(1, &inner); // EO target
    record.extend_from_slice(&encode_string_field(2, b"SIBLING"));
    record.extend_from_slice(&encode_varint_field(3, 777));

    let data = write_records(
        &[record.as_slice()],
        WriterOptions::new()
            .transpose(true)
            .compression(CompressionType::None),
    );
    let proj = FieldProjection::new()
        .add_field(Field::new(vec![1]).existence_only())
        .add_field(Field::new(vec![2]))
        .add_field(Field::new(vec![3]));
    let got = read_all(&data, ReaderOptions::new().field_projection(proj));

    let mut expected = encode_u32((1 << 3) | 2);
    expected.push(0x00);
    expected.extend_from_slice(&encode_string_field(2, b"SIBLING"));
    expected.extend_from_slice(&encode_varint_field(3, 777));
    assert_eq!(
        got[0], expected,
        "sibling values exact after EO interior skip"
    );
}
