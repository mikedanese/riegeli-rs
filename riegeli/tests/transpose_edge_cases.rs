//! Transpose encoding edge cases.

mod common;

use common::{cpp_read, cpp_write, cross_lang_roundtrip, rust_read, rust_write};
use riegeli::{CompressionType, WriterOptions as RustWriterOptions};
use riegeli_ffi::{Compression, WriterOptions as FfiWriterOptions};

// ---------------------------------------------------------------------------
// Adversarial: all-empty records through transpose
// ---------------------------------------------------------------------------

#[test]
#[cfg(feature = "brotli")]
fn adversarial_transpose_all_empty_records_rust_to_cpp() {
    let records: Vec<Vec<u8>> = (0..50).map(|_| vec![]).collect();
    cross_lang_roundtrip(
        "adversarial: all-empty records transpose+brotli rust->cpp",
        &records,
        |recs| {
            rust_write(
                recs,
                RustWriterOptions::new()
                    .compression(CompressionType::Brotli)
                    .transpose(true),
            )
        },
        cpp_read,
    );
}

#[test]
#[cfg(feature = "brotli")]
fn adversarial_transpose_all_empty_records_cpp_to_rust() {
    let records: Vec<Vec<u8>> = (0..50).map(|_| vec![]).collect();
    cross_lang_roundtrip(
        "adversarial: all-empty records transpose+brotli cpp->rust",
        &records,
        |recs| {
            cpp_write(
                recs,
                FfiWriterOptions::new()
                    .compression(Compression::Brotli(6))
                    .transpose(true),
            )
        },
        rust_read,
    );
}

// ---------------------------------------------------------------------------
// Adversarial: all-identical records (maximally compressible)
// ---------------------------------------------------------------------------

#[test]
#[cfg(feature = "zstd")]
fn adversarial_transpose_identical_records_rust_to_cpp() {
    let record = vec![0xDEu8; 256];
    let records: Vec<Vec<u8>> = (0..200).map(|_| record.clone()).collect();
    cross_lang_roundtrip(
        "adversarial: identical records transpose+zstd rust->cpp",
        &records,
        |recs| {
            rust_write(
                recs,
                RustWriterOptions::new()
                    .compression(CompressionType::Zstd)
                    .transpose(true),
            )
        },
        cpp_read,
    );
}

#[test]
#[cfg(feature = "zstd")]
fn adversarial_transpose_identical_records_cpp_to_rust() {
    let record = vec![0xDEu8; 256];
    let records: Vec<Vec<u8>> = (0..200).map(|_| record.clone()).collect();
    cross_lang_roundtrip(
        "adversarial: identical records transpose+zstd cpp->rust",
        &records,
        |recs| {
            cpp_write(
                recs,
                FfiWriterOptions::new()
                    .compression(Compression::Zstd(3))
                    .transpose(true),
            )
        },
        rust_read,
    );
}

// ---------------------------------------------------------------------------
// Adversarial: high-entropy pseudo-random data (hard to compress)
// ---------------------------------------------------------------------------

#[test]
#[cfg(feature = "brotli")]
fn adversarial_transpose_high_entropy_rust_to_cpp() {
    // Use a simple LCG to generate pseudo-random bytes
    let mut seed: u64 = 0xDEADBEEFCAFE;
    let records: Vec<Vec<u8>> = (0..100)
        .map(|_| {
            (0..512)
                .map(|_| {
                    seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
                    (seed >> 33) as u8
                })
                .collect()
        })
        .collect();
    cross_lang_roundtrip(
        "adversarial: high-entropy transpose+brotli rust->cpp",
        &records,
        |recs| {
            rust_write(
                recs,
                RustWriterOptions::new()
                    .compression(CompressionType::Brotli)
                    .transpose(true),
            )
        },
        cpp_read,
    );
}

#[test]
#[cfg(feature = "brotli")]
fn adversarial_transpose_high_entropy_cpp_to_rust() {
    let mut seed: u64 = 0xDEADBEEFCAFE;
    let records: Vec<Vec<u8>> = (0..100)
        .map(|_| {
            (0..512)
                .map(|_| {
                    seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
                    (seed >> 33) as u8
                })
                .collect()
        })
        .collect();
    cross_lang_roundtrip(
        "adversarial: high-entropy transpose+brotli cpp->rust",
        &records,
        |recs| {
            cpp_write(
                recs,
                FfiWriterOptions::new()
                    .compression(Compression::Brotli(6))
                    .transpose(true),
            )
        },
        rust_read,
    );
}

