# TODO

Roadmap for riegeli-rs.

## Completed

| Sprint | Work | Status |
|---|---|---|
| 1–7 | Core reader/writer, simple chunks, all compression codecs, block boundaries, recovery | Done |
| 8–13 | Transpose chunk encoding/decoding | Done |
| 14 | C++ golden files, cross-language conformance tests | Done |
| 15 | Writer tuning: `compression_level`, `window_log`, `bucket_fraction`, `final_padding` | Done |
| 16 | Benchmark matrix + CPU profiling baseline | Done |
| 17 | `set_metadata`, `last_record_is_valid`, `seek_back`, `size`, `check_file_format` | Done |
| 18 | `FieldProjection` (transpose decoder column pruning) | Done |
| 19 | `set_field_projection`, `search` (binary search over sorted records) | Done |

## Remaining gaps vs. C++ reference

### Performance

- **`parallelism(usize)`** — parallel chunk compression via `rayon`. Encode up to
  N chunks concurrently while the main thread fills the next one. Biggest write
  throughput win for Brotli:6+.
- **Compression context recycling** — C++ pools compressor/decompressor state
  across chunks to reduce allocation overhead.

### Compression

- **Custom dictionaries** — C++ Brotli and Zstd codecs support user-provided
  dictionaries for better compression of small records with shared structure.

### Protobuf integration

- **`SerializedMessageReader` / `SerializedMessageWriter`** — streaming proto
  field-level reading/writing without full deserialization.
- **`FieldHandlers` / `DynamicFieldHandler`** — callback-driven proto field
  processing framework.
- **`ContextProjection`** — richer projection API integrated with proto
  descriptors.
- **Projection include-resolution: deliberate divergence from C++.** This
  implementation resolves field inclusion per visit, keyed by the full
  ancestry context. The C++ reference caches the resolution on first visit,
  which makes its output order-dependent when the same field number appears
  both in an included context and inside an excluded group (one wire order
  emits excluded data, the mirrored order omits included data). The
  divergence is documented and both orders are pinned as known-divergent
  cases in `riegeli/tests/differential.rs` (cases J/J2); revisit if the
  reference implementation changes its resolution behavior.

### Flush durability levels

- C++ distinguishes `kFromObject` (in-process visibility), `kFromProcess`
  (OS-visible, default), and `kFromMachine` (fsync). Rust has a single `flush()`.
  Mainly matters if parallelism is added.

### Ancillary modules (not part of the record format)

- **CSV reader/writer** — `riegeli/csv/`
- **Text/line reader/writer** — `riegeli/lines/`, `riegeli/text/`
- **Digest framework** — 9 algorithms with digesting reader/writer wrappers
- **Ordered varint** — order-preserving encoding for sorted keys
- **TensorFlow integration** — custom TF kernels and ops

### Bindings & tooling

- **tokio async adapter** — `AsyncRecordWriter` / `AsyncRecordReader`. Not in the
  C++ reference. Main design question: sync compression on the calling task vs.
  `spawn_blocking` per chunk.
- **CLI tools** — `riegeli-describe` (dump block/chunk structure),
  `riegeli-cat` (decode and print records to stdout).
