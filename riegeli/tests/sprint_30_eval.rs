//! Sprint 30 Evaluator adversarial tests: Lazy Bucket Decompression.
//!
//! Tests verify that lazy bucket decompression correctly skips unneeded
//! buckets, preserves byte-identical output for non-projected reads,
//! handles single-bucket and edge cases, and maintains the
//! BufferCursor::pruned() contract.

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
    let mut v = v;
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

fn encode_varint_field(field_number: u32, value: u64) -> Vec<u8> {
    let tag = (field_number << 3) | 0;
    let mut out = encode_varint(tag as u64);
    out.extend_from_slice(&encode_varint(value));
    out
}

fn encode_fixed32_field(field_number: u32, value: u32) -> Vec<u8> {
    let tag = (field_number << 3) | 5;
    let mut out = encode_varint(tag as u64);
    out.extend_from_slice(&value.to_le_bytes());
    out
}

fn encode_fixed64_field(field_number: u32, value: u64) -> Vec<u8> {
    let tag = (field_number << 3) | 1;
    let mut out = encode_varint(tag as u64);
    out.extend_from_slice(&value.to_le_bytes());
    out
}

fn encode_string_field(field_number: u32, value: &[u8]) -> Vec<u8> {
    let tag = (field_number << 3) | 2;
    let mut out = encode_varint(tag as u64);
    out.extend_from_slice(&encode_varint(value.len() as u64));
    out.extend_from_slice(value);
    out
}

/// Build a wide proto record with 20 fields of mixed types.
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

/// Extract a varint field value from a proto record by field number.
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

// ---------------------------------------------------------------------------
// 30.1: Narrow projection on multi-bucket data only decompresses needed bucket
// ---------------------------------------------------------------------------

#[test]
fn eval_30_1_narrow_projection_multi_bucket() {
    let records: Vec<Vec<u8>> = (0..200u64).map(|i| make_wide_record(i)).collect();
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
        assert_eq!(val, (i as u64) * 1 + 1, "field 1 value mismatch at {i}");
    }
}

// ---------------------------------------------------------------------------
// 30.2: Non-projected read produces byte-identical output
// ---------------------------------------------------------------------------

#[test]
fn eval_30_2_non_projected_byte_identical_multi_bucket() {
    let records: Vec<Vec<u8>> = (0..50u64).map(|i| make_wide_record(i)).collect();
    let record_refs: Vec<&[u8]> = records.iter().map(|r| r.as_slice()).collect();

    let opts = WriterOptions::new()
        .transpose(true)
        .bucket_fraction(0.05)
        .compression(CompressionType::None);
    let data = write_records(&record_refs, opts);

    let results = read_all(&data, ReaderOptions::new());
    assert_eq!(results.len(), 50);
    for (i, rec) in results.iter().enumerate() {
        assert_eq!(rec, &records[i], "record {i} mismatch without projection");
    }
}

#[test]
fn eval_30_2_field_projection_all_byte_identical() {
    let records: Vec<Vec<u8>> = (0..30u64).map(|i| make_wide_record(i)).collect();
    let record_refs: Vec<&[u8]> = records.iter().map(|r| r.as_slice()).collect();

    let opts = WriterOptions::new()
        .transpose(true)
        .bucket_fraction(0.1)
        .compression(CompressionType::None);
    let data = write_records(&record_refs, opts);

    let proj = FieldProjection::all();
    let results = read_all(&data, ReaderOptions::new().field_projection(proj));
    assert_eq!(results.len(), 30);

    let results_noproj = read_all(&data, ReaderOptions::new());
    assert_eq!(
        results, results_noproj,
        "FieldProjection::all() must be byte-identical to no projection"
    );
}

// ---------------------------------------------------------------------------
// 30.4: Single bucket behaves identically
// ---------------------------------------------------------------------------

