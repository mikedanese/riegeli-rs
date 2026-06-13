//! `RecordWriter` — writes a Riegeli file to any `Write` sink.
//!
//! ## File layout produced
//!
//! ```text
//! offset 0:   BlockHeader { previous_chunk=0, next_chunk=64 }  [24 bytes]
//! offset 24:  ChunkHeader for FileSignature chunk              [40 bytes]
//!             (chunk_type=FileSignature, num_records=0, data_size=0, decoded_data_size=0)
//! offset 64:  data records start here (in Simple chunks)
//! ```
//!
//! Block headers (24 bytes) are inserted at every 65536-byte boundary in the raw
//! file stream. Chunks are written contiguously across those boundaries; the
//! `BlockHeader` bytes are interleaved transparently so that `file_pos` always
//! reflects the true offset in the underlying `Write` stream.

use std::io::Write;

use crate::block_header::BlockHeader;
use crate::chunk_header::ChunkHeader;
use crate::chunk_header::ChunkType;
use crate::compression::CompressOptions;
use crate::compression::CompressionType;
use crate::constants::{BLOCK_HEADER_SIZE, BLOCK_SIZE, CHUNK_HEADER_SIZE};
use crate::error::RiegeliError;
use crate::simple_chunk::{Chunk, SimpleChunkEncoder};
use crate::transpose::encoder::TransposeChunkEncoder;

/// The default chunk size threshold (1 MiB).
const DEFAULT_CHUNK_SIZE: u64 = 1 << 20;

/// Options for configuring a [`RecordWriter`].
#[derive(Debug, Clone)]
pub struct WriterOptions {
    compression: CompressionType,
    chunk_size: u64,
    /// If non-zero, pad so the starting position is a multiple of this value when
    /// appending at a nonzero position (no effect on a fresh file, like C++).
    initial_padding: u64,
    /// If non-zero, pad the file so its total size is a multiple of this value after every flush() and close().
    final_padding: u64,
    /// Use transpose encoding instead of simple encoding.
    transpose: bool,
    /// Fraction of chunk_size to use as bucket_size for transpose encoding (0.0–1.0).
    bucket_fraction: f64,
    /// Compression level / quality override.
    compression_level: Option<i32>,
    /// Window size log2 override for the compressor.
    window_log: Option<u32>,
    /// File metadata payload; if set, a FileMetadata chunk is written after the signature chunk.
    metadata: Option<Vec<u8>>,
}

impl WriterOptions {
    /// Create a `WriterOptions` with default settings:
    /// - `CompressionType::None`
    /// - chunk size: 1 MiB
    /// - no padding
    /// - bucket_fraction: 1.0 (bucket_size = chunk_size)
    pub fn new() -> Self {
        Self {
            compression: CompressionType::None,
            chunk_size: DEFAULT_CHUNK_SIZE,
            initial_padding: 0,
            final_padding: 0,
            transpose: false,
            bucket_fraction: 1.0,
            compression_level: None,
            window_log: None,
            metadata: None,
        }
    }

    /// Set file metadata from a typed `RecordsMetadata` proto message.
    ///
    /// The message is serialized to bytes and written as a `ChunkType::FileMetadata`
    /// chunk immediately after the file signature chunk.
    /// This metadata can be read back via `RecordReader::read_metadata()`.
    pub fn set_metadata(mut self, metadata: crate::RecordsMetadata) -> Self {
        use protobuf::Serialize;
        self.metadata = Some(metadata.serialize().expect("serialize RecordsMetadata"));
        self
    }

    /// Set file metadata from pre-serialized bytes.
    ///
    /// Like [`set_metadata`](Self::set_metadata), but accepts already-serialized
    /// `RecordsMetadata` proto bytes. This can be read back via
    /// `RecordReader::read_serialized_metadata()`.
    pub fn set_serialized_metadata(mut self, data: Vec<u8>) -> Self {
        self.metadata = Some(data);
        self
    }

    /// Set the compression type.
    pub fn compression(mut self, c: CompressionType) -> Self {
        self.compression = c;
        self
    }

    /// Set the chunk size threshold in bytes. Once accumulated uncompressed record
    /// bytes exceed this value, the current chunk is flushed.
    pub fn chunk_size(mut self, n: u64) -> Self {
        self.chunk_size = n;
        self
    }

    /// Enable or disable transpose encoding.
    ///
    /// When enabled, records are encoded using `TransposeChunkEncoder` which
    /// decomposes proto records column-wise for better compression. Non-proto
    /// records are handled transparently.
    pub fn transpose(mut self, enabled: bool) -> Self {
        self.transpose = enabled;
        self
    }

    /// Pad the file so the starting position is a multiple of `size` bytes
    /// when appending to an existing file at a nonzero position.
    ///
    /// This matches the C++ option of the same name: padding is written at the
    /// *beginning* of the output, and only when the writer starts at a nonzero
    /// position. This writer always starts a fresh file at position 0, where
    /// the C++ writer skips initial padding entirely, so this option currently
    /// has no effect. It is kept for option parity with the C++ implementation.
    /// Use [`final_padding`](Self::final_padding) to align the file size.
    ///
    /// Setting `size` to 0 disables padding (the default).
    pub fn initial_padding(mut self, size: u64) -> Self {
        self.initial_padding = size;
        self
    }

    /// Pad the file so its total size is a multiple of `size` bytes after every
    /// `flush()` and `close()`.
    ///
    /// Unlike `initial_padding` (which only pads on `close()`), this option pads
    /// after every flush, making it suitable for streaming writers where intermediate
    /// file states also need to be aligned.
    ///
    /// Setting `size` to 0 disables padding (the default).
    pub fn final_padding(mut self, size: u64) -> Self {
        self.final_padding = size;
        self
    }

    /// Override the compression level / quality for the selected codec.
    ///
    /// - Brotli: 0–11 (default 6)
    /// - Zstd: -131072..=22 (default 3)
    /// - Snappy and None: ignored
    pub fn compression_level(mut self, level: i32) -> Self {
        self.compression_level = Some(level);
        self
    }

    /// Override the window size (log₂ bytes) for the selected codec.
    ///
    /// - Brotli: 10–30 (default 22)
    /// - Zstd: 10–31 (default: automatic)
    /// - Must be `None` for `CompressionType::None` and Snappy — returns an error
    ///   at `RecordWriter::new` if set for those codecs.
    pub fn window_log(mut self, log: Option<u32>) -> Self {
        self.window_log = log;
        self
    }

    /// Set the bucket fraction for transpose encoding.
    ///
    /// `bucket_size = round(chunk_size * bucket_fraction)`, clamped to a minimum
    /// of 1 byte (a fraction of 0.0 places every buffer in its own bucket).
    /// Values outside `[0.0, 1.0]` are clamped into that range. The default is
    /// 1.0 (one bucket per chunk's worth of data).
    ///
    /// Smaller buckets enable finer-grained skipping during field projection at
    /// the cost of slightly worse compression.
    pub fn bucket_fraction(mut self, fraction: f64) -> Self {
        self.bucket_fraction = fraction.clamp(0.0, 1.0);
        self
    }
}

impl Default for WriterOptions {
    fn default() -> Self {
        Self::new()
    }
}

/// Active chunk encoder — either simple or transposed.
enum ActiveEncoder {
    Simple(SimpleChunkEncoder),
    Transpose(Box<TransposeChunkEncoder>),
}

impl ActiveEncoder {
    fn add_record(&mut self, data: &[u8]) -> Result<(), RiegeliError> {
        match self {
            ActiveEncoder::Simple(e) => {
                e.add_record(data);
                Ok(())
            }
            ActiveEncoder::Transpose(e) => e.add_record(data),
        }
    }

    fn encode(self) -> Result<Chunk, RiegeliError> {
        match self {
            ActiveEncoder::Simple(e) => e.encode(),
            ActiveEncoder::Transpose(e) => (*e).encode(),
        }
    }
}

