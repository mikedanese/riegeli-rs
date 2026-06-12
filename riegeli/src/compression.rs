//! Compression and decompression utilities for Riegeli chunks.
//!
//! Provides codec dispatch for Brotli, Zstd, and Snappy, plus helper functions
//! that match the C++ `Compressor::EncodeAndClose` and
//! `Compressor::LengthPrefixedEncodeAndClose` wire formats.

use crate::error::RiegeliError;
use crate::varint::{decode_u64, encode_u64};

/// The compression algorithm applied to the record data inside a chunk.
///
/// The discriminant values match the C++ wire format exactly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum CompressionType {
    /// No compression (byte value `0`).
    None = 0,
    /// Brotli compression (type byte `'b'`).
    Brotli = b'b',
    /// Zstd compression (type byte `'z'`).
    Zstd = b'z',
    /// Snappy compression (type byte `'s'`).
    Snappy = b's',
}

impl TryFrom<u8> for CompressionType {
    type Error = RiegeliError;

    fn try_from(b: u8) -> Result<Self, Self::Error> {
        match b {
            0 => Ok(CompressionType::None),
            b'b' => Ok(CompressionType::Brotli),
            b'z' => Ok(CompressionType::Zstd),
            b's' => Ok(CompressionType::Snappy),
            _ => Err(RiegeliError::UnknownCompressionType(b)),
        }
    }
}

/// Compression tuning options passed to low-level compression functions.
///
/// Carries optional overrides for quality/level and window size. `None` means
/// use the codec's built-in default.
#[derive(Debug, Clone, Copy, Default)]
pub struct CompressOptions {
    /// Compression level / quality override.
    ///
    /// For Brotli: 0-11 (default 6). For Zstd: -131072..=22 (default 3).
    /// Ignored for Snappy and `CompressionType::None`.
    pub level: Option<i32>,
    /// Window size (log2 of window size in bytes).
    ///
    /// For Brotli: 10-30 (default 22, mapped to `lgwin`).
    /// For Zstd: 10-31 (default 0 = automatic, mapped to `window_log`).
    /// Must be `None` for Snappy and `CompressionType::None`.
    pub window_log: Option<u32>,
}

/// Compress bytes using Brotli.
#[cfg(feature = "brotli")]
pub(crate) fn compress_brotli(
    input: &[u8],
    opts: CompressOptions,
) -> Result<Vec<u8>, RiegeliError> {
    use std::io::Write as _;
    let quality = opts.level.unwrap_or(6).clamp(0, 11) as u32;
    let lgwin = opts.window_log.unwrap_or(22).clamp(10, 30);
    let mut output = Vec::new();
    {
        let mut writer = brotli::CompressorWriter::new(&mut output, 4096, quality, lgwin);
        writer.write_all(input).map_err(|e| {
            RiegeliError::MalformedData(format!("brotli compress error: {e}").into())
        })?;
    }
    Ok(output)
}

