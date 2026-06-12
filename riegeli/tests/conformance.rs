//! Conformance tests: Rust-written files are readable by C++ and vice versa.

// Some imports are used only by feature-gated tests; in reduced-feature
// builds they would otherwise trip unused_imports.
#![cfg_attr(
    not(all(feature = "brotli", feature = "zstd", feature = "snappy")),
    allow(unused_imports)
)]
use riegeli::{ReaderOptions, RecordReader, RecordWriter, WriterOptions};
use std::io::Cursor;

/// Number of records in each golden file.
const NUM_RECORDS: usize = 100;

/// Generate the expected record content for index `i`.
fn expected_record(i: usize) -> String {
    format!("record_{:06}", i)
}

/// Read all records from a golden file and verify they match expectations.
fn verify_golden_file(data: &[u8], label: &str) {
    let cursor = Cursor::new(data);
    let mut reader = RecordReader::new(cursor, ReaderOptions::new())
        .unwrap_or_else(|e| panic!("{label}: failed to open: {e:?}"));

    let mut count = 0;
    while let Some(record) = reader
        .read_record()
        .unwrap_or_else(|e| panic!("{label}: read_record failed at index {count}: {e:?}"))
    {
        let expected = expected_record(count);
        assert_eq!(
            record,
            expected.as_bytes(),
            "{label}: record {count} mismatch: expected {:?}, got {:?}",
            expected,
            String::from_utf8_lossy(&record)
        );
        count += 1;
    }

    assert_eq!(
        count, NUM_RECORDS,
        "{label}: expected {NUM_RECORDS} records, got {count}"
    );
}

// ---------------------------------------------------------------------------
// simple_none.riegeli decoded correctly
// ---------------------------------------------------------------------------
#[test]
fn simple_none() {
    let data = include_bytes!("../testdata/golden/simple_none.riegeli");
    verify_golden_file(data, "simple_none");
}

// ---------------------------------------------------------------------------
// simple_brotli.riegeli decoded correctly
// ---------------------------------------------------------------------------
#[test]
#[cfg(feature = "brotli")]
fn simple_brotli() {
    let data = include_bytes!("../testdata/golden/simple_brotli.riegeli");
    verify_golden_file(data, "simple_brotli");
}

// ---------------------------------------------------------------------------
// simple_zstd.riegeli decoded correctly
// ---------------------------------------------------------------------------
#[test]
#[cfg(feature = "zstd")]
fn simple_zstd() {
    let data = include_bytes!("../testdata/golden/simple_zstd.riegeli");
    verify_golden_file(data, "simple_zstd");
}

// ---------------------------------------------------------------------------
// transpose_none.riegeli decoded correctly
// ---------------------------------------------------------------------------
#[test]
fn transpose_none() {
    let data = include_bytes!("../testdata/golden/transpose_none.riegeli");
    verify_golden_file(data, "transpose_none");
}

// ---------------------------------------------------------------------------
// transpose_brotli.riegeli decoded correctly
// ---------------------------------------------------------------------------
#[test]
#[cfg(feature = "brotli")]
fn transpose_brotli() {
    let data = include_bytes!("../testdata/golden/transpose_brotli.riegeli");
    verify_golden_file(data, "transpose_brotli");
}

// ---------------------------------------------------------------------------
// transpose_zstd.riegeli decoded correctly
// ---------------------------------------------------------------------------
#[test]
#[cfg(feature = "zstd")]
fn transpose_zstd() {
    let data = include_bytes!("../testdata/golden/transpose_zstd.riegeli");
    verify_golden_file(data, "transpose_zstd");
}

// ---------------------------------------------------------------------------
// Rust-written simple+Brotli file verified by C++ verifier
// ---------------------------------------------------------------------------
#[test]
#[cfg(feature = "brotli")]
fn rust_simple_brotli_verified_by_cpp() {
    let verifier_path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .join("cpp_verifier_bin");

    // Skip if the C++ verifier binary is not available
    if !verifier_path.exists() {
        // Try the bazel-built binary location
        let bazel_path =
            std::path::Path::new("/home/mike.danese/code/riegeli/bazel-bin/tools/verify_rust_file");
        if !bazel_path.exists() {
            eprintln!(
                "Skipping: C++ verifier not found at {:?} or {:?}",
                verifier_path, bazel_path
            );
            return;
        }
        run_cpp_verification(bazel_path, false);
        return;
    }
    run_cpp_verification(&verifier_path, false);
}