/// A writer that produces a valid Riegeli file.
///
/// Records are accumulated into chunks up to `chunk_size` bytes, then flushed.
/// Block headers are inserted at every 65536-byte boundary in the output stream.
pub struct RecordWriter<W: Write> {
    /// The underlying writer.
    writer: W,
    /// Compression type for data chunks.
    compression: CompressionType,
    /// Desired chunk size threshold in bytes (capped so that `num_records`
    /// cannot overflow the 56-bit field of `chunk_type_and_num_records`).
    chunk_size: u64,
    /// If non-zero, pad so the starting position reaches a multiple of this value when
    /// appending at a nonzero position. This writer always starts a fresh file, so the
    /// value is currently unused (kept for parity with the C++ option; C++ never
    /// applies initial padding at close either).
    #[allow(dead_code)]
    initial_padding: u64,
    /// If non-zero, pad after every flush() and close().
    final_padding: u64,
    /// Whether to use transpose encoding.
    transpose: bool,
    /// Bucket size for transpose encoding (computed from chunk_size * bucket_fraction).
    transpose_bucket_size: u64,
    /// Compression tuning options (level, window_log).
    compress_opts: CompressOptions,
    /// Current file position (byte offset in the underlying stream).
    file_pos: u64,
    /// File position where the last chunk started (used for block header `previous_chunk`).
    last_chunk_start: u64,
    /// Encoder accumulating records for the current chunk.
    encoder: ActiveEncoder,
    /// Accounted size of the current chunk: the sum of record sizes plus 8
    /// bytes of per-record overhead (mirroring `chunk_size_so_far_` in the C++
    /// reference, which bounds the decoder's record-limits array).
    accumulated_bytes: u64,
    /// Number of records pending in the current encoder (may be >0 even when accumulated_bytes==0 for empty records).
    pending_record_count: u64,
    /// Whether `close()` has been called.
    closed: bool,
    /// Set to the message of the first error once any operation fails. Like the
    /// C++ writer (which checks `ok()` on every public entry point), a failed
    /// writer refuses all further operations: the stream may hold a partial
    /// chunk and `file_pos` may no longer match the true stream length.
    failed: Option<String>,
}

impl<W: Write> RecordWriter<W> {
    /// Create a new `RecordWriter`.
    ///
    /// Immediately writes the initial block header and the file signature chunk.
    ///
    /// Returns an error if `window_log` is set but `compression` is
    /// `CompressionType::None` or `CompressionType::Snappy`.
    pub fn new(mut writer: W, options: WriterOptions) -> Result<Self, RiegeliError> {
        // Validate compression tuning options eagerly, matching the C++
        // `CompressorOptions` setters which assert these ranges at option-set
        // time (so misconfigurations are rejected before any bytes are written).
        match options.compression {
            CompressionType::None | CompressionType::Snappy => {
                if options.window_log.is_some() {
                    return Err(RiegeliError::MalformedData(
                        format!(
                            "window_log is not applicable to compression type {:?}",
                            options.compression
                        )
                        .into(),
                    ));
                }
            }
            CompressionType::Brotli => {
                if let Some(level) = options.compression_level
                    && !(0..=11).contains(&level)
                {
                    return Err(RiegeliError::MalformedData(
                        format!(
                            "compression_level out of range for Brotli: {level} (valid: 0..=11)"
                        )
                        .into(),
                    ));
                }
                if let Some(log) = options.window_log
                    && !(10..=30).contains(&log)
                {
                    return Err(RiegeliError::MalformedData(
                        format!("window_log out of range for Brotli: {log} (valid: 10..=30)")
                            .into(),
                    ));
                }
            }
            CompressionType::Zstd => {
                if let Some(level) = options.compression_level
                    && !(-131072..=22).contains(&level)
                {
                    return Err(RiegeliError::MalformedData(
                        format!(
                            "compression_level out of range for Zstd: {level} (valid: -131072..=22)"
                        )
                        .into(),
                    ));
                }
                if let Some(log) = options.window_log
                    && !(10..=31).contains(&log)
                {
                    return Err(RiegeliError::MalformedData(
                        format!("window_log out of range for Zstd: {log} (valid: 10..=31)").into(),
                    ));
                }
            }
        }

        // We start at position 0. Write the first block header.
        // The file signature chunk spans positions 0..64 in the C++ model:
        //   block header (24 bytes) + chunk header (40 bytes) + 0 data bytes = 64 total.
        // previous_chunk = 0 (the chunk containing this block boundary starts at 0)
        // next_chunk = 64 (the end of the file signature chunk / start of next chunk)
        let sig_chunk_end = BLOCK_HEADER_SIZE + CHUNK_HEADER_SIZE; // = 64
        let block_hdr = BlockHeader::from_parts(0, sig_chunk_end);
        writer.write_all(&block_hdr.to_bytes())?;

        let mut file_pos = BLOCK_HEADER_SIZE; // = 24
        let last_chunk_start = 0; // signature chunk starts at position 0 in the chunk model

        // Write the signature chunk header (data_size=0, data is empty).
        let sig_header = ChunkHeader::from_parts(&[], ChunkType::FileSignature, 0, 0);
        writer.write_all(&sig_header.to_bytes())?;
        file_pos += CHUNK_HEADER_SIZE; // = 64

        // No data bytes for the signature chunk.
        // file_pos is now 64, which is where the next chunk starts.

        let compression = options.compression;
        // Cap the chunk-size threshold so that `num_records` (each record is
        // accounted as at least 8 bytes) cannot overflow the 56-bit field of
        // `chunk_type_and_num_records`, matching the C++
        // `desired_chunk_size_ = min(chunk_size, kMaxNumRecords * sizeof(uint64_t))`.
        const MAX_NUM_RECORDS: u64 = u64::MAX >> 8;
        let chunk_size = options.chunk_size.min(MAX_NUM_RECORDS * 8);
        let initial_padding = options.initial_padding;
        let final_padding = options.final_padding;
        let transpose = options.transpose;

        // Compute bucket_size for transpose encoding. Matches the C++ reference
        // (RecordWriterBase::Worker::MakeChunkEncoder):
        // bucket_size = round(chunk_size * bucket_fraction), clamped to at least 1
        // and saturating at u64::MAX.
        let bucket_fraction = options.bucket_fraction.clamp(0.0, 1.0);
        let bucket_size_rounded = (options.chunk_size as f64 * bucket_fraction).round();
        let transpose_bucket_size = if bucket_size_rounded >= u64::MAX as f64 {
            u64::MAX
        } else if bucket_size_rounded >= 1.0 {
            bucket_size_rounded as u64
        } else {
            1
        };

        let compress_opts = CompressOptions {
            level: options.compression_level,
            window_log: options.window_log,
        };

        let encoder = if transpose {
            ActiveEncoder::Transpose(Box::new(
                TransposeChunkEncoder::new(compression)
                    .compress_opts(compress_opts)
                    .bucket_size(transpose_bucket_size),
            ))
        } else {
            ActiveEncoder::Simple(SimpleChunkEncoder::with_options(compression, compress_opts))
        };

        let mut this = Self {
            writer,
            compression,
            chunk_size,
            initial_padding,
            final_padding,
            transpose,
            transpose_bucket_size,
            compress_opts,
            file_pos,
            last_chunk_start,
            encoder,
            accumulated_bytes: 0,
            pending_record_count: 0,
            closed: false,
            failed: None,
        };

        // Write the optional FileMetadata chunk immediately after the signature chunk.
        //
        // Matches the C++ reference (RecordWriterBase::Worker::EncodeMetadata):
        // the serialized RecordsMetadata is itself transpose-encoded with the
        // configured compressor (default tuning, i.e. a single bucket), and the
        // chunk header carries num_records = 0 with decoded_data_size equal to
        // the serialized metadata size.
        if let Some(metadata_bytes) = options.metadata {
            let mut metadata_encoder =
                TransposeChunkEncoder::new(compression).compress_opts(compress_opts);
            metadata_encoder.add_record(&metadata_bytes)?;
            let encoded = metadata_encoder.encode()?;
            let metadata_header = ChunkHeader::from_parts(
                &encoded.data,
                ChunkType::FileMetadata,
                0,
                metadata_bytes.len() as u64,
            );
            this.write_chunk_raw(&metadata_header, &encoded.data)?;
        }

        Ok(this)
    }

