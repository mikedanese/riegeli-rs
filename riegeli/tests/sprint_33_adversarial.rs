// Sprint 33 Adversarial Tests
//
// Tests targeting edge cases the Generator's tests may not cover:
// - existence-only on nested submessage fields
// - existence-only with inline varints (small values in tag_data)
// - single-field existence-only records
// - existence-only alongside excluded fields (not just included)
// - existence-only with repeated field occurrences
// - full projection byte-identity with compression
// - empty projection with single-field records

use std::io::Cursor;

use riegeli::{
    CompressionType, Field, FieldProjection, ReaderOptions, RecordReader, RecordWriter,
    WriterOptions,
};

// Proto encoding helpers (duplicated for standalone test binary)
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
                let start = pos;
                while pos < record.len() && record[pos] >= 0x80 {
                    pos += 1;
                }
                pos += 1;
                record[start..pos].to_vec()
            }
            1 => {
                let b = record[pos..pos + 8].to_vec();
                pos += 8;
                b
            }
            5 => {
                let b = record[pos..pos + 4].to_vec();
                pos += 4;
                b
            }
            2 => {
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

// ---------------------------------------------------------------------------
// Adversarial: existence-only with inline varints (value 0..127 stored in tag)
// The transpose encoder may encode small varints inline in tag_data.
// Existence-only must still produce 0x00 regardless.
// ---------------------------------------------------------------------------

#[test]
fn adversarial_existence_only_inline_varint() {
    // Values 0-127 fit in a single byte varint, which the transpose encoder
    // may store inline. Existence-only must still output tag + 0x00.
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

// ---------------------------------------------------------------------------
// Adversarial: single-field record with existence-only
// ---------------------------------------------------------------------------

#[test]
fn adversarial_single_field_existence_only() {
    // Records with only one field, projected as existence-only.
    let records: Vec<Vec<u8>> = (0..20u64)
        .map(|i| encode_varint_field(1, i * 999))
        .collect();
    let data = write_transpose(&records, CompressionType::None);

    let proj = FieldProjection::new().add_field(Field::new(vec![1]).existence_only());
    let results = read_projected(&data, &proj);

    assert_eq!(results.len(), 20);
    for (i, rec) in results.iter().enumerate() {
        let fields = parse_fields(rec);
        assert_eq!(fields.len(), 1, "record {i}");
        assert_eq!(fields[0].2, vec![0x00], "record {i}");
    }
}

// ---------------------------------------------------------------------------
// Adversarial: existence-only alongside excluded fields (verify excluded are gone)
// ---------------------------------------------------------------------------

#[test]
fn adversarial_existence_only_with_excluded_fields() {
    let records: Vec<Vec<u8>> = (0..10u64)
        .map(|i| {
            let mut r = encode_varint_field(1, i);
            r.extend(encode_fixed32_field(2, 0xDEAD));
            r.extend(encode_fixed64_field(3, 0xBEEF));
            r.extend(encode_bytes_field(4, b"excluded"));
            r.extend(encode_varint_field(5, i + 100));
            r
        })
        .collect();
    let data = write_transpose(&records, CompressionType::None);

    // Include field 1 as existence-only, field 5 fully. Fields 2,3,4 excluded.
    let proj = FieldProjection::new()
        .add_field(Field::new(vec![1]).existence_only())
        .add_field(Field::new(vec![5]));
    let results = read_projected(&data, &proj);

    assert_eq!(results.len(), 10);
    for (i, rec) in results.iter().enumerate() {
        let fields = parse_fields(rec);
        assert_eq!(
            fields.len(),
            2,
            "record {i}: only existence-only field 1 and included field 5"
        );
        // Field 1: existence-only
        assert_eq!(fields[0].0, 1);
        assert_eq!(fields[0].2, vec![0x00]);
        // Field 5: fully included
        assert_eq!(fields[1].0, 5);
        let (val, _) = decode_varint(&fields[1].2);
        assert_eq!(val, (i + 100) as u64, "record {i}: field 5 value");
    }
}

// ---------------------------------------------------------------------------
// Adversarial: full projection with Brotli compression produces byte-identical
// ---------------------------------------------------------------------------

#[test]
#[cfg(feature = "brotli")]
fn adversarial_full_projection_brotli_byte_identical() {
    let records: Vec<Vec<u8>> = (0..30u64)
        .map(|i| {
            let mut r = encode_varint_field(1, i);
            r.extend(encode_fixed32_field(2, i as u32));
            r.extend(encode_bytes_field(3, format!("data-{i}").as_bytes()));
            r
        })
        .collect();
    // Use Brotli compression.
    let data = write_transpose(&records, CompressionType::Brotli);

    let no_proj = read_all(&data);
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
// Adversarial: empty projection with single-field records
// ---------------------------------------------------------------------------

#[test]
fn adversarial_empty_projection_single_field_records() {
    let records: Vec<Vec<u8>> = (0..50u64).map(|i| encode_varint_field(1, i)).collect();
    let data = write_transpose(&records, CompressionType::None);

    let proj = FieldProjection::new();
    let results = read_projected(&data, &proj);

    assert_eq!(results.len(), 50);
    for (i, rec) in results.iter().enumerate() {
        assert!(
            rec.is_empty(),
            "record {i}: empty projection should yield empty record, got {} bytes",
            rec.len()
        );
    }
}

// ---------------------------------------------------------------------------
// Adversarial: existence-only on nested submessage field path
// ---------------------------------------------------------------------------

#[test]
fn adversarial_existence_only_nested_submessage() {
    // Record: field 1 (varint), field 2 (submessage containing field 1 varint).
    // Project field [2, 1] as existence-only.
    let records: Vec<Vec<u8>> = (0..10u64)
        .map(|i| {
            let inner = encode_varint_field(1, i * 999);
            let mut r = encode_varint_field(1, i);
            r.extend(encode_submessage_field(2, &inner));
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
        let (inner_len, lc) = decode_varint(val_bytes);
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
// Adversarial: existence-only on a field with zero value already
// (ensure no confusion between real 0 and existence-only 0)
// ---------------------------------------------------------------------------

#[test]
fn adversarial_existence_only_field_already_zero() {
    // Write records where field 1 already has value 0.
    // Existence-only should still produce tag + 0x00 (same output, but from
    // the existence-only path, not the data buffer).
    let records: Vec<Vec<u8>> = (0..10u64)
        .map(|_| {
            let mut r = encode_varint_field(1, 0); // value is already 0
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
        assert_eq!(fields[0].2, vec![0x00], "record {i}: should be 0x00");
    }
}

// ---------------------------------------------------------------------------
// Adversarial: high field number (> 16, requires multi-byte tag varint)
// ---------------------------------------------------------------------------

#[test]
fn adversarial_existence_only_high_field_number() {
    // Field number 100 requires 2-byte tag varint.
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
            "record {i}: high field num existence-only"
        );
    }
}

// ---------------------------------------------------------------------------
// Adversarial: existence-only for fixed32 and fixed64 with non-zero original values
// (verify the data buffer is consumed but not written)
// ---------------------------------------------------------------------------

#[test]
fn adversarial_existence_only_fixed_consumes_buffer() {
    // Records have fixed32 field 1 and fixed64 field 2.
    // After projecting both as existence-only, read remaining field 3 fully
    // to verify buffer positions are correct (buffers consumed for fields 1,2).
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
        let (val, _) = decode_varint(&fields[2].2);
        assert_eq!(val, i as u64, "record {i}: field 3 value");
    }
}

// ---------------------------------------------------------------------------
// Adversarial: Zstd compression with existence-only
// ---------------------------------------------------------------------------

#[test]
#[cfg(feature = "zstd")]
fn adversarial_existence_only_zstd() {
    let records: Vec<Vec<u8>> = (0..20u64)
        .map(|i| {
            let mut r = encode_varint_field(1, i);
            r.extend(encode_bytes_field(2, &[0xAA; 100]));
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
// Adversarial: verify non-projected read is still byte-identical (invariant)
// ---------------------------------------------------------------------------

#[test]
fn adversarial_non_projected_read_unchanged() {
    let records: Vec<Vec<u8>> = (0..100u64)
        .map(|i| {
            let mut r = encode_varint_field(1, i);
            r.extend(encode_fixed32_field(2, i as u32));
            r.extend(encode_bytes_field(3, format!("v{i}").as_bytes()));
            r
        })
        .collect();
    let data = write_transpose(&records, CompressionType::None);

    // Read without any projection.
    let results = read_all(&data);

    assert_eq!(results.len(), 100);
    for (i, rec) in results.iter().enumerate() {
        // Parse and verify the original values.
        let fields = parse_fields(rec);
        assert_eq!(fields.len(), 3, "record {i}");
        let (v1, _) = decode_varint(&fields[0].2);
        assert_eq!(v1, i as u64, "record {i}: field 1");
    }
}