// ---------------------------------------------------------------------------
// Rust-written transpose+Brotli file verified by C++ verifier
// ---------------------------------------------------------------------------
#[test]
#[cfg(feature = "brotli")]
fn rust_transpose_brotli_verified_by_cpp() {
    let bazel_path =
        std::path::Path::new("/home/mike.danese/code/riegeli/bazel-bin/tools/verify_rust_file");
    if !bazel_path.exists() {
        eprintln!("Skipping: C++ verifier not found at {:?}", bazel_path);
        return;
    }
    run_cpp_verification(bazel_path, true);
}

/// Write a Rust riegeli file with 100 records, then invoke the C++ verifier.
#[cfg(feature = "brotli")]
fn run_cpp_verification(verifier: &std::path::Path, transpose: bool) {
    use riegeli::CompressionType;

    // Write the file to a temp location
    let tmp_dir = std::env::temp_dir();
    let suffix = if transpose {
        "transpose_brotli"
    } else {
        "simple_brotli"
    };
    let tmp_file = tmp_dir.join(format!("riegeli_rs_test_{}.riegeli", suffix));

    {
        let file = std::fs::File::create(&tmp_file).expect("create temp file");
        let opts = WriterOptions::new()
            .compression(CompressionType::Brotli)
            .transpose(transpose);
        let mut writer = RecordWriter::new(file, opts).expect("create writer");
        for i in 0..NUM_RECORDS {
            let record = expected_record(i);
            writer
                .write_record(record.as_bytes())
                .expect("write record");
        }
        writer.close().expect("close writer");
    }

    // Invoke the C++ verifier
    let output = std::process::Command::new(verifier)
        .arg(format!("--file={}", tmp_file.display()))
        .arg(format!("--num_records={}", NUM_RECORDS))
        .output()
        .expect("failed to execute C++ verifier");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(
        output.status.success(),
        "C++ verifier failed for {suffix}:\nstdout: {stdout}\nstderr: {stderr}"
    );

    assert!(
        stdout.contains("OK"),
        "C++ verifier did not print OK for {suffix}:\nstdout: {stdout}\nstderr: {stderr}"
    );

    // Clean up
    let _ = std::fs::remove_file(&tmp_file);
}

// ---------------------------------------------------------------------------
// Additional test: Verify C++ golden file with RecordReader + seek
// ---------------------------------------------------------------------------
#[test]
fn read_cpp_golden_file() {
    // Use the simple_none golden file (no compression needed)
    let data = include_bytes!("../testdata/golden/simple_none.riegeli");
    let cursor = Cursor::new(data.as_slice());
    let mut reader = RecordReader::new(cursor, ReaderOptions::new()).expect("open");

    // Read all records
    let mut records = Vec::new();
    while let Some(rec) = reader.read_record().expect("read") {
        records.push(rec);
    }
    assert_eq!(records.len(), NUM_RECORDS);

    // Verify each record
    for (i, rec) in records.iter().enumerate() {
        assert_eq!(rec.as_slice(), expected_record(i).as_bytes());
    }
}

// ---------------------------------------------------------------------------
// Block-boundary reference files
//
// Written by the reference C++ implementation and validated by C++ read-back
// (sha256 manifest in testdata/boundary/). They
// pin the chunk-position convention at block boundaries: q = chunk_begin +
// num_records landing at boundary+1 (A), boundary+24 — the excluded alias
// offset (B), and exactly on the boundary (C); D is a boundary-coincident
// chunk (a padded file concatenated with a complete second file, whose
// signature chunk is addressed AT 65536 with header bytes at 65560).
//
// Note: the *_with_tail files are the only behavioral discriminators against
// a reader using the old (boundary+24) convention — the tail chunk sits at
// boundary+25, so the old reader fails with a header-hash mismatch at 65560.
// The tail-less A/B/C files read identically under either convention (the
// stray byte past the last chunk is absorbed as EOF), and D's physical layout
// is convention-neutral. Do not drop the tails.
// ---------------------------------------------------------------------------