    /// Write a single record.
    ///
    /// Returns `Err(RiegeliError::WriterClosed)` if the writer has been closed,
    /// and `Err(RiegeliError::WriterFailed)` if a previous operation failed.
    ///
    /// The flush policy mirrors the C++ reference (`WriteRecordImpl`): each
    /// record is accounted as its size plus 8 bytes (decoding a chunk stores
    /// record positions in a limits array, so even empty records have a cost
    /// and cannot accumulate unboundedly). The current chunk is flushed
    /// *before* adding a record that would push the accounted size over the
    /// threshold, and flushed after adding when not even an empty record
    /// would still fit.
    pub fn write_record(&mut self, data: &[u8]) -> Result<(), RiegeliError> {
        if self.closed {
            return Err(RiegeliError::WriterClosed);
        }
        self.check_not_failed()?;
        let result = self.write_record_impl(data);
        self.latch_failure(result)
    }

    fn write_record_impl(&mut self, data: &[u8]) -> Result<(), RiegeliError> {
        let added_size = data.len() as u64 + 8;
        if self.accumulated_bytes + added_size > self.chunk_size && self.accumulated_bytes > 0 {
            self.flush_chunk()?;
        }
        self.accumulated_bytes += added_size;
        self.pending_record_count += 1;
        self.encoder.add_record(data)?;
        if self.accumulated_bytes + 8 > self.chunk_size {
            // No more records will fit in this chunk, most likely because a
            // single record exceeds the desired chunk size. Write the chunk now
            // to avoid keeping a large chunk in memory.
            self.flush_chunk()?;
        }
        Ok(())
    }

    /// Flush any pending records as a chunk to the underlying writer.
    ///
    /// After this call, the file contains all records written so far and is in a
    /// valid, readable state.
    pub fn flush(&mut self) -> Result<(), RiegeliError> {
        if self.closed {
            return Ok(());
        }
        self.check_not_failed()?;
        let result = self.flush_impl();
        self.latch_failure(result)
    }

    fn flush_impl(&mut self) -> Result<(), RiegeliError> {
        if self.pending_record_count > 0 {
            self.flush_chunk()?;
        }
        if self.final_padding > 0 {
            self.write_padding_to_multiple(self.final_padding)?;
        }
        Ok(self.writer.flush()?)
    }

    /// Close the writer, flushing any pending data.
    ///
    /// After `close()`, calling `write_record()` returns an error.
    pub fn close(mut self) -> Result<(), RiegeliError> {
        self.flush_internal()
    }

    // -----------------------------------------------------------------------
    // Internal helpers
    // -----------------------------------------------------------------------

    /// Returns an error if a previous operation failed.
    fn check_not_failed(&self) -> Result<(), RiegeliError> {
        match &self.failed {
            Some(msg) => Err(RiegeliError::WriterFailed(msg.clone().into())),
            None => Ok(()),
        }
    }

    /// Record the first failure so that all subsequent operations refuse to
    /// write more bytes (mirroring the C++ `!ok()` guard on every entry point).
    fn latch_failure(&mut self, result: Result<(), RiegeliError>) -> Result<(), RiegeliError> {
        if let Err(e) = &result
            && self.failed.is_none()
        {
            self.failed = Some(e.to_string());
        }
        result
    }

    /// Flush pending records and mark the writer closed.
    fn flush_internal(&mut self) -> Result<(), RiegeliError> {
        if self.closed {
            return Ok(());
        }
        self.closed = true;
        self.check_not_failed()?;
        let result = self.flush_internal_impl();
        self.latch_failure(result)
    }

    fn flush_internal_impl(&mut self) -> Result<(), RiegeliError> {
        if self.pending_record_count > 0 {
            self.flush_chunk()?;
        }
        // Like the C++ writer, only final_padding applies at close
        // (`RecordWriterBase::Done` calls `MaybePadToFinalBoundary` only;
        // initial_padding is applied solely when appending at a nonzero
        // position, which this writer does not support).
        if self.final_padding > 0 {
            self.write_padding_to_multiple(self.final_padding)?;
        }
        Ok(self.writer.flush()?)
    }

    /// Write a padding chunk so that the file size becomes a multiple of `alignment`.
    ///
    /// The padding chunk has `ChunkType::Padding` and enough data bytes that the
    /// total file size lands on an alignment boundary. The target position is
    /// computed by [`pos_after_padding`], mirroring the C++ implementation
    /// (`DefaultChunkWriterBase::WritePadding` in records/chunk_writer.cc).
    fn write_padding_to_multiple(&mut self, alignment: u64) -> Result<(), RiegeliError> {
        let current = self.file_pos;
        let end_pos = pos_after_padding(current, alignment);
        if end_pos == current {
            return Ok(());
        }

        // Excludes the chunk header and any intervening block headers.
        let data_size = crate::block_arithmetic::distance_without_overhead(current, end_pos)
            - CHUNK_HEADER_SIZE;
        let padding_data = vec![0u8; data_size as usize];
        let padding_header = ChunkHeader::from_parts(&padding_data, ChunkType::Padding, 0, 0);
        self.write_chunk_raw(&padding_header, &padding_data)?;

        Ok(())
    }

    /// Flush the accumulated records as a chunk (simple or transposed).
    fn flush_chunk(&mut self) -> Result<(), RiegeliError> {
        // Take the current encoder and replace with a fresh one.
        let new_encoder = if self.transpose {
            ActiveEncoder::Transpose(Box::new(
                TransposeChunkEncoder::new(self.compression)
                    .compress_opts(self.compress_opts)
                    .bucket_size(self.transpose_bucket_size),
            ))
        } else {
            ActiveEncoder::Simple(SimpleChunkEncoder::with_options(
                self.compression,
                self.compress_opts,
            ))
        };
        let encoder = std::mem::replace(&mut self.encoder, new_encoder);
        self.accumulated_bytes = 0;
        self.pending_record_count = 0;

        let chunk = encoder.encode()?;
        self.write_chunk_raw(&chunk.header, &chunk.data)
    }

    /// Write a chunk (header + data) to the stream, inserting block headers
    /// at every 65536-byte boundary.
    ///
    /// Updates `self.file_pos` and `self.last_chunk_start` as a side-effect.
    fn write_chunk_raw(&mut self, header: &ChunkHeader, data: &[u8]) -> Result<(), RiegeliError> {
        let chunk_start = self.file_pos;

        // Compute where this chunk ends in the file stream.
        //
        // The C++ reference implementation uses:
        //   chunk_end = max(
        //     AddWithOverhead(chunk_begin, header_size + data_size),
        //     RoundUpToPossibleChunkBoundary(chunk_begin + num_records)
        //   )
        //
        // The second term ensures that each chunk occupies at least `num_records`
        // file bytes past its start position. This is required for recovery scanning:
        // the C++ reader enforces this invariant at `Close()` time.
        let chunk_end_pos = crate::block_arithmetic::chunk_end(
            chunk_start,
            header.data_size(),
            header.num_records(),
        );

        // The bytes we need to write: chunk header bytes followed by data bytes.
        let header_bytes = header.to_bytes();
        let all_bytes: Vec<u8> = header_bytes
            .iter()
            .copied()
            .chain(data.iter().copied())
            .collect();

        // Write all_bytes to the stream, inserting block headers at boundaries.
        // Block header fields are distances from the boundary: back to
        // chunk_start and forward to chunk_end_pos.
        self.last_chunk_start = chunk_start;
        self.write_bytes_with_block_headers(&all_bytes, chunk_start, chunk_end_pos)?;

        // If the chunk data ended before `chunk_end_pos`, write zero-padding bytes so
        // the file position advances to `chunk_end_pos`. This matches the C++ behaviour
        // where `WritePadding(chunk_begin, chunk_end, dest)` is called after the data.
        // Loop on file_pos rather than a separate byte counter: block headers
        // emitted at boundaries advance the file position toward chunk_end_pos
        // just like padding bytes do. A counter that doesn't account for them
        // overshoots the chunk end — writing zeros where the next chunk header
        // belongs — or underflows computing a block header's next_chunk field.
        while self.file_pos < chunk_end_pos {
            if self.file_pos.is_multiple_of(BLOCK_SIZE) {
                let block_pos = self.file_pos;
                let prev = block_pos - chunk_start;
                let next = chunk_end_pos - block_pos;
                let bh = crate::block_header::BlockHeader::from_parts(prev, next);
                self.writer.write_all(&bh.to_bytes())?;
                self.file_pos += BLOCK_HEADER_SIZE;
                continue;
            }
            let pos_in_block = self.file_pos % BLOCK_SIZE;
            let space_in_block = BLOCK_SIZE - pos_in_block;
            let to_write = (chunk_end_pos - self.file_pos).min(space_in_block) as usize;
            let pad = vec![0u8; to_write];
            self.writer.write_all(&pad)?;
            self.file_pos += to_write as u64;
        }

        Ok(())
    }

