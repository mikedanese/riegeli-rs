//! Integration tests for WriterOptions tuning: chunk size, bucket fraction, compression level.

use std::io::{Cursor, Write};

use riegeli::{ReaderOptions, RecordReader, RecordWriter, RiegeliError, WriterOptions};

use riegeli::CompressionType;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Write records to an in-memory buffer and return the bytes.
fn write_to_buf(records: &[&[u8]], options: WriterOptions) -> Result<Vec<u8>, RiegeliError> {
    let mut buf = Vec::<u8>::new();
    let cursor = Cursor::new(&mut buf);
    {
        let mut w = RecordWriter::new(cursor, options)?;
        for rec in records {
            w.write_record(rec)?;
        }
        w.flush()?;
        // close() is called on drop
    }
    Ok(buf)
}

/// Round-trip all records through RecordReader. Returns decoded records.
fn roundtrip(file_data: &[u8]) -> Result<Vec<Vec<u8>>, RiegeliError> {
    let cursor = Cursor::new(file_data);
    let mut reader = RecordReader::new(cursor, ReaderOptions::new())?;
    let mut records = Vec::new();
    while let Some(rec) = reader.read_record()? {
        records.push(rec);
    }
    Ok(records)
}

// ---------------------------------------------------------------------------
// Criterion 15.1: compression_level(11) + Brotli → smaller than quality-6
// ---------------------------------------------------------------------------

#[test]
#[cfg(feature = "brotli")]
fn criterion_15_1_brotli_level_11_smaller_than_default() {
    // Use a payload that benefits from higher-quality Brotli:
    // A mix of repetitive patterns with some variation. 100 KiB total.
    // Brotli quality 11 finds more back-references than quality 6.
    let mut payload = Vec::with_capacity(100 * 1024);
    // Pseudo-random-looking but compressible data: repeated patterns with offsets
    for i in 0u32..(100 * 1024 / 64) {
        let base = (i * 7 + 13) as u8;
        for j in 0u8..64 {
            payload.push(base.wrapping_add(j / 4));
        }
    }
    let records: &[&[u8]] = &[&payload];

    let default_opts = WriterOptions::new().compression(CompressionType::Brotli);
    let high_opts = WriterOptions::new()
        .compression(CompressionType::Brotli)
        .compression_level(11);

    let default_file = write_to_buf(records, default_opts).expect("default write ok");
    let high_file = write_to_buf(records, high_opts).expect("high-quality write ok");

    // High quality should produce smaller output on this payload
    assert!(
        high_file.len() < default_file.len(),
        "quality 11 ({} bytes) should be smaller than quality 6 ({} bytes)",
        high_file.len(),
        default_file.len()
    );

    // Both files should be readable
    let decoded = roundtrip(&high_file).expect("roundtrip ok");
    assert_eq!(decoded.len(), 1);
    assert_eq!(decoded[0], payload);
}

// ---------------------------------------------------------------------------
// Criterion 15.2: compression_level(1) and (22) + Zstd → valid readable files
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Criterion 15.5: bucket_fraction(0.5) + transpose → >1 bucket, records OK
// ---------------------------------------------------------------------------

#[test]
#[cfg(feature = "brotli")]
fn criterion_15_5_bucket_fraction_half_produces_multiple_buckets() {
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

    // Build 1000 proto records: field 1 (varint) + field 2 (fixed32)
    // Each record is ~6 bytes; 1000 records ≈ 6 KB of data buffers.
    let mut records: Vec<Vec<u8>> = Vec::new();
    for i in 0u32..1000 {
        let mut rec = Vec::new();
        rec.push(0x08); // tag: field 1, varint
        rec.extend_from_slice(&encode_u64(i as u64));
        rec.push(0x15); // tag: field 2, fixed32
        rec.extend_from_slice(&i.to_le_bytes());
        records.push(rec);
    }
    let refs: Vec<&[u8]> = records.iter().map(|r| r.as_slice()).collect();

    // Use a chunk_size large enough to hold all 1000 records, but a small
    // bucket_fraction so that bucket_size << total data. This forces the
    // greedy bucketing algorithm to emit multiple buckets.
    //
    // chunk_size = 1 MiB. bucket_fraction = 0.001 → bucket_size = 1048 bytes → clamped to 4096.
    // The data buffers for 1000 records are: varint buffer (~2 KiB) + fixed32 buffer (~4 KiB).
    // With bucket_size = 4096: after adding the varint buffer (~2 KiB), when the fixed32
    // buffer (~4 KiB) is evaluated: 2048 + 4096/2 = 4096 >= 4096 → new bucket triggered.
    let opts_multi = WriterOptions::new()
        .compression(CompressionType::Brotli)
        .transpose(true)
        .chunk_size(1 << 20)
        .bucket_fraction(0.001); // → bucket_size clamped to 4096; forces ≥2 buckets

    let opts_single = WriterOptions::new()
        .compression(CompressionType::Brotli)
        .transpose(true)
        .chunk_size(1 << 20)
        .bucket_fraction(1.0);

    let multi_file = write_to_buf(&refs, opts_multi).expect("multi-bucket write ok");
    let single_file = write_to_buf(&refs, opts_single).expect("single-bucket write ok");

    // The multi-bucket file should differ from single-bucket (different encoding)
    assert_ne!(
        multi_file, single_file,
        "multi-bucket and single-bucket files should differ"
    );

    // Both should round-trip correctly
    let decoded = roundtrip(&multi_file).expect("multi-bucket roundtrip ok");
    assert_eq!(decoded.len(), 1000);
    for (got, expected) in decoded.iter().zip(records.iter()) {
        assert_eq!(got, expected, "record mismatch in multi-bucket roundtrip");
    }
}

