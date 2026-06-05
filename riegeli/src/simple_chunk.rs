//! Simple chunk encode and decode for all compression types.
//!
//! The wire format matches the C++ riegeli reference implementation exactly.
//!
//! Uncompressed data layout:
//! ```text
//! byte 0:       compression_type = 0x00
//! bytes 1..A:   varint64(sizes_byte_length)           -- byte count of sizes section
//! bytes A..B:   sizes section (array of varint64 record lengths)
//! bytes B..:    values section (concatenated record bytes)
//! ```
//!
//! Compressed data layout:
//! ```text
//! byte 0:       compression_type byte (b'b'=Brotli, b'z'=Zstd, b's'=Snappy)
//! bytes 1..A:   varint64(compressed_sizes_len)        -- byte count of the sizes blob below
//! bytes A..B:   sizes blob:
//!                 varint64(uncompressed_sizes_len)     -- decompressed byte count
//!                 compressed sizes data
//! bytes B..:    values blob:
//!                 varint64(uncompressed_values_len)    -- decompressed byte count
//!                 compressed values data
//! ```

use crate::chunk_header::{ChunkHeader, ChunkType};
#[cfg(any(feature = "brotli", feature = "zstd", feature = "snappy"))]
use crate::compression::decompress_data;
use crate::compression::{CompressOptions, CompressionType};
use crate::error::RiegeliError;
use crate::varint::{decode_u64, encode_u64};

/// A Riegeli chunk: a header plus raw data bytes.
#[derive(Debug, Clone)]
pub struct Chunk {
    /// The 40-byte chunk header.
    pub header: ChunkHeader,
    /// The raw chunk data bytes (length == `header.data_size()`).
    pub data: Vec<u8>,
}

/// Encoder that accumulates records and produces a simple chunk.
pub struct SimpleChunkEncoder {
    records: Vec<Vec<u8>>,
    compression: CompressionType,
    /// Compression tuning options (level, window_log).
    compress_opts: CompressOptions,
}

impl SimpleChunkEncoder {
    /// Create a new encoder using `CompressionType::None`.
    pub fn new() -> Self {
        Self {
            records: Vec::new(),
            compression: CompressionType::None,
            compress_opts: CompressOptions::default(),
        }
    }

    /// Create a new encoder with the specified compression type.
    pub fn with_compression(compression: CompressionType) -> Self {
        Self {
            records: Vec::new(),
            compression,
            compress_opts: CompressOptions::default(),
        }
    }

    /// Create a new encoder with the specified compression type and tuning options.
    pub fn with_options(compression: CompressionType, compress_opts: CompressOptions) -> Self {
        Self {
            records: Vec::new(),
            compression,
            compress_opts,
        }
    }

    /// Accumulate a record to be included in the next `encode()` call.
    pub fn add_record(&mut self, data: &[u8]) {
        self.records.push(data.to_vec());
    }

    /// Produce the encoded `Chunk`.
    pub fn encode(self) -> Result<Chunk, RiegeliError> {
        let num_records = self.records.len() as u64;
        let decoded_data_size: u64 = self.records.iter().map(|r| r.len() as u64).sum();

        let data = match self.compression {
            CompressionType::None => {
                // Build the sizes section first to measure its byte length.
                let mut sizes_section: Vec<u8> = Vec::new();
                for record in &self.records {
                    sizes_section.extend_from_slice(&encode_u64(record.len() as u64));
                }

                let mut data: Vec<u8> = Vec::new();
                data.push(0x00);
                // Length-prefix the sizes section (C++ LengthPrefixedEncodeAndClose
                // writes varint64(compressed_size) which for uncompressed = raw size).
                data.extend_from_slice(&encode_u64(sizes_section.len() as u64));
                data.extend_from_slice(&sizes_section);
                for record in &self.records {
                    data.extend_from_slice(record);
                }
                data
            }
            CompressionType::Brotli => {
                #[cfg(feature = "brotli")]
                {
                    let opts = self.compress_opts;
                    encode_compressed(&self.records, CompressionType::Brotli, |b| {
                        crate::compression::compress_brotli(b, opts)
                    })?
                }
                #[cfg(not(feature = "brotli"))]
                {
                    return Err(RiegeliError::UnsupportedCompression(
                        CompressionType::Brotli as u8,
                    ));
                }
            }
            CompressionType::Zstd => {
                #[cfg(feature = "zstd")]
                {
                    let opts = self.compress_opts;
                    encode_compressed(&self.records, CompressionType::Zstd, |b| {
                        crate::compression::compress_zstd(b, opts)
                    })?
                }
                #[cfg(not(feature = "zstd"))]
                {
                    return Err(RiegeliError::UnsupportedCompression(
                        CompressionType::Zstd as u8,
                    ));
                }
            }
            CompressionType::Snappy => {
                #[cfg(feature = "snappy")]
                {
                    encode_compressed(&self.records, CompressionType::Snappy, |b| {
                        crate::compression::compress_snappy(b)
                    })?
                }
                #[cfg(not(feature = "snappy"))]
                {
                    return Err(RiegeliError::UnsupportedCompression(
                        CompressionType::Snappy as u8,
                    ));
                }
            }
        };

        let header =
            ChunkHeader::from_parts(&data, ChunkType::Simple, num_records, decoded_data_size);

        Ok(Chunk { header, data })
    }
}