    /// Write `bytes` to the underlying stream starting at `self.file_pos`,
    /// inserting a `BlockHeader` whenever we cross a block boundary.
    ///
    /// `chunk_begin` and `chunk_end` are the absolute file positions of the chunk
    /// start and end. Block headers use **relative** offsets from the block boundary,
    /// matching the C++ format:
    /// - `previous_chunk = block_boundary_pos - chunk_begin`
    /// - `next_chunk = chunk_end - block_boundary_pos`
    fn write_bytes_with_block_headers(
        &mut self,
        bytes: &[u8],
        chunk_begin: u64,
        chunk_end: u64,
    ) -> Result<(), RiegeliError> {
        let mut offset = 0usize; // how many bytes of `bytes` we've written so far

        while offset < bytes.len() {
            // If we're at a block boundary, write a block header first.
            if self.file_pos.is_multiple_of(BLOCK_SIZE) {
                let block_pos = self.file_pos;
                let prev = block_pos - chunk_begin;
                let next = chunk_end - block_pos;
                let bh = BlockHeader::from_parts(prev, next);
                self.writer.write_all(&bh.to_bytes())?;
                self.file_pos += BLOCK_HEADER_SIZE;
            }

            // How many bytes can we write before the next block boundary?
            let pos_in_block = (self.file_pos % BLOCK_SIZE) as usize;
            let space_in_block = BLOCK_SIZE as usize - pos_in_block;
            let remaining = bytes.len() - offset;
            let to_write = remaining.min(space_in_block);

            self.writer.write_all(&bytes[offset..offset + to_write])?;
            self.file_pos += to_write as u64;
            offset += to_write;
        }

        Ok(())
    }
}

/// The position after writing a padding chunk at `pos` so that the resulting
/// position is a multiple of `padding`. Returns `pos` unchanged when no padding
/// chunk is needed.
///
/// This mirrors `records_internal::PosAfterPadding` (records/chunk_writer.cc):
/// - the padded length is grown in `padding` steps until it can hold at least a
///   chunk header (40 bytes);
/// - the end position is then grown in `padding` steps while it is not a
///   possible chunk boundary (inside or immediately after a block header; a
///   block boundary itself is allowed — the block header belongs to the chunk).
///
/// One extra guard beyond C++: the end position is also grown while the span
/// holds less than a chunk header of *usable* bytes (block headers interleaved
/// in the span don't count). The C++ arithmetic underflows in that case
/// (`DistanceWithoutOverhead(...) - ChunkHeader::size()` wraps); growing the
/// span further keeps the file aligned without panicking.
fn pos_after_padding(pos: u64, padding: u64) -> u64 {
    if padding <= 1 {
        return pos;
    }
    let remainder = pos % padding;
    if remainder == 0 {
        return pos;
    }
    let mut length = padding - remainder;
    while length < CHUNK_HEADER_SIZE {
        // Not enough space for the chunk header.
        length += padding;
    }
    let mut end_pos = pos + length;
    loop {
        // C++ `IsPossibleChunkBoundary`: `RemainingInBlock(pos) < kUsableBlockSize`,
        // i.e. block offset 0 (the block header belongs to the chunk) or >= 25.
        let is_possible_chunk_boundary =
            crate::block_arithmetic::remaining_in_block(end_pos) < BLOCK_SIZE - BLOCK_HEADER_SIZE;
        if is_possible_chunk_boundary
            && crate::block_arithmetic::distance_without_overhead(pos, end_pos) >= CHUNK_HEADER_SIZE
        {
            return end_pos;
        }
        // `end_pos` falls inside a block header, or the span has no room for
        // the chunk header once interleaved block headers are excluded.
        end_pos += padding;
    }
}

impl<W: Write> Drop for RecordWriter<W> {
    fn drop(&mut self) {
        let _ = self.flush_internal();
    }
}

#[cfg(test)]
mod tests {

    use super::*;
    use crate::block_header::BlockHeader;
    use crate::chunk_header::ChunkHeader;
    use crate::constants::BLOCK_SIZE;
    use crate::simple_chunk::{Chunk, SimpleChunkDecoder};

    // Helper that returns the bytes of the written file
    fn write_file(records: &[&[u8]], options: WriterOptions) -> Vec<u8> {
        struct BufWriter {
            data: Vec<u8>,
        }
        impl Write for BufWriter {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                self.data.extend_from_slice(buf);
                Ok(buf.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }

        let mut bw = BufWriter { data: Vec::new() };
        {
            let mut w = RecordWriter::new(&mut bw, options).expect("new ok");
            for rec in records {
                w.write_record(rec).expect("write ok");
            }
            w.flush().expect("flush ok");
        }
        bw.data
    }

    // -----------------------------------------------------------------------
    // bytes 0–23 are a valid BlockHeader with prev=0, next=24
    // -----------------------------------------------------------------------
    #[test]
    fn first_block_header_valid() {
        let data = write_file(&[b"hello"], WriterOptions::new());
        assert!(data.len() >= 24);
        let bh = BlockHeader::from_bytes(data[..24].try_into().unwrap());
        assert!(bh.is_valid(), "first block header must be valid");
        assert_eq!(bh.previous_chunk(), 0, "previous_chunk must be 0");
        assert_eq!(bh.next_chunk(), 64, "next_chunk must be 64");
    }

    // -----------------------------------------------------------------------
    // chunk at offset 24 is FileSignature with data_size=0
    // -----------------------------------------------------------------------
    #[test]
    fn signature_chunk_at_offset_24() {
        let data = write_file(&[], WriterOptions::new());
        assert!(data.len() >= 64, "file must be at least 64 bytes");
        let ch = ChunkHeader::from_bytes(data[24..64].try_into().unwrap());
        assert!(ch.is_header_valid(), "signature chunk header must be valid");
        assert_eq!(ch.chunk_type().unwrap(), ChunkType::FileSignature);
        assert_eq!(ch.data_size(), 0);
        assert_eq!(ch.num_records(), 0);
    }

    // -----------------------------------------------------------------------
    // after close(), write_record returns Err
    // -----------------------------------------------------------------------
    #[test]
    fn write_after_close_returns_err() {
        // We use a Vec<u8> reference via a shared mutable pointer so we can
        // recover the data after close(). close() consumes self, so we verify
        // that the closed flag is properly respected by using mark_closed().
        //
        // The design: close() calls flush_internal() which sets closed=true.
        // write_record() checks closed and returns Err(WriterClosed).
        // We verify this by using close_mut() which is a &mut self variant.

        let data_store = std::cell::RefCell::new(Vec::<u8>::new());

        // Verify write succeeds before close
        {
            struct RefWriter<'a> {
                data: &'a std::cell::RefCell<Vec<u8>>,
            }
            impl Write for RefWriter<'_> {
                fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                    self.data.borrow_mut().extend_from_slice(buf);
                    Ok(buf.len())
                }
                fn flush(&mut self) -> std::io::Result<()> {
                    Ok(())
                }
            }

