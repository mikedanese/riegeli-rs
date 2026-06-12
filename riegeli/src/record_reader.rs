//! `RecordReader` — reads a Riegeli file from any `Read + Seek` source.
//!
//! ## Reading algorithm
//!
//! The reader maintains a "next chunk file position" cursor. On each `read_record()` call:
//! 1. If the current chunk decoder has records remaining, yield the next one.
//! 2. Otherwise, advance to the next chunk: skip any block header at block boundaries,
//!    read a 40-byte `ChunkHeader`, read `data_size` bytes, validate, decode.
//! 3. If a hash validation fails:
//!    - Without recovery: return `Err`.
//!    - With recovery: compute the invalid region, call the callback with
//!      it, and — if the callback returns `true` — resume reading at the
//!      region's end (exactly one bad chunk when its header is trustworthy,
//!      the next block boundary otherwise).

use std::cmp::Ordering;
use std::io::{Read, Seek, SeekFrom};

use crate::block_arithmetic::{is_block_boundary, round_down_to_block_boundary};
use crate::block_header::BlockHeader;
use crate::chunk_header::{ChunkHeader, ChunkType};
use crate::constants::{BLOCK_HEADER_SIZE, BLOCK_SIZE, CHUNK_HEADER_SIZE};
use crate::error::RiegeliError;
use crate::field_projection::FieldProjection;
use crate::record_position::RecordPosition;
use crate::simple_chunk::{Chunk, SimpleChunkDecoder};
use crate::transpose::decoder::TransposeChunkDecoder;

/// Type alias for the optional recovery callback.
///
/// C++ equivalent: `std::function<bool(const SkippedRegion&, RecordReaderBase&)>`.
/// The Rust callback does not receive the reader (it is owned by the reader,
/// so a mutable reference would alias); after a cancelled operation returns,
/// the caller repositions the reader itself, and
/// [`RecordReader::last_skipped_region`] exposes the region that was
/// reported.
type RecoveryCallback = Option<Box<dyn FnMut(&crate::SkippedRegion) -> bool>>;

/// Options for configuring a [`RecordReader`].
pub struct ReaderOptions {
    recovery: RecoveryCallback,
    /// Optional field projection for column pruning in transpose chunks.
    field_projection: Option<FieldProjection>,
}

impl ReaderOptions {
    /// Create `ReaderOptions` with default settings (no recovery, no projection).
    pub fn new() -> Self {
        Self {
            recovery: None,
            field_projection: None,
        }
    }

    /// Set a recovery callback invoked when a corrupted region is
    /// encountered.
    ///
    /// The callback receives the [`SkippedRegion`](crate::SkippedRegion)
    /// about to be skipped. Returning `true` skips the region and continues
    /// the operation; returning `false` cancels it, and the operation
    /// returns the original error.
    ///
    /// The region's `end()` is exactly where the reader resumes: one bad
    /// chunk when its header is trustworthy (hash-valid with
    /// stream-bounded claims), the next block boundary otherwise.
    ///
    /// Divergences from the C++ `set_recovery`
    /// (`std::function<bool(const SkippedRegion&, RecordReaderBase&)>`):
    ///
    /// - No reader parameter: the reader owns the callback, so passing it a
    ///   mutable reference would alias. Use
    ///   [`RecordReader::last_skipped_region`] after a cancelled call.
    /// - Cancelling follows the C++ shape: the region is already skipped
    ///   when the callback runs, so returning `false` reports the error
    ///   once and the NEXT operation continues past the rejected region —
    ///   the callback is never re-invoked for a region it rejected, and a
    ///   retry loop makes progress. A caller that wants to stop AT the
    ///   damage can take [`RecordReader::last_skipped_region`] and
    ///   `seek` to its `begin()`.
    pub fn recovery<F: FnMut(&crate::SkippedRegion) -> bool + 'static>(mut self, f: F) -> Self {
        self.recovery = Some(Box::new(f));
        self
    }

    /// Set a `FieldProjection` to enable column pruning for transpose chunks.
    ///
    /// When set to a non-`all()` projection, the `TransposeChunkDecoder` will
    /// skip data buffers for fields not in the projection and filter decoded
    /// records to contain only the projected fields.
    ///
    /// Non-proto records and simple (non-transpose) chunks are returned unchanged.
    pub fn field_projection(mut self, proj: FieldProjection) -> Self {
        self.field_projection = Some(proj);
        self
    }
}

impl Default for ReaderOptions {
    fn default() -> Self {
        Self::new()
    }
}

/// Active chunk decoder — either simple or transposed.
enum ActiveDecoder {
    Simple(SimpleChunkDecoder),
    Transposed(TransposeChunkDecoder),
}

impl ActiveDecoder {
    // NOTE: the recovery design relies on decoders being structurally
    // infallible after construction — both variants slice pre-validated
    // ranges out of pre-decoded buffers, so the Result below has no
    // reachable Err today. That is what lets every recoverable failure be
    // position-stable at a chunk boundary (the C++ kRecoverChunkDecoder
    // sub-chunk case collapses to the chunk case here). If a streaming or
    // lazy decoder is ever introduced, mid-chunk failures become possible
    // and need their own position-stability and SkippedRegion story.
    fn read_record(&mut self) -> Result<Option<Vec<u8>>, RiegeliError> {
        match self {
            ActiveDecoder::Simple(d) => d.read_record(),
            ActiveDecoder::Transposed(d) => d.read_record(),
        }
    }
}

/// A reader that parses a Riegeli file record by record.
pub struct RecordReader<R: Read + Seek> {
    /// The underlying I/O source.
    reader: R,
    /// Optional recovery callback for corrupted regions.
    recovery: RecoveryCallback,
    /// Optional field projection for column pruning in transpose chunks.
    field_projection: Option<FieldProjection>,
    /// File position of the chunk currently being decoded (its `ChunkHeader` starts here).
    current_chunk_begin: u64,
    /// File position where the NEXT chunk header will be read from.
    next_chunk_file_pos: u64,
    /// The decoder for the current chunk, if one has been loaded.
    current_decoder: Option<ActiveDecoder>,
    /// How many records have been yielded from the current chunk.
    current_record_index: u64,
    /// Logical read-cursor position: points at the next record to be returned.
    pos: RecordPosition,
    /// Position of the last successfully read record.
    last_pos: RecordPosition,
    /// True once we've hit EOF (no more chunks).
    at_eof: bool,
    /// True if the last record was read from a valid (non-recovered) chunk.
    last_record_is_valid: bool,
    /// Stream length as last measured (re-measured on demand if a chunk's
    /// claims exceed it, so a file growing between reads keeps working).
    /// Bounds header-claimed sizes before they drive arithmetic or
    /// allocation — the header hash proves integrity, not honesty.
    stream_len: u64,
    /// The most recent region reported to the recovery callback (whether it
    /// continued or cancelled). `None` until the callback first fires.
    last_skipped_region: Option<crate::SkippedRegion>,
    /// Failure-time classification for recovery: `Some(chunk_end)` when the
    /// failing chunk's header was hash-valid with stream-bounded claims at
    /// the MOMENT of failure (its extent is trustworthy), `None` otherwise.
    /// Set by the error sites, consumed by try_recover_at — never derived
    /// by re-reading, which is a time-of-check/time-of-failure hazard (a
    /// stream that grows between failure and recovery could reclassify an
    /// untrusted failure as trusted and skip a readable chunk).
    pending_trusted_end: Option<u64>,
}

impl<R: Read + Seek> RecordReader<R> {
    /// Open a Riegeli file.
    ///
    /// Validates the initial block header and signature chunk, then positions
    /// the reader at the first data chunk.
    pub fn new(mut reader: R, options: ReaderOptions) -> Result<Self, RiegeliError> {
        let stream_len = reader.seek(SeekFrom::End(0))?;
        reader.seek(SeekFrom::Start(0))?;

        // Read and validate the first block header at offset 0.
        let mut bh_bytes = [0u8; 24]; // BLOCK_HEADER_SIZE
        reader.read_exact(&mut bh_bytes)?;
        let block_hdr = BlockHeader::from_bytes(bh_bytes);
        if !block_hdr.is_valid() {
            return Err(RiegeliError::MalformedData(
                "invalid block header hash at offset 0".into(),
            ));
        }

        // Validate the signature chunk at offset 24 by exact comparison: the
        // riegeli file signature is a fixed 40-byte constant (empty data,
        // zero records). Comparing bytes — rather than checking the hash and
        // type and then trusting the header's claimed sizes — does no
        // arithmetic on attacker-controlled values at all: a hash-valid
        // signature header claiming a huge data_size used to overflow the
        // position sum in debug and seek backward through the i64 cast in
        // release. This matches the C++ reader, which verifies the
        // signature bytes.
        let mut ch_bytes = [0u8; 40]; // CHUNK_HEADER_SIZE
        reader.read_exact(&mut ch_bytes)?;
        let canonical = ChunkHeader::from_parts(&[], ChunkType::FileSignature, 0, 0).to_bytes();
        if ch_bytes != canonical {
            return Err(RiegeliError::MalformedData(
                "invalid file signature chunk at offset 24".into(),
            ));
        }

        // File position after the signature chunk: 24 (BH) + 40 (CH) + 0 = 64.
        let next_chunk_file_pos = BLOCK_HEADER_SIZE + CHUNK_HEADER_SIZE;

        // Initial position matches the C++ reference: numeric 0 (the
        // beginning of the file), not the first chunk's canonical address
        // 24 — verified by the differential harness (an earlier criterion
        // documented 24; the reference disagrees and wins).
        let initial_pos = RecordPosition::new(0, 0);

        Ok(Self {
            reader,
            recovery: options.recovery,
            field_projection: options.field_projection,
            current_chunk_begin: BLOCK_HEADER_SIZE,
            next_chunk_file_pos,
            current_decoder: None,
            current_record_index: 0,
            pos: initial_pos,
            last_pos: initial_pos,
            at_eof: false,
            last_record_is_valid: true,
            stream_len,
            last_skipped_region: None,
            pending_trusted_end: None,
        })
    }

    /// Read the next record from the file.
    ///
    /// Returns `Ok(Some(bytes))` for a record, `Ok(None)` at EOF, or `Err` on
    /// unrecoverable corruption (when no recovery callback is set).
    pub fn read_record(&mut self) -> Result<Option<Vec<u8>>, RiegeliError> {
        self.pending_trusted_end = None;
        loop {
            if self.at_eof {
                return Ok(None);
            }

            // If we have an active decoder, try to get a record from it.
            if let Some(decoder) = &mut self.current_decoder {
                match decoder.read_record()? {
                    Some(rec) => {
                        // Record successfully read.
                        let rec_pos = RecordPosition::new(
                            self.current_chunk_begin,
                            self.current_record_index,
                        );
                        self.last_pos = rec_pos;
                        self.current_record_index += 1;
                        self.pos = RecordPosition::new(
                            self.current_chunk_begin,
                            self.current_record_index,
                        );
                        self.last_record_is_valid = true;
                        return Ok(Some(rec));
                    }
                    None => {
                        // Current chunk exhausted; fall through to load next chunk.
                        self.current_decoder = None;
                    }
                }
            }

            // Try to load the next chunk.
            match self.load_next_chunk() {
                Ok(true) => {
                    // Chunk loaded; loop back to read from it.
                }
                Ok(false) => {
                    // EOF reached.
                    self.at_eof = true;
                    return Ok(None);
                }
                Err(e) => {
                    // Corruption detected. With recovery: report the region
                    // and — on `true` — resume at its end (C++: ReadRecord is
                    // retried). On `false` or without recovery, return the
                    // original error (the reader stays position-stable at
                    // the bad region; a bare retry re-reports it).
                    let at = self.next_chunk_file_pos;
                    if self.try_recover_at(at, &e) {
                        if self.at_eof {
                            // Resync seek failed (region end past a
                            // shrunken stream) — clean end of file.
                            return Ok(None);
                        }
                        // Continue reading from the region end.
                    } else {
                        return Err(e);
                    }
                }
            }
        }
    }

    /// Returns the current logical read position.
    ///
    /// Before the first `read_record()` call, returns `{ chunk_begin: 24, record_index: 0 }`.
    /// After reading records, points at the next record to be returned.
    pub fn pos(&self) -> RecordPosition {
        self.pos
    }

    /// Returns the position of the last successfully read record.
    ///
    /// Before any records have been read, returns `{ chunk_begin: 24, record_index: 0 }`.
    pub fn last_pos(&self) -> RecordPosition {
        self.last_pos
    }

    /// Seek to a specific record position.
    ///
    /// Loads the chunk at `pos.chunk_begin` and skips `pos.record_index` records.
    pub fn seek(&mut self, pos: RecordPosition) -> Result<(), RiegeliError> {
        self.pending_trusted_end = None;
        // Seek to the chunk_begin, accounting for block headers.
        let chunk_file_pos = pos.chunk_begin;

        self.reader.seek(SeekFrom::Start(chunk_file_pos))?;

        // Load that chunk.
        self.current_decoder = None;
        self.at_eof = false;
        self.next_chunk_file_pos = chunk_file_pos;
        self.current_chunk_begin = chunk_file_pos;
        self.current_record_index = 0;

        // Load the chunk at this position.
        match self.load_chunk_at(chunk_file_pos) {
            Ok(Some(decoder)) => {
                self.current_decoder = Some(decoder);
            }
            Ok(None) => {
                self.at_eof = true;
                self.pos = pos;
                // last_pos deliberately NOT updated: it tracks the last
                // successfully READ record (seek_back's target), and a seek
                // reads nothing.
                return Ok(());
            }
            Err(e) => {
                // C++: Seek returns the result of the recovery function.
                // On `true` the reader is positioned at the region end.
                let at = self.next_chunk_file_pos;
                if self.try_recover_at(at, &e) {
                    self.pos = RecordPosition::new(self.next_chunk_file_pos, 0);
                    return Ok(());
                }
                return Err(e);
            }
        }

        // Skip record_index records.
        for _ in 0..pos.record_index {
            if let Some(ref mut dec) = self.current_decoder {
                match dec.read_record()? {
                    Some(_) => {
                        self.current_record_index += 1;
                    }
                    None => {
                        return Err(RiegeliError::MalformedData(
                            format!(
                                "seek: record_index {} is out of range for chunk at {}",
                                pos.record_index, chunk_file_pos
                            )
                            .into(),
                        ));
                    }
                }
            }
        }

        self.pos = pos;
        // last_pos deliberately NOT updated — see the EOF arm above.
        Ok(())
    }