// ---------------------------------------------------------------------------
// Adversarial: proto-like byte patterns that aren't valid protobuf
// ---------------------------------------------------------------------------

#[test]
fn adversarial_transpose_proto_like_bytes_rust_to_cpp() {
    // Records that start with valid-looking protobuf field tags but have
    // inconsistent wire types / lengths, stressing the transpose encoder's
    // opaque-blob path.
    let records: Vec<Vec<u8>> = (0..50)
        .map(|i| {
            let tag = ((i as u8 + 1) << 3) | 2; // length-delimited wire type
            let mut rec = vec![tag, 0xFF]; // impossible length
            rec.extend(vec![0xAA; 20]);
            rec
        })
        .collect();
    cross_lang_roundtrip(
        "adversarial: proto-like bytes transpose+none rust->cpp",
        &records,
        |recs| {
            rust_write(
                recs,
                RustWriterOptions::new()
                    .compression(CompressionType::None)
                    .transpose(true),
            )
        },
        cpp_read,
    );
}

#[test]
fn adversarial_transpose_proto_like_bytes_cpp_to_rust() {
    let records: Vec<Vec<u8>> = (0..50)
        .map(|i| {
            let tag = ((i as u8 + 1) << 3) | 2;
            let mut rec = vec![tag, 0xFF];
            rec.extend(vec![0xAA; 20]);
            rec
        })
        .collect();
    cross_lang_roundtrip(
        "adversarial: proto-like bytes transpose+none cpp->rust",
        &records,
        |recs| {
            cpp_write(
                recs,
                FfiWriterOptions::new()
                    .compression(Compression::None)
                    .transpose(true),
            )
        },
        rust_read,
    );
}

// ---------------------------------------------------------------------------
// Adversarial: very large single record through transpose
// ---------------------------------------------------------------------------

#[test]
#[cfg(feature = "zstd")]
fn adversarial_transpose_single_large_record_rust_to_cpp() {
    // 64 KiB record through transpose+zstd
    let records = vec![vec![0x42u8; 65536]];
    cross_lang_roundtrip(
        "adversarial: single 64KiB record transpose+zstd rust->cpp",
        &records,
        |recs| {
            rust_write(
                recs,
                RustWriterOptions::new()
                    .compression(CompressionType::Zstd)
                    .transpose(true),
            )
        },
        cpp_read,
    );
}

#[test]
#[cfg(feature = "zstd")]
fn adversarial_transpose_single_large_record_cpp_to_rust() {
    let records = vec![vec![0x42u8; 65536]];
    cross_lang_roundtrip(
        "adversarial: single 64KiB record transpose+zstd cpp->rust",
        &records,
        |recs| {
            cpp_write(
                recs,
                FfiWriterOptions::new()
                    .compression(Compression::Zstd(3))
                    .transpose(true),
            )
        },
        rust_read,
    );
}

// ---------------------------------------------------------------------------
// Adversarial: tiny chunk_size forcing ~1 record per chunk
// ---------------------------------------------------------------------------

#[test]
fn adversarial_transpose_one_record_per_chunk_rust_to_cpp() {
    // chunk_size=64 with records of ~32 bytes => roughly 1 record per chunk
    let records: Vec<Vec<u8>> = (0..100)
        .map(|i| format!("rec-{i:04}-padding-data-here").into_bytes())
        .collect();
    cross_lang_roundtrip(
        "adversarial: ~1 rec/chunk transpose+none rust->cpp",
        &records,
        |recs| {
            rust_write(
                recs,
                RustWriterOptions::new()
                    .compression(CompressionType::None)
                    .transpose(true)
                    .chunk_size(64),
            )
        },
        cpp_read,
    );
}

#[test]
fn adversarial_transpose_one_record_per_chunk_cpp_to_rust() {
    let records: Vec<Vec<u8>> = (0..100)
        .map(|i| format!("rec-{i:04}-padding-data-here").into_bytes())
        .collect();
    cross_lang_roundtrip(
        "adversarial: ~1 rec/chunk transpose+none cpp->rust",
        &records,
        |recs| {
            cpp_write(
                recs,
                FfiWriterOptions::new()
                    .compression(Compression::None)
                    .transpose(true)
                    .chunk_size(64),
            )
        },
        rust_read,
    );
}