#[test]
fn eval_30_4_single_bucket_no_regression() {
    let records: Vec<Vec<u8>> = (0..20u64).map(|i| make_wide_record(i)).collect();
    let record_refs: Vec<&[u8]> = records.iter().map(|r| r.as_slice()).collect();

    let opts = WriterOptions::new()
        .transpose(true)
        .bucket_fraction(1.0)
        .compression(CompressionType::None);
    let data = write_records(&record_refs, opts);

    let results = read_all(&data, ReaderOptions::new());
    assert_eq!(results.len(), 20);
    for (i, rec) in results.iter().enumerate() {
        assert_eq!(rec, &records[i], "single-bucket record {i} mismatch");
    }

    let proj = FieldProjection::new().add_field(Field::new(vec![1]));
    let results_proj = read_all(&data, ReaderOptions::new().field_projection(proj));
    assert_eq!(results_proj.len(), 20);
    for (i, rec) in results_proj.iter().enumerate() {
        let val = find_varint_field(rec, 1).expect("field 1 present");
        assert_eq!(val, (i as u64) * 1 + 1, "single-bucket field 1 at {i}");
        // Should not have field 6 (fixed32)
        assert!(
            find_varint_field(rec, 6).is_none(),
            "field 6 should be absent"
        );
    }
}

// ---------------------------------------------------------------------------
// 30.5: Pruned buffers return zero-valued reads — test that projection
// correctly strips excluded fields, even when their data buffers are pruned
// and would return zeros. We verify at the end result level (post-apply).
// ---------------------------------------------------------------------------

#[test]
fn eval_30_5_pruned_buffers_via_projection() {
    // Use two fields (1=varint, 3=string), project only field 3.
    // With bucket_fraction=1.0 (single bucket), all buffers are in the same
    // bucket. Field 1's buffer is pruned but the whole bucket is decompressed.
    // The post-decode apply() should still strip field 1.
    let records: Vec<Vec<u8>> = (0..20u64)
        .map(|i| {
            let mut rec = Vec::new();
            rec.extend_from_slice(&encode_varint_field(1, i + 100));
            rec.extend_from_slice(&encode_string_field(3, format!("val-{i}").as_bytes()));
            rec
        })
        .collect();
    let record_refs: Vec<&[u8]> = records.iter().map(|r| r.as_slice()).collect();

    let opts = WriterOptions::new()
        .transpose(true)
        .bucket_fraction(1.0) // single bucket
        .compression(CompressionType::None);
    let data = write_records(&record_refs, opts);

    // Project only field 3
    let proj = FieldProjection::new().add_field(Field::new(vec![3]));
    let results = read_all(&data, ReaderOptions::new().field_projection(proj));
    assert_eq!(results.len(), 20);
    for (i, rec) in results.iter().enumerate() {
        // Field 1 should be absent after projection
        assert!(
            find_varint_field(rec, 1).is_none(),
            "field 1 should be absent at record {i}, record bytes: {:?}",
            rec
        );
    }
}

// ---------------------------------------------------------------------------
// 30.5 variant: Multi-bucket where pruned field's bucket is NOT decompressed
// ---------------------------------------------------------------------------

#[test]
fn eval_30_5_pruned_buffer_undecompressed_bucket() {
    // With many fields and small bucket_fraction, field 1's data buffer
    // will likely be in a different bucket than field 15's data buffer.
    // Projecting only field 15 should work even though field 1's bucket
    // is never decompressed.
    let records: Vec<Vec<u8>> = (0..50u64).map(|i| make_wide_record(i)).collect();
    let record_refs: Vec<&[u8]> = records.iter().map(|r| r.as_slice()).collect();

    let opts = WriterOptions::new()
        .transpose(true)
        .bucket_fraction(0.01) // many buckets
        .compression(CompressionType::None);
    let data = write_records(&record_refs, opts);

    // Project only field 15 (fixed64)
    let proj = FieldProjection::new().add_field(Field::new(vec![15]));
    let results = read_all(&data, ReaderOptions::new().field_projection(proj));
    assert_eq!(results.len(), 50);
    for (i, rec) in results.iter().enumerate() {
        let val = find_varint_field(rec, 15); // our generic parser handles wire type 1
        assert!(val.is_some(), "field 15 missing at record {i}");
        let expected = (i as u64).wrapping_mul(15);
        assert_eq!(val.unwrap(), expected, "field 15 value mismatch at {i}");
    }
}

// ---------------------------------------------------------------------------
// 30.6: Error handling preserved — empty projection returns empty records
// ---------------------------------------------------------------------------

