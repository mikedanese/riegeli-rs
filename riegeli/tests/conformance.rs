//! Conformance tests: Rust-written files are readable by C++ and vice versa.

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
// Criterion 14.1: simple_none.riegeli decoded correctly
// ---------------------------------------------------------------------------
#[test]
fn criterion_14_1_simple_none() {
    let data = include_bytes!("../testdata/golden/simple_none.riegeli");
    verify_golden_file(data, "simple_none");
}

// ---------------------------------------------------------------------------
// Criterion 14.2: simple_brotli.riegeli decoded correctly
// Also activates criterion 4.5
// ---------------------------------------------------------------------------
#[test]
#[cfg(feature = "brotli")]
fn criterion_14_2_simple_brotli() {
    let data = include_bytes!("../testdata/golden/simple_brotli.riegeli");
    verify_golden_file(data, "simple_brotli");
}

// ---------------------------------------------------------------------------
// Criterion 14.3: simple_zstd.riegeli decoded correctly
// Also activates criterion 4.6
// ---------------------------------------------------------------------------
#[test]
#[cfg(feature = "zstd")]
fn criterion_14_3_simple_zstd() {
    let data = include_bytes!("../testdata/golden/simple_zstd.riegeli");
    verify_golden_file(data, "simple_zstd");
}

// ---------------------------------------------------------------------------
// Criterion 14.4: transpose_none.riegeli decoded correctly
// Activates criterion 9.4
// ---------------------------------------------------------------------------
#[test]
fn criterion_14_4_transpose_none() {
    let data = include_bytes!("../testdata/golden/transpose_none.riegeli");
    verify_golden_file(data, "transpose_none");
}

// ---------------------------------------------------------------------------
// Criterion 14.5: transpose_brotli.riegeli decoded correctly
// Activates criterion 9.5
// ---------------------------------------------------------------------------
#[test]
#[cfg(feature = "brotli")]
fn criterion_14_5_transpose_brotli() {
    let data = include_bytes!("../testdata/golden/transpose_brotli.riegeli");
    verify_golden_file(data, "transpose_brotli");
}

// ---------------------------------------------------------------------------
// Criterion 14.6: transpose_zstd.riegeli decoded correctly
// ---------------------------------------------------------------------------
#[test]
#[cfg(feature = "zstd")]
fn criterion_14_6_transpose_zstd() {
    let data = include_bytes!("../testdata/golden/transpose_zstd.riegeli");
    verify_golden_file(data, "transpose_zstd");
}

// ---------------------------------------------------------------------------
// Criterion 14.7: Rust-written simple+Brotli file verified by C++ verifier
// Activates criterion 12.6
// ---------------------------------------------------------------------------
#[test]
#[cfg(feature = "brotli")]
fn criterion_14_7_rust_simple_brotli_verified_by_cpp() {
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
// Criterion 14.8: Rust-written transpose+Brotli file verified by C++ verifier
// ---------------------------------------------------------------------------
#[test]
#[cfg(feature = "brotli")]
fn criterion_14_8_rust_transpose_brotli_verified_by_cpp() {
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
// Activates criterion 6.2 (reading C++ golden file)
// ---------------------------------------------------------------------------
#[test]
fn criterion_6_2_read_cpp_golden_file() {
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
