//! Integration tests for corruption recovery in RecordReader.

use std::io::Cursor;

use riegeli::{
    CompressionType, ReaderOptions, RecordReader, RecordWriter, WriterOptions as RustWriterOptions,
};
use riegeli_ffi::{
    Compression, RecordReader as FfiReader, RecordWriter as FfiWriter,
    WriterOptions as FfiWriterOptions,
};

// ---------------------------------------------------------------------------
// Helpers (duplicated to keep this file self-contained)
// ---------------------------------------------------------------------------

fn rust_write(records: &[Vec<u8>], opts: RustWriterOptions) -> Vec<u8> {
    let mut buf = Cursor::new(Vec::<u8>::new());
    {
        let mut w = RecordWriter::new(&mut buf, opts).expect("rust writer ok");
        for rec in records {
            w.write_record(rec).expect("rust write_record ok");
        }
        w.flush().expect("rust flush ok");
    }
    buf.into_inner()
}

fn cpp_write(records: &[Vec<u8>], opts: FfiWriterOptions) -> Vec<u8> {
    let mut w = FfiWriter::new(opts).expect("cpp writer ok");
    for rec in records {
        w.write_record(rec).expect("cpp write_record ok");
    }
    w.close().expect("cpp writer close ok")
}

fn rust_read(data: &[u8]) -> Vec<Vec<u8>> {
    let mut reader =
        RecordReader::new(Cursor::new(data), ReaderOptions::new()).expect("rust reader ok");
    let mut out = Vec::new();
    while let Some(rec) = reader.read_record().expect("rust read_record ok") {
        out.push(rec);
    }
    out
}

fn cpp_read(data: &[u8]) -> Vec<Vec<u8>> {
    let mut reader = FfiReader::new(data).expect("cpp reader ok");
    let mut out = Vec::new();
    while let Some(rec) = reader.read_record().expect("cpp read_record ok") {
        out.push(rec);
    }
    reader.close().expect("cpp reader close ok");
    out
}

fn cross_lang_roundtrip<W, R>(label: &str, records: &[Vec<u8>], write_fn: W, read_fn: R)
where
    W: FnOnce(&[Vec<u8>]) -> Vec<u8>,
    R: FnOnce(&[u8]) -> Vec<Vec<u8>>,
{
    let file_bytes = write_fn(records);
    assert!(
        !file_bytes.is_empty(),
        "{label}: writer produced empty output"
    );
    let got = read_fn(&file_bytes);
    assert_eq!(
        got.len(),
        records.len(),
        "{label}: expected {} records, got {}",
        records.len(),
        got.len()
    );
    for (idx, (expected, actual)) in records.iter().zip(got.iter()).enumerate() {
        assert_eq!(actual, expected, "{label}: record {idx} mismatch");
    }
}

// ---------------------------------------------------------------------------
// Adversarial test 1: tiny chunk_size forces many chunks (both directions)
//
// This exercises the ChunkEnd padding formula across many chunk boundaries.
// chunk_size(128) with 200 small records will produce ~200 separate chunks.
// ---------------------------------------------------------------------------

#[test]
#[cfg(feature = "brotli")]
fn adv_rust_write_tiny_chunks_brotli_cpp_read() {
    // 200 records × ~13 bytes each with chunk_size=128 → many separate chunks
    let records: Vec<Vec<u8>> = (0..200)
        .map(|i| format!("rec-{i:04}").into_bytes())
        .collect();
    cross_lang_roundtrip(
        "adv tiny-chunk-size/brotli/rust→cpp",
        &records,
        |recs| {
            rust_write(
                recs,
                RustWriterOptions::new()
                    .compression(CompressionType::Brotli)
                    .chunk_size(128),
            )
        },
        cpp_read,
    );
}

#[test]
#[cfg(feature = "brotli")]
fn adv_cpp_write_tiny_chunks_brotli_rust_read() {
    let records: Vec<Vec<u8>> = (0..200)
        .map(|i| format!("rec-{i:04}").into_bytes())
        .collect();
    cross_lang_roundtrip(
        "adv tiny-chunk-size/brotli/cpp→rust",
        &records,
        |recs| {
            cpp_write(
                recs,
                FfiWriterOptions::new()
                    .compression(Compression::Brotli(6))
                    .chunk_size(128),
            )
        },
        rust_read,
    );
}

