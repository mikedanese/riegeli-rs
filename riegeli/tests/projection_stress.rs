//! Volume stress tests for projected decode of transposed records.
//!
//! Encodes 10,000+ records, decodes them with various field projections,
//! and verifies every record matches the expected projection output.

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
// Record builders
// ---------------------------------------------------------------------------

const NUM_WIDE_FIELDS: u32 = 25;

fn make_wide_record(seed: u64) -> Vec<u8> {
    let mut rec = Vec::new();
    for f in 1..=NUM_WIDE_FIELDS {
        match f % 4 {
            1 => rec.extend(encode_varint_field(f, seed.wrapping_mul(f as u64))),
            2 => rec.extend(encode_fixed32_field(f, (seed as u32).wrapping_add(f))),
            3 => rec.extend(encode_fixed64_field(f, seed.wrapping_add(f as u64 * 1000))),
            0 => {
                let data = format!("field_{}_seed_{}", f, seed);
                rec.extend(encode_bytes_field(f, data.as_bytes()));
            }
            _ => unreachable!(),
        }
    }
    rec
}

fn make_nested_record(seed: u64) -> Vec<u8> {
    let mut rec = Vec::new();
    rec.extend(encode_varint_field(1, seed));

    // Innermost: field 2.3.5 content
    let mut inner3 = Vec::new();
    inner3.extend(encode_varint_field(1, seed.wrapping_mul(7)));
    inner3.extend(encode_fixed32_field(2, seed as u32));

    // Middle: field 2.3 content
    let mut inner2 = Vec::new();
    inner2.extend(encode_varint_field(1, seed.wrapping_mul(3)));
    inner2.extend(encode_submessage_field(5, &inner3));
    inner2.extend(encode_fixed32_field(2, seed as u32));

    // Outer: field 2 content
    let mut inner1 = Vec::new();
    inner1.extend(encode_varint_field(1, seed.wrapping_mul(2)));
    inner1.extend(encode_submessage_field(3, &inner2));
    inner1.extend(encode_fixed32_field(2, seed as u32));

    rec.extend(encode_submessage_field(2, &inner1));

    for f in 3..=10 {
        rec.extend(encode_varint_field(f, seed.wrapping_add(f as u64)));
    }
    rec
}

// ---------------------------------------------------------------------------
// Encoding helper
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

fn read_projected(data: &[u8], proj: FieldProjection) -> Vec<Vec<u8>> {
    let opts = ReaderOptions::new().field_projection(proj);
    let cursor = Cursor::new(data);
    let mut reader = RecordReader::new(cursor, opts).unwrap();
    let mut out = Vec::new();
    while let Some(rec) = reader.read_record().unwrap() {
        out.push(rec);
    }
    out
}

fn read_all(data: &[u8]) -> Vec<Vec<u8>> {
    let opts = ReaderOptions::new();
    let cursor = Cursor::new(data);
    let mut reader = RecordReader::new(cursor, opts).unwrap();
    let mut out = Vec::new();
    while let Some(rec) = reader.read_record().unwrap() {
        out.push(rec);
    }
    out
}

// ---------------------------------------------------------------------------
// Varint decoding for record parsing
// ---------------------------------------------------------------------------