/// Decompress Brotli bytes, requiring the stream to span the whole input.
///
/// The whole input must be consumed by the Brotli stream: the C++
/// `Decompressor::VerifyEndAndClose` rejects any bytes left over after the
/// compressed stream, so trailing data here is an error, not ignored.
///
/// Output is capped at `max_len`: decompression errors once it would
/// materialize more than `max_len + 1` bytes, bounding decompression bombs.
#[cfg(feature = "brotli")]
pub(crate) fn decompress_brotli(input: &[u8], max_len: u64) -> Result<Vec<u8>, RiegeliError> {
    use brotli::{BrotliDecompressStream, BrotliResult, BrotliState, HeapAlloc};

    const MAX_DECOMPRESS_PREALLOC: u64 = 1 << 24; // 16 MiB
    let hard_cap = usize::try_from(max_len.saturating_add(1)).unwrap_or(usize::MAX);
    let mut state = BrotliState::new(
        HeapAlloc::<u8>::new(0),
        HeapAlloc::<u32>::new(0),
        HeapAlloc::new(Default::default()),
    );
    let mut available_in = input.len();
    let mut input_offset = 0usize;
    let mut output = vec![0u8; hard_cap.min(MAX_DECOMPRESS_PREALLOC as usize).max(64)];
    let mut output_offset = 0usize;
    let mut total_out = 0usize;
    loop {
        let mut available_out = output.len() - output_offset;
        match BrotliDecompressStream(
            &mut available_in,
            &mut input_offset,
            input,
            &mut available_out,
            &mut output_offset,
            &mut output,
            &mut total_out,
            &mut state,
        ) {
            BrotliResult::ResultSuccess => {
                if available_in != 0 {
                    return Err(RiegeliError::MalformedData(
                        "trailing data after Brotli-compressed stream".into(),
                    ));
                }
                output.truncate(output_offset);
                if output.len() as u64 > max_len {
                    return Err(RiegeliError::MalformedData(
                        format!("decompressed data exceeds its declared size ({max_len} bytes)")
                            .into(),
                    ));
                }
                return Ok(output);
            }
            BrotliResult::NeedsMoreOutput => {
                if output.len() >= hard_cap {
                    return Err(RiegeliError::MalformedData(
                        format!("decompressed data exceeds its declared size ({max_len} bytes)")
                            .into(),
                    ));
                }
                let new_len = output.len().saturating_mul(2).max(4096).min(hard_cap);
                output.resize(new_len, 0);
            }
            BrotliResult::NeedsMoreInput => {
                return Err(RiegeliError::MalformedData(
                    "brotli decompress error: truncated stream".into(),
                ));
            }
            BrotliResult::ResultFailure => {
                return Err(RiegeliError::MalformedData(
                    "brotli decompress error: invalid stream".into(),
                ));
            }
        }
    }
}

/// Compress bytes using Zstd.
#[cfg(feature = "zstd")]
pub(crate) fn compress_zstd(input: &[u8], opts: CompressOptions) -> Result<Vec<u8>, RiegeliError> {
    let level = opts.level.unwrap_or(3).clamp(-131072, 22);
    let compressed = if let Some(wlog) = opts.window_log {
        // Use the streaming encoder to set window_log.
        use std::io::Write as _;
        let mut output = Vec::new();
        {
            let mut encoder = zstd::Encoder::new(&mut output, level).map_err(|e| {
                RiegeliError::MalformedData(format!("zstd encoder init: {e}").into())
            })?;
            encoder
                .window_log(wlog)
                .map_err(|e| RiegeliError::MalformedData(format!("zstd window_log: {e}").into()))?;
            encoder.write_all(input).map_err(|e| {
                RiegeliError::MalformedData(format!("zstd compress error: {e}").into())
            })?;
            encoder.finish().map_err(|e| {
                RiegeliError::MalformedData(format!("zstd finish error: {e}").into())
            })?;
        }
        output
    } else {
        zstd::encode_all(input, level)
            .map_err(|e| RiegeliError::MalformedData(format!("zstd compress error: {e}").into()))?
    };
    Ok(compressed)
}

/// Decompress Zstd bytes, requiring the frame to span the whole input.
///
/// Decodes exactly one Zstd frame and requires it to span the whole input.
/// The C++ `ZstdReader` does not concatenate frames, and
/// `Decompressor::VerifyEndAndClose` rejects any bytes left over after the
/// frame (including a second valid frame), so trailing data here is an error.
///
/// Output is capped at `max_len`: reading stops one byte past the cap and
/// errors, bounding decompression bombs.
#[cfg(feature = "zstd")]
pub(crate) fn decompress_zstd(input: &[u8], max_len: u64) -> Result<Vec<u8>, RiegeliError> {
    use std::io::Read as _;

    const MAX_DECOMPRESS_PREALLOC: u64 = 1 << 24; // 16 MiB
    let cursor = std::io::Cursor::new(input);
    let mut decoder = zstd::stream::read::Decoder::with_buffer(cursor)
        .map_err(|e| RiegeliError::MalformedData(format!("zstd decompress error: {e}").into()))?
        .single_frame();
    let mut output = Vec::with_capacity(max_len.min(MAX_DECOMPRESS_PREALLOC) as usize);
    (&mut decoder)
        .take(max_len.saturating_add(1))
        .read_to_end(&mut output)
        .map_err(|e| RiegeliError::MalformedData(format!("zstd decompress error: {e}").into()))?;
    if output.len() as u64 > max_len {
        return Err(RiegeliError::MalformedData(
            format!("decompressed data exceeds its declared size ({max_len} bytes)").into(),
        ));
    }
    let consumed = decoder.finish().position() as usize;
    if consumed < input.len() {
        return Err(RiegeliError::MalformedData(
            "trailing data after Zstd-compressed stream".into(),
        ));
    }
    Ok(output)
}