// ---------------------------------------------------------------------------
// Adversarial test 2: exactly 560 records with brotli (padding boundary)
//
// The original bug (missing num_records padding) manifested at ~560 records
// when the compressed data was smaller than num_records bytes. This test pins
// that boundary: if the padding bug regresses, C++ will refuse the file.
// ---------------------------------------------------------------------------

#[test]
#[cfg(feature = "brotli")]
fn adv_rust_write_560_records_brotli_cpp_read() {
    // Use repetitive content (compresses well) to keep data_size < num_records
    let records: Vec<Vec<u8>> = (0..560).map(|_| vec![0xABu8; 20]).collect();
    cross_lang_roundtrip(
        "adv 560-records/brotli/rust→cpp (padding boundary)",
        &records,
        |recs| {
            rust_write(
                recs,
                RustWriterOptions::new().compression(CompressionType::Brotli),
            )
        },
        cpp_read,
    );
}

#[test]
#[cfg(feature = "brotli")]
fn adv_rust_write_1000_repetitive_brotli_cpp_read() {
    // 1000 records of all-zeros (extreme compressibility: data_size << num_records)
    let records: Vec<Vec<u8>> = (0..1000).map(|_| vec![0u8; 100]).collect();
    cross_lang_roundtrip(
        "adv 1000-repetitive/brotli/rust→cpp (data_size<<num_records)",
        &records,
        |recs| {
            rust_write(
                recs,
                RustWriterOptions::new().compression(CompressionType::Brotli),
            )
        },
        cpp_read,
    );
}

// ---------------------------------------------------------------------------
// Adversarial test 3: records spanning block boundaries
//
// A record large enough that its data crosses a 65536-byte block boundary
// exercises both the writer's block-header insertion and the reader's skip logic.
// ---------------------------------------------------------------------------

#[test]
fn adv_rust_write_large_record_spans_block_cpp_read() {
    // One record of 70000 bytes — guaranteed to cross the 65536-byte block boundary
    let records: Vec<Vec<u8>> = vec![vec![0x42u8; 70_000]];
    cross_lang_roundtrip(
        "adv 70KiB-record/none/rust→cpp (spans block boundary)",
        &records,
        |recs| {
            rust_write(
                recs,
                RustWriterOptions::new().compression(CompressionType::None),
            )
        },
        cpp_read,
    );
}

#[test]
fn adv_cpp_write_large_record_spans_block_rust_read() {
    let records: Vec<Vec<u8>> = vec![vec![0x42u8; 70_000]];
    cross_lang_roundtrip(
        "adv 70KiB-record/none/cpp→rust (spans block boundary)",
        &records,
        |recs| cpp_write(recs, FfiWriterOptions::new().compression(Compression::None)),
        rust_read,
    );
}

// ---------------------------------------------------------------------------
// Adversarial test 4: Rust file read twice (data not consumed on first pass)
//
// Verifies the reader does not advance its internal state destructively
// when we construct a second reader from the same bytes.
// ---------------------------------------------------------------------------

#[test]
#[cfg(feature = "brotli")]
fn adv_rust_write_read_twice_by_cpp() {
    let records: Vec<Vec<u8>> = (0..50)
        .map(|i| format!("record-{i}").into_bytes())
        .collect();
    let file_bytes = rust_write(
        &records,
        RustWriterOptions::new().compression(CompressionType::Brotli),
    );

    // First read
    let got1 = cpp_read(&file_bytes);
    assert_eq!(got1.len(), records.len(), "first read: record count");

    // Second independent read from same bytes — must produce identical results
    let got2 = cpp_read(&file_bytes);
    assert_eq!(got2.len(), records.len(), "second read: record count");
    assert_eq!(got1, got2, "both reads must produce identical records");
}

// ---------------------------------------------------------------------------
// Adversarial test 5: mixed record sizes in a single file
//
// Exercises the chunk accumulator: tiny (1 byte) and large (8 KiB) records
// interleaved, so some chunks will be dominated by a single large record and
// others will be packed with many tiny ones.
// ---------------------------------------------------------------------------

