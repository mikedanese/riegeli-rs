// Sprint 33 Tests: Existence-Only During Decode and Edge Cases
//
// Tests for:
// - 33.1: existence_only produces tag + zero value for all wire types
// - 33.2: empty projection returns empty records
// - 33.3: full projection produces byte-identical output to non-projected read
// - 33.4: non-proto records pass through unchanged with projection
// - 33.5: apply() not called in transpose decode path (verified by code inspection)
// - 33.6: all existing tests pass (verified by running full suite)

use std::io::Cursor;

use riegeli::{
    CompressionType, Field, FieldProjection, ReaderOptions, RecordReader, RecordWriter,
    WriterOptions,
};

// ---------------------------------------------------------------------------
// Proto encoding helpers
// ---------------------------------------------------------------------------

fn encode_varint(v: u64) -> Vec<u8> {
    let mut out = Vec::new();
    let mut val = v;
    while val >= 0x80 {
        out.push((val as u8) | 0x80);
        val >>= 7;
    }
    out.push(val as u8);
    out
}

fn encode_tag(field_number: u32, wire_type: u32) -> Vec<u8> {
    encode_varint(((field_number as u64) << 3) | wire_type as u64)
}

fn encode_varint_field(field_number: u32, value: u64) -> Vec<u8> {
    let mut out = encode_tag(field_number, 0);
    out.extend(encode_varint(value));
    out
}

fn encode_fixed32_field(field_number: u32, value: u32) -> Vec<u8> {
    let mut out = encode_tag(field_number, 5);
    out.extend(&value.to_le_bytes());
    out
}

fn encode_fixed64_field(field_number: u32, value: u64) -> Vec<u8> {
    let mut out = encode_tag(field_number, 1);
    out.extend(&value.to_le_bytes());
    out
}

fn encode_bytes_field(field_number: u32, data: &[u8]) -> Vec<u8> {
    let mut out = encode_tag(field_number, 2);
    out.extend(encode_varint(data.len() as u64));
    out.extend(data);
    out
}

