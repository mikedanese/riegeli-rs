//! Integration tests for RecordPosition, seek, and error recovery.

// Some imports are used only by feature-gated tests; in reduced-feature
// builds they would otherwise trip unused_imports.
#![cfg_attr(
    not(all(feature = "brotli", feature = "zstd", feature = "snappy")),
    allow(unused_imports)
)]
use std::io::{Cursor, Seek, SeekFrom, Write};

use riegeli::{ReaderOptions, RecordReader, RecordWriter, RiegeliError, WriterOptions};

#[cfg(any(feature = "brotli", feature = "zstd"))]
use riegeli::CompressionType;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

#[cfg(feature = "brotli")]
fn write_and_roundtrip(
    records: &[&[u8]],
    options: WriterOptions,
) -> Result<Vec<Vec<u8>>, RiegeliError> {
    let mut buf = Vec::<u8>::new();
    {
        let cursor = Cursor::new(&mut buf);
        let mut w = RecordWriter::new(cursor, options)?;
        for rec in records {
            w.write_record(rec)?;
        }
        w.flush()?;
    }
    let cursor = Cursor::new(&buf);
    let mut reader = RecordReader::new(cursor, ReaderOptions::new())?;
    let mut out = Vec::new();
    while let Some(rec) = reader.read_record()? {
        out.push(rec);
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Adversarial: Brotli compression_level boundary values
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Adversarial: window_log validation — None compression rejection
// ---------------------------------------------------------------------------

/// window_log(Some(N)) with any compressed type (not None/Snappy) must NOT
/// return an error. Verify Brotli and Zstd both accept window_log.
#[test]
#[cfg(all(feature = "brotli", feature = "zstd"))]
fn adv_15_window_log_accepted_for_brotli_and_zstd() {
    struct NullSink;
    impl Write for NullSink {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    impl Seek for NullSink {
        fn seek(&mut self, _pos: SeekFrom) -> std::io::Result<u64> {
            Ok(0)
        }
    }

    // Brotli with window_log — must succeed
    let result = RecordWriter::new(
        NullSink,
        WriterOptions::new()
            .compression(CompressionType::Brotli)
            .window_log(Some(22)),
    );
    assert!(
        result.is_ok(),
        "window_log should be accepted for Brotli: {:?}",
        result.err()
    );

    // Zstd with window_log — must succeed
    let result = RecordWriter::new(
        NullSink,
        WriterOptions::new()
            .compression(CompressionType::Zstd)
            .window_log(Some(22)),
    );
    assert!(
        result.is_ok(),
        "window_log should be accepted for Zstd: {:?}",
        result.err()
    );
}

// ---------------------------------------------------------------------------
// Adversarial: final_padding with empty flush
// ---------------------------------------------------------------------------

/// final_padding and initial_padding can coexist. After close(), both padding
/// policies are applied, and the file is still readable.
#[test]
fn adv_15_final_padding_and_initial_padding_coexist() {
    let records: Vec<Vec<u8>> = (0u32..5).map(|i| format!("rec-{i}").into_bytes()).collect();
    let refs: Vec<&[u8]> = records.iter().map(|r| r.as_slice()).collect();

    let mut buf = Vec::<u8>::new();
    {
        let cursor = Cursor::new(&mut buf);
        let opts = WriterOptions::new()
            .final_padding(65536)
            .initial_padding(65536);
        let mut w = RecordWriter::new(cursor, opts).expect("writer ok");
        for rec in &refs {
            w.write_record(rec).expect("write ok");
        }
        w.flush().expect("flush ok");
        // Drop triggers close() which also applies initial_padding and final_padding
    }

    let size = buf.len() as u64;
    assert_eq!(
        size % 65536,
        0,
        "file with both paddings should be aligned: got {size} bytes"
    );

    // Must still be readable
    let cursor = Cursor::new(&buf);
    let mut reader = RecordReader::new(cursor, ReaderOptions::new()).expect("reader ok");
    let mut decoded = Vec::new();
    while let Some(rec) = reader.read_record().expect("read ok") {
        decoded.push(rec);
    }
    assert_eq!(decoded.len(), records.len());
}

// ---------------------------------------------------------------------------
// Adversarial: bucket_fraction near-boundary values
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Adversarial: compression_level with transpose encoding
// ---------------------------------------------------------------------------

/// compression_level should also work when transpose=true.
/// Verify that the level is threaded through the transpose chunk encoder.
#[test]
#[cfg(feature = "brotli")]
fn adv_15_compression_level_threads_through_transpose() {
    fn encode_u64(mut v: u64) -> Vec<u8> {
        let mut out = Vec::new();
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

    let mut records: Vec<Vec<u8>> = Vec::new();
    for i in 0u32..200 {
        let mut rec = Vec::new();
        rec.push(0x08);
        rec.extend_from_slice(&encode_u64(i as u64));
        rec.push(0x15);
        rec.extend_from_slice(&i.to_le_bytes());
        records.push(rec);
    }
    let refs: Vec<&[u8]> = records.iter().map(|r| r.as_slice()).collect();

    // Both level 1 and level 11 should produce valid files with transpose
    let decoded_low = write_and_roundtrip(
        &refs,
        WriterOptions::new()
            .compression(CompressionType::Brotli)
            .transpose(true)
            .compression_level(1),
    )
    .expect("transpose + brotli level 1 should succeed");

    let decoded_high = write_and_roundtrip(
        &refs,
        WriterOptions::new()
            .compression(CompressionType::Brotli)
            .transpose(true)
            .compression_level(11),
    )
    .expect("transpose + brotli level 11 should succeed");

    assert_eq!(decoded_low.len(), records.len());
    assert_eq!(decoded_high.len(), records.len());
    for i in 0..records.len() {
        assert_eq!(decoded_low[i], records[i]);
        assert_eq!(decoded_high[i], records[i]);
    }
}