#[test]
#[cfg(feature = "zstd")]
fn adv_rust_write_mixed_sizes_zstd_cpp_read() {
    let mut records: Vec<Vec<u8>> = Vec::new();
    for i in 0..100 {
        if i % 10 == 0 {
            records.push(vec![i as u8; 8192]); // large
        } else {
            records.push(vec![i as u8; 1]); // tiny
        }
    }
    cross_lang_roundtrip(
        "adv mixed-sizes/zstd/rust→cpp",
        &records,
        |recs| {
            rust_write(
                recs,
                RustWriterOptions::new().compression(CompressionType::Zstd),
            )
        },
        cpp_read,
    );
}

#[test]
#[cfg(feature = "zstd")]
fn adv_cpp_write_mixed_sizes_zstd_rust_read() {
    let mut records: Vec<Vec<u8>> = Vec::new();
    for i in 0..100 {
        if i % 10 == 0 {
            records.push(vec![i as u8; 8192]);
        } else {
            records.push(vec![i as u8; 1]);
        }
    }
    cross_lang_roundtrip(
        "adv mixed-sizes/zstd/cpp→rust",
        &records,
        |recs| {
            cpp_write(
                recs,
                FfiWriterOptions::new().compression(Compression::Zstd(3)),
            )
        },
        rust_read,
    );
}

// ---------------------------------------------------------------------------
// Adversarial test 6: all-byte-value records (full 0x00–0xFF coverage)
//
// Each record contains all 256 byte values. This catches any inadvertent
// text-mode treatment or byte-value-dependent bug in the FFI bridge.
// ---------------------------------------------------------------------------

#[test]
#[cfg(feature = "brotli")]
fn adv_all_byte_values_per_record_rust_write_cpp_read() {
    // 10 records, each containing all 256 byte values
    let base: Vec<u8> = (0u8..=255u8).collect();
    let records: Vec<Vec<u8>> = (0..10).map(|_| base.clone()).collect();
    cross_lang_roundtrip(
        "adv all-byte-values/brotli/rust→cpp",
        &records,
        |recs| {
            rust_write(
                recs,
                RustWriterOptions::new().compression(CompressionType::Brotli),
            )
        },
        cpp_read,
    );
}

#[test]
#[cfg(feature = "brotli")]
fn adv_all_byte_values_per_record_cpp_write_rust_read() {
    let base: Vec<u8> = (0u8..=255u8).collect();
    let records: Vec<Vec<u8>> = (0..10).map(|_| base.clone()).collect();
    cross_lang_roundtrip(
        "adv all-byte-values/brotli/cpp→rust",
        &records,
        |recs| {
            cpp_write(
                recs,
                FfiWriterOptions::new().compression(Compression::Brotli(6)),
            )
        },
        rust_read,
    );
}

/// Regression: a file of exactly one block plus a block header (so EOF sits
/// on the boundary+24 alias address) once produced overlapping recovery
/// regions — the second region's canonicalized begin stepped 24 bytes back
/// inside the first. Regions reported during forward reads must be nonempty
/// and non-overlapping.
#[test]
fn recovery_regions_stay_monotone_on_alias_eof_file() {
    use std::cell::RefCell;
    use std::rc::Rc;

    let bytes = std::fs::read(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/testdata/regression/recovery_regions_alias.riegeli"
    ))
    .expect("regression input");

    let regions: Rc<RefCell<Vec<(u64, u64)>>> = Rc::default();
    let sink = regions.clone();
    let opts = riegeli::ReaderOptions::new().recovery(move |reg| {
        sink.borrow_mut().push((reg.begin(), reg.end()));
        true
    });
    let Ok(mut reader) = riegeli::RecordReader::new(std::io::Cursor::new(bytes), opts) else {
        return; // construction may fail without a callback verdict; fine
    };
    while let Ok(Some(_)) = reader.read_record() {}

    let regs = regions.borrow();
    let mut last_end = 0u64;
    for &(b, e) in regs.iter() {
        assert!(b < e, "empty region in {regs:?}");
        assert!(b >= last_end, "regions moved backward: {regs:?}");
        last_end = e;
    }
}
