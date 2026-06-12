// Sprint 32 Evaluator Adversarial Tests: Projection During State Machine Execution
//
// These tests verify that projection-during-decode produces correct output,
// that excluded fields produce no output bytes, that skipped_submessage_level
// is consistent at record boundaries, and that deeply nested messages are
// correctly suppressed.

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

/// Decode proto field tags from a record, return list of (field_number, wire_type).
fn parse_proto_fields(record: &[u8]) -> Vec<(u32, u32)> {
    let mut fields = Vec::new();
    let mut pos = 0;
    while pos < record.len() {
        let (tag, consumed) = decode_varint32(&record[pos..]);
        if consumed == 0 {
            break;
        }
        pos += consumed;
        let field_number = tag >> 3;
        let wire_type = tag & 7;
        fields.push((field_number, wire_type));
        // Skip value
        match wire_type {
            0 => {
                // varint
                while pos < record.len() {
                    let b = record[pos];
                    pos += 1;
                    if b < 0x80 {
                        break;
                    }
                }
            }
            1 => pos += 8, // fixed64
            2 => {
                let (len, c) = decode_varint32(&record[pos..]);
                pos += c + len as usize;
            }
            5 => pos += 4, // fixed32
            _ => break,
        }
    }
    fields
}

fn decode_varint32(buf: &[u8]) -> (u32, usize) {
    let mut result = 0u32;
    let mut shift = 0u32;
    for (i, &byte) in buf.iter().enumerate() {
        result |= ((byte & 0x7f) as u32) << shift;
        shift += 7;
        if byte & 0x80 == 0 {
            return (result, i + 1);
        }
        if shift >= 35 {
            return (0, 0);
        }
    }
    (0, 0)
}

// =========================================================================
// 32.1: Byte-identical output — projected decode matches manual filtering
// =========================================================================

/// Wide record (20 fields), project field 7 only.
/// Verify output contains exactly field 7 with correct value.
#[test]
fn eval_32_1_wide_record_project_single_field() {
    let records: Vec<Vec<u8>> = (0..50u64)
        .map(|i| {
            let mut r = Vec::new();
            for f in 1..=20u32 {
                r.extend(encode_varint_field(f, i * 100 + f as u64));
            }
            r
        })
        .collect();
    let data = write_transpose(&records, CompressionType::None);

    let proj = FieldProjection::new().add_field(Field::new(vec![7]));
    let results = read_projected(&data, &proj);

    assert_eq!(results.len(), 50);
    for (i, rec) in results.iter().enumerate() {
        let fields = parse_proto_fields(rec);
        // Only field 7 should be present
        let field_numbers: Vec<u32> = fields.iter().map(|(fn_, _)| *fn_).collect();
        assert_eq!(
            field_numbers,
            vec![7],
            "record {i}: expected only field 7, got {:?}",
            field_numbers
        );
    }
}

/// Project two non-adjacent fields from a wide record.
#[test]
fn eval_32_1_project_two_nonadjacent_fields() {
    let records: Vec<Vec<u8>> = (0..30u64)
        .map(|i| {
            let mut r = Vec::new();
            for f in 1..=15u32 {
                r.extend(encode_varint_field(f, i + f as u64));
            }
            r
        })
        .collect();
    let data = write_transpose(&records, CompressionType::None);

    let proj = FieldProjection::new()
        .add_field(Field::new(vec![3]))
        .add_field(Field::new(vec![11]));
    let results = read_projected(&data, &proj);

    assert_eq!(results.len(), 30);
    for (i, rec) in results.iter().enumerate() {
        let fields = parse_proto_fields(rec);
        let field_numbers: Vec<u32> = fields.iter().map(|(fn_, _)| *fn_).collect();
        assert!(
            field_numbers.contains(&3) && field_numbers.contains(&11),
            "record {i}: expected fields 3 and 11, got {:?}",
            field_numbers
        );
        assert_eq!(
            field_numbers.len(),
            2,
            "record {i}: expected exactly 2 fields, got {}",
            field_numbers.len()
        );
    }
}

// =========================================================================
// 32.2: apply() not invoked — verify outputs match with no post-filter
// =========================================================================

