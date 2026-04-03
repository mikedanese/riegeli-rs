# TODO

Phase 3+ roadmap for riegeli-rs.

## Sprint plan

| Sprint | Work |
|---|---|
| 14 | C++ golden files + activate `skip` tests |
| 15 | Writer tuning: `compression_level`, `window_log`, `bucket_fraction`, `final_padding` |
| 16 | Benchmark matrix + CPU profiling baseline |
| 17 | `set_metadata`, `last_record_is_valid`, `seek_back`, `size`, `check_file_format` |
| 18 | `FieldProjection` (transpose decoder column pruning) |
| 19 | `SetFieldProjection`, `Search` |

**Rationale for this order:**
- Conformance first: 13 sprints of `skip` criteria mean we have no hard evidence
  our files are C++-compatible.
- Benchmarks before `FieldProjection`: FieldProjection is the hardest item on the
  list; you want a measurement baseline before investing in it, and you need
  `bucket_fraction < 1.0` written into files before projection saves any work.
- Writer tuning knobs together: `compression_level`, `window_log`, `bucket_fraction`
  all touch the same call sites; batching them avoids churning those files twice.

---

## Sprint 14 — C++ conformance

### Goal

Verify both directions:
- Files written by Rust are readable by the C++ `RecordReader`.
- Files written by C++ are readable by our `RecordReader`.

### Approach: golden files

Write a small C++ tool (`tools/gen_golden.cc`, built with Bazel) that emits a
fixed set of `.riegeli` files:

| Variant                | File |
|------------------------|------|
| Simple + None          | `testdata/golden/simple_none.riegeli` |
| Simple + Brotli        | `testdata/golden/simple_brotli.riegeli` |
| Simple + Zstd          | `testdata/golden/simple_zstd.riegeli` |
| Transpose + None       | `testdata/golden/transpose_none.riegeli` |
| Transpose + Brotli     | `testdata/golden/transpose_brotli.riegeli` |
| Transpose + Zstd       | `testdata/golden/transpose_zstd.riegeli` |
| With FileMetadata      | `testdata/golden/with_metadata.riegeli` |
| Multi-block (> 64 KiB) | `testdata/golden/multiblock.riegeli` |

Check the generated files into the repo. Add Rust integration tests that read
them with `RecordReader` and assert record-for-record correctness. This activates
the `skip` criteria in sprints 4, 6, 9, 12.

A companion `tools/verify_rust_file.cc` reads a Rust-written file and asserts
correctness — invoked from `cargo test` via `std::process::Command`.

**Build note:** Riegeli is Bazel-only with Abseil, HighwayHash, Brotli, Zstd,
Snappy dependencies. Build the tools once on a dev machine; check in the golden
files as binary assets (not the tool itself).

### Why not autocxx / cxx bindings?

`autocxx` parses C++ headers with libclang and generates Rust FFI. It would work
in principle but the riegeli API makes it painful:

- **Heavy template instantiation**: `RecordReader<FdReader<>>`,
  `RecordWriter<FdWriter<>>` — autocxx handles simple templates but this hierarchy
  is fragile.
- **Abseil types everywhere**: `absl::Status`, `Chain`, `absl::Cord` — no
  automatic Rust mappings; a hand-written shim layer would be needed anyway.
- **Bazel-only**: no pkg-config or CMake; would need Bazel to produce a `.a` and
  thread it into `build.rs`. Painful in CI.

A thin `extern "C"` wrapper taking `const char*` / `size_t` for records would be
simpler than autocxx if in-process FFI ever becomes necessary. For now golden
files cover conformance with far less maintenance overhead.

---

## Sprint 15 — Writer tuning knobs

All four touch `WriterOptions` and the compressor call sites; doing them together
avoids revisiting those files twice.

### `compression_level(i32)`

Per-codec compression level. The C++ reference exposes this; we currently pick
hard-coded defaults.

| Codec  | Range        | Default |
|--------|--------------|---------|
| Brotli | 0–11         | 6       |
| Zstd   | −131072..22  | 3       |
| Snappy | 1–2          | 1       |

Add `level: Option<i32>` to `WriterOptions`. Thread into `compress_brotli`
(the `quality` parameter) and `compress_zstd` (the `level` parameter) in
`simple_chunk.rs` and `transpose/encoder.rs`.

### `window_log(Option<u32>)`

Compressor window size. `None` = codec default. Valid ranges: Brotli 10–30,
Zstd 10–31. Must be `None` for Snappy/None compression. Maps to `lgwin` in the
brotli crate and `window_log` in the zstd crate.

### `bucket_fraction(f64)`

Controls how transpose data is split into independently-compressed buckets.
Range 0.0–1.0, default 1.0.

```
bucket_size = chunk_size × bucket_fraction
```

