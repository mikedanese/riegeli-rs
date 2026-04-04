//! Criterion benchmarks for transpose projection optimizations (Phase 6).
//!
//! Measures throughput improvement from projection-during-decode vs.
//! the pre-phase-6 baseline of full decode + post-decode field filtering.
//!
//! Run:
//! ```text
//! cargo bench -p riegeli --bench transpose_projection_bench --features "brotli,zstd,snappy"
//! ```

use std::io::Cursor;

use criterion::{Criterion, SamplingMode, Throughput, criterion_group, criterion_main};

use riegeli::{
    CompressionType, Field, FieldProjection, ReaderOptions, RecordReader, RecordWriter,
    WriterOptions,
};

// ---------------------------------------------------------------------------
// Proto encoding helpers
// ---------------------------------------------------------------------------

fn encode_varint(v: u64) -> Vec<u8> {
    let mut out = Vec::new();
    let mut val = v;
    while val >= 0x80 {
        out.push((val as u8) | 0x80);
        val >>= 7;
    }
    out.push(val as u8);
    out
}

fn encode_tag(field_number: u32, wire_type: u32) -> Vec<u8> {
    encode_varint(((field_number as u64) << 3) | wire_type as u64)
}

fn encode_varint_field(field_number: u32, value: u64) -> Vec<u8> {
    let mut out = encode_tag(field_number, 0);
    out.extend(encode_varint(value));
    out
}

fn encode_fixed32_field(field_number: u32, value: u32) -> Vec<u8> {
    let mut out = encode_tag(field_number, 5);
    out.extend(&value.to_le_bytes());
    out
}

fn encode_fixed64_field(field_number: u32, value: u64) -> Vec<u8> {
    let mut out = encode_tag(field_number, 1);
    out.extend(&value.to_le_bytes());
    out
}

fn encode_bytes_field(field_number: u32, data: &[u8]) -> Vec<u8> {
    let mut out = encode_tag(field_number, 2);
    out.extend(encode_varint(data.len() as u64));
    out.extend(data);
    out
}

fn encode_submessage_field(field_number: u32, inner: &[u8]) -> Vec<u8> {
    encode_bytes_field(field_number, inner)
}

// ---------------------------------------------------------------------------
// Decode helpers (for varint parsing in the baseline filter)
// ---------------------------------------------------------------------------

fn decode_varint(buf: &[u8]) -> Option<(u64, usize)> {
    let mut result = 0u64;
    let mut shift = 0u32;
    for (i, &byte) in buf.iter().enumerate() {
        if shift >= 64 {
            return None;
        }
        result |= ((byte & 0x7f) as u64) << shift;
        shift += 7;
        if byte & 0x80 == 0 {
            return Some((result, i + 1));
        }
    }
    None
}

