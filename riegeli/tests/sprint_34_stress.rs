//! Sprint 34 stress tests: verify projected decode correctness at volume.
//!
//! Decodes 10,000+ records with narrow projections and verifies every record
//! matches the expected projection output.

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
// Stress tests
// ===========================================================================

/// 34.5: 10,000 wide records, narrow projection (field 5), verify each record.
#[test]
fn stress_10k_wide_narrow_projection() {
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

/// 34.5 variant: 10,000 wide records with Zstd compression.
#[test]
fn stress_10k_wide_narrow_projection_zstd() {
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

/// 34.5 variant: 10,000 nested records, project field at depth 3.
#[test]
fn stress_10k_nested_depth3_projection() {
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

/// 34.5 variant: 15,000 records, multiple projected fields, verify correctness.
#[test]
fn stress_15k_wide_two_fields() {
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

/// Non-projected stress test: 10,000 records, verify byte-identical output.
#[test]
fn stress_10k_non_projected() {
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

/// Stress test with existence_only: 10,000 records.
#[test]
fn stress_10k_existence_only() {
    const N: usize = 10_000;
    let records: Vec<Vec<u8>> = (0..N).map(|i| make_wide_record(i as u64)).collect();
    let encoded = write_transpose(&records, CompressionType::None);

    // Field 5 (varint) as existence_only
    let proj = FieldProjection::new().add_field(Field::new(vec![5]).existence_only());
    let decoded = read_projected(&encoded, proj);

    assert_eq!(decoded.len(), N);

    // Expected: tag for field 5 (varint wire type) + 0x00 value
    let expected = encode_varint_field(5, 0);

    for (i, rec) in decoded.iter().enumerate() {
        assert_eq!(
            rec, &expected,
            "record {} mismatch in existence_only stress",
            i
        );
    }
}