    /// Seek to the record at or after file position `numeric`.
    ///
    /// Interprets `numeric` as `chunk_begin + record_index` (from `RecordPosition::numeric()`).
    /// Scans forward through the file to find the chunk where `chunk_begin <= numeric`
    /// and returns positioned at `record_index = numeric - chunk_begin` within that chunk.
    pub fn seek_numeric(&mut self, numeric: u64) -> Result<(), RiegeliError> {
        self.pending_trusted_end = None;
        // Scan from the first data chunk (offset 64) to find the right chunk.
        // We need to find a chunk where chunk_begin <= numeric < chunk_begin + num_records.
        // If no such chunk exists, seek to the first chunk at/after numeric.

        let first_data_chunk = BLOCK_HEADER_SIZE + CHUNK_HEADER_SIZE; // 64

        // Start scan from the beginning of data chunks.
        let mut scan_pos = first_data_chunk;

        loop {
            // peek_chunk_header canonicalizes and skips block headers itself,
            // so scan_pos stays a canonical chunk address. (Pre-skipping here
            // would turn a boundary-coincident chunk's address into the
            // boundary+24 alias and shift its numeric positions by 24.)
            match self.peek_chunk_header(scan_pos) {
                Ok(None) => {
                    // EOF — seek to end.
                    self.at_eof = true;
                    self.pos = RecordPosition::new(scan_pos, 0);
                    // last_pos keeps the last successfully read record.
                    self.current_decoder = None;
                    return Ok(());
                }
                Err(e) if self.recovery.is_some() => {
                    // Skip the invalid region and keep scanning, or cancel
                    // with the original error.
                    if self.try_recover_at(scan_pos, &e) {
                        scan_pos = self.next_chunk_file_pos;
                        continue;
                    }
                    return Err(e);
                }
                Ok(Some(ch)) => {
                    let chunk_begin = scan_pos;
                    let num_records = ch.num_records();
                    let data_size = ch.data_size();

                    if matches!(
                        ch.chunk_type(),
                        Ok(ChunkType::Simple) | Ok(ChunkType::Transposed)
                    ) {
                        if chunk_begin <= numeric && numeric < chunk_begin + num_records {
                            let record_index = numeric - chunk_begin;
                            return self.seek(RecordPosition::new(chunk_begin, record_index));
                        } else if chunk_begin > numeric {
                            return self.seek(RecordPosition::new(chunk_begin, 0));
                        }
                    }

                    // Advance to the next chunk.
                    let chunk_header_file_pos = scan_pos;
                    scan_pos = crate::block_arithmetic::chunk_end(
                        chunk_header_file_pos,
                        data_size,
                        num_records,
                    );
                }
                Err(e) => return Err(e),
            }
        }
    }

    /// Returns `true` since file-based I/O supports seeking.
    pub fn supports_random_access(&self) -> bool {
        true
    }

    /// Read the file metadata chunk as a typed `RecordsMetadata` proto, if present.
    ///
    /// Peeks at the chunk immediately after the file signature (offset 64) to check
    /// if it is a `ChunkType::FileMetadata` chunk. If so, parses and returns the
    /// `RecordsMetadata` message. Does not change the current read position.
    pub fn read_metadata(&mut self) -> Result<Option<crate::RecordsMetadata>, RiegeliError> {
        use protobuf::Parse;
        match self.read_serialized_metadata()? {
            Some(bytes) => {
                let msg = crate::RecordsMetadata::parse(&bytes).map_err(|e| {
                    RiegeliError::MalformedData(
                        format!("failed to parse RecordsMetadata: {e}").into(),
                    )
                })?;
                Ok(Some(msg))
            }
            None => Ok(None),
        }
    }

    /// Read the file metadata chunk as raw bytes, if present.
    ///
    /// Like [`read_metadata`](Self::read_metadata), but returns the raw serialized
    /// proto bytes without parsing. Does not change the current read position —
    /// including under recovery: corruption at the metadata position is
    /// reported to the callback but the region is never consumed, so a
    /// repeated call reports the same region again (the one deliberate
    /// exception to the consumed-once cancel contract, because consuming
    /// would move a position this method promises not to touch).
    pub fn read_serialized_metadata(&mut self) -> Result<Option<Vec<u8>>, RiegeliError> {
        self.pending_trusted_end = None;
        // The metadata chunk, if present, is at offset 64 (right after signature).
        let metadata_chunk_pos = BLOCK_HEADER_SIZE + CHUNK_HEADER_SIZE; // = 64

        // Peek at the chunk header at offset 64. A clean EOF (file ends
        // before any chunk) means no metadata; a real error — hash-invalid
        // header, impossible claims, I/O failure — is corruption and must
        // not be reported as "no metadata": a caller inspecting metadata
        // first would proceed as if the file were clean.
        let ch = match self.peek_chunk_header(metadata_chunk_pos) {
            Ok(Some(ch)) => ch,
            Ok(None) => return Ok(None),
            Err(e) => {
                // C++: ReadMetadata returns the result of the recovery
                // function — on `true` the file simply has no (readable)
                // metadata. REPORT-ONLY: this method's contract is that it
                // does not change the read position (C++'s ReadMetadata is
                // a sequential read with no such promise, so there is no
                // reference position behavior to match — ours must honor
                // our own documentation). Repositioning here rewound a
                // mid-stream reader to the skipped region's end, replaying
                // or dropping records.
                if self.report_region_at(metadata_chunk_pos, &e) == Some(true) {
                    return Ok(None);
                }
                return Err(e);
            }
        };

        if !matches!(ch.chunk_type(), Ok(ChunkType::FileMetadata)) {
            return Ok(None);
        }

        // Read the chunk data. The metadata chunk header is at offset 64, far
        // from any block boundary, so its data always begins at 64 + 40.
        let data = self.read_chunk_data(metadata_chunk_pos + CHUNK_HEADER_SIZE, ch.data_size())?;

        // Validate data hash.
        if !ch.is_data_valid(&data) {
            return Err(RiegeliError::MalformedData(
                "metadata chunk data hash mismatch".into(),
            ));
        }

        Ok(Some(data))
    }

    /// Change the active field projection, taking effect at the next chunk boundary.
    ///
    /// The current chunk decoder (if any) continues with the old projection until
    /// it is exhausted. New chunks loaded after this call will use the new projection.
    ///
    /// To switch back to returning all fields, pass `FieldProjection::all()`.
    pub fn set_field_projection(&mut self, proj: FieldProjection) {
        self.field_projection = if proj.is_all() { None } else { Some(proj) };
    }

    /// Binary search for a record in a sorted file.
    ///
    /// `test` is called with the bytes of individual records; it should return
    /// `Ordering::Less` if the target is after this record, `Ordering::Greater`
    /// if before, and `Ordering::Equal` if this is the target record.
    ///
    /// After a successful search, the reader is positioned so that the next
    /// `read_record()` returns the found record.
    ///
    /// Returns `Ok(true)` if a record for which `test` returns `Equal` was found,
    /// `Ok(false)` if the target does not exist in the file.
    ///
    /// The search reads at most O(log N) records where N is the total number of records.
    pub fn search<F>(&mut self, mut test: F) -> Result<bool, RiegeliError>
    where
        F: FnMut(&[u8]) -> Ordering,
    {
        // Collect all data chunk positions and their record counts.
        let chunks = self.collect_data_chunks()?;

        if chunks.is_empty() {
            self.at_eof = true;
            return Ok(false);
        }

        // Binary search over chunks using the first record of each chunk as a pivot.
        // Invariant: if the target exists, it is in chunks[lo..hi].
        let mut lo = 0usize;
        let mut hi = chunks.len();

        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            let (chunk_pos, _num_records) = chunks[mid];

            // Read just the first record of this chunk to probe.
            let first_record = self.read_record_at(chunk_pos, 0)?;

            match test(&first_record) {
                Ordering::Less => {
                    // Target is after this chunk's first record → search right half.
                    lo = mid + 1;
                }
                Ordering::Greater => {
                    // Target is before this chunk's first record → search left half.
                    hi = mid;
                }
                Ordering::Equal => {
                    // First record of this chunk matches. Seek to it and return.
                    let target = crate::record_position::RecordPosition::new(chunk_pos, 0);
                    self.seek(target)?;
                    return Ok(true);
                }
            }
        }

        // lo == hi: the target might be inside chunks[lo-1].
        // That chunk's first record is < target (test returned Less), but a later
        // record in that chunk might equal the target.
        if lo > 0 {
            let (chunk_pos, num_records) = chunks[lo - 1];
            let found = self.binary_search_within_chunk(chunk_pos, num_records, &mut test)?;
            if found {
                return Ok(true);
            }
        }