/// Compress bytes using Snappy.
#[cfg(feature = "snappy")]
pub(crate) fn compress_snappy(input: &[u8]) -> Result<Vec<u8>, RiegeliError> {
    let mut encoder = snap::raw::Encoder::new();
    encoder
        .compress_vec(input)
        .map_err(|e| RiegeliError::MalformedData(format!("snappy compress error: {e}").into()))
}

/// Decompress Snappy bytes.
#[cfg(feature = "snappy")]
pub(crate) fn decompress_snappy(input: &[u8]) -> Result<Vec<u8>, RiegeliError> {
    let mut decoder = snap::raw::Decoder::new();
    decoder
        .decompress_vec(input)
        .map_err(|e| RiegeliError::MalformedData(format!("snappy decompress error: {e}").into()))
}

/// Compress data using the specified compression type and options.
///
/// This is the shared compression dispatch used by both the simple and transpose
/// chunk encoders. For `CompressionType::None`, the data is returned as-is (copied).
pub(crate) fn compress_data(
    data: &[u8],
    compression: CompressionType,
    opts: CompressOptions,
) -> Result<Vec<u8>, RiegeliError> {
    match compression {
        CompressionType::None => Ok(data.to_vec()),
        CompressionType::Brotli => {
            #[cfg(feature = "brotli")]
            {
                compress_brotli(data, opts)
            }
            #[cfg(not(feature = "brotli"))]
            {
                Err(RiegeliError::UnsupportedCompression(
                    CompressionType::Brotli as u8,
                ))
            }
        }
        CompressionType::Zstd => {
            #[cfg(feature = "zstd")]
            {
                compress_zstd(data, opts)
            }
            #[cfg(not(feature = "zstd"))]
            {
                Err(RiegeliError::UnsupportedCompression(
                    CompressionType::Zstd as u8,
                ))
            }
        }
        CompressionType::Snappy => {
            #[cfg(feature = "snappy")]
            {
                compress_snappy(data)
            }
            #[cfg(not(feature = "snappy"))]
            {
                Err(RiegeliError::UnsupportedCompression(
                    CompressionType::Snappy as u8,
                ))
            }
        }
    }
}

/// Compress data matching the C++ `Compressor::EncodeAndClose` format.
///
/// For compressed types, writes `varint64(uncompressed_size)` followed by
/// compressed bytes. For `CompressionType::None`, writes the raw bytes
/// with no prefix.
pub(crate) fn compress_with_prefix(
    data: &[u8],
    compression: CompressionType,
    opts: CompressOptions,
) -> Result<Vec<u8>, RiegeliError> {
    let compressed = compress_data(data, compression, opts)?;
    if compression == CompressionType::None {
        return Ok(compressed);
    }
    let mut result = Vec::new();
    result.extend_from_slice(&encode_u64(data.len() as u64));
    result.extend_from_slice(&compressed);
    Ok(result)
}

