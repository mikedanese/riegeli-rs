# riegeli-rs

[![crates.io](https://img.shields.io/crates/v/riegeli.svg)](https://crates.io/crates/riegeli)
[![docs.rs](https://img.shields.io/docsrs/riegeli)](https://docs.rs/riegeli)

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

## Benchmarks

See [`riegeli/benches/README.md`](riegeli/benches/README.md) for the full
head-to-head Rust vs. C++ benchmark matrix. Representative results on Linux
x86-64 (10 000 records, large payload):

| Config             | Rust write | Rust read | C++ write | C++ read |
|--------------------|----------:|----------:|----------:|---------:|
| simple+none        | 1343 MB/s | 2778 MB/s |  752 MB/s | 1396 MB/s |
| simple+zstd:3      | 1996 MB/s | 2994 MB/s | 3444 MB/s | 5849 MB/s |
| transpose+none     |  950 MB/s | 1613 MB/s |  706 MB/s | 1185 MB/s |
| transpose+zstd:3   | 1502 MB/s | 1607 MB/s | 2881 MB/s | 4021 MB/s |

C++ read throughput is measured through the FFI bridge and includes a per-record
copy across the boundary, making it lower than native C++ performance.

## License

Apache-2.0