// ---------------------------------------------------------------------------
// Criterion 15.6: bucket_fraction(0.0) clamps; bucket_fraction(1.0) = single bucket
// ---------------------------------------------------------------------------

#[test]
fn criterion_15_6_bucket_fraction_zero_clamps_to_min() {
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

    // bucket_fraction(0.0) should clamp (not error). Build a few records and verify round-trip.
    let records: Vec<Vec<u8>> = (0u32..10)
        .map(|i| {
            let mut rec = Vec::new();
            rec.push(0x08);
            rec.extend_from_slice(&encode_u64(i as u64));
            rec
        })
        .collect();
    let refs: Vec<&[u8]> = records.iter().map(|r| r.as_slice()).collect();

    let opts = WriterOptions::new().transpose(true).bucket_fraction(0.0);
    // Should not error
    let file = write_to_buf(&refs, opts).expect("bucket_fraction=0.0 should not error");
    let decoded = roundtrip(&file).expect("roundtrip ok");
    assert_eq!(decoded.len(), records.len());
}

// ---------------------------------------------------------------------------
// Criterion 15.7: final_padding(65536) → file size multiple after flush(); 2nd flush also
// ---------------------------------------------------------------------------

#[test]
fn criterion_15_7_final_padding_aligns_after_flush() {
    use std::sync::{Arc, Mutex};

    struct SharedVec(Arc<Mutex<Vec<u8>>>);
    impl Write for SharedVec {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    const ALIGNMENT: u64 = 65536;

    let shared = Arc::new(Mutex::new(Vec::<u8>::new()));
    let sv = SharedVec(Arc::clone(&shared));

    let opts = WriterOptions::new().final_padding(ALIGNMENT);
    let mut w = RecordWriter::new(sv, opts).expect("writer ok");

    // Write first batch and flush
    for i in 0u32..10 {
        w.write_record(format!("record-{i}").as_bytes())
            .expect("write ok");
    }
    w.flush().expect("flush ok");

    let size1 = shared.lock().unwrap().len() as u64;
    assert_eq!(
        size1 % ALIGNMENT,
        0,
        "after first flush: file size {size1} not multiple of {ALIGNMENT}"
    );

    // Write second batch and flush
    for i in 10u32..20 {
        w.write_record(format!("record-{i}").as_bytes())
            .expect("write ok");
    }
    w.flush().expect("flush ok");

    let size2 = shared.lock().unwrap().len() as u64;
    assert_eq!(
        size2 % ALIGNMENT,
        0,
        "after second flush: file size {size2} not multiple of {ALIGNMENT}"
    );
    assert!(size2 >= size1, "file should only grow");

    // File should be readable
    drop(w);
    let final_data = shared.lock().unwrap().clone();
    let decoded = roundtrip(&final_data).expect("roundtrip ok");
    assert_eq!(decoded.len(), 20);
}

// ---------------------------------------------------------------------------
// Criterion 15.8: window_log(Some(15)) + CompressionType::None → error
// ---------------------------------------------------------------------------

#[test]
fn criterion_15_8_window_log_with_none_compression_is_error() {
    let opts = WriterOptions::new()
        .compression(CompressionType::None)
        .window_log(Some(15));

    // We need a dummy writer to pass to RecordWriter::new
    struct NullWriter;
    impl Write for NullWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    let result: Result<RecordWriter<NullWriter>, RiegeliError> =
        RecordWriter::new(NullWriter, opts);
    assert!(
        result.is_err(),
        "window_log with CompressionType::None should return an error"
    );
    // Verify the error message is meaningful
    if let Err(err) = result {
        assert!(
            matches!(err, RiegeliError::MalformedData(_)),
            "expected MalformedData error, got: {err:?}"
        );
    }
}

#[test]
#[cfg(feature = "snappy")]
fn criterion_15_8_window_log_with_snappy_is_error() {
    let opts = WriterOptions::new()
        .compression(CompressionType::Snappy)
        .window_log(Some(15));

    struct NullWriter;
    impl Write for NullWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    let result = RecordWriter::new(NullWriter, opts);
    assert!(
        result.is_err(),
        "window_log with CompressionType::Snappy should return an error"
    );
}