        // Target not found in the file.
        self.at_eof = true;
        self.current_decoder = None;
        Ok(false)
    }

    /// Collect (file_pos, num_records) for all Simple and Transposed data chunks.
    ///
    /// Scans the entire file, reading only chunk headers (no data decompression).
    fn collect_data_chunks(&mut self) -> Result<Vec<(u64, u64)>, RiegeliError> {
        self.pending_trusted_end = None;
        let first_data_chunk = BLOCK_HEADER_SIZE + CHUNK_HEADER_SIZE; // = 64
        let mut scan_pos = first_data_chunk;
        let mut chunks = Vec::new();

        // Read chunk headers until EOF (skipping leading and interleaved
        // block headers). With recovery set, invalid regions are skipped and
        // the scan continues (C++: Search skips invalid regions while the
        // recovery function returns true).
        loop {
            let (ch, chunk_begin, _) = match self.read_chunk_header_at(scan_pos) {
                Ok(Some(v)) => v,
                Ok(None) => break,
                Err(e) => {
                    if self.try_recover_at(scan_pos, &e) {
                        scan_pos = self.next_chunk_file_pos;
                        continue;
                    }
                    return Err(e);
                }
            };
            if !ch.is_header_valid() {
                let e = RiegeliError::MalformedData(
                    format!("invalid chunk header at {chunk_begin} during search scan").into(),
                );
                if self.try_recover_at(scan_pos, &e) {
                    scan_pos = self.next_chunk_file_pos;
                    continue;
                }
                return Err(e);
            }

            let data_size = ch.data_size();
            let num_records = ch.num_records();

            // Zero-record chunks cannot serve as search pivots: probing one
            // via read_record_at falls through to the NEXT chunk's first
            // record, so the comparator sees a misattributed record and the
            // search can report a present target as absent.
            if num_records > 0
                && matches!(
                    ch.chunk_type(),
                    Ok(ChunkType::Simple) | Ok(ChunkType::Transposed)
                )
            {
                chunks.push((chunk_begin, num_records));
            }

            scan_pos = crate::block_arithmetic::chunk_end(chunk_begin, data_size, num_records);
        }

        Ok(chunks)
    }

    /// Read the record at `record_index` within the chunk at `chunk_pos`.
    ///
    /// Uses `seek()` to position at the exact record. Does NOT preserve reader state.
    fn read_record_at(
        &mut self,
        chunk_pos: u64,
        record_index: u64,
    ) -> Result<Vec<u8>, RiegeliError> {
        let target = crate::record_position::RecordPosition::new(chunk_pos, record_index);
        self.seek(target)?;
        match self.read_record()? {
            // Guard against misattribution: if the read fell through to a
            // different chunk (e.g. the requested chunk had no records),
            // report absence rather than another chunk's record.
            Some(rec) if self.current_chunk_begin == chunk_pos => Ok(rec),
            _ => Ok(Vec::new()),
        }
    }

    /// Binary search within a single chunk for a matching record.
    ///
    /// Uses O(log num_records) reads by seeking to specific record indices.
    /// On success, positions the reader at the matching record.
    fn binary_search_within_chunk<F>(
        &mut self,
        chunk_pos: u64,
        num_records: u64,
        test: &mut F,
    ) -> Result<bool, RiegeliError>
    where
        F: FnMut(&[u8]) -> Ordering,
    {
        if num_records == 0 {
            return Ok(false);
        }

        // Binary search over record indices [0, num_records).
        // Invariant: if the target is in this chunk, it is at index [lo, hi).
        // We already know record 0 gives test == Less (from the outer binary search).
        let mut lo = 1u64; // record 0 was already checked and returned Less
        let mut hi = num_records;

        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            let rec = self.read_record_at(chunk_pos, mid)?;

            match test(&rec) {
                Ordering::Less => {
                    lo = mid + 1;
                }
                Ordering::Greater => {
                    hi = mid;
                }
                Ordering::Equal => {
                    // Found the target. Position the reader at this record.
                    let target = crate::record_position::RecordPosition::new(chunk_pos, mid);
                    self.seek(target)?;
                    return Ok(true);
                }
            }
        }

        // Target not in this chunk.
        Ok(false)
    }

    /// Returns `true` if the most recently returned record came from a valid
    /// (non-recovered) chunk.
    ///
    /// Returns `true` initially (before any record is read) and after each
    /// successful record read. Returns `false` after a recovery callback fires
    /// due to a corrupted chunk.
    pub fn last_record_is_valid(&self) -> bool {
        self.last_record_is_valid
    }

    /// Seek to the previous record.
    ///
    /// After this call, the next `read_record()` returns the same record that
    /// was most recently returned by `read_record()`.
    ///
    /// Returns `Ok(true)` if there is a previous record to seek to.
    /// Returns `Ok(false)` if positioned at or before the first record.
    pub fn seek_back(&mut self) -> Result<bool, RiegeliError> {
        // The initial position: chunk_begin=24 (BLOCK_HEADER_SIZE), record_index=0.
        // If last_pos is the initial position, there is no previous record.
        // The initial position (no record read yet) is numeric 0.
        if self.last_pos.chunk_begin == 0 && self.last_pos.record_index == 0 {
            return Ok(false);
        }

        // Seek to the last successfully read record position.
        let target = self.last_pos;
        self.seek(target)?;
        Ok(true)
    }

    /// Return the total number of records in the file.
    ///
    /// Scans all chunk headers summing `num_records` without decompressing any
    /// record data. The current read position is preserved — the next
    /// `read_record()` after `size()` returns the same record it would have
    /// without the `size()` call.
    pub fn size(&mut self) -> Result<u64, RiegeliError> {
        // Save the current read state.
        let saved_pos = self.pos;
        let saved_last_pos = self.last_pos;
        let saved_next_chunk_file_pos = self.next_chunk_file_pos;
        let saved_current_chunk_begin = self.current_chunk_begin;
        let saved_current_record_index = self.current_record_index;
        let saved_at_eof = self.at_eof;
        let saved_last_record_is_valid = self.last_record_is_valid;

        // Scan from the first data chunk (offset 64).
        let first_data_chunk = BLOCK_HEADER_SIZE + CHUNK_HEADER_SIZE; // = 64
        let mut scan_pos = first_data_chunk;
        let mut total_records: u64 = 0;

        loop {
            // Read chunk header (skipping leading and interleaved block headers).
            let (ch, chunk_begin, _) = match self.read_chunk_header_at(scan_pos) {
                Ok(Some(v)) => v,
                Ok(None) => break, // EOF
                Err(e) => {
                    self.restore_state(
                        saved_pos,
                        saved_last_pos,
                        saved_next_chunk_file_pos,
                        saved_current_chunk_begin,
                        saved_current_record_index,
                        saved_at_eof,
                        saved_last_record_is_valid,
                    );
                    return Err(e);
                }
            };

            if !ch.is_header_valid() {
                self.restore_state(
                    saved_pos,
                    saved_last_pos,
                    saved_next_chunk_file_pos,
                    saved_current_chunk_begin,
                    saved_current_record_index,
                    saved_at_eof,
                    saved_last_record_is_valid,
                );
                return Err(RiegeliError::MalformedData(
                    format!("invalid chunk header at {chunk_begin} during size scan").into(),
                ));
            }

            let data_size = ch.data_size();
            let num_records = ch.num_records();

            if matches!(
                ch.chunk_type(),
                Ok(ChunkType::Simple) | Ok(ChunkType::Transposed)
            ) {
                total_records += num_records;
            }

            // Advance past this chunk.
            scan_pos = crate::block_arithmetic::chunk_end(chunk_begin, data_size, num_records);
        }

        // Restore state.
        self.restore_state(
            saved_pos,
            saved_last_pos,
            saved_next_chunk_file_pos,
            saved_current_chunk_begin,
            saved_current_record_index,
            saved_at_eof,
            saved_last_record_is_valid,
        );
        // Also reset current_decoder to None (we disrupted the internal state).
        // Seek back to restore the active decoder.
        if !saved_at_eof {
            // Special-case: if saved_pos is the initial position (before any
            // records have been read), seek() would try to load the chunk at
            // chunk_begin=24 (the signature chunk), which sets at_eof=true and
            // breaks subsequent reads. Instead, directly restore the initial state.
            let is_initial_position =
                saved_pos.chunk_begin == BLOCK_HEADER_SIZE && saved_pos.record_index == 0;

            if is_initial_position {
                // Restore directly without calling seek().
                self.next_chunk_file_pos = BLOCK_HEADER_SIZE + CHUNK_HEADER_SIZE; // = 64
                self.at_eof = false;
                self.current_decoder = None;
            } else {
                // Re-seek to restore decoder state.
                let _ = self.seek(saved_pos);
                // Restore last_pos and valid flag after seek changes them.
                self.last_pos = saved_last_pos;
                self.last_record_is_valid = saved_last_record_is_valid;
            }
        }

        Ok(total_records)
    }

    /// Validate all block and chunk headers and data hashes in the file.
    ///
    /// Does not decompress any record data — only validates the raw (possibly
    /// compressed) chunk data against the stored hash. Returns `Ok(())` if all
    /// headers and data hashes are valid, or `Err(RiegeliError::MalformedData(_))`
    /// on the first validation failure.
    ///
    /// The current read position is not changed by this method.
    pub fn check_file_format(&mut self) -> Result<(), RiegeliError> {
        // Validate the initial block header at offset 0.
        self.reader.seek(SeekFrom::Start(0))?;
        let mut bh_bytes = [0u8; 24];
        self.reader.read_exact(&mut bh_bytes)?;
        let bh = BlockHeader::from_bytes(bh_bytes);
        if !bh.is_valid() {
            return Err(RiegeliError::MalformedData(
                "invalid block header hash at offset 0".into(),
            ));
        }

        // Scan all chunks starting from the signature chunk (offset 24).
        let mut scan_pos: u64 = BLOCK_HEADER_SIZE; // = 24

        // Read chunk headers until EOF (skipping leading and interleaved block headers).
        while let Some((ch, chunk_begin, data_begin)) = self.read_chunk_header_at(scan_pos)? {
            if !ch.is_header_valid() {
                return Err(RiegeliError::MalformedData(
                    format!("invalid chunk header hash at offset {chunk_begin}").into(),
                ));
            }

            let data_size = ch.data_size();
            let num_records = ch.num_records();

            // Read the raw chunk data (without decompressing) and validate data hash.
            let chunk_data = self.read_chunk_data(data_begin, data_size)?;
            if !ch.is_data_valid(&chunk_data) {
                return Err(RiegeliError::MalformedData(
                    format!("chunk data hash mismatch at offset {chunk_begin}").into(),
                ));
            }

            // Advance past this chunk.
            scan_pos = crate::block_arithmetic::chunk_end(chunk_begin, data_size, num_records);
        }

        Ok(())
    }

    // Helper to restore reader state after a non-destructive scan.
    #[allow(clippy::too_many_arguments)]
    fn restore_state(
        &mut self,
        pos: RecordPosition,
        last_pos: RecordPosition,
        next_chunk_file_pos: u64,
        current_chunk_begin: u64,
        current_record_index: u64,
        at_eof: bool,
        last_record_is_valid: bool,
    ) {
        self.pos = pos;
        self.last_pos = last_pos;
        self.next_chunk_file_pos = next_chunk_file_pos;
        self.current_chunk_begin = current_chunk_begin;
        self.current_record_index = current_record_index;
        self.at_eof = at_eof;
        self.last_record_is_valid = last_record_is_valid;
        self.current_decoder = None;
    }

    // -------------------------------------------------------------------------
    // Internal helpers
    // -------------------------------------------------------------------------

    /// The most recent region reported to the recovery callback, whether the
    /// callback continued or cancelled. `None` if the callback never fired.
    ///
    /// This is the Rust stand-in for the reader parameter the C++ recovery
    /// callback receives: after a cancelled operation returns its error, the
    /// caller can inspect the region here and reposition explicitly.
    pub fn last_skipped_region(&self) -> Option<&crate::SkippedRegion> {
        self.last_skipped_region.as_ref()
    }

    /// Attempt recovery for `error`, which left the reader position-stable
    /// at `at` (every error path rewinds to the failed chunk — the
    /// persistence invariant).
    ///
    /// Computes the skipped region — COUPLED to the resync target by
    /// construction: `region.end` is exactly where reading resumes.
    /// A readable, hash-valid header (whose claims passed the stream bound)
    /// gives a trustworthy extent, so exactly that chunk is skipped
    /// (preserving siblings in the same block — more precise than a
    /// boundary skip). Otherwise the claims cannot be trusted and the
    /// resync is the next block boundary.
    ///
    /// Returns `true` if a callback is set, it returned `true`, and the
    /// reader was repositioned to the region end; `false` otherwise (the
    /// caller returns the original error).
    /// Compute the region for `error` and invoke the callback WITHOUT any
    /// repositioning side effects — for operations whose contract promises
    /// not to move the read cursor (metadata reads). Returns `None` when no
    /// callback is set, otherwise `Some(callback verdict)`.
    fn report_region_at(&mut self, at: u64, error: &RiegeliError) -> Option<bool> {
        // Consume the failure-time classification unconditionally so it can
        // never go stale across attempts or operations.
        let trusted_end = self.pending_trusted_end.take();
        self.recovery.as_ref()?;
        let begin = crate::block_arithmetic::canonical_chunk_address(at);
        let end = match trusted_end {
            // The failing chunk's extent was trustworthy WHEN it failed
            // (hash-valid header, stream-bounded claims): skip exactly it.
            Some(end) => end,
            // Header unreadable, hash-invalid, claims unvalidated, or I/O
            // failure: nothing about the extent can be trusted — resync at
            // the next block boundary, clamped to the stream length (a
            // region cannot extend past the file — except by the minimal
            // progress margin when corruption sits at EOF — and C++
            // reports EOF-ended regions the same way).
            None => {
                let boundary = next_block_boundary(begin);
                let boundary = if boundary == begin {
                    begin + BLOCK_SIZE
                } else {
                    boundary
                };
                boundary.min(self.stream_len.max(begin))
            }
        };
        // Forward progress no matter what the arithmetic said — measured
        // from the RESYNC ORIGIN `at`, not just `begin`: when `at` is the
        // boundary+24 alias of a canonical `begin` 24 bytes earlier, an
        // EOF-clamped end can satisfy end > begin while still equaling
        // `at`, and the reader would spin on the same region forever (a
        // past-EOF end is fine — the resync seek lands at EOF and reads
        // terminate).
        let end = end.max(begin + 1).max(at.saturating_add(1));
        // The resume position must never be a canonical ALIAS of this
        // region's own begin: an EOF-clamped end landing on begin's
        // boundary+24 alias would re-read the same corrupt bytes at
        // canonical begin and double-report overlapping regions (breaking
        // begin-monotonicity). The condition only triggers for that exact
        // alias, so the bump is at most one byte and cannot loop.
        let end = if crate::block_arithmetic::canonical_chunk_address(end) <= begin {
            end + 1
        } else {
            end
        };
        let region = crate::SkippedRegion::new(begin, end, error.to_string());
        self.last_skipped_region = Some(region);
        let cb = self.recovery.as_mut().expect("checked above");
        let region_ref = self.last_skipped_region.as_ref().expect("just set");
        Some(cb(region_ref))
    }

    fn try_recover_at(&mut self, at: u64, error: &RiegeliError) -> bool {
        let Some(go) = self.report_region_at(at, error) else {
            // No callback configured: leave the reader untouched so the
            // error stays position-stable and persistent on retry.
            return false;
        };
        // The region is consumed REGARDLESS of the verdict, matching the
        // C++ reference (Recover() repositions before the callback is
        // consulted): cancel reports the error once, but the next
        // operation continues past the rejected region. This is what
        // makes a naive retry loop around a cancelling callback make
        // progress instead of re-firing the same region forever.
        let end = self
            .last_skipped_region
            .as_ref()
            .expect("set by report_region_at")
            .end();
        self.last_record_is_valid = false;
        self.current_decoder = None;
        self.at_eof = false;
        self.next_chunk_file_pos = end;
        if self.reader.seek(SeekFrom::Start(end)).is_err() {
            self.at_eof = true;
        }
        go
    }

    /// Load the next chunk from `self.next_chunk_file_pos`.
    ///
    /// Returns `Ok(true)` if a chunk was loaded into `self.current_decoder`.
    /// Returns `Ok(false)` at EOF.
    /// Returns `Err` on corruption (without recovery).
    fn load_next_chunk(&mut self) -> Result<bool, RiegeliError> {
        loop {
            // Each chunk attempt re-classifies from scratch.
            self.pending_trusted_end = None;
            let pos = self.next_chunk_file_pos;

            // Read the chunk header, skipping any leading block header and any
            // block header interleaved within the 40-byte header span.
            // `chunk_begin` is the position of the header's first byte — the
            // chunk's canonical address.
            let (ch, chunk_begin, data_begin) = match self.read_chunk_header_at(pos)? {
                Some(v) => v,
                None => return Ok(false), // EOF
            };

            if !ch.is_header_valid() {
                return Err(RiegeliError::MalformedData(
                    format!("invalid chunk header hash at file position {chunk_begin}").into(),
                ));
            }

            let data_size = ch.data_size();
            let num_records = ch.num_records();

            // Compute where the chunk data ends in the file (accounting for block headers).
            let data_file_end =
                crate::block_arithmetic::chunk_end(chunk_begin, data_size, num_records);

            // Read the chunk data (skipping block headers).
            let chunk_data = self.read_chunk_data(data_begin, data_size)?;

            // Past this point the header is hash-valid, its claims are
            // stream-bounded, and the data bytes were physically present:
            // the chunk's extent is trustworthy at failure time, whatever
            // the failure (bad data hash, unknown type, construction).
            self.pending_trusted_end = Some(data_file_end);

            // Validate data hash.
            if !ch.is_data_valid(&chunk_data) {
                // Leave next_chunk_file_pos at the chunk start so the error
                // persists on retry and recovery scans from the right place.
                self.next_chunk_file_pos = chunk_begin;
                return Err(RiegeliError::MalformedData(
                    format!("chunk data hash mismatch at file position {chunk_begin}").into(),
                ));
            }

            // Update state for the next chunk.
            self.next_chunk_file_pos = data_file_end;

            // Resolve the chunk type only after next_chunk_file_pos has been
            // advanced: an unknown type must skip the whole chunk (forward
            // compatibility), and skipping requires the loop to make progress.
            //
            // Matching the C++ ChunkDecoder: an unknown chunk type is ignored
            // only when it carries no records; skipping a chunk with records
            // would lose data silently, so that case is an error.
            let chunk_type = match ch.chunk_type() {
                Ok(ct) => ct,
                Err(_) if num_records == 0 => continue,
                Err(e) => {
                    // Mirror the hash-mismatch convention: reset to the chunk
                    // start so the error is persistent — a bare retry must
                    // re-hit this chunk, not silently resume past its
                    // (dropped) records at the next chunk.
                    self.next_chunk_file_pos = chunk_begin;
                    return Err(e);
                }
            };

            match chunk_type {
                ChunkType::Simple => {
                    let chunk = Chunk {
                        header: ch,
                        data: chunk_data,
                    };
                    // Persistence: construction failures (malformed chunk
                    // interior behind valid hashes) must rewind like the
                    // hash-mismatch and unknown-type paths do — a bare retry
                    // must re-hit this chunk, not silently skip its records.
                    let decoder = match SimpleChunkDecoder::new(chunk) {
                        Ok(d) => d,
                        Err(e) => {
                            self.next_chunk_file_pos = chunk_begin;
                            return Err(e);
                        }
                    };
                    self.current_chunk_begin = chunk_begin;
                    self.current_record_index = 0;
                    self.pos = RecordPosition::new(chunk_begin, 0);
                    self.current_decoder = Some(ActiveDecoder::Simple(decoder));
                    let _ = num_records;
                    return Ok(true);
                }
                ChunkType::Transposed => {
                    let chunk = Chunk {
                        header: ch,
                        data: chunk_data,
                    };
                    // Same persistence convention as the Simple arm above.
                    let decoder = match TransposeChunkDecoder::new_with_projection(
                        chunk,
                        self.field_projection.as_ref(),
                    ) {
                        Ok(d) => d,
                        Err(e) => {
                            self.next_chunk_file_pos = chunk_begin;
                            return Err(e);
                        }
                    };
                    self.current_chunk_begin = chunk_begin;
                    self.current_record_index = 0;
                    self.pos = RecordPosition::new(chunk_begin, 0);
                    self.current_decoder = Some(ActiveDecoder::Transposed(decoder));
                    let _ = num_records;
                    return Ok(true);
                }
                ChunkType::FileSignature | ChunkType::Padding => {
                    continue;
                }
                ChunkType::FileMetadata => {
                    continue;
                }
            }
        }
    }

    /// Read the 40-byte chunk header at `pos`, skipping and validating the
    /// block headers that the writer interleaves at every block boundary —
    /// both a block header directly at `pos` and one falling inside the
    /// 40-byte span (a chunk header may straddle a block boundary).
    ///
    /// Returns `Ok(None)` on a clean EOF. Otherwise returns
    /// `(header, chunk_begin, data_begin)`: `chunk_begin` is the position of
    /// the header's first byte (after any leading block header) — the value
    /// to use for record positions and `advance_past_chunk` — and
    /// `data_begin` is the position of the first chunk-data byte.
    fn read_chunk_header_at(
        &mut self,
        pos: u64,
    ) -> Result<Option<(ChunkHeader, u64, u64)>, RiegeliError> {
        // Canonicalize: a chunk whose header physically follows a block
        // header is addressed AT the block boundary; the first-header-byte
        // form (boundary + 24) is accepted as an alias of the same chunk.
        let chunk_begin = crate::block_arithmetic::canonical_chunk_address(pos);
        let mut file_pos = chunk_begin;
        let mut bytes = [0u8; 40]; // CHUNK_HEADER_SIZE
        let mut filled: usize = 0;

        while filled < bytes.len() {
            if is_block_boundary(file_pos) {
                self.reader.seek(SeekFrom::Start(file_pos))?;
                let mut bh_bytes = [0u8; 24]; // BLOCK_HEADER_SIZE
                match self.reader.read_exact(&mut bh_bytes) {
                    Ok(()) => {}
                    Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
                    Err(e) => return Err(e.into()),
                }
                let bh = BlockHeader::from_bytes(bh_bytes);
                if !bh.is_valid() {
                    return Err(RiegeliError::MalformedData(
                        format!("invalid block header hash at file position {file_pos}").into(),
                    ));
                }
                file_pos += BLOCK_HEADER_SIZE;
            }
            let until_boundary = BLOCK_SIZE - (file_pos % BLOCK_SIZE);
            let to_read = ((bytes.len() - filled) as u64).min(until_boundary) as usize;
            self.reader.seek(SeekFrom::Start(file_pos))?;
            match self.reader.read_exact(&mut bytes[filled..filled + to_read]) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
                Err(e) => return Err(e.into()),
            }
            filled += to_read;
            file_pos += to_read as u64;
        }

        let ch = ChunkHeader::from_bytes(bytes);

        // Validate header-claimed sizes against the physical stream before
        // any caller does arithmetic, allocation, or overhead walking with
        // them. The header hash only proves integrity, not honesty — anyone
        // authoring a file can hash arbitrary claims, and unchecked claims
        // reach u64 arithmetic (overflow), Vec::with_capacity (allocation
        // bombs), and an O(claim) block-overhead walk. No well-formed file
        // is rejected: chunk data cannot extend past end of file, and the
        // format guarantees a chunk spans at least num_records file bytes.
        // Claims of a hash-invalid header are not checked here — callers
        // report those with their own header-hash errors.
        if ch.is_header_valid() {
            let data_begin = file_pos;
            if ch.data_size() > self.stream_len.saturating_sub(data_begin)
                || ch.num_records() > self.stream_len.saturating_sub(chunk_begin)
            {
                // Re-measure before rejecting: a seek resets EOF, so a file
                // that grew since the last measurement is a supported way
                // to keep reading — the bound must track the growth.
                self.stream_len = self.reader.seek(SeekFrom::End(0))?;
                if ch.data_size() > self.stream_len.saturating_sub(data_begin) {
                    return Err(RiegeliError::MalformedData(format!(
                        "chunk at {chunk_begin} claims {} data bytes with only {} bytes left in the stream",
                        ch.data_size(),
                        self.stream_len.saturating_sub(data_begin)
                    ).into()));
                }
                if ch.num_records() > self.stream_len.saturating_sub(chunk_begin) {
                    return Err(RiegeliError::MalformedData(format!(
                        "chunk at {chunk_begin} claims {} records with only {} bytes left in the stream",
                        ch.num_records(),
                        self.stream_len.saturating_sub(chunk_begin)
                    ).into()));
                }
            }
        }

        Ok(Some((ch, chunk_begin, file_pos)))
    }

    /// Read `data_size` bytes of chunk data starting at `data_begin`,
    /// skipping block headers at boundaries. `data_begin` must be the
    /// position of the first data byte (as returned by
    /// `read_chunk_header_at`), which is not `chunk_begin + 40` when the
    /// chunk header straddles a block boundary.
    fn read_chunk_data(
        &mut self,
        data_begin: u64,
        data_size: u64,
    ) -> Result<Vec<u8>, RiegeliError> {
        // data_size is validated against the stream by read_chunk_header_at;
        // the min is defense in depth for any future unvalidated caller.
        let mut result = Vec::with_capacity(data_size.min(self.stream_len) as usize);
        let mut remaining = data_size;
        let mut file_pos = data_begin;

        // Always position explicitly: callers cannot guarantee the reader's
        // physical position (the header read may have re-measured the stream
        // length against a growing file, which seeks to the end).
        self.reader.seek(SeekFrom::Start(file_pos))?;

        while remaining > 0 {
            // Skip block header if at boundary.
            if is_block_boundary(file_pos) {
                let mut bh_bytes = [0u8; 24]; // BLOCK_HEADER_SIZE
                self.reader.seek(SeekFrom::Start(file_pos))?;
                self.reader.read_exact(&mut bh_bytes)?;
                let bh = BlockHeader::from_bytes(bh_bytes);
                if !bh.is_valid() {
                    return Err(RiegeliError::MalformedData(
                        format!(
                        "invalid block header hash at file position {file_pos} (during data read)"
                    )
                        .into(),
                    ));
                }
                file_pos += BLOCK_HEADER_SIZE;
                // Seek to data position after the block header.
                self.reader.seek(SeekFrom::Start(file_pos))?;
            }

            // How many bytes can we read before hitting the next block boundary?
            let bytes_until_boundary = BLOCK_SIZE - (file_pos % BLOCK_SIZE);
            let to_read = remaining.min(bytes_until_boundary) as usize;

            let old_len = result.len();
            result.resize(old_len + to_read, 0);
            self.reader.read_exact(&mut result[old_len..])?;

            file_pos += to_read as u64;
            remaining -= to_read as u64;
        }

        Ok(result)
    }

    /// Load and decode a chunk at the given file position, returning the decoder.
    ///
    /// Returns `Ok(None)` at EOF.
    fn load_chunk_at(&mut self, file_pos: u64) -> Result<Option<ActiveDecoder>, RiegeliError> {
        let mut file_pos = file_pos;
        loop {
            // Each chunk attempt re-classifies from scratch.
            self.pending_trusted_end = None;
            // Read the chunk header (skipping leading and interleaved block headers).
            let (ch, chunk_begin, data_begin) = match self.read_chunk_header_at(file_pos)? {
                Some(v) => v,
                None => return Ok(None), // EOF
            };

            if !ch.is_header_valid() {
                return Err(RiegeliError::MalformedData(
                    format!("invalid chunk header hash at file position {chunk_begin}").into(),
                ));
            }

            let data_size = ch.data_size();
            let chunk_data = self.read_chunk_data(data_begin, data_size)?;

            // Advance only on success: if decoder construction below fails, the
            // position stays at this chunk so the error is persistent on retry
            // (same convention as load_next_chunk).
            let chunk_end_pos =
                crate::block_arithmetic::chunk_end(chunk_begin, data_size, ch.num_records());

            // Header valid, claims bounded, data present: trustworthy extent
            // for any failure from here on (see load_next_chunk).
            self.pending_trusted_end = Some(chunk_end_pos);

            if !ch.is_data_valid(&chunk_data) {
                return Err(RiegeliError::MalformedData(
                    format!("chunk data hash mismatch at file position {chunk_begin}").into(),
                ));
            }

            self.current_chunk_begin = chunk_begin;
            self.current_record_index = 0;

            match ch.chunk_type() {
                Ok(ChunkType::Simple) => {
                    let chunk = Chunk {
                        header: ch,
                        data: chunk_data,
                    };
                    let decoder = SimpleChunkDecoder::new(chunk)?;
                    self.next_chunk_file_pos = chunk_end_pos;
                    return Ok(Some(ActiveDecoder::Simple(decoder)));
                }
                Ok(ChunkType::Transposed) => {
                    let chunk = Chunk {
                        header: ch,
                        data: chunk_data,
                    };
                    let decoder = TransposeChunkDecoder::new_with_projection(
                        chunk,
                        self.field_projection.as_ref(),
                    )?;
                    self.next_chunk_file_pos = chunk_end_pos;
                    return Ok(Some(ActiveDecoder::Transposed(decoder)));
                }
                _ => {
                    // A non-data chunk (signature, metadata, padding) is not
                    // EOF: scan forward to the next data chunk, the same way
                    // seek_numeric does. Conflating the two wedged the reader
                    // at EOF for every read after a seek to such an address.
                    // Iterative on purpose: a long run of tiny padding chunks
                    // must cost O(file size) scanning, not a stack frame per
                    // chunk (a crafted 2 MB padding run overflowed the stack
                    // when this was recursive).
                    self.next_chunk_file_pos = chunk_end_pos;
                    file_pos = chunk_end_pos;
                }
            }
        }
    }

    /// Peek at the chunk header at file_pos without advancing state.
    fn peek_chunk_header(&mut self, file_pos: u64) -> Result<Option<ChunkHeader>, RiegeliError> {
        match self.read_chunk_header_at(file_pos)? {
            None => Ok(None),
            // Hash-invalid headers carry unvalidated claims (the stream-bound
            // check in read_chunk_header_at only covers hash-valid headers,
            // since the other callers report hash failures themselves). A
            // peek must not hand such claims to seek scans or metadata reads
            // — a corrupted header claiming a huge size would drive an
            // O(claim) overhead walk or an oversized read.
            Some((ch, chunk_begin, _)) => {
                if !ch.is_header_valid() {
                    return Err(RiegeliError::MalformedData(
                        format!("invalid chunk header hash at file position {chunk_begin}").into(),
                    ));
                }
                Ok(Some(ch))
            }
        }
    }
}

