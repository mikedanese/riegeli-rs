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
        writer
            .write_all(input)
            .map_err(|e| RiegeliError::MalformedData(format!("brotli compress error: {e}").into()))?;
    }
    Ok(output)
}

/// Decompress Brotli bytes.
#[cfg(feature = "brotli")]
pub(crate) fn decompress_brotli(
    input: &[u8],
    expected_len: usize,
) -> Result<Vec<u8>, RiegeliError> {
    use std::io::Read as _;
    let mut output = Vec::with_capacity(expected_len);
    let mut reader = brotli::Decompressor::new(input, 4096);
    reader
        .read_to_end(&mut output)
        .map_err(|e| RiegeliError::MalformedData(format!("brotli decompress error: {e}").into()))?;
    Ok(output)
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
            let mut encoder = zstd::Encoder::new(&mut output, level)
                .map_err(|e| RiegeliError::MalformedData(format!("zstd encoder init: {e}").into()))?;
            encoder
                .window_log(wlog)
                .map_err(|e| RiegeliError::MalformedData(format!("zstd window_log: {e}").into()))?;
            encoder
                .write_all(input)
                .map_err(|e| RiegeliError::MalformedData(format!("zstd compress error: {e}").into()))?;
            encoder
                .finish()
                .map_err(|e| RiegeliError::MalformedData(format!("zstd finish error: {e}").into()))?;
        }
        output
    } else {
        zstd::encode_all(input, level)
            .map_err(|e| RiegeliError::MalformedData(format!("zstd compress error: {e}").into()))?
    };
    Ok(compressed)
}

/// Decompress Zstd bytes.
#[cfg(feature = "zstd")]
pub(crate) fn decompress_zstd(input: &[u8]) -> Result<Vec<u8>, RiegeliError> {
    zstd::decode_all(input)
        .map_err(|e| RiegeliError::MalformedData(format!("zstd decompress error: {e}").into()))
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
    // Strip the varint64(uncompressed_size) prefix.
    let (_uncompressed_size, consumed) = decode_u64(data).map_err(|e| {
        RiegeliError::MalformedData(format!("reading uncompressed_size prefix: {e}").into())
    })?;
    decompress_raw(&data[consumed..], compression)
}

/// Decompress raw compressed bytes (no varint prefix).
///
/// For `CompressionType::None`, the data is returned as-is (copied).
pub(crate) fn decompress_raw(
    data: &[u8],
    compression: CompressionType,
) -> Result<Vec<u8>, RiegeliError> {
    match compression {
        CompressionType::None => Ok(data.to_vec()),
        _ => decompress_data(data, compression),
    }
}

/// Decompress data using the specified compression type.
///
/// This is the shared decompression dispatch used by both the simple and transpose
/// chunk decoders. For `CompressionType::None`, the data is returned as-is (copied).
pub(crate) fn decompress_data(
    data: &[u8],
    compression: CompressionType,
) -> Result<Vec<u8>, RiegeliError> {
    match compression {
        CompressionType::None => Ok(data.to_vec()),
        CompressionType::Brotli => {
            #[cfg(feature = "brotli")]
            {
                decompress_brotli(data, data.len() * 4)
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
                decompress_zstd(data)
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
