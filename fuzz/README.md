# Fuzz targets

Requires nightly and [cargo-fuzz](https://github.com/rust-fuzz/cargo-fuzz)
(`cargo install cargo-fuzz`). Run from the repository root:

```bash
# Seed the reader corpus with the boundary conformance files — they carry
# block headers, straddling chunk headers, and boundary-coincident chunks.
mkdir -p fuzz/corpus/read_records
cp riegeli/testdata/boundary/*.riegeli fuzz/corpus/read_records/

# -max_len must exceed 64 KiB or the seeds get truncated below the first
# block boundary, which is where the interesting behavior starts.
cargo fuzz run read_records    -- -max_len=262144
cargo fuzz run transpose_decode
cargo fuzz run varint          -- -max_len=64
```

CI builds all targets and smoke-runs each briefly; real fuzzing time is
expected to be spent locally or on a dedicated runner.
