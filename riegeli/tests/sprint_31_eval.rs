// Sprint 31 Evaluator Adversarial Tests: Projection-Aware Callback Types
//
// These tests verify edge cases and invariants using the public API
// (RecordWriter/RecordReader) with transpose encoding enabled.

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

// =========================================================================
// 31.2: When projection active, nodes use SelectCallback — output unchanged
// =========================================================================

#[test]
fn eval_31_2_projection_single_field_record() {
    let record = encode_varint_field(1, 150);
    let data = write_transpose(&[record.clone()], CompressionType::None);

    let no_proj = read_all(&data);
    let proj = FieldProjection::new().add_field(Field::new(vec![1]));
    let with_proj = read_projected(&data, &proj);

    assert_eq!(no_proj, with_proj);
}

#[test]
fn eval_31_2_projection_multi_record_consistency() {
    let records: Vec<Vec<u8>> = (0..20u64)
        .map(|i| {
            let mut r = encode_varint_field(1, i);
            r.extend(encode_varint_field(2, i + 100));
            r
        })
        .collect();
    let data = write_transpose(&records, CompressionType::None);

    let proj = FieldProjection::new().add_field(Field::new(vec![1]));
    let result = read_projected(&data, &proj);

    assert_eq!(result.len(), 20);
    for (i, r) in result.iter().enumerate() {
        let expected = encode_varint_field(1, i as u64);
        assert_eq!(r, &expected, "record {i}: field 1 projection mismatch");
    }
}

// =========================================================================
// 31.4: No SelectCallback when no projection — identical output
// =========================================================================

#[test]
fn eval_31_4_all_projection_identical() {
    let records = vec![
        encode_varint_field(1, 1),
        {
            let mut r = encode_varint_field(1, 2);
            r.extend(encode_varint_field(2, 3));
            r
        },
        {
            let mut r = encode_varint_field(1, 4);
            r.extend(encode_varint_field(2, 5));
            r.extend(encode_varint_field(3, 6));
            r
        },
    ];
    let data = write_transpose(&records, CompressionType::None);

    let no_proj = read_all(&data);
    let proj_all = FieldProjection::all();
    let with_all = read_projected(&data, &proj_all);

    assert_eq!(no_proj, with_all);
}

// =========================================================================
// 31.5: Post-decode apply() fallback still active
// =========================================================================

#[test]
fn eval_31_5_apply_fallback_filters_field2() {
    let mut record = encode_varint_field(1, 42);
    record.extend(encode_varint_field(2, 99));
    let data = write_transpose(&[record], CompressionType::None);

    let proj = FieldProjection::new().add_field(Field::new(vec![1]));
    let result = read_projected(&data, &proj);

    assert_eq!(result.len(), 1);
    assert_eq!(result[0], encode_varint_field(1, 42));
}

#[test]
fn eval_31_5_empty_projection_empty_records() {
    let mut record = encode_varint_field(1, 42);
    record.extend(encode_varint_field(2, 99));
    let data = write_transpose(&[record], CompressionType::None);

    let proj = FieldProjection::new();
    let result = read_projected(&data, &proj);

    assert_eq!(result.len(), 1);
    assert_eq!(result[0], Vec::<u8>::new());
}

// =========================================================================
// 31.6: Nested submessage resolution
// =========================================================================

#[test]
fn eval_31_6_nested_submessage_projection_field1_only() {
    let inner = encode_varint_field(3, 7);
    let mut record = encode_varint_field(1, 42);
    record.extend(encode_submessage_field(2, &inner));
    let data = write_transpose(&[record.clone()], CompressionType::None);

    let proj = FieldProjection::new().add_field(Field::new(vec![1]));
    let result = read_projected(&data, &proj);
    assert_eq!(result.len(), 1);
    assert_eq!(result[0], encode_varint_field(1, 42));
}

// =========================================================================
// Additional edge cases — projection + transpose correctness
// =========================================================================

#[test]
#[cfg(feature = "brotli")]
fn eval_31_projection_with_brotli() {
    let mut record = encode_varint_field(1, 42);
    record.extend(encode_varint_field(2, 99));
    let data = write_transpose(&[record], CompressionType::Brotli);

    let proj = FieldProjection::new().add_field(Field::new(vec![1]));
    let result = read_projected(&data, &proj);
    assert_eq!(result.len(), 1);
    assert_eq!(result[0], encode_varint_field(1, 42));
}

#[test]
fn eval_31_existence_only_preserves_tag() {
    let record = encode_varint_field(1, 42);
    let data = write_transpose(&[record], CompressionType::None);

    let proj = FieldProjection::new().add_field(Field::new(vec![1]).existence_only());
    let result = read_projected(&data, &proj);
    assert_eq!(result.len(), 1);
    let expected = encode_varint_field(1, 0);
    assert_eq!(result[0], expected);
}

#[test]
fn eval_31_multiple_projections_same_parent() {
    let mut record = encode_varint_field(1, 10);
    record.extend(encode_varint_field(2, 20));
    record.extend(encode_varint_field(3, 30));
    let data = write_transpose(&[record], CompressionType::None);

    let proj = FieldProjection::new()
        .add_field(Field::new(vec![1]))
        .add_field(Field::new(vec![3]));
    let result = read_projected(&data, &proj);
    assert_eq!(result.len(), 1);
    let mut expected = encode_varint_field(1, 10);
    expected.extend(encode_varint_field(3, 30));
    assert_eq!(result[0], expected);
}

#[test]
fn eval_31_fixed32_projection() {
    let mut record = encode_varint_field(1, 100);
    record.extend(encode_fixed32_field(2, 0xDEADBEEF));
    let data = write_transpose(&[record], CompressionType::None);

    let proj = FieldProjection::new().add_field(Field::new(vec![2]));
    let result = read_projected(&data, &proj);
    assert_eq!(result.len(), 1);
    assert_eq!(result[0], encode_fixed32_field(2, 0xDEADBEEF));
}

#[test]
fn eval_31_fixed64_projection() {
    let mut record = encode_varint_field(1, 100);
    record.extend(encode_fixed64_field(2, 0xCAFEBABECAFEBABE));
    let data = write_transpose(&[record], CompressionType::None);

    let proj = FieldProjection::new().add_field(Field::new(vec![2]));
    let result = read_projected(&data, &proj);
    assert_eq!(result.len(), 1);
    assert_eq!(result[0], encode_fixed64_field(2, 0xCAFEBABECAFEBABE));
}

#[test]
fn eval_31_string_field_projection() {
    let mut record = encode_varint_field(1, 42);
    record.extend(encode_bytes_field(2, b"hello world"));
    let data = write_transpose(&[record], CompressionType::None);

    let proj = FieldProjection::new().add_field(Field::new(vec![2]));
    let result = read_projected(&data, &proj);
    assert_eq!(result.len(), 1);
    assert_eq!(result[0], encode_bytes_field(2, b"hello world"));
}