/// Read a boundary reference file end-to-end: all records must be empty (or
/// the known tail record), the count exact, EOF clean, and the distinct
/// chunk addresses must match the expected canonical positions.
#[cfg(feature = "zstd")]
fn verify_boundary_reference(
    data: &[u8],
    label: &str,
    expected_records: usize,
    expected_chunk_begins: &[u64],
) {
    let mut reader = RecordReader::new(Cursor::new(data.to_vec()), ReaderOptions::new())
        .unwrap_or_else(|e| panic!("{label}: failed to open: {e:?}"));
    let mut count = 0usize;
    let mut begins: Vec<u64> = Vec::new();
    while reader
        .read_record()
        .unwrap_or_else(|e| panic!("{label}: read_record failed at index {count}: {e:?}"))
        .is_some()
    {
        count += 1;
        let b = reader.last_pos().chunk_begin;
        if begins.last() != Some(&b) {
            begins.push(b);
        }
    }
    assert_eq!(count, expected_records, "{label}: record count");
    assert_eq!(begins, expected_chunk_begins, "{label}: chunk addresses");
}

#[test]
#[cfg(feature = "zstd")]
fn boundary_reference_a_q_boundary_plus_1() {
    let data = include_bytes!("../testdata/boundary/A_q_boundary_plus_1.riegeli");
    verify_boundary_reference(data, "A", 65_473, &[64]);
}

#[test]
#[cfg(feature = "zstd")]
fn boundary_reference_a_with_tail() {
    let data = include_bytes!("../testdata/boundary/A_q_boundary_plus_1_with_tail.riegeli");
    // The tail chunk must land at exactly 65561 (= 65536 + 25).
    verify_boundary_reference(data, "A_with_tail", 65_474, &[64, 65_561]);
}

#[test]
#[cfg(feature = "zstd")]
fn boundary_reference_b_q_boundary_plus_24() {
    let data = include_bytes!("../testdata/boundary/B_q_boundary_plus_24.riegeli");
    verify_boundary_reference(data, "B", 65_496, &[64]);
}

#[test]
#[cfg(feature = "zstd")]
fn boundary_reference_b_with_tail() {
    let data = include_bytes!("../testdata/boundary/B_q_boundary_plus_24_with_tail.riegeli");
    verify_boundary_reference(data, "B_with_tail", 65_497, &[64, 65_561]);
}

#[test]
#[cfg(feature = "zstd")]
fn boundary_reference_c_q_boundary_exact() {
    let data = include_bytes!("../testdata/boundary/C_q_boundary_exact.riegeli");
    verify_boundary_reference(data, "C", 65_472, &[64]);
}

#[test]
#[cfg(feature = "zstd")]
fn boundary_reference_d_chunk_starts_at_boundary() {
    let data = include_bytes!("../testdata/boundary/D_chunk_starts_at_boundary.riegeli");
    // Second concatenated file: signature chunk addressed AT the boundary
    // (65536), record chunk at 65600.
    verify_boundary_reference(data, "D", 2, &[64, 65_600]);
}

/// The Rust writer must reproduce the A/B/C reference files byte-for-byte
/// from their recipes (empty records, zstd level 3 — the default level).
///
/// Byte-identity includes the zstd-compressed bytes and holds for libzstd
/// 1.5.x (Cargo.lock pins zstd-sys 2.0.16+zstd.1.5.7, matching the C++
/// writer's libzstd). If a future zstd bump breaks only this test, downgrade
/// the assertion to chunk positions, file lengths, and block-header fields —
/// those are the actual format spec; the compressed bytes are not.
#[test]
#[cfg(feature = "zstd")]
fn boundary_reference_writer_reproduction() {
    use riegeli::CompressionType;
    for (name, n, reference) in [
        (
            "A",
            65_473usize,
            &include_bytes!("../testdata/boundary/A_q_boundary_plus_1.riegeli")[..],
        ),
        (
            "B",
            65_496,
            &include_bytes!("../testdata/boundary/B_q_boundary_plus_24.riegeli")[..],
        ),
        (
            "C",
            65_472,
            &include_bytes!("../testdata/boundary/C_q_boundary_exact.riegeli")[..],
        ),
    ] {
        let mut buf = Cursor::new(Vec::<u8>::new());
        {
            let mut w = RecordWriter::new(
                &mut buf,
                WriterOptions::new().compression(CompressionType::Zstd),
            )
            .expect("writer new ok");
            for _ in 0..n {
                w.write_record(b"").expect("write ok");
            }
            w.flush().expect("flush ok");
        }
        let mine = buf.into_inner();
        assert_eq!(
            mine.len(),
            reference.len(),
            "{name}: file length (positions desync)"
        );
        assert_eq!(mine, reference, "{name}: byte-for-byte reproduction");
    }
}