/// Helper: build compressed chunk data for a given compression function.
///
/// Matches the C++ format exactly:
/// - `LengthPrefixedEncodeAndClose` for sizes: writes varint64(compressed_sizes_len)
///   where compressed_sizes_len = varint_len(uncompressed_sizes_len) + compressed_data_len,
///   then varint64(uncompressed_sizes_len), then compressed sizes data.
/// - `EncodeAndClose` for values: writes varint64(uncompressed_values_len),
///   then compressed values data.
#[cfg(any(feature = "brotli", feature = "zstd", feature = "snappy"))]
fn encode_compressed<F>(
    records: &[Vec<u8>],
    compression: CompressionType,
    compress: F,
) -> Result<Vec<u8>, RiegeliError>
where
    F: Fn(&[u8]) -> Result<Vec<u8>, RiegeliError>,
{
    use crate::varint::length_varint_u64;

    // Build raw sizes section: varint64 per record length
    let mut sizes_section: Vec<u8> = Vec::new();
    for record in records {
        sizes_section.extend_from_slice(&encode_u64(record.len() as u64));
    }

    // Build raw values section: concatenated record bytes
    let mut values_section: Vec<u8> = Vec::new();
    for record in records {
        values_section.extend_from_slice(record);
    }

    let uncompressed_sizes_len = sizes_section.len() as u64;
    let uncompressed_values_len = values_section.len() as u64;

    let compressed_sizes = compress(&sizes_section)?;
    let compressed_values = compress(&values_section)?;

    // LengthPrefixed format: the "compressed_size" includes the
    // varint(uncompressed_size) prefix that precedes the actual compressed data.
    let uncompressed_sizes_varint_len = length_varint_u64(uncompressed_sizes_len);
    let total_sizes_blob_len = uncompressed_sizes_varint_len as u64 + compressed_sizes.len() as u64;

    let mut data: Vec<u8> = Vec::new();
    data.push(compression as u8);

    // Sizes section: length-prefixed
    data.extend_from_slice(&encode_u64(total_sizes_blob_len));
    data.extend_from_slice(&encode_u64(uncompressed_sizes_len));
    data.extend_from_slice(&compressed_sizes);

    // Values section: uncompressed length prefix + compressed data
    data.extend_from_slice(&encode_u64(uncompressed_values_len));
    data.extend_from_slice(&compressed_values);

    Ok(data)
}

impl Default for SimpleChunkEncoder {
    fn default() -> Self {
        Self::new()
    }
}

/// Decoder that reads records one at a time from a simple chunk (any compression type).
#[derive(Debug)]
pub struct SimpleChunkDecoder {
    /// Record boundaries as (offset, length) pairs into the values section.
    record_ranges: Vec<(usize, usize)>,
    /// The (decompressed) values section bytes.
    values: Vec<u8>,
    /// Index of the next record to yield.
    next_record: usize,
}

impl SimpleChunkDecoder {
    /// Construct a decoder from a `Chunk`, validating the data hash before parsing.
    ///
    /// Returns `Err(RiegeliError::DataHashMismatch)` if the stored hash does not match the data.
    /// Returns `Err(RiegeliError::UnsupportedCompression)` for unknown compression type bytes.
    pub fn new(chunk: Chunk) -> Result<Self, RiegeliError> {
        // Validate hash before doing anything else.
        if !chunk.header.is_data_valid(&chunk.data) {
            return Err(RiegeliError::DataHashMismatch);
        }

        let data = &chunk.data;
        let num_records = chunk.header.num_records() as usize;
        let _decoded_data_size = chunk.header.decoded_data_size() as usize;

        // Must have at least 1 byte for compression type.
        if data.is_empty() {
            return Err(RiegeliError::MalformedData(
                "chunk data is empty".into(),
            ));
        }

        let compression_byte = data[0];

        match compression_byte {
            0x00 => {
                // Uncompressed path
                decode_uncompressed(&data[1..], num_records)
            }
            b'b' => {
                // Brotli
                #[cfg(feature = "brotli")]
                {
                    decode_compressed(&data[1..], num_records, CompressionType::Brotli)
                }
                #[cfg(not(feature = "brotli"))]
                {
                    Err(RiegeliError::UnsupportedCompression(b'b'))
                }
            }
            b'z' => {
                // Zstd
                #[cfg(feature = "zstd")]
                {
                    decode_compressed(&data[1..], num_records, CompressionType::Zstd)
                }
                #[cfg(not(feature = "zstd"))]
                {
                    Err(RiegeliError::UnsupportedCompression(b'z'))
                }
            }
            b's' => {
                // Snappy
                #[cfg(feature = "snappy")]
                {
                    decode_compressed(&data[1..], num_records, CompressionType::Snappy)
                }
                #[cfg(not(feature = "snappy"))]
                {
                    Err(RiegeliError::UnsupportedCompression(b's'))
                }
            }
            other => Err(RiegeliError::UnsupportedCompression(other)),
        }
    }

    /// Read the next record, returning `Ok(None)` when all records have been consumed.
    pub fn read_record(&mut self) -> Result<Option<Vec<u8>>, RiegeliError> {
        if self.next_record >= self.record_ranges.len() {
            return Ok(None);
        }
        let (offset, len) = self.record_ranges[self.next_record];
        self.next_record += 1;
        Ok(Some(self.values[offset..offset + len].to_vec()))
    }
}

/// Decode an uncompressed payload (bytes after the compression_type byte).
///
/// Format: varint64(sizes_byte_length), sizes varints, values data.
fn decode_uncompressed(
    payload: &[u8],
    num_records: usize,
) -> Result<SimpleChunkDecoder, RiegeliError> {
    // Read the sizes section byte length (LengthPrefixed format).
    let (sizes_byte_len, varint_consumed) = decode_u64(payload).map_err(|e| {
        RiegeliError::MalformedData(format!("failed to read sizes_byte_length: {e}").into())
    })?;
    let sizes_byte_len = sizes_byte_len as usize;

    let sizes_start = varint_consumed;
    if sizes_start + sizes_byte_len > payload.len() {
        return Err(RiegeliError::MalformedData(format!(
            "sizes section truncated: need {sizes_byte_len} bytes starting at offset {sizes_start}, \
             but payload is only {} bytes",
            payload.len()
        ).into()));
    }

    let sizes_data = &payload[sizes_start..sizes_start + sizes_byte_len];
    let values_start = sizes_start + sizes_byte_len;

    // Parse individual record sizes from the sizes section.
    let mut pos = 0usize;
    let mut sizes: Vec<usize> = Vec::with_capacity(num_records);
    for i in 0..num_records {
        if pos >= sizes_data.len() {
            return Err(RiegeliError::MalformedData(format!(
                "unexpected end of sizes section at record {i}"
            ).into()));
        }
        let (size, consumed) = decode_u64(&sizes_data[pos..]).map_err(|e| {
            RiegeliError::MalformedData(format!("varint decode error at record {i}: {e}").into())
        })?;
        pos += consumed;
        sizes.push(size as usize);
    }

    let total_values_len: usize = sizes.iter().sum();
    if values_start + total_values_len > payload.len() {
        return Err(RiegeliError::MalformedData(format!(
            "values section truncated: need {total_values_len} bytes but only {} available",
            payload.len() - values_start
        ).into()));
    }

    let mut record_ranges: Vec<(usize, usize)> = Vec::with_capacity(num_records);
    let mut offset = 0usize;
    for size in &sizes {
        record_ranges.push((offset, *size));
        offset += size;
    }

    let values = payload[values_start..values_start + total_values_len].to_vec();

    Ok(SimpleChunkDecoder {
        record_ranges,
        values,
        next_record: 0,
    })
}

