//! Head-to-head Rust-vs-C++ performance benchmark.
//!
//! Measures write throughput, read throughput, and compression ratio for both
//! the pure-Rust `riegeli` crate and the C++ reference implementation (via
//! `riegeli-ffi`) across 6 configurations and 2 payload sizes (12 cells total).
//!
//! # Running
//!
//! ```text
//! cargo bench --bench head_to_head --offline
//! ```
//!
//! The benchmark prints a human-readable summary table to stdout comparing
//! Rust and C++ side by side for every cell.
//!
//! # Configuration matrix
//!
//! | Config             | Compression | Transpose |
//! |--------------------|-------------|-----------|
//! | simple+none        | None        | No        |
//! | simple+brotli:6    | Brotli(6)   | No        |
//! | simple+zstd:3      | Zstd(3)     | No        |
//! | transpose+none     | None        | Yes       |
//! | transpose+brotli:6 | Brotli(6)   | Yes       |
//! | transpose+zstd:3   | Zstd(3)     | Yes       |
//!
//! # Payload sizes
//!
//! - **small**: 100-byte records (JSON-like text with binary padding)
//! - **large**: 10,240-byte (10 KiB) records
//!
//! Each cell uses 10,000 records to amortize setup overhead and produce stable
//! throughput measurements. Each measurement is repeated 3 times and the median
//! is reported.

use std::io::Cursor;
use std::time::{Duration, Instant};

use riegeli::{
    CompressionType, ReaderOptions, RecordReader, RecordWriter, WriterOptions as RustWriterOptions,
};

use riegeli_ffi::{
    Compression as FfiCompression, RecordReader as FfiReader, RecordWriter as FfiWriter,
    WriterOptions as FfiWriterOptions,
};

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Number of records per benchmark cell.
const NUM_RECORDS: usize = 10_000;

/// Small record size in bytes.
const SMALL_RECORD_SIZE: usize = 100;

/// Large record size in bytes (10 KiB).
const LARGE_RECORD_SIZE: usize = 10_240;

/// Number of repetitions per measurement (we take the median).
const REPS: usize = 5;

// ---------------------------------------------------------------------------
// Payload generation
// ---------------------------------------------------------------------------