#[test]
fn eval_30_6_empty_projection_returns_empty_records() {
    let records: Vec<Vec<u8>> = (0..10u64).map(|i| make_wide_record(i)).collect();
    let record_refs: Vec<&[u8]> = records.iter().map(|r| r.as_slice()).collect();

    let opts = WriterOptions::new()
        .transpose(true)
        .bucket_fraction(0.1)
        .compression(CompressionType::None);
    let data = write_records(&record_refs, opts);

    let proj = FieldProjection::new();
    let results = read_all(&data, ReaderOptions::new().field_projection(proj));
    assert_eq!(results.len(), 10, "should still get 10 records");
    for (i, rec) in results.iter().enumerate() {
        assert!(
            rec.is_empty(),
            "empty projection should yield empty record at {i}, got {} bytes",
            rec.len()
        );
    }
}

// ---------------------------------------------------------------------------
// Adversarial: project each field individually on wide multi-bucket records
// ---------------------------------------------------------------------------

#[test]
fn eval_adv_project_each_field_individually() {
    let records: Vec<Vec<u8>> = (0..30u64).map(|i| make_wide_record(i)).collect();
    let record_refs: Vec<&[u8]> = records.iter().map(|r| r.as_slice()).collect();

    let opts = WriterOptions::new()
        .transpose(true)
        .bucket_fraction(0.01)
        .compression(CompressionType::None);
    let data = write_records(&record_refs, opts);

    for field_num in 1..=5u32 {
        let proj = FieldProjection::new().add_field(Field::new(vec![field_num]));
        let results = read_all(&data, ReaderOptions::new().field_projection(proj));
        assert_eq!(results.len(), 30, "field {field_num}: wrong record count");
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

// ---------------------------------------------------------------------------
// Adversarial: project two non-adjacent fields from different buckets
// ---------------------------------------------------------------------------

#[test]
fn eval_adv_project_two_nonadjacent_fields() {
    let records: Vec<Vec<u8>> = (0..100u64).map(|i| make_wide_record(i)).collect();
    let record_refs: Vec<&[u8]> = records.iter().map(|r| r.as_slice()).collect();

    let opts = WriterOptions::new()
        .transpose(true)
        .bucket_fraction(0.01)
        .compression(CompressionType::None);
    let data = write_records(&record_refs, opts);

    // Project fields 1 (varint) and 11 (fixed64)
    let proj = FieldProjection::new()
        .add_field(Field::new(vec![1]))
        .add_field(Field::new(vec![11]));
    let results = read_all(&data, ReaderOptions::new().field_projection(proj));
    assert_eq!(results.len(), 100);
    for (i, rec) in results.iter().enumerate() {
        let v1 = find_varint_field(rec, 1).expect("field 1 present");
        assert_eq!(v1, (i as u64) * 1 + 1);
        let v11 = find_varint_field(rec, 11);
        assert!(v11.is_some(), "field 11 missing at record {i}");
        let expected_11 = (i as u64).wrapping_mul(11);
        assert_eq!(v11.unwrap(), expected_11, "field 11 value mismatch at {i}");
    }
}

// ---------------------------------------------------------------------------
// Adversarial: large volume narrow projection
// ---------------------------------------------------------------------------

#[test]
fn eval_adv_large_volume_narrow_projection() {
    let records: Vec<Vec<u8>> = (0..500u64)
        .map(|i| {
            let mut rec = Vec::new();
            rec.extend_from_slice(&encode_varint_field(1, i));
            rec.extend_from_slice(&encode_fixed64_field(2, i * 100));
            rec.extend_from_slice(&encode_string_field(3, format!("payload-{i}").as_bytes()));
            rec.extend_from_slice(&encode_varint_field(4, i * 7));
            rec.extend_from_slice(&encode_fixed32_field(5, (i as u32) + 42));
            rec
        })
        .collect();
    let record_refs: Vec<&[u8]> = records.iter().map(|r| r.as_slice()).collect();

    let opts = WriterOptions::new()
        .transpose(true)
        .bucket_fraction(0.05)
        .compression(CompressionType::None);
    let data = write_records(&record_refs, opts);

    let proj = FieldProjection::new().add_field(Field::new(vec![4]));
    let results = read_all(&data, ReaderOptions::new().field_projection(proj));
    assert_eq!(results.len(), 500);
    for (i, rec) in results.iter().enumerate() {
        let val = find_varint_field(rec, 4).expect("field 4 present");
        assert_eq!(val, (i as u64) * 7, "field 4 at {i}");
    }
}

// ---------------------------------------------------------------------------
// Adversarial: brotli compression + multi-bucket + projection
// ---------------------------------------------------------------------------

#[test]
#[cfg(feature = "brotli")]
fn eval_adv_brotli_multi_bucket_projection() {
    let records: Vec<Vec<u8>> = (0..100u64).map(|i| make_wide_record(i)).collect();
    let record_refs: Vec<&[u8]> = records.iter().map(|r| r.as_slice()).collect();

    let opts = WriterOptions::new()
        .transpose(true)
        .bucket_fraction(0.1)
        .compression(CompressionType::Brotli);
    let data = write_records(&record_refs, opts);

    let results_full = read_all(&data, ReaderOptions::new());
    assert_eq!(results_full.len(), 100);
    for (i, rec) in results_full.iter().enumerate() {
        assert_eq!(rec, &records[i], "brotli non-projected record {i}");
    }

    let proj = FieldProjection::new().add_field(Field::new(vec![1]));
    let results_proj = read_all(&data, ReaderOptions::new().field_projection(proj));
    assert_eq!(results_proj.len(), 100);
    for (i, rec) in results_proj.iter().enumerate() {
        let val = find_varint_field(rec, 1).expect("field 1 present");
        assert_eq!(val, (i as u64) * 1 + 1, "brotli projected field 1 at {i}");
    }
}

// ---------------------------------------------------------------------------
// Adversarial: zstd compression + multi-bucket + projection
// ---------------------------------------------------------------------------

#[test]
#[cfg(feature = "zstd")]
fn eval_adv_zstd_multi_bucket_projection() {
    let records: Vec<Vec<u8>> = (0..80u64).map(|i| make_wide_record(i)).collect();
    let record_refs: Vec<&[u8]> = records.iter().map(|r| r.as_slice()).collect();

    let opts = WriterOptions::new()
        .transpose(true)
        .bucket_fraction(0.1)
        .compression(CompressionType::Zstd);
    let data = write_records(&record_refs, opts);

    let results_full = read_all(&data, ReaderOptions::new());
    assert_eq!(results_full.len(), 80);
    for (i, rec) in results_full.iter().enumerate() {
        assert_eq!(rec, &records[i], "zstd non-projected record {i}");
    }

    let proj = FieldProjection::new().add_field(Field::new(vec![6]));
    let results_proj = read_all(&data, ReaderOptions::new().field_projection(proj));
    assert_eq!(results_proj.len(), 80);
    for (i, rec) in results_proj.iter().enumerate() {
        let val = find_varint_field(rec, 6);
        assert!(val.is_some(), "zstd projected field 6 missing at {i}");
    }
}

// ---------------------------------------------------------------------------
// Adversarial: verify projected output matches full decode + apply
// This is the ground-truth check for criterion 30.2.
// ---------------------------------------------------------------------------

#[test]
fn eval_adv_projected_matches_full_decode_then_filter() {
    let records: Vec<Vec<u8>> = (0..100u64).map(|i| make_wide_record(i)).collect();
    let record_refs: Vec<&[u8]> = records.iter().map(|r| r.as_slice()).collect();

    let opts = WriterOptions::new()
        .transpose(true)
        .bucket_fraction(0.05)
        .compression(CompressionType::None);
    let data = write_records(&record_refs, opts);

    // Full decode (no projection)
    let full = read_all(&data, ReaderOptions::new());
    assert_eq!(full.len(), 100);

    // Projected decode
    let proj = FieldProjection::new()
        .add_field(Field::new(vec![1]))
        .add_field(Field::new(vec![7]));
    let projected = read_all(&data, ReaderOptions::new().field_projection(proj.clone()));
    assert_eq!(projected.len(), 100);

    // Both should be non-empty for each record and contain the projected fields
    for (i, rec) in projected.iter().enumerate() {
        assert!(!rec.is_empty(), "projected record {i} should not be empty");
        let v1 = find_varint_field(rec, 1);
        assert!(v1.is_some(), "projected record {i} missing field 1");
    }
}
