//! Cross-language roundtrip tests: files written by Rust are readable by C++ and vice versa.

// Some imports are used only by feature-gated tests; in reduced-feature
// builds they would otherwise trip unused_imports.
#![cfg_attr(
    not(all(feature = "brotli", feature = "zstd", feature = "snappy")),
    allow(unused_imports)
)]
mod common;

use common::{
    cpp_read, cpp_write, cross_lang_roundtrip, make_large_records, make_small_records, rust_read,
    rust_write,
};
use riegeli::{CompressionType, WriterOptions as RustWriterOptions};
use riegeli_ffi::{Compression, WriterOptions as FfiWriterOptions};

// ---------------------------------------------------------------------------
// 20.1 – Rust-write simple+none (100 records) read by C++
// ---------------------------------------------------------------------------

#[test]
fn criterion_20_1_rust_write_none_100_cpp_read() {
    let records = make_small_records(100);
    cross_lang_roundtrip(
        "20.1 rust-write/none/cpp-read",
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

// ---------------------------------------------------------------------------
// 20.2 – C++-write simple+none (100 records) read by Rust
// ---------------------------------------------------------------------------

#[test]
fn criterion_20_2_cpp_write_none_100_rust_read() {
    let records = make_small_records(100);
    cross_lang_roundtrip(
        "20.2 cpp-write/none/rust-read",
        &records,
        |recs| cpp_write(recs, FfiWriterOptions::new().compression(Compression::None)),
        rust_read,
    );
}

// ---------------------------------------------------------------------------
// 20.3 – Rust-write simple+brotli:6 (10,000 × 1 KiB) read by C++
// ---------------------------------------------------------------------------

#[test]
#[cfg(feature = "brotli")]
fn criterion_20_3_rust_write_brotli6_10k_cpp_read() {
    let records = make_large_records(10_000, 1024);
    cross_lang_roundtrip(
        "20.3 rust-write/brotli:6/cpp-read 10k×1KiB",
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
// 20.4 – C++-write simple+brotli:6 (10,000 × 1 KiB) read by Rust
// ---------------------------------------------------------------------------

#[test]
#[cfg(feature = "brotli")]
fn criterion_20_4_cpp_write_brotli6_10k_rust_read() {
    let records = make_large_records(10_000, 1024);
    cross_lang_roundtrip(
        "20.4 cpp-write/brotli:6/rust-read 10k×1KiB",
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

// ---------------------------------------------------------------------------
// 20.5 – Rust+C++ simple+zstd:3 round-trips both directions
// ---------------------------------------------------------------------------

#[test]
#[cfg(feature = "zstd")]
fn criterion_20_5a_rust_write_zstd3_cpp_read() {
    let records = make_small_records(100);
    cross_lang_roundtrip(
        "20.5a rust-write/zstd:3/cpp-read",
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
fn criterion_20_5b_cpp_write_zstd3_rust_read() {
    let records = make_small_records(100);
    cross_lang_roundtrip(
        "20.5b cpp-write/zstd:3/rust-read",
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
// 20.6 – Rust+C++ simple+snappy round-trips both directions
// ---------------------------------------------------------------------------

#[test]
#[cfg(feature = "snappy")]
fn criterion_20_6a_rust_write_snappy_cpp_read() {
    let records = make_small_records(100);
    cross_lang_roundtrip(
        "20.6a rust-write/snappy/cpp-read",
        &records,
        |recs| {
            rust_write(
                recs,
                RustWriterOptions::new().compression(CompressionType::Snappy),
            )
        },
        cpp_read,
    );
}

#[test]
#[cfg(feature = "snappy")]
fn criterion_20_6b_cpp_write_snappy_rust_read() {
    let records = make_small_records(100);
    cross_lang_roundtrip(
        "20.6b cpp-write/snappy/rust-read",
        &records,
        |recs| {
            cpp_write(
                recs,
                FfiWriterOptions::new().compression(Compression::Snappy(1)),
            )
        },
        rust_read,
    );
}

// ---------------------------------------------------------------------------
// 20.7 – Edge cases: single-record and empty files (simple+brotli)
// ---------------------------------------------------------------------------

#[test]
#[cfg(feature = "brotli")]
fn criterion_20_7a_rust_write_brotli_single_record_cpp_read() {
    let records = vec![b"the only record".to_vec()];
    cross_lang_roundtrip(
        "20.7a rust-write/brotli/single-record/cpp-read",
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
fn criterion_20_7b_cpp_write_brotli_single_record_rust_read() {
    let records = vec![b"the only record".to_vec()];
    cross_lang_roundtrip(
        "20.7b cpp-write/brotli/single-record/rust-read",
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

// Not gated on the brotli feature: zero records are written, so no brotli
// code runs and the empty-file edge stays covered in reduced-feature builds.
#[test]
fn criterion_20_7c_rust_write_brotli_empty_file_cpp_read() {
    let records: Vec<Vec<u8>> = vec![];
    cross_lang_roundtrip(
        "20.7c rust-write/brotli/empty/cpp-read",
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

// Not gated on the brotli feature: zero records are written, so no brotli
// code runs and the empty-file edge stays covered in reduced-feature builds.
#[test]
fn criterion_20_7d_cpp_write_brotli_empty_file_rust_read() {
    let records: Vec<Vec<u8>> = vec![];
    cross_lang_roundtrip(
        "20.7d cpp-write/brotli/empty/rust-read",
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

// ---------------------------------------------------------------------------
// Additional coverage: larger record sets for each compression (both directions)
// ---------------------------------------------------------------------------

#[test]
fn extra_rust_write_none_1000_cpp_read() {
    let records = make_small_records(1000);
    cross_lang_roundtrip(
        "extra rust-write/none/1000/cpp-read",
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
fn extra_cpp_write_none_1000_rust_read() {
    let records = make_small_records(1000);
    cross_lang_roundtrip(
        "extra cpp-write/none/1000/rust-read",
        &records,
        |recs| cpp_write(recs, FfiWriterOptions::new().compression(Compression::None)),
        rust_read,
    );
}

#[test]
fn extra_rust_write_none_binary_records_cpp_read() {
    // Binary records (not printable ASCII) to verify byte-exact fidelity.
    let records: Vec<Vec<u8>> = (0u8..=255u8).map(|b| vec![b; 16]).collect();
    cross_lang_roundtrip(
        "extra rust-write/none/binary/cpp-read",
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
fn extra_cpp_write_none_binary_records_rust_read() {
    let records: Vec<Vec<u8>> = (0u8..=255u8).map(|b| vec![b; 16]).collect();
    cross_lang_roundtrip(
        "extra cpp-write/none/binary/rust-read",
        &records,
        |recs| cpp_write(recs, FfiWriterOptions::new().compression(Compression::None)),
        rust_read,
    );
}

#[test]
#[cfg(feature = "brotli")]
fn extra_rust_write_brotli_zero_len_records_cpp_read() {
    // Zero-length records (empty payload) round-trip correctly.
    let records: Vec<Vec<u8>> = (0..10).map(|_| vec![]).collect();
    cross_lang_roundtrip(
        "extra rust-write/brotli/zero-len/cpp-read",
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
fn extra_cpp_write_brotli_zero_len_records_rust_read() {
    let records: Vec<Vec<u8>> = (0..10).map(|_| vec![]).collect();
    cross_lang_roundtrip(
        "extra cpp-write/brotli/zero-len/rust-read",
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

// ---------------------------------------------------------------------------
// Chunk headers straddling a 64 KiB block boundary, cross-language.
//
// The writer interleaves a 24-byte block header inside the 40-byte chunk
// header when a chunk begins within 40 bytes of a block boundary. Sweep
// record sizes with one-record chunks so some chunk header in the file lands
// in that window, and require that the sweep really exercised it.
// ---------------------------------------------------------------------------

/// True if any chunk in `data` has a header beginning within 40 bytes below
/// a 64 KiB multiple (i.e. the header is interrupted by a block header).
///
/// Detected by direct inspection of the raw block-header bytes — deliberately
/// NOT via either reader, whose position bookkeeping is part of what these
/// tests exercise. The 24-byte block header at each 64 KiB boundary stores at
/// bytes [8..16] the distance back to the start of the chunk in progress
/// there; a 40-byte chunk header straddles the boundary iff that distance is
/// in (0, 40).
fn has_straddling_chunk_header(data: &[u8]) -> bool {
    const BLOCK_SIZE: usize = 65536;
    const BLOCK_HEADER_SIZE: usize = 24;
    const CHUNK_HEADER_SIZE: u64 = 40;
    let mut boundary = BLOCK_SIZE;
    let mut straddles = false;
    while boundary + BLOCK_HEADER_SIZE <= data.len() {
        let prev = u64::from_le_bytes(data[boundary + 8..boundary + 16].try_into().unwrap());
        if 0 < prev && prev < CHUNK_HEADER_SIZE {
            straddles = true;
        }
        boundary += BLOCK_SIZE;
    }
    straddles
}

#[test]
fn straddling_chunk_header_rust_write_cpp_read() {
    let mut straddle_layouts = 0;
    for rec_size in 16300..16340usize {
        let records: Vec<Vec<u8>> = (0..6).map(|i| vec![(i % 251) as u8; rec_size]).collect();
        let data = rust_write(
            &records,
            RustWriterOptions::new()
                .compression(CompressionType::None)
                .chunk_size(rec_size as u64),
        );
        if has_straddling_chunk_header(&data) {
            straddle_layouts += 1;
        }
        let got = cpp_read(&data);
        assert_eq!(got.len(), records.len(), "rec_size={rec_size}: count");
        for (i, (g, w)) in got.iter().zip(&records).enumerate() {
            assert_eq!(g, w, "rec_size={rec_size}: record {i}");
        }
    }
    assert!(
        straddle_layouts > 0,
        "sweep never produced a straddling chunk header; widen the range"
    );
}

#[test]
fn straddling_chunk_header_cpp_write_rust_read() {
    let mut straddle_layouts = 0;
    for rec_size in 16300..16340usize {
        let records: Vec<Vec<u8>> = (0..6).map(|i| vec![(i % 251) as u8; rec_size]).collect();
        let data = cpp_write(
            &records,
            FfiWriterOptions::new()
                .compression(Compression::None)
                .chunk_size(rec_size as u64),
        );
        if has_straddling_chunk_header(&data) {
            straddle_layouts += 1;
        }
        let got = rust_read(&data);
        assert_eq!(got.len(), records.len(), "rec_size={rec_size}: count");
        for (i, (g, w)) in got.iter().zip(&records).enumerate() {
            assert_eq!(g, w, "rec_size={rec_size}: record {i}");
        }
    }
    assert!(
        straddle_layouts > 0,
        "sweep never produced a straddling chunk header; widen the range"
    );
}