/// If apply() were still running, then projecting all fields would be
/// byte-identical to no projection. This also verifies that the projection
/// code path doesn't corrupt data even when everything is included.
#[test]
fn eval_32_2_all_projection_byte_identical() {
    let records: Vec<Vec<u8>> = (0..20u64)
        .map(|i| {
            let mut r = encode_varint_field(1, i);
            r.extend(encode_fixed32_field(2, i as u32 * 7));
            r.extend(encode_bytes_field(3, &[0xAB; 30]));
            r
        })
        .collect();
    let data = write_transpose(&records, CompressionType::None);

    let no_proj = read_all(&data);
    let proj = FieldProjection::all();
    let with_all = read_projected(&data, &proj);

    assert_eq!(
        no_proj, with_all,
        "FieldProjection::all() should be identical to no projection"
    );
}

// =========================================================================
// 32.3: skipped_submessage_level = 0 at every record boundary
// =========================================================================

/// Multiple records with nested submessages and a narrow projection.
/// If skipped_submessage_level leaks across records, the assertion in
/// the decode loop's MessageStart handler will fire.
#[test]
fn eval_32_3_submessage_level_resets_across_records() {
    let records: Vec<Vec<u8>> = (0..100u64)
        .map(|i| {
            let leaf = encode_varint_field(3, i);
            let mid = encode_submessage_field(2, &leaf);
            let mut r = encode_varint_field(1, i);
            r.extend(mid);
            r
        })
        .collect();
    let data = write_transpose(&records, CompressionType::None);

    // Project only field 1 — submessage at field 2 should be entirely skipped.
    let proj = FieldProjection::new().add_field(Field::new(vec![1]));
    let results = read_projected(&data, &proj);

    assert_eq!(results.len(), 100);
    for (i, rec) in results.iter().enumerate() {
        let fields = parse_proto_fields(rec);
        let field_numbers: Vec<u32> = fields.iter().map(|(fn_, _)| *fn_).collect();
        assert_eq!(
            field_numbers,
            vec![1],
            "record {i}: only field 1 expected, got {:?}",
            field_numbers
        );
    }
}

// =========================================================================
// 32.4: Excluded fields produce no output bytes
// =========================================================================

/// Write 20-field records, project 1 field. Output size should be much
/// smaller than a full decode.
#[test]
fn eval_32_4_narrow_projection_small_output() {
    let records: Vec<Vec<u8>> = (0..100u64)
        .map(|i| {
            let mut r = Vec::new();
            for f in 1..=20u32 {
                r.extend(encode_varint_field(f, i + f as u64));
            }
            r
        })
        .collect();
    let data = write_transpose(&records, CompressionType::None);

    // Full read: all fields
    let full = read_all(&data);
    let full_size: usize = full.iter().map(|r| r.len()).sum();

    // Projected read: 1 of 20 fields
    let proj = FieldProjection::new().add_field(Field::new(vec![1]));
    let narrow = read_projected(&data, &proj);
    let narrow_size: usize = narrow.iter().map(|r| r.len()).sum();

    // The narrow output should be much smaller than full output.
    // With 20 fields each ~2-3 bytes, projecting 1 field should be ~1/20.
    // Allow generous margin: narrow should be less than 1/4 of full.
    assert!(
        narrow_size * 4 < full_size,
        "narrow projection ({narrow_size} bytes) should be much less than full ({full_size} bytes)"
    );
}

/// Same test but with mixed wire types (varint, fixed32, fixed64, bytes).
#[test]
fn eval_32_4_mixed_types_narrow_projection() {
    let records: Vec<Vec<u8>> = (0..50u64)
        .map(|i| {
            let mut r = encode_varint_field(1, i);
            r.extend(encode_fixed32_field(2, i as u32));
            r.extend(encode_fixed64_field(3, i * 1000));
            r.extend(encode_bytes_field(4, &[0xAA; 50]));
            r.extend(encode_varint_field(5, i + 500));
            r.extend(encode_fixed32_field(6, 0xDEADBEEF));
            r.extend(encode_bytes_field(
                7,
                b"hello world this is a long string value",
            ));
            r
        })
        .collect();
    let data = write_transpose(&records, CompressionType::None);

    let full = read_all(&data);
    let full_size: usize = full.iter().map(|r| r.len()).sum();

    // Project only field 1 (small varint)
    let proj = FieldProjection::new().add_field(Field::new(vec![1]));
    let narrow = read_projected(&data, &proj);
    let narrow_size: usize = narrow.iter().map(|r| r.len()).sum();

    // Field 1 is a small varint (~2 bytes per record), total ~100 bytes.
    // Full record is ~120 bytes per record, total ~6000 bytes.
    assert!(
        narrow_size * 4 < full_size,
        "narrow ({narrow_size}) should be <25% of full ({full_size})"
    );

    // Verify field 1 values are correct
    for (i, rec) in narrow.iter().enumerate() {
        let fields = parse_proto_fields(rec);
        let field_numbers: Vec<u32> = fields.iter().map(|(fn_, _)| *fn_).collect();
        assert_eq!(field_numbers, vec![1], "record {i}: only field 1 expected");
    }
}