/// Return the next block boundary strictly after `pos`.
fn next_block_boundary(pos: u64) -> u64 {
    if is_block_boundary(pos) {
        pos
    } else {
        round_down_to_block_boundary(pos) + BLOCK_SIZE
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::io::Cursor;
    use std::rc::Rc;

    use super::*;
    use crate::compression::CompressionType;
    use crate::record_writer::{RecordWriter, WriterOptions};

    /// Write records to a Vec<u8> and return the bytes.
    fn write_records(records: &[&[u8]], opts: WriterOptions) -> Vec<u8> {
        let mut buf = Cursor::new(Vec::<u8>::new());
        {
            let mut w = RecordWriter::new(&mut buf, opts).expect("new ok");
            for rec in records {
                w.write_record(rec).expect("write ok");
            }
            w.flush().expect("flush ok");
        }
        buf.into_inner()
    }

    // -------------------------------------------------------------------------
    // Criterion 6.1: read back a RecordWriter-written file
    // -------------------------------------------------------------------------
    #[test]
    fn roundtrip_basic() {
        let records: &[&[u8]] = &[b"hello", b"world", b"riegeli"];
        let data = write_records(records, WriterOptions::new());
        let cursor = Cursor::new(data);
        let mut reader = RecordReader::new(cursor, ReaderOptions::new()).expect("reader new ok");

        let mut got = Vec::new();
        while let Some(rec) = reader.read_record().expect("read ok") {
            got.push(rec);
        }
        assert_eq!(got.len(), records.len());
        for (i, (got, expected)) in got.iter().zip(records.iter()).enumerate() {
            assert_eq!(got.as_slice(), *expected, "record {i} mismatch");
        }
    }

    // -------------------------------------------------------------------------
    // Criterion 6.1: 100 records
    // -------------------------------------------------------------------------
    #[test]
    fn roundtrip_100_records() {
        let record_data: Vec<u8> = (0..100u8).collect();
        let records: Vec<&[u8]> = (0..100).map(|_| record_data.as_slice()).collect();
        let data = write_records(&records, WriterOptions::new());
        let cursor = Cursor::new(data);
        let mut reader = RecordReader::new(cursor, ReaderOptions::new()).expect("reader new ok");

        let mut count = 0usize;
        while let Some(rec) = reader.read_record().expect("read ok") {
            assert_eq!(rec, record_data, "record {count} mismatch");
            count += 1;
        }
        assert_eq!(count, 100);
    }

    // -------------------------------------------------------------------------
    // Criterion 6.3: pos() at start
    // -------------------------------------------------------------------------
    #[test]
    fn pos_at_start() {
        let data = write_records(&[b"test"], WriterOptions::new());
        let cursor = Cursor::new(data);
        let reader = RecordReader::new(cursor, ReaderOptions::new()).expect("reader new ok");

        let pos = reader.pos();
        // Matches the C++ reference: initial position is numeric 0 (the
        // earlier criterion said 24; the differential harness showed the
        // reference returns 0 and the reference wins).
        assert_eq!(pos.numeric(), 0, "initial position is numeric 0");
        assert_eq!(pos.record_index, 0, "record_index should be 0");
    }

    // -------------------------------------------------------------------------
    // Criterion 6.4: last_pos().numeric() → seek_numeric → same record
    // -------------------------------------------------------------------------
    #[test]
    fn seek_numeric_roundtrip() {
        let records: Vec<Vec<u8>> = (0..10u8).map(|i| vec![i; 50]).collect();
        let record_refs: Vec<&[u8]> = records.iter().map(|r| r.as_slice()).collect();
        let data = write_records(&record_refs, WriterOptions::new().chunk_size(200));
        let data = std::sync::Arc::new(data);

        let cursor = Cursor::new((*data).clone());
        let mut reader = RecordReader::new(cursor, ReaderOptions::new()).expect("reader new ok");

        // Read a few records and verify seek_numeric can re-read them.
        let mut positions = Vec::new();
        let mut read_records_vec = Vec::new();
        while let Some(rec) = reader.read_record().expect("read ok") {
            positions.push(reader.last_pos());
            read_records_vec.push(rec);
        }

        // Now for each position, seek_numeric and re-read.
        for (i, (&pos, expected)) in positions.iter().zip(read_records_vec.iter()).enumerate() {
            let cursor2 = Cursor::new((*data).clone());
            let mut reader2 =
                RecordReader::new(cursor2, ReaderOptions::new()).expect("reader new ok");
            reader2
                .seek_numeric(pos.numeric())
                .expect("seek_numeric ok");
            let rec = reader2
                .read_record()
                .expect("read ok after seek")
                .expect("should have record");
            assert_eq!(&rec, expected, "record {i} mismatch after seek_numeric");
        }
    }

    // -------------------------------------------------------------------------
    // Criterion 6.5: corruption handling
    // -------------------------------------------------------------------------
    #[test]
    fn corruption_no_recovery() {
        let records: &[&[u8]] = &[b"before", b"during", b"after"];
        let mut data = write_records(records, WriterOptions::new().chunk_size(10));

        // Corrupt the second chunk's data (skip header at 0, sig chunk at 24..64, first data chunk starts at 64).
        // The first data chunk header is at 64 (40 bytes), data starts at 104.
        // Let's find the second data chunk by reading the first chunk's size.
        // For simplicity, just corrupt some bytes in the middle of the file.
        let mid = data.len() / 2;
        // Flip some bytes in the middle, making sure we're not in a block header.
        for i in mid..mid + 4 {
            if i < data.len() {
                data[i] ^= 0xFF;
            }
        }

        let cursor = Cursor::new(data);
        let mut reader = RecordReader::new(cursor, ReaderOptions::new()).expect("reader new ok");

        // Without recovery, should return Err at some point.
        let mut found_err = false;
        for _ in 0..10 {
            match reader.read_record() {
                Err(_) => {
                    found_err = true;
                    break;
                }
                Ok(None) => break,
                Ok(Some(_)) => {}
            }
        }
        assert!(
            found_err,
            "expected an error when reading corrupted file without recovery"
        );
    }

    #[test]
    fn corruption_with_recovery() {
        // Write many records spread across multiple chunks.
        let records: Vec<Vec<u8>> = (0..50u8).map(|i| vec![i; 100]).collect();
        let record_refs: Vec<&[u8]> = records.iter().map(|r| r.as_slice()).collect();
        let mut data = write_records(&record_refs, WriterOptions::new().chunk_size(200));

        // Corrupt the middle of the file (past the first block of data, so
        // there are records before and after the corruption).
        // Find a good spot: skip initial headers and corrupt something in the data area.
        // We need to corrupt inside a chunk (not a block header) to trigger recovery.
        let mid = (data.len() / 2).max(100);
        // Make sure we're not corrupting a block header position.
        let mid = if mid % 65536 < 24 { mid + 24 } else { mid };
        if mid + 8 < data.len() {
            for b in &mut data[mid..mid + 8] {
                *b ^= 0xFF;
            }
        }

        let recovered_positions: Rc<RefCell<Vec<u64>>> = Rc::new(RefCell::new(Vec::new()));
        let recovered_clone = Rc::clone(&recovered_positions);

        let cursor = Cursor::new(data);
        let opts = ReaderOptions::new().recovery(move |region| {
            recovered_clone.borrow_mut().push(region.begin());
            true
        });
        let mut reader = RecordReader::new(cursor, opts).expect("reader new ok");

        // Read all records (with recovery, should not return Err).
        let mut all_records = Vec::new();
        loop {
            match reader.read_record() {
                Ok(Some(rec)) => all_records.push(rec),
                Ok(None) => break,
                Err(e) => panic!("unexpected error with recovery: {e}"),
            }
        }

        // Recovery should have been triggered (some records recovered or skipped).
        // We should have read at least some records.
        assert!(
            !all_records.is_empty(),
            "should have read some records with recovery"
        );
        // Recovery callback should have been called at least once.
        assert!(
            !recovered_positions.borrow().is_empty(),
            "recovery callback should have been called"
        );
    }

    // -------------------------------------------------------------------------
    // Criterion 6.6: seek_numeric to middle of chunk
    // -------------------------------------------------------------------------
    #[test]
    fn seek_numeric_mid_chunk() {
        // Write records into a single chunk (large chunk_size so all go in one).
        let records: Vec<Vec<u8>> = (0..10u8).map(|i| vec![i; 20]).collect();
        let record_refs: Vec<&[u8]> = records.iter().map(|r| r.as_slice()).collect();
        let data = write_records(&record_refs, WriterOptions::new().chunk_size(1 << 20));

        // All 10 records are in one chunk starting at 64.
        // chunk_begin = 64, record_index 0..9.
        // numeric for record 5 = 64 + 5 = 69.
        // seek_numeric(67) should resolve to the record at chunk_begin=64, record_index=3 (67-64=3).
        // That is records[3] = vec![3; 20].
        let cursor = Cursor::new(data);
        let mut reader = RecordReader::new(cursor, ReaderOptions::new()).expect("reader new ok");

        reader.seek_numeric(64 + 3).expect("seek_numeric ok");
        let rec = reader
            .read_record()
            .expect("read ok")
            .expect("should have record");
        assert_eq!(rec, vec![3u8; 20], "expected record[3]");
    }

    // -------------------------------------------------------------------------
    // Criterion 6.7: read_metadata returns None
    // -------------------------------------------------------------------------
    #[test]
    fn read_metadata_returns_none() {
        let data = write_records(&[b"x"], WriterOptions::new());
        let cursor = Cursor::new(data);
        let mut reader = RecordReader::new(cursor, ReaderOptions::new()).expect("reader new ok");
        let meta = reader.read_metadata().expect("read_metadata ok");
        assert!(meta.is_none(), "expected None from read_metadata");
    }

    // -------------------------------------------------------------------------
    // Criterion 6.8: EOF returns Ok(None), then Ok(None) again
    // -------------------------------------------------------------------------
    #[test]
    fn eof_returns_none_repeatedly() {
        let data = write_records(&[b"only"], WriterOptions::new());
        let cursor = Cursor::new(data);
        let mut reader = RecordReader::new(cursor, ReaderOptions::new()).expect("reader new ok");

        // Read the one record.
        let rec = reader
            .read_record()
            .expect("first read ok")
            .expect("should have record");
        assert_eq!(rec, b"only");

        // EOF.
        let r1 = reader.read_record().expect("second read ok");
        assert!(r1.is_none(), "expected None at EOF");

        // EOF again.
        let r2 = reader.read_record().expect("third read ok");
        assert!(r2.is_none(), "expected None again");
    }

    // -------------------------------------------------------------------------
    // Multi-block roundtrip
    // -------------------------------------------------------------------------
    #[test]
    fn roundtrip_multi_block() {
        // Write enough data to span multiple blocks.
        let record: Vec<u8> = vec![0xAB; 1000];
        let records: Vec<&[u8]> = (0..100).map(|_| record.as_slice()).collect();
        let data = write_records(&records, WriterOptions::new().chunk_size(4096));

        let cursor = Cursor::new(data);
        let mut reader = RecordReader::new(cursor, ReaderOptions::new()).expect("reader new ok");

        let mut count = 0usize;
        while let Some(rec) = reader.read_record().expect("read ok") {
            assert_eq!(rec, record, "record {count} mismatch");
            count += 1;
        }
        assert_eq!(count, 100, "should read exactly 100 records");
    }

    // -------------------------------------------------------------------------
    // Criterion 9.8: Interleaved simple and transposed chunks
    // -------------------------------------------------------------------------
    #[test]
    fn interleaved_simple_and_transposed() {
        // Build a file by hand: signature + simple chunk + transposed chunk.
        // We use the record_writer to write a normal file (simple chunks only),
        // then manually splice in a transposed chunk.
        //
        // For simplicity, we write a file with simple chunk records, then create
        // a separate transposed chunk and concatenate them into a valid file.
        use crate::block_header::BlockHeader;
        use crate::chunk_header::{ChunkHeader, ChunkType};
        use crate::simple_chunk::SimpleChunkEncoder;
        use crate::transpose::internal::message_id;
        use crate::varint::{encode_u32, encode_u64};

        // Build the file manually:
        // [BlockHeader at 0] [FileSignature ChunkHeader at 24] [Simple ChunkHeader] [Simple Data] [Transposed ChunkHeader] [Transposed Data]

        let mut file_data: Vec<u8> = Vec::new();

        // Block header at offset 0.
        // We'll fill it in later once we know sizes.
        let bh_placeholder = [0u8; 24];
        file_data.extend_from_slice(&bh_placeholder);

        // File signature chunk.
        let sig_header = ChunkHeader::from_parts(&[], ChunkType::FileSignature, 0, 0);
        file_data.extend_from_slice(&sig_header.to_bytes());

        // Simple chunk with 2 records.
        let mut simple_enc = SimpleChunkEncoder::new();
        simple_enc.add_record(b"simple_one");
        simple_enc.add_record(b"simple_two");
        let simple_chunk = simple_enc.encode().unwrap();
        file_data.extend_from_slice(&simple_chunk.header.to_bytes());
        file_data.extend_from_slice(&simple_chunk.data);

        // Transposed chunk with 1 nonproto record "transposed".
        let nonproto_data = b"transposed".to_vec();
        let mut nonproto_lengths = Vec::new();
        nonproto_lengths.extend_from_slice(&encode_u32(10));

        // Build transpose header.
        let mut header_bytes: Vec<u8> = Vec::new();
        header_bytes.extend_from_slice(&encode_u32(1)); // num_buckets
        header_bytes.extend_from_slice(&encode_u32(2)); // num_buffers
        let total_buf: usize = nonproto_data.len() + nonproto_lengths.len();
        header_bytes.extend_from_slice(&encode_u64(total_buf as u64)); // bucket compressed size
        header_bytes.extend_from_slice(&encode_u64(nonproto_data.len() as u64)); // buf 0 size
        header_bytes.extend_from_slice(&encode_u64(nonproto_lengths.len() as u64)); // buf 1 size
        header_bytes.extend_from_slice(&encode_u32(1)); // num_states
        header_bytes.extend_from_slice(&encode_u32(message_id::NON_PROTO)); // tag for state 0
        header_bytes.extend_from_slice(&encode_u32(0)); // next_node for state 0
                                                        // NonProto reads buffer_index:
        header_bytes.extend_from_slice(&encode_u32(0)); // buffer_index = 0 (nonproto data)
        header_bytes.extend_from_slice(&encode_u32(0)); // first_node

        let mut trans_data: Vec<u8> = Vec::new();
        trans_data.push(0x00); // CompressionType::None
        trans_data.extend_from_slice(&encode_u64(header_bytes.len() as u64));
        trans_data.extend_from_slice(&header_bytes);
        trans_data.extend_from_slice(&nonproto_data);
        trans_data.extend_from_slice(&nonproto_lengths);
        // no transitions

        let trans_header = ChunkHeader::from_parts(&trans_data, ChunkType::Transposed, 1, 10);

        file_data.extend_from_slice(&trans_header.to_bytes());
        file_data.extend_from_slice(&trans_data);

        // Fix the block header.
        // next_chunk = distance from 0 to end of signature chunk = 64.
        // previous_chunk = 0.
        let bh = BlockHeader::from_parts(0, 64);
        let bh_bytes = bh.to_bytes();
        file_data[..24].copy_from_slice(&bh_bytes);

        // Read all records.
        let cursor = Cursor::new(file_data);
        let mut reader = RecordReader::new(cursor, ReaderOptions::new()).expect("reader new ok");

        let mut got = Vec::new();
        while let Some(rec) = reader.read_record().expect("read ok") {
            got.push(rec);
        }

        assert_eq!(got.len(), 3, "should have 3 records total");
        assert_eq!(got[0], b"simple_one");
        assert_eq!(got[1], b"simple_two");
        assert_eq!(got[2], b"transposed");
    }

    // -------------------------------------------------------------------------
    // Brotli roundtrip (when feature enabled)
    // -------------------------------------------------------------------------
    #[test]
    #[cfg(feature = "brotli")]
    fn roundtrip_brotli() {
        let records: &[&[u8]] = &[b"compressed1", b"compressed2", b"compressed3"];
        let data = write_records(
            records,
            WriterOptions::new().compression(CompressionType::Brotli),
        );
        let cursor = Cursor::new(data);
        let mut reader = RecordReader::new(cursor, ReaderOptions::new()).expect("reader new ok");

        let mut got = Vec::new();
        while let Some(rec) = reader.read_record().expect("read ok") {
            got.push(rec);
        }
        assert_eq!(got.len(), records.len());
        for (i, (got, expected)) in got.iter().zip(records.iter()).enumerate() {
            assert_eq!(got.as_slice(), *expected, "brotli record {i} mismatch");
        }
    }

    // -------------------------------------------------------------------------
    // Unknown chunk types must be skipped, not re-read forever
    // -------------------------------------------------------------------------

    /// A well-formed 40-byte chunk header whose type byte is not any known
    /// `ChunkType` discriminant, with zero data bytes. Both hashes are valid,
    /// so only the type is unrecognized — the forward-compatibility case.
    fn unknown_type_chunk() -> Vec<u8> {
        unknown_type_chunk_with_records(0)
    }

    /// A 40-byte Simple-chunk header with valid hashes but hostile claimed
    /// sizes, and no data bytes. The hash proves integrity, not honesty —
    /// these claims must be rejected against the physical stream.
    fn hostile_simple_chunk(data_size: u64, num_records: u64) -> Vec<u8> {
        let data_hash = crate::hash::highway_hash_64(&[]);
        let chunk_type_and_num_records: u64 = (num_records << 8) | (ChunkType::Simple as u8 as u64);
        let decoded_data_size: u64 = 0;

        let mut body = [0u8; 32];
        body[0..8].copy_from_slice(&data_size.to_le_bytes());
        body[8..16].copy_from_slice(&data_hash.to_le_bytes());
        body[16..24].copy_from_slice(&chunk_type_and_num_records.to_le_bytes());
        body[24..32].copy_from_slice(&decoded_data_size.to_le_bytes());
        let header_hash = crate::hash::highway_hash_64(&body);

        let mut out = Vec::with_capacity(40);
        out.extend_from_slice(&header_hash.to_le_bytes());
        out.extend_from_slice(&body);
        out
    }

    fn unknown_type_chunk_with_records(num_records: u64) -> Vec<u8> {
        let data_size: u64 = 0;
        let data_hash = crate::hash::highway_hash_64(&[]);
        let chunk_type_and_num_records: u64 = (num_records << 8) | b'z' as u64; // 'z' is not a known type
        let decoded_data_size: u64 = 0;

        let mut body = [0u8; 32];
        body[0..8].copy_from_slice(&data_size.to_le_bytes());
        body[8..16].copy_from_slice(&data_hash.to_le_bytes());
        body[16..24].copy_from_slice(&chunk_type_and_num_records.to_le_bytes());
        body[24..32].copy_from_slice(&decoded_data_size.to_le_bytes());
        let header_hash = crate::hash::highway_hash_64(&body);

        let mut out = Vec::with_capacity(40);
        out.extend_from_slice(&header_hash.to_le_bytes());
        out.extend_from_slice(&body);
        out
    }

    #[test]
    fn unknown_chunk_type_at_end_is_skipped() {
        let mut data = write_records(&[b"only"], WriterOptions::new());
        data.extend_from_slice(&unknown_type_chunk());

        let mut reader =
            RecordReader::new(Cursor::new(data), ReaderOptions::new()).expect("reader new ok");
        assert_eq!(
            reader.read_record().expect("read ok").as_deref(),
            Some(&b"only"[..])
        );
        // Before the fix this call never returned: load_next_chunk() hit the
        // unknown chunk and retried the same file position forever.
        assert_eq!(reader.read_record().expect("read ok"), None);
    }

    // -------------------------------------------------------------------------
    // Chunk headers that straddle a 64 KiB block boundary
    // -------------------------------------------------------------------------

    /// Write `n` single-record chunks of `rec_size` incompressible-layout
    /// bytes (flush per record, no compression) and return the file bytes.
    /// With rec_size around 16 KiB the fourth chunk's header lands near the
    /// first 64 KiB block boundary.
    fn write_chunks_past_first_block(rec_size: usize, n: usize) -> Vec<u8> {
        let mut buf = Cursor::new(Vec::<u8>::new());
        {
            let mut w = RecordWriter::new(
                &mut buf,
                WriterOptions::new().compression(CompressionType::None),
            )
            .expect("writer new ok");
            for i in 0..n {
                let rec = vec![(i % 251) as u8; rec_size];
                w.write_record(&rec).expect("write ok");
                w.flush().expect("flush ok");
            }
        }
        buf.into_inner()
    }

    /// Returns true if any chunk in the file has a header straddling a block
    /// boundary (header begins within 40 bytes below a 64 KiB multiple).
    ///
    /// Detected by direct inspection of the raw block-header bytes the
    /// writer emitted — deliberately NOT via the reader under test, whose
    /// position bookkeeping is part of what the straddle tests exercise.
    /// The block header at a boundary stores the distance back to the start
    /// of the chunk in progress there; a 40-byte chunk header straddles the
    /// boundary iff that distance is in (0, 40).
    fn has_straddling_chunk_header(data: &[u8]) -> bool {
        let block = BLOCK_SIZE as usize;
        let mut boundary = block;
        let mut straddles = false;
        while boundary + BLOCK_HEADER_SIZE as usize <= data.len() {
            let prev = u64::from_le_bytes(data[boundary + 8..boundary + 16].try_into().unwrap());
            if 0 < prev && prev < CHUNK_HEADER_SIZE {
                straddles = true;
            }
            boundary += block;
        }
        straddles
    }

    /// The writer interleaves a 24-byte block header inside the 40-byte chunk
    /// header when a chunk begins within 40 bytes of a block boundary. Before
    /// the fix, every read path fetched the header with one contiguous
    /// read_exact and failed with "invalid chunk header hash" on such files.
    #[test]
    fn chunk_header_straddling_block_boundary_roundtrip() {
        let n = 6;
        let mut exercised_straddle = false;
        // Sweep the record size so the fourth chunk's header walks across the
        // 64 KiB boundary; the exact straddling sizes shift if writer overhead
        // changes, which is why this is a sweep and not a single size.
        for rec_size in 16300..16340usize {
            let data = write_chunks_past_first_block(rec_size, n);
            exercised_straddle |= has_straddling_chunk_header(&data);

            let mut reader =
                RecordReader::new(Cursor::new(data), ReaderOptions::new()).expect("reader new ok");
            let mut count = 0;
            while let Some(rec) = reader.read_record().expect("read ok") {
                assert_eq!(rec.len(), rec_size, "rec_size={rec_size} record {count}");
                // The fill byte identifies the record, so a read that comes
                // back the right length but from the wrong place still fails.
                let fill = (count % 251) as u8;
                assert!(
                    rec.iter().all(|&b| b == fill),
                    "rec_size={rec_size} record {count}: content mismatch"
                );
                count += 1;
            }
            assert_eq!(count, n, "rec_size={rec_size}: wrong record count");
        }
        assert!(
            exercised_straddle,
            "sweep never produced a straddling chunk header; widen the range"
        );
    }

    /// size() and seek() walk chunk headers with their own scan loops; they
    /// must handle straddling headers too.
    #[test]
    fn chunk_header_straddling_block_boundary_size_and_seek() {
        let n = 6;
        // Find a straddling layout within the sweep window.
        let data = (16300..16340usize)
            .map(|rec_size| write_chunks_past_first_block(rec_size, n))
            .find(|data| has_straddling_chunk_header(data))
            .expect("no straddling layout found in sweep; widen the range");

        let mut reader =
            RecordReader::new(Cursor::new(data), ReaderOptions::new()).expect("reader new ok");
        assert_eq!(reader.size().expect("size ok"), n as u64);

        // Collect record positions, then seek back to each and re-read.
        let mut positions = Vec::new();
        while reader.read_record().expect("read ok").is_some() {
            positions.push(reader.last_pos());
        }
        assert_eq!(positions.len(), n);
        for (i, pos) in positions.into_iter().enumerate() {
            reader.seek(pos).expect("seek ok");
            let rec = reader
                .read_record()
                .expect("read after seek ok")
                .unwrap_or_else(|| panic!("record {i} missing after seek"));
            assert_eq!(rec[0], (i % 251) as u8, "record {i} content after seek");
        }
    }

    #[test]
    fn unknown_chunk_type_mid_stream_is_skipped() {
        // Build two single-record files and splice an unknown chunk between
        // the first file and the second file's record chunk. Layout of each
        // writer output: block header (24) | signature chunk (40) | record chunk.
        let first = write_records(&[b"first"], WriterOptions::new());
        let second = write_records(&[b"second"], WriterOptions::new());

        let mut data = first;
        data.extend_from_slice(&unknown_type_chunk());
        data.extend_from_slice(&second[64..]); // strip block header + signature chunk

        let mut reader =
            RecordReader::new(Cursor::new(data), ReaderOptions::new()).expect("reader new ok");
        assert_eq!(
            reader.read_record().expect("read ok").as_deref(),
            Some(&b"first"[..])
        );
        assert_eq!(
            reader.read_record().expect("read ok").as_deref(),
            Some(&b"second"[..])
        );
        assert_eq!(reader.read_record().expect("read ok"), None);
    }

    /// Matching the C++ ChunkDecoder: an unknown chunk type that claims to
    /// carry records cannot be skipped — its records would be silently lost —
    /// so it is an error rather than a forward-compatibility skip.
    #[test]
    fn unknown_chunk_type_with_records_is_an_error() {
        let mut data = write_records(&[b"only"], WriterOptions::new());
        data.extend_from_slice(&unknown_type_chunk_with_records(3));

        let mut reader =
            RecordReader::new(Cursor::new(data), ReaderOptions::new()).expect("reader new ok");
        assert_eq!(
            reader.read_record().expect("read ok").as_deref(),
            Some(&b"only"[..])
        );
        let err = reader
            .read_record()
            .expect_err("unknown chunk with records must error");
        assert!(
            err.to_string().contains("unknown chunk type")
                || err.to_string().contains("chunk type"),
            "unexpected error: {err}"
        );
    }

    /// The unknown-chunk-with-records error must be persistent: a caller
    /// that ignores the error and calls `read_record` again must hit the
    /// same error, not silently resume at the next chunk — that would skip
    /// the unknown chunk's claimed records after all, defeating the guard.
    /// Layout: [chunk "first"] [unknown type, num_records=3] [chunk "second"].
    #[test]
    fn unknown_chunk_type_error_is_persistent_and_does_not_skip_to_next_chunk() {
        // chunk_size(1) forces one chunk per record, so the two-record file
        // is [block hdr][signature][chunk "first"][chunk "second"] and the
        // one-record file is the same minus the last chunk. Their shared
        // prefix lets us splice an unknown chunk between the two chunks;
        // chunks are position-independent below the first block boundary.
        let one = write_records(&[b"first"], WriterOptions::new().chunk_size(1));
        let two = write_records(&[b"first", b"second"], WriterOptions::new().chunk_size(1));
        assert_eq!(
            &two[..one.len()],
            &one[..],
            "one-record file must be a prefix"
        );
        let second_chunk = &two[one.len()..];

        let mut data = one.clone();
        data.extend_from_slice(&unknown_type_chunk_with_records(3));
        data.extend_from_slice(second_chunk);
        assert!(
            data.len() < 65536,
            "test assumes no block boundary is crossed"
        );

        let mut reader =
            RecordReader::new(Cursor::new(data), ReaderOptions::new()).expect("reader new ok");
        assert_eq!(
            reader.read_record().expect("read ok").as_deref(),
            Some(&b"first"[..])
        );

        // First attempt errors on the unknown chunk.
        let err = reader
            .read_record()
            .expect_err("unknown chunk with records must error");
        assert!(
            err.to_string().contains("chunk type"),
            "unexpected error: {err}"
        );

        // Retries must keep erroring on the same chunk; "second" must never
        // surface past the guard.
        for attempt in 0..3 {
            match reader.read_record() {
                Err(e) => assert!(
                    e.to_string().contains("chunk type"),
                    "attempt {attempt}: unexpected error: {e}"
                ),
                Ok(rec) => panic!(
                    "attempt {attempt}: error was not persistent; got {:?}",
                    rec.as_deref()
                        .map(|r| String::from_utf8_lossy(r).into_owned())
                ),
            }
        }
    }

    /// Cancel semantics (C++ shape, adopted after the empirical trace):
    /// the region is consumed BEFORE the callback runs — `false` makes the
    /// failing operation return the ORIGINAL error once, but the next
    /// operation continues past the rejected region and the callback is
    /// never re-invoked for it. A LATER corrupt region fires the callback
    /// afresh.
    #[test]
    fn recovery_cancel_reports_once_and_consumes_the_region() {
        // [a][b CORRUPT][c][d CORRUPT][e]
        let mut lens = Vec::new();
        let recs: Vec<&[u8]> = vec![b"a", b"b", b"c", b"d", b"e"];
        for k in 1..=recs.len() {
            lens.push(write_records(&recs[..k], WriterOptions::new().chunk_size(1)).len());
        }
        let mut data = write_records(&recs, WriterOptions::new().chunk_size(1));
        data[lens[1] - 1] ^= 0xFF; // chunk "b" data
        data[lens[3] - 1] ^= 0xFF; // chunk "d" data

        let regions: Rc<RefCell<Vec<crate::SkippedRegion>>> = Rc::new(RefCell::new(Vec::new()));
        let rc = Rc::clone(&regions);
        let opts = ReaderOptions::new().recovery(move |region| {
            rc.borrow_mut().push(region.clone());
            false // cancel every region
        });
        let mut reader = RecordReader::new(Cursor::new(data), opts).expect("reader new ok");

        assert_eq!(
            reader.read_record().expect("read ok").as_deref(),
            Some(&b"a"[..])
        );
        let e1 = reader
            .read_record()
            .expect_err("cancel returns the original error");
        // The region was consumed: the next read continues past "b" to "c".
        assert_eq!(
            reader.read_record().expect("read ok").as_deref(),
            Some(&b"c"[..]),
            "rejected region is already skipped; reading continues"
        );
        let e2 = reader
            .read_record()
            .expect_err("the LATER corrupt region errors afresh");
        assert_eq!(
            reader.read_record().expect("read ok").as_deref(),
            Some(&b"e"[..])
        );
        assert_eq!(reader.read_record().expect("read ok"), None);

        let regions = regions.borrow();
        assert_eq!(regions.len(), 2, "one callback per region — never re-fired");
        assert_eq!(regions[0].begin(), lens[0] as u64, "region 1 = chunk b");
        assert_eq!(regions[0].end(), lens[1] as u64);
        assert_eq!(regions[1].begin(), lens[2] as u64, "region 2 = chunk d");
        assert_eq!(regions[1].end(), lens[3] as u64);
        assert_ne!(e1.to_string(), "", "errors carry messages");
        let _ = e2;
        // The accessor exposes the most recent reported region.
        assert_eq!(reader.last_skipped_region(), Some(&regions[1]));
    }

    /// Coupled region/resync precision: corrupting one chunk's DATA (its
    /// header stays hash-valid, so its extent is trustworthy) must skip
    /// exactly that chunk — the region is [chunk_begin, chunk_end) and the
    /// SIBLING chunk in the same block is recovered. The old boundary skip
    /// threw the sibling away.
    #[test]
    fn recovery_skips_exactly_one_chunk_when_header_is_valid() {
        let one = write_records(&[b"a"], WriterOptions::new().chunk_size(1));
        let three = write_records(&[b"a", b"b", b"c"], WriterOptions::new().chunk_size(1));
        let two = write_records(&[b"a", b"b"], WriterOptions::new().chunk_size(1));
        assert_eq!(&three[..two.len()], &two[..]);

        let mut data = three.clone();
        data[two.len() - 1] ^= 0xFF; // corrupt chunk "b"'s final data byte only
        let chunk_b_begin = one.len() as u64;
        let chunk_c_begin = two.len() as u64;

        let regions: Rc<RefCell<Vec<crate::SkippedRegion>>> = Rc::new(RefCell::new(Vec::new()));
        let rc = Rc::clone(&regions);
        let opts = ReaderOptions::new().recovery(move |region| {
            rc.borrow_mut().push(region.clone());
            true
        });
        let mut reader = RecordReader::new(Cursor::new(data), opts).expect("reader new ok");
        assert_eq!(
            reader.read_record().expect("read ok").as_deref(),
            Some(&b"a"[..])
        );
        // "b" is skipped; "c" — a sibling in the same block — is recovered.
        assert_eq!(
            reader.read_record().expect("read ok").as_deref(),
            Some(&b"c"[..]),
            "sibling after the bad chunk must be recovered"
        );
        assert_eq!(reader.read_record().expect("read ok"), None);
        // last_record_is_valid is per-record: "c" came from a valid chunk,
        // so the flag is true again after it returns.
        assert!(reader.last_record_is_valid());

        let regions = regions.borrow();
        assert_eq!(regions.len(), 1);
        assert_eq!(
            regions[0].begin(),
            chunk_b_begin,
            "region begins at the bad chunk"
        );
        assert_eq!(
            regions[0].end(),
            chunk_c_begin,
            "region ends exactly where the next chunk begins (the resync position)"
        );
        assert!(regions[0].message().contains("hash mismatch"));
    }

    /// When the chunk HEADER is hash-invalid, its claims cannot be trusted
    /// to compute an extent — the region ends at the next block boundary.
    #[test]
    fn recovery_resyncs_to_boundary_when_header_is_invalid() {
        let one = write_records(&[b"first"], WriterOptions::new().chunk_size(1));
        let two = write_records(&[b"first", b"second"], WriterOptions::new().chunk_size(1));
        let mut data = two.clone();
        data[one.len()] ^= 0xFF; // corrupt chunk 2's HEADER hash
        let data_len = data.len() as u64;

        let regions: Rc<RefCell<Vec<crate::SkippedRegion>>> = Rc::new(RefCell::new(Vec::new()));
        let rc = Rc::clone(&regions);
        let opts = ReaderOptions::new().recovery(move |region| {
            rc.borrow_mut().push(region.clone());
            true
        });
        let mut reader = RecordReader::new(Cursor::new(data), opts).expect("reader new ok");
        assert_eq!(
            reader.read_record().expect("read ok").as_deref(),
            Some(&b"first"[..])
        );
        assert_eq!(
            reader.read_record().expect("read ok"),
            None,
            "rest of block skipped"
        );

        let regions = regions.borrow();
        assert_eq!(regions.len(), 1);
        assert_eq!(regions[0].begin(), one.len() as u64);
        // Boundary-class, clamped to the stream length (the file ends well
        // before the next 64 KiB boundary; a region cannot extend past the
        // file — C++ reports EOF-ended regions the same way).
        assert_eq!(
            regions[0].end(),
            data_len,
            "boundary resync clamps to EOF for untrusted claims"
        );
    }

    /// Classification must happen at FAILURE time, not recovery time: a
    /// stream that grows between the two (every stream-length probe is a
    /// growth opportunity) must not reclassify an untrusted failure as
    /// trusted — that mislabeled a readable chunk as a precisely-skipped
    /// region. With failure-time classification the claim failure stays
    /// untrusted and the resync is the boundary, never a chunk_end computed
    /// from claims that were unvalidatable when the error happened.
    #[test]
    fn recovery_classifies_at_failure_time_not_recovery_time() {
        struct GrowOnNthEndSeek {
            full: Vec<u8>,
            visible: usize,
            pos: u64,
            end_seeks: u32,
            grow_at: u32,
        }
        impl std::io::Read for GrowOnNthEndSeek {
            fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
                let avail = &self.full[..self.visible];
                let start = (self.pos as usize).min(avail.len());
                let n = buf.len().min(avail.len() - start);
                buf[..n].copy_from_slice(&avail[start..start + n]);
                self.pos += n as u64;
                Ok(n)
            }
        }
        impl std::io::Seek for GrowOnNthEndSeek {
            fn seek(&mut self, pos: std::io::SeekFrom) -> std::io::Result<u64> {
                match pos {
                    std::io::SeekFrom::Start(p) => self.pos = p,
                    std::io::SeekFrom::End(off) => {
                        self.end_seeks += 1;
                        if self.end_seeks >= self.grow_at {
                            self.visible = self.full.len();
                        }
                        self.pos = (self.visible as i64 + off).max(0) as u64;
                    }
                    std::io::SeekFrom::Current(off) => {
                        self.pos = (self.pos as i64 + off).max(0) as u64;
                    }
                }
                Ok(self.pos)
            }
        }

        let one = write_records(&[b"first"], WriterOptions::new().chunk_size(1));
        let full = write_records(&[b"first", b"second"], WriterOptions::new().chunk_size(1));
        let cut = one.len() + 40; // chunk B header readable, data truncated

        let regions: Rc<RefCell<Vec<crate::SkippedRegion>>> = Rc::new(RefCell::new(Vec::new()));
        let rc = Rc::clone(&regions);
        // Grow generously late so construction-time probes don't trigger it,
        // but any recovery-time re-read (the bug) would.
        for grow_at in [3u32, 4, 5] {
            regions.borrow_mut().clear();
            let rc2 = Rc::clone(&rc);
            let reader_src = GrowOnNthEndSeek {
                full: full.clone(),
                visible: cut,
                pos: 0,
                end_seeks: 0,
                grow_at,
            };
            let opts = ReaderOptions::new().recovery(move |region| {
                rc2.borrow_mut().push(region.clone());
                true
            });
            let mut reader = RecordReader::new(reader_src, opts).expect("reader new ok");
            assert_eq!(
                reader.read_record().expect("read ok").as_deref(),
                Some(&b"first"[..])
            );
            let _ = reader.read_record(); // recovery fires on the truncated chunk

            for region in regions.borrow().iter() {
                assert!(
                    region.end() == BLOCK_SIZE || region.end() <= cut as u64 + BLOCK_SIZE,
                    "grow_at={grow_at}: untrusted failure must resync at a boundary"
                );
                assert_ne!(
                    region.end(),
                    full.len() as u64,
                    "grow_at={grow_at}: region end must not be a chunk_end derived \
                     from claims that were unvalidatable at failure time"
                );
            }
        }
    }

    /// A long run of corrupt-data chunks must produce one callback per
    /// chunk with contiguous, exactly-one-chunk regions (the coupling
    /// invariant universally, not just for a single chunk), terminating at
    /// EOF — no loop, no boundary fallback for trusted failures.
    #[test]
    fn recovery_walks_corrupt_chain_with_contiguous_regions() {
        const N: usize = 200; // corrupt chunks between two good ones
                              // Record k..=N+1 prefix files give every chunk span.
        let mut lens = Vec::with_capacity(N + 3);
        let mut recs: Vec<Vec<u8>> = Vec::new();
        for k in 0..(N + 2) {
            recs.push(format!("r{k:03}").into_bytes());
            let refs: Vec<&[u8]> = recs.iter().map(|r| r.as_slice()).collect();
            lens.push(write_records(&refs, WriterOptions::new().chunk_size(1)).len());
        }
        let refs: Vec<&[u8]> = recs.iter().map(|r| r.as_slice()).collect();
        let mut data = write_records(&refs, WriterOptions::new().chunk_size(1));
        assert!(
            data.len() < BLOCK_SIZE as usize,
            "keep it single-block for span math"
        );
        // Corrupt the final data byte of chunks 1..=N (leave first and last).
        for &len in &lens[1..=N] {
            data[len - 1] ^= 0xFF;
        }

        let regions: Rc<RefCell<Vec<crate::SkippedRegion>>> = Rc::new(RefCell::new(Vec::new()));
        let rc = Rc::clone(&regions);
        let opts = ReaderOptions::new().recovery(move |region| {
            rc.borrow_mut().push(region.clone());
            true
        });
        let mut reader = RecordReader::new(Cursor::new(data), opts).expect("reader new ok");
        assert_eq!(
            reader.read_record().expect("read ok").as_deref(),
            Some(&b"r000"[..])
        );
        assert_eq!(
            reader.read_record().expect("read ok").as_deref(),
            Some(format!("r{:03}", N + 1).as_bytes()),
            "the good chunk after the corrupt run must be reached"
        );
        assert_eq!(reader.read_record().expect("read ok"), None);

        let regions = regions.borrow();
        assert_eq!(regions.len(), N, "exactly one callback per corrupt chunk");
        for (i, region) in regions.iter().enumerate() {
            let k = i + 1;
            assert_eq!(region.begin(), lens[k - 1] as u64, "chunk {k} begin");
            assert_eq!(region.end(), lens[k] as u64, "chunk {k} end == next begin");
        }
    }

    /// A corrupt 24-byte block-header tail at an exact block boundary: the
    /// reader's raw position (boundary+24) canonicalizes 24 bytes BACK, and
    /// the EOF-clamped region end equals the raw position — forward
    /// progress must be measured from the raw position or the reader spins
    /// forever re-reporting the same region (CWE-835, found in review).
    #[test]
    fn recovery_terminates_on_corrupt_block_header_tail_at_boundary() {
        // Fill block 0 exactly: a padded file of BLOCK_SIZE bytes, then a
        // corrupt 24-byte pseudo block header.
        let mut data = write_records(&[b"a"], WriterOptions::new().final_padding(BLOCK_SIZE));
        assert_eq!(data.len() as u64, BLOCK_SIZE);
        data.extend_from_slice(&[0xFFu8; 24]); // hash-invalid block header
        assert_eq!(data.len() as u64, BLOCK_SIZE + 24);

        let regions: Rc<RefCell<Vec<crate::SkippedRegion>>> = Rc::new(RefCell::new(Vec::new()));
        let rr = Rc::clone(&regions);
        let opts = ReaderOptions::new().recovery(move |r| {
            rr.borrow_mut().push(r.clone());
            true
        });
        let mut reader = RecordReader::new(Cursor::new(data), opts).expect("reader new ok");
        assert_eq!(
            reader.read_record().expect("read ok").as_deref(),
            Some(&b"a"[..])
        );
        // Must terminate (the old clamp spun forever here).
        assert_eq!(reader.read_record().expect("read ok"), None, "clean EOF");
        let regions = regions.borrow();
        assert_eq!(
            regions.len(),
            1,
            "exactly ONE region — the alias-end double-report is repaired (got {regions:?})"
        );
        assert_eq!(regions[0].begin(), BLOCK_SIZE, "canonical begin");
        assert_eq!(
            regions[0].end(),
            BLOCK_SIZE + 25,
            "minimal progress margin: one byte past the 24-byte corrupt tail"
        );
    }

    /// Metadata reads promise not to move the read position — recovery
    /// firing inside one must be report-only. Repositioning here rewound a
    /// mid-stream reader to the region end (replaying records) or skipped
    /// block-0 chunks at the start.
    #[test]
    fn metadata_recovery_does_not_move_the_read_position() {
        // [sig][chunkA@64][padding to 64 KiB][fileB: sig][chunkB "b"]
        let mut data = write_records(&[b"a"], WriterOptions::new().final_padding(BLOCK_SIZE));
        assert_eq!(data.len() as u64, BLOCK_SIZE);
        data.extend_from_slice(&write_records(&[b"b"], WriterOptions::new()));
        data[64] ^= 0xFF; // corrupt chunk A's header (the metadata position)

        let count: Rc<RefCell<u32>> = Rc::new(RefCell::new(0));
        let rc = Rc::clone(&count);
        let opts = ReaderOptions::new().recovery(move |_r| {
            *rc.borrow_mut() += 1;
            true
        });
        let mut reader = RecordReader::new(Cursor::new(data), opts).expect("reader new ok");

        let pos_before = reader.pos();
        assert_eq!(
            reader.read_serialized_metadata().expect("metadata ok"),
            None,
            "skipped region reads as absent metadata"
        );
        assert_eq!(*count.borrow(), 1, "callback reported the region");
        assert_eq!(
            reader.pos(),
            pos_before,
            "metadata read must not move the read position"
        );

        // The record stream is undisturbed: reading proceeds from the start,
        // recovers past the corrupt chunk normally, and reaches "b".
        assert_eq!(
            reader.read_record().expect("read ok").as_deref(),
            Some(&b"b"[..])
        );
        assert_eq!(*count.borrow(), 2, "read-path recovery fired separately");
    }

    /// Seek with recovery: C++ `Seek` returns the result of the recovery
    /// function — on `true` the reader is positioned past the region.
    #[test]
    fn seek_recovers_past_invalid_region() {
        let one = write_records(&[b"a"], WriterOptions::new().chunk_size(1));
        let three = write_records(&[b"a", b"b", b"c"], WriterOptions::new().chunk_size(1));
        let two = write_records(&[b"a", b"b"], WriterOptions::new().chunk_size(1));
        let mut data = three.clone();
        data[two.len() - 1] ^= 0xFF; // corrupt chunk "b"'s final data byte

        let opts = ReaderOptions::new().recovery(|_region| true);
        let mut reader = RecordReader::new(Cursor::new(data), opts).expect("reader new ok");
        reader
            .seek(RecordPosition::new(one.len() as u64, 0))
            .expect("seek with recovery must succeed");
        assert_eq!(
            reader.read_record().expect("read ok").as_deref(),
            Some(&b"c"[..]),
            "positioned past the skipped region"
        );
        let _ = two;
    }

    /// Search with recovery skips invalid regions during the scan (C++
    /// contract) and still finds targets beyond them.
    #[test]
    fn search_recovers_past_invalid_region() {
        let one = write_records(&[b"a"], WriterOptions::new().chunk_size(1));
        let three = write_records(&[b"a", b"b", b"c"], WriterOptions::new().chunk_size(1));
        let mut data = three;
        data[one.len()] ^= 0xFF; // corrupt chunk "b"'s HEADER (scan-visible)

        let opts = ReaderOptions::new().recovery(|_region| true);
        let mut reader = RecordReader::new(Cursor::new(data), opts).expect("reader new ok");
        let found = reader.search(|rec| rec.cmp(&b"c"[..])).expect("search ok");
        // "c" sits in the region skipped by the boundary resync? No: the
        // corrupt header forces a boundary skip during the scan, and "c"
        // lives below the boundary too, so the honest outcome is NOT
        // FOUND without error — the scan completed, the region was
        // skipped, and the target was inside it.
        assert!(
            !found,
            "target inside the skipped region is reported absent"
        );

        // A target in an intact chunk before the corruption is still found.
        let mut reader2 = {
            let one = write_records(&[b"a"], WriterOptions::new().chunk_size(1));
            let three = write_records(&[b"a", b"b", b"c"], WriterOptions::new().chunk_size(1));
            let mut data = three;
            data[one.len()] ^= 0xFF;
            RecordReader::new(
                Cursor::new(data),
                ReaderOptions::new().recovery(|_region| true),
            )
            .expect("reader new ok")
        };
        assert!(reader2.search(|rec| rec.cmp(&b"a"[..])).expect("search ok"));
    }

    /// Seeking to a non-data chunk's address (here: the signature chunk of
    /// a concatenated second file) must scan forward to the next data chunk
    /// — it used to wedge the reader at EOF, hiding all following records.
    #[test]
    fn seek_to_non_data_chunk_scans_forward() {
        let mut data = write_records(&[b"a"], WriterOptions::new().final_padding(BLOCK_SIZE));
        assert_eq!(data.len() as u64, BLOCK_SIZE);
        let second = write_records(&[b"b"], WriterOptions::new());
        data.extend_from_slice(&second);

        let mut reader =
            RecordReader::new(Cursor::new(data), ReaderOptions::new()).expect("reader new ok");
        // The concatenated file's signature chunk is addressed at the block
        // boundary — a non-data chunk.
        reader
            .seek(RecordPosition::new(BLOCK_SIZE, 0))
            .expect("seek ok");
        assert_eq!(
            reader.read_record().expect("read ok").as_deref(),
            Some(&b"b"[..]),
            "seek to a non-data chunk must land on the next data chunk, not EOF"
        );
    }

    /// A long run of consecutive tiny padding chunks must be scanned
    /// iteratively: the forward scan over non-data chunks used to recurse
    /// once per chunk, and a crafted ~2 MB padding run overflowed the
    /// stack (SIGABRT). With the loop this completes in milliseconds.
    #[test]
    fn seek_across_long_padding_run_does_not_overflow_stack() {
        fn padding_run_file(n: usize) -> Vec<u8> {
            let mut file = write_records(&[], WriterOptions::new());
            assert_eq!(file.len() as u64, BLOCK_HEADER_SIZE + CHUNK_HEADER_SIZE);
            let header =
                crate::chunk_header::ChunkHeader::from_parts(&[], ChunkType::Padding, 0, 0)
                    .to_bytes();
            for _ in 0..n {
                let chunk_begin =
                    crate::block_arithmetic::canonical_chunk_address(file.len() as u64);
                let chunk_end = crate::block_arithmetic::chunk_end(chunk_begin, 0, 0);
                let mut i = 0usize;
                while i < header.len() {
                    let pos = file.len() as u64;
                    if pos.is_multiple_of(BLOCK_SIZE) {
                        let bh = crate::block_header::BlockHeader::from_parts(
                            pos - chunk_begin,
                            chunk_end - pos,
                        );
                        file.extend_from_slice(&bh.to_bytes());
                        continue;
                    }
                    let until = (BLOCK_SIZE - pos % BLOCK_SIZE) as usize;
                    let take = until.min(header.len() - i);
                    file.extend_from_slice(&header[i..i + take]);
                    i += take;
                }
            }
            file
        }

        let data = padding_run_file(50_000);
        let mut reader =
            RecordReader::new(Cursor::new(data), ReaderOptions::new()).expect("reader new ok");
        reader
            .seek(RecordPosition::new(
                BLOCK_HEADER_SIZE + CHUNK_HEADER_SIZE,
                0,
            ))
            .expect("seek across the padding run must not crash");
        assert_eq!(reader.read_record().expect("read ok"), None, "clean EOF");
    }

    /// Zero-record data chunks must not serve as binary-search pivots:
    /// probing one falls through to the NEXT chunk's record, corrupting
    /// pivot decisions (a present target could be reported absent).
    #[test]
    fn search_skips_zero_record_chunks() {
        /// A valid empty Simple chunk: zero records, uncompressed, empty
        /// sizes section.
        fn empty_simple_chunk() -> Vec<u8> {
            let data = [0x00u8, 0x00]; // compression none, sizes_byte_len 0
            let data_hash = crate::hash::highway_hash_64(&data);
            let chunk_type_and_num_records: u64 = ChunkType::Simple as u8 as u64; // 0 records
            let mut body = [0u8; 32];
            body[0..8].copy_from_slice(&(data.len() as u64).to_le_bytes());
            body[8..16].copy_from_slice(&data_hash.to_le_bytes());
            body[16..24].copy_from_slice(&chunk_type_and_num_records.to_le_bytes());
            body[24..32].copy_from_slice(&0u64.to_le_bytes());
            let header_hash = crate::hash::highway_hash_64(&body);
            let mut out = Vec::with_capacity(42);
            out.extend_from_slice(&header_hash.to_le_bytes());
            out.extend_from_slice(&body);
            out.extend_from_slice(&data);
            out
        }

        // Convergence layout: the target lives in the LAST real chunk and a
        // trailing empty chunk becomes the final pivot — probing it at EOF
        // yields an empty record, the comparator steers into the empty
        // chunk, and the within-chunk search reports the target absent.
        let mut data = write_records(&[b"a", b"m"], WriterOptions::new());
        data.extend_from_slice(&empty_simple_chunk());

        let mut reader =
            RecordReader::new(Cursor::new(data), ReaderOptions::new()).expect("reader new ok");
        let found = reader.search(|rec| rec.cmp(&b"m"[..])).expect("search ok");
        assert!(
            found,
            "present record must be found despite an empty pivot chunk"
        );
        assert_eq!(
            reader.read_record().expect("read ok").as_deref(),
            Some(&b"m"[..])
        );
    }

    /// seek() must not clobber last_pos — it tracks the last successfully
    /// READ record, and seek_back() is documented to return there.
    #[test]
    fn seek_back_returns_to_last_read_record_not_seek_target() {
        let data = write_records(&[b"first", b"second"], WriterOptions::new().chunk_size(1));
        let mut reader =
            RecordReader::new(Cursor::new(data), ReaderOptions::new()).expect("reader new ok");

        assert_eq!(
            reader.read_record().expect("read ok").as_deref(),
            Some(&b"first"[..])
        );
        let second_pos = {
            // Find the second record's position by reading it, then rewind.
            let p = reader.pos();
            assert_eq!(
                reader.read_record().expect("read ok").as_deref(),
                Some(&b"second"[..])
            );
            let sp = reader.last_pos();
            // Restore: last read should again be "first" for the real test.
            reader.seek(p).expect("seek ok");
            sp
        };

        // Fresh reader: read "first", then seek AWAY to second's position
        // without reading, then seek_back — must land on "first".
        let data2 = write_records(&[b"first", b"second"], WriterOptions::new().chunk_size(1));
        let mut reader2 =
            RecordReader::new(Cursor::new(data2), ReaderOptions::new()).expect("reader new ok");
        assert_eq!(
            reader2.read_record().expect("read ok").as_deref(),
            Some(&b"first"[..])
        );
        reader2.seek(second_pos).expect("seek ok");
        assert!(reader2.seek_back().expect("seek_back ok"));
        assert_eq!(
            reader2.read_record().expect("read ok").as_deref(),
            Some(&b"first"[..]),
            "seek_back must return to the last READ record, not the last seek target"
        );
    }

    /// Decoder-construction failures (malformed chunk interior behind valid
    /// hashes) must be persistent like every other read error: a bare retry
    /// must re-hit the same chunk, not silently resume at the next one.
    /// Layout: [chunk "first"][valid-hash chunk with hostile interior]
    /// [chunk "second"].
    #[test]
    fn decoder_construction_error_is_persistent() {
        // A Simple chunk whose hashes are valid but whose single data byte
        // is an unknown compression type — SimpleChunkDecoder::new fails.
        fn hostile_interior_chunk() -> Vec<u8> {
            let data = [0xFFu8]; // unknown compression byte
            let data_hash = crate::hash::highway_hash_64(&data);
            let chunk_type_and_num_records: u64 = (1u64 << 8) | ChunkType::Simple as u8 as u64;
            let decoded_data_size: u64 = 0;
            let mut body = [0u8; 32];
            body[0..8].copy_from_slice(&(data.len() as u64).to_le_bytes());
            body[8..16].copy_from_slice(&data_hash.to_le_bytes());
            body[16..24].copy_from_slice(&chunk_type_and_num_records.to_le_bytes());
            body[24..32].copy_from_slice(&decoded_data_size.to_le_bytes());
            let header_hash = crate::hash::highway_hash_64(&body);
            let mut out = Vec::with_capacity(41);
            out.extend_from_slice(&header_hash.to_le_bytes());
            out.extend_from_slice(&body);
            out.extend_from_slice(&data);
            out
        }

        let one = write_records(&[b"first"], WriterOptions::new().chunk_size(1));
        let two = write_records(&[b"first", b"second"], WriterOptions::new().chunk_size(1));
        assert_eq!(&two[..one.len()], &one[..]);
        let second_chunk = &two[one.len()..];

        let mut data = one.clone();
        data.extend_from_slice(&hostile_interior_chunk());
        data.extend_from_slice(second_chunk);
        assert!(data.len() < 65536);

        let mut reader =
            RecordReader::new(Cursor::new(data), ReaderOptions::new()).expect("reader new ok");
        assert_eq!(
            reader.read_record().expect("read ok").as_deref(),
            Some(&b"first"[..])
        );
        assert!(
            reader.read_record().is_err(),
            "construction failure must error"
        );
        for attempt in 0..3 {
            match reader.read_record() {
                Err(_) => {}
                Ok(rec) => panic!(
                    "attempt {attempt}: construction error was not persistent; got {:?}",
                    rec.as_deref()
                        .map(|r| String::from_utf8_lossy(r).into_owned())
                ),
            }
        }
    }

    /// A corrupt chunk header where the metadata chunk would live must
    /// surface as an error from the metadata APIs, not as "no metadata" —
    /// a caller inspecting metadata first must not conclude the file is
    /// clean.
    #[test]
    fn metadata_peek_propagates_corruption() {
        let mut data = write_records(&[b"only"], WriterOptions::new());
        data[64] ^= 0xFF; // corrupt the first chunk header after the signature

        let mut reader =
            RecordReader::new(Cursor::new(data), ReaderOptions::new()).expect("reader new ok");
        assert!(
            reader.read_serialized_metadata().is_err(),
            "corruption at the metadata position must not read as absent metadata"
        );

        // Sanity: an intact file without a metadata chunk still reports None.
        let clean = write_records(&[b"only"], WriterOptions::new());
        let mut reader =
            RecordReader::new(Cursor::new(clean), ReaderOptions::new()).expect("reader new ok");
        assert!(reader.read_serialized_metadata().expect("ok").is_none());
    }

    /// The signature chunk is a fixed constant; a hash-valid signature
    /// header with nonzero claimed sizes must be rejected by the exact
    /// byte comparison — trusting its data_size used to overflow the
    /// position arithmetic (debug panic) or seek backward through the
    /// i64 cast (release).
    #[test]
    fn hostile_signature_chunk_claims_are_rejected() {
        fn hostile_signature(data_size: u64) -> Vec<u8> {
            let data_hash = crate::hash::highway_hash_64(&[]);
            let chunk_type_and_num_records: u64 = ChunkType::FileSignature as u8 as u64;
            let decoded_data_size: u64 = 0;
            let mut body = [0u8; 32];
            body[0..8].copy_from_slice(&data_size.to_le_bytes());
            body[8..16].copy_from_slice(&data_hash.to_le_bytes());
            body[16..24].copy_from_slice(&chunk_type_and_num_records.to_le_bytes());
            body[24..32].copy_from_slice(&decoded_data_size.to_le_bytes());
            let header_hash = crate::hash::highway_hash_64(&body);
            let mut out = Vec::with_capacity(40);
            out.extend_from_slice(&header_hash.to_le_bytes());
            out.extend_from_slice(&body);
            out
        }

        let valid = write_records(&[b"x"], WriterOptions::new());
        for data_size in [u64::MAX, 1000u64] {
            let mut data = valid[..24].to_vec(); // keep the valid block header
            data.extend_from_slice(&hostile_signature(data_size));

            let err = RecordReader::new(Cursor::new(data), ReaderOptions::new())
                .err()
                .expect("hostile signature chunk must be rejected");
            assert!(
                err.to_string().contains("file signature"),
                "data_size={data_size}: unexpected error: {err}"
            );
        }

        // Sanity: a writer-produced file still opens (its signature IS the
        // canonical constant).
        RecordReader::new(Cursor::new(valid), ReaderOptions::new()).expect("valid file opens");
    }

    /// A reader over a file that grows between reads is supported: hitting
    /// a chunk whose claims exceed the current length re-measures the
    /// stream, and a read after the file has grown must succeed. The
    /// re-measure seeks to the end, so the subsequent data read must
    /// position itself explicitly rather than assume the header read left
    /// the reader at the data start.
    #[test]
    fn growing_file_read_resumes_after_remeasure() {
        struct SharedReader(Rc<RefCell<Cursor<Vec<u8>>>>);
        impl std::io::Read for SharedReader {
            fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
                self.0.borrow_mut().read(buf)
            }
        }
        impl std::io::Seek for SharedReader {
            fn seek(&mut self, pos: std::io::SeekFrom) -> std::io::Result<u64> {
                self.0.borrow_mut().seek(pos)
            }
        }

        let one = write_records(&[b"first"], WriterOptions::new().chunk_size(1));
        let full = write_records(&[b"first", b"second"], WriterOptions::new().chunk_size(1));
        assert_eq!(&full[..one.len()], &one[..]);

        // Truncate right after the second chunk's 40-byte header: the header
        // parses, but its claimed data extends past the current end.
        let cut = one.len() + 40;
        assert!(cut < full.len());
        let shared = Rc::new(RefCell::new(Cursor::new(full[..cut].to_vec())));

        let mut reader = RecordReader::new(SharedReader(Rc::clone(&shared)), ReaderOptions::new())
            .expect("reader new ok");
        assert_eq!(
            reader.read_record().expect("read ok").as_deref(),
            Some(&b"first"[..])
        );

        // Before the file grows, the claim exceeds the stream even after
        // re-measurement: clean error.
        assert!(reader.read_record().is_err(), "truncated read must error");

        // Grow the underlying file to its full contents (position preserved)
        // and retry: the re-measure must accept it AND the data read must
        // land on the chunk, not wherever the re-measure seek ended up.
        {
            let mut c = shared.borrow_mut();
            let pos = c.position();
            *c = Cursor::new(full.clone());
            c.set_position(pos);
        }
        assert_eq!(
            reader
                .read_record()
                .expect("read after growth ok")
                .as_deref(),
            Some(&b"second"[..]),
            "growing-file read must resume correctly after re-measure"
        );
        assert_eq!(reader.read_record().expect("read ok"), None);
    }

    /// A hash-INVALID header's claims are just as hostile as a hash-valid
    /// one's: seek scans peek headers without reporting hash errors inline,
    /// and must not feed unvalidated claims into chunk-end arithmetic (a
    /// u64::MAX claim drives an O(claim) overhead walk — an effective hang).
    #[test]
    fn seek_numeric_rejects_hash_invalid_header_claims() {
        let base = write_records(&[b"only"], WriterOptions::new());
        let mut hostile = hostile_simple_chunk(u64::MAX, 1);
        hostile[0] ^= 0xFF; // corrupt the header hash
        let mut data = base.clone();
        data.extend_from_slice(&hostile);

        let mut reader =
            RecordReader::new(Cursor::new(data), ReaderOptions::new()).expect("reader new ok");
        // Without the peek validity check this call never returns (the
        // overhead walk runs ~2^48 iterations); with it, a clean error.
        let err = reader
            .seek_numeric(u64::MAX / 2)
            .expect_err("corrupt header in scan path must error");
        assert!(
            err.to_string().contains("invalid chunk header hash"),
            "unexpected error: {err}"
        );
    }

    /// Header-claimed sizes beyond the physical stream must produce a clean,
    /// persistent error — no arithmetic overflow (debug panic / release
    /// wrap), no claim-sized allocation, no O(claim) overhead walk. Sweeps
    /// the overflow, mid-range, and barely-past-EOF regimes for both the
    /// data-size and record-count claims.
    #[test]
    fn hostile_header_claims_are_rejected() {
        let base = write_records(&[b"only"], WriterOptions::new());
        for (data_size, num_records) in [
            (u64::MAX, 1u64),   // overflow regime (wraps without saturation)
            (u64::MAX - 40, 1), // offset overflow variant
            (1u64 << 40, 1),    // 1 TiB claim: allocation / walk regime
            (4096, 1),          // modest claim, still past EOF
            (0, u64::MAX >> 8), // maximal record-count claim
            (0, 1u64 << 40),    // mid-range record-count claim
        ] {
            let mut data = base.clone();
            data.extend_from_slice(&hostile_simple_chunk(data_size, num_records));

            let mut reader =
                RecordReader::new(Cursor::new(data), ReaderOptions::new()).expect("reader new ok");
            assert_eq!(
                reader.read_record().expect("read ok").as_deref(),
                Some(&b"only"[..]),
                "data_size={data_size} num_records={num_records}"
            );
            let err = reader
                .read_record()
                .expect_err("hostile claim must be rejected");
            assert!(
                err.to_string().contains("claims"),
                "data_size={data_size} num_records={num_records}: unexpected error: {err}"
            );
            // The rejection must be persistent, like every other read error.
            assert!(
                reader.read_record().is_err(),
                "data_size={data_size} num_records={num_records}: error not persistent"
            );
        }
    }
}
