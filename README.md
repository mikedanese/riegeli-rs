# riegeli-rs

A pure-Rust implementation of [Google's Riegeli/records](https://github.com/google/riegeli)
file format. Riegeli is a seekable, compressed record store designed for high-throughput
sequential reads and writes, commonly used in machine learning pipelines and large-scale
data processing. This crate is byte-level compatible with the C++ reference implementation.

## Quick start

```toml
[dependencies]
riegeli = "0.1"
```

```rust
use std::io::Cursor;
use riegeli::{RecordWriter, RecordReader, WriterOptions, ReaderOptions, CompressionType};

// Write
let mut buf = Vec::new();
let opts = WriterOptions::new().compression(CompressionType::Zstd);
let mut writer = RecordWriter::new(Cursor::new(&mut buf), opts).unwrap();
writer.write_record(b"alpha").unwrap();
writer.write_record(b"bravo").unwrap();
writer.write_record(b"charlie").unwrap();
writer.close().unwrap();

// Read
let mut reader = RecordReader::new(Cursor::new(&buf), ReaderOptions::new()).unwrap();
while let Some(record) = reader.read_record().unwrap() {
    println!("{}", std::str::from_utf8(&record).unwrap());
}
```

## Build requirements

This crate generates Rust code from `.proto` files at build time using
`protobuf-codegen`, which requires a compatible `protoc` binary on your `PATH`.
Download the latest release from the
[protobuf releases page](https://github.com/protocolbuffers/protobuf/releases)
(look for `protoc-<version>-<platform>.zip`).

## Benchmarks

See [`riegeli/benches/README.md`](riegeli/benches/README.md) for the full
head-to-head Rust vs. C++ benchmark matrix. Representative results on Linux
x86-64 (10 000 records, large payload):

| Config             | Rust write | Rust read | C++ write | C++ read |
|--------------------|----------:|----------:|----------:|---------:|
| simple+none        | 1348 MB/s | 2832 MB/s |  747 MB/s | 1343 MB/s |
| simple+zstd:3      | 2123 MB/s | 3070 MB/s | 3693 MB/s | 5914 MB/s |
| transpose+none     |  956 MB/s |  808 MB/s |  693 MB/s | 1149 MB/s |
| transpose+zstd:3   | 1605 MB/s |  845 MB/s | 3142 MB/s | 4123 MB/s |

C++ read throughput is measured through the FFI bridge and includes a per-record
copy across the boundary, making it lower than native C++ performance.

## License

Apache-2.0
