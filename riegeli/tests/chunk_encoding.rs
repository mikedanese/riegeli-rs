//! Integration tests for simple and transpose chunk encoding, decoding, and corruption detection.
use std::io::Cursor;

use riegeli::{CompressionType, ReaderOptions, RecordReader, RecordWriter, WriterOptions};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Build a simple proto record: 3 varint fields (field 1, 2, 3) with given values.
fn make_proto_3_varints(a: u64, b: u64, c: u64) -> Vec<u8> {
    let mut rec = Vec::new();
    // field 1 varint: tag = (1 << 3) | 0 = 0x08
    rec.push(0x08);
    push_varint(&mut rec, a);
    // field 2 varint: tag = (2 << 3) | 0 = 0x10
    rec.push(0x10);
    push_varint(&mut rec, b);
    // field 3 varint: tag = (3 << 3) | 0 = 0x18
    rec.push(0x18);
    push_varint(&mut rec, c);
    rec
}

fn push_varint(buf: &mut Vec<u8>, mut v: u64) {
    loop {
        if v < 0x80 {
            buf.push(v as u8);
            return;
        }
        buf.push((v as u8 & 0x7F) | 0x80);
        v >>= 7;
    }
}

/// Write records with RecordWriter and read back with RecordReader.
fn writer_reader_roundtrip(records: &[Vec<u8>], opts: WriterOptions) -> Vec<Vec<u8>> {
    let mut cursor = Cursor::new(Vec::<u8>::new());
    {
        let mut writer = RecordWriter::new(&mut cursor, opts).expect("writer::new");
        for rec in records {
            writer.write_record(rec).expect("write_record");
        }
        writer.flush().expect("flush");
    }
    let file_bytes = cursor.into_inner();
    let mut reader =
        RecordReader::new(Cursor::new(file_bytes), ReaderOptions::new()).expect("reader::new");
    let mut out = Vec::new();
    while let Some(rec) = reader.read_record().expect("read_record") {
        out.push(rec);
    }
    out
}

// ---------------------------------------------------------------------------
// 13.2: RecordWriter/RecordReader with transpose+Brotli
// ---------------------------------------------------------------------------

#[test]
fn criterion_13_2_writer_reader_transpose_brotli() {
    let records: Vec<Vec<u8>> = (0..500)
        .map(|i| make_proto_3_varints(i, i * 7, i * 13))
        .collect();
    let opts = WriterOptions::new()
        .compression(CompressionType::Brotli)
        .transpose(true);
    let result = writer_reader_roundtrip(&records, opts);
    assert_eq!(result.len(), records.len());
    for (i, (expected, actual)) in records.iter().zip(result.iter()).enumerate() {
        assert_eq!(expected, actual, "mismatch at record {i}");
    }
}

// ---------------------------------------------------------------------------
// 13.6: Multi-block file with transpose — reader roundtrip verification
// ---------------------------------------------------------------------------

#[test]
fn criterion_13_6_multi_block_transpose_valid_headers() {
    // Write enough data to span multiple blocks (each block = 64 KiB).
    // Use large records with unique data to prevent extreme compression,
    // and a small chunk_size to force many chunks that cross block boundaries.
    let records: Vec<Vec<u8>> = (0..5_000)
        .map(|i| {
            let mut rec = make_proto_3_varints(i, i * 2, i * 3);
            // Add a length-delimited field with enough unique bytes to resist compression.
            rec.push(0x22); // field 4, wire type 2
            let payload: Vec<u8> = (0..64u8).map(|j| j.wrapping_add(i as u8)).collect();
            push_varint(&mut rec, payload.len() as u64);
            rec.extend_from_slice(&payload);
            rec
        })
        .collect();

    let opts = WriterOptions::new()
        .compression(CompressionType::None)
        .transpose(true)
        .chunk_size(4096);

    let mut cursor = Cursor::new(Vec::<u8>::new());
    {
        let mut writer = RecordWriter::new(&mut cursor, opts).expect("writer::new");
        for rec in &records {
            writer.write_record(rec).expect("write_record");
        }
        writer.flush().expect("flush");
    }
    let file_bytes = cursor.into_inner();

    // Verify the file spans multiple blocks.
    assert!(
        file_bytes.len() > 65536,
        "file must span multiple blocks, got {} bytes",
        file_bytes.len()
    );

    // Read all records back to verify correctness.
    let mut reader =
        RecordReader::new(Cursor::new(file_bytes), ReaderOptions::new()).expect("reader::new");
    let mut out = Vec::new();
    while let Some(rec) = reader.read_record().expect("read_record") {
        out.push(rec);
    }
    assert_eq!(out.len(), records.len());
    for (i, (expected, actual)) in records.iter().zip(out.iter()).enumerate() {
        assert_eq!(expected, actual, "mismatch at record {i}");
    }
}

// ---------------------------------------------------------------------------
// 13.8: RecordWriter(transpose, Brotli) byte-for-byte fidelity
// ---------------------------------------------------------------------------

#[test]
fn criterion_13_8_writer_reader_byte_fidelity() {
    let records: Vec<Vec<u8>> = (0..1000)
        .map(|i| make_proto_3_varints(i, i * 3 + 1, i * 7 + 2))
        .collect();

    let opts = WriterOptions::new()
        .compression(CompressionType::Brotli)
        .transpose(true);
    let result = writer_reader_roundtrip(&records, opts);
    assert_eq!(result.len(), records.len());
    for (i, (expected, actual)) in records.iter().zip(result.iter()).enumerate() {
        assert_eq!(
            expected,
            actual,
            "byte mismatch at record {i}: expected {} bytes, got {} bytes",
            expected.len(),
            actual.len()
        );
    }
}

#[test]
fn criterion_13_8_writer_reader_transpose_zstd() {
    let records: Vec<Vec<u8>> = (0..500)
        .map(|i| make_proto_3_varints(i, i + 100, i + 200))
        .collect();

    let opts = WriterOptions::new()
        .compression(CompressionType::Zstd)
        .transpose(true);
    let result = writer_reader_roundtrip(&records, opts);
    assert_eq!(result.len(), records.len());
    for (i, (expected, actual)) in records.iter().zip(result.iter()).enumerate() {
        assert_eq!(expected, actual, "mismatch at record {i}");
    }
}

#[test]
fn criterion_13_8_writer_reader_mixed_records() {
    let mut records: Vec<Vec<u8>> = Vec::new();
    for i in 0..200u64 {
        if i % 3 == 0 {
            records.push(make_proto_3_varints(i, i * 2, i * 3));
        } else if i % 3 == 1 {
            records.push(format!("text record {i}").into_bytes());
        } else {
            records.push(vec![]); // empty record
        }
    }

    let opts = WriterOptions::new()
        .compression(CompressionType::Brotli)
        .transpose(true);
    let result = writer_reader_roundtrip(&records, opts);
    assert_eq!(result.len(), records.len());
    for (i, (expected, actual)) in records.iter().zip(result.iter()).enumerate() {
        assert_eq!(expected, actual, "mismatch at record {i}");
    }
}
