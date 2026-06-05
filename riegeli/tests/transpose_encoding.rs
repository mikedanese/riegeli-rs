//! Integration tests for transpose chunk encoding roundtrips and edge cases.
use std::io::Cursor;

use riegeli::{CompressionType, ReaderOptions, RecordReader, RecordWriter, WriterOptions};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

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

fn make_proto_3_varints(a: u64, b: u64, c: u64) -> Vec<u8> {
    let mut rec = Vec::new();
    rec.push(0x08);
    push_varint(&mut rec, a);
    rec.push(0x10);
    push_varint(&mut rec, b);
    rec.push(0x18);
    push_varint(&mut rec, c);
    rec
}

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
// 13.6: Multi-block boundary verification — reader round-trip only
// ---------------------------------------------------------------------------

/// Multi-block with Brotli compression -- verify records are readable.
#[test]
#[cfg(feature = "brotli")]
fn eval_13_6_multi_block_brotli() {
    // Each record is 128 bytes. 1000 records should exceed 64 KiB.
    let mut records: Vec<Vec<u8>> = Vec::new();
    let mut state: u64 = 12345678901234567890;
    for _ in 0..1000 {
        let mut rec = Vec::with_capacity(128);
        for _ in 0..128 {
            state = state
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            rec.push((state >> 33) as u8);
        }
        records.push(rec);
    }

    let opts = WriterOptions::new()
        .compression(CompressionType::Brotli)
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
        "expected file > 64 KiB, got {} bytes",
        file_bytes.len()
    );

    // And read back all records.
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
// 13.8: RecordWriter fidelity with edge-case records
// ---------------------------------------------------------------------------

/// A mix of proto, nonproto, empty, and large records through RecordWriter.
#[test]
#[cfg(feature = "brotli")]
fn eval_13_8_writer_reader_diverse_records() {
    let mut records: Vec<Vec<u8>> = Vec::new();
    // Proto records
    for i in 0..100 {
        records.push(make_proto_3_varints(i, i * 2, i * 3));
    }
    // Nonproto (invalid proto)
    for i in 0..50 {
        records.push(vec![0xFF; i + 1]);
    }
    // Empty records
    for _ in 0..20 {
        records.push(vec![]);
    }
    // Large record
    let mut large = Vec::new();
    large.push(0x0A); // field 1, length-delimited
    let payload = vec![0xAB; 8000];
    push_varint(&mut large, payload.len() as u64);
    large.extend_from_slice(&payload);
    records.push(large);

    let opts = WriterOptions::new()
        .compression(CompressionType::Brotli)
        .transpose(true);
    let result = writer_reader_roundtrip(&records, opts);
    assert_eq!(result.len(), records.len());
    for (i, (expected, actual)) in records.iter().zip(result.iter()).enumerate() {
        assert_eq!(expected, actual, "mismatch at record {i}");
    }
}

/// Writer/reader round-trip with transpose+None (no compression).
#[test]
fn eval_13_8_writer_reader_transpose_none() {
    let records: Vec<Vec<u8>> = (0..500)
        .map(|i| make_proto_3_varints(i, i + 1, i + 2))
        .collect();
    let opts = WriterOptions::new()
        .compression(CompressionType::None)
        .transpose(true);
    let result = writer_reader_roundtrip(&records, opts);
    assert_eq!(result.len(), records.len());
    for (i, (expected, actual)) in records.iter().zip(result.iter()).enumerate() {
        assert_eq!(expected, actual, "mismatch at record {i}");
    }
}

/// Records with fixed32 and fixed64 fields through writer/reader.
#[test]
#[cfg(feature = "brotli")]
fn eval_13_8_writer_reader_fixed_fields() {
    let mut records: Vec<Vec<u8>> = Vec::new();
    for i in 0u32..200 {
        let mut rec = Vec::new();
        // field 1, fixed32 (wire type 5 = tag 0x0D)
        rec.push(0x0D);
        rec.extend_from_slice(&i.to_le_bytes());
        // field 2, fixed64 (wire type 1 = tag 0x11)
        rec.push(0x11);
        rec.extend_from_slice(&(i as u64 * 1000).to_le_bytes());
        // field 3, varint
        rec.push(0x18);
        push_varint(&mut rec, i as u64);
        records.push(rec);
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

/// Seek after reading some records in a transpose file.
#[test]
#[cfg(feature = "brotli")]
fn eval_13_8_writer_reader_seek_transpose() {
    let records: Vec<Vec<u8>> = (0..100)
        .map(|i| make_proto_3_varints(i, i * 2, i * 3))
        .collect();

    let opts = WriterOptions::new()
        .compression(CompressionType::Brotli)
        .transpose(true);

    let mut cursor = Cursor::new(Vec::<u8>::new());
    {
        let mut writer = RecordWriter::new(&mut cursor, opts).expect("writer::new");
        for rec in &records {
            writer.write_record(rec).expect("write_record");
        }
        writer.flush().expect("flush");
    }
    let file_bytes = cursor.into_inner();

    let mut reader =
        RecordReader::new(Cursor::new(file_bytes), ReaderOptions::new()).expect("reader::new");

    // Read all records, noting positions.
    let mut positions = Vec::new();
    while let Some(_rec) = reader.read_record().expect("read_record") {
        positions.push(reader.last_pos());
    }
    assert_eq!(positions.len(), 100);

    // Seek back to record 0 and verify.
    reader.seek_numeric(positions[0].numeric()).expect("seek");
    let rec = reader
        .read_record()
        .expect("read_record")
        .expect("should have record");
    assert_eq!(rec, records[0]);
}
