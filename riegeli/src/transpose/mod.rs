//! Transpose chunk encoding and decoding.
//!
//! This module implements `ChunkType::Transposed` (`'t'`), which decomposes
//! protobuf records column-wise by field path for dramatically better
//! compression of structured data.

pub mod decoder;
pub mod encoder;
pub mod internal;