/// Generate `n` records of `size` bytes with realistic, partially compressible
/// content. Each record starts with a JSON-like text prefix and is padded with
/// a repeating binary pattern derived from its index.
fn make_payload(n: usize, size: usize) -> Vec<Vec<u8>> {
    (0..n)
        .map(|i| {
            let mut rec = Vec::with_capacity(size);

            // JSON-like prefix for realism
            let prefix = format!(
                "{{\"id\":{},\"seq\":{},\"tag\":\"bench-record\",\"data\":\"",
                i,
                i * 7 + 13
            );
            rec.extend_from_slice(prefix.as_bytes());

            // Binary pattern fill for partial compressibility
            let pattern: [u8; 16] = [
                (i & 0xff) as u8,
                ((i >> 8) & 0xff) as u8,
                0xAA,
                0x55,
                (i & 0xff) as u8,
                0xBB,
                0x33,
                ((i >> 4) & 0xff) as u8,
                0xDE,
                0xAD,
                (i & 0x3f) as u8,
                0xBE,
                0xEF,
                ((i >> 2) & 0xff) as u8,
                0xCA,
                0xFE,
            ];

            while rec.len() < size.saturating_sub(2) {
                let remaining = size.saturating_sub(2) - rec.len();
                let chunk = remaining.min(pattern.len());
                rec.extend_from_slice(&pattern[..chunk]);
            }

            // Close the JSON-like structure
            rec.extend_from_slice(b"\"}");
            rec.truncate(size);
            while rec.len() < size {
                rec.push(0x20); // space padding
            }

            rec
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct BenchConfig {
    name: &'static str,
    rust_opts: RustWriterOptions,
    ffi_opts_fn: fn() -> FfiWriterOptions,
    compressed: bool,
    transpose: bool,
}

fn make_configs() -> Vec<BenchConfig> {
    vec![
        BenchConfig {
            name: "simple+none",
            rust_opts: RustWriterOptions::new().compression(CompressionType::None),
            ffi_opts_fn: || FfiWriterOptions::new().compression(FfiCompression::None),
            compressed: false,
            transpose: false,
        },
        BenchConfig {
            name: "simple+brotli:6",
            rust_opts: RustWriterOptions::new()
                .compression(CompressionType::Brotli)
                .compression_level(6),
            ffi_opts_fn: || FfiWriterOptions::new().compression(FfiCompression::Brotli(6)),
            compressed: true,
            transpose: false,
        },
        BenchConfig {
            name: "simple+zstd:3",
            rust_opts: RustWriterOptions::new()
                .compression(CompressionType::Zstd)
                .compression_level(3),
            ffi_opts_fn: || FfiWriterOptions::new().compression(FfiCompression::Zstd(3)),
            compressed: true,
            transpose: false,
        },
        BenchConfig {
            name: "transpose+none",
            rust_opts: RustWriterOptions::new()
                .transpose(true)
                .compression(CompressionType::None),
            ffi_opts_fn: || {
                FfiWriterOptions::new()
                    .transpose(true)
                    .compression(FfiCompression::None)
            },
            compressed: false,
            transpose: true,
        },
        BenchConfig {
            name: "transpose+brotli:6",
            rust_opts: RustWriterOptions::new()
                .transpose(true)
                .compression(CompressionType::Brotli)
                .compression_level(6),
            ffi_opts_fn: || {
                FfiWriterOptions::new()
                    .transpose(true)
                    .compression(FfiCompression::Brotli(6))
            },
            compressed: true,
            transpose: true,
        },
        BenchConfig {
            name: "transpose+zstd:3",
            rust_opts: RustWriterOptions::new()
                .transpose(true)
                .compression(CompressionType::Zstd)
                .compression_level(3),
            ffi_opts_fn: || {
                FfiWriterOptions::new()
                    .transpose(true)
                    .compression(FfiCompression::Zstd(3))
            },
            compressed: true,
            transpose: true,
        },
    ]
}

// ---------------------------------------------------------------------------
// Write helpers
// ---------------------------------------------------------------------------

/// Write records with the Rust writer, returning (file_bytes, duration).
fn rust_write(records: &[Vec<u8>], opts: RustWriterOptions) -> (Vec<u8>, Duration) {
    let start = Instant::now();
    let buf: Vec<u8> = Vec::with_capacity(records.len() * records[0].len() * 2);
    let mut cursor = Cursor::new(buf);
    let mut writer = RecordWriter::new(&mut cursor, opts).expect("rust writer::new");
    for rec in records {
        writer.write_record(rec).expect("rust write_record");
    }
    writer.close().expect("rust close");
    let elapsed = start.elapsed();
    (cursor.into_inner(), elapsed)
}

/// Write records with the C++ writer, returning (file_bytes, duration).
fn cpp_write(records: &[Vec<u8>], opts_fn: fn() -> FfiWriterOptions) -> (Vec<u8>, Duration) {
    let start = Instant::now();
    let opts = opts_fn();
    let mut writer = FfiWriter::new(opts).expect("cpp writer::new");
    for rec in records {
        writer.write_record(rec).expect("cpp write_record");
    }
    let data = writer.close().expect("cpp close");
    let elapsed = start.elapsed();
    (data, elapsed)
}

// ---------------------------------------------------------------------------
// Read helpers
// ---------------------------------------------------------------------------

/// Read all records from file bytes using the Rust reader, returning (count, duration).
fn rust_read(data: &[u8]) -> (usize, Duration) {
    let start = Instant::now();
    let cursor = Cursor::new(data);
    let mut reader = RecordReader::new(cursor, ReaderOptions::new()).expect("rust reader::new");
    let mut count = 0;
    while let Some(_rec) = reader.read_record().expect("rust read_record") {
        count += 1;
    }
    let elapsed = start.elapsed();
    (count, elapsed)
}

/// Read all records from file bytes using the C++ reader, returning (count, duration).
fn cpp_read(data: &[u8]) -> (usize, Duration) {
    let start = Instant::now();
    let mut reader = FfiReader::new(data).expect("cpp reader::new");
    let mut count = 0;
    while reader.read_next().expect("cpp read_next") {
        count += 1;
    }
    reader.close().expect("cpp reader close");
    let elapsed = start.elapsed();
    (count, elapsed)
}

// ---------------------------------------------------------------------------
// Measurement utilities
// ---------------------------------------------------------------------------

/// Compute throughput in MB/s given raw data size and duration.
fn throughput_mbps(raw_bytes: usize, duration: Duration) -> f64 {
    let secs = duration.as_secs_f64();
    if secs == 0.0 {
        return f64::INFINITY;
    }
    (raw_bytes as f64) / (1_000_000.0) / secs
}

/// Take the median of a slice of durations.
fn median_duration(durations: &mut [Duration]) -> Duration {
    durations.sort();
    durations[durations.len() / 2]
}

/// Compression ratio = compressed_size / uncompressed_size.
fn compression_ratio(compressed_size: usize, uncompressed_size: usize) -> f64 {
    compressed_size as f64 / uncompressed_size as f64
}

// ---------------------------------------------------------------------------
// Per-cell result
// ---------------------------------------------------------------------------

struct CellResult {
    config_name: String,
    payload_label: String,
    raw_bytes: usize,
    compressed: bool,
    transpose: bool,
    rust_write_mbps: f64,
    cpp_write_mbps: f64,
    rust_read_mbps: f64,
    cpp_read_mbps: f64,
    rust_compression_ratio: f64,
    cpp_compression_ratio: f64,
}

// ---------------------------------------------------------------------------
// Main benchmark driver
// ---------------------------------------------------------------------------

fn run_cell(config: &BenchConfig, payload_label: &str, records: &[Vec<u8>]) -> CellResult {
    let raw_bytes = records.iter().map(|r| r.len()).sum::<usize>();

    // --- Warmup run to settle CPU frequency scaling ---
    let _ = rust_write(records, config.rust_opts.clone());
    let _ = cpp_write(records, config.ffi_opts_fn);

    // --- Write measurements ---
    let mut rust_write_durations = Vec::with_capacity(REPS);
    let mut cpp_write_durations = Vec::with_capacity(REPS);
    let mut rust_file = Vec::new();
    let mut cpp_file = Vec::new();

    for rep in 0..REPS {
        let (rf, rd) = rust_write(records, config.rust_opts.clone());
        rust_write_durations.push(rd);
        if rep == 0 {
            rust_file = rf;
        }

        let (cf, cd) = cpp_write(records, config.ffi_opts_fn);
        cpp_write_durations.push(cd);
        if rep == 0 {
            cpp_file = cf;
        }
    }

    let rust_write_dur = median_duration(&mut rust_write_durations);
    let cpp_write_dur = median_duration(&mut cpp_write_durations);

    // --- Read measurements ---
    // Rust reads Rust-written file; C++ reads C++-written file
    let mut rust_read_durations = Vec::with_capacity(REPS);
    let mut cpp_read_durations = Vec::with_capacity(REPS);

    for _ in 0..REPS {
        let (count, dur) = rust_read(&rust_file);
        assert_eq!(count, records.len(), "Rust read record count mismatch");
        rust_read_durations.push(dur);

        let (count, dur) = cpp_read(&cpp_file);
        assert_eq!(count, records.len(), "C++ read record count mismatch");
        cpp_read_durations.push(dur);
    }

    let rust_read_dur = median_duration(&mut rust_read_durations);
    let cpp_read_dur = median_duration(&mut cpp_read_durations);

    CellResult {
        config_name: config.name.to_string(),
        payload_label: payload_label.to_string(),
        raw_bytes,
        compressed: config.compressed,
        transpose: config.transpose,
        rust_write_mbps: throughput_mbps(raw_bytes, rust_write_dur),
        cpp_write_mbps: throughput_mbps(raw_bytes, cpp_write_dur),
        rust_read_mbps: throughput_mbps(raw_bytes, rust_read_dur),
        cpp_read_mbps: throughput_mbps(raw_bytes, cpp_read_dur),
        rust_compression_ratio: compression_ratio(rust_file.len(), raw_bytes),
        cpp_compression_ratio: compression_ratio(cpp_file.len(), raw_bytes),
    }
}

fn print_summary(results: &[CellResult]) {
    println!();
    println!(
        "============================================================================================================================================"
    );
    println!("  Rust-vs-C++ Head-to-Head Performance Benchmark");
    println!(
        "  {} records per cell, median of {} runs",
        NUM_RECORDS, REPS
    );
    println!(
        "============================================================================================================================================"
    );
    println!();
    println!(
        "{:<22} {:>7} {:>12} {:>12} {:>12} {:>12} {:>12} {:>12}",
        "Config",
        "Payload",
        "Rust W MB/s",
        "C++ W MB/s",
        "Rust R MB/s",
        "C++ R MB/s",
        "Rust Ratio",
        "C++ Ratio"
    );
    println!(
        "{:<22} {:>7} {:>12} {:>12} {:>12} {:>12} {:>12} {:>12}",
        "------",
        "-------",
        "-----------",
        "----------",
        "-----------",
        "----------",
        "----------",
        "---------"
    );

    for r in results {
        println!(
            "{:<22} {:>7} {:>12.1} {:>12.1} {:>12.1} {:>12.1} {:>12.3} {:>12.3}",
            r.config_name,
            r.payload_label,
            r.rust_write_mbps,
            r.cpp_write_mbps,
            r.rust_read_mbps,
            r.cpp_read_mbps,
            r.rust_compression_ratio,
            r.cpp_compression_ratio,
        );
    }

    println!();
    println!(
        "Compression ratio = file_size / raw_data_size (lower is better; 1.0 = no compression)"
    );
    println!("Throughput in MB/s (higher is better; MB = 1,000,000 bytes)");
    println!();

    // Check compression ratio agreement
    let mut ratio_issues = Vec::new();
    for r in results {
        if r.rust_compression_ratio > 0.0 && r.cpp_compression_ratio > 0.0 {
            let diff = (r.rust_compression_ratio - r.cpp_compression_ratio).abs()
                / r.cpp_compression_ratio;
            if diff > 0.20 {
                ratio_issues.push(format!(
                    "  WARNING: {} {} -- Rust ratio {:.3} vs C++ ratio {:.3} (diff {:.1}%)",
                    r.config_name,
                    r.payload_label,
                    r.rust_compression_ratio,
                    r.cpp_compression_ratio,
                    diff * 100.0,
                ));
            }
        }
    }
    if ratio_issues.is_empty() {
        println!("All compression ratios within 20% between Rust and C++. OK");
    } else {
        println!("Compression ratio discrepancies:");
        for issue in &ratio_issues {
            println!("{}", issue);
        }
    }
    println!();
}

fn main() {
    // Generate payloads
    eprintln!("Generating payloads...");
    let small_records = make_payload(NUM_RECORDS, SMALL_RECORD_SIZE);
    let large_records = make_payload(NUM_RECORDS, LARGE_RECORD_SIZE);

    let configs = make_configs();
    let mut results = Vec::with_capacity(configs.len() * 2);

    for config in &configs {
        eprint!("  {} / small ... ", config.name);
        let r = run_cell(config, "small", &small_records);
        eprintln!(
            "Rust W:{:.1} R:{:.1}  C++ W:{:.1} R:{:.1} MB/s",
            r.rust_write_mbps, r.rust_read_mbps, r.cpp_write_mbps, r.cpp_read_mbps
        );
        results.push(r);

        eprint!("  {} / large ... ", config.name);
        let r = run_cell(config, "large", &large_records);
        eprintln!(
            "Rust W:{:.1} R:{:.1}  C++ W:{:.1} R:{:.1} MB/s",
            r.rust_write_mbps, r.rust_read_mbps, r.cpp_write_mbps, r.cpp_read_mbps
        );
        results.push(r);
    }

    print_summary(&results);

    // Plausibility checks
    let mut plausibility_ok = true;
    for r in &results {
        // Transpose encoding has high per-record overhead (state machine
        // construction, column splitting) especially for small records, so we
        // use a lower write threshold. Even C++ transpose+none with small
        // records is much slower than simple+none.
        let min_write = if r.compressed || r.transpose {
            10.0
        } else {
            100.0
        };
        let min_read = 10.0; // all modes should read at least 10 MB/s
        for (label, val, min) in [
            ("Rust write", r.rust_write_mbps, min_write),
            ("C++ write", r.cpp_write_mbps, min_write),
            ("Rust read", r.rust_read_mbps, min_read),
            ("C++ read", r.cpp_read_mbps, min_read),
        ] {
            if val < min {
                eprintln!(
                    "PLAUSIBILITY WARNING: {} {} {} = {:.1} MB/s (expected >= {:.0})",
                    r.config_name, r.payload_label, label, val, min
                );
                plausibility_ok = false;
            }
        }

        // Verify raw_bytes is correct
        let expected_raw = if r.payload_label == "small" {
            NUM_RECORDS * SMALL_RECORD_SIZE
        } else {
            NUM_RECORDS * LARGE_RECORD_SIZE
        };
        assert_eq!(
            r.raw_bytes, expected_raw,
            "raw_bytes mismatch for {}",
            r.config_name
        );
    }

    if plausibility_ok {
        eprintln!("All throughput values are plausible. OK");
    }

    // Verify we ran all 12 cells
    assert_eq!(
        results.len(),
        12,
        "Expected 12 cells (6 configs x 2 payloads)"
    );
    eprintln!("Benchmark complete: {} cells measured.", results.len());
}