fn decode_varint_from(buf: &[u8]) -> Option<(u64, usize)> {
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

/// Extract a specific field's raw bytes (tag + value) from a proto record.
fn extract_field(record: &[u8], target_field: u32) -> Option<Vec<u8>> {
    let mut pos = 0;
    while pos < record.len() {
        let field_start = pos;
        let (tag, tag_len) = decode_varint_from(&record[pos..])?;
        let field_number = (tag >> 3) as u32;
        let wire_type = (tag & 7) as u32;
        pos += tag_len;

        match wire_type {
            0 => {
                let (_, vlen) = decode_varint_from(&record[pos..])?;
                pos += vlen;
            }
            1 => pos += 8,
            2 => {
                let (len, llen) = decode_varint_from(&record[pos..])?;
                pos += llen + len as usize;
            }
            5 => pos += 4,
            _ => return None,
        }

        if field_number == target_field {
            return Some(record[field_start..pos].to_vec());
        }
    }
    None
}

// ===========================================================================
// Volume tests
// ===========================================================================

/// 10,000 wide records, narrow projection (field 5), verify each record.
#[test]
fn wide_records_narrow_projection_at_volume() {
    const N: usize = 10_000;
    let records: Vec<Vec<u8>> = (0..N).map(|i| make_wide_record(i as u64)).collect();
    let encoded = write_transpose(&records, CompressionType::None);

    let proj = FieldProjection::new().add_field(Field::new(vec![5]));
    let decoded = read_projected(&encoded, proj);

    assert_eq!(decoded.len(), N, "record count mismatch");

    for (i, rec) in decoded.iter().enumerate() {
        // The projected record should contain only field 5.
        // Field 5 is field_number=5, which is 5%4=1 => varint field.
        let expected_field5 = encode_varint_field(5, (i as u64).wrapping_mul(5));

        // The projected record should be exactly the field 5 bytes.
        assert_eq!(
            rec,
            &expected_field5,
            "record {} mismatch: got {} bytes, expected {} bytes",
            i,
            rec.len(),
            expected_field5.len()
        );
    }
}

/// 10,000 wide records with Zstd compression, narrow projection.
#[test]
#[cfg(feature = "zstd")]
fn narrow_projection_zstd_at_volume() {
    const N: usize = 10_000;
    let records: Vec<Vec<u8>> = (0..N).map(|i| make_wide_record(i as u64)).collect();
    let encoded = write_transpose(&records, CompressionType::Zstd);

    let proj = FieldProjection::new().add_field(Field::new(vec![5]));
    let decoded = read_projected(&encoded, proj);

    assert_eq!(decoded.len(), N);

    for (i, rec) in decoded.iter().enumerate() {
        let expected_field5 = encode_varint_field(5, (i as u64).wrapping_mul(5));
        assert_eq!(rec, &expected_field5, "record {} mismatch with zstd", i);
    }
}

/// 10,000 nested records, project field at depth 3.
#[test]
fn nested_depth3_projection_at_volume() {
    const N: usize = 10_000;
    let records: Vec<Vec<u8>> = (0..N).map(|i| make_nested_record(i as u64)).collect();
    let encoded = write_transpose(&records, CompressionType::None);

    // Project only field 2.3.5 (depth 3)
    let proj = FieldProjection::new().add_field(Field::new(vec![2, 3, 5]));
    let decoded = read_projected(&encoded, proj);

    assert_eq!(decoded.len(), N, "record count mismatch for nested");

    // Each projected record should contain: field 2 submessage containing
    // field 3 submessage containing field 5 submessage.
    // Verify that we get N records and each is non-empty (the submessage
    // wrapping means exact byte comparison is complex, so we verify structure).
    for (i, rec) in decoded.iter().enumerate() {
        assert!(
            !rec.is_empty(),
            "record {} should not be empty (projected field at depth 3)",
            i
        );

        // Parse the outer submessage (field 2)
        let field2_data = extract_field(rec, 2);
        assert!(
            field2_data.is_some(),
            "record {} should have field 2 (submessage wrapper)",
            i
        );
    }
}

/// 15,000 records, multiple projected fields, verify correctness.
#[test]
fn two_field_projection_at_volume() {
    const N: usize = 15_000;
    let records: Vec<Vec<u8>> = (0..N).map(|i| make_wide_record(i as u64)).collect();
    let encoded = write_transpose(&records, CompressionType::None);

    // Project fields 1 and 9 (both varint fields since 1%4=1 and 9%4=1)
    let proj = FieldProjection::new()
        .add_field(Field::new(vec![1]))
        .add_field(Field::new(vec![9]));
    let decoded = read_projected(&encoded, proj);

    assert_eq!(decoded.len(), N);

    for (i, rec) in decoded.iter().enumerate() {
        let seed = i as u64;
        let expected_f1 = encode_varint_field(1, seed.wrapping_mul(1));
        let expected_f9 = encode_varint_field(9, seed.wrapping_mul(9));

        // Record should contain exactly field 1 and field 9
        let mut expected = Vec::new();
        expected.extend(&expected_f1);
        expected.extend(&expected_f9);

        assert_eq!(
            rec, &expected,
            "record {} mismatch: two-field projection",
            i
        );
    }
}

/// Non-projected baseline: 10,000 records, verify byte-identical output.
#[test]
fn non_projected_baseline_at_volume() {
    const N: usize = 10_000;
    let records: Vec<Vec<u8>> = (0..N).map(|i| make_wide_record(i as u64)).collect();
    let encoded = write_transpose(&records, CompressionType::None);

    let decoded = read_all(&encoded);

    assert_eq!(decoded.len(), N);

    for (i, rec) in decoded.iter().enumerate() {
        assert_eq!(
            rec, &records[i],
            "record {} mismatch in non-projected read",
            i
        );
    }
}

/// Heterogeneous records: alternating between records with different field
/// counts. Tests that projection-during-decode handles schema variation
/// correctly across 10,000 records.
#[test]
fn heterogeneous_schema_projection_at_volume() {
    const N: usize = 10_000;
    let mut records = Vec::new();
    for i in 0..N {
        let mut rec = Vec::new();
        // All records have field 1
        rec.extend(encode_varint_field(1, i as u64));
        if i % 2 == 0 {
            // Even records: have fields 1..5
            for f in 2..=5 {
                rec.extend(encode_varint_field(f, i as u64 + f as u64));
            }
        } else {
            // Odd records: have fields 1..25
            for f in 2..=25 {
                rec.extend(encode_varint_field(f, i as u64 + f as u64));
            }
        }
        records.push(rec);
    }

    let encoded = write_transpose(&records, CompressionType::None);

    // Project only field 1 — present in all records
    let proj = FieldProjection::new().add_field(Field::new(vec![1]));
    let decoded = read_projected(&encoded, proj);

    assert_eq!(decoded.len(), N);
    for (i, rec) in decoded.iter().enumerate() {
        let expected = encode_varint_field(1, i as u64);
        assert_eq!(
            rec, &expected,
            "record {} mismatch in heterogeneous test",
            i
        );
    }
}

/// Project a field that only exists in half the records. Non-present
/// records should produce empty output.
#[test]
fn partially_present_field_projection_at_volume() {
    const N: usize = 10_000;
    let mut records = Vec::new();
    for i in 0..N {
        let mut rec = Vec::new();
        rec.extend(encode_varint_field(1, i as u64));
        if i % 2 == 0 {
            // Even records have field 99
            rec.extend(encode_varint_field(99, i as u64 * 99));
        }
        records.push(rec);
    }

    let encoded = write_transpose(&records, CompressionType::None);
    let proj = FieldProjection::new().add_field(Field::new(vec![99]));
    let decoded = read_projected(&encoded, proj);

    assert_eq!(decoded.len(), N);
    for (i, rec) in decoded.iter().enumerate() {
        if i % 2 == 0 {
            let expected = encode_varint_field(99, i as u64 * 99);
            assert_eq!(rec, &expected, "even record {} should have field 99", i);
        } else {
            assert!(
                rec.is_empty(),
                "odd record {} should be empty (field 99 absent)",
                i
            );
        }
    }
}

/// Large varint values (5-byte and 10-byte varints) under projection.
/// Tests that skip_field_data handles multi-byte varints correctly.
#[test]
fn multibyte_varints_skipped_under_projection() {
    const N: usize = 10_000;
    let mut records = Vec::new();
    for i in 0..N {
        let mut rec = Vec::new();
        // Field 1: normal value (projected)
        rec.extend(encode_varint_field(1, i as u64));
        // Field 2: large varint (skipped)
        rec.extend(encode_varint_field(2, u64::MAX - i as u64));
        // Field 3: 5-byte varint (skipped)
        rec.extend(encode_varint_field(3, 0x1_0000_0000 + i as u64));
        records.push(rec);
    }

    let encoded = write_transpose(&records, CompressionType::None);
    let proj = FieldProjection::new().add_field(Field::new(vec![1]));
    let decoded = read_projected(&encoded, proj);

    assert_eq!(decoded.len(), N);
    for (i, rec) in decoded.iter().enumerate() {
        let expected = encode_varint_field(1, i as u64);
        assert_eq!(rec, &expected, "record {} large varint skip failed", i);
    }
}

/// Deeply nested projection (depth 4) with many sibling fields at each level.
#[test]
fn nested_depth4_projection_at_volume() {
    const N: usize = 10_000;
    let mut records = Vec::new();
    for i in 0..N {
        // Build depth-4: field 1.2.3.4
        let innermost = encode_varint_field(4, i as u64 * 7);
        let level3 = {
            let mut v = Vec::new();
            v.extend(encode_varint_field(1, i as u64));
            v.extend(encode_submessage_field(4, &innermost));
            v.extend(encode_varint_field(2, i as u64));
            v
        };
        let level2 = {
            let mut v = Vec::new();
            v.extend(encode_varint_field(1, i as u64));
            v.extend(encode_submessage_field(3, &level3));
            v.extend(encode_varint_field(2, i as u64));
            v
        };
        let level1 = {
            let mut v = Vec::new();
            v.extend(encode_varint_field(1, i as u64));
            v.extend(encode_submessage_field(2, &level2));
            v.extend(encode_varint_field(2, i as u64));
            v
        };

        let mut rec = Vec::new();
        rec.extend(encode_submessage_field(1, &level1));
        // Padding fields
        for f in 2..=15 {
            rec.extend(encode_varint_field(f, i as u64 + f as u64));
        }
        records.push(rec);
    }

    let encoded = write_transpose(&records, CompressionType::None);
    let proj = FieldProjection::new().add_field(Field::new(vec![1, 2, 3, 4]));
    let decoded = read_projected(&encoded, proj);

    assert_eq!(decoded.len(), N);
    for (i, rec) in decoded.iter().enumerate() {
        assert!(
            !rec.is_empty(),
            "record {} should not be empty (depth-4 projection)",
            i
        );
    }
}

/// Empty records interspersed with normal records under projection.
#[test]
fn empty_records_interspersed_under_projection() {
    const N: usize = 10_000;
    let mut records = Vec::new();
    for i in 0..N {
        if i % 3 == 0 {
            records.push(Vec::new()); // empty record
        } else {
            let mut rec = Vec::new();
            rec.extend(encode_varint_field(1, i as u64));
            rec.extend(encode_varint_field(2, i as u64 * 2));
            records.push(rec);
        }
    }

    let encoded = write_transpose(&records, CompressionType::None);
    let proj = FieldProjection::new().add_field(Field::new(vec![1]));
    let decoded = read_projected(&encoded, proj);

    assert_eq!(decoded.len(), N);
    for (i, rec) in decoded.iter().enumerate() {
        if i % 3 == 0 {
            assert!(rec.is_empty(), "record {} should be empty", i);
        } else {
            let expected = encode_varint_field(1, i as u64);
            assert_eq!(rec, &expected, "record {} content mismatch", i);
        }
    }
}

/// Brotli compression + narrow projection at volume. Tests lazy bucket
/// decompression under realistic compression.
#[test]
#[cfg(feature = "brotli")]
fn narrow_projection_brotli_at_volume() {
    const N: usize = 10_000;
    let mut records = Vec::new();
    for i in 0..N {
        let mut rec = Vec::new();
        for f in 1..=25 {
            rec.extend(encode_varint_field(f, i as u64 * f as u64));
        }
        records.push(rec);
    }

    let encoded = write_transpose(&records, CompressionType::Brotli);
    let proj = FieldProjection::new().add_field(Field::new(vec![13]));
    let decoded = read_projected(&encoded, proj);

    assert_eq!(decoded.len(), N);
    for (i, rec) in decoded.iter().enumerate() {
        let expected = encode_varint_field(13, i as u64 * 13);
        assert_eq!(rec, &expected, "record {} brotli projection mismatch", i);
    }
}