/// Post-decode field filter, simulating the pre-phase-6 FieldProjection::apply().
/// Keeps only the field with the given field_number from a proto record.
fn filter_record_keep_field(record: &[u8], keep_field: u32) -> Vec<u8> {
    let mut out = Vec::new();
    let mut pos = 0;
    while pos < record.len() {
        let (tag, tag_len) = match decode_varint(&record[pos..]) {
            Some(v) => v,
            None => break,
        };
        let field_number = (tag >> 3) as u32;
        let wire_type = (tag & 7) as u32;
        let field_start = pos;
        pos += tag_len;

        // Advance past the field value
        match wire_type {
            0 => {
                // varint
                let (_, vlen) = decode_varint(&record[pos..]).unwrap_or((0, 0));
                pos += vlen;
            }
            1 => {
                // fixed64
                pos += 8;
            }
            2 => {
                // length-delimited
                let (len, llen) = decode_varint(&record[pos..]).unwrap_or((0, 0));
                pos += llen + len as usize;
            }
            5 => {
                // fixed32
                pos += 4;
            }
            _ => break,
        }

        if field_number == keep_field {
            out.extend_from_slice(&record[field_start..pos]);
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Wide flat message: 25 fields of mixed types
// ---------------------------------------------------------------------------

const NUM_WIDE_FIELDS: u32 = 25;

/// Build a wide proto record with 25 fields: mix of varint, fixed32, fixed64,
/// and length-delimited fields.
fn make_wide_record(seed: u64) -> Vec<u8> {
    let mut rec = Vec::new();
    for f in 1..=NUM_WIDE_FIELDS {
        match f % 4 {
            1 => rec.extend(encode_varint_field(f, seed.wrapping_mul(f as u64))),
            2 => rec.extend(encode_fixed32_field(f, (seed as u32).wrapping_add(f))),
            3 => rec.extend(encode_fixed64_field(f, seed.wrapping_add(f as u64 * 1000))),
            0 => {
                let data = format!("field_{}_seed_{}", f, seed);
                rec.extend(encode_bytes_field(f, data.as_bytes()));
            }
            _ => unreachable!(),
        }
    }
    rec
}

/// Encode a riegeli file with N wide records using transpose encoding.
fn encode_wide_transpose(n: usize, compression: CompressionType) -> Vec<u8> {
    let mut buf = Vec::new();
    {
        let opts = WriterOptions::default()
            .compression(compression)
            .transpose(true);
        let cursor = Cursor::new(&mut buf);
        let mut writer = RecordWriter::new(cursor, opts).unwrap();
        for i in 0..n {
            writer.write_record(&make_wide_record(i as u64)).unwrap();
        }
        writer.close().unwrap();
    }
    buf
}

// ---------------------------------------------------------------------------
// Nested message: depth 3
// ---------------------------------------------------------------------------

/// Build a nested proto record with a target field at depth 3.
/// Structure:
///   field 1 (varint) — top-level counter
///   field 2 (submessage) {
///     field 1 (varint)
///     field 3 (submessage) {
///       field 1 (varint)
///       field 5 (submessage) {
///         field 1 (varint) — the target at depth 3
///         field 2 (fixed32)
///       }
///       field 2 (fixed32)
///     }
///     field 2 (fixed32)
///   }
///   fields 3..10 (varint) — padding fields at top level
fn make_nested_record(seed: u64) -> Vec<u8> {
    let mut rec = Vec::new();

    // Top-level field 1
    rec.extend(encode_varint_field(1, seed));

    // Build the innermost submessage (field 2.3.5)
    let mut inner3 = Vec::new();
    inner3.extend(encode_varint_field(1, seed.wrapping_mul(7)));
    inner3.extend(encode_fixed32_field(2, seed as u32));

    // Middle submessage (field 2.3)
    let mut inner2 = Vec::new();
    inner2.extend(encode_varint_field(1, seed.wrapping_mul(3)));
    inner2.extend(encode_submessage_field(5, &inner3));
    inner2.extend(encode_fixed32_field(2, seed as u32));

    // Outer submessage (field 2)
    let mut inner1 = Vec::new();
    inner1.extend(encode_varint_field(1, seed.wrapping_mul(2)));
    inner1.extend(encode_submessage_field(3, &inner2));
    inner1.extend(encode_fixed32_field(2, seed as u32));

    rec.extend(encode_submessage_field(2, &inner1));

    // Top-level padding fields 3..25 — these are excluded by the projection,
    // giving the projection-during-decode path substantial work to skip.
    for f in 3..=25 {
        match f % 4 {
            1 => rec.extend(encode_varint_field(f, seed.wrapping_mul(f as u64))),
            2 => rec.extend(encode_fixed32_field(f, (seed as u32).wrapping_add(f))),
            3 => rec.extend(encode_fixed64_field(f, seed.wrapping_add(f as u64 * 100))),
            0 => {
                let data = format!("pad_{}_{}", f, seed);
                rec.extend(encode_bytes_field(f, data.as_bytes()));
            }
            _ => unreachable!(),
        }
    }

    rec
}

/// Encode a riegeli file with N nested records using transpose encoding.
fn encode_nested_transpose(n: usize, compression: CompressionType) -> Vec<u8> {
    let mut buf = Vec::new();
    {
        let opts = WriterOptions::default()
            .compression(compression)
            .transpose(true);
        let cursor = Cursor::new(&mut buf);
        let mut writer = RecordWriter::new(cursor, opts).unwrap();
        for i in 0..n {
            writer.write_record(&make_nested_record(i as u64)).unwrap();
        }
        writer.close().unwrap();
    }
    buf
}

// ---------------------------------------------------------------------------
// Benchmark: Wide flat message, narrow projection
// ---------------------------------------------------------------------------

const BENCH_RECORDS: usize = 2000;

fn bench_wide_flat(c: &mut Criterion) {
    let encoded = encode_wide_transpose(BENCH_RECORDS, CompressionType::None);

    // Total raw payload size for throughput calculation
    let raw_size: u64 = (0..BENCH_RECORDS)
        .map(|i| make_wide_record(i as u64).len() as u64)
        .sum();

    let mut group = c.benchmark_group("wide_flat");
    group.throughput(Throughput::Bytes(raw_size));
    group.sampling_mode(SamplingMode::Flat);
    group.sample_size(20);

    // (a) Projected decode: 1 of 25 fields (field 5, a varint) — current path
    group.bench_function("projected_1_of_25", |b| {
        b.iter(|| {
            let proj = FieldProjection::new().add_field(Field::new(vec![5]));
            let opts = ReaderOptions::new().field_projection(proj);
            let cursor = Cursor::new(&encoded);
            let mut reader = RecordReader::new(cursor, opts).unwrap();
            let mut count = 0;
            while let Some(_rec) = reader.read_record().unwrap() {
                count += 1;
            }
            assert_eq!(count, BENCH_RECORDS);
            count
        });
    });

    // (b) Baseline: full decode + manual field filter (pre-phase-6 approach)
    group.bench_function("baseline_full_decode_then_filter", |b| {
        b.iter(|| {
            let opts = ReaderOptions::new();
            let cursor = Cursor::new(&encoded);
            let mut reader = RecordReader::new(cursor, opts).unwrap();
            let mut count = 0;
            while let Some(rec) = reader.read_record().unwrap() {
                let _filtered = filter_record_keep_field(&rec, 5);
                count += 1;
            }
            assert_eq!(count, BENCH_RECORDS);
            count
        });
    });

    // (c) Full decode — no projection at all
    group.bench_function("full_decode_no_projection", |b| {
        b.iter(|| {
            let opts = ReaderOptions::new();
            let cursor = Cursor::new(&encoded);
            let mut reader = RecordReader::new(cursor, opts).unwrap();
            let mut count = 0;
            while let Some(_rec) = reader.read_record().unwrap() {
                count += 1;
            }
            assert_eq!(count, BENCH_RECORDS);
            count
        });
    });

    group.finish();
}

// ---------------------------------------------------------------------------
// Benchmark: Nested message, depth-3 projection
// ---------------------------------------------------------------------------

fn bench_nested_depth3(c: &mut Criterion) {
    let encoded = encode_nested_transpose(BENCH_RECORDS, CompressionType::None);

    let raw_size: u64 = (0..BENCH_RECORDS)
        .map(|i| make_nested_record(i as u64).len() as u64)
        .sum();

    let mut group = c.benchmark_group("nested_depth3");
    group.throughput(Throughput::Bytes(raw_size));
    group.sampling_mode(SamplingMode::Flat);
    group.sample_size(20);

    // (a) Projected: only field 2.3.5 (depth 3)
    group.bench_function("projected_depth3_field", |b| {
        b.iter(|| {
            let proj = FieldProjection::new().add_field(Field::new(vec![2, 3, 5]));
            let opts = ReaderOptions::new().field_projection(proj);
            let cursor = Cursor::new(&encoded);
            let mut reader = RecordReader::new(cursor, opts).unwrap();
            let mut count = 0;
            while let Some(_rec) = reader.read_record().unwrap() {
                count += 1;
            }
            assert_eq!(count, BENCH_RECORDS);
            count
        });
    });

    // (b) Baseline: full decode + filter (simulate pre-phase-6)
    // For nested, the baseline filter is just full decode since the apply()
    // also did a full decode first. We measure the full decode time.
    group.bench_function("baseline_full_decode", |b| {
        b.iter(|| {
            let opts = ReaderOptions::new();
            let cursor = Cursor::new(&encoded);
            let mut reader = RecordReader::new(cursor, opts).unwrap();
            let mut count = 0;
            while let Some(_rec) = reader.read_record().unwrap() {
                count += 1;
            }
            assert_eq!(count, BENCH_RECORDS);
            count
        });
    });

    group.finish();
}

// ---------------------------------------------------------------------------
// Benchmark: Non-projected regression check
// ---------------------------------------------------------------------------

fn bench_non_projected_regression(c: &mut Criterion) {
    // Use the wide records to check that non-projected reads haven't regressed.
    // This is the same as full_decode_no_projection but in its own group
    // for clear comparison.
    let encoded = encode_wide_transpose(BENCH_RECORDS, CompressionType::None);

    let raw_size: u64 = (0..BENCH_RECORDS)
        .map(|i| make_wide_record(i as u64).len() as u64)
        .sum();

    let mut group = c.benchmark_group("non_projected_regression");
    group.throughput(Throughput::Bytes(raw_size));
    group.sampling_mode(SamplingMode::Flat);
    group.sample_size(20);

    // No projection — should have no regression from phase-6 changes
    group.bench_function("full_decode", |b| {
        b.iter(|| {
            let opts = ReaderOptions::new();
            let cursor = Cursor::new(&encoded);
            let mut reader = RecordReader::new(cursor, opts).unwrap();
            let mut count = 0;
            while let Some(_rec) = reader.read_record().unwrap() {
                count += 1;
            }
            assert_eq!(count, BENCH_RECORDS);
            count
        });
    });

    // Also test with FieldProjection::all() — should behave identically
    group.bench_function("all_projection", |b| {
        b.iter(|| {
            let proj = FieldProjection::all();
            let opts = ReaderOptions::new().field_projection(proj);
            let cursor = Cursor::new(&encoded);
            let mut reader = RecordReader::new(cursor, opts).unwrap();
            let mut count = 0;
            while let Some(_rec) = reader.read_record().unwrap() {
                count += 1;
            }
            assert_eq!(count, BENCH_RECORDS);
            count
        });
    });

    group.finish();
}

// ---------------------------------------------------------------------------
// Benchmark: Compression variants (Brotli, Zstd) with projection
// ---------------------------------------------------------------------------

fn bench_projection_with_compression(c: &mut Criterion) {
    let compressions = vec![
        ("brotli", CompressionType::Brotli),
        ("zstd", CompressionType::Zstd),
    ];

    for (name, comp) in &compressions {
        let encoded = encode_wide_transpose(BENCH_RECORDS, *comp);

        let raw_size: u64 = (0..BENCH_RECORDS)
            .map(|i| make_wide_record(i as u64).len() as u64)
            .sum();

        let mut group = c.benchmark_group(format!("wide_{}", name));
        group.throughput(Throughput::Bytes(raw_size));
        group.sampling_mode(SamplingMode::Flat);
        group.sample_size(20);

        group.bench_function("projected_1_of_25", |b| {
            b.iter(|| {
                let proj = FieldProjection::new().add_field(Field::new(vec![5]));
                let opts = ReaderOptions::new().field_projection(proj);
                let cursor = Cursor::new(&encoded);
                let mut reader = RecordReader::new(cursor, opts).unwrap();
                let mut count = 0;
                while let Some(_rec) = reader.read_record().unwrap() {
                    count += 1;
                }
                assert_eq!(count, BENCH_RECORDS);
                count
            });
        });

        group.bench_function("full_decode", |b| {
            b.iter(|| {
                let opts = ReaderOptions::new();
                let cursor = Cursor::new(&encoded);
                let mut reader = RecordReader::new(cursor, opts).unwrap();
                let mut count = 0;
                while let Some(_rec) = reader.read_record().unwrap() {
                    count += 1;
                }
                assert_eq!(count, BENCH_RECORDS);
                count
            });
        });

        group.finish();
    }
}

// ---------------------------------------------------------------------------
// Criterion harness
// ---------------------------------------------------------------------------

criterion_group! {
    name = projection_benches;
    config = Criterion::default()
        .significance_level(0.05)
        .noise_threshold(0.05);
    targets =
        bench_wide_flat,
        bench_nested_depth3,
        bench_non_projected_regression,
        bench_projection_with_compression
}
criterion_main!(projection_benches);