/// Compress data matching the C++ `Compressor::LengthPrefixedEncodeAndClose` format.
///
/// Writes `varint64(blob_len)` where blob_len includes the varint(uncompressed_size)
/// prefix (for compressed types), then varint64(uncompressed_size) (for compressed),
/// then the compressed/raw data.
pub(crate) fn compress_length_prefixed(
    data: &[u8],
    compression: CompressionType,
    opts: CompressOptions,
) -> Result<Vec<u8>, RiegeliError> {
    use crate::varint::length_varint_u64;

    let compressed = compress_data(data, compression, opts)?;
    let mut blob_len = compressed.len() as u64;
    if compression != CompressionType::None {
        blob_len += length_varint_u64(data.len() as u64) as u64;
    }

    let mut result = Vec::new();
    result.extend_from_slice(&encode_u64(blob_len));
    if compression != CompressionType::None {
        result.extend_from_slice(&encode_u64(data.len() as u64));
    }
    result.extend_from_slice(&compressed);
    Ok(result)
}

/// Decompress data produced by C++ `Compressor::EncodeAndClose`.
///
/// For compressed types, the data is prefixed with `varint64(uncompressed_size)`
/// followed by compressed bytes. For `CompressionType::None`, the data is the
/// raw bytes with no prefix.
///
/// This matches the C++ `EncodeAndClose` format used by bucket data and
/// transition data in transpose chunks.
pub(crate) fn decompress_with_prefix(
    data: &[u8],
    compression: CompressionType,
) -> Result<Vec<u8>, RiegeliError> {
    if compression == CompressionType::None {
        return Ok(data.to_vec());
    }
    // Strip the varint64(uncompressed_size) prefix, then hold the stream to
    // it: decompression stops (and errors) one byte past the claim instead
    // of materializing whatever the stream expands to, and the result must
    // match the claim exactly — downstream bucket/buffer splitting indexes
    // by these declared sizes.
    let (uncompressed_size, consumed) = decode_u64(data).map_err(|e| {
        RiegeliError::MalformedData(format!("reading uncompressed_size prefix: {e}").into())
    })?;
    let out = decompress_data_capped(&data[consumed..], compression, uncompressed_size)?;
    if out.len() as u64 != uncompressed_size {
        return Err(RiegeliError::MalformedData(
            format!(
                "decompressed size {} != declared {uncompressed_size}",
                out.len()
            )
            .into(),
        ));
    }
    Ok(out)
}

