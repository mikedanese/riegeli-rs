//! Tests verifying correctness properties relied on by the benchmark harness.

use std::io::Cursor;

use riegeli::{CompressionType, ReaderOptions, RecordReader, RecordWriter, WriterOptions};

use riegeli_ffi::{
    Compression as FfiCompression, RecordReader as FfiReader, RecordWriter as FfiWriter,
    WriterOptions as FfiWriterOptions,
};

/// Generate benchmark-style payload of n records at exact size.
fn make_payload(n: usize, size: usize) -> Vec<Vec<u8>> {
    (0..n)
        .map(|i| {
            let mut rec = Vec::with_capacity(size);
            let prefix = format!(
                "{{\"id\":{},\"seq\":{},\"tag\":\"bench-record\",\"data\":\"",
                i,
                i * 7 + 13
            );
            rec.extend_from_slice(prefix.as_bytes());
            let pattern: [u8; 16] = [
                (i & 0xff) as u8,
                ((i >> 8) & 0xff) as u8,
                0xAA,
                0x55,
                (i & 0xff) as u8,
                0xBB,
                0x33,
                ((i >> 4) & 0xff) as u8,
                0xDE,
                0xAD,
                (i & 0x3f) as u8,
                0xBE,
                0xEF,
                ((i >> 2) & 0xff) as u8,
                0xCA,
                0xFE,
            ];
            while rec.len() < size.saturating_sub(2) {
                let remaining = size.saturating_sub(2) - rec.len();
                let chunk = remaining.min(pattern.len());
                rec.extend_from_slice(&pattern[..chunk]);
            }
            rec.extend_from_slice(b"\"}");
            rec.truncate(size);
            while rec.len() < size {
                rec.push(0x20);
            }
            rec
        })
        .collect()
}

// ---- Round-trip correctness for all 6 configs (small batch) ----

fn rust_write_read_roundtrip(opts: WriterOptions, records: &[Vec<u8>]) {
    let buf: Vec<u8> = Vec::new();
    let mut cursor = Cursor::new(buf);
    let mut writer = RecordWriter::new(&mut cursor, opts).unwrap();
    for rec in records {
        writer.write_record(rec).unwrap();
    }
    writer.close().unwrap();
    let data = cursor.into_inner();
    assert!(!data.is_empty(), "Rust writer produced empty output");

    let mut reader = RecordReader::new(Cursor::new(&data), ReaderOptions::new()).unwrap();
    let mut count = 0;
    while let Some(rec) = reader.read_record().unwrap() {
        assert_eq!(
            rec.as_slice(),
            records[count].as_slice(),
            "Record {} mismatch",
            count
        );
        count += 1;
    }
    assert_eq!(count, records.len());
}

fn cpp_write_read_roundtrip(opts: FfiWriterOptions, records: &[Vec<u8>]) {
    let mut writer = FfiWriter::new(opts).unwrap();
    for rec in records {
        writer.write_record(rec).unwrap();
    }
    let data = writer.close().unwrap();
    assert!(!data.is_empty(), "C++ writer produced empty output");

    let mut reader = FfiReader::new(&data).unwrap();
    let mut count = 0;
    while let Some(rec) = reader.read_record().unwrap() {
        assert_eq!(
            rec.as_slice(),
            records[count].as_slice(),
            "Record {} mismatch",
            count
        );
        count += 1;
    }
    reader.close().unwrap();
    assert_eq!(count, records.len());
}

#[test]
fn roundtrip_simple_none() {
    let records = make_payload(100, 100);
    rust_write_read_roundtrip(
        WriterOptions::new().compression(CompressionType::None),
        &records,
    );
    cpp_write_read_roundtrip(
        FfiWriterOptions::new().compression(FfiCompression::None),
        &records,
    );
}

#[test]
fn roundtrip_simple_brotli() {
    let records = make_payload(100, 100);
    rust_write_read_roundtrip(
        WriterOptions::new()
            .compression(CompressionType::Brotli)
            .compression_level(6),
        &records,
    );
    cpp_write_read_roundtrip(
        FfiWriterOptions::new().compression(FfiCompression::Brotli(6)),
        &records,
    );
}

#[test]
fn roundtrip_simple_zstd() {
    let records = make_payload(100, 100);
    rust_write_read_roundtrip(
        WriterOptions::new()
            .compression(CompressionType::Zstd)
            .compression_level(3),
        &records,
    );
    cpp_write_read_roundtrip(
        FfiWriterOptions::new().compression(FfiCompression::Zstd(3)),
        &records,
    );
}

#[test]
fn roundtrip_transpose_none() {
    let records = make_payload(100, 100);
    rust_write_read_roundtrip(
        WriterOptions::new()
            .transpose(true)
            .compression(CompressionType::None),
        &records,
    );
    cpp_write_read_roundtrip(
        FfiWriterOptions::new()
            .transpose(true)
            .compression(FfiCompression::None),
        &records,
    );
}