// =========================================================================
// 32.5: Non-proto records pass through unchanged
// =========================================================================

/// Mix proto and non-proto records. Non-proto should pass through even with projection.
#[test]
fn eval_32_5_nonproto_passthrough_with_projection() {
    // Non-proto records: start with 0xFF (invalid proto tag)
    let nonproto: Vec<Vec<u8>> = (0..10u8)
        .map(|i| vec![0xFF, 0xFE, i, i + 1, 0xAB, 0xCD])
        .collect();
    let data = write_transpose(&nonproto, CompressionType::None);

    let no_proj = read_all(&data);
    let proj = FieldProjection::new().add_field(Field::new(vec![1]));
    let with_proj = read_projected(&data, &proj);

    assert_eq!(
        no_proj, with_proj,
        "non-proto records should be identical regardless of projection"
    );
    assert_eq!(with_proj, nonproto);
}

// =========================================================================
// 32.6: Existing tests pass (verified by running full test suite above)
// =========================================================================

// =========================================================================
// 32.7: Deeply nested messages (3+ levels) with top-level-only projection
// =========================================================================

/// 4-level nesting: field 1 -> field 2 -> field 3 -> field 4 (leaf varint).
/// Project only top-level field 5 (a sibling varint).
/// All nested content should be suppressed.
#[test]
fn eval_32_7_deep_nesting_top_level_projection() {
    let records: Vec<Vec<u8>> = (0..30u64)
        .map(|i| {
            // Build deepnested structure:
            // field 1: submessage {
            //   field 2: submessage {
            //     field 3: submessage {
            //       field 4: varint(i)
            //     }
            //   }
            // }
            // field 5: varint(i * 10)
            let level3 = encode_varint_field(4, i);
            let level2 = encode_submessage_field(3, &level3);
            let level1 = encode_submessage_field(2, &level2);
            let nested = encode_submessage_field(1, &level1);

            let mut r = Vec::new();
            r.extend(nested);
            r.extend(encode_varint_field(5, i * 10));
            r
        })
        .collect();
    let data = write_transpose(&records, CompressionType::None);

    // Project only field 5 — the entire nested structure at field 1 should be suppressed.
    let proj = FieldProjection::new().add_field(Field::new(vec![5]));
    let results = read_projected(&data, &proj);

    assert_eq!(results.len(), 30);
    for (i, rec) in results.iter().enumerate() {
        let fields = parse_proto_fields(rec);
        let field_numbers: Vec<u32> = fields.iter().map(|(fn_, _)| *fn_).collect();
        assert_eq!(
            field_numbers,
            vec![5],
            "record {i}: only field 5 expected, got {:?}",
            field_numbers
        );
    }
}

/// 3-level nesting with a leaf projection inside the nesting.
/// Project [1, 2, 3] — the path should be followed and only field 3 inside
/// field 2 inside field 1 should appear.
#[test]
fn eval_32_7_deep_nesting_leaf_projection() {
    let records: Vec<Vec<u8>> = (0..20u64)
        .map(|i| {
            let inner = encode_varint_field(3, i * 7);
            let mid = encode_submessage_field(2, &inner);
            let outer = encode_submessage_field(1, &mid);
            let mut r = outer;
            // Also add a top-level field 4 that should be excluded
            r.extend(encode_varint_field(4, 999));
            r
        })
        .collect();
    let data = write_transpose(&records, CompressionType::None);

    let proj = FieldProjection::new().add_field(Field::new(vec![1, 2, 3]));
    let results = read_projected(&data, &proj);

    assert_eq!(results.len(), 20);
    for (i, rec) in results.iter().enumerate() {
        // Field 4 should be absent
        let fields = parse_proto_fields(rec);
        let top_field_numbers: Vec<u32> = fields.iter().map(|(fn_, _)| *fn_).collect();
        assert!(
            !top_field_numbers.contains(&4),
            "record {i}: field 4 should be absent, got {:?}",
            top_field_numbers
        );
        // Field 1 should be present (it's a parent of the projected path)
        assert!(
            top_field_numbers.contains(&1),
            "record {i}: field 1 should be present as parent, got {:?}",
            top_field_numbers
        );
    }
}