At `1.0` all fields land in one big bucket — best compression ratio. Lower values
create more, smaller buckets — worse ratio but faster projection reads (the
decoder skips buckets whose fields aren't in the projection). This is a write-time
decision that determines how much benefit `FieldProjection` can deliver.

`TransposeChunkEncoder` already accepts a `bucket_size`; just compute it from the
fraction and pass it through from `WriterOptions`.

### `final_padding(u64)`

Like `initial_padding` but also applied after every `flush()` call, not just
`close()`. Used when downstream consumers require files to end on a block
boundary.

---

## Sprint 16 — Benchmark matrix and profiling baseline

Expand the existing `benches/throughput.rs` to cover the full configuration
matrix. Run this *before* implementing `FieldProjection` so we have a baseline to
measure the projection benefit against.

### Benchmark matrix

Mirror the C++ `records_benchmark.cc` configuration matrix:

| Config                      | Write MB/s | Read MB/s |
|-----------------------------|------------|-----------|
| simple + none               |            |           |
| simple + brotli:6           |            |           |
| simple + brotli:9           |            |           |
| simple + zstd:3             |            |           |
| simple + snappy             |            |           |
| transpose + none            |            |           |
| transpose + brotli:6        |            |           |
| transpose + zstd:3          |            |           |
| transpose + brotli:6 + proj |            |           |

The `proj` row requires `FieldProjection` (Sprint 18); leave it as a placeholder.
Fill in numbers from `cargo bench` and compare against C++ reference numbers from
a standalone Bazel `records_benchmark` run on the same machine.

### CPU profiling with pprof-rs

Add [pprof-rs](https://github.com/tikv/pprof-rs) to the criterion benchmarks for
flamegraph output:

```toml
[dev-dependencies]
pprof = { version = "0.13", features = ["flamegraph", "criterion"] }
```

```rust
// benches/throughput.rs
use pprof::criterion::{Output, PProfProfiler};

criterion_group! {
    name = benches;
    config = Criterion::default()
        .with_profiler(PProfProfiler::new(100, Output::Flamegraph(None)));
    targets = bench_write, bench_read
}
```

Flamegraphs land in `target/criterion/<bench>/profile/flamegraph.svg`. Identify
any hotspots before investing in the `FieldProjection` and `parallelism` work.

---

## Sprint 17 — RecordWriter/RecordReader completeness

### `set_metadata(Vec<u8>)` / `set_serialized_metadata(Vec<u8>)` · WriterOptions

Write a `ChunkType::FileMetadata` chunk immediately after the signature chunk.
Carries schema information (record type name, proto file descriptor). The reader
side already exists (`read_metadata()`); the writer side is missing.

### `last_record_is_valid() -> bool` · RecordReader

After a recovery callback fires, indicates whether the most recently returned
record came from a valid (non-recovered) chunk. The C++ reference resets this to
`false` at Seek/SeekBack/Search/Close and sets it `true` on every successful read.

Add `last_record_is_valid: bool` to `RecordReader`. One field, a few assignment
sites.

### `seek_back() -> Result<bool, RiegeliError>` · RecordReader

Seek to the immediately previous record. We already track `last_pos:
RecordPosition`; expose it as a public method that calls
`seek_numeric(last_pos.numeric())`.

### `size() -> Result<u64, RiegeliError>` · RecordReader

Total record count. Scan all chunk headers summing `num_records` (no data
decompression needed). Save and restore the current seek position.

### `check_file_format() -> Result<(), RiegeliError>` · RecordReader

Validate all block headers and chunk header + data hashes without decoding any
record data. Useful as a health check in data pipelines.

---

## Sprint 18 — FieldProjection

The main read-time payoff for transpose encoding. A `FieldProjection` is a set of
proto field paths; when set, the decoder decompresses only the buckets that
contribute to those fields.

### Types

```rust
/// A proto field path (sequence of field numbers from the root message).
pub struct Field { path: Vec<u32> }

/// A set of fields to include in decoded records.
/// `FieldProjection::all()` includes everything (the default).
pub struct FieldProjection { fields: Option<Vec<Field>> }
```

`Field::existence_only()` mirrors the C++ `kExistenceOnly` sentinel — preserves
field presence but zeroes the value.

### Integration

- `ReaderOptions::field_projection(FieldProjection)`.
- `TransposeChunkDecoder` already parses the bucket index at load time; extend it
  to skip decompression for buckets whose tags don't match any projected field.
- Non-proto records must always pass through regardless of projection.
- `WriterOptions::bucket_fraction < 1.0` must have been used at write time for
  projection to save any decompression work (Sprint 15 sets this up).

---

## Sprint 19 — SetFieldProjection and Search

### `set_field_projection(FieldProjection)` · RecordReader

Change the active projection mid-read without constructing a new reader. Store
the new projection and apply it when the next `TransposeChunkDecoder` is created
(i.e., at the next chunk boundary). Depends on Sprint 18.

### `search<F>(test: F) -> Result<bool, RiegeliError>` · RecordReader

Binary search over the file by record content:

```rust
reader.search(|record: &[u8]| -> Ordering { ... })
```

Bisects over chunk positions, reads one record per chunk to call `test`, then
narrows the range. Makes riegeli usable as a sorted key-value store — a headline
capability the C++ reference documents prominently.

Depends on `size()` (Sprint 17) and `seek_back()` (Sprint 17).

---

## Deferred

### `parallelism(usize)` · WriterOptions

Parallel chunk compression via `rayon`. Encode up to N chunks concurrently while
the main thread fills the next one. Biggest write throughput win for Brotli:6+.
Deferred until after the Sprint 16 profiling pass confirms this is the bottleneck.

### tokio async adapter

`AsyncRecordWriter<W: AsyncWrite + Unpin>` / `AsyncRecordReader<R: AsyncRead + Unpin>`.
Not in the C++ reference. Main design question: sync compression on the calling
task vs. `spawn_blocking` per chunk.

### Python bindings (pyo3)

The C++ reference has a full `python/riegeli/` package. Depends on stable public
API first.

### CLI tools

- `riegeli-describe` — dump block/chunk structure (mirrors `describe_riegeli_file`)
- `riegeli-cat` — decode and print records to stdout
