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

/// Decode the field numbers present in a proto record, in order of appearance.
fn proto_field_numbers(record: &[u8]) -> Vec<u32> {
    let mut fields = Vec::new();
    let mut pos = 0;
    while pos < record.len() {
        let (tag, consumed) = match decode_u32(&record[pos..]) {
            Ok(v) => v,
            Err(_) => break,
        };
        pos += consumed;
        fields.push(tag >> 3);
        // Skip the value.
        match tag & 7 {
            0 => {
                while pos < record.len() {
                    let b = record[pos];
                    pos += 1;
                    if b < 0x80 {
                        break;
                    }
                }
            }
            1 => pos += 8,
            2 => {
                let (len, c) = match decode_u32(&record[pos..]) {
                    Ok(v) => v,
                    Err(_) => break,
                };
                pos += c + len as usize;
            }
            5 => pos += 4,
            _ => break,
        }
    }
    fields
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

/// Encode a proto fixed64 field: tag (field_number, wire_type=1) + 8 bytes.
fn encode_fixed64_field(field_number: u32, value: u64) -> Vec<u8> {
    let tag = (field_number << 3) | 1;
    let mut out = encode_u32(tag);
    out.extend_from_slice(&value.to_le_bytes());
    out
}

/// Build a wide proto record with 20 fields of mixed wire types:
/// fields 1-5 varint, 6-10 fixed32, 11-15 fixed64, 16-20 string.
fn make_wide_record(seed: u64) -> Vec<u8> {
    let mut rec = Vec::new();
    // Fields 1-5: varint
    for f in 1..=5u32 {
        rec.extend_from_slice(&encode_varint_field(f, seed * (f as u64) + 1));
    }
    // Fields 6-10: fixed32
    for f in 6..=10u32 {
        rec.extend_from_slice(&encode_fixed32_field(f, (seed as u32).wrapping_mul(f)));
    }
    // Fields 11-15: fixed64
    for f in 11..=15u32 {
        rec.extend_from_slice(&encode_fixed64_field(f, seed.wrapping_mul(f as u64)));
    }
    // Fields 16-20: string
    for f in 16..=20u32 {
        let payload = format!("f{f}-{seed}");
        rec.extend_from_slice(&encode_string_field(f, payload.as_bytes()));
    }
    rec
}

fn decode_varint(buf: &[u8]) -> Option<(u64, usize)> {
    let mut result = 0u64;
    let mut shift = 0u32;
    for (i, &byte) in buf.iter().enumerate() {
        if shift >= 64 {
            return None;
        }
        result |= ((byte & 0x7f) as u64) << shift;
        shift += 7;
        if byte & 0x80 == 0 {
            return Some((result, i + 1));
        }
    }
    None
}

/// Extract a numeric field value from a proto record by field number.
/// Handles varint, fixed32, and fixed64 wire types.
fn find_varint_field(record: &[u8], field_number: u32) -> Option<u64> {
    let mut pos = 0;
    while pos < record.len() {
        let (tag, consumed) = decode_varint(&record[pos..])?;
        pos += consumed;
        let wire_type = (tag & 0x7) as u32;
        let fn_ = (tag >> 3) as u32;
        match wire_type {
            0 => {
                let (val, consumed) = decode_varint(&record[pos..])?;
                pos += consumed;
                if fn_ == field_number {
                    return Some(val);
                }
            }
            1 => {
                if fn_ == field_number {
                    if pos + 8 > record.len() {
                        return None;
                    }
                    return Some(u64::from_le_bytes(record[pos..pos + 8].try_into().ok()?));
                }
                pos += 8;
            }
            5 => {
                if fn_ == field_number {
                    if pos + 4 > record.len() {
                        return None;
                    }
                    return Some(u32::from_le_bytes(record[pos..pos + 4].try_into().ok()?) as u64);
                }
                pos += 4;
            }
            2 => {
                let (len, consumed) = decode_varint(&record[pos..])?;
                pos += consumed;
                pos += len as usize;
            }
            _ => return None,
        }
    }
    None
}

/// Write records with transpose encoding at the given compression.
fn write_transpose(records: &[Vec<u8>], compression: CompressionType) -> Vec<u8> {
    let refs: Vec<&[u8]> = records.iter().map(|r| r.as_slice()).collect();
    write_records(
        &refs,
        WriterOptions::new()
            .transpose(true)
            .compression(compression),
    )
}

/// Read all records with the given field projection applied.
fn read_projected(data: &[u8], proj: &FieldProjection) -> Vec<Vec<u8>> {
    read_all(data, ReaderOptions::new().field_projection(proj.clone()))
}

/// Parse a proto record into (field_number, wire_type, value_bytes) tuples.
/// For length-delimited fields the value bytes include the length prefix.
fn parse_fields(record: &[u8]) -> Vec<(u32, u32, Vec<u8>)> {
    let mut fields = Vec::new();
    let mut pos = 0;
    while pos < record.len() {
        let (tag, consumed) = decode_varint(&record[pos..]).expect("tag varint");
        pos += consumed;
        let field_number = (tag >> 3) as u32;
        let wire_type = (tag & 7) as u32;
        let value_bytes = match wire_type {
            0 => {
                // varint
                let start = pos;
                while pos < record.len() && record[pos] >= 0x80 {
                    pos += 1;
                }
                pos += 1; // terminal byte
                record[start..pos].to_vec()
            }
            1 => {
                // fixed64
                let b = record[pos..pos + 8].to_vec();
                pos += 8;
                b
            }
            5 => {
                // fixed32
                let b = record[pos..pos + 4].to_vec();
                pos += 4;
                b
            }
            2 => {
                // length-delimited
                let (len, lc) = decode_varint(&record[pos..]).expect("length varint");
                let start = pos;
                pos += lc + len as usize;
                record[start..pos].to_vec()
            }
            _ => panic!("unknown wire type {wire_type}"),
        };
        fields.push((field_number, wire_type, value_bytes));
    }
    fields
}

// ---------------------------------------------------------------------------
// proto records with fields 1/2/3; projection = {field 1}
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
// bucket_fraction(0.1) + projection returns correct field 1 values
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
// non-proto records pass through unchanged
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
// bucket_fraction(1.0) + projection works correctly
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
// simple (non-transpose) chunk + projection → all records unmodified
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
// nested field path [1, 2, 3] correctly included
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

// ---------------------------------------------------------------------------
// Multi-bucket projection: a small bucket_fraction splits the data buffers of
// wide records across several buckets, so projected reads only need to
// decompress the buckets holding the requested fields' buffers.
// ---------------------------------------------------------------------------

/// Narrow projection over enough wide records to span multiple buckets
/// (200 records ≈ 21 KB of data buffers vs a ~10.5 KB bucket at
/// bucket_fraction 0.01) must still return exact field values.
#[test]
fn narrow_projection_200_records_spans_multiple_buckets() {
    let records: Vec<Vec<u8>> = (0..200u64).map(make_wide_record).collect();
    let record_refs: Vec<&[u8]> = records.iter().map(|r| r.as_slice()).collect();

    let opts = WriterOptions::new()
        .transpose(true)
        .bucket_fraction(0.01)
        .compression(CompressionType::None);
    let data = write_records(&record_refs, opts);

    let proj = FieldProjection::new().add_field(Field::new(vec![1]));
    let results = read_all(&data, ReaderOptions::new().field_projection(proj));

    assert_eq!(results.len(), 200);
    for (i, rec) in results.iter().enumerate() {
        let val = find_varint_field(rec, 1).expect("field 1 present");
        assert_eq!(val, (i as u64) + 1, "field 1 value mismatch at {i}");
    }
}

/// Projecting a field whose data buffer sits in a later bucket must decode
/// correct values even though earlier buckets — holding only pruned fields'
/// buffers — are never decompressed.
#[test]
fn projected_field_in_late_bucket_with_pruned_earlier_buckets() {
    let records: Vec<Vec<u8>> = (0..200u64).map(make_wide_record).collect();
    let record_refs: Vec<&[u8]> = records.iter().map(|r| r.as_slice()).collect();

    let opts = WriterOptions::new()
        .transpose(true)
        .bucket_fraction(0.01) // many buckets
        .compression(CompressionType::None);
    let data = write_records(&record_refs, opts);

    // Project only field 15 (fixed64), whose buffer lands in a late bucket.
    let proj = FieldProjection::new().add_field(Field::new(vec![15]));
    let results = read_all(&data, ReaderOptions::new().field_projection(proj));
    assert_eq!(results.len(), 200);
    for (i, rec) in results.iter().enumerate() {
        let val = find_varint_field(rec, 15);
        assert!(val.is_some(), "field 15 missing at record {i}");
        let expected = (i as u64).wrapping_mul(15);
        assert_eq!(val.unwrap(), expected, "field 15 value mismatch at {i}");
    }
}

/// Project each of the five varint fields individually over multi-bucket
/// data: each pass needs a different subset of buckets, and each must decode
/// exact values.
#[test]
fn each_field_projected_individually_multi_bucket() {
    let records: Vec<Vec<u8>> = (0..200u64).map(make_wide_record).collect();
    let record_refs: Vec<&[u8]> = records.iter().map(|r| r.as_slice()).collect();

    let opts = WriterOptions::new()
        .transpose(true)
        .bucket_fraction(0.01)
        .compression(CompressionType::None);
    let data = write_records(&record_refs, opts);

    for field_num in 1..=5u32 {
        let proj = FieldProjection::new().add_field(Field::new(vec![field_num]));
        let results = read_all(&data, ReaderOptions::new().field_projection(proj));
        assert_eq!(results.len(), 200, "field {field_num}: wrong record count");
        for (i, rec) in results.iter().enumerate() {
            let val = find_varint_field(rec, field_num)
                .unwrap_or_else(|| panic!("field {field_num} missing at record {i}"));
            let expected = (i as u64) * (field_num as u64) + 1;
            assert_eq!(
                val, expected,
                "field {field_num} value mismatch at record {i}"
            );
        }
    }
}

/// Project two non-adjacent fields (1 = varint, 11 = fixed64) whose data
/// buffers land in different buckets; both must decode exact values.
#[test]
fn two_fields_from_distinct_buckets_projected() {
    let records: Vec<Vec<u8>> = (0..200u64).map(make_wide_record).collect();
    let record_refs: Vec<&[u8]> = records.iter().map(|r| r.as_slice()).collect();

    let opts = WriterOptions::new()
        .transpose(true)
        .bucket_fraction(0.01)
        .compression(CompressionType::None);
    let data = write_records(&record_refs, opts);

    let proj = FieldProjection::new()
        .add_field(Field::new(vec![1]))
        .add_field(Field::new(vec![11]));
    let results = read_all(&data, ReaderOptions::new().field_projection(proj));
    assert_eq!(results.len(), 200);
    for (i, rec) in results.iter().enumerate() {
        let v1 = find_varint_field(rec, 1).expect("field 1 present");
        assert_eq!(v1, (i as u64) + 1);
        let v11 = find_varint_field(rec, 11);
        assert!(v11.is_some(), "field 11 missing at record {i}");
        let expected_11 = (i as u64).wrapping_mul(11);
        assert_eq!(v11.unwrap(), expected_11, "field 11 value mismatch at {i}");
    }
}

/// Zstd-compressed buckets with a non-default bucket_fraction: the full read
/// must be byte-identical, and a narrow projection must decode exact values —
/// exercising per-bucket zstd decompression on a multi-bucket layout.
#[test]
#[cfg(feature = "zstd")]
fn zstd_multi_bucket_projection_and_full_read() {
    let records: Vec<Vec<u8>> = (0..200u64).map(make_wide_record).collect();
    let record_refs: Vec<&[u8]> = records.iter().map(|r| r.as_slice()).collect();

    let opts = WriterOptions::new()
        .transpose(true)
        .bucket_fraction(0.01)
        .compression(CompressionType::Zstd);
    let data = write_records(&record_refs, opts);

    let results_full = read_all(&data, ReaderOptions::new());
    assert_eq!(results_full.len(), 200);
    for (i, rec) in results_full.iter().enumerate() {
        assert_eq!(rec, &records[i], "zstd non-projected record {i}");
    }

    let proj = FieldProjection::new().add_field(Field::new(vec![6]));
    let results_proj = read_all(&data, ReaderOptions::new().field_projection(proj));
    assert_eq!(results_proj.len(), 200);
    for (i, rec) in results_proj.iter().enumerate() {
        let val = find_varint_field(rec, 6);
        assert!(val.is_some(), "zstd projected field 6 missing at {i}");
        let expected = (i as u32).wrapping_mul(6) as u64;
        assert_eq!(val.unwrap(), expected, "zstd field 6 value mismatch at {i}");
    }
}

// ---------------------------------------------------------------------------
// Skipped-submessage state must reset at every record boundary
// ---------------------------------------------------------------------------

/// Many records with nested submessages and a narrow projection. The
/// decoder tracks how deep it is inside a skipped submessage; if that depth
/// leaked across record boundaries, later records would decode incorrectly.
#[test]
fn skipped_submessage_state_resets_across_records() {
    let records: Vec<Vec<u8>> = (0..100u64)
        .map(|i| {
            let leaf = encode_varint_field(3, i);
            let mid = encode_string_field(2, &leaf);
            let mut r = encode_varint_field(1, i);
            r.extend_from_slice(&mid);
            r
        })
        .collect();
    let record_refs: Vec<&[u8]> = records.iter().map(|r| r.as_slice()).collect();

    let opts = WriterOptions::new()
        .transpose(true)
        .compression(CompressionType::None);
    let data = write_records(&record_refs, opts);

    // Project only field 1 — the submessage at field 2 must be entirely skipped.
    let proj = FieldProjection::new().add_field(Field::new(vec![1]));
    let results = read_all(&data, ReaderOptions::new().field_projection(proj));

    assert_eq!(results.len(), 100);
    for (i, rec) in results.iter().enumerate() {
        let field_numbers = proto_field_numbers(rec);
        assert_eq!(
            field_numbers,
            vec![1],
            "record {i}: only field 1 expected, got {:?}",
            field_numbers
        );
    }
}

// ---------------------------------------------------------------------------
// Deeply nested excluded submessages are suppressed in full
// ---------------------------------------------------------------------------

/// 4-level nesting: field 1 -> field 2 -> field 3 -> field 4 (leaf varint),
/// followed by a top-level sibling varint at field 5. Projecting only field 5
/// must suppress the entire nested structure at every level.
#[test]
fn deeply_nested_submessage_suppressed_by_sibling_projection() {
    let records: Vec<Vec<u8>> = (0..30u64)
        .map(|i| {
            let level3 = encode_varint_field(4, i);
            let level2 = encode_string_field(3, &level3);
            let level1 = encode_string_field(2, &level2);
            let nested = encode_string_field(1, &level1);

            let mut r = Vec::new();
            r.extend_from_slice(&nested);
            r.extend_from_slice(&encode_varint_field(5, i * 10));
            r
        })
        .collect();
    let record_refs: Vec<&[u8]> = records.iter().map(|r| r.as_slice()).collect();

    let opts = WriterOptions::new()
        .transpose(true)
        .compression(CompressionType::None);
    let data = write_records(&record_refs, opts);

    let proj = FieldProjection::new().add_field(Field::new(vec![5]));
    let results = read_all(&data, ReaderOptions::new().field_projection(proj));

    assert_eq!(results.len(), 30);
    for (i, rec) in results.iter().enumerate() {
        let field_numbers = proto_field_numbers(rec);
        assert_eq!(
            field_numbers,
            vec![5],
            "record {i}: only field 5 expected, got {:?}",
            field_numbers
        );
    }
}

/// 3-level nesting with a leaf projection inside the nesting. Projecting
/// path [1, 2, 3] must follow the path (keeping field 1 as the framed
/// parent) while excluding the top-level sibling at field 4.
#[test]
fn nested_leaf_projection_excludes_top_level_sibling() {
    let records: Vec<Vec<u8>> = (0..20u64)
        .map(|i| {
            let inner = encode_varint_field(3, i * 7);
            let mid = encode_string_field(2, &inner);
            let mut r = encode_string_field(1, &mid);
            // Top-level field 4 that should be excluded.
            r.extend_from_slice(&encode_varint_field(4, 999));
            r
        })
        .collect();
    let record_refs: Vec<&[u8]> = records.iter().map(|r| r.as_slice()).collect();

    let opts = WriterOptions::new()
        .transpose(true)
        .compression(CompressionType::None);
    let data = write_records(&record_refs, opts);

    let proj = FieldProjection::new().add_field(Field::new(vec![1, 2, 3]));
    let results = read_all(&data, ReaderOptions::new().field_projection(proj));

    assert_eq!(results.len(), 20);
    for (i, rec) in results.iter().enumerate() {
        let top_field_numbers = proto_field_numbers(rec);
        assert!(
            !top_field_numbers.contains(&4),
            "record {i}: field 4 should be absent, got {:?}",
            top_field_numbers
        );
        assert!(
            top_field_numbers.contains(&1),
            "record {i}: field 1 should be present as parent, got {:?}",
            top_field_numbers
        );
    }
}

// ---------------------------------------------------------------------------
// Brotli-compressed buckets: excluded fields must be absent
// ---------------------------------------------------------------------------

/// Projection under brotli compression: the projected field must be the
/// only field present — excluded siblings (varint and bytes) must be absent.
#[test]
#[cfg(feature = "brotli")]
fn brotli_projection_excludes_other_fields() {
    let records: Vec<Vec<u8>> = (0..50u64)
        .map(|i| {
            let mut r = encode_varint_field(1, i);
            r.extend_from_slice(&encode_fixed32_field(2, i as u32));
            r.extend_from_slice(&encode_string_field(3, &[0xBB; 20]));
            r
        })
        .collect();
    let record_refs: Vec<&[u8]> = records.iter().map(|r| r.as_slice()).collect();

    let opts = WriterOptions::new()
        .transpose(true)
        .compression(CompressionType::Brotli);
    let data = write_records(&record_refs, opts);

    let proj = FieldProjection::new().add_field(Field::new(vec![2]));
    let results = read_all(&data, ReaderOptions::new().field_projection(proj));

    assert_eq!(results.len(), 50);
    for (i, rec) in results.iter().enumerate() {
        let field_numbers = proto_field_numbers(rec);
        assert_eq!(field_numbers, vec![2], "record {i}: only field 2 expected");
    }
}

// ---------------------------------------------------------------------------
// Existence-only projection: tag + zeroed value for every wire type
// ---------------------------------------------------------------------------

/// A record carrying all four wire types, every field projected as
/// existence-only: each field must come back as its tag plus a zeroed
/// value (varint 0x00, fixed64 eight zero bytes, length-delimited zero
/// length, fixed32 four zero bytes).
#[test]
fn existence_only_all_wire_types_zeroed() {
    let records: Vec<Vec<u8>> = (0..10u64)
        .map(|i| {
            let mut r = encode_varint_field(1, i * 100);
            r.extend(encode_fixed64_field(2, i * 200));
            r.extend(encode_string_field(3, format!("data-{i}").as_bytes()));
            r.extend(encode_fixed32_field(4, i as u32));
            r
        })
        .collect();
    let data = write_transpose(&records, CompressionType::None);

    let proj = FieldProjection::new()
        .add_field(Field::new(vec![1]).existence_only())
        .add_field(Field::new(vec![2]).existence_only())
        .add_field(Field::new(vec![3]).existence_only())
        .add_field(Field::new(vec![4]).existence_only());
    let results = read_projected(&data, &proj);

    assert_eq!(results.len(), 10);
    for (i, rec) in results.iter().enumerate() {
        let fields = parse_fields(rec);
        assert_eq!(
            fields.len(),
            4,
            "record {i}: should have 4 existence-only fields"
        );

        // Field 1: varint -> 0x00
        assert_eq!(fields[0].0, 1);
        assert_eq!(fields[0].1, 0); // varint
        assert_eq!(fields[0].2, vec![0x00]);

        // Field 2: fixed64 -> 8 zero bytes
        assert_eq!(fields[1].0, 2);
        assert_eq!(fields[1].1, 1); // fixed64
        assert_eq!(fields[1].2, vec![0x00; 8]);

        // Field 3: length-delimited -> zero-length
        assert_eq!(fields[2].0, 3);
        assert_eq!(fields[2].1, 2); // length-delimited
        assert_eq!(fields[2].2, vec![0x00]);

        // Field 4: fixed32 -> 4 zero bytes
        assert_eq!(fields[3].0, 4);
        assert_eq!(fields[3].1, 5); // fixed32
        assert_eq!(fields[3].2, vec![0x00; 4]);
    }
}

/// Existence-only must skip a maximum-length (10-byte, u64::MAX) varint
/// value correctly and still emit tag + 0x00.
#[test]
fn existence_only_max_length_varint_zeroed() {
    let records: Vec<Vec<u8>> = (0..5u64)
        .map(|_| {
            let mut r = encode_varint_field(1, u64::MAX); // 10-byte varint
            r.extend(encode_varint_field(2, 42));
            r
        })
        .collect();
    let data = write_transpose(&records, CompressionType::None);

    let proj = FieldProjection::new().add_field(Field::new(vec![1]).existence_only());
    let results = read_projected(&data, &proj);

    assert_eq!(results.len(), 5);
    for (i, rec) in results.iter().enumerate() {
        let fields = parse_fields(rec);
        assert_eq!(fields.len(), 1, "record {i}");
        assert_eq!(fields[0].0, 1);
        assert_eq!(
            fields[0].2,
            vec![0x00],
            "record {i}: should be zeroed varint"
        );
    }
}

/// Small varint values (0..10, including a real 0) may be stored inline in
/// the encoder's tag data rather than in a data buffer. Existence-only must
/// still output tag + 0x00 for every record regardless of representation.
#[test]
fn existence_only_inline_varint_zeroed() {
    let records: Vec<Vec<u8>> = (0..10u64)
        .map(|i| {
            let mut r = encode_varint_field(1, i); // small values likely inline
            r.extend(encode_varint_field(2, 42));
            r
        })
        .collect();
    let data = write_transpose(&records, CompressionType::None);

    let proj = FieldProjection::new().add_field(Field::new(vec![1]).existence_only());
    let results = read_projected(&data, &proj);

    assert_eq!(results.len(), 10);
    for (i, rec) in results.iter().enumerate() {
        let fields = parse_fields(rec);
        assert_eq!(fields.len(), 1, "record {i}");
        assert_eq!(fields[0].0, 1);
        assert_eq!(fields[0].1, 0); // varint
        assert_eq!(
            fields[0].2,
            vec![0x00],
            "record {i}: inline varint existence-only should be 0x00"
        );
    }
}

/// Existence-only with a field number (100) whose tag needs a multi-byte
/// varint: the emitted tag must round-trip the full field number.
#[test]
fn existence_only_multibyte_tag_field_number() {
    let records: Vec<Vec<u8>> = (0..10u64)
        .map(|i| {
            let mut r = encode_varint_field(100, i * 1000);
            r.extend(encode_varint_field(1, i));
            r
        })
        .collect();
    let data = write_transpose(&records, CompressionType::None);

    let proj = FieldProjection::new().add_field(Field::new(vec![100]).existence_only());
    let results = read_projected(&data, &proj);

    assert_eq!(results.len(), 10);
    for (i, rec) in results.iter().enumerate() {
        let fields = parse_fields(rec);
        assert_eq!(fields.len(), 1, "record {i}");
        assert_eq!(fields[0].0, 100);
        assert_eq!(fields[0].1, 0); // varint
        assert_eq!(
            fields[0].2,
            vec![0x00],
            "record {i}: high field number existence-only"
        );
    }
}

/// Existence-only fixed32 and fixed64 fields must consume their data
/// buffers without emitting them: a fully-included field that follows must
/// still decode its real value from the correct buffer position.
#[test]
fn existence_only_fixed_fields_preserve_following_value() {
    let records: Vec<Vec<u8>> = (0..10u64)
        .map(|i| {
            let mut r = encode_fixed32_field(1, 0xFFFFFFFF);
            r.extend(encode_fixed64_field(2, u64::MAX));
            r.extend(encode_varint_field(3, i));
            r
        })
        .collect();
    let data = write_transpose(&records, CompressionType::None);

    let proj = FieldProjection::new()
        .add_field(Field::new(vec![1]).existence_only())
        .add_field(Field::new(vec![2]).existence_only())
        .add_field(Field::new(vec![3]));
    let results = read_projected(&data, &proj);

    assert_eq!(results.len(), 10);
    for (i, rec) in results.iter().enumerate() {
        let fields = parse_fields(rec);
        assert_eq!(fields.len(), 3, "record {i}");
        // Field 1: fixed32, zeroed
        assert_eq!(fields[0].0, 1);
        assert_eq!(fields[0].2, vec![0x00; 4]);
        // Field 2: fixed64, zeroed
        assert_eq!(fields[1].0, 2);
        assert_eq!(fields[1].2, vec![0x00; 8]);
        // Field 3: fully included, correct value
        assert_eq!(fields[2].0, 3);
        let (val, _) = decode_varint(&fields[2].2).expect("field 3 varint");
        assert_eq!(val, i as u64, "record {i}: field 3 value");
    }
}

/// Existence-only on a nested path ([2, 1]): the parent submessage is
/// framed, the inner leaf is zeroed, and the top-level sibling is dropped.
#[test]
fn existence_only_nested_leaf_zeroes_inner_value() {
    // Record: field 1 (varint), field 2 (submessage containing field 1 varint).
    let records: Vec<Vec<u8>> = (0..10u64)
        .map(|i| {
            let inner = encode_varint_field(1, i * 999);
            let mut r = encode_varint_field(1, i);
            r.extend(encode_string_field(2, &inner));
            r
        })
        .collect();
    let data = write_transpose(&records, CompressionType::None);

    let proj = FieldProjection::new().add_field(Field::new(vec![2, 1]).existence_only());
    let results = read_projected(&data, &proj);

    assert_eq!(results.len(), 10);
    for (i, rec) in results.iter().enumerate() {
        // Should have field 2 (submessage) containing a zeroed field 1.
        let outer_fields = parse_fields(rec);
        // Top-level field 1 should be excluded; only field 2 present.
        assert!(
            !outer_fields.is_empty(),
            "record {i}: should have submessage field 2"
        );
        // Find field 2.
        let f2 = outer_fields.iter().find(|(fn_num, _, _)| *fn_num == 2);
        assert!(
            f2.is_some(),
            "record {i}: field 2 (submessage) should be present"
        );
        let (_, wt, val_bytes) = f2.unwrap();
        assert_eq!(*wt, 2, "record {i}: field 2 should be length-delimited");
        // Parse the inner submessage.
        let (inner_len, lc) = decode_varint(val_bytes).expect("inner length varint");
        let inner_data = &val_bytes[lc..lc + inner_len as usize];
        let inner_fields = parse_fields(inner_data);
        assert_eq!(
            inner_fields.len(),
            1,
            "record {i}: inner submessage should have 1 field"
        );
        assert_eq!(inner_fields[0].0, 1, "record {i}: inner field number");
        assert_eq!(
            inner_fields[0].2,
            vec![0x00],
            "record {i}: inner varint should be zeroed"
        );
    }
}

// ---------------------------------------------------------------------------
// Existence-only under compressed buckets
// ---------------------------------------------------------------------------

/// Existence-only plus a fully-included field over Snappy-compressed
/// buckets: the zeroed field and the included field's framing must both be
/// correct after decompression.
#[test]
#[cfg(feature = "snappy")]
fn existence_only_with_snappy_compression() {
    let records: Vec<Vec<u8>> = (0..20u64)
        .map(|i| {
            let mut r = encode_varint_field(1, i * 7);
            r.extend(encode_fixed64_field(2, i * 1000));
            r.extend(encode_string_field(3, &[0xBB; 50]));
            r
        })
        .collect();
    let data = write_transpose(&records, CompressionType::Snappy);

    let proj = FieldProjection::new()
        .add_field(Field::new(vec![1]).existence_only())
        .add_field(Field::new(vec![3])); // include field 3 fully
    let results = read_projected(&data, &proj);

    assert_eq!(results.len(), 20);
    for (i, rec) in results.iter().enumerate() {
        let fields = parse_fields(rec);
        assert_eq!(fields.len(), 2, "record {i}: should have 2 fields");
        // Field 1: existence-only varint
        assert_eq!(fields[0].0, 1);
        assert_eq!(fields[0].2, vec![0x00]);
        // Field 3: included fully
        assert_eq!(fields[1].0, 3);
        assert_eq!(fields[1].1, 2); // length-delimited
    }
}

/// Existence-only over Zstd-compressed buckets must still emit tag + 0x00.
#[test]
#[cfg(feature = "zstd")]
fn existence_only_with_zstd_compression() {
    let records: Vec<Vec<u8>> = (0..20u64)
        .map(|i| {
            let mut r = encode_varint_field(1, i);
            r.extend(encode_string_field(2, &[0xAA; 100]));
            r
        })
        .collect();
    let data = write_transpose(&records, CompressionType::Zstd);

    let proj = FieldProjection::new().add_field(Field::new(vec![1]).existence_only());
    let results = read_projected(&data, &proj);

    assert_eq!(results.len(), 20);
    for (i, rec) in results.iter().enumerate() {
        let fields = parse_fields(rec);
        assert_eq!(fields.len(), 1, "record {i}");
        assert_eq!(fields[0].0, 1);
        assert_eq!(fields[0].2, vec![0x00], "record {i}");
    }
}

// ---------------------------------------------------------------------------
// Full (explicitly enumerated) projections are byte-identical
// ---------------------------------------------------------------------------

/// Explicitly enumerating every top-level field over records with
/// doubly-nested submessages and top-level siblings must reproduce each
/// record byte-for-byte.
#[test]
fn full_projection_nested_submessages_byte_identical() {
    let records: Vec<Vec<u8>> = (0..10u64)
        .map(|i| {
            let inner = encode_varint_field(1, i);
            let middle = encode_string_field(2, &inner);
            let mut outer = encode_varint_field(1, i + 100);
            outer.extend(encode_string_field(2, &middle));
            outer.extend(encode_varint_field(3, i + 200));
            outer
        })
        .collect();
    let data = write_transpose(&records, CompressionType::None);

    let no_proj = read_all(&data, ReaderOptions::new());

    // Include all top-level fields (1, 2, 3) -- field 2 is a submessage that
    // should pass through fully.
    let proj = FieldProjection::new()
        .add_field(Field::new(vec![1]))
        .add_field(Field::new(vec![2]))
        .add_field(Field::new(vec![3]));
    let with_proj = read_projected(&data, &proj);

    assert_eq!(no_proj.len(), with_proj.len());
    for (i, (a, b)) in no_proj.iter().zip(with_proj.iter()).enumerate() {
        assert_eq!(
            a, b,
            "record {i}: full nested projection should be byte-identical"
        );
    }
}

/// An explicitly enumerated full projection over Brotli-compressed buckets
/// must be byte-identical to a non-projected read.
#[test]
#[cfg(feature = "brotli")]
fn explicit_full_projection_brotli_byte_identical() {
    let records: Vec<Vec<u8>> = (0..30u64)
        .map(|i| {
            let mut r = encode_varint_field(1, i);
            r.extend(encode_fixed32_field(2, i as u32));
            r.extend(encode_string_field(3, format!("data-{i}").as_bytes()));
            r
        })
        .collect();
    let data = write_transpose(&records, CompressionType::Brotli);

    let no_proj = read_all(&data, ReaderOptions::new());
    let proj = FieldProjection::new()
        .add_field(Field::new(vec![1]))
        .add_field(Field::new(vec![2]))
        .add_field(Field::new(vec![3]));
    let with_proj = read_projected(&data, &proj);

    assert_eq!(no_proj.len(), with_proj.len());
    for (i, (a, b)) in no_proj.iter().zip(with_proj.iter()).enumerate() {
        assert_eq!(
            a, b,
            "record {i}: full projection with Brotli should be byte-identical"
        );
    }
}

// ---------------------------------------------------------------------------
// Empty projection and non-proto passthrough edge cases
// ---------------------------------------------------------------------------

/// Non-proto records must pass through unchanged even under an EMPTY
/// projection, where every proto field would be pruned.
#[test]
fn nonproto_passthrough_with_empty_projection() {
    let nonproto: Vec<Vec<u8>> = (0..5u8).map(|i| vec![0xFF, i]).collect();
    let data = write_transpose(&nonproto, CompressionType::None);

    let proj = FieldProjection::new();
    let results = read_projected(&data, &proj);

    assert_eq!(results.len(), nonproto.len());
    for (i, (got, expected)) in results.iter().zip(nonproto.iter()).enumerate() {
        assert_eq!(
            got, expected,
            "record {i}: non-proto record should pass through even with empty projection"
        );
    }
}

/// An empty projection over data spanning multiple chunks: per-chunk decoder
/// setup with a fully-pruning projection must still yield the right number
/// of (empty) records.
#[test]
fn empty_projection_across_multiple_chunks() {
    // ~200-byte records with a 2000-byte chunk size force multiple chunks.
    let records: Vec<Vec<u8>> = (0..200u64)
        .map(|i| {
            let mut r = encode_varint_field(1, i);
            r.extend(encode_string_field(2, &[0xCC; 200]));
            r
        })
        .collect();
    let record_refs: Vec<&[u8]> = records.iter().map(|r| r.as_slice()).collect();
    let data = write_records(
        &record_refs,
        WriterOptions::new()
            .transpose(true)
            .compression(CompressionType::None)
            .chunk_size(2000),
    );

    let proj = FieldProjection::new();
    let results = read_projected(&data, &proj);

    assert_eq!(results.len(), 200);
    for (i, rec) in results.iter().enumerate() {
        assert!(
            rec.is_empty(),
            "record {i}: empty projection should produce empty record"
        );
    }
}