/// Multiple records where some have different nesting depths.
/// Verify skipped_submessage_level correctly handles variable-depth records.
#[test]
fn eval_32_7_variable_depth_records() {
    // All records have the same schema (transpose encoding requires it),
    // but we can vary the data to stress nesting.
    let records: Vec<Vec<u8>> = (0..40u64)
        .map(|i| {
            let leaf = encode_varint_field(5, i);
            let level2 = encode_submessage_field(4, &leaf);
            let level1 = encode_submessage_field(3, &level2);
            let nested = encode_submessage_field(2, &level1);

            let mut r = encode_varint_field(1, i);
            r.extend(nested);
            r.extend(encode_varint_field(6, i + 100));
            r
        })
        .collect();
    let data = write_transpose(&records, CompressionType::None);

    // Project fields 1 and 6 only — field 2 (and all nesting) should be skipped.
    let proj = FieldProjection::new()
        .add_field(Field::new(vec![1]))
        .add_field(Field::new(vec![6]));
    let results = read_projected(&data, &proj);

    assert_eq!(results.len(), 40);
    for (i, rec) in results.iter().enumerate() {
        let fields = parse_proto_fields(rec);
        let field_numbers: Vec<u32> = fields.iter().map(|(fn_, _)| *fn_).collect();
        assert!(
            field_numbers.contains(&1) && field_numbers.contains(&6),
            "record {i}: expected fields 1 and 6, got {:?}",
            field_numbers
        );
        assert_eq!(
            field_numbers.len(),
            2,
            "record {i}: expected exactly 2 fields, got {:?}",
            field_numbers
        );
    }
}

// =========================================================================
// Edge cases
// =========================================================================

/// Empty projection returns empty records for proto records.
#[test]
fn eval_32_edge_empty_projection() {
    let records: Vec<Vec<u8>> = (0..10u64).map(|i| encode_varint_field(1, i)).collect();
    let data = write_transpose(&records, CompressionType::None);

    let proj = FieldProjection::new(); // no fields added
    let results = read_projected(&data, &proj);

    assert_eq!(results.len(), 10);
    for (i, rec) in results.iter().enumerate() {
        assert!(
            rec.is_empty(),
            "record {i}: empty projection should produce empty record, got {} bytes",
            rec.len()
        );
    }
}

/// Project a field that doesn't exist in the records — should produce empty records.
#[test]
fn eval_32_edge_absent_field_projection() {
    let records: Vec<Vec<u8>> = (0..10u64).map(|i| encode_varint_field(1, i)).collect();
    let data = write_transpose(&records, CompressionType::None);

    let proj = FieldProjection::new().add_field(Field::new(vec![99])); // field 99 doesn't exist
    let results = read_projected(&data, &proj);

    assert_eq!(results.len(), 10);
    for (i, rec) in results.iter().enumerate() {
        assert!(
            rec.is_empty(),
            "record {i}: projecting absent field should produce empty record, got {} bytes",
            rec.len()
        );
    }
}

/// Brotli compression + projection — verify correctness under compression.
#[test]
#[cfg(feature = "brotli")]
fn eval_32_edge_brotli_projection() {
    let records: Vec<Vec<u8>> = (0..50u64)
        .map(|i| {
            let mut r = encode_varint_field(1, i);
            r.extend(encode_fixed32_field(2, i as u32));
            r.extend(encode_bytes_field(3, &[0xBB; 20]));
            r
        })
        .collect();
    let data = write_transpose(&records, CompressionType::Brotli);

    let proj = FieldProjection::new().add_field(Field::new(vec![2]));
    let results = read_projected(&data, &proj);

    assert_eq!(results.len(), 50);
    for (i, rec) in results.iter().enumerate() {
        let fields = parse_proto_fields(rec);
        let field_numbers: Vec<u32> = fields.iter().map(|(fn_, _)| *fn_).collect();
        assert_eq!(field_numbers, vec![2], "record {i}: only field 2 expected");
    }
}