            let rw = RefWriter { data: &data_store };
            let mut w = RecordWriter::new(rw, WriterOptions::new()).expect("new ok");
            // write_record before close should succeed
            w.write_record(b"hello")
                .expect("write before close should succeed");
            // Manually set closed flag to verify write_record returns Err
            w.closed = true;
            let result = w.write_record(b"after close");
            assert!(
                matches!(result, Err(RiegeliError::WriterClosed)),
                "expected WriterClosed error, got: {result:?}"
            );
        }
    }

    // -----------------------------------------------------------------------
    // flush() makes file readable with correct record count
    // -----------------------------------------------------------------------
    #[test]
    fn flush_makes_file_readable() {
        let data = write_file(&[b"record1", b"record2", b"record3"], WriterOptions::new());
        // The file should have: block header (24), sig chunk header (40),
        // then one or more Simple chunks.
        verify_can_decode_records(&data, &[b"record1", b"record2", b"record3"]);
    }

    // -----------------------------------------------------------------------
    // 10,000 records with Brotli compression
    // -----------------------------------------------------------------------
    #[test]
    #[cfg(feature = "brotli")]
    fn ten_thousand_records_brotli() {
        let record_data: Vec<u8> = (0..100u8).cycle().take(100).collect();
        let records: Vec<&[u8]> = (0..10_000).map(|_| record_data.as_slice()).collect();
        let opts = WriterOptions::new().compression(CompressionType::Brotli);
        let data = write_file(&records, opts);

        // Check first block header
        let bh = BlockHeader::from_bytes(data[..24].try_into().unwrap());
        assert!(bh.is_valid());
        assert_eq!(bh.previous_chunk(), 0);
        assert_eq!(bh.next_chunk(), 64);

        // Check signature chunk
        let ch = ChunkHeader::from_bytes(data[24..64].try_into().unwrap());
        assert!(ch.is_header_valid());
        assert_eq!(ch.chunk_type().unwrap(), ChunkType::FileSignature);

        // Check all block headers at block boundaries
        let mut pos = BLOCK_SIZE as usize;
        while pos < data.len() {
            if pos + 24 <= data.len() {
                let bh = BlockHeader::from_bytes(data[pos..pos + 24].try_into().unwrap());
                assert!(bh.is_valid(), "block header at offset {pos} must be valid");
            }
            pos += BLOCK_SIZE as usize;
        }
    }

    // -----------------------------------------------------------------------
    // every block boundary has a valid BlockHeader
    // -----------------------------------------------------------------------
    #[test]
    fn block_headers_at_boundaries() {
        // Write enough data to span multiple blocks
        // BLOCK_SIZE = 65536, we need > 65536 bytes total file size
        // Each record is 1000 bytes; chunk_size = 4096 so chunks flush often
        // 200 records × 1000 bytes = 200 KiB of records
        let record: Vec<u8> = vec![0xAB; 1000];
        let records: Vec<&[u8]> = (0..200).map(|_| record.as_slice()).collect();
        let opts = WriterOptions::new().chunk_size(4096);
        let data = write_file(&records, opts);

        assert!(
            data.len() > BLOCK_SIZE as usize,
            "file must span multiple blocks; got {} bytes",
            data.len()
        );

        // Check first block header
        let bh0 = BlockHeader::from_bytes(data[..24].try_into().unwrap());
        assert!(bh0.is_valid(), "first block header invalid");

        // Check all subsequent block boundaries
        let mut pos = BLOCK_SIZE as usize;
        while pos + 24 <= data.len() {
            let bh = BlockHeader::from_bytes(data[pos..pos + 24].try_into().unwrap());
            assert!(
                bh.is_valid(),
                "block header at offset {pos} is invalid (prev={}, next={})",
                bh.previous_chunk(),
                bh.next_chunk()
            );
            pos += BLOCK_SIZE as usize;
        }
    }

    // -----------------------------------------------------------------------
    // Decode helper: parse the Simple chunks and verify record contents
    // -----------------------------------------------------------------------
    fn verify_can_decode_records(file_data: &[u8], expected_records: &[&[u8]]) {
        // Skip the first block header (24 bytes) and signature chunk (40 bytes header + 0 bytes data).
        // Then read Simple chunks sequentially, skipping block headers at block boundaries.
        let mut pos = 64usize; // after sig chunk
        let mut all_records: Vec<Vec<u8>> = Vec::new();

        while pos < file_data.len() {
            // Skip block header if at block boundary
            if pos.is_multiple_of(BLOCK_SIZE as usize) {
                pos += BLOCK_HEADER_SIZE as usize;
                if pos >= file_data.len() {
                    break;
                }
            }

            // Read a chunk header
            if pos + CHUNK_HEADER_SIZE as usize > file_data.len() {
                break;
            }
            let ch_bytes: [u8; 40] = file_data[pos..pos + 40].try_into().unwrap();
            let ch = ChunkHeader::from_bytes(ch_bytes);
            pos += 40;

            if !ch.is_header_valid() {
                panic!("invalid chunk header at pos {}", pos - 40);
            }

            let data_size = ch.data_size() as usize;
            // Read data bytes, potentially spanning block boundaries
            let mut chunk_data = Vec::with_capacity(data_size);
            let mut remaining = data_size;
            while remaining > 0 {
                if pos.is_multiple_of(BLOCK_SIZE as usize) {
                    pos += BLOCK_HEADER_SIZE as usize;
                }
                let space = (BLOCK_SIZE as usize - (pos % BLOCK_SIZE as usize)).min(remaining);
                chunk_data.extend_from_slice(&file_data[pos..pos + space]);
                pos += space;
                remaining -= space;
            }

            if ch.chunk_type().unwrap() == ChunkType::Simple {
                let chunk = Chunk {
                    header: ch,
                    data: chunk_data,
                };
                let mut decoder = SimpleChunkDecoder::new(chunk).expect("decoder ok");
                while let Some(rec) = decoder.read_record().expect("read ok") {
                    all_records.push(rec);
                }
            }
        }

        assert_eq!(
            all_records.len(),
            expected_records.len(),
            "record count mismatch: got {} expected {}",
            all_records.len(),
            expected_records.len()
        );
        for (i, (got, expected)) in all_records.iter().zip(expected_records.iter()).enumerate() {
            assert_eq!(got.as_slice(), *expected, "record {i} mismatch");
        }
    }

    #[test]
    fn roundtrip_uncompressed() {
        let records: &[&[u8]] = &[b"alpha", b"beta", b"gamma"];
        let data = write_file(records, WriterOptions::new());
        verify_can_decode_records(&data, records);
    }

    #[test]
    #[cfg(feature = "brotli")]
    fn roundtrip_brotli() {
        let records: &[&[u8]] = &[b"hello brotli", b"world brotli"];
        let data = write_file(
            records,
            WriterOptions::new().compression(CompressionType::Brotli),
        );
        verify_can_decode_records(&data, records);
    }

    #[test]
    fn roundtrip_many_records_multi_chunk() {
        // 100 records of 100 bytes each, chunk_size=512 so multiple chunks
        let record: Vec<u8> = vec![0x42; 100];
        let records: Vec<&[u8]> = (0..100).map(|_| record.as_slice()).collect();
        let opts = WriterOptions::new().chunk_size(512);
        let data = write_file(&records, opts);
        verify_can_decode_records(&data, &records);
    }

    // -----------------------------------------------------------------------
    // RecordWriter with transpose -> RecordReader round-trip
    // -----------------------------------------------------------------------

    /// Helper using RecordReader for full round-trip (handles both Simple and Transposed chunks).
    fn roundtrip_with_reader(records: &[&[u8]], options: WriterOptions) -> Vec<Vec<u8>> {
        use crate::record_reader::{ReaderOptions, RecordReader};

        let file_data = write_file(records, options);
        let cursor = std::io::Cursor::new(file_data);
        let mut reader = RecordReader::new(cursor, ReaderOptions::new()).expect("reader ok");
        let mut result = Vec::new();
        while let Some(rec) = reader.read_record().expect("read_record") {
            result.push(rec);
        }
        result
    }

    #[test]
    fn transpose_roundtrip_proto() {
        // Proto records: field 1 varint.
        let r0 = vec![0x08, 0x2A]; // varint 42
        let r1 = vec![0x08, 0x01]; // varint 1
        let r2 = vec![0x08, 0x7F]; // varint 127
        let opts = WriterOptions::new().transpose(true);
        let result = roundtrip_with_reader(&[&r0, &r1, &r2], opts);
        assert_eq!(result.len(), 3);
        assert_eq!(result[0], r0);
        assert_eq!(result[1], r1);
        assert_eq!(result[2], r2);
    }

    #[test]
    fn transpose_roundtrip_nonproto() {
        let r0 = vec![0xFF, 0x01, 0x02]; // not valid proto
        let opts = WriterOptions::new().transpose(true);
        let result = roundtrip_with_reader(&[&r0], opts);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0], r0);
    }

    #[test]
    fn transpose_roundtrip_mixed() {
        let proto_rec = vec![0x08, 0x2A];
        let nonproto_rec = vec![0xFF, 0xAA];
        let proto_rec2 = vec![0x10, 0x01]; // field 2, varint 1
        let opts = WriterOptions::new().transpose(true);
        let result = roundtrip_with_reader(&[&proto_rec, &nonproto_rec, &proto_rec2], opts);
        assert_eq!(result.len(), 3);
        assert_eq!(result[0], proto_rec);
        assert_eq!(result[1], nonproto_rec);
        assert_eq!(result[2], proto_rec2);
    }

    #[test]
    fn transpose_roundtrip_1000_records() {
        use crate::varint::encode_u64;
        let mut records: Vec<Vec<u8>> = Vec::new();
        for i in 0u32..1000 {
            let mut rec = Vec::new();
            rec.push(0x08);
            rec.extend_from_slice(&encode_u64(i as u64));
            rec.push(0x15);
            rec.extend_from_slice(&i.to_le_bytes());
            records.push(rec);
        }
        let refs: Vec<&[u8]> = records.iter().map(|r| r.as_slice()).collect();
        let opts = WriterOptions::new().transpose(true);
        let result = roundtrip_with_reader(&refs, opts);
        assert_eq!(result.len(), 1000);
        for (i, (got, expected)) in result.iter().zip(records.iter()).enumerate() {
            assert_eq!(got, expected, "record {i} mismatch");
        }
    }

    #[test]
    fn transpose_roundtrip_nested_submessage() {
        // field 1 = submessage { field 2 = varint 42 }
        let record = vec![0x0A, 0x02, 0x10, 0x2A];
        let opts = WriterOptions::new().transpose(true);
        let result = roundtrip_with_reader(&[&record], opts);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0], record);
    }

    // -----------------------------------------------------------------------
    // Chunk flush policy must match the C++ reference (WriteRecordImpl):
    // each record is accounted as size + 8 bytes, a chunk is flushed BEFORE
    // adding a record that would overflow the threshold, and flushed after
    // adding when not even an empty record would fit.
    // -----------------------------------------------------------------------

    /// Parse the file and return `num_records` for each Simple chunk.
    fn data_chunk_record_counts(file_data: &[u8]) -> Vec<u64> {
        let mut pos = 64usize; // after the signature chunk
        let mut counts = Vec::new();
        while pos < file_data.len() {
            if pos % BLOCK_SIZE as usize == 0 {
                pos += BLOCK_HEADER_SIZE as usize;
            }
            if pos + CHUNK_HEADER_SIZE as usize > file_data.len() {
                break;
            }
            let ch_bytes: [u8; 40] = file_data[pos..pos + 40].try_into().unwrap();
            let ch = ChunkHeader::from_bytes(ch_bytes);
            assert!(ch.is_header_valid(), "invalid chunk header at {pos}");
            if ch.chunk_type().unwrap() == ChunkType::Simple {
                counts.push(ch.num_records());
            }
            // Skip data, accounting for interleaved block headers.
            let mut remaining = ch.data_size() as usize;
            pos += 40;
            while remaining > 0 {
                if pos % BLOCK_SIZE as usize == 0 {
                    pos += BLOCK_HEADER_SIZE as usize;
                }
                let space = (BLOCK_SIZE as usize - (pos % BLOCK_SIZE as usize)).min(remaining);
                pos += space;
                remaining -= space;
            }
        }
        counts
    }

    #[test]
    fn flush_before_adding_record_that_overflows_chunk() {
        // chunk_size 1000: the first 600-byte record is accounted as 608 bytes;
        // before adding the second one, 608 + 608 > 1000, so the chunk must be
        // flushed first. The result is two chunks of one record each, never a
        // single chunk overshooting the threshold.
        let rec = vec![0u8; 600];
        let data = write_file(&[&rec, &rec], WriterOptions::new().chunk_size(1000));
        assert_eq!(data_chunk_record_counts(&data), vec![1, 1]);
    }

    #[test]
    fn empty_records_flush_in_bounded_chunks() {
        // Empty records still cost 8 bytes of accounting each (bounding the
        // decoder's record-limits array), so with chunk_size 80 every 10 empty
        // records must flush a chunk instead of accumulating indefinitely.
        let records: Vec<&[u8]> = vec![&[]; 25];
        let data = write_file(&records, WriterOptions::new().chunk_size(80));
        assert_eq!(data_chunk_record_counts(&data), vec![10, 10, 5]);
    }

    #[test]
    fn oversized_record_flushed_immediately() {
        // A single record larger than chunk_size is flushed right after being
        // added, so it never lingers in memory and each oversized record gets
        // its own chunk.
        let big = vec![0xCDu8; 5000];
        let data = write_file(&[&big, &big], WriterOptions::new().chunk_size(1000));
        assert_eq!(data_chunk_record_counts(&data), vec![1, 1]);
    }

    // -----------------------------------------------------------------------
    // bucket_fraction -> bucket_size mapping must match the C++ reference:
    // bucket_size = round(chunk_size * bucket_fraction), clamped to at least 1.
    // -----------------------------------------------------------------------
    #[test]
    fn bucket_size_matches_reference_semantics() {
        fn bucket_size_for(chunk_size: u64, fraction: f64) -> u64 {
            let writer = RecordWriter::new(
                std::io::Cursor::new(Vec::<u8>::new()),
                WriterOptions::new()
                    .chunk_size(chunk_size)
                    .transpose(true)
                    .bucket_fraction(fraction),
            )
            .expect("writer ok");
            writer.transpose_bucket_size
        }

        // Fraction 1.0: one bucket per chunk's worth of data, not unbounded.
        assert_eq!(bucket_size_for(1 << 20, 1.0), 1 << 20);
        // Fraction 0.0: every buffer gets its own bucket (bucket_size 1).
        assert_eq!(bucket_size_for(1 << 20, 0.0), 1);
        // No artificial 256-byte floor.
        assert_eq!(bucket_size_for(100, 0.5), 50);
        // Rounding to nearest.
        assert_eq!(bucket_size_for(101, 0.5), 51);
        assert_eq!(bucket_size_for(1000, 0.3), 300);
    }

    #[test]
    fn transpose_roundtrip_empty() {
        let opts = WriterOptions::new().transpose(true);
        let result = roundtrip_with_reader(&[], opts);
        assert!(result.is_empty());
    }

    // -------------------------------------------------------------------------
    // Padding that spans block boundaries (record-count-dominated chunk end)
    // -------------------------------------------------------------------------

    use std::io::Cursor;

    /// Many empty records with compression make `num_records` exceed the
    /// chunk's physical data bytes, so the chunk end comes from
    /// `round_up_to_possible_chunk_boundary(chunk_begin + num_records)` and
    /// the writer pads tens of kilobytes — crossing block boundaries. The
    /// padding loop must count emitted block headers toward the distance to
    /// the chunk end; a loop that doesn't overshoots the chunk end (zeros
    /// where the next chunk header belongs) and the file fails to read back.
    #[test]
    #[cfg(feature = "zstd")]
    fn record_count_padding_across_block_boundaries() {
        use crate::record_reader::{ReaderOptions, RecordReader};

        /// Block boundaries falling strictly inside a chunk's span, read
        /// straight from the raw block-header bytes (previous-chunk distance
        /// nonzero), independent of reader and writer position bookkeeping.
        fn interior_boundaries(data: &[u8]) -> usize {
            let block = BLOCK_SIZE as usize;
            let mut boundary = block;
            let mut count = 0;
            while boundary + BLOCK_HEADER_SIZE as usize <= data.len() {
                let prev =
                    u64::from_le_bytes(data[boundary + 8..boundary + 16].try_into().unwrap());
                if prev > 0 {
                    count += 1;
                }
                boundary += block;
            }
            count
        }

        // Sweep counts so chunk_begin + num_records lands before, inside, and
        // after block-header windows, crossing one and two boundaries.
        //
        // The multi-boundary values are the load-bearing ones: with the old
        // overshoot bug, a single-boundary file (e.g. n=70k) still read back
        // cleanly — the 24 stray bytes landed at EOF and were absorbed —
        // while only padding that crossed a second boundary corrupted a
        // readable position. The assertion below keeps the sweep honest if
        // the list is ever trimmed.
        let mut max_interior = 0usize;
        for n in [70_000usize, 131_000, 131_020, 131_080, 200_000, 262_100] {
            let mut buf = Cursor::new(Vec::<u8>::new());
            {
                let mut w = RecordWriter::new(
                    &mut buf,
                    WriterOptions::new().compression(CompressionType::Zstd),
                )
                .expect("writer new ok");
                for _ in 0..n {
                    w.write_record(b"").expect("write ok");
                }
                w.flush().expect("flush ok");
            }
            let data = buf.into_inner();
            max_interior = max_interior.max(interior_boundaries(&data));

            let mut reader =
                RecordReader::new(Cursor::new(data), ReaderOptions::new()).expect("reader new ok");
            let mut count = 0usize;
            loop {
                match reader.read_record() {
                    Ok(Some(rec)) => {
                        assert!(rec.is_empty(), "n={n}: record {count} not empty");
                        count += 1;
                    }
                    Ok(None) => break,
                    Err(e) => panic!("n={n}: read failed after {count} records: {e:?}"),
                }
            }
            assert_eq!(count, n, "n={n}: record count mismatch");
        }
        assert!(
            max_interior >= 2,
            "sweep never produced padding crossing two block boundaries \
             (max interior boundaries: {max_interior}); the old overshoot \
             bug is only observable past the second boundary"
        );
    }

    /// Spec tests for the C++ chunk-position convention: sweep
    /// `chunk_begin + num_records` landing at boundary+δ for
    /// δ ∈ {0, 1, 11, 24, 25, 26}, checking the writer's byte layout and that
    /// the reader lands on the following chunk exactly.
    ///
    /// The record chunk begins at 64, so num_records = 131072 + δ - 64 puts
    /// the round-up input at the second block boundary + δ. Expected chunk
    /// ends (canonical addresses): δ=0 → the boundary itself; δ ∈ {1..24} →
    /// boundary+25; δ=25 → boundary+25; δ=26 → boundary+26.
    #[test]
    #[cfg(feature = "zstd")]
    fn chunk_position_convention_delta_sweep() {
        let boundary: u64 = 131_072; // 2 * 65536

        for (delta, expected_next_chunk) in [
            (0u64, boundary),
            (1, boundary + 25),
            (11, boundary + 25),
            (24, boundary + 25),
            (25, boundary + 25),
            (26, boundary + 26),
        ] {
            let n = (boundary + delta - 64) as usize;
            let mut buf = Cursor::new(Vec::<u8>::new());
            {
                let mut w = RecordWriter::new(
                    &mut buf,
                    WriterOptions::new().compression(CompressionType::Zstd),
                )
                .expect("writer new ok");
                for _ in 0..n {
                    w.write_record(b"").expect("write ok");
                }
                w.flush().expect("flush ok");
                w.write_record(b"marker").expect("write ok");
                w.flush().expect("flush ok");
            }
            let data = buf.into_inner();

            // Writer-side byte layout: the block header at the boundary.
            let bh = crate::block_header::BlockHeader::from_bytes(
                data[boundary as usize..boundary as usize + 24]
                    .try_into()
                    .unwrap(),
            );
            assert!(bh.is_valid(), "delta={delta}: block header hash invalid");
            if delta == 0 {
                // The padded chunk ends AT the boundary; the marker chunk is
                // boundary-coincident, so the block header belongs to it:
                // previous_chunk = 0, header bytes at boundary+24.
                assert_eq!(bh.previous_chunk(), 0, "delta=0 previous_chunk");
            } else {
                // The boundary falls inside the padded chunk (which began at
                // 64): previous_chunk = boundary - 64, next_chunk = distance
                // to the following chunk address.
                assert_eq!(
                    bh.previous_chunk(),
                    boundary - 64,
                    "delta={delta} previous_chunk"
                );
                assert_eq!(
                    bh.next_chunk(),
                    expected_next_chunk - boundary,
                    "delta={delta} next_chunk"
                );
            }

            // Reader-side: all records read, marker lands at the expected
            // canonical chunk address, clean EOF.
            let mut reader = crate::record_reader::RecordReader::new(
                Cursor::new(data),
                crate::record_reader::ReaderOptions::new(),
            )
            .expect("reader new ok");
            let mut count = 0usize;
            let mut marker_pos = None;
            while let Some(rec) = reader.read_record().expect("read ok") {
                count += 1;
                if rec == b"marker" {
                    marker_pos = Some(reader.last_pos());
                }
            }
            assert_eq!(count, n + 1, "delta={delta}: record count");
            let marker_pos = marker_pos.expect("marker record present");
            assert_eq!(
                marker_pos.chunk_begin, expected_next_chunk,
                "delta={delta}: marker chunk address"
            );

            // Seek back to the marker by its RecordPosition.
            reader.seek(marker_pos).expect("seek ok");
            assert_eq!(
                reader.read_record().expect("read after seek").as_deref(),
                Some(&b"marker"[..]),
                "delta={delta}: marker after seek"
            );

            // δ=0: the alias address (boundary + 24) must reach the same chunk.
            if delta == 0 {
                let alias = crate::record_position::RecordPosition::new(boundary + 24, 0);
                reader.seek(alias).expect("alias seek ok");
                assert_eq!(
                    reader
                        .read_record()
                        .expect("read after alias seek")
                        .as_deref(),
                    Some(&b"marker"[..]),
                    "delta=0: alias address must resolve to the same chunk"
                );
            }
        }
    }

    /// Acceptance data point verified against the real C++
    /// implementation: 131,019 empty records with zstd → record chunk at 64,
    /// C++ pads to exactly 131097 (= 131072 + 25, since 64 + 131019 =
    /// 131072 + 11); the next chunk header starts there; the block header at
    /// 131072 carries previous_chunk = 131008 and next_chunk = 25.
    #[test]
    #[cfg(feature = "zstd")]
    fn chunk_position_convention_cpp_acceptance_point() {
        let n = 131_019usize;
        let mut buf = Cursor::new(Vec::<u8>::new());
        {
            let mut w = RecordWriter::new(
                &mut buf,
                WriterOptions::new().compression(CompressionType::Zstd),
            )
            .expect("writer new ok");
            for _ in 0..n {
                w.write_record(b"").expect("write ok");
            }
            w.flush().expect("flush ok");
            w.write_record(b"next").expect("write ok");
            w.flush().expect("flush ok");
        }
        let data = buf.into_inner();

        let bh = crate::block_header::BlockHeader::from_bytes(
            data[131_072..131_096].try_into().unwrap(),
        );
        assert!(bh.is_valid());
        assert_eq!(bh.previous_chunk(), 131_008);
        assert_eq!(bh.next_chunk(), 25);

        let mut reader = crate::record_reader::RecordReader::new(
            Cursor::new(data),
            crate::record_reader::ReaderOptions::new(),
        )
        .expect("reader new ok");
        let mut count = 0usize;
        let mut last_begin = 0;
        while reader.read_record().expect("read ok").is_some() {
            count += 1;
            last_begin = reader.last_pos().chunk_begin;
        }
        assert_eq!(count, n + 1);
        assert_eq!(last_begin, 131_097, "next chunk header starts at 131097");
    }

    /// Same shape, but with a second chunk after the padded one: the next
    /// chunk must begin exactly at the computed chunk end, which only holds
    /// if padding stopped there.
    #[test]
    #[cfg(feature = "zstd")]
    fn chunk_after_record_count_padding() {
        use crate::record_reader::{ReaderOptions, RecordReader};

        let n = 131_020usize;
        let mut buf = Cursor::new(Vec::<u8>::new());
        {
            let mut w = RecordWriter::new(
                &mut buf,
                WriterOptions::new().compression(CompressionType::Zstd),
            )
            .expect("writer new ok");
            for _ in 0..n {
                w.write_record(b"").expect("write ok");
            }
            w.flush().expect("flush ok");
            w.write_record(b"after-the-padding").expect("write ok");
            w.flush().expect("flush ok");
        }
        let data = buf.into_inner();

        let mut reader =
            RecordReader::new(Cursor::new(data), ReaderOptions::new()).expect("reader new ok");
        let mut count = 0usize;
        let mut last = Vec::new();
        while let Some(rec) = reader.read_record().expect("read ok") {
            count += 1;
            last = rec;
        }
        assert_eq!(count, n + 1);
        assert_eq!(last, b"after-the-padding");
    }

    // -----------------------------------------------------------------------
    // Once any operation fails, the writer stays failed — mirroring the C++
    // writer, where every public entry point checks `ok()` and refuses to
    // continue after a failure instead of writing more bytes at positions
    // that no longer match the underlying stream.
    // -----------------------------------------------------------------------

    struct FailingSink {
        written: usize,
        fail_after: usize,
    }

    impl Write for FailingSink {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            if self.written + buf.len() > self.fail_after {
                return Err(std::io::Error::other("sink failed"));
            }
            self.written += buf.len();
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn writer_latches_failure() {
        // Accept exactly the 64-byte file header (block header + signature
        // chunk header), then fail on the first data chunk.
        let sink = FailingSink {
            written: 0,
            fail_after: 64,
        };
        let mut w = RecordWriter::new(sink, WriterOptions::new()).expect("signature fits");
        w.write_record(b"hello").expect("record is buffered");
        assert!(w.flush().is_err(), "flush must surface the sink error");
        // The writer is now poisoned: it must not accept (and silently drop)
        // further records, and a retried flush must not report success.
        assert!(
            w.write_record(b"more").is_err(),
            "write_record after a failure must return an error"
        );
        assert!(
            w.flush().is_err(),
            "flush after a failure must return an error, not silently succeed"
        );
        assert!(
            w.close().is_err(),
            "close after a failure must return an error"
        );
    }

    // -----------------------------------------------------------------------
    // write_padding_to_multiple mirrors the C++ records_internal::PosAfterPadding
    // arithmetic (chunk_writer.cc), with an extra guard so that spans crossing a
    // block boundary never leave less than a chunk header of usable space.
    // -----------------------------------------------------------------------

    /// Expected positions follow C++ `PosAfterPadding`:
    /// pos 64, padding 17 → length 4 → bumped to 55 (>= chunk header) → end 119.
    #[test]
    fn padding_small_non_power_of_two_alignment() {
        let data = write_file(&[], WriterOptions::new().final_padding(17));
        assert_eq!(
            data.len() % 17,
            0,
            "file must be a multiple of 17, got {} bytes",
            data.len()
        );
        assert_eq!(data.len(), 119, "C++ PosAfterPadding(64, 17) == 119");
    }

    /// pos 64, padding 104: the gap is exactly one chunk header (40 bytes), so a
    /// zero-data padding chunk is written, exactly as C++ does (no extra bump).
    #[test]
    fn padding_zero_data_chunk_when_gap_is_exactly_header() {
        let data = write_file(&[], WriterOptions::new().final_padding(104));
        assert_eq!(data.len(), 104, "C++ PosAfterPadding(64, 104) == 104");
    }

    fn writer_at(pos: u64) -> RecordWriter<std::io::Cursor<Vec<u8>>> {
        let mut w =
            RecordWriter::new(std::io::Cursor::new(Vec::new()), WriterOptions::new()).unwrap();
        // Simulate a writer whose chunks ended at `pos`; only `file_pos` drives
        // the padding arithmetic.
        w.file_pos = pos;
        w
    }

    /// pos 64, padding 65560: the first candidate end (65560) has block offset 24
    /// — immediately after a block header — which C++ `IsPossibleChunkBoundary`
    /// rejects, so the end is bumped by one more multiple to 131120.
    #[test]
    fn padding_end_inside_block_header_is_bumped() {
        let mut w = writer_at(64);
        w.write_padding_to_multiple(65560).unwrap();
        assert_eq!(
            w.file_pos, 131120,
            "C++ PosAfterPadding(64, 65560) == 131120"
        );
        assert_eq!(w.file_pos % 65560, 0);
    }

    /// pos 65530, padding 60: the first candidate span (50 file bytes) crosses the
    /// block boundary at 65536, leaving only 26 usable bytes — less than a chunk
    /// header. The C++ arithmetic would underflow here; we extend to the next
    /// multiple instead of panicking or wrapping.
    #[test]
    fn padding_span_crossing_block_header_does_not_underflow() {
        let mut w = writer_at(65530);
        w.write_padding_to_multiple(60).unwrap();
        assert_eq!(w.file_pos % 60, 0);
        assert_eq!(w.file_pos, 65640);
    }

    /// Same as above with a small alignment: pos 65530, padding 17 → candidate
    /// span 56 bytes crosses the boundary (32 usable < 40), extended to 65603.
    #[test]
    fn padding_small_alignment_crossing_block_header() {
        let mut w = writer_at(65530);
        w.write_padding_to_multiple(17).unwrap();
        assert_eq!(w.file_pos % 17, 0);
        assert_eq!(w.file_pos, 65603);
    }

    // -----------------------------------------------------------------------
    // Compression tuning options are validated eagerly at construction,
    // matching the C++ `CompressorOptions` setters which assert the ranges
    // at option-set time (compressor_options.h).
    // -----------------------------------------------------------------------

    fn try_new(options: WriterOptions) -> Result<(), RiegeliError> {
        RecordWriter::new(std::io::Cursor::new(Vec::<u8>::new()), options).map(|_| ())
    }

    #[test]
    #[cfg(feature = "zstd")]
    fn zstd_options_out_of_range_rejected_at_construction() {
        // C++ ZstdWriterBase::Options: compression_level in -131072..=22,
        // window_log in 10..=31 (64-bit build).
        for level in [-131073, 23] {
            let opts = WriterOptions::new()
                .compression(CompressionType::Zstd)
                .compression_level(level);
            assert!(
                try_new(opts).is_err(),
                "zstd compression_level({level}) must be rejected at construction"
            );
        }
        for log in [9u32, 32] {
            let opts = WriterOptions::new()
                .compression(CompressionType::Zstd)
                .window_log(Some(log));
            assert!(
                try_new(opts).is_err(),
                "zstd window_log({log}) must be rejected at construction"
            );
        }
        // Boundary values are accepted.
        for level in [-131072, 22] {
            let opts = WriterOptions::new()
                .compression(CompressionType::Zstd)
                .compression_level(level);
            assert!(
                try_new(opts).is_ok(),
                "zstd compression_level({level}) is valid"
            );
        }
        for log in [10u32, 31] {
            let opts = WriterOptions::new()
                .compression(CompressionType::Zstd)
                .window_log(Some(log));
            assert!(try_new(opts).is_ok(), "zstd window_log({log}) is valid");
        }
    }

    #[test]
    #[cfg(feature = "brotli")]
    fn brotli_options_out_of_range_rejected_at_construction() {
        // C++ BrotliWriterBase::Options: compression_level in 0..=11,
        // window_log in 10..=30. The Rust writer must reject out-of-range
        // values instead of silently clamping them.
        for level in [-1, 12] {
            let opts = WriterOptions::new()
                .compression(CompressionType::Brotli)
                .compression_level(level);
            assert!(
                try_new(opts).is_err(),
                "brotli compression_level({level}) must be rejected at construction"
            );
        }
        for log in [9u32, 31] {
            let opts = WriterOptions::new()
                .compression(CompressionType::Brotli)
                .window_log(Some(log));
            assert!(
                try_new(opts).is_err(),
                "brotli window_log({log}) must be rejected at construction"
            );
        }
        // Boundary values are accepted.
        for level in [0, 11] {
            let opts = WriterOptions::new()
                .compression(CompressionType::Brotli)
                .compression_level(level);
            assert!(
                try_new(opts).is_ok(),
                "brotli compression_level({level}) is valid"
            );
        }
        for log in [10u32, 30] {
            let opts = WriterOptions::new()
                .compression(CompressionType::Brotli)
                .window_log(Some(log));
            assert!(try_new(opts).is_ok(), "brotli window_log({log}) is valid");
        }
    }
}
