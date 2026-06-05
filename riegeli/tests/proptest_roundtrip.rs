//! Property-based roundtrip tests: arbitrary records survive write+read unchanged.
// Some imports are used only by feature-gated tests; in reduced-feature
// builds they would otherwise trip unused_imports.
#![cfg_attr(
    not(all(feature = "brotli", feature = "zstd", feature = "snappy")),
    allow(unused_imports)
)]
use std::io::Cursor;

use proptest::prelude::*;
use riegeli::{CompressionType, ReaderOptions, RecordReader, RecordWriter, WriterOptions};

/// Strategy: Vec of up to 100 records, each up to 65536 bytes.
fn records_strategy() -> impl Strategy<Value = Vec<Vec<u8>>> {
    prop::collection::vec(prop::collection::vec(any::<u8>(), 0..=65536), 0..=100)
}

fn roundtrip_with_options(records: &[Vec<u8>], opts: WriterOptions) {
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

    let mut got: Vec<Vec<u8>> = Vec::new();
    while let Some(rec) = reader.read_record().expect("read_record") {
        got.push(rec);
    }

    assert_eq!(
        got.len(),
        records.len(),
        "record count mismatch: wrote {} read {}",
        records.len(),
        got.len()
    );
    for (i, (expected, actual)) in records.iter().zip(got.iter()).enumerate() {
        assert_eq!(
            expected.as_slice(),
            actual.as_slice(),
            "record {i} mismatch: expected {} bytes, got {} bytes",
            expected.len(),
            actual.len()
        );
    }
}

proptest! {
    // Criterion 7.1: 1000 iterations for uncompressed mode.
    #![proptest_config(ProptestConfig { cases: 1000, ..ProptestConfig::default() })]

    #[test]
    #[ignore]
    fn proptest_roundtrip_uncompressed(records in records_strategy()) {
        roundtrip_with_options(&records, WriterOptions::new());
    }
}

proptest! {
    // Criterion 7.1: 1000 iterations for Brotli compression.
    #![proptest_config(ProptestConfig { cases: 1000, ..ProptestConfig::default() })]

    #[test]
    #[ignore]
    #[cfg(feature = "brotli")]
    fn proptest_roundtrip_brotli(records in records_strategy()) {
        roundtrip_with_options(
            &records,
            WriterOptions::new().compression(CompressionType::Brotli),
        );
    }
}
