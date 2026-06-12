//! Integration tests for RecordPosition, seek, and error recovery.

// Some imports are used only by feature-gated tests; in reduced-feature
// builds they would otherwise trip unused_imports.
#![cfg_attr(
    not(all(feature = "brotli", feature = "zstd", feature = "snappy")),
    allow(unused_imports)
)]
use std::io::{Cursor, Write};

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

/// Regression (found in review): seek_numeric must not shift a
/// boundary-coincident chunk to its boundary+24 alias address — that made
/// numeric containment and record_index off by 24.
#[test]
#[cfg(feature = "zstd")]
fn seek_numeric_into_boundary_coincident_chunk() {
    // First chunk: 131,008 empty zstd records -> chunk_end = round_up(64 + 131008)
    // = 131072 exactly (delta = 0). Second chunk: 30 distinct records in ONE
    // chunk, boundary-coincident at canonical address 131072.
    let n = 131_008usize;
    let boundary: u64 = 131_072;
    let mut buf = Cursor::new(Vec::<u8>::new());
    {
        let mut w = RecordWriter::new(
            &mut buf,
            WriterOptions::new().compression(CompressionType::Zstd),
        )
        .expect("new ok");
        for _ in 0..n {
            w.write_record(b"").expect("write ok");
        }
        w.flush().expect("flush ok");
        for i in 0..30u32 {
            w.write_record(format!("r{i}").as_bytes())
                .expect("write ok");
        }
        w.flush().expect("flush ok");
    }
    let data = buf.into_inner();

    // Sanity: read everything; find the second chunk's canonical address.
    let mut reader =
        RecordReader::new(Cursor::new(data.clone()), ReaderOptions::new()).expect("new ok");
    let mut after_begin = None;
    let mut after_count = 0;
    while let Some(rec) = reader.read_record().expect("read ok") {
        if !rec.is_empty() {
            after_count += 1;
            after_begin.get_or_insert(reader.last_pos().chunk_begin);
        }
    }
    assert_eq!(after_count, 30);
    assert_eq!(
        after_begin.unwrap(),
        boundary,
        "second chunk should be boundary-coincident at the canonical address"
    );

    // seek_numeric to record 5 of the boundary-coincident chunk:
    // numeric = canonical chunk_begin + record_index = 131072 + 5.
    let mut reader = RecordReader::new(Cursor::new(data), ReaderOptions::new()).expect("new ok");
    reader.seek_numeric(boundary + 5).expect("seek_numeric ok");
    let rec = reader
        .read_record()
        .expect("read after seek_numeric ok")
        .expect("record expected");
    assert_eq!(
        rec, b"r5",
        "seek_numeric(boundary+5) must land on record 5 of the boundary-coincident chunk"
    );
}
