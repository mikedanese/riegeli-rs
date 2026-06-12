//! Integration tests for projection over multi-bucket and compressed
//! transpose chunks: lazy, per-bucket decompression must not change the
//! decoded output.

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

fn encode_bytes_field(field_number: u32, data: &[u8]) -> Vec<u8> {
    let mut out = encode_tag(field_number, 2);
    out.extend(encode_varint(data.len() as u64));
    out.extend(data);
    out
}

// ---------------------------------------------------------------------------
// Write/read helpers
// ---------------------------------------------------------------------------

fn write_with_opts(records: &[Vec<u8>], opts: WriterOptions) -> Vec<u8> {
    let mut buf = Vec::new();
    {
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

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// An explicitly enumerated full projection (include map present, as opposed
/// to a projection-disabled read) over a multi-bucket layout
/// (bucket_fraction 0.1) must be byte-identical to a non-projected read.
#[test]
fn explicit_full_projection_multi_bucket_byte_identical() {
    let records: Vec<Vec<u8>> = (0..50u64)
        .map(|i| {
            let mut r = encode_varint_field(1, i);
            r.extend(encode_fixed32_field(2, i as u32));
            r.extend(encode_bytes_field(3, format!("v{i}").as_bytes()));
            r
        })
        .collect();
    let data = write_with_opts(
        &records,
        WriterOptions::default()
            .compression(CompressionType::None)
            .transpose(true)
            .bucket_fraction(0.1),
    );

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
            "record {i}: full projection with bucket_fraction should be byte-identical"
        );
    }
}

/// An empty projection over Snappy-compressed buckets must still return the
/// right number of records, each pruned down to empty.
#[test]
#[cfg(feature = "snappy")]
fn empty_projection_snappy_compressed_returns_empty_records() {
    let records: Vec<Vec<u8>> = (0..20u64)
        .map(|i| {
            let mut r = encode_varint_field(1, i);
            r.extend(encode_bytes_field(2, &[0xAA; 100]));
            r
        })
        .collect();
    let data = write_with_opts(
        &records,
        WriterOptions::default()
            .compression(CompressionType::Snappy)
            .transpose(true),
    );

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