#[test]
fn roundtrip_transpose_brotli() {
    let records = make_payload(100, 100);
    rust_write_read_roundtrip(
        WriterOptions::new()
            .transpose(true)
            .compression(CompressionType::Brotli)
            .compression_level(6),
        &records,
    );
    cpp_write_read_roundtrip(
        FfiWriterOptions::new()
            .transpose(true)
            .compression(FfiCompression::Brotli(6)),
        &records,
    );
}

#[test]
fn roundtrip_transpose_zstd() {
    let records = make_payload(100, 100);
    rust_write_read_roundtrip(
        WriterOptions::new()
            .transpose(true)
            .compression(CompressionType::Zstd)
            .compression_level(3),
        &records,
    );
    cpp_write_read_roundtrip(
        FfiWriterOptions::new()
            .transpose(true)
            .compression(FfiCompression::Zstd(3)),
        &records,
    );
}

// ---- Compression ratio agreement between Rust and C++ ----

fn compression_ratio_check(
    rust_opts: WriterOptions,
    ffi_opts: FfiWriterOptions,
    records: &[Vec<u8>],
    config_name: &str,
) {
    let raw_bytes: usize = records.iter().map(|r| r.len()).sum();

    // Rust write
    let buf: Vec<u8> = Vec::new();
    let mut cursor = Cursor::new(buf);
    let mut writer = RecordWriter::new(&mut cursor, rust_opts).unwrap();
    for rec in records {
        writer.write_record(rec).unwrap();
    }
    writer.close().unwrap();
    let rust_file = cursor.into_inner();

    // C++ write
    let mut cpp_writer = FfiWriter::new(ffi_opts).unwrap();
    for rec in records {
        cpp_writer.write_record(rec).unwrap();
    }
    let cpp_file = cpp_writer.close().unwrap();

    let rust_ratio = rust_file.len() as f64 / raw_bytes as f64;
    let cpp_ratio = cpp_file.len() as f64 / raw_bytes as f64;

    let diff = (rust_ratio - cpp_ratio).abs() / cpp_ratio;
    assert!(
        diff <= 0.20,
        "{}: compression ratio mismatch: Rust={:.4}, C++={:.4}, diff={:.1}%",
        config_name,
        rust_ratio,
        cpp_ratio,
        diff * 100.0
    );
}

#[test]
fn compression_ratio_agreement_all_configs_small() {
    let records = make_payload(1000, 100);
    compression_ratio_check(
        WriterOptions::new().compression(CompressionType::None),
        FfiWriterOptions::new().compression(FfiCompression::None),
        &records,
        "simple+none/small",
    );
    compression_ratio_check(
        WriterOptions::new()
            .compression(CompressionType::Brotli)
            .compression_level(6),
        FfiWriterOptions::new().compression(FfiCompression::Brotli(6)),
        &records,
        "simple+brotli:6/small",
    );
    compression_ratio_check(
        WriterOptions::new()
            .compression(CompressionType::Zstd)
            .compression_level(3),
        FfiWriterOptions::new().compression(FfiCompression::Zstd(3)),
        &records,
        "simple+zstd:3/small",
    );
}

#[test]
fn compression_ratio_agreement_all_configs_large() {
    let records = make_payload(100, 10_240);
    compression_ratio_check(
        WriterOptions::new()
            .transpose(true)
            .compression(CompressionType::None),
        FfiWriterOptions::new()
            .transpose(true)
            .compression(FfiCompression::None),
        &records,
        "transpose+none/large",
    );
    compression_ratio_check(
        WriterOptions::new()
            .transpose(true)
            .compression(CompressionType::Brotli)
            .compression_level(6),
        FfiWriterOptions::new()
            .transpose(true)
            .compression(FfiCompression::Brotli(6)),
        &records,
        "transpose+brotli:6/large",
    );
    compression_ratio_check(
        WriterOptions::new()
            .transpose(true)
            .compression(CompressionType::Zstd)
            .compression_level(3),
        FfiWriterOptions::new()
            .transpose(true)
            .compression(FfiCompression::Zstd(3)),
        &records,
        "transpose+zstd:3/large",
    );
}

// ---- Verify 10,000 records constant matches contract ----

#[test]
fn ten_thousand_records_roundtrip_simple_none() {
    // Verify the benchmark's record count (10,000) actually works
    let records = make_payload(10_000, 100);
    assert_eq!(records.len(), 10_000);

    let buf: Vec<u8> = Vec::with_capacity(10_000 * 100 * 2);
    let mut cursor = Cursor::new(buf);
    let mut writer = RecordWriter::new(
        &mut cursor,
        WriterOptions::new().compression(CompressionType::None),
    )
    .unwrap();
    for rec in &records {
        writer.write_record(rec).unwrap();
    }
    writer.close().unwrap();
    let data = cursor.into_inner();

    let mut reader = RecordReader::new(Cursor::new(&data), ReaderOptions::new()).unwrap();
    let mut count = 0;
    while let Some(_) = reader.read_record().unwrap() {
        count += 1;
    }
    assert_eq!(count, 10_000, "Expected exactly 10,000 records back");
}
