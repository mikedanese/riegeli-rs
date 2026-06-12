# riegeli-ffi

Test-only bridge to the C++ riegeli reference implementation.

This crate exists for the test suite: differential tests, cross-language
round-trips, and head-to-head benchmarks build the reference implementation
and drive it through this bridge to verify that the Rust port in `../riegeli`
matches it byte for byte.

It is **not** part of the published library:

- `publish = false` — it never goes to crates.io.
- No API stability: interfaces are shaped by what the tests need and change
  without notice.
- Do not depend on it from anything outside this repository.

The first build downloads and compiles the C++ reference and its
dependencies, which takes several minutes; results are cached in `target/`.
