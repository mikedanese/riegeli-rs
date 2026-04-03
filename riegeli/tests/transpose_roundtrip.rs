//! Integration tests for transpose encoding write+read roundtrips.

mod common;

use common::{
    cpp_read, cpp_write, cross_lang_roundtrip, make_large_records, make_small_records, rust_read,
    rust_write,
};
use riegeli::{CompressionType, WriterOptions as RustWriterOptions};
use riegeli_ffi::{Compression, WriterOptions as FfiWriterOptions};

// ---------------------------------------------------------------------------
// Record generation helpers specific to transpose tests
// ---------------------------------------------------------------------------

/// Generate `n` non-proto opaque binary records with deterministic content.
///
/// Each record has a distinct byte pattern that is not valid protobuf, ensuring
/// the transpose encoder treats them as opaque blobs. Record `i` contains
/// bytes derived from a simple hash of `i` to provide variety in field
/// structure visible to the transpose encoder.
fn make_non_proto_records(n: usize) -> Vec<Vec<u8>> {
    (0..n)
        .map(|i| {
            // Each record: 4-byte little-endian index + 28 bytes of patterned data.
            // This is deliberately NOT valid protobuf (no field tags/wire types).
            let mut rec = Vec::with_capacity(32);
            rec.extend_from_slice(&(i as u32).to_le_bytes());
            for j in 0..28u8 {
                rec.push(((i as u8).wrapping_mul(7).wrapping_add(j)) ^ 0xAB);
            }
            rec
        })
        .collect()
}

/// Generate mixed-size records cycling through: empty, 1-byte, 100-byte,
/// 1 KiB, and 10 KiB. Produces `n` records total.
fn make_mixed_size_records(n: usize) -> Vec<Vec<u8>> {
    let sizes = [0usize, 1, 100, 1024, 10240];
    (0..n)
        .map(|i| {
            let size = sizes[i % sizes.len()];
            if size == 0 {
                vec![]
            } else {
                // Fill with a deterministic pattern based on record index
                (0..size).map(|j| ((i + j) % 256) as u8).collect()
            }
        })
        .collect()
}

// ---------------------------------------------------------------------------
// 21.1 – transpose+none (100 non-proto records), both directions
// ---------------------------------------------------------------------------

#[test]
fn criterion_21_1a_rust_write_transpose_none_100_cpp_read() {
    let records = make_non_proto_records(100);
    cross_lang_roundtrip(
        "21.1a rust-write/transpose+none/cpp-read",
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
fn criterion_21_1b_cpp_write_transpose_none_100_rust_read() {
    let records = make_non_proto_records(100);
    cross_lang_roundtrip(
        "21.1b cpp-write/transpose+none/rust-read",
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
// 21.2 – transpose+brotli:6 (100 non-proto records), both directions
// ---------------------------------------------------------------------------

#[test]
fn criterion_21_2a_rust_write_transpose_brotli6_100_cpp_read() {
    let records = make_non_proto_records(100);
    cross_lang_roundtrip(
        "21.2a rust-write/transpose+brotli:6/cpp-read",
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
fn criterion_21_2b_cpp_write_transpose_brotli6_100_rust_read() {
    let records = make_non_proto_records(100);
    cross_lang_roundtrip(
        "21.2b cpp-write/transpose+brotli:6/rust-read",
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
// 21.3 – transpose+zstd:3 (100 non-proto records), both directions
// ---------------------------------------------------------------------------

#[test]
fn criterion_21_3a_rust_write_transpose_zstd3_100_cpp_read() {
    let records = make_non_proto_records(100);
    cross_lang_roundtrip(
        "21.3a rust-write/transpose+zstd:3/cpp-read",
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
fn criterion_21_3b_cpp_write_transpose_zstd3_100_rust_read() {
    let records = make_non_proto_records(100);
    cross_lang_roundtrip(
        "21.3b cpp-write/transpose+zstd:3/rust-read",
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
// 21.4 – 10,000 × 1 KiB records, transpose+brotli:6, both directions
// ---------------------------------------------------------------------------

#[test]
fn criterion_21_4a_rust_write_transpose_brotli6_10k_cpp_read() {
    let records = make_large_records(10_000, 1024);
    cross_lang_roundtrip(
        "21.4a rust-write/transpose+brotli:6/10k×1KiB/cpp-read",
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
fn criterion_21_4b_cpp_write_transpose_brotli6_10k_rust_read() {
    let records = make_large_records(10_000, 1024);
    cross_lang_roundtrip(
        "21.4b cpp-write/transpose+brotli:6/10k×1KiB/rust-read",
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
// 21.5 – mixed-size records, transpose+zstd:3, both directions
// ---------------------------------------------------------------------------

#[test]
fn criterion_21_5a_rust_write_transpose_zstd3_mixed_cpp_read() {
    // 100 records cycling through empty, 1-byte, 100-byte, 1 KiB, 10 KiB
    let records = make_mixed_size_records(100);
    cross_lang_roundtrip(
        "21.5a rust-write/transpose+zstd:3/mixed-sizes/cpp-read",
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
fn criterion_21_5b_cpp_write_transpose_zstd3_mixed_rust_read() {
    let records = make_mixed_size_records(100);
    cross_lang_roundtrip(
        "21.5b cpp-write/transpose+zstd:3/mixed-sizes/rust-read",
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
// 21.6 – single-record and two-record files, transpose+none, both directions
// ---------------------------------------------------------------------------

#[test]
fn criterion_21_6a_rust_write_transpose_none_single_record_cpp_read() {
    let records = vec![b"single transpose record".to_vec()];
    cross_lang_roundtrip(
        "21.6a rust-write/transpose+none/single/cpp-read",
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
fn criterion_21_6b_cpp_write_transpose_none_single_record_rust_read() {
    let records = vec![b"single transpose record".to_vec()];
    cross_lang_roundtrip(
        "21.6b cpp-write/transpose+none/single/rust-read",
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

#[test]
fn criterion_21_6c_rust_write_transpose_none_two_records_cpp_read() {
    let records = vec![b"first record".to_vec(), b"second record".to_vec()];
    cross_lang_roundtrip(
        "21.6c rust-write/transpose+none/two-records/cpp-read",
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
fn criterion_21_6d_cpp_write_transpose_none_two_records_rust_read() {
    let records = vec![b"first record".to_vec(), b"second record".to_vec()];
    cross_lang_roundtrip(
        "21.6d cpp-write/transpose+none/two-records/rust-read",
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
// 21.7 – transpose+brotli, chunk_size=4096, 500 records, both directions
// ---------------------------------------------------------------------------

#[test]
fn criterion_21_7a_rust_write_transpose_brotli_small_chunks_cpp_read() {
    let records = make_small_records(500);
    cross_lang_roundtrip(
        "21.7a rust-write/transpose+brotli/chunk_size=4096/500/cpp-read",
        &records,
        |recs| {
            rust_write(
                recs,
                RustWriterOptions::new()
                    .compression(CompressionType::Brotli)
                    .transpose(true)
                    .chunk_size(4096),
            )
        },
        cpp_read,
    );
}

#[test]
fn criterion_21_7b_cpp_write_transpose_brotli_small_chunks_rust_read() {
    let records = make_small_records(500);
    cross_lang_roundtrip(
        "21.7b cpp-write/transpose+brotli/chunk_size=4096/500/rust-read",
        &records,
        |recs| {
            cpp_write(
                recs,
                FfiWriterOptions::new()
                    .compression(Compression::Brotli(6))
                    .transpose(true)
                    .chunk_size(4096),
            )
        },
        rust_read,
    );
}