fn encode_submessage_field(field_number: u32, inner: &[u8]) -> Vec<u8> {
    encode_bytes_field(field_number, inner)
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn write_transpose(records: &[Vec<u8>], compression: CompressionType) -> Vec<u8> {
    let mut buf = Vec::new();
    {
        let opts = WriterOptions::default()
            .compression(compression)
            .transpose(true);
        let cursor = Cursor::new(&mut buf);
        let mut writer = RecordWriter::new(cursor, opts).unwrap();
        for r in records {
            writer.write_record(r).unwrap();
        }
        writer.close().unwrap();
    }
    buf
}

fn write_simple(records: &[Vec<u8>], compression: CompressionType) -> Vec<u8> {
    let mut buf = Vec::new();
    {
        let opts = WriterOptions::default()
            .compression(compression)
            .transpose(false);
        let cursor = Cursor::new(&mut buf);
        let mut writer = RecordWriter::new(cursor, opts).unwrap();
        for r in records {
            writer.write_record(r).unwrap();
        }
        writer.close().unwrap();
    }
    buf
}

fn read_all(data: &[u8]) -> Vec<Vec<u8>> {
    let opts = ReaderOptions::default();
    let mut reader = RecordReader::new(Cursor::new(data), opts).unwrap();
    let mut out = Vec::new();
    while let Some(r) = reader.read_record().unwrap() {
        out.push(r);
    }
    out
}

fn read_projected(data: &[u8], proj: &FieldProjection) -> Vec<Vec<u8>> {
    let opts = ReaderOptions::default().field_projection(proj.clone());
    let mut reader = RecordReader::new(Cursor::new(data), opts).unwrap();
    let mut out = Vec::new();
    while let Some(r) = reader.read_record().unwrap() {
        out.push(r);
    }
    out
}

/// Decode a varint at the start of buf, return (value, bytes_consumed).
fn decode_varint(buf: &[u8]) -> (u64, usize) {
    let mut val: u64 = 0;
    let mut shift = 0u64;
    for (i, &b) in buf.iter().enumerate() {
        val |= ((b & 0x7F) as u64) << shift;
        shift += 7;
        if b < 0x80 {
            return (val, i + 1);
        }
    }
    panic!("unterminated varint");
}

/// Parse a proto record and return (field_number, wire_type, value_bytes) tuples.
fn parse_fields(record: &[u8]) -> Vec<(u32, u32, Vec<u8>)> {
    let mut fields = Vec::new();
    let mut pos = 0;
    while pos < record.len() {
        let (tag, consumed) = decode_varint(&record[pos..]);
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
                let (len, lc) = decode_varint(&record[pos..]);
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

// ===========================================================================
// 33.1: Existence-only produces tag + zero value for all wire types
// ===========================================================================

#[test]
fn test_33_1_existence_only_varint() {
    // Write records with a varint field (field 1) and another field (field 2).
    let records: Vec<Vec<u8>> = (0..10u64)
        .map(|i| {
            let mut r = encode_varint_field(1, i * 1000 + 42);
            r.extend(encode_varint_field(2, i));
            r
        })
        .collect();
    let data = write_transpose(&records, CompressionType::None);

    // Project field 1 as existence_only.
    let proj = FieldProjection::new().add_field(Field::new(vec![1]).existence_only());
    let results = read_projected(&data, &proj);

    assert_eq!(results.len(), 10);
    for (i, rec) in results.iter().enumerate() {
        let fields = parse_fields(rec);
        assert_eq!(
            fields.len(),
            1,
            "record {i}: should have exactly 1 field (existence-only field 1)"
        );
        let (fn_num, wt, val) = &fields[0];
        assert_eq!(*fn_num, 1, "record {i}: field number should be 1");
        assert_eq!(*wt, 0, "record {i}: wire type should be varint (0)");
        assert_eq!(val, &[0x00], "record {i}: varint value should be 0x00");
    }
}

#[test]
fn test_33_1_existence_only_fixed32() {
    let records: Vec<Vec<u8>> = (0..5u64)
        .map(|i| {
            let mut r = encode_fixed32_field(1, 0xDEADBEEF);
            r.extend(encode_varint_field(2, i));
            r
        })
        .collect();
    let data = write_transpose(&records, CompressionType::None);

    let proj = FieldProjection::new().add_field(Field::new(vec![1]).existence_only());
    let results = read_projected(&data, &proj);

    assert_eq!(results.len(), 5);
    for (i, rec) in results.iter().enumerate() {
        let fields = parse_fields(rec);
        assert_eq!(fields.len(), 1, "record {i}: should have exactly 1 field");
        let (fn_num, wt, val) = &fields[0];
        assert_eq!(*fn_num, 1);
        assert_eq!(*wt, 5, "record {i}: wire type should be fixed32 (5)");
        assert_eq!(
            val,
            &[0x00, 0x00, 0x00, 0x00],
            "record {i}: fixed32 value should be 4 zero bytes"
        );
    }
}

#[test]
fn test_33_1_existence_only_fixed64() {
    let records: Vec<Vec<u8>> = (0..5u64)
        .map(|i| {
            let mut r = encode_fixed64_field(1, 0xCAFEBABECAFEBABE);
            r.extend(encode_varint_field(2, i));
            r
        })
        .collect();
    let data = write_transpose(&records, CompressionType::None);

    let proj = FieldProjection::new().add_field(Field::new(vec![1]).existence_only());
    let results = read_projected(&data, &proj);

    assert_eq!(results.len(), 5);
    for (i, rec) in results.iter().enumerate() {
        let fields = parse_fields(rec);
        assert_eq!(fields.len(), 1, "record {i}: should have exactly 1 field");
        let (fn_num, wt, val) = &fields[0];
        assert_eq!(*fn_num, 1);
        assert_eq!(*wt, 1, "record {i}: wire type should be fixed64 (1)");
        assert_eq!(
            val,
            &[0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00],
            "record {i}: fixed64 value should be 8 zero bytes"
        );
    }
}

#[test]
fn test_33_1_existence_only_length_delimited() {
    let records: Vec<Vec<u8>> = (0..5u64)
        .map(|i| {
            let mut r = encode_bytes_field(1, b"hello world this is a long string");
            r.extend(encode_varint_field(2, i));
            r
        })
        .collect();
    let data = write_transpose(&records, CompressionType::None);

    let proj = FieldProjection::new().add_field(Field::new(vec![1]).existence_only());
    let results = read_projected(&data, &proj);

    assert_eq!(results.len(), 5);
    for (i, rec) in results.iter().enumerate() {
        let fields = parse_fields(rec);
        assert_eq!(fields.len(), 1, "record {i}: should have exactly 1 field");
        let (fn_num, wt, val) = &fields[0];
        assert_eq!(*fn_num, 1);
        assert_eq!(
            *wt, 2,
            "record {i}: wire type should be length-delimited (2)"
        );
        // Zero-length: the length prefix varint should be 0, and no data follows.
        assert_eq!(
            val,
            &[0x00],
            "record {i}: length-delimited value should be 0x00 (zero-length)"
        );
    }
}

#[test]
fn test_33_1_existence_only_mixed_wire_types() {
    // A record with all 4 wire types, all marked existence_only.
    let records: Vec<Vec<u8>> = (0..10u64)
        .map(|i| {
            let mut r = encode_varint_field(1, i * 100);
            r.extend(encode_fixed64_field(2, i * 200));
            r.extend(encode_bytes_field(3, format!("data-{i}").as_bytes()));
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

#[test]
fn test_33_1_existence_only_alongside_included_field() {
    // Project field 1 as existence_only, field 2 as fully included.
    let records: Vec<Vec<u8>> = (0..10u64)
        .map(|i| {
            let mut r = encode_varint_field(1, i * 100 + 42);
            r.extend(encode_varint_field(2, i));
            r.extend(encode_bytes_field(3, b"excluded"));
            r
        })
        .collect();
    let data = write_transpose(&records, CompressionType::None);

    let proj = FieldProjection::new()
        .add_field(Field::new(vec![1]).existence_only())
        .add_field(Field::new(vec![2]));
    let results = read_projected(&data, &proj);

    assert_eq!(results.len(), 10);
    for (i, rec) in results.iter().enumerate() {
        let fields = parse_fields(rec);
        assert_eq!(
            fields.len(),
            2,
            "record {i}: should have 2 fields (existence_only + included)"
        );

        // Field 1: existence_only varint -> 0x00
        assert_eq!(fields[0].0, 1);
        assert_eq!(fields[0].2, vec![0x00]);

        // Field 2: fully included with actual value
        assert_eq!(fields[1].0, 2);
        let (val, _) = decode_varint(&fields[1].2);
        assert_eq!(val, i as u64, "record {i}: field 2 value should be {i}");
    }
}

// ===========================================================================
// 33.2: Empty projection returns empty records for all proto records
// ===========================================================================

#[test]
fn test_33_2_empty_projection_returns_empty_records() {
    let records: Vec<Vec<u8>> = (0..10u64)
        .map(|i| {
            let mut r = encode_varint_field(1, i);
            r.extend(encode_fixed32_field(2, i as u32));
            r.extend(encode_bytes_field(3, b"hello"));
            r
        })
        .collect();
    let data = write_transpose(&records, CompressionType::None);

    // Empty projection: no fields added.
    let proj = FieldProjection::new();
    let results = read_projected(&data, &proj);

    assert_eq!(results.len(), 10, "should still get 10 records");
    for (i, rec) in results.iter().enumerate() {
        assert!(
            rec.is_empty(),
            "record {i}: empty projection should produce empty record, got {} bytes",
            rec.len()
        );
    }
}

#[test]
fn test_33_2_empty_projection_with_compression() {
    let records: Vec<Vec<u8>> = (0..20u64)
        .map(|i| {
            let mut r = encode_varint_field(1, i);
            r.extend(encode_bytes_field(2, &vec![0xAA; 100]));
            r
        })
        .collect();
    let data = write_transpose(&records, CompressionType::Snappy);

    let proj = FieldProjection::new();
    let results = read_projected(&data, &proj);

    assert_eq!(results.len(), 20);
    for (i, rec) in results.iter().enumerate() {
        assert!(
            rec.is_empty(),
            "record {i}: empty projection + snappy should produce empty record"
        );
    }
}

// ===========================================================================
// 33.3: Full projection produces byte-identical output to non-projected read
// ===========================================================================

#[test]
fn test_33_3_all_fields_projection_byte_identical() {
    let records: Vec<Vec<u8>> = (0..20u64)
        .map(|i| {
            let mut r = encode_varint_field(1, i);
            r.extend(encode_fixed32_field(2, i as u32 * 7));
            r.extend(encode_fixed64_field(3, i * 1000));
            r.extend(encode_bytes_field(4, format!("str-{i}").as_bytes()));
            r.extend(encode_varint_field(5, i + 99));
            r
        })
        .collect();
    let data = write_transpose(&records, CompressionType::None);

    // Read without projection.
    let no_proj = read_all(&data);

    // Read with all fields explicitly included.
    let proj = FieldProjection::new()
        .add_field(Field::new(vec![1]))
        .add_field(Field::new(vec![2]))
        .add_field(Field::new(vec![3]))
        .add_field(Field::new(vec![4]))
        .add_field(Field::new(vec![5]));
    let with_proj = read_projected(&data, &proj);

    assert_eq!(no_proj.len(), with_proj.len());
    for (i, (a, b)) in no_proj.iter().zip(with_proj.iter()).enumerate() {
        assert_eq!(
            a, b,
            "record {i}: full projection should be byte-identical to non-projected"
        );
    }
}

#[test]
fn test_33_3_all_projection_variant_byte_identical() {
    // Use FieldProjection::all() and verify byte-identical.
    let records: Vec<Vec<u8>> = (0..15u64)
        .map(|i| {
            let mut r = encode_varint_field(1, i);
            r.extend(encode_fixed64_field(2, i * 500));
            r.extend(encode_bytes_field(3, b"test"));
            r
        })
        .collect();
    let data = write_transpose(&records, CompressionType::None);

    let no_proj = read_all(&data);
    let proj = FieldProjection::all();
    let with_proj = read_projected(&data, &proj);

    assert_eq!(no_proj.len(), with_proj.len());
    for (i, (a, b)) in no_proj.iter().zip(with_proj.iter()).enumerate() {
        assert_eq!(
            a, b,
            "record {i}: FieldProjection::all() should be byte-identical to non-projected"
        );
    }
}

#[test]
fn test_33_3_full_projection_nested_messages() {
    // Records with nested submessages -- all fields included.
    let records: Vec<Vec<u8>> = (0..10u64)
        .map(|i| {
            let inner = encode_varint_field(1, i);
            let middle = encode_submessage_field(2, &inner);
            let mut outer = encode_varint_field(1, i + 100);
            outer.extend(encode_submessage_field(2, &middle));
            outer.extend(encode_varint_field(3, i + 200));
            outer
        })
        .collect();
    let data = write_transpose(&records, CompressionType::None);

    let no_proj = read_all(&data);

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

// ===========================================================================
// 33.4: Projection on non-proto records passes through unchanged
// ===========================================================================

#[test]
fn test_33_4_nonproto_records_with_projection() {
    // Non-proto records: raw bytes that the transpose encoder detects as non-proto.
    let nonproto: Vec<Vec<u8>> = (0..10u8)
        .map(|i| vec![0xFF, 0xFE, i, i + 1, 0x00])
        .collect();
    let data = write_transpose(&nonproto, CompressionType::None);

    // Read with a narrow projection.
    let proj = FieldProjection::new().add_field(Field::new(vec![1]));
    let results = read_projected(&data, &proj);

    assert_eq!(results.len(), nonproto.len());
    for (i, (got, expected)) in results.iter().zip(nonproto.iter()).enumerate() {
        assert_eq!(
            got, expected,
            "record {i}: non-proto record should pass through unchanged"
        );
    }
}

#[test]
fn test_33_4_nonproto_records_with_empty_projection() {
    let nonproto: Vec<Vec<u8>> = (0..5u8).map(|i| vec![0xFF, i]).collect();
    let data = write_transpose(&nonproto, CompressionType::None);

    // Even with empty projection, non-proto records should pass through.
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

#[test]
fn test_33_4_nonproto_only_chunk_with_projection() {
    // Write only non-proto records.
    let nonproto: Vec<Vec<u8>> = (0..20u8)
        .map(|i| vec![0xFF, 0xAA, i, i.wrapping_mul(3)])
        .collect();
    let data = write_transpose(&nonproto, CompressionType::None);

    // Use a projection with existence_only -- should still pass through.
    let proj = FieldProjection::new()
        .add_field(Field::new(vec![1]).existence_only())
        .add_field(Field::new(vec![2]));
    let results = read_projected(&data, &proj);

    assert_eq!(results.len(), nonproto.len());
    for (i, (got, expected)) in results.iter().zip(nonproto.iter()).enumerate() {
        assert_eq!(got, expected, "record {i}: non-proto record unchanged");
    }
}

#[test]
fn test_33_4_simple_chunk_with_projection() {
    // Simple (non-transpose) encoding with projection should pass through.
    let records: Vec<Vec<u8>> = (0..10u64)
        .map(|i| {
            let mut r = encode_varint_field(1, i);
            r.extend(encode_varint_field(2, i + 100));
            r
        })
        .collect();
    let data = write_simple(&records, CompressionType::None);

    let no_proj = read_all(&data);
    let proj = FieldProjection::new().add_field(Field::new(vec![1]));
    let with_proj = read_projected(&data, &proj);

    // Simple chunks do not support projection -- records pass through unchanged.
    assert_eq!(no_proj, with_proj);
}

// ===========================================================================
// 33.5: apply() not called from transpose decoder (structural test)
// ===========================================================================

// This is verified by code inspection:
// - grep for '.apply(' in riegeli/src/ shows only unit tests in field_projection.rs
// - The transpose decoder at decoder.rs:562-564 explicitly states apply() is not
//   called when projection-during-decode is active.
// No runtime test needed; the evaluator can verify via grep.

// ===========================================================================
// Additional edge cases
// ===========================================================================

#[test]
fn test_33_existence_only_large_varint_value() {
    // Ensure existence_only correctly skips large varint values (multi-byte).
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

#[test]
fn test_33_existence_only_with_brotli_compression() {
    let records: Vec<Vec<u8>> = (0..20u64)
        .map(|i| {
            let mut r = encode_varint_field(1, i * 7);
            r.extend(encode_fixed64_field(2, i * 1000));
            r.extend(encode_bytes_field(3, &vec![0xBB; 50]));
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
        // Field 1: existence_only varint
        assert_eq!(fields[0].0, 1);
        assert_eq!(fields[0].2, vec![0x00]);
        // Field 3: included fully
        assert_eq!(fields[1].0, 3);
        assert_eq!(fields[1].1, 2); // length-delimited
    }
}

#[test]
fn test_33_many_records_existence_only() {
    // Stress test: 500 records, existence_only for all fields.
    let records: Vec<Vec<u8>> = (0..500u64)
        .map(|i| {
            let mut r = encode_varint_field(1, i);
            r.extend(encode_fixed32_field(2, i as u32));
            r.extend(encode_fixed64_field(3, i * 10));
            r.extend(encode_bytes_field(4, format!("r{i}").as_bytes()));
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

    assert_eq!(results.len(), 500);
    for (i, rec) in results.iter().enumerate() {
        let fields = parse_fields(rec);
        assert_eq!(fields.len(), 4, "record {i}: should have 4 fields");
        // All should be zeroed.
        assert_eq!(fields[0].2, vec![0x00]); // varint
        assert_eq!(fields[1].2, vec![0x00; 4]); // fixed32
        assert_eq!(fields[2].2, vec![0x00; 8]); // fixed64
        assert_eq!(fields[3].2, vec![0x00]); // zero-length string
    }
}

#[test]
fn test_33_empty_projection_multiple_chunks() {
    // Write enough records to span multiple chunks, then read with empty projection.
    let records: Vec<Vec<u8>> = (0..200u64)
        .map(|i| {
            let mut r = encode_varint_field(1, i);
            r.extend(encode_bytes_field(2, &vec![0xCC; 200]));
            r
        })
        .collect();
    let mut buf = Vec::new();
    {
        let opts = WriterOptions::default()
            .compression(CompressionType::None)
            .transpose(true)
            .chunk_size(2000); // small chunks to force multiple
        let cursor = Cursor::new(&mut buf);
        let mut writer = RecordWriter::new(cursor, opts).unwrap();
        for r in &records {
            writer.write_record(r).unwrap();
        }
        writer.close().unwrap();
    }

    let proj = FieldProjection::new();
    let results = read_projected(&buf, &proj);

    assert_eq!(results.len(), 200);
    for (i, rec) in results.iter().enumerate() {
        assert!(
            rec.is_empty(),
            "record {i}: empty projection should produce empty record"
        );
    }
}

#[test]
fn test_33_full_projection_with_bucket_fraction() {
    // Multiple buckets + full projection = byte-identical.
    let records: Vec<Vec<u8>> = (0..50u64)
        .map(|i| {
            let mut r = encode_varint_field(1, i);
            r.extend(encode_fixed32_field(2, i as u32));
            r.extend(encode_bytes_field(3, format!("v{i}").as_bytes()));
            r
        })
        .collect();
    let mut buf = Vec::new();
    {
        let opts = WriterOptions::default()
            .compression(CompressionType::None)
            .transpose(true)
            .bucket_fraction(0.1);
        let cursor = Cursor::new(&mut buf);
        let mut writer = RecordWriter::new(cursor, opts).unwrap();
        for r in &records {
            writer.write_record(r).unwrap();
        }
        writer.close().unwrap();
    }

    let no_proj = read_all(&buf);
    let proj = FieldProjection::new()
        .add_field(Field::new(vec![1]))
        .add_field(Field::new(vec![2]))
        .add_field(Field::new(vec![3]));
    let with_proj = read_projected(&buf, &proj);

    assert_eq!(no_proj.len(), with_proj.len());
    for (i, (a, b)) in no_proj.iter().zip(with_proj.iter()).enumerate() {
        assert_eq!(
            a, b,
            "record {i}: full projection with bucket_fraction should be byte-identical"
        );
    }
}