/// Decode a compressed payload (bytes after the compression_type byte).
///
/// C++ format:
/// - varint64(sizes_blob_len): byte count of the sizes blob below
/// - sizes blob: varint64(uncompressed_sizes_len), compressed sizes data
/// - values blob: varint64(uncompressed_values_len), compressed values data
#[cfg(any(feature = "brotli", feature = "zstd", feature = "snappy"))]
fn decode_compressed(
    payload: &[u8],
    num_records: usize,
    compression: CompressionType,
) -> Result<SimpleChunkDecoder, RiegeliError> {
    if payload.is_empty() {
        return Err(RiegeliError::MalformedData(
            "compressed payload is empty".into(),
        ));
    }

    let mut pos = 0usize;

    // Read varint64(sizes_blob_len) -- the LengthPrefixed total byte count
    let (sizes_blob_len, consumed) = decode_u64(&payload[pos..])
        .map_err(|e| RiegeliError::MalformedData(format!("failed to read sizes_blob_len: {e}").into()))?;
    pos += consumed;
    let sizes_blob_len = sizes_blob_len as usize;

    if pos + sizes_blob_len > payload.len() {
        return Err(RiegeliError::MalformedData(format!(
            "sizes blob truncated: need {sizes_blob_len} bytes at offset {pos}, \
             payload is {} bytes",
            payload.len()
        ).into()));
    }

    let sizes_blob = &payload[pos..pos + sizes_blob_len];
    pos += sizes_blob_len;

    // Parse sizes blob: varint64(uncompressed_sizes_len), compressed sizes data
    let (uncompressed_sizes_len, consumed2) = decode_u64(sizes_blob).map_err(|e| {
        RiegeliError::MalformedData(format!("failed to read uncompressed_sizes_len: {e}").into())
    })?;
    let uncompressed_sizes_len = uncompressed_sizes_len as usize;
    let compressed_sizes = &sizes_blob[consumed2..];

    let sizes_bytes = decompress_data(compressed_sizes, compression)?;
    if sizes_bytes.len() != uncompressed_sizes_len {
        return Err(RiegeliError::MalformedData(format!(
            "decompressed sizes length {} != expected {}",
            sizes_bytes.len(),
            uncompressed_sizes_len
        ).into()));
    }

    // Parse values blob: varint64(uncompressed_values_len), compressed values data
    let values_blob = &payload[pos..];
    let (uncompressed_values_len, consumed3) = decode_u64(values_blob).map_err(|e| {
        RiegeliError::MalformedData(format!("failed to read uncompressed_values_len: {e}").into())
    })?;
    let _uncompressed_values_len = uncompressed_values_len as usize;
    let compressed_values = &values_blob[consumed3..];

    let values_bytes = decompress_data(compressed_values, compression)?;

    // Parse sizes section
    let mut spos = 0usize;
    let mut sizes: Vec<usize> = Vec::with_capacity(num_records);
    for i in 0..num_records {
        if spos >= sizes_bytes.len() {
            return Err(RiegeliError::MalformedData(format!(
                "unexpected end of decompressed sizes at record {i}"
            ).into()));
        }
        let (size, consumed) = decode_u64(&sizes_bytes[spos..]).map_err(|e| {
            RiegeliError::MalformedData(format!("varint decode in sizes at record {i}: {e}").into())
        })?;
        spos += consumed;
        sizes.push(size as usize);
    }

    // Verify total values length
    let total_values_len: usize = sizes.iter().sum();
    if total_values_len != values_bytes.len() {
        return Err(RiegeliError::MalformedData(format!(
            "values length mismatch: sizes sum {total_values_len} != decompressed values {}",
            values_bytes.len()
        ).into()));
    }

    let mut record_ranges: Vec<(usize, usize)> = Vec::with_capacity(num_records);
    let mut offset = 0usize;
    for size in &sizes {
        record_ranges.push((offset, *size));
        offset += size;
    }

    Ok(SimpleChunkDecoder {
        record_ranges,
        values: values_bytes,
        next_record: 0,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hash::highway_hash_64;

    // -------------------------------------------------------------------------
    // Sprint 3 regression tests (uncompressed)
    // -------------------------------------------------------------------------

    #[test]
    fn encode_zero_records() {
        let encoder = SimpleChunkEncoder::new();
        let chunk = encoder.encode().expect("encode ok");
        assert_eq!(chunk.header.num_records(), 0);
        assert!(
            chunk.header.data_size() > 0,
            "data_size must be > 0 (has compression byte)"
        );
        assert_eq!(chunk.header.decoded_data_size(), 0);
    }

    #[test]
    fn encode_decode_hello() {
        let mut encoder = SimpleChunkEncoder::new();
        encoder.add_record(b"hello");
        let chunk = encoder.encode().expect("encode ok");

        let mut decoder = SimpleChunkDecoder::new(chunk).expect("valid chunk");
        let record = decoder
            .read_record()
            .expect("no error")
            .expect("has record");
        assert_eq!(record, b"hello");
        assert!(decoder.read_record().expect("no error").is_none());
    }

    #[test]
    fn encode_decode_three_records() {
        let records: &[&[u8]] = &[b"alpha", b"bb", b"ccccc"];
        let mut encoder = SimpleChunkEncoder::new();
        for r in records {
            encoder.add_record(r);
        }
        let chunk = encoder.encode().expect("encode ok");

        let mut decoder = SimpleChunkDecoder::new(chunk).expect("valid chunk");
        for expected in records {
            let got = decoder
                .read_record()
                .expect("no error")
                .expect("has record");
            assert_eq!(got.as_slice(), *expected);
        }
        assert!(decoder.read_record().expect("no error").is_none());
    }

    #[test]
    fn data_hash_matches() {
        let mut encoder = SimpleChunkEncoder::new();
        encoder.add_record(b"hello");
        let chunk = encoder.encode().expect("encode ok");
        assert_eq!(chunk.header.data_hash(), highway_hash_64(&chunk.data));
    }

    #[test]
    fn header_hash_valid() {
        let mut encoder = SimpleChunkEncoder::new();
        encoder.add_record(b"test record");
        let chunk = encoder.encode().expect("encode ok");
        assert!(chunk.header.is_header_valid());
    }

    #[test]
    fn round_trip_no_corruption() {
        let mut encoder = SimpleChunkEncoder::new();
        encoder.add_record(b"foo");
        encoder.add_record(b"bar");
        let chunk = encoder.encode().expect("encode ok");
        assert!(SimpleChunkDecoder::new(chunk).is_ok());
    }

    #[test]
    fn corrupted_data_hash_returns_err() {
        let mut encoder = SimpleChunkEncoder::new();
        encoder.add_record(b"hello");
        let mut chunk = encoder.encode().expect("encode ok");

        let mut bytes = chunk.header.to_bytes();
        bytes[16] ^= 0xff;
        chunk.header = ChunkHeader::from_bytes(bytes);

        let result = SimpleChunkDecoder::new(chunk);
        assert!(matches!(result, Err(RiegeliError::DataHashMismatch)));
    }

    #[test]
    fn exact_byte_layout_hello() {
        let mut encoder = SimpleChunkEncoder::new();
        encoder.add_record(b"hello");
        let chunk = encoder.encode().expect("encode ok");

        // C++ format: 0x00 (compression), varint(1) = sizes_byte_len, 0x05 (size of "hello"), "hello"
        let expected: &[u8] = &[0x00, 0x01, 0x05, b'h', b'e', b'l', b'l', b'o'];
        assert_eq!(chunk.data, expected);
    }

    // -------------------------------------------------------------------------
    // Sprint 4: compressed tests
    // -------------------------------------------------------------------------

    // Criterion 4.1: Brotli chunk has first data byte == b'b'
    #[test]
    #[cfg(feature = "brotli")]
    fn brotli_first_byte_is_b() {
        let mut encoder = SimpleChunkEncoder::with_compression(CompressionType::Brotli);
        encoder.add_record(b"hello brotli");
        let chunk = encoder.encode().expect("encode ok");
        assert_eq!(chunk.data[0], b'b', "first data byte must be b'b' (0x62)");
    }

    // Criterion 4.2: round-trip with Brotli
    #[test]
    #[cfg(feature = "brotli")]
    fn brotli_round_trip_single_record() {
        let input = b"hello compressed world";
        let mut encoder = SimpleChunkEncoder::with_compression(CompressionType::Brotli);
        encoder.add_record(input);
        let chunk = encoder.encode().expect("encode ok");

        let mut decoder = SimpleChunkDecoder::new(chunk).expect("valid chunk");
        let got = decoder
            .read_record()
            .expect("no error")
            .expect("has record");
        assert_eq!(got, input);
        assert!(decoder.read_record().expect("no error").is_none());
    }

    // Criterion 4.2: round-trip with Zstd
    #[test]
    #[cfg(feature = "zstd")]
    fn zstd_round_trip_single_record() {
        let input = b"hello zstd world";
        let mut encoder = SimpleChunkEncoder::with_compression(CompressionType::Zstd);
        encoder.add_record(input);
        let chunk = encoder.encode().expect("encode ok");

        let mut decoder = SimpleChunkDecoder::new(chunk).expect("valid chunk");
        let got = decoder
            .read_record()
            .expect("no error")
            .expect("has record");
        assert_eq!(got, input);
        assert!(decoder.read_record().expect("no error").is_none());
    }

    // Criterion 4.2: round-trip with Snappy
    #[test]
    #[cfg(feature = "snappy")]
    fn snappy_round_trip_single_record() {
        let input = b"hello snappy world";
        let mut encoder = SimpleChunkEncoder::with_compression(CompressionType::Snappy);
        encoder.add_record(input);
        let chunk = encoder.encode().expect("encode ok");

        let mut decoder = SimpleChunkDecoder::new(chunk).expect("valid chunk");
        let got = decoder
            .read_record()
            .expect("no error")
            .expect("has record");
        assert_eq!(got, input);
        assert!(decoder.read_record().expect("no error").is_none());
    }

    // Criterion 4.3: compressed sizes section has the C++ format:
    // varint(sizes_blob_len), varint(uncompressed_sizes_len), compressed_sizes, ...
    #[test]
    #[cfg(feature = "brotli")]
    fn brotli_sizes_section_has_varint_prefix() {
        // Encode a single record of known size so we know what sizes_section looks like.
        // For one record b"hello" (5 bytes): sizes_section = encode_u64(5) = [0x05]
        // uncompressed_sizes_len = 1
        let mut encoder = SimpleChunkEncoder::with_compression(CompressionType::Brotli);
        encoder.add_record(b"hello");
        let chunk = encoder.encode().expect("encode ok");

        // data[0] = b'b'
        // data[1..]: varint64(sizes_blob_len) -- the LengthPrefixed total
        // Then inside the blob: varint64(uncompressed_sizes_len), compressed_sizes
        let data = &chunk.data;
        assert_eq!(data[0], b'b');
        let (sizes_blob_len, vlen1) = decode_u64(&data[1..]).expect("varint decode ok");
        // The blob contains varint(1) = [0x01] + compressed sizes bytes.
        let blob = &data[1 + vlen1..1 + vlen1 + sizes_blob_len as usize];
        let (uncompressed_sizes_len, _) = decode_u64(blob).expect("varint decode ok");
        assert_eq!(
            uncompressed_sizes_len, 1u64,
            "sizes_section for 1 record of any size is 1 varint byte for 5 bytes"
        );
    }

    // Criterion 4.3: three records -- check the sizes blob structure
    #[test]
    #[cfg(feature = "brotli")]
    fn brotli_sizes_section_prefix_three_records() {
        // Three records with lengths 5, 2, 5 -> sizes_section = [0x05, 0x02, 0x05] = 3 bytes
        let mut encoder = SimpleChunkEncoder::with_compression(CompressionType::Brotli);
        encoder.add_record(b"hello");
        encoder.add_record(b"bb");
        encoder.add_record(b"world");
        let chunk = encoder.encode().expect("encode ok");

        let data = &chunk.data;
        let (sizes_blob_len, vlen1) = decode_u64(&data[1..]).expect("varint decode ok");
        let blob = &data[1 + vlen1..1 + vlen1 + sizes_blob_len as usize];
        let (uncompressed_sizes_len, _) = decode_u64(blob).expect("varint decode ok");
        assert_eq!(
            uncompressed_sizes_len, 3u64,
            "three records each needing 1 varint byte = 3 bytes"
        );
    }

    // Criterion 4.4: 1000 records × 1 KiB with Brotli is smaller than uncompressed
    #[test]
    #[cfg(feature = "brotli")]
    fn brotli_compression_actually_compresses() {
        // Use repetitive data so Brotli can compress it well
        let record: Vec<u8> = b"AAAAAAAAAA".iter().cycle().take(1024).cloned().collect();

        let mut enc_compressed = SimpleChunkEncoder::with_compression(CompressionType::Brotli);
        let mut enc_none = SimpleChunkEncoder::new();
        for _ in 0..1000 {
            enc_compressed.add_record(&record);
            enc_none.add_record(&record);
        }

        let compressed_chunk = enc_compressed.encode().expect("encode ok");
        let uncompressed_chunk = enc_none.encode().expect("encode ok");

        assert!(
            compressed_chunk.data.len() < uncompressed_chunk.data.len(),
            "compressed={} should be < uncompressed={}",
            compressed_chunk.data.len(),
            uncompressed_chunk.data.len()
        );
    }

    // Criterion 4.7: unknown compression byte returns Err, not panic
    #[test]
    fn unsupported_compression_byte_returns_err() {
        // Build a chunk with compression byte 0xFF by crafting raw data
        // and building the header from scratch.
        let data: Vec<u8> = vec![0xFF, 0x00]; // compression=0xFF, then garbage
        let header = ChunkHeader::from_parts(&data, ChunkType::Simple, 0, 0);
        let chunk = Chunk { header, data };
        let result = SimpleChunkDecoder::new(chunk);
        assert!(
            matches!(result, Err(RiegeliError::UnsupportedCompression(0xFF))),
            "expected UnsupportedCompression(0xFF), got: {result:?}"
        );
    }

    // Criterion 4.8: decoded_data_size == sum of uncompressed record lengths for all compression types
    #[test]
    #[cfg(feature = "brotli")]
    fn decoded_data_size_brotli() {
        let records: &[&[u8]] = &[b"hello", b"world", b"foo"];
        let expected_sum: u64 = records.iter().map(|r| r.len() as u64).sum();

        let mut encoder = SimpleChunkEncoder::with_compression(CompressionType::Brotli);
        for r in records {
            encoder.add_record(r);
        }
        let chunk = encoder.encode().expect("encode ok");
        assert_eq!(chunk.header.decoded_data_size(), expected_sum);
    }

    #[test]
    #[cfg(feature = "zstd")]
    fn decoded_data_size_zstd() {
        let records: &[&[u8]] = &[b"hello", b"world", b"foo"];
        let expected_sum: u64 = records.iter().map(|r| r.len() as u64).sum();

        let mut encoder = SimpleChunkEncoder::with_compression(CompressionType::Zstd);
        for r in records {
            encoder.add_record(r);
        }
        let chunk = encoder.encode().expect("encode ok");
        assert_eq!(chunk.header.decoded_data_size(), expected_sum);
    }

    #[test]
    fn decoded_data_size_none() {
        let records: &[&[u8]] = &[b"hello", b"world", b"foo"];
        let expected_sum: u64 = records.iter().map(|r| r.len() as u64).sum();

        let mut encoder = SimpleChunkEncoder::new();
        for r in records {
            encoder.add_record(r);
        }
        let chunk = encoder.encode().expect("encode ok");
        assert_eq!(chunk.header.decoded_data_size(), expected_sum);
    }

    // Round-trip multiple records with Brotli
    #[test]
    #[cfg(feature = "brotli")]
    fn brotli_round_trip_multiple_records() {
        let records: &[&[u8]] = &[b"alpha", b"beta", b"gamma delta epsilon"];
        let mut encoder = SimpleChunkEncoder::with_compression(CompressionType::Brotli);
        for r in records {
            encoder.add_record(r);
        }
        let chunk = encoder.encode().expect("encode ok");

        let mut decoder = SimpleChunkDecoder::new(chunk).expect("valid chunk");
        for expected in records {
            let got = decoder
                .read_record()
                .expect("no error")
                .expect("has record");
            assert_eq!(got.as_slice(), *expected);
        }
        assert!(decoder.read_record().expect("no error").is_none());
    }

    // Round-trip multiple records with Zstd
    #[test]
    #[cfg(feature = "zstd")]
    fn zstd_round_trip_multiple_records() {
        let records: &[&[u8]] = &[b"alpha", b"beta", b"gamma delta epsilon"];
        let mut encoder = SimpleChunkEncoder::with_compression(CompressionType::Zstd);
        for r in records {
            encoder.add_record(r);
        }
        let chunk = encoder.encode().expect("encode ok");

        let mut decoder = SimpleChunkDecoder::new(chunk).expect("valid chunk");
        for expected in records {
            let got = decoder
                .read_record()
                .expect("no error")
                .expect("has record");
            assert_eq!(got.as_slice(), *expected);
        }
        assert!(decoder.read_record().expect("no error").is_none());
    }

    // Zero records with Brotli
    #[test]
    #[cfg(feature = "brotli")]
    fn brotli_zero_records() {
        let encoder = SimpleChunkEncoder::with_compression(CompressionType::Brotli);
        let chunk = encoder.encode().expect("encode ok");
        assert_eq!(chunk.header.num_records(), 0);
        assert_eq!(chunk.header.decoded_data_size(), 0);
        let mut decoder = SimpleChunkDecoder::new(chunk).expect("valid chunk");
        assert!(decoder.read_record().expect("no error").is_none());
    }

    // Zero records with Zstd
    #[test]
    #[cfg(feature = "zstd")]
    fn zstd_zero_records() {
        let encoder = SimpleChunkEncoder::with_compression(CompressionType::Zstd);
        let chunk = encoder.encode().expect("encode ok");
        assert_eq!(chunk.header.num_records(), 0);
        assert_eq!(chunk.header.decoded_data_size(), 0);
        let mut decoder = SimpleChunkDecoder::new(chunk).expect("valid chunk");
        assert!(decoder.read_record().expect("no error").is_none());
    }

    // data_hash and header_hash still valid for compressed chunks
    #[test]
    #[cfg(feature = "brotli")]
    fn brotli_chunk_hashes_valid() {
        let mut encoder = SimpleChunkEncoder::with_compression(CompressionType::Brotli);
        encoder.add_record(b"test");
        let chunk = encoder.encode().expect("encode ok");
        assert_eq!(chunk.header.data_hash(), highway_hash_64(&chunk.data));
        assert!(chunk.header.is_header_valid());
    }

    // Adversarial tests carried over
    #[test]
    fn adversarial_1000_records() {
        let mut encoder = SimpleChunkEncoder::new();
        for i in 0u64..1000 {
            encoder.add_record(&i.to_le_bytes());
        }
        let chunk = encoder.encode().expect("encode ok");
        let mut decoder = SimpleChunkDecoder::new(chunk).expect("valid");
        for i in 0u64..1000 {
            let got = decoder.read_record().expect("ok").expect("record");
            assert_eq!(got, i.to_le_bytes());
        }
        assert!(decoder.read_record().expect("ok").is_none());
    }

    #[test]
    fn test_decode_zero_records() {
        let encoder = SimpleChunkEncoder::new();
        let chunk = encoder.encode().expect("encode ok");
        let mut decoder = SimpleChunkDecoder::new(chunk).expect("valid chunk");
        assert!(decoder.read_record().unwrap().is_none());
    }

    #[test]
    fn test_empty_record_roundtrip() {
        let mut encoder = SimpleChunkEncoder::new();
        encoder.add_record(b"");
        let chunk = encoder.encode().expect("encode ok");
        assert_eq!(chunk.header.num_records(), 1);
        assert_eq!(chunk.header.decoded_data_size(), 0);
        let mut decoder = SimpleChunkDecoder::new(chunk).expect("valid chunk");
        let record = decoder
            .read_record()
            .unwrap()
            .expect("should have one record");
        assert_eq!(record, b"");
        assert!(decoder.read_record().unwrap().is_none());
    }

    #[test]
    fn test_multiple_empty_records() {
        let mut encoder = SimpleChunkEncoder::new();
        for _ in 0..5 {
            encoder.add_record(b"");
        }
        let chunk = encoder.encode().expect("encode ok");
        assert_eq!(chunk.header.num_records(), 5);
        assert_eq!(chunk.header.decoded_data_size(), 0);
        let mut decoder = SimpleChunkDecoder::new(chunk).expect("valid chunk");
        for _ in 0..5 {
            let record = decoder.read_record().unwrap().expect("should have record");
            assert_eq!(record, b"");
        }
        assert!(decoder.read_record().unwrap().is_none());
    }

    #[test]
    fn test_varint_boundary_128_bytes() {
        let record_128 = vec![0xAB_u8; 128];
        let mut encoder = SimpleChunkEncoder::new();
        encoder.add_record(&record_128);
        let chunk = encoder.encode().expect("encode ok");
        // 128 encodes as 2-byte varint [0x80, 0x01]
        assert_eq!(chunk.data[2], 0x80);
        assert_eq!(chunk.data[3], 0x01);
        let mut decoder = SimpleChunkDecoder::new(chunk).expect("valid chunk");
        let result = decoder.read_record().unwrap().expect("should have record");
        assert_eq!(result, record_128);
        assert!(decoder.read_record().unwrap().is_none());
    }

    #[test]
    fn test_varint_boundary_mixed_sizes() {
        let record_127 = vec![0x01_u8; 127];
        let record_128 = vec![0x02_u8; 128];
        let mut encoder = SimpleChunkEncoder::new();
        encoder.add_record(&record_127);
        encoder.add_record(&record_128);
        let chunk = encoder.encode().expect("encode ok");
        let mut decoder = SimpleChunkDecoder::new(chunk).expect("valid chunk");
        assert_eq!(
            decoder.read_record().unwrap().expect("record 1"),
            record_127
        );
        assert_eq!(
            decoder.read_record().unwrap().expect("record 2"),
            record_128
        );
        assert!(decoder.read_record().unwrap().is_none());
    }

    #[test]
    fn test_bit_flip_in_data_detected() {
        let mut encoder = SimpleChunkEncoder::new();
        encoder.add_record(b"important data");
        let mut chunk = encoder.encode().expect("encode ok");
        let last = chunk.data.len() - 1;
        chunk.data[last] ^= 0x01;
        assert!(matches!(
            SimpleChunkDecoder::new(chunk),
            Err(RiegeliError::DataHashMismatch)
        ));
    }

    #[test]
    fn test_bit_flip_in_compression_byte_detected() {
        let mut encoder = SimpleChunkEncoder::new();
        encoder.add_record(b"test");
        let mut chunk = encoder.encode().expect("encode ok");
        chunk.data[0] ^= 0x01;
        assert!(matches!(
            SimpleChunkDecoder::new(chunk),
            Err(RiegeliError::DataHashMismatch)
        ));
    }

    #[test]
    fn test_truncated_data_returns_error() {
        let mut encoder = SimpleChunkEncoder::new();
        encoder.add_record(b"hello world, this is a longer record");
        let mut chunk = encoder.encode().expect("encode ok");
        let original_len = chunk.data.len();
        chunk.data.truncate(original_len / 2);
        assert!(SimpleChunkDecoder::new(chunk).is_err());
    }

    #[test]
    fn test_empty_data_returns_error() {
        use crate::chunk_header::ChunkType;
        let data: Vec<u8> = vec![];
        let header = crate::chunk_header::ChunkHeader::from_parts(&data, ChunkType::Simple, 0, 0);
        let chunk = Chunk { header, data };
        assert!(SimpleChunkDecoder::new(chunk).is_err());
    }

    #[test]
    fn test_record_count_exceeds_sizes() {
        use crate::chunk_header::ChunkType;
        // compression=0x00, sizes_len=1, one varint size (5), "hello"
        let data: Vec<u8> = vec![0x00, 0x01, 0x05, b'h', b'e', b'l', b'l', b'o'];
        // header claims 3 records but only 1 size varint provided
        let header = crate::chunk_header::ChunkHeader::from_parts(&data, ChunkType::Simple, 3, 5);
        let chunk = Chunk { header, data };
        assert!(SimpleChunkDecoder::new(chunk).is_err());
    }

    #[test]
    fn test_values_section_truncated() {
        use crate::chunk_header::ChunkType;
        // compression=0x00, sizes_section_len=1, size varint=10, but only 3 bytes of values
        let data: Vec<u8> = vec![0x00, 0x01, 0x0A, b'a', b'b', b'c'];
        let header = crate::chunk_header::ChunkHeader::from_parts(&data, ChunkType::Simple, 1, 10);
        let chunk = Chunk { header, data };
        assert!(SimpleChunkDecoder::new(chunk).is_err());
    }

    #[test]
    fn test_invalid_compression_bytes_rejected() {
        use crate::chunk_header::ChunkType;
        let valid: &[u8] = &[0x00, b'b', b'z', b's'];
        for byte in 0u8..=255 {
            if valid.contains(&byte) {
                continue;
            }
            let data: Vec<u8> = vec![byte, 0x00];
            let header =
                crate::chunk_header::ChunkHeader::from_parts(&data, ChunkType::Simple, 0, 0);
            let chunk = Chunk {
                header,
                data: data.clone(),
            };
            assert!(
                matches!(
                    SimpleChunkDecoder::new(chunk),
                    Err(RiegeliError::UnsupportedCompression(b)) if b == byte
                ),
                "byte {byte:#04x} should return UnsupportedCompression"
            );
        }
    }

    // -------------------------------------------------------------------------
    // Sprint 3 adversarial: additional edge cases
    // -------------------------------------------------------------------------

    /// Encode and decode 1000 records of varying lengths (0..999 bytes).
    #[test]
    fn test_1000_varying_length_records() {
        let mut encoder = SimpleChunkEncoder::new();
        let mut expected: Vec<Vec<u8>> = Vec::with_capacity(1000);

        for i in 0u32..1000 {
            let record: Vec<u8> = (0..i).map(|b| (b % 256) as u8).collect();
            encoder.add_record(&record);
            expected.push(record);
        }

        let chunk = encoder.encode().expect("encode ok");
        assert_eq!(chunk.header.num_records(), 1000);
        // decoded_data_size == sum of 0+1+2+...+999 = 499500
        assert_eq!(chunk.header.decoded_data_size(), 499_500);

        let mut decoder = SimpleChunkDecoder::new(chunk).expect("valid chunk");
        for (i, exp) in expected.iter().enumerate() {
            let got = decoder.read_record().unwrap().unwrap_or_else(|| {
                panic!("expected record {i} but got None");
            });
            assert_eq!(got, *exp, "mismatch at record {i}");
        }
        assert!(decoder.read_record().unwrap().is_none());
    }

    /// Verify data_hash for multiple encodings with different content.
    #[test]
    fn test_data_hash_various_inputs() {
        for input in [b"" as &[u8], b"a", b"hello world", &[0xFF; 10000]] {
            let mut encoder = SimpleChunkEncoder::new();
            encoder.add_record(input);
            let chunk = encoder.encode().expect("encode ok");
            assert_eq!(
                chunk.header.data_hash(),
                crate::hash::highway_hash_64(&chunk.data),
                "data_hash mismatch for input of length {}",
                input.len()
            );
            assert!(
                chunk.header.is_header_valid(),
                "header_hash invalid for input of length {}",
                input.len()
            );
        }
    }

    /// After all records are consumed, repeated calls to read_record return Ok(None).
    #[test]
    fn test_repeated_none_after_exhaustion() {
        let mut encoder = SimpleChunkEncoder::new();
        encoder.add_record(b"only");
        let chunk = encoder.encode().expect("encode ok");

        let mut decoder = SimpleChunkDecoder::new(chunk).expect("valid chunk");
        let _ = decoder.read_record().unwrap().expect("record");
        for _ in 0..5 {
            assert!(decoder.read_record().unwrap().is_none());
        }
    }

    // -------------------------------------------------------------------------
    // Sprint 4 adversarial: compressed chunk edge cases
    // -------------------------------------------------------------------------

    /// Round-trip 5 empty records with Brotli.
    #[test]
    #[cfg(feature = "brotli")]
    fn test_brotli_five_empty_records() {
        let mut encoder = SimpleChunkEncoder::with_compression(CompressionType::Brotli);
        for _ in 0..5 {
            encoder.add_record(b"");
        }
        let chunk = encoder.encode().expect("encode ok");
        assert_eq!(chunk.header.num_records(), 5);
        assert_eq!(chunk.header.decoded_data_size(), 0);

        let mut decoder = SimpleChunkDecoder::new(chunk).expect("valid chunk");
        for _ in 0..5 {
            let record = decoder.read_record().unwrap().expect("should have record");
            assert_eq!(record, b"");
        }
        assert!(decoder.read_record().unwrap().is_none());
    }

    /// Round-trip with a 128-byte record (varint boundary) under Brotli.
    #[test]
    #[cfg(feature = "brotli")]
    fn test_brotli_128_byte_varint_boundary() {
        let record = vec![0xAB_u8; 128];
        let mut encoder = SimpleChunkEncoder::with_compression(CompressionType::Brotli);
        encoder.add_record(&record);
        let chunk = encoder.encode().expect("encode ok");

        let mut decoder = SimpleChunkDecoder::new(chunk).expect("valid chunk");
        let got = decoder.read_record().unwrap().expect("has record");
        assert_eq!(got, record);
        assert!(decoder.read_record().unwrap().is_none());
    }

    /// Round-trip with mixed record sizes under Zstd.
    #[test]
    #[cfg(feature = "zstd")]
    fn test_zstd_mixed_record_sizes() {
        let records: Vec<Vec<u8>> = vec![
            vec![],            // 0 bytes
            vec![0x42; 1],     // 1 byte
            vec![0x43; 127],   // 127 bytes (varint = 1 byte)
            vec![0x44; 128],   // 128 bytes (varint = 2 bytes)
            vec![0x45; 16384], // 16384 bytes (varint = 3 bytes)
        ];

        let mut encoder = SimpleChunkEncoder::with_compression(CompressionType::Zstd);
        for r in &records {
            encoder.add_record(r);
        }
        let chunk = encoder.encode().expect("encode ok");

        let mut decoder = SimpleChunkDecoder::new(chunk).expect("valid chunk");
        for (i, expected) in records.iter().enumerate() {
            let got = decoder.read_record().unwrap().unwrap_or_else(|| {
                panic!("expected record {i} but got None");
            });
            assert_eq!(got, *expected, "mismatch at record {i}");
        }
        assert!(decoder.read_record().unwrap().is_none());
    }

    /// Round-trip 1000 records with Zstd.
    #[test]
    #[cfg(feature = "zstd")]
    fn test_zstd_1000_records() {
        let mut encoder = SimpleChunkEncoder::with_compression(CompressionType::Zstd);
        let mut expected: Vec<Vec<u8>> = Vec::with_capacity(1000);
        for i in 0u32..1000 {
            let record: Vec<u8> = (0..i).map(|b| (b % 256) as u8).collect();
            encoder.add_record(&record);
            expected.push(record);
        }
        let chunk = encoder.encode().expect("encode ok");

        let mut decoder = SimpleChunkDecoder::new(chunk).expect("valid chunk");
        for (i, exp) in expected.iter().enumerate() {
            let got = decoder.read_record().unwrap().unwrap_or_else(|| {
                panic!("expected record {i} but got None");
            });
            assert_eq!(got, *exp, "mismatch at record {i}");
        }
        assert!(decoder.read_record().unwrap().is_none());
    }

    /// Verify the varint64(uncompressed_sizes_len) prefix for 10 records of 200 bytes.
    #[test]
    #[cfg(feature = "brotli")]
    fn test_brotli_varint_prefix_value_10_records() {
        let mut encoder = SimpleChunkEncoder::with_compression(CompressionType::Brotli);
        for _ in 0..10 {
            encoder.add_record(&vec![0xAA; 200]);
        }
        let chunk = encoder.encode().expect("encode ok");
        assert_eq!(chunk.data[0], b'b');

        let (blob_len, blob_len_consumed) = decode_u64(&chunk.data[1..]).expect("varint decode ok");
        let blob_start = 1 + blob_len_consumed;
        let blob_data = &chunk.data[blob_start..blob_start + blob_len as usize];
        let (uncompressed_sizes_len, _) = decode_u64(blob_data).expect("varint decode ok");

        // Each record is 200 bytes -> varint(200) = 2 bytes. 10 records -> 20 bytes.
        assert_eq!(
            uncompressed_sizes_len, 20,
            "10 records of 200 bytes each: sizes_section should be 20 bytes"
        );
    }

    /// Corrupted varint64(uncompressed_sizes_len) prefix causes decode failure.
    #[test]
    #[cfg(feature = "brotli")]
    fn test_brotli_corrupted_varint_prefix_fails_decode() {
        let mut encoder = SimpleChunkEncoder::with_compression(CompressionType::Brotli);
        encoder.add_record(b"hello");
        encoder.add_record(b"world");
        let chunk = encoder.encode().expect("encode ok");

        let original_data = chunk.data.clone();
        assert_eq!(original_data[0], b'b');
        let (original_prefix, prefix_len) =
            decode_u64(&original_data[1..]).expect("varint decode ok");

        // Use a prefix that is larger than the actual blob length.
        let wrong_prefix = original_prefix + 100;
        let wrong_prefix_bytes = encode_u64(wrong_prefix);

        let mut corrupted_data = Vec::new();
        corrupted_data.push(b'b');
        corrupted_data.extend_from_slice(&wrong_prefix_bytes);
        corrupted_data.extend_from_slice(&original_data[1 + prefix_len..]);

        use crate::chunk_header::ChunkType;
        let header =
            crate::chunk_header::ChunkHeader::from_parts(&corrupted_data, ChunkType::Simple, 2, 10);
        let corrupted_chunk = Chunk {
            header,
            data: corrupted_data,
        };

        let result = SimpleChunkDecoder::new(corrupted_chunk);
        assert!(
            result.is_err(),
            "Decoder should fail when varint64(uncompressed_sizes_len) prefix is corrupted"
        );
    }

    /// The varint prefix in the data determines the sizes/values split point.
    #[test]
    #[cfg(feature = "brotli")]
    fn test_brotli_varint_prefix_determines_split() {
        let mut encoder = SimpleChunkEncoder::with_compression(CompressionType::Brotli);
        for i in 0..5 {
            encoder.add_record(&vec![0x42; 50 + i * 10]); // 50, 60, 70, 80, 90 bytes
        }
        let chunk = encoder.encode().expect("encode ok");

        let (blob_len, blob_len_consumed) = decode_u64(&chunk.data[1..]).expect("varint decode ok");
        let blob_start = 1 + blob_len_consumed;
        let blob_data = &chunk.data[blob_start..blob_start + blob_len as usize];
        let (uncompressed_sizes_len, _) = decode_u64(blob_data).expect("varint decode ok");

        // 5 records all with sizes < 128 -> each uses 1-byte varint -> 5 bytes total
        assert_eq!(
            uncompressed_sizes_len, 5,
            "5 records with sizes < 128 should produce 5-byte sizes section"
        );

        assert_eq!(chunk.data[0], b'b');
        let rest_len = chunk.data.len() - blob_start - blob_len as usize;
        assert!(
            rest_len > 0,
            "should have compressed values data after sizes blob"
        );
    }

    /// Repeated read_record after exhaustion returns Ok(None) for Brotli chunks.
    #[test]
    #[cfg(feature = "brotli")]
    fn test_brotli_repeated_none_after_exhaustion() {
        let mut encoder = SimpleChunkEncoder::with_compression(CompressionType::Brotli);
        encoder.add_record(b"only");
        let chunk = encoder.encode().expect("encode ok");

        let mut decoder = SimpleChunkDecoder::new(chunk).expect("valid chunk");
        let _ = decoder.read_record().unwrap().expect("record");
        for _ in 0..5 {
            assert!(decoder.read_record().unwrap().is_none());
        }
    }

    /// Bit flip in compressed data is caught by the data hash check.
    #[test]
    #[cfg(feature = "brotli")]
    fn test_brotli_bit_flip_in_compressed_data() {
        let mut encoder = SimpleChunkEncoder::with_compression(CompressionType::Brotli);
        encoder.add_record(b"important data that should be protected");
        let mut chunk = encoder.encode().expect("encode ok");

        if chunk.data.len() > 5 {
            chunk.data[5] ^= 0x01;
        }

        let result = SimpleChunkDecoder::new(chunk);
        assert!(
            matches!(result, Err(RiegeliError::DataHashMismatch)),
            "bit flip in compressed data should cause DataHashMismatch"
        );
    }
}