/// Decompress with a hard output cap: reading stops one byte past
/// `max_len` and errors, so a decompression bomb can materialize at most
/// `max_len + 1` bytes no matter what the stream would expand to.
pub(crate) fn decompress_data_capped(
    data: &[u8],
    compression: CompressionType,
    max_len: u64,
) -> Result<Vec<u8>, RiegeliError> {
    let check = |out: Vec<u8>| {
        if out.len() as u64 > max_len {
            Err(RiegeliError::MalformedData(
                format!("decompressed data exceeds its declared size ({max_len} bytes)").into(),
            ))
        } else {
            Ok(out)
        }
    };
    match compression {
        CompressionType::None => check(data.to_vec()),
        CompressionType::Brotli => {
            #[cfg(feature = "brotli")]
            {
                decompress_brotli(data, max_len)
            }
            #[cfg(not(feature = "brotli"))]
            {
                Err(RiegeliError::UnsupportedCompression(
                    CompressionType::Brotli as u8,
                ))
            }
        }
        CompressionType::Zstd => {
            #[cfg(feature = "zstd")]
            {
                decompress_zstd(data, max_len)
            }
            #[cfg(not(feature = "zstd"))]
            {
                Err(RiegeliError::UnsupportedCompression(
                    CompressionType::Zstd as u8,
                ))
            }
        }
        CompressionType::Snappy => {
            #[cfg(feature = "snappy")]
            {
                // Snappy frames declare their decompressed length up front;
                // reject before allocating rather than after.
                let declared = snap::raw::decompress_len(data).map_err(|e| {
                    RiegeliError::MalformedData(format!("snappy length error: {e}").into())
                })?;
                if declared as u64 > max_len {
                    return Err(RiegeliError::MalformedData(
                        format!("decompressed data exceeds its declared size ({max_len} bytes)")
                            .into(),
                    ));
                }
                decompress_snappy(data)
            }
            #[cfg(not(feature = "snappy"))]
            {
                Err(RiegeliError::UnsupportedCompression(
                    CompressionType::Snappy as u8,
                ))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(unused_imports)]
    use super::*;

    const INPUT: &[u8] = b"hello world hello world hello world";

    // C++ `Decompressor::VerifyEndAndClose` requires the source to end exactly
    // where the compressed stream ends; bytes left over after the stream make
    // the chunk invalid. The tests below pin that accept/reject behavior.

    #[test]
    #[cfg(feature = "brotli")]
    fn brotli_round_trip() {
        let compressed = compress_brotli(INPUT, CompressOptions::default()).unwrap();
        let out = decompress_data_capped(&compressed, CompressionType::Brotli, 1 << 20).unwrap();
        assert_eq!(out, INPUT);
    }

    #[test]
    #[cfg(feature = "brotli")]
    fn brotli_trailing_garbage_rejected() {
        let mut compressed = compress_brotli(INPUT, CompressOptions::default()).unwrap();
        compressed.extend_from_slice(&[0xde, 0xad, 0xbe, 0xef]);
        let result = decompress_data_capped(&compressed, CompressionType::Brotli, 1 << 20);
        assert!(
            result.is_err(),
            "trailing bytes after the Brotli stream must be rejected, got {result:?}"
        );
    }

    #[test]
    #[cfg(feature = "brotli")]
    fn brotli_truncated_stream_rejected() {
        let compressed = compress_brotli(INPUT, CompressOptions::default()).unwrap();
        let truncated = &compressed[..compressed.len() - 1];
        let result = decompress_data_capped(truncated, CompressionType::Brotli, 1 << 20);
        assert!(
            result.is_err(),
            "truncated Brotli stream must be rejected, got {result:?}"
        );
    }

    #[test]
    #[cfg(feature = "zstd")]
    fn zstd_round_trip() {
        let compressed = compress_zstd(INPUT, CompressOptions::default()).unwrap();
        let out = decompress_data_capped(&compressed, CompressionType::Zstd, 1 << 20).unwrap();
        assert_eq!(out, INPUT);
    }

    #[test]
    #[cfg(feature = "zstd")]
    fn zstd_trailing_garbage_rejected() {
        let mut compressed = compress_zstd(INPUT, CompressOptions::default()).unwrap();
        compressed.extend_from_slice(&[0xde, 0xad, 0xbe, 0xef]);
        let result = decompress_data_capped(&compressed, CompressionType::Zstd, 1 << 20);
        assert!(
            result.is_err(),
            "trailing bytes after the Zstd frame must be rejected, got {result:?}"
        );
    }

    #[test]
    #[cfg(feature = "zstd")]
    fn zstd_second_frame_rejected() {
        // C++ ZstdReader does not concatenate frames: it stops at the end of
        // the first frame and VerifyEnd rejects the leftover bytes, even if
        // they form another valid frame.
        let mut compressed = compress_zstd(INPUT, CompressOptions::default()).unwrap();
        let empty_frame = compress_zstd(b"", CompressOptions::default()).unwrap();
        compressed.extend_from_slice(&empty_frame);
        let result = decompress_data_capped(&compressed, CompressionType::Zstd, 1 << 20);
        assert!(
            result.is_err(),
            "a second Zstd frame after the first must be rejected, got {result:?}"
        );
    }

    #[test]
    #[cfg(feature = "snappy")]
    fn snappy_round_trip() {
        let compressed = compress_snappy(INPUT).unwrap();
        let out = decompress_data_capped(&compressed, CompressionType::Snappy, 1 << 20).unwrap();
        assert_eq!(out, INPUT);
    }

    #[test]
    #[cfg(feature = "snappy")]
    fn snappy_trailing_garbage_rejected() {
        let mut compressed = compress_snappy(INPUT).unwrap();
        compressed.extend_from_slice(&[0xde, 0xad, 0xbe, 0xef]);
        let result = decompress_data_capped(&compressed, CompressionType::Snappy, 1 << 20);
        assert!(
            result.is_err(),
            "trailing bytes after the Snappy data must be rejected, got {result:?}"
        );
    }
}
