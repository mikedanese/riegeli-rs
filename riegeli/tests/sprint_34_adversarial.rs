//! Adversarial tests for Sprint 34: Benchmarks and Validation.
//!
//! These tests probe edge cases in the projection-during-decode path
//! at volume, verifying correctness under conditions the stress tests
//! might not cover.

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

// ===========================================================================
// Adversarial tests
// ===========================================================================

/// Heterogeneous records: alternating between records with different field
/// counts. Tests that projection-during-decode handles schema variation
/// correctly across 10,000 records.
#[test]
fn adv34_heterogeneous_schema_10k() {
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
fn adv34_project_absent_field_10k() {
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

/// All-fields projection must be byte-identical to no projection at volume.
#[test]
fn adv34_all_projection_vs_none_10k() {
    const N: usize = 10_000;
    let mut records = Vec::new();
    for i in 0..N {
        let mut rec = Vec::new();
        for f in 1..=20 {
            match f % 4 {
                0 => rec.extend(encode_bytes_field(f, format!("v{}", i).as_bytes())),
                1 => rec.extend(encode_varint_field(f, i as u64 * f as u64)),
                2 => rec.extend(encode_fixed32_field(f, i as u32 + f)),
                3 => rec.extend(encode_fixed64_field(f, i as u64 + f as u64 * 100)),
                _ => unreachable!(),
            }
        }
        records.push(rec);
    }

    let encoded = write_transpose(&records, CompressionType::None);

    let all_decoded = {
        let proj = FieldProjection::all();
        read_projected(&encoded, proj)
    };
    let none_decoded = read_all(&encoded);

    assert_eq!(all_decoded.len(), N);
    assert_eq!(none_decoded.len(), N);
    for i in 0..N {
        assert_eq!(
            all_decoded[i], none_decoded[i],
            "record {} differs between all() and no projection",
            i
        );
    }
}

/// Large varint values (5-byte and 10-byte varints) under projection.
/// Tests that skip_field_data handles multi-byte varints correctly.
#[test]
fn adv34_large_varints_under_projection() {
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
fn adv34_deep_nesting_depth4_10k() {
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
fn adv34_empty_records_interspersed_10k() {
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

/// Existence-only on a fixed64 field: should produce tag + 8 zero bytes.
#[test]
fn adv34_existence_only_fixed64_10k() {
    const N: usize = 10_000;
    let mut records = Vec::new();
    for i in 0..N {
        let mut rec = Vec::new();
        rec.extend(encode_varint_field(1, i as u64));
        rec.extend(encode_fixed64_field(3, i as u64 * 1000));
        records.push(rec);
    }

    let encoded = write_transpose(&records, CompressionType::None);
    let proj = FieldProjection::new().add_field(Field::new(vec![3]).existence_only());
    let decoded = read_projected(&encoded, proj);

    assert_eq!(decoded.len(), N);
    // Expected: tag for field 3, wire type 1 (fixed64), + 8 zero bytes
    let expected = encode_fixed64_field(3, 0);
    for (i, rec) in decoded.iter().enumerate() {
        assert_eq!(
            rec, &expected,
            "record {} existence_only fixed64 mismatch",
            i
        );
    }
}

/// Existence-only on a length-delimited field: tag + zero-length.
#[test]
fn adv34_existence_only_bytes_10k() {
    const N: usize = 10_000;
    let mut records = Vec::new();
    for i in 0..N {
        let mut rec = Vec::new();
        rec.extend(encode_varint_field(1, i as u64));
        rec.extend(encode_bytes_field(5, format!("data_{}", i).as_bytes()));
        records.push(rec);
    }

    let encoded = write_transpose(&records, CompressionType::None);
    let proj = FieldProjection::new().add_field(Field::new(vec![5]).existence_only());
    let decoded = read_projected(&encoded, proj);

    assert_eq!(decoded.len(), N);
    // Expected: tag for field 5, wire type 2 (length-delimited), + 0x00 (zero length)
    let expected = encode_bytes_field(5, &[]);
    for (i, rec) in decoded.iter().enumerate() {
        assert_eq!(rec, &expected, "record {} existence_only bytes mismatch", i);
    }
}

/// Mixed inclusion types: one field fully included, one existence-only,
/// others excluded. Verify output matches expectations at volume.
#[test]
fn adv34_mixed_inclusion_types_10k() {
    const N: usize = 10_000;
    let mut records = Vec::new();
    for i in 0..N {
        let mut rec = Vec::new();
        rec.extend(encode_varint_field(1, i as u64)); // fully included
        rec.extend(encode_fixed32_field(2, i as u32)); // excluded
        rec.extend(encode_varint_field(3, i as u64 * 3)); // existence only
        rec.extend(encode_varint_field(4, i as u64 * 4)); // excluded
        records.push(rec);
    }

    let encoded = write_transpose(&records, CompressionType::None);
    let proj = FieldProjection::new()
        .add_field(Field::new(vec![1])) // fully included
        .add_field(Field::new(vec![3]).existence_only()); // existence only

    let decoded = read_projected(&encoded, proj);

    assert_eq!(decoded.len(), N);
    for (i, rec) in decoded.iter().enumerate() {
        let mut expected = Vec::new();
        expected.extend(encode_varint_field(1, i as u64));
        expected.extend(encode_varint_field(3, 0)); // existence only -> zero
        assert_eq!(rec, &expected, "record {} mixed inclusion mismatch", i);
    }
}

/// Brotli compression + narrow projection at volume. Tests lazy bucket
/// decompression under realistic compression.
#[test]
#[cfg(feature = "brotli")]
fn adv34_brotli_narrow_projection_10k() {
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
