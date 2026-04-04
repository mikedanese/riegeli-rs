//! Transpose chunk decoder for the Riegeli file format.
//!
//! Decodes `ChunkType::Transposed` chunks produced by the C++ `TransposeEncoder`
//! or the Rust `TransposeChunkEncoder`. The transpose wire format consists of
//! five sections:
//!
//! 1. **Compression type** (`u8`): identifies the compression algorithm used
//!    for the header, buckets, and transitions.
//! 2. **Header length** (`varint64`): the compressed byte count of section 3.
//! 3. **Compressed header**: contains `num_buckets`, `num_buffers`, per-bucket
//!    compressed sizes, per-buffer uncompressed sizes, `num_states`, per-state
//!    tags, next-node indices, subtypes (for varint states), buffer indices
//!    (for states that read data), and `first_node`.
//! 4. **Bucket data**: `num_buckets` concatenated compressed data buckets.
//!    Each bucket contains one or more data buffers concatenated together.
//! 5. **Transitions**: compressed state-machine transition bytes.
//!
//! The decoder parses the header to build a state machine of
//! `StateMachineNode`s, decompresses all data buffers from their buckets,
//! then drives the state machine to reconstruct records using the
//! **backward-writing pattern**: each record is built by prepending field data
//! (tag + value) in reverse field order, then the accumulated backward data is
//! reversed to produce the correct forward byte sequence. Record boundaries
//! (limits) are recorded during backward writing and then reversed/complemented
//! to yield correct forward slicing positions.

use crate::compression::{CompressionType, decompress_with_prefix};
use crate::error::RiegeliError;
use crate::field_projection::FieldProjection;
use crate::proto::{WireType, is_valid_proto_tag, tag_field_number, tag_wire_type};
use crate::simple_chunk::Chunk;
use crate::transpose::internal::{
    SUBMESSAGE_WIRE_TYPE, has_data_buffer, has_subtype, message_id, subtype,
};
use crate::varint::{decode_u32, decode_u64, encode_u32};

/// Identifies the action a state machine node performs during record
/// reconstruction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CallbackType {
    /// No operation -- just follow the `next_node` transition.
    NoOp,
    /// Start of a new record (pushes a record-boundary limit).
    MessageStart,
    /// Start of a submessage (pops from the submessage stack, writes
    /// the tag + length-varint header).
    SubmessageStart,
    /// End of a submessage (pushes the current position onto the
    /// submessage stack).
    SubmessageEnd,
    /// Non-proto record: reads length from the nonproto-lengths buffer
    /// and raw bytes from the nonproto data buffer.
    NonProto,
    /// Copy the pre-encoded tag bytes (and optional inline varint value)
    /// verbatim to the output.
    CopyTag,
    /// Read `data_length` bytes from a data buffer, restore high bits on
    /// all bytes except the last (reversing the varint high-bit stripping),
    /// and prepend the tag + restored varint to the output.
    Varint {
        /// Number of raw varint bytes to read from the buffer.
        data_length: u8,
    },
    /// Read 4 bytes from a data buffer and prepend the tag + those bytes.
    Fixed32,
    /// Read 8 bytes from a data buffer and prepend the tag + those bytes.
    Fixed64,
    /// Read a varint32 length and then that many bytes from a data buffer,
    /// and prepend the tag + length-varint + data.
    StringField,
    /// Existence-only: write tag + zero value, skip data buffer.
    /// The zero value depends on the wire type:
    /// - Varint: single 0x00 byte
    /// - Fixed32: 4 zero bytes
    /// - Fixed64: 8 zero bytes
    /// - LengthDelimited: 0x00 (zero-length)
    ExistenceOnly,
    /// Sentinel node indicating an invalid/unreachable state.
    Failure,
    /// Deferred-resolution callback: at execution time, the node's concrete
    /// callback type is resolved by walking the submessage stack against the
    /// projection's include-field map. Used only when a `FieldProjection` is
    /// active. The node's `node_template_index` identifies the `NodeTemplate`
    /// holding the raw tag, subtype, and tag_length for resolution.
    SelectCallback,
    /// A submessage-end node for an excluded submessage: increments
    /// `skipped_submessage_level` instead of pushing a frame onto the stack.
    SkippedSubmessageEnd,
    /// A submessage-start node for an excluded submessage: decrements
    /// `skipped_submessage_level` instead of popping and writing a header.
    SkippedSubmessageStart,
}

// ---------------------------------------------------------------------------
// Projection-aware types for deferred callback resolution
// ---------------------------------------------------------------------------

/// How a field should be included in projected output.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FieldIncluded {
    /// Include the field fully (tag + value from buffer).
    Yes,
    /// Exclude the field entirely (no output).
    No,
    /// Include the tag but zero the value (existence-only mode).
    ExistenceOnly,
}

/// How a node in the include-field tree contributes to projection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum IncludeType {
    /// Include the field and all its children fully.
    IncludeFully = 0,
    /// Include only specific children (this node is a submessage ancestor).
    IncludeChild = 1,
    /// Include the field's tag but zero its value.
    ExistenceOnly = 2,
}

/// An entry in the include-field map.
#[derive(Debug, Clone, Copy)]
struct IncludedField {
    /// Sequential ID assigned to this field (used as parent_id for children).
    field_id: u32,
    /// How this field is included.
    include_type: IncludeType,
}

/// Sentinel value for the root parent ID (no parent).
const INVALID_POS: u32 = u32::MAX;

/// Pre-computed include-field tree built from a `FieldProjection`.
///
/// Maps `(parent_field_id, field_number)` to `IncludedField`. The root
/// parent ID is `INVALID_POS`. This mirrors C++'s `Context::include_fields`.
#[derive(Debug, Clone)]
struct IncludeFieldMap {
    map: std::collections::HashMap<(u32, u32), IncludedField>,
}

impl IncludeFieldMap {
    /// Build the include-field map from a `FieldProjection`.
    ///
    /// Returns `None` if the projection is `all()` (no filtering needed).
    fn from_projection(projection: &FieldProjection) -> Option<Self> {
        let fields = projection.fields()?;
        let mut map = std::collections::HashMap::new();
        for field in fields {
            let path = &field.path;
            if path.is_empty() {
                // An empty path means include everything — projection disabled.
                return None;
            }
            let path_len = path.len();
            let existence_only = field.existence_only;
            let mut current_id = INVALID_POS;
            for (i, &field_number) in path.iter().enumerate() {
                let next_id = map.len() as u32;
                let include_type = if i + 1 == path_len {
                    if existence_only {
                        IncludeType::ExistenceOnly
                    } else {
                        IncludeType::IncludeFully
                    }
                } else {
                    IncludeType::IncludeChild
                };
                let entry = map
                    .entry((current_id, field_number))
                    .or_insert(IncludedField {
                        field_id: next_id,
                        include_type,
                    });
                // If the same key is inserted multiple times, use the more
                // inclusive type (lower ordinal wins, matching C++'s std::min).
                if include_type < entry.include_type {
                    entry.include_type = include_type;
                }
                current_id = entry.field_id;
            }
        }
        Some(Self { map })
    }

    /// Look up an entry in the include-field map.
    fn get(&self, parent_id: u32, field_number: u32) -> Option<&IncludedField> {
        self.map.get(&(parent_id, field_number))
    }
}

/// Template data stored for a `SelectCallback` node, holding the information
/// needed to resolve the concrete callback type at execution time.
#[derive(Debug, Clone)]
struct NodeTemplate {
    /// The proto tag (with submessage wire type already mapped to LengthDelimited).
    tag: u32,
    /// The subtype byte.
    subtype: u8,
    /// Length of the tag varint encoding.
    tag_length: usize,
}

/// A single node in the transpose state machine.
///
/// Each node represents one "action" in the record-reconstruction process.
/// The state machine is driven by transition bytes that select which node
/// to visit next.
#[derive(Debug, Clone)]
struct StateMachineNode {
    /// Pre-encoded tag bytes (varint-encoded proto tag).
    /// For inline varint nodes, includes the inline value byte after the tag.
    tag_data: Vec<u8>,
    /// Number of bytes from `tag_data` to copy for [`CallbackType::CopyTag`].
    tag_data_size: usize,
    /// What this node does when executed.
    callback_type: CallbackType,
    /// Index into the decompressed buffers vec, if this node reads data.
    buffer_index: Option<usize>,
    /// Index of the next node to transition to.
    next_node_index: usize,
    /// Whether the transition to `next_node_index` is implicit (no transition
    /// byte consumed from the transitions stream).
    is_implicit: bool,
    /// Index into the `node_templates` vec for [`CallbackType::SelectCallback`]
    /// nodes. `None` for non-projection nodes.
    node_template_index: Option<usize>,
}

/// A cursor over a byte buffer that tracks the current read position.
///
/// Used to read sequentially from decompressed data buffers and from the
/// decompressed header during parsing.
///
/// When `pruned` is `true`, this buffer was skipped during decompression as
/// part of a field projection optimisation. All reads return zero bytes /
/// values instead of real data.
#[derive(Debug)]
struct BufferCursor {
    /// The underlying byte data.
    data: Vec<u8>,
    /// Current read position.
    pos: usize,
    /// When `true`, this buffer has been pruned (not decompressed).
    /// All read operations return zero values and succeed without error.
    pruned: bool,
    /// Scratch storage for `read_exact` on a pruned buffer.
    scratch: Vec<u8>,
}

impl BufferCursor {
    /// Create a new cursor at position 0.
    fn new(data: Vec<u8>) -> Self {
        Self {
            data,
            pos: 0,
            pruned: false,
            scratch: Vec::new(),
        }
    }

    /// Create a pruned (zero-data) cursor.
    fn pruned() -> Self {
        Self {
            data: Vec::new(),
            pos: 0,
            pruned: true,
            scratch: Vec::new(),
        }
    }

    /// Read exactly `n` bytes, advancing the position.
    fn read_exact(&mut self, n: usize) -> Result<&[u8], RiegeliError> {
        if self.pruned {
            self.scratch.clear();
            self.scratch.resize(n, 0u8);
            return Ok(&self.scratch);
        }
        if self.pos + n > self.data.len() {
            return Err(RiegeliError::MalformedData(
                "buffer underflow in transpose chunk".to_string(),
            ));
        }
        let slice = &self.data[self.pos..self.pos + n];
        self.pos += n;
        Ok(slice)
    }

    /// Decode and return a varint32, advancing the position.
    fn read_varint32(&mut self) -> Result<u32, RiegeliError> {
        if self.pruned {
            return Ok(0);
        }
        let remaining = &self.data[self.pos..];
        let (val, consumed) = decode_u32(remaining).map_err(|e| {
            RiegeliError::MalformedData(format!("varint32 decode error in buffer: {e}"))
        })?;
        self.pos += consumed;
        Ok(val)
    }

    /// Decode and return a varint64, advancing the position.
    fn read_varint64(&mut self) -> Result<u64, RiegeliError> {
        if self.pruned {
            return Ok(0);
        }
        let remaining = &self.data[self.pos..];
        let (val, consumed) = decode_u64(remaining).map_err(|e| {
            RiegeliError::MalformedData(format!("varint64 decode error in buffer: {e}"))
        })?;
        self.pos += consumed;
        Ok(val)
    }

    /// Read a single byte, advancing the position.
    fn read_byte(&mut self) -> Result<u8, RiegeliError> {
        if self.pruned {
            return Ok(0);
        }
        if self.pos >= self.data.len() {
            return Err(RiegeliError::MalformedData(
                "buffer underflow reading byte".to_string(),
            ));
        }
        let b = self.data[self.pos];
        self.pos += 1;
        Ok(b)
    }

    /// Returns `true` if no more bytes remain.
    fn is_empty(&self) -> bool {
        if self.pruned {
            return true;
        }
        self.pos >= self.data.len()
    }

    /// Return the current position for later restoration.
    fn save_pos(&self) -> usize {
        self.pos
    }

    /// Restore a previously saved position.
    fn restore_pos(&mut self, saved: usize) {
        self.pos = saved;
    }

    /// Clone the cursor data for the purpose of scanning needed buffers.
    /// Only used internally; does not copy `scratch` or `pruned`.
    fn clone_for_needed_buffers(&self, _num_buffers: usize) -> BufferCursorSnapshot {
        BufferCursorSnapshot {
            data: self.data.clone(),
            pos: self.pos,
        }
    }
}

/// A lightweight snapshot of a `BufferCursor` used for scanning buffer
/// indices before decompression.
struct BufferCursorSnapshot {
    data: Vec<u8>,
    pos: usize,
}

impl BufferCursorSnapshot {
    /// Decode a varint32 from the snapshot, returning `None` on failure.
    fn read_varint32(&mut self) -> Option<u32> {
        let remaining = &self.data[self.pos..];
        let (val, consumed) = decode_u32(remaining).ok()?;
        self.pos += consumed;
        Some(val)
    }
}

/// A buffer that supports backward writing (prepending).
///
/// The C++ transpose decoder uses a `BackwardWriter` that prepends bytes to the
/// output. This struct replicates that behaviour: internally, bytes are appended
/// in reverse order and the whole buffer is reversed at the end via
/// [`into_forward`](BackwardBuffer::into_forward).
struct BackwardBuffer {
    /// Data stored in reverse order.
    data: Vec<u8>,
}

impl BackwardBuffer {
    /// Create a new backward buffer with the given capacity hint.
    fn new(capacity: usize) -> Self {
        Self {
            data: Vec::with_capacity(capacity),
        }
    }

    /// Current number of bytes written.
    fn pos(&self) -> usize {
        self.data.len()
    }

    /// Prepend `bytes` to the buffer.
    ///
    /// In backward-writer semantics this places `bytes` before all previously
    /// written data. Internally the bytes are appended in reverse order.
    fn write(&mut self, bytes: &[u8]) {
        for &b in bytes.iter().rev() {
            self.data.push(b);
        }
    }

    /// Consume the buffer and return the data in correct forward order.
    fn into_forward(mut self) -> Vec<u8> {
        self.data.reverse();
        self.data
    }
}

/// A frame on the decoder's submessage stack.
///
/// Pushed when a [`CallbackType::SubmessageEnd`] node is visited; popped when
/// the matching [`CallbackType::SubmessageStart`] node is visited.
struct SubmessageFrame {
    /// The backward-buffer position at the point the submessage end was seen.
    end_pos: usize,
    /// The state machine node index of the submessage-end node (whose
    /// `tag_data` contains the tag bytes for the enclosing length-delimited
    /// field).
    node_index: usize,
}

/// State machine metadata extracted from the header.
struct StateMetadata {
    /// Raw tag values for each state.
    tags: Vec<u32>,
    /// Raw next-node indices for each state.
    next_node_indices: Vec<u32>,
    /// Subtype bytes (only for states where `has_subtype` is true).
    subtypes_bytes: Vec<u8>,
    /// Number of states.
    num_states: usize,
}

/// Result of parsing the transpose header and decompressing buckets.
struct ParsedHeader {
    /// Decompressed data buffers (one per buffer index).
    buffers: Vec<BufferCursor>,
    /// Number of states in the state machine.
    num_states: usize,
    /// Raw tag values for each state.
    tags: Vec<u32>,
    /// Raw next-node indices for each state.
    next_node_indices: Vec<u32>,
    /// Subtype bytes (only for states where `has_subtype` is true).
    subtypes_bytes: Vec<u8>,
    /// Remaining header cursor (positioned just after subtypes, ready to
    /// read buffer indices and first_node).
    hdr: BufferCursor,
    /// Number of buffers.
    num_buffers: usize,
    /// Compression type for transitions.
    compression_type: CompressionType,
    /// Raw transitions bytes (compressed).
    transitions_compressed: Vec<u8>,
}

/// Result of building state machine nodes from the parsed header.
struct BuiltStateMachine {
    /// The state machine nodes (includes sentinel failure nodes).
    nodes: Vec<StateMachineNode>,
    /// Index of the first node to execute.
    first_node: usize,
    /// Whether any node uses the NonProto callback.
    has_nonproto_op: bool,
    /// Templates for `SelectCallback` nodes (only populated when projection active).
    node_templates: Vec<NodeTemplate>,
}

/// Decodes a transposed chunk, yielding records one at a time.
///
/// Constructed from a [`Chunk`] with `ChunkType::Transposed`. Eagerly parses
/// the header, decompresses all data buffers, and runs the state machine to
/// reconstruct all records during construction.
///
/// Records are stored in a single contiguous buffer with a parallel `limits`
/// array of record-end positions, matching the C++ `ChunkDecoder` layout.
/// This avoids N per-record heap allocations.
pub struct TransposeChunkDecoder {
    /// All record bytes concatenated in forward order.
    data: Vec<u8>,
    /// Sorted record-end positions into `data`. `limits[i]` is the exclusive
    /// end of record `i`; `limits[i-1]` (or 0) is the start.
    limits: Vec<usize>,
    /// Index of the next record to yield.
    next_yield: usize,
}

impl TransposeChunkDecoder {
    /// Parse the transpose chunk header and decode all records.
    pub fn new(chunk: Chunk) -> Result<Self, RiegeliError> {
        Self::new_with_projection(chunk, None)
    }

    /// Parse the transpose chunk header and decode all records, optionally
    /// applying a `FieldProjection` to prune unneeded data buffers and filter
    /// decoded records.
    ///
    /// When `projection` is `None` or `FieldProjection::all()`, this behaves
    /// identically to `new`.
    pub fn new_with_projection(
        chunk: Chunk,
        projection: Option<&FieldProjection>,
    ) -> Result<Self, RiegeliError> {
        let num_records = chunk.header.num_records();
        let decoded_data_size = chunk.header.decoded_data_size();

        if num_records == 0 {
            return Ok(Self {
                data: Vec::new(),
                limits: Vec::new(),
                next_yield: 0,
            });
        }

        // Determine if projection filtering is active.
        let active_projection: Option<&FieldProjection> = match projection {
            Some(p) if !p.is_all() => Some(p),
            _ => None,
        };

        let mut parsed = Self::parse_header_and_buckets_with_projection(&chunk, active_projection)?;

        // Build include-field map when projection is active.
        let include_map = active_projection.and_then(IncludeFieldMap::from_projection);
        let built = Self::build_state_machine_nodes(&mut parsed, include_map.is_some())?;

        // Determine nonproto_lengths buffer index (always the last buffer).
        let nonproto_lengths_index = if built.has_nonproto_op {
            if parsed.num_buffers == 0 {
                return Err(RiegeliError::MalformedData(
                    "nonproto op but no buffers".to_string(),
                ));
            }
            Some(parsed.num_buffers - 1)
        } else {
            None
        };

        // Decompress transitions.
        // Transitions use EncodeAndClose format (varint prefix for compressed types).
        let transitions_data =
            decompress_with_prefix(&parsed.transitions_compressed, parsed.compression_type)?;

        let (decoded_data, limits) = decode_all_records(
            num_records,
            decoded_data_size,
            &built.nodes,
            &mut parsed.buffers,
            &mut BufferCursor::new(transitions_data),
            built.first_node,
            nonproto_lengths_index,
            &built.node_templates,
            include_map.as_ref(),
        )?;

        // When projection-during-decode is active (include_map was Some), the
        // decoded data is already narrow — no post-processing apply() needed.
        // The apply() fallback is only used for non-transpose code paths.
        let (data, limits) = (decoded_data, limits);

        Ok(Self {
            data,
            limits,
            next_yield: 0,
        })
    }

    /// Like `parse_header_and_buckets` (without projection) but with optional projection for
    /// bucket pruning.
    ///
    /// When `projection` is `Some(proj)` (and not `all()`), this function:
    /// 1. Parses the header (cheap).
    /// 2. Reads the state machine metadata to determine which buffers are
    ///    needed by projected fields.
    /// 3. Only decompresses buckets that contain at least one needed buffer.
    /// 4. Stubs out pruned buffers with `BufferCursor::pruned()`.
    fn parse_header_and_buckets_with_projection(
        chunk: &Chunk,
        projection: Option<&FieldProjection>,
    ) -> Result<ParsedHeader, RiegeliError> {
        let data = &chunk.data;
        let mut pos: usize = 0;

        if data.is_empty() {
            return Err(RiegeliError::MalformedData(
                "transpose chunk data is empty".to_string(),
            ));
        }
        let compression_type = CompressionType::try_from(data[0])?;
        pos += 1;

        let (header_length, consumed) = decode_u64(&data[pos..])
            .map_err(|e| RiegeliError::MalformedData(format!("reading header_length: {e}")))?;
        pos += consumed;

        let header_end = pos + header_length as usize;
        if header_end > data.len() {
            return Err(RiegeliError::MalformedData(
                "transpose header extends past chunk data".to_string(),
            ));
        }
        let header_compressed = &data[pos..header_end];
        pos = header_end;

        // The header uses LengthPrefixed format: the blob may contain a
        // varint64(uncompressed_size) prefix for compressed types.
        let header_data = decompress_with_prefix(header_compressed, compression_type)?;
        let mut hdr = BufferCursor::new(header_data);

        let num_buckets = hdr.read_varint32()? as usize;
        let num_buffers = hdr.read_varint32()? as usize;

        let mut bucket_compressed_sizes = Vec::with_capacity(num_buckets);
        for _ in 0..num_buckets {
            bucket_compressed_sizes.push(hdr.read_varint64()? as usize);
        }
        let mut buffer_uncompressed_sizes = Vec::with_capacity(num_buffers);
        for _ in 0..num_buffers {
            buffer_uncompressed_sizes.push(hdr.read_varint64()? as usize);
        }

        // Parse state machine metadata (needed for projection even if we
        // haven't decompressed buffers yet).
        let sm = Self::parse_state_metadata(&mut hdr)?;

        // Read raw bucket data from sections 4.
        let (bucket_compressed_data, new_pos) =
            Self::read_bucket_data(data, pos, &bucket_compressed_sizes)?;
        let transitions_compressed = data[new_pos..].to_vec();

        // Determine which buffers are needed.
        let needed_buffers = if let Some(proj) = projection {
            // Save current header position (just after subtypes).
            // Scan buffer_index entries for each state to build needed set.
            let buf_idx_scan_pos = hdr.save_pos();
            let mut snap = hdr.clone_for_needed_buffers(num_buffers);
            let result = Self::compute_needed_buffers_from_scan(
                &sm.tags,
                &sm.subtypes_bytes,
                &mut snap,
                num_buffers,
                proj,
            );
            // Restore position so build_state_machine_nodes can re-read
            // buffer indices normally.
            hdr.restore_pos(buf_idx_scan_pos);
            result
        } else {
            // All buffers needed.
            vec![true; num_buffers]
        };

        // Decompress buckets and split into individual buffers, pruning
        // buffers not in the needed set.
        let buffers = Self::decompress_into_buffers_with_pruning(
            &bucket_compressed_data,
            &buffer_uncompressed_sizes,
            compression_type,
            &needed_buffers,
        )?;

        Ok(ParsedHeader {
            buffers,
            num_states: sm.num_states,
            tags: sm.tags,
            next_node_indices: sm.next_node_indices,
            subtypes_bytes: sm.subtypes_bytes,
            hdr,
            num_buffers,
            compression_type,
            transitions_compressed,
        })
    }

    /// Compute the set of buffer indices that are needed for a given
    /// `FieldProjection`.
    ///
    /// Reads through the buffer_index section of the header (using a snapshot
    /// cursor so the main cursor position is not affected) to determine which
    /// buffers correspond to which fields.
    ///
    /// A buffer is needed if:
    /// - Its state's field number is included in the projection, OR
    /// - It belongs to a NonProto state (non-proto records pass through), OR
    /// - It is the nonproto lengths buffer (last buffer, if any nonproto exists).
    fn compute_needed_buffers_from_scan(
        tags: &[u32],
        subtypes_bytes: &[u8],
        snap: &mut BufferCursorSnapshot,
        num_buffers: usize,
        projection: &FieldProjection,
    ) -> Vec<bool> {
        if num_buffers == 0 {
            return Vec::new();
        }
        let mut needed = vec![false; num_buffers];
        let mut subtype_idx: usize = 0;
        let mut has_nonproto = false;

        for &raw_tag in tags {
            // Determine if this state reads from a buffer.
            let (reads_buffer, field_number_opt) = match raw_tag {
                t if t == message_id::NO_OP
                    || t == message_id::START_OF_MESSAGE
                    || t == message_id::START_OF_SUBMESSAGE =>
                {
                    (false, None)
                }
                t if t == message_id::NON_PROTO => {
                    has_nonproto = true;
                    (true, None) // nonproto — always needed
                }
                _ => {
                    // Proto tag — check wire type.
                    let mut tag = raw_tag;
                    let wire_raw = tag & 7;
                    // Map submessage wire type to LengthDelimited.
                    let st: u8 = if wire_raw == crate::transpose::internal::SUBMESSAGE_WIRE_TYPE {
                        tag = tag - crate::transpose::internal::SUBMESSAGE_WIRE_TYPE
                            + WireType::LengthDelimited as u32;
                        crate::transpose::internal::subtype::LENGTH_DELIMITED_END_OF_SUBMESSAGE
                    } else if is_valid_proto_tag(tag) && has_subtype(tag) {
                        let s = subtypes_bytes.get(subtype_idx).copied().unwrap_or(0);
                        subtype_idx += 1;
                        s
                    } else {
                        crate::transpose::internal::subtype::TRIVIAL
                    };

                    if is_valid_proto_tag(tag) && has_data_buffer(tag, st) {
                        let fn_num = tag_field_number(tag);
                        (true, Some(fn_num))
                    } else {
                        (false, None)
                    }
                }
            };

            if reads_buffer && let Some(buf_idx) = snap.read_varint32() {
                let buf_idx = buf_idx as usize;
                if buf_idx < num_buffers {
                    // Check if this buffer is needed.
                    let needed_for_field = match field_number_opt {
                        None => true, // nonproto — always needed
                        Some(fn_num) => projection.includes_top_level_field(fn_num),
                    };
                    if needed_for_field {
                        needed[buf_idx] = true;
                    }
                }
            }
        }

        // The nonproto lengths buffer is always the last buffer if nonproto exists.
        if has_nonproto && num_buffers > 0 {
            needed[num_buffers - 1] = true;
        }

        needed
    }

    /// Read raw compressed bucket data from the chunk.
    fn read_bucket_data(
        data: &[u8],
        mut pos: usize,
        bucket_compressed_sizes: &[usize],
    ) -> Result<(Vec<Vec<u8>>, usize), RiegeliError> {
        let mut bucket_compressed_data = Vec::with_capacity(bucket_compressed_sizes.len());
        for &size in bucket_compressed_sizes {
            let end = pos + size;
            if end > data.len() {
                return Err(RiegeliError::MalformedData(
                    "bucket data extends past chunk data".to_string(),
                ));
            }
            bucket_compressed_data.push(data[pos..end].to_vec());
            pos = end;
        }
        Ok((bucket_compressed_data, pos))
    }

    /// Read the decompressed size of a bucket without actually decompressing it.
    ///
    /// For `CompressionType::None`, the compressed data *is* the decompressed
    /// data, so the size is just the length. For compressed types, the bucket
    /// uses `EncodeAndClose` format with a varint64 prefix giving the
    /// uncompressed size.
    fn bucket_decompressed_size(
        compressed: &[u8],
        compression_type: CompressionType,
    ) -> Result<usize, RiegeliError> {
        if compression_type == CompressionType::None {
            return Ok(compressed.len());
        }
        let (size, _consumed) = decode_u64(compressed).map_err(|e| {
            RiegeliError::MalformedData(format!("reading bucket uncompressed_size prefix: {e}"))
        })?;
        Ok(size as usize)
    }

    /// Compute the mapping from buffer indices to bucket indices.
    ///
    /// Returns a `Vec<usize>` where `result[buffer_i]` is the bucket index
    /// that contains buffer `buffer_i`. The mapping is derived by summing
    /// buffer uncompressed sizes and comparing against bucket decompressed
    /// sizes.
    fn compute_buffer_to_bucket(
        bucket_compressed_data: &[Vec<u8>],
        buffer_uncompressed_sizes: &[usize],
        compression_type: CompressionType,
    ) -> Result<Vec<usize>, RiegeliError> {
        let num_buckets = bucket_compressed_data.len();
        let num_buffers = buffer_uncompressed_sizes.len();
        let mut buffer_to_bucket = Vec::with_capacity(num_buffers);

        if num_buckets == 0 || num_buffers == 0 {
            return Ok(buffer_to_bucket);
        }

        // Compute decompressed size of each bucket (without decompressing).
        let mut bucket_decompressed_sizes = Vec::with_capacity(num_buckets);
        for compressed in bucket_compressed_data {
            bucket_decompressed_sizes.push(Self::bucket_decompressed_size(
                compressed,
                compression_type,
            )?);
        }

        let mut bucket_index: usize = 0;
        let mut bucket_remaining: usize = bucket_decompressed_sizes[0];

        for (i, &buf_size) in buffer_uncompressed_sizes.iter().enumerate() {
            // Advance to next bucket if current one is exhausted.
            while bucket_remaining == 0 && bucket_index + 1 < num_buckets {
                bucket_index += 1;
                bucket_remaining = bucket_decompressed_sizes[bucket_index];
            }
            if buf_size > bucket_remaining {
                return Err(RiegeliError::MalformedData(format!(
                    "buffer {} (size {}) exceeds remaining bucket {} capacity ({})",
                    i, buf_size, bucket_index, bucket_remaining
                )));
            }
            buffer_to_bucket.push(bucket_index);
            bucket_remaining -= buf_size;
        }

        Ok(buffer_to_bucket)
    }

    /// Decompress buckets into per-buffer cursors with lazy bucket
    /// decompression, pruning buffers not in `needed_buffers`.
    ///
    /// Only decompresses buckets that contain at least one needed buffer.
    /// Buckets with zero needed buffers are never passed to the decompression
    /// codec. Buffers whose index is `false` in `needed_buffers` get a
    /// `BufferCursor::pruned()` stub that returns zero bytes on reads.
    fn decompress_into_buffers_with_pruning(
        bucket_compressed_data: &[Vec<u8>],
        buffer_uncompressed_sizes: &[usize],
        compression_type: CompressionType,
        needed_buffers: &[bool],
    ) -> Result<Vec<BufferCursor>, RiegeliError> {
        let num_buckets = bucket_compressed_data.len();
        let num_buffers = buffer_uncompressed_sizes.len();

        if num_buckets == 0 || num_buffers == 0 {
            return Ok(Vec::new());
        }

        // Step 1: Compute which buffer belongs to which bucket.
        let buffer_to_bucket = Self::compute_buffer_to_bucket(
            bucket_compressed_data,
            buffer_uncompressed_sizes,
            compression_type,
        )?;

        // Step 2: Determine which buckets need decompression.
        let mut bucket_needed = vec![false; num_buckets];
        for (i, &needed) in needed_buffers.iter().enumerate() {
            if needed && let Some(&bi) = buffer_to_bucket.get(i) {
                bucket_needed[bi] = true;
            }
        }

        // Step 3: Decompress only needed buckets (lazy at bucket level).
        let mut bucket_decompressed: Vec<Option<Vec<u8>>> = Vec::with_capacity(num_buckets);
        for (bi, compressed) in bucket_compressed_data.iter().enumerate() {
            if bucket_needed[bi] {
                let decompressed = decompress_with_prefix(compressed, compression_type)?;
                bucket_decompressed.push(Some(decompressed));
            } else {
                bucket_decompressed.push(None);
            }
        }

        // Step 4: Extract individual buffers from decompressed bucket data.
        let mut buffers: Vec<BufferCursor> = Vec::with_capacity(num_buffers);
        // Track the current offset within each bucket's decompressed data.
        let mut bucket_offsets = vec![0usize; num_buckets];

        for (i, &buf_size) in buffer_uncompressed_sizes.iter().enumerate() {
            let bi = buffer_to_bucket[i];
            let offset = bucket_offsets[bi];

            let is_needed = needed_buffers.get(i).copied().unwrap_or(true);
            if is_needed {
                let decompressed = bucket_decompressed[bi].as_ref().ok_or_else(|| {
                    RiegeliError::MalformedData(format!(
                        "needed buffer {} in undecompressed bucket {}",
                        i, bi
                    ))
                })?;
                let end = offset + buf_size;
                if end > decompressed.len() {
                    return Err(RiegeliError::MalformedData(format!(
                        "buffer {} (size {}) exceeds bucket {} data (len {})",
                        i,
                        buf_size,
                        bi,
                        decompressed.len()
                    )));
                }
                buffers.push(BufferCursor::new(decompressed[offset..end].to_vec()));
            } else {
                buffers.push(BufferCursor::pruned());
            }
            bucket_offsets[bi] = offset + buf_size;
        }

        Ok(buffers)
    }

    /// Parse state machine metadata (tags, next-node indices, subtypes) from
    /// the header cursor.
    fn parse_state_metadata(hdr: &mut BufferCursor) -> Result<StateMetadata, RiegeliError> {
        let num_states = hdr.read_varint32()? as usize;

        let mut tags = Vec::with_capacity(num_states);
        for _ in 0..num_states {
            tags.push(hdr.read_varint32()?);
        }
        let mut next_node_indices = Vec::with_capacity(num_states);
        for _ in 0..num_states {
            next_node_indices.push(hdr.read_varint32()?);
        }

        let mut num_subtypes = 0usize;
        for &tag in &tags {
            if is_valid_proto_tag(tag) && has_subtype(tag) {
                num_subtypes += 1;
            }
        }
        let mut subtypes_bytes = Vec::with_capacity(num_subtypes);
        for _ in 0..num_subtypes {
            subtypes_bytes.push(hdr.read_byte()?);
        }

        Ok(StateMetadata {
            tags,
            next_node_indices,
            subtypes_bytes,
            num_states,
        })
    }

    /// Build [`StateMachineNode`]s from the parsed header, reading buffer
    /// indices and `first_node`, then validate (implicit-loop detection).
    fn build_state_machine_nodes(
        parsed: &mut ParsedHeader,
        projection_enabled: bool,
    ) -> Result<BuiltStateMachine, RiegeliError> {
        let num_states = parsed.num_states;
        let num_buffers = parsed.num_buffers;
        let hdr = &mut parsed.hdr;

        let mut nodes: Vec<StateMachineNode> = Vec::with_capacity(num_states + 0xFF);
        let mut node_templates: Vec<NodeTemplate> = Vec::new();
        let mut subtype_idx: usize = 0;
        let mut has_nonproto_op = false;

        for i in 0..num_states {
            let raw_tag = parsed.tags[i];
            let next_raw = parsed.next_node_indices[i] as usize;

            let (is_implicit, next_node_idx) = if next_raw >= num_states {
                let adjusted = next_raw - num_states;
                if adjusted >= num_states {
                    return Err(RiegeliError::MalformedData(format!(
                        "node index {} too large (num_states={})",
                        adjusted, num_states
                    )));
                }
                (true, adjusted)
            } else {
                (false, next_raw)
            };

            let node = Self::build_single_node(
                raw_tag,
                next_node_idx,
                is_implicit,
                i,
                hdr,
                num_buffers,
                &parsed.subtypes_bytes,
                &mut subtype_idx,
                &mut has_nonproto_op,
                projection_enabled,
                &mut node_templates,
            )?;
            nodes.push(node);
        }

        // Read first_node.
        let first_node = hdr.read_varint32()? as usize;
        if num_states > 0 && first_node >= num_states {
            return Err(RiegeliError::MalformedData(format!(
                "first_node {} >= num_states {}",
                first_node, num_states
            )));
        }

        // Add 0xFF failure sentinel nodes.
        for _ in 0..0xFF_usize {
            nodes.push(StateMachineNode {
                tag_data: Vec::new(),
                tag_data_size: 0,
                callback_type: CallbackType::Failure,
                buffer_index: None,
                next_node_index: 0,
                is_implicit: false,
                node_template_index: None,
            });
        }

        // Validate: check for implicit loops.
        if contains_implicit_loop(&nodes, num_states) {
            return Err(RiegeliError::MalformedData(
                "state machine contains an implicit loop".to_string(),
            ));
        }

        Ok(BuiltStateMachine {
            nodes,
            first_node,
            has_nonproto_op,
            node_templates,
        })
    }

    /// Build a single [`StateMachineNode`] from its raw tag and metadata.
    #[allow(clippy::too_many_arguments)]
    fn build_single_node(
        raw_tag: u32,
        next_node_idx: usize,
        is_implicit: bool,
        state_index: usize,
        hdr: &mut BufferCursor,
        num_buffers: usize,
        subtypes_bytes: &[u8],
        subtype_idx: &mut usize,
        has_nonproto_op: &mut bool,
        projection_enabled: bool,
        node_templates: &mut Vec<NodeTemplate>,
    ) -> Result<StateMachineNode, RiegeliError> {
        // Reserved message IDs (tag < 8).
        match raw_tag {
            t if t == message_id::NO_OP => {
                return Ok(StateMachineNode {
                    tag_data: Vec::new(),
                    tag_data_size: 0,
                    callback_type: CallbackType::NoOp,
                    buffer_index: None,
                    next_node_index: next_node_idx,
                    is_implicit,
                    node_template_index: None,
                });
            }
            t if t == message_id::NON_PROTO => {
                let buf_idx = hdr.read_varint32()? as usize;
                if buf_idx >= num_buffers {
                    return Err(RiegeliError::MalformedData(
                        "nonproto buffer index too large".to_string(),
                    ));
                }
                *has_nonproto_op = true;
                return Ok(StateMachineNode {
                    tag_data: Vec::new(),
                    tag_data_size: 0,
                    callback_type: CallbackType::NonProto,
                    buffer_index: Some(buf_idx),
                    next_node_index: next_node_idx,
                    is_implicit,
                    node_template_index: None,
                });
            }
            t if t == message_id::START_OF_MESSAGE => {
                return Ok(StateMachineNode {
                    tag_data: Vec::new(),
                    tag_data_size: 0,
                    callback_type: CallbackType::MessageStart,
                    buffer_index: None,
                    next_node_index: next_node_idx,
                    is_implicit,
                    node_template_index: None,
                });
            }
            t if t == message_id::START_OF_SUBMESSAGE => {
                if projection_enabled {
                    // When projection is active, StartOfSubmessage gets a
                    // SelectCallback so it can be resolved at execution time
                    // to either SubmessageStart or SkippedSubmessageStart.
                    let tmpl_idx = node_templates.len();
                    node_templates.push(NodeTemplate {
                        tag: message_id::START_OF_SUBMESSAGE,
                        subtype: 0,
                        tag_length: 0,
                    });
                    return Ok(StateMachineNode {
                        tag_data: Vec::new(),
                        tag_data_size: 0,
                        callback_type: CallbackType::SelectCallback,
                        buffer_index: None,
                        next_node_index: next_node_idx,
                        is_implicit,
                        node_template_index: Some(tmpl_idx),
                    });
                } else {
                    return Ok(StateMachineNode {
                        tag_data: Vec::new(),
                        tag_data_size: 0,
                        callback_type: CallbackType::SubmessageStart,
                        buffer_index: None,
                        next_node_index: next_node_idx,
                        is_implicit,
                        node_template_index: None,
                    });
                }
            }
            _ => {}
        }

        Self::build_proto_tag_node(
            raw_tag,
            next_node_idx,
            is_implicit,
            state_index,
            hdr,
            num_buffers,
            subtypes_bytes,
            subtype_idx,
            projection_enabled,
            node_templates,
        )
    }

    /// Build a [`StateMachineNode`] for a proto tag (not a reserved message ID).
    #[allow(clippy::too_many_arguments)]
    fn build_proto_tag_node(
        raw_tag: u32,
        next_node_idx: usize,
        is_implicit: bool,
        state_index: usize,
        hdr: &mut BufferCursor,
        num_buffers: usize,
        subtypes_bytes: &[u8],
        subtype_idx: &mut usize,
        projection_enabled: bool,
        node_templates: &mut Vec<NodeTemplate>,
    ) -> Result<StateMachineNode, RiegeliError> {
        let mut tag = raw_tag;
        let mut st: u8 = subtype::TRIVIAL;

        // Check for submessage end (synthetic wire type 6).
        let wire_raw = tag & 7;
        if wire_raw == SUBMESSAGE_WIRE_TYPE {
            tag = tag - SUBMESSAGE_WIRE_TYPE + WireType::LengthDelimited as u32;
            st = subtype::LENGTH_DELIMITED_END_OF_SUBMESSAGE;
        }

        if !is_valid_proto_tag(tag) {
            return Err(RiegeliError::MalformedData(format!(
                "invalid tag {} in state {}",
                tag, state_index
            )));
        }

        let tag_bytes = encode_u32(tag);
        let tag_length = tag_bytes.len();

        if has_subtype(tag) {
            st = subtypes_bytes[*subtype_idx];
            *subtype_idx += 1;
        }

        let buf_idx = if has_data_buffer(tag, st) {
            let idx = hdr.read_varint32()? as usize;
            if idx >= num_buffers {
                return Err(RiegeliError::MalformedData(
                    "buffer index too large".to_string(),
                ));
            }
            Some(idx)
        } else {
            None
        };

        let mut tag_data_vec = tag_bytes;
        let tag_data_size =
            if tag_wire_type(tag) == Some(WireType::Varint) && st >= subtype::VARINT_INLINE_0 {
                tag_data_vec.push(st - subtype::VARINT_INLINE_0);
                tag_length + 1
            } else {
                tag_length
            };

        if projection_enabled {
            // When projection is active, proto field nodes get SelectCallback
            // so their inclusion can be resolved at execution time.
            let tmpl_idx = node_templates.len();
            node_templates.push(NodeTemplate {
                tag,
                subtype: st,
                tag_length,
            });
            Ok(StateMachineNode {
                tag_data: tag_data_vec,
                tag_data_size,
                callback_type: CallbackType::SelectCallback,
                buffer_index: buf_idx,
                next_node_index: next_node_idx,
                is_implicit,
                node_template_index: Some(tmpl_idx),
            })
        } else {
            let wt = tag_wire_type(tag);
            let callback_type = Self::callback_for_wire_type(wt, st)?;
            Ok(StateMachineNode {
                tag_data: tag_data_vec,
                tag_data_size,
                callback_type,
                buffer_index: buf_idx,
                next_node_index: next_node_idx,
                is_implicit,
                node_template_index: None,
            })
        }
    }

    /// Determine the callback type from a wire type and subtype.
    fn callback_for_wire_type(wt: Option<WireType>, st: u8) -> Result<CallbackType, RiegeliError> {
        match wt {
            Some(WireType::Varint) => {
                if st >= subtype::VARINT_INLINE_0 {
                    Ok(CallbackType::CopyTag)
                } else {
                    Ok(CallbackType::Varint {
                        data_length: st + 1,
                    })
                }
            }
            Some(WireType::Fixed32) => Ok(CallbackType::Fixed32),
            Some(WireType::Fixed64) => Ok(CallbackType::Fixed64),
            Some(WireType::LengthDelimited) => match st {
                s if s == subtype::LENGTH_DELIMITED_STRING => Ok(CallbackType::StringField),
                s if s == subtype::LENGTH_DELIMITED_END_OF_SUBMESSAGE => {
                    Ok(CallbackType::SubmessageEnd)
                }
                _ => Err(RiegeliError::MalformedData(format!(
                    "unknown LengthDelimited subtype {st}"
                ))),
            },
            Some(WireType::StartGroup) | Some(WireType::EndGroup) => Ok(CallbackType::CopyTag),
            None => Err(RiegeliError::MalformedData(
                "invalid wire type in tag".to_string(),
            )),
        }
    }

    /// Read the next record from this transpose chunk.
    ///
    /// Returns `Ok(None)` when all records have been yielded.
    pub fn read_record(&mut self) -> Result<Option<Vec<u8>>, RiegeliError> {
        if self.next_yield >= self.limits.len() {
            return Ok(None);
        }
        let start = if self.next_yield == 0 {
            0
        } else {
            self.limits[self.next_yield - 1]
        };
        let end = self.limits[self.next_yield];
        self.next_yield += 1;
        Ok(Some(self.data[start..end].to_vec()))
    }
}

// ---------------------------------------------------------------------------
// Select-callback resolution (projection-aware callback type resolution)
// ---------------------------------------------------------------------------

/// Resolve a `SelectCallback` node to its concrete callback type by walking
/// the submessage stack against the include-field map.
///
/// This mirrors C++ `TransposeDecoder::SetCallbackType`. For
/// `StartOfSubmessage` nodes, resolves to `SubmessageStart` or
/// `SkippedSubmessageStart` based on the current `skipped_submessage_level`.
/// For proto field nodes, walks the submessage stack to classify the field
/// as included, excluded, or existence-only, then maps that to the concrete
/// callback type.
fn resolve_select_callback(
    node: &StateMachineNode,
    node_templates: &[NodeTemplate],
    include_map: &IncludeFieldMap,
    submessage_stack: &[SubmessageFrame],
    nodes: &[StateMachineNode],
    skipped_submessage_level: i32,
) -> Result<CallbackType, RiegeliError> {
    let tmpl_idx = node.node_template_index.ok_or_else(|| {
        RiegeliError::MalformedData("SelectCallback node missing template index".into())
    })?;
    let tmpl = &node_templates[tmpl_idx];

    // StartOfSubmessage: resolve based on skipped level.
    if tmpl.tag == message_id::START_OF_SUBMESSAGE {
        return if skipped_submessage_level > 0 {
            Ok(CallbackType::SkippedSubmessageStart)
        } else {
            Ok(CallbackType::SubmessageStart)
        };
    }

    // Proto field node: walk submessage stack to determine inclusion.
    let mut field_included = FieldIncluded::No;

    if skipped_submessage_level == 0 {
        // Start with ExistenceOnly assumption and walk the stack to resolve.
        field_included = FieldIncluded::ExistenceOnly;
        let mut current_parent_id = INVALID_POS;

        for frame in submessage_stack.iter() {
            let frame_node = &nodes[frame.node_index];
            // The frame node is a SubmessageEnd node whose tag_data contains
            // the tag bytes for the enclosing length-delimited field.
            if frame_node.tag_data.is_empty() {
                field_included = FieldIncluded::No;
                break;
            }
            let (frame_tag, _) = decode_u32(&frame_node.tag_data)
                .map_err(|e| RiegeliError::MalformedData(format!("decoding frame tag: {e}")))?;
            let frame_field_number = tag_field_number(frame_tag);

            match include_map.get(current_parent_id, frame_field_number) {
                None => {
                    field_included = FieldIncluded::No;
                    break;
                }
                Some(entry) => {
                    if entry.include_type == IncludeType::IncludeFully {
                        field_included = FieldIncluded::Yes;
                        break;
                    }
                    current_parent_id = entry.field_id;
                }
            }
        }

        // Now resolve the field itself (unless already decided).
        if field_included == FieldIncluded::ExistenceOnly {
            let node_field_number = tag_field_number(tmpl.tag);
            match include_map.get(current_parent_id, node_field_number) {
                None => {
                    field_included = FieldIncluded::No;
                }
                Some(entry) => {
                    if entry.include_type == IncludeType::IncludeFully
                        || entry.include_type == IncludeType::IncludeChild
                    {
                        field_included = FieldIncluded::Yes;
                    }
                    // else stays ExistenceOnly
                }
            }
        }
    }

    // Map FieldIncluded to concrete CallbackType.
    callback_for_field_included(field_included, tmpl.tag, tmpl.subtype)
}

/// Map a `FieldIncluded` classification to the concrete `CallbackType` for
/// a proto field node with the given tag and subtype.
fn callback_for_field_included(
    field_included: FieldIncluded,
    tag: u32,
    st: u8,
) -> Result<CallbackType, RiegeliError> {
    let wt = tag_wire_type(tag);
    match field_included {
        FieldIncluded::Yes => {
            // Normal included field — dispatch to the standard callback.
            TransposeChunkDecoder::callback_for_wire_type(wt, st)
        }
        FieldIncluded::No => {
            // Excluded field — produce no output.
            match wt {
                Some(WireType::Varint) | Some(WireType::Fixed32) | Some(WireType::Fixed64) => {
                    Ok(CallbackType::NoOp)
                }
                Some(WireType::LengthDelimited) => match st {
                    s if s == subtype::LENGTH_DELIMITED_STRING => Ok(CallbackType::NoOp),
                    s if s == subtype::LENGTH_DELIMITED_END_OF_SUBMESSAGE => {
                        Ok(CallbackType::SkippedSubmessageEnd)
                    }
                    _ => Err(RiegeliError::MalformedData(format!(
                        "unknown LengthDelimited subtype {st} in excluded field"
                    ))),
                },
                Some(WireType::StartGroup) => Ok(CallbackType::SkippedSubmessageStart),
                Some(WireType::EndGroup) => Ok(CallbackType::SkippedSubmessageEnd),
                None => Err(RiegeliError::MalformedData(
                    "invalid wire type in excluded field".into(),
                )),
            }
        }
        FieldIncluded::ExistenceOnly => {
            // Existence-only: emit tag + zero value, skip data buffer.
            // For submessage end nodes, produce a SkippedSubmessageEnd since
            // existence-only on a submessage means "include the tag but not
            // the contents" — the submessage is effectively skipped.
            match wt {
                Some(WireType::LengthDelimited)
                    if st == subtype::LENGTH_DELIMITED_END_OF_SUBMESSAGE =>
                {
                    Ok(CallbackType::SkippedSubmessageEnd)
                }
                _ => Ok(CallbackType::ExistenceOnly),
            }
        }
    }
}

/// Mutable decode state passed through the reconstruction loop.
struct DecodeState<'a> {
    dest: &'a mut BackwardBuffer,
    buffers: &'a mut [BufferCursor],
    submessage_stack: &'a mut Vec<SubmessageFrame>,
    limits: &'a mut Vec<usize>,
    num_records: u64,
    nonproto_lengths_index: Option<usize>,
    /// Templates for SelectCallback nodes (empty when no projection).
    node_templates: &'a [NodeTemplate],
    /// Include-field map for projection resolution (None when no projection).
    include_map: Option<&'a IncludeFieldMap>,
    /// Number of nested skipped submessage levels (projection only).
    skipped_submessage_level: i32,
}

/// Execute one node action, writing field data to the backward buffer.
///
/// Handles all `CallbackType` variants: structural markers (NoOp, MessageStart,
/// SubmessageStart/End), data fields (Varint, Fixed32, Fixed64, StringField,
/// CopyTag), non-proto records, and projection-aware variants (SelectCallback,
/// SkippedSubmessageStart, SkippedSubmessageEnd).
///
/// When projection-during-decode is active (include_map is Some), SelectCallback
/// nodes are resolved via `resolve_select_callback` to determine the concrete
/// callback. Excluded fields resolve to NoOp (with buffer data skipped),
/// excluded submessages use SkippedSubmessageEnd/Start to track nesting depth
/// without producing output.
fn execute_node_action(
    node: &StateMachineNode,
    current_node_idx: usize,
    nodes: &[StateMachineNode],
    state: &mut DecodeState<'_>,
) -> Result<(), RiegeliError> {
    // For SelectCallback, resolve to the concrete callback type using the
    // projection's include-field map and current submessage stack state.
    let effective_callback = if node.callback_type == CallbackType::SelectCallback {
        if let Some(include_map) = state.include_map {
            resolve_select_callback(
                node,
                state.node_templates,
                include_map,
                state.submessage_stack,
                nodes,
                state.skipped_submessage_level,
            )?
        } else {
            // No projection active but SelectCallback present — should not
            // happen, but resolve to included as a safe fallback.
            let tmpl_idx = node.node_template_index.ok_or_else(|| {
                RiegeliError::MalformedData("SelectCallback missing template".into())
            })?;
            let tmpl = &state.node_templates[tmpl_idx];
            if tmpl.tag == message_id::START_OF_SUBMESSAGE {
                CallbackType::SubmessageStart
            } else {
                TransposeChunkDecoder::callback_for_wire_type(
                    tag_wire_type(tmpl.tag),
                    tmpl.subtype,
                )?
            }
        }
    } else {
        node.callback_type
    };

    match effective_callback {
        CallbackType::NoOp => {
            // When a field is excluded by projection, it resolves to NoOp.
            // If the node has a data buffer, we must consume (skip) the buffer
            // bytes to keep the buffer cursor in sync, without writing output.
            if let Some(buf_idx) = node.buffer_index {
                skip_field_data(node, state.buffers, buf_idx, state.node_templates)?;
            }
        }
        CallbackType::MessageStart => {
            if !state.submessage_stack.is_empty() {
                return Err(RiegeliError::MalformedData(
                    "submessages still open at record boundary".into(),
                ));
            }
            if state.skipped_submessage_level != 0 {
                return Err(RiegeliError::MalformedData(format!(
                    "skipped_submessage_level is {} at record boundary, expected 0",
                    state.skipped_submessage_level
                )));
            }
            if state.limits.len() as u64 == state.num_records {
                return Err(RiegeliError::MalformedData("too many records".into()));
            }
            state.limits.push(state.dest.pos());
        }
        CallbackType::SubmessageEnd => {
            state.submessage_stack.push(SubmessageFrame {
                end_pos: state.dest.pos(),
                node_index: current_node_idx,
            });
        }
        CallbackType::SubmessageStart => {
            write_submessage_header(state.dest, nodes, state.submessage_stack)?;
        }
        CallbackType::NonProto => {
            write_nonproto_record(node, state)?;
        }
        CallbackType::CopyTag => {
            state.dest.write(&node.tag_data[..node.tag_data_size]);
        }
        CallbackType::Varint { data_length } => {
            write_varint_field(node, state.dest, state.buffers, data_length)?;
        }
        CallbackType::Fixed32 => {
            write_fixed_field(node, state.dest, state.buffers, 4)?;
        }
        CallbackType::Fixed64 => {
            write_fixed_field(node, state.dest, state.buffers, 8)?;
        }
        CallbackType::StringField => {
            write_string_field(node, state.dest, state.buffers)?;
        }
        CallbackType::ExistenceOnly => {
            // Write tag + zero value, skip data buffer bytes.
            write_existence_only_field(node, state.dest, state.buffers, state.node_templates)?;
        }
        CallbackType::Failure => {
            return Err(RiegeliError::MalformedData(
                "hit failure node in state machine".into(),
            ));
        }
        // Projection-aware: excluded submessage end — increment skip level,
        // no stack push, no output bytes.
        CallbackType::SkippedSubmessageEnd => {
            state.skipped_submessage_level += 1;
        }
        // Projection-aware: excluded submessage start — decrement skip level,
        // no stack pop, no output bytes.
        CallbackType::SkippedSubmessageStart => {
            if state.skipped_submessage_level <= 0 {
                return Err(RiegeliError::MalformedData(
                    "skipped submessage stack underflow".into(),
                ));
            }
            state.skipped_submessage_level -= 1;
        }
        // SelectCallback should have been resolved above.
        CallbackType::SelectCallback => {
            return Err(RiegeliError::MalformedData(
                "unresolved SelectCallback in decode loop".into(),
            ));
        }
    }
    Ok(())
}

/// Skip (consume) the data buffer bytes for an excluded field without writing
/// any output. This keeps the buffer cursor in sync when a field is excluded
/// by projection.
///
/// The skip logic must match what the corresponding write function would read:
/// - Varint: skip `data_length` bytes (from tag_data_size - tag_length, or from template)
/// - Fixed32: skip 4 bytes
/// - Fixed64: skip 8 bytes
/// - StringField: read varint32 length, skip that many bytes
/// - CopyTag (inline varint): no data buffer bytes to skip
///
/// Uses the node's `NodeTemplate` to determine the original wire type and
/// subtype, then skips the appropriate number of bytes:
/// - Varint (non-inline): skip `subtype + 1` bytes
/// - Inline varint: no buffer data to skip (data is in tag_data)
/// - Fixed32: skip 4 bytes
/// - Fixed64: skip 8 bytes
/// - StringField: read varint32 length, skip that many bytes
fn skip_field_data(
    node: &StateMachineNode,
    buffers: &mut [BufferCursor],
    buf_idx: usize,
    node_templates: &[NodeTemplate],
) -> Result<(), RiegeliError> {
    // Use the node template for tag and subtype when available.
    let (tag, st) = if let Some(tmpl_idx) = node.node_template_index {
        let tmpl = &node_templates[tmpl_idx];
        (tmpl.tag, tmpl.subtype)
    } else if !node.tag_data.is_empty() {
        let (tag, _) = decode_u32(&node.tag_data)
            .map_err(|e| RiegeliError::MalformedData(format!("decoding tag for skip: {e}")))?;
        (tag, 0u8)
    } else {
        return Ok(());
    };

    let wt = tag_wire_type(tag);
    match wt {
        Some(WireType::Varint) => {
            if st >= subtype::VARINT_INLINE_0 {
                // Inline varint — data is stored in tag_data, no buffer read.
                return Ok(());
            }
            // Non-inline varint: data_length = subtype + 1.
            let data_length = (st + 1) as usize;
            buffers[buf_idx].read_exact(data_length)?;
        }
        Some(WireType::Fixed32) => {
            buffers[buf_idx].read_exact(4)?;
        }
        Some(WireType::Fixed64) => {
            buffers[buf_idx].read_exact(8)?;
        }
        Some(WireType::LengthDelimited) => {
            // String field: read length varint, skip that many data bytes.
            let str_len = buffers[buf_idx].read_varint32()? as usize;
            buffers[buf_idx].read_exact(str_len)?;
        }
        _ => {
            // StartGroup/EndGroup or unknown — no data to skip.
        }
    }
    Ok(())
}

/// Pop a submessage frame and write the enclosing tag + length-varint header
/// to the backward buffer.
fn write_submessage_header(
    dest: &mut BackwardBuffer,
    nodes: &[StateMachineNode],
    submessage_stack: &mut Vec<SubmessageFrame>,
) -> Result<(), RiegeliError> {
    let frame = submessage_stack
        .pop()
        .ok_or_else(|| RiegeliError::MalformedData("submessage stack underflow".into()))?;
    if dest.pos() < frame.end_pos {
        return Err(RiegeliError::MalformedData(
            "destination position decreased".into(),
        ));
    }
    let length = dest.pos() - frame.end_pos;
    if length > u32::MAX as usize {
        return Err(RiegeliError::MalformedData("submessage too large".into()));
    }
    let submsg_node = &nodes[frame.node_index];
    let tag_bytes = &submsg_node.tag_data[..submsg_node.tag_data_size];
    let len_varint = encode_u32(length as u32);
    let mut hdr = Vec::with_capacity(tag_bytes.len() + len_varint.len());
    hdr.extend_from_slice(tag_bytes);
    hdr.extend_from_slice(&len_varint);
    dest.write(&hdr);
    Ok(())
}

/// Read a non-proto record from the data buffer and write it to the backward
/// buffer, then push a record-boundary limit.
fn write_nonproto_record(
    node: &StateMachineNode,
    state: &mut DecodeState<'_>,
) -> Result<(), RiegeliError> {
    let lengths_idx = state
        .nonproto_lengths_index
        .ok_or_else(|| RiegeliError::MalformedData("nonproto op but no lengths buffer".into()))?;
    let length = state.buffers[lengths_idx].read_varint32()? as usize;
    let data_idx = node
        .buffer_index
        .ok_or_else(|| RiegeliError::MalformedData("nonproto node missing buffer index".into()))?;
    let data_bytes = state.buffers[data_idx].read_exact(length)?.to_vec();
    state.dest.write(&data_bytes);
    if !state.submessage_stack.is_empty() {
        return Err(RiegeliError::MalformedData(
            "submessages still open at nonproto record".into(),
        ));
    }
    if state.limits.len() as u64 == state.num_records {
        return Err(RiegeliError::MalformedData("too many records".into()));
    }
    state.limits.push(state.dest.pos());
    Ok(())
}

/// Read varint bytes from a data buffer, restore high bits (reversing the
/// encoder's stripping), and prepend the tag + restored varint to the output.
fn write_varint_field(
    node: &StateMachineNode,
    dest: &mut BackwardBuffer,
    buffers: &mut [BufferCursor],
    data_length: u8,
) -> Result<(), RiegeliError> {
    let buf_idx = node
        .buffer_index
        .ok_or_else(|| RiegeliError::MalformedData("varint node missing buffer index".into()))?;
    let raw = buffers[buf_idx].read_exact(data_length as usize)?.to_vec();
    let tag_size = node.tag_data_size;
    let mut combined = Vec::with_capacity(tag_size + raw.len());
    combined.extend_from_slice(&node.tag_data[..tag_size]);
    for (j, &b) in raw.iter().enumerate() {
        combined.push(if j < raw.len() - 1 { b | 0x80 } else { b });
    }
    dest.write(&combined);
    Ok(())
}

/// Read `n` fixed-width bytes from a data buffer and prepend tag + data to
/// the backward buffer. Used for both Fixed32 (n=4) and Fixed64 (n=8).
fn write_fixed_field(
    node: &StateMachineNode,
    dest: &mut BackwardBuffer,
    buffers: &mut [BufferCursor],
    n: usize,
) -> Result<(), RiegeliError> {
    let buf_idx = node.buffer_index.ok_or_else(|| {
        RiegeliError::MalformedData("fixed field node missing buffer index".into())
    })?;
    let data = buffers[buf_idx].read_exact(n)?.to_vec();
    let tag_size = node.tag_data_size;
    let mut combined = Vec::with_capacity(tag_size + n);
    combined.extend_from_slice(&node.tag_data[..tag_size]);
    combined.extend_from_slice(&data);
    dest.write(&combined);
    Ok(())
}

/// Read a length-delimited string/bytes field from a data buffer and prepend
/// tag + length-varint + payload to the backward buffer.
fn write_string_field(
    node: &StateMachineNode,
    dest: &mut BackwardBuffer,
    buffers: &mut [BufferCursor],
) -> Result<(), RiegeliError> {
    let buf_idx = node
        .buffer_index
        .ok_or_else(|| RiegeliError::MalformedData("string node missing buffer index".into()))?;
    let str_len = buffers[buf_idx].read_varint32()? as usize;
    let str_data = buffers[buf_idx].read_exact(str_len)?.to_vec();
    let len_varint = encode_u32(str_len as u32);
    let tag_size = node.tag_data_size;
    let mut combined = Vec::with_capacity(tag_size + len_varint.len() + str_data.len());
    combined.extend_from_slice(&node.tag_data[..tag_size]);
    combined.extend_from_slice(&len_varint);
    combined.extend_from_slice(&str_data);
    dest.write(&combined);
    Ok(())
}

/// Write an existence-only field: tag + zero value to the backward buffer,
/// then skip (consume) the data buffer bytes without using them.
///
/// The zero value depends on the wire type:
/// - Varint: single 0x00 byte
/// - Fixed32: 4 zero bytes
/// - Fixed64: 8 zero bytes
/// - LengthDelimited (string): 0x00 (zero-length)
fn write_existence_only_field(
    node: &StateMachineNode,
    dest: &mut BackwardBuffer,
    buffers: &mut [BufferCursor],
    node_templates: &[NodeTemplate],
) -> Result<(), RiegeliError> {
    // Get the tag and subtype from the template.
    let (tag, _st) = if let Some(tmpl_idx) = node.node_template_index {
        let tmpl = &node_templates[tmpl_idx];
        (tmpl.tag, tmpl.subtype)
    } else if !node.tag_data.is_empty() {
        let (tag, _) = decode_u32(&node.tag_data)
            .map_err(|e| RiegeliError::MalformedData(format!("decoding tag: {e}")))?;
        (tag, 0u8)
    } else {
        return Ok(());
    };

    let tag_bytes = &node.tag_data[..node.tag_data_size.min(node.tag_data.len())];
    // For inline varints, tag_data already includes the value byte.
    // We need just the tag portion.
    let tmpl_tag_length = if let Some(tmpl_idx) = node.node_template_index {
        node_templates[tmpl_idx].tag_length
    } else {
        encode_u32(tag).len()
    };

    let wt = tag_wire_type(tag);
    let zero_value: &[u8] = match wt {
        Some(WireType::Varint) => &[0x00],
        Some(WireType::Fixed32) => &[0x00, 0x00, 0x00, 0x00],
        Some(WireType::Fixed64) => &[0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00],
        Some(WireType::LengthDelimited) => &[0x00], // zero-length
        _ => &[],
    };

    // Write tag + zero value using a stack buffer (max tag = 5 bytes varint +
    // max zero = 8 bytes for fixed64 = 13 bytes total).
    let tag_only = &tag_bytes[..tmpl_tag_length.min(tag_bytes.len())];
    let total = tag_only.len() + zero_value.len();
    let mut buf = [0u8; 13];
    buf[..tag_only.len()].copy_from_slice(tag_only);
    buf[tag_only.len()..total].copy_from_slice(zero_value);
    dest.write(&buf[..total]);

    // Skip the data buffer bytes.
    if let Some(buf_idx) = node.buffer_index {
        skip_field_data(node, buffers, buf_idx, node_templates)?;
    }
    Ok(())
}

/// Advance the state machine by one step: follow the current node's
/// `next_node_index`, then either consume a transition byte or decrement the
/// implicit-repeat counter.
///
/// Returns `(new_node_idx, new_num_iters, should_break)`.
fn advance_state_machine(
    current_node_idx: usize,
    num_iters: i32,
    nodes: &[StateMachineNode],
    transitions: &mut BufferCursor,
) -> Result<(usize, i32, bool), RiegeliError> {
    let mut idx = nodes[current_node_idx].next_node_index;
    if num_iters == 0 {
        if transitions.is_empty() {
            return Ok((idx, 0, true));
        }
        let tb = transitions.read_byte()?;
        let offset = (tb >> 2) as usize;
        let repeat = (tb & 3) as i32;
        idx += offset;
        if idx >= nodes.len() {
            return Err(RiegeliError::MalformedData(
                "transition offset overflow".into(),
            ));
        }
        let iters = repeat + if nodes[idx].is_implicit { 1 } else { 0 };
        Ok((idx, iters, false))
    } else {
        let iters = num_iters - if nodes[idx].is_implicit { 0 } else { 1 };
        Ok((idx, iters, false))
    }
}

/// Drive the state machine to reconstruct all records using the
/// backward-writing pattern.
///
/// Each transition byte encodes `(offset << 2) | repeat_count` where `offset`
/// is the relative state index jump and `repeat_count` (0..3) is the number of
/// additional zero-offset transitions to perform.
fn run_state_machine(
    num_records: u64,
    decoded_data_size: u64,
    nodes: &[StateMachineNode],
    buffers: &mut [BufferCursor],
    transitions: &mut BufferCursor,
    first_node: usize,
    nonproto_lengths_index: Option<usize>,
    node_templates: &[NodeTemplate],
    include_map: Option<&IncludeFieldMap>,
) -> Result<(BackwardBuffer, Vec<usize>), RiegeliError> {
    let mut dest = BackwardBuffer::new(decoded_data_size as usize);
    let mut limits: Vec<usize> = Vec::with_capacity(num_records as usize);
    let mut submessage_stack: Vec<SubmessageFrame> = Vec::new();
    let mut current_node_idx = first_node;
    let mut num_iters: i32 = if nodes[current_node_idx].is_implicit {
        1
    } else {
        0
    };

    {
        let mut state = DecodeState {
            dest: &mut dest,
            buffers,
            submessage_stack: &mut submessage_stack,
            limits: &mut limits,
            num_records,
            nonproto_lengths_index,
            node_templates,
            include_map,
            skipped_submessage_level: 0,
        };
        loop {
            execute_node_action(
                &nodes[current_node_idx],
                current_node_idx,
                nodes,
                &mut state,
            )?;
            let (next, iters, done) =
                advance_state_machine(current_node_idx, num_iters, nodes, transitions)?;
            if done {
                break;
            }
            current_node_idx = next;
            num_iters = iters;
        }
    }

    if !submessage_stack.is_empty() {
        return Err(RiegeliError::MalformedData(
            "submessages still open after decode".into(),
        ));
    }
    if limits.len() as u64 != num_records {
        return Err(RiegeliError::MalformedData(format!(
            "expected {} records, got {}",
            num_records,
            limits.len()
        )));
    }
    Ok((dest, limits))
}

/// Convert backward-written data and record-boundary limits into a contiguous
/// forward-order buffer and sorted record-end positions.
///
/// Limits recorded during backward writing are in reverse order. This function
/// reverses and complements them to produce correct forward byte boundaries,
/// matching the C++ `TransposeDecoder::DecodingState::Finish()` algorithm.
///
/// Returns `(data, limits)` where `data` is the concatenated record bytes and
/// `limits[i]` is the exclusive end position of record `i` in `data`.
fn finalize_records(
    dest: BackwardBuffer,
    mut limits: Vec<usize>,
    num_records: u64,
) -> Result<(Vec<u8>, Vec<usize>), RiegeliError> {
    let total_size = dest.pos();
    let n = limits.len();
    if let Some(&last_limit) = limits.last()
        && last_limit != total_size
    {
        return Err(RiegeliError::MalformedData(format!(
            "last limit {} != total size {}",
            last_limit, total_size
        )));
    }
    // Reverse and complement limits (C++ algorithm).
    {
        let size = total_size;
        let (mut first, mut last) = (0usize, n);
        if first != last {
            last -= 1;
            while first < last {
                last -= 1;
                let tmp = size - limits[first];
                limits[first] = size - limits[last];
                limits[last] = tmp;
                first += 1;
            }
        }
    }
    // Validate boundaries.
    let mut prev = 0usize;
    for &end in &limits {
        if end > total_size || prev > end {
            return Err(RiegeliError::MalformedData(
                "record boundary out of range".into(),
            ));
        }
        prev = end;
    }
    let _ = num_records;
    Ok((dest.into_forward(), limits))
}

/// Top-level decode: run the state machine then finalize into contiguous storage.
fn decode_all_records(
    num_records: u64,
    decoded_data_size: u64,
    nodes: &[StateMachineNode],
    buffers: &mut [BufferCursor],
    transitions: &mut BufferCursor,
    first_node: usize,
    nonproto_lengths_index: Option<usize>,
    node_templates: &[NodeTemplate],
    include_map: Option<&IncludeFieldMap>,
) -> Result<(Vec<u8>, Vec<usize>), RiegeliError> {
    let (dest, limits) = run_state_machine(
        num_records,
        decoded_data_size,
        nodes,
        buffers,
        transitions,
        first_node,
        nonproto_lengths_index,
        node_templates,
        include_map,
    )?;
    finalize_records(dest, limits, num_records)
}

/// Detect implicit loops in the state machine.
///
/// An implicit loop is a cycle of nodes all connected by implicit transitions.
/// Such a cycle would cause the decoder to loop forever without consuming any
/// transition bytes.
fn contains_implicit_loop(nodes: &[StateMachineNode], _num_states: usize) -> bool {
    let total = nodes.len();
    let mut loop_ids = vec![0usize; total];
    let mut next_id: usize = 1;
    for i in 0..total {
        if loop_ids[i] != 0 {
            continue;
        }
        let mut idx = i;
        loop_ids[idx] = next_id;
        while nodes[idx].is_implicit {
            idx = nodes[idx].next_node_index;
            if idx >= total {
                break;
            }
            if loop_ids[idx] == next_id {
                return true;
            }
            if loop_ids[idx] != 0 {
                break;
            }
            loop_ids[idx] = next_id;
        }
        next_id += 1;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chunk_header::{ChunkHeader, ChunkType};
    use crate::field_projection::Field;
    use crate::proto::make_tag;
    use crate::varint::encode_u64;

    /// Helper to build a transpose chunk from hand-crafted state machine.
    fn build_transpose_chunk(
        compression: CompressionType,
        num_records: u64,
        decoded_data_size: u64,
        states: &[TestState],
        buffers_data: &[Vec<u8>],
        transitions: &[u8],
        first_node: u32,
    ) -> Chunk {
        let mut header_bytes: Vec<u8> = Vec::new();

        let num_buckets: u32 = if buffers_data.is_empty() { 0 } else { 1 };
        let num_buffers = buffers_data.len() as u32;
        header_bytes.extend_from_slice(&encode_u32(num_buckets));
        header_bytes.extend_from_slice(&encode_u32(num_buffers));

        // Bucket compressed size = total buffer sizes.
        let total_buf_size: usize = buffers_data.iter().map(|b| b.len()).sum();
        if num_buckets > 0 {
            header_bytes.extend_from_slice(&encode_u64(total_buf_size as u64));
        }

        for buf in buffers_data {
            header_bytes.extend_from_slice(&encode_u64(buf.len() as u64));
        }

        let num_states = states.len() as u32;
        header_bytes.extend_from_slice(&encode_u32(num_states));

        for state in states {
            header_bytes.extend_from_slice(&encode_u32(state.tag));
        }

        for state in states {
            header_bytes.extend_from_slice(&encode_u32(state.next_node));
        }

        // Subtypes.
        for state in states {
            let tag = state.tag;
            if is_valid_proto_tag(tag) && has_subtype(tag) {
                header_bytes.push(state.subtype);
            }
        }

        // Buffer indices.
        for state in states {
            let mut tag = state.tag;
            let mut st = state.subtype;

            if tag < 8 {
                if tag == message_id::NON_PROTO {
                    header_bytes.extend_from_slice(&encode_u32(state.buffer_index));
                }
                continue;
            }

            let wire_raw = tag & 7;
            if wire_raw == SUBMESSAGE_WIRE_TYPE {
                tag = tag - SUBMESSAGE_WIRE_TYPE + WireType::LengthDelimited as u32;
                st = subtype::LENGTH_DELIMITED_END_OF_SUBMESSAGE;
            }

            if has_data_buffer(tag, st) {
                header_bytes.extend_from_slice(&encode_u32(state.buffer_index));
            }
        }

        header_bytes.extend_from_slice(&encode_u32(first_node));

        let mut chunk_data: Vec<u8> = Vec::new();
        chunk_data.push(compression as u8);
        chunk_data.extend_from_slice(&encode_u64(header_bytes.len() as u64));
        chunk_data.extend_from_slice(&header_bytes);
        for buf in buffers_data {
            chunk_data.extend_from_slice(buf);
        }
        chunk_data.extend_from_slice(transitions);

        let chunk_header = ChunkHeader::from_parts(
            &chunk_data,
            ChunkType::Transposed,
            num_records,
            decoded_data_size,
        );

        Chunk {
            header: chunk_header,
            data: chunk_data,
        }
    }

    struct TestState {
        tag: u32,
        next_node: u32,
        subtype: u8,
        buffer_index: u32,
    }

    impl TestState {
        fn new(tag: u32, next_node: u32, subtype: u8, buffer_index: u32) -> Self {
            Self {
                tag,
                next_node,
                subtype,
                buffer_index,
            }
        }
    }

    // -------------------------------------------------------------------
    // 9.1: Zero-record transpose chunk returns Ok(None)
    // -------------------------------------------------------------------
    #[test]
    fn test_zero_records() {
        // Build a minimal zero-record chunk.
        let mut header_bytes: Vec<u8> = Vec::new();
        header_bytes.extend_from_slice(&encode_u32(0)); // num_buckets
        header_bytes.extend_from_slice(&encode_u32(0)); // num_buffers
        header_bytes.extend_from_slice(&encode_u32(0)); // num_states
        header_bytes.extend_from_slice(&encode_u32(0)); // first_node

        let mut chunk_data: Vec<u8> = Vec::new();
        chunk_data.push(0x00); // CompressionType::None
        chunk_data.extend_from_slice(&encode_u64(header_bytes.len() as u64));
        chunk_data.extend_from_slice(&header_bytes);

        let chunk_header = ChunkHeader::from_parts(&chunk_data, ChunkType::Transposed, 0, 0);
        let chunk = Chunk {
            header: chunk_header,
            data: chunk_data,
        };

        let mut dec = TransposeChunkDecoder::new(chunk).expect("new ok");
        assert!(dec.read_record().unwrap().is_none());
    }

    // -------------------------------------------------------------------
    // 9.2: Single NonProto record round-trips exactly
    // -------------------------------------------------------------------
    #[test]
    fn test_single_nonproto_record() {
        let nonproto_data = b"hello".to_vec();
        let mut nonproto_lengths = Vec::new();
        nonproto_lengths.extend_from_slice(&encode_u32(5));

        // State 0: NonProto, next_node=0 (explicit), buffer_index=0
        let states = vec![TestState::new(message_id::NON_PROTO, 0, 0, 0)];

        let chunk = build_transpose_chunk(
            CompressionType::None,
            1,
            5,
            &states,
            &[nonproto_data, nonproto_lengths],
            &[],
            0,
        );

        let mut dec = TransposeChunkDecoder::new(chunk).expect("new ok");
        let rec = dec.read_record().unwrap().expect("should have record");
        assert_eq!(rec, b"hello");
        assert!(dec.read_record().unwrap().is_none());
    }

    // -------------------------------------------------------------------
    // 9.3: Single proto record with varint/fixed32/fixed64/string/nested submessage
    // -------------------------------------------------------------------
    #[test]
    fn test_single_proto_record() {
        // Expected record:
        // field 1 (varint) = 42:    08 2A
        // field 2 (fixed32):        15 04030201
        // field 3 (fixed64):        19 0807060504030201
        // field 4 (string "abc"):   22 03 616263
        // field 5 (submessage):     2A 02 08 07
        //   (inner field 1 varint = 7)
        let expected: Vec<u8> = vec![
            0x08, 0x2A, 0x15, 0x04, 0x03, 0x02, 0x01, 0x19, 0x08, 0x07, 0x06, 0x05, 0x04, 0x03,
            0x02, 0x01, 0x22, 0x03, 0x61, 0x62, 0x63, 0x2A, 0x02, 0x08, 0x07,
        ];

        // Buffers:
        // 0: varint data for outer field 1 (42) -> [0x2A] (high bit already clear for 1-byte)
        // 1: fixed32 data for field 2 -> [04 03 02 01]
        // 2: fixed64 data for field 3 -> [08 07 06 05 04 03 02 01]
        // 3: string data for field 4 -> [03 61 62 63] (length + data)
        // 4: varint data for inner field 1 (7) -> [0x07]
        let buf0 = vec![0x2A]; // varint 42
        let buf1 = vec![0x04, 0x03, 0x02, 0x01]; // fixed32
        let buf2 = vec![0x08, 0x07, 0x06, 0x05, 0x04, 0x03, 0x02, 0x01]; // fixed64
        let buf3 = vec![0x03, 0x61, 0x62, 0x63]; // string "abc" with length prefix
        let buf4 = vec![0x07]; // inner varint 7

        let num_states = 8u32;
        let states = vec![
            // State 0: SubmessageEnd (field 5, wire type 6)
            TestState::new(0x2E, num_states + 1, 0, 0),
            // State 1: Varint (inner field 1, 1-byte)
            TestState::new(0x08, num_states + 2, subtype::VARINT_1, 4),
            // State 2: SubmessageStart
            TestState::new(message_id::START_OF_SUBMESSAGE, num_states + 3, 0, 0),
            // State 3: String (field 4)
            TestState::new(0x22, num_states + 4, 0, 3),
            // State 4: Fixed64 (field 3)
            TestState::new(0x19, num_states + 5, 0, 2),
            // State 5: Fixed32 (field 2)
            TestState::new(0x15, num_states + 6, 0, 1),
            // State 6: Varint (outer field 1, 1-byte)
            TestState::new(0x08, num_states + 7, subtype::VARINT_1, 0),
            // State 7: MessageStart
            TestState::new(message_id::START_OF_MESSAGE, 0, 0, 0),
        ];

        let chunk = build_transpose_chunk(
            CompressionType::None,
            1,
            expected.len() as u64,
            &states,
            &[buf0, buf1, buf2, buf3, buf4],
            &[], // no transitions (all implicit)
            0,   // first_node
        );

        let mut dec = TransposeChunkDecoder::new(chunk).expect("new ok");
        let rec = dec.read_record().unwrap().expect("should have record");
        assert_eq!(rec, expected, "proto record mismatch");
        assert!(dec.read_record().unwrap().is_none());
    }

    // -------------------------------------------------------------------
    // 9.6: Mixed proto + nonproto records
    // -------------------------------------------------------------------
    #[test]
    fn test_mixed_proto_nonproto() {
        // Record 0: proto with field 1 varint = 1: [08 01]
        // Record 1: nonproto "xyz" (3 bytes)
        //
        // Expected output: record 0 = [08 01], record 1 = b"xyz"

        // Buffers:
        // 0: varint data for field 1 value 1 -> [0x01]
        // 1: nonproto data -> b"xyz"
        // 2: nonproto lengths -> varint(3)
        let buf0 = vec![0x01];
        let buf1 = b"xyz".to_vec();
        let mut buf2 = Vec::new();
        buf2.extend_from_slice(&encode_u32(3));

        // State machine (backward order, record 1 first, then record 0):
        // State 0: NonProto -> implicit to state 1 (buffer_index=1)
        // State 1: Varint(field 1, 1 byte) -> implicit to state 2 (buffer=0)
        // State 2: MessageStart -> next = 0
        let num_states = 3u32;
        let states = vec![
            // State 0: NonProto
            TestState::new(message_id::NON_PROTO, num_states + 1, 0, 1),
            // State 1: Varint field 1, 1-byte
            TestState::new(0x08, num_states + 2, subtype::VARINT_1, 0),
            // State 2: MessageStart
            TestState::new(message_id::START_OF_MESSAGE, 0, 0, 0),
        ];

        let chunk = build_transpose_chunk(
            CompressionType::None,
            2,
            5, // 2 (proto) + 3 (nonproto)
            &states,
            &[buf0, buf1, buf2],
            &[], // all implicit
            0,
        );

        let mut dec = TransposeChunkDecoder::new(chunk).expect("new ok");
        let rec0 = dec.read_record().unwrap().expect("should have record 0");
        let rec1 = dec.read_record().unwrap().expect("should have record 1");
        assert!(dec.read_record().unwrap().is_none());

        // Record 0 is the PROTO record (processed second in backward order).
        // Record 1 is the NONPROTO record (processed first in backward order).
        // Wait, the backward writer processes the LAST record first.
        // With 2 records, the state machine processes record 1 first, then record 0.
        // But our extraction reverses the order, so:
        //   records[0] = record 0 (proto) = [08 01]
        //   records[1] = record 1 (nonproto) = "xyz"
        assert_eq!(rec0, vec![0x08, 0x01], "record 0 should be proto");
        assert_eq!(rec1, b"xyz", "record 1 should be nonproto");
    }

    // -------------------------------------------------------------------
    // 9.3b: Multi-byte varint with high-bit restoration
    // -------------------------------------------------------------------
    #[test]
    fn test_varint_high_bit_restoration() {
        // Varint value 300 = 0xAC 0x02 (2-byte varint).
        // The encoder strips high bits: stores [0x2C, 0x02].
        // The decoder must restore: [0x2C | 0x80, 0x02] = [0xAC, 0x02].
        //
        // Proto record: field 1 varint = 300 -> tag=0x08, value=0xAC 0x02
        // Expected: [0x08, 0xAC, 0x02]
        let expected: Vec<u8> = vec![0x08, 0xAC, 0x02];

        // Buffer: varint data with stripped high bits -> [0x2C, 0x02]
        let buf0 = vec![0x2C, 0x02];

        let num_states = 2u32;
        let states = vec![
            // State 0: Varint (field 1, 2-byte)
            TestState::new(0x08, num_states + 1, subtype::VARINT_1 + 1, 0),
            // State 1: MessageStart
            TestState::new(message_id::START_OF_MESSAGE, 0, 0, 0),
        ];

        let chunk = build_transpose_chunk(
            CompressionType::None,
            1,
            expected.len() as u64,
            &states,
            &[buf0],
            &[],
            0,
        );

        let mut dec = TransposeChunkDecoder::new(chunk).expect("new ok");
        let rec = dec.read_record().unwrap().expect("record");
        assert_eq!(
            rec, expected,
            "multi-byte varint high-bit restoration failed"
        );
    }

    // -------------------------------------------------------------------
    // 9.3c: Inline varint (value stored in subtype)
    // -------------------------------------------------------------------
    #[test]
    fn test_inline_varint() {
        // Proto record: field 1 varint = 0 -> tag=0x08, value=0x00
        // Inline varint with subtype = VARINT_INLINE_0 (10).
        // The tag_data should be [0x08, 0x00] and CopyTag writes both.
        let expected: Vec<u8> = vec![0x08, 0x00];

        // No data buffer needed (inline).
        let num_states = 2u32;
        let states = vec![
            TestState::new(0x08, num_states + 1, subtype::VARINT_INLINE_0, 0),
            TestState::new(message_id::START_OF_MESSAGE, 0, 0, 0),
        ];

        let chunk = build_transpose_chunk(
            CompressionType::None,
            1,
            expected.len() as u64,
            &states,
            &[], // no buffers needed
            &[],
            0,
        );

        let mut dec = TransposeChunkDecoder::new(chunk).expect("new ok");
        let rec = dec.read_record().unwrap().expect("record");
        assert_eq!(rec, expected, "inline varint mismatch");
    }

    // -------------------------------------------------------------------
    // 9.7: Corrupted bucket returns Err, not panic
    // -------------------------------------------------------------------
    #[test]
    fn test_corrupted_bucket() {
        let nonproto_data = b"hello".to_vec();
        let mut nonproto_lengths = Vec::new();
        nonproto_lengths.extend_from_slice(&encode_u32(5));

        let states = vec![TestState::new(message_id::NON_PROTO, 0, 0, 0)];

        let mut chunk = build_transpose_chunk(
            CompressionType::None,
            1,
            5,
            &states,
            &[nonproto_data, nonproto_lengths],
            &[],
            0,
        );

        // Corrupt bytes in the bucket data area.
        let data_len = chunk.data.len();
        if data_len > 20 {
            chunk.data[data_len - 3] ^= 0xFF;
            chunk.data[data_len - 2] ^= 0xFF;
        }

        // Rebuild header to pass hash check (we want to test bucket parsing).
        chunk.header = ChunkHeader::from_parts(&chunk.data, ChunkType::Transposed, 1, 5);

        // Should return Err or produce wrong data, but not panic.
        let result = TransposeChunkDecoder::new(chunk);
        match result {
            Ok(mut dec) => {
                // Might produce wrong data or error during read.
                let _ = dec.read_record();
            }
            Err(e) => {
                // Should be MalformedData variant.
                match e {
                    RiegeliError::MalformedData(_) => {}
                    other => panic!("expected MalformedData, got {other:?}"),
                }
            }
        }
    }

    // -------------------------------------------------------------------
    // Test with explicit transitions (multiple records, same schema)
    // -------------------------------------------------------------------
    #[test]
    fn test_multiple_records_with_transitions() {
        // 3 proto records, each with field 1 varint:
        //   Record 0: [08 05] (field 1 = 5)
        //   Record 1: [08 0A] (field 1 = 10)
        //   Record 2: [08 2A] (field 1 = 42)
        //
        // All varint data stored in one buffer: [05 0A 2A] (reversed for backward writing)
        // Wait, the encoder writes data in reverse record order.
        // So the buffer has: record 2's varint first, then record 1's, then record 0's.
        // Buffer = [2A, 0A, 05]

        let expected_records: Vec<Vec<u8>> =
            vec![vec![0x08, 0x05], vec![0x08, 0x0A], vec![0x08, 0x2A]];

        let buf0 = vec![0x2A, 0x0A, 0x05]; // reversed order

        // State machine:
        //   State 0: Varint(field 1, 1-byte), next -> implicit to state 1
        //   State 1: MessageStart, next -> 0 (explicit)
        // Transition needed: after state 1, go back to state 0.
        // Transition offset = 0 (node->next_node = 0, add offset 0).
        // For 3 records, we need 2 explicit transitions back to state 0.
        // Transition byte: high 6 bits = offset from base, low 2 bits = repeat-1.
        // If base is state 0, offset 0, repeat for 2 more iterations:
        //   repeat count = 1 (meaning we go back 2 more times: 1 byte with repeat=1 -> 2 transitions)
        //   Actually, low 2 bits = number of ADDITIONAL iterations (0-3).
        //   We need transitions for 3 records. After the first record (done via initial num_iters),
        //   we need 2 more transitions. One transition byte with repeat=1: (0 << 2) | 1 = 0x01.
        //   That gives repeat=1 -> 2 transitions (the byte itself + 1 repeat).
        //   With the initial implicit iteration, we'd have 1 (initial) + 2 = 3 records.

        let num_states = 2u32;
        let states = vec![
            // State 0: Varint(field 1, 1-byte)
            TestState::new(0x08, num_states + 1, subtype::VARINT_1, 0),
            // State 1: MessageStart
            TestState::new(message_id::START_OF_MESSAGE, 0, 0, 0),
        ];

        // Transitions: 1 byte with offset=0, repeat=1
        // That gives us 2 explicit transitions (plus 1 implicit from start = 3 records).
        let transitions = vec![0x01]; // (0 << 2) | 1

        let total_decoded: u64 = expected_records.iter().map(|r| r.len() as u64).sum();

        let chunk = build_transpose_chunk(
            CompressionType::None,
            3,
            total_decoded,
            &states,
            &[buf0],
            &transitions,
            0,
        );

        let mut dec = TransposeChunkDecoder::new(chunk).expect("new ok");
        for (i, expected) in expected_records.iter().enumerate() {
            let rec = dec
                .read_record()
                .unwrap()
                .unwrap_or_else(|| panic!("expected record {i}"));
            assert_eq!(&rec, expected, "record {i} mismatch");
        }
        assert!(dec.read_record().unwrap().is_none());
    }

    // -------------------------------------------------------------------
    // 9.4/9.5: C++-generated files (skip - no golden files)
    // -------------------------------------------------------------------
    // These would require actual C++-generated files. Marked as skip in progress.json.

    // -------------------------------------------------------------------
    // Sprint 9 adversarial: roundtrip tests using TransposeChunkEncoder
    // -------------------------------------------------------------------

    /// Helper: encode records then decode, returning decoded records.
    fn roundtrip_via_encoder(records: &[&[u8]], compression: CompressionType) -> Vec<Vec<u8>> {
        use crate::transpose::encoder::TransposeChunkEncoder;
        let mut enc = TransposeChunkEncoder::new(compression);
        for rec in records {
            enc.add_record(rec).expect("add_record");
        }
        let chunk = enc.encode().expect("encode");
        let mut dec = TransposeChunkDecoder::new(chunk).expect("decoder");
        let mut out = Vec::new();
        while let Some(rec) = dec.read_record().expect("read_record") {
            out.push(rec);
        }
        out
    }

    #[test]
    fn test_zero_records_via_encoder() {
        use crate::chunk_header::ChunkType;
        use crate::transpose::encoder::TransposeChunkEncoder;
        let enc = TransposeChunkEncoder::new(CompressionType::None);
        let chunk = enc.encode().expect("encode");
        assert_eq!(chunk.header.num_records(), 0);
        assert_eq!(chunk.header.chunk_type().unwrap(), ChunkType::Transposed);

        let mut dec = TransposeChunkDecoder::new(chunk).expect("decoder");
        assert!(
            dec.read_record().unwrap().is_none(),
            "first call should be None"
        );
        assert!(
            dec.read_record().unwrap().is_none(),
            "second call should also be None"
        );
    }

    #[test]
    fn test_nonproto_binary() {
        // Binary data that's definitely not valid proto (wire type 7).
        let record: Vec<u8> = vec![0x0F, 0xDE, 0xAD, 0xBE, 0xEF];
        let result = roundtrip_via_encoder(&[&record], CompressionType::None);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0], record);
    }

    #[test]
    fn test_nonproto_single_byte() {
        let record = vec![0xFF];
        let result = roundtrip_via_encoder(&[&record], CompressionType::None);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0], record);
    }

    #[test]
    fn test_nonproto_empty_record_roundtrip() {
        // Empty record: valid proto (empty message), byte-for-byte round-trip.
        let record: Vec<u8> = vec![];
        let result = roundtrip_via_encoder(&[&record], CompressionType::None);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0], record);
    }

    #[test]
    fn test_all_wire_types_roundtrip() {
        // Construct a proto with varint, fixed32, fixed64, length-delimited, nested submessage.
        let record: Vec<u8> = vec![
            0x08, 0x2A, 0x15, 0x01, 0x02, 0x03, 0x04, 0x19, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06,
            0x07, 0x08, 0x22, 0x03, 0x61, 0x62, 0x63, 0x2A, 0x02, 0x08, 0x07,
        ];
        let result = roundtrip_via_encoder(&[&record], CompressionType::None);
        assert_eq!(result.len(), 1);
        assert_eq!(
            result[0], record,
            "proto with all wire types must round-trip exactly"
        );
    }

    #[test]
    fn test_inline_varint_0_through_3() {
        // Values 0-3 are stored inline in the subtype.
        for val in 0u8..=3 {
            let record = vec![0x08, val];
            let result = roundtrip_via_encoder(&[&record], CompressionType::None);
            assert_eq!(result.len(), 1, "inline varint {val}");
            assert_eq!(result[0], record, "inline varint {val} mismatch");
        }
    }

    #[test]
    fn test_max_varint64_roundtrip() {
        // u64::MAX = 10-byte varint.
        let record: Vec<u8> = vec![
            0x08, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x01,
        ];
        let result = roundtrip_via_encoder(&[&record], CompressionType::None);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0], record, "max varint64 must round-trip");
    }

    #[test]
    fn test_multiple_fields_same_type() {
        // Two varint fields: field 1 = 10, field 2 = 20
        let record: Vec<u8> = vec![0x08, 0x0A, 0x10, 0x14];
        let result = roundtrip_via_encoder(&[&record], CompressionType::None);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0], record);
    }

    #[test]
    fn test_mixed_proto_nonproto_interleaved() {
        let proto1 = vec![0x08, 0x2A];
        let nonproto = vec![0xFF, 0xAA, 0xBB];
        let proto2 = vec![0x10, 0x01];
        let result = roundtrip_via_encoder(&[&proto1, &nonproto, &proto2], CompressionType::None);
        assert_eq!(result.len(), 3);
        assert_eq!(result[0], proto1, "record 0 proto mismatch");
        assert_eq!(result[1], nonproto, "record 1 nonproto mismatch");
        assert_eq!(result[2], proto2, "record 2 proto mismatch");
    }

    #[test]
    fn test_many_mixed_records() {
        let mut records: Vec<Vec<u8>> = Vec::new();
        for i in 0u32..20 {
            if i % 3 == 0 {
                records.push(vec![0xFF, i as u8]);
            } else {
                let mut rec = vec![0x08];
                rec.extend_from_slice(&encode_u64(i as u64));
                records.push(rec);
            }
        }
        let refs: Vec<&[u8]> = records.iter().map(|r| r.as_slice()).collect();
        let result = roundtrip_via_encoder(&refs, CompressionType::None);
        assert_eq!(result.len(), records.len());
        for (i, (got, expected)) in result.iter().zip(records.iter()).enumerate() {
            assert_eq!(got, expected, "record {i} mismatch");
        }
    }

    #[test]
    fn test_corrupted_bucket_no_panic() {
        use crate::chunk_header::ChunkHeader;
        use crate::transpose::encoder::TransposeChunkEncoder;
        let record = vec![0x08, 0x2A];
        let mut enc = TransposeChunkEncoder::new(CompressionType::None);
        enc.add_record(&record).unwrap();
        let mut chunk = enc.encode().unwrap();

        let len = chunk.data.len();
        if len > 10 {
            for i in (len - 5)..len {
                chunk.data[i] ^= 0xFF;
            }
        }

        // Rebuild header to pass hash validation.
        chunk.header = ChunkHeader::from_parts(
            &chunk.data,
            crate::chunk_header::ChunkType::Transposed,
            1,
            2,
        );

        let result = std::panic::catch_unwind(|| match TransposeChunkDecoder::new(chunk) {
            Ok(mut dec) => {
                let _ = dec.read_record();
            }
            Err(_) => {}
        });
        assert!(result.is_ok(), "corrupted bucket must not panic");
    }

    #[test]
    fn test_truncated_chunk_data_no_panic() {
        use crate::chunk_header::ChunkHeader;
        use crate::chunk_header::ChunkType;
        use crate::simple_chunk::Chunk;
        use crate::transpose::encoder::TransposeChunkEncoder;
        let record = vec![0x08, 0x2A];
        let mut enc = TransposeChunkEncoder::new(CompressionType::None);
        enc.add_record(&record).unwrap();
        let chunk = enc.encode().unwrap();

        let truncated_data = chunk.data[..chunk.data.len() / 2].to_vec();
        let truncated_header =
            ChunkHeader::from_parts(&truncated_data, ChunkType::Transposed, 1, 2);
        let truncated_chunk = Chunk {
            header: truncated_header,
            data: truncated_data,
        };

        let result = std::panic::catch_unwind(|| {
            let _ = TransposeChunkDecoder::new(truncated_chunk);
        });
        assert!(result.is_ok(), "truncated chunk must not panic");
    }

    #[test]
    fn test_empty_chunk_data_returns_err() {
        use crate::chunk_header::ChunkHeader;
        use crate::chunk_header::ChunkType;
        use crate::simple_chunk::Chunk;
        let chunk_data: Vec<u8> = Vec::new();
        let chunk_header = ChunkHeader::from_parts(&chunk_data, ChunkType::Transposed, 1, 5);
        let chunk = Chunk {
            header: chunk_header,
            data: chunk_data,
        };

        let result = TransposeChunkDecoder::new(chunk);
        assert!(result.is_err(), "empty chunk data should be Err");
    }

    #[test]
    fn test_interleaved_simple_and_transpose_files() {
        use crate::compression::CompressionType;
        use crate::record_reader::{ReaderOptions, RecordReader};
        use crate::record_writer::{RecordWriter, WriterOptions};

        // Write file with simple chunks.
        let mut buf_simple: Vec<u8> = Vec::new();
        {
            let opts = WriterOptions::new().compression(CompressionType::None);
            let cursor = std::io::Cursor::new(&mut buf_simple);
            let mut writer = RecordWriter::new(cursor, opts).unwrap();
            writer.write_record(b"simple_record_1").unwrap();
            writer.write_record(b"simple_record_2").unwrap();
            writer.close().unwrap();
        }

        // Write file with transpose chunks.
        let mut buf_transpose: Vec<u8> = Vec::new();
        {
            let opts = WriterOptions::new()
                .compression(CompressionType::None)
                .transpose(true);
            let cursor = std::io::Cursor::new(&mut buf_transpose);
            let mut writer = RecordWriter::new(cursor, opts).unwrap();
            writer.write_record(b"transpose_record_1").unwrap();
            writer.write_record(b"transpose_record_2").unwrap();
            writer.close().unwrap();
        }

        let simple_records = {
            let cursor = std::io::Cursor::new(&buf_simple);
            let mut reader = RecordReader::new(cursor, ReaderOptions::new()).unwrap();
            let mut recs = Vec::new();
            while let Some(rec) = reader.read_record().unwrap() {
                recs.push(rec);
            }
            recs
        };
        assert_eq!(simple_records.len(), 2);
        assert_eq!(simple_records[0], b"simple_record_1");
        assert_eq!(simple_records[1], b"simple_record_2");

        let transpose_records = {
            let cursor = std::io::Cursor::new(&buf_transpose);
            let mut reader = RecordReader::new(cursor, ReaderOptions::new()).unwrap();
            let mut recs = Vec::new();
            while let Some(rec) = reader.read_record().unwrap() {
                recs.push(rec);
            }
            recs
        };
        assert_eq!(transpose_records.len(), 2);
        assert_eq!(transpose_records[0], b"transpose_record_1");
        assert_eq!(transpose_records[1], b"transpose_record_2");
    }

    #[test]
    fn test_large_proto_string_field_roundtrip() {
        // field 2, length-delimited, 5000 bytes of 'A'.
        let mut record = vec![0x12]; // tag: field 2, length-delimited
        record.extend_from_slice(&encode_u32(5000));
        record.extend(std::iter::repeat(0x41).take(5000));
        let result = roundtrip_via_encoder(&[&record], CompressionType::None);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0], record, "large string field must round-trip");
    }

    #[test]
    fn test_repeated_read_after_none() {
        use crate::transpose::encoder::TransposeChunkEncoder;
        let record = vec![0x08, 0x01];
        let mut enc = TransposeChunkEncoder::new(CompressionType::None);
        enc.add_record(&record).unwrap();
        let chunk = enc.encode().unwrap();
        let mut dec = TransposeChunkDecoder::new(chunk).unwrap();

        assert!(dec.read_record().unwrap().is_some());
        assert!(dec.read_record().unwrap().is_none());
        assert!(dec.read_record().unwrap().is_none());
        assert!(dec.read_record().unwrap().is_none());
    }

    // =========================================================================
    // Sprint 31: Projection-aware callback type tests
    // =========================================================================

    #[test]
    fn test_include_field_map_from_simple_projection() {
        // Projection: include field 1 at top level.
        let proj = FieldProjection::new().add_field(Field::new(vec![1]));
        let map = IncludeFieldMap::from_projection(&proj).unwrap();
        let entry = map.get(INVALID_POS, 1).expect("field 1 should be in map");
        assert_eq!(entry.include_type, IncludeType::IncludeFully);
        // Field 2 should not be present.
        assert!(map.get(INVALID_POS, 2).is_none());
    }

    #[test]
    fn test_include_field_map_nested_path() {
        // Projection: include field path [2, 3, 5].
        let proj = FieldProjection::new().add_field(Field::new(vec![2, 3, 5]));
        let map = IncludeFieldMap::from_projection(&proj).unwrap();

        // Root -> field 2: IncludeChild
        let entry2 = map.get(INVALID_POS, 2).expect("field 2 at root");
        assert_eq!(entry2.include_type, IncludeType::IncludeChild);

        // field 2 -> field 3: IncludeChild
        let entry3 = map.get(entry2.field_id, 3).expect("field 3 under field 2");
        assert_eq!(entry3.include_type, IncludeType::IncludeChild);

        // field 3 -> field 5: IncludeFully
        let entry5 = map.get(entry3.field_id, 5).expect("field 5 under field 3");
        assert_eq!(entry5.include_type, IncludeType::IncludeFully);
    }

    #[test]
    fn test_include_field_map_existence_only() {
        let proj = FieldProjection::new().add_field(Field::new(vec![1]).existence_only());
        let map = IncludeFieldMap::from_projection(&proj).unwrap();
        let entry = map.get(INVALID_POS, 1).expect("field 1 should be present");
        assert_eq!(entry.include_type, IncludeType::ExistenceOnly);
    }

    #[test]
    fn test_include_field_map_more_inclusive_wins() {
        // Two paths through same prefix: [1, 2] fully and [1, 3] fully.
        // Field 1 should be IncludeChild (not overridden to IncludeFully).
        let proj = FieldProjection::new()
            .add_field(Field::new(vec![1, 2]))
            .add_field(Field::new(vec![1, 3]));
        let map = IncludeFieldMap::from_projection(&proj).unwrap();
        let entry1 = map.get(INVALID_POS, 1).expect("field 1 at root");
        assert_eq!(entry1.include_type, IncludeType::IncludeChild);

        // Now add a path that includes field 1 fully: [1].
        let proj2 = proj.add_field(Field::new(vec![1]));
        let map2 = IncludeFieldMap::from_projection(&proj2).unwrap();
        let entry1b = map2.get(INVALID_POS, 1).expect("field 1 at root");
        // IncludeFully < IncludeChild, so IncludeFully wins.
        assert_eq!(entry1b.include_type, IncludeType::IncludeFully);
    }

    #[test]
    fn test_include_field_map_all_projection_returns_none() {
        let proj = FieldProjection::all();
        assert!(IncludeFieldMap::from_projection(&proj).is_none());
    }

    #[test]
    fn test_callback_for_field_included_excluded_varint() {
        let tag = make_tag(1, WireType::Varint);
        let result = callback_for_field_included(FieldIncluded::No, tag, 0).unwrap();
        assert_eq!(result, CallbackType::NoOp);
    }

    #[test]
    fn test_callback_for_field_included_excluded_submessage_end() {
        let tag = make_tag(1, WireType::LengthDelimited);
        let result = callback_for_field_included(
            FieldIncluded::No,
            tag,
            subtype::LENGTH_DELIMITED_END_OF_SUBMESSAGE,
        )
        .unwrap();
        assert_eq!(result, CallbackType::SkippedSubmessageEnd);
    }

    #[test]
    fn test_callback_for_field_included_excluded_string() {
        let tag = make_tag(1, WireType::LengthDelimited);
        let result =
            callback_for_field_included(FieldIncluded::No, tag, subtype::LENGTH_DELIMITED_STRING)
                .unwrap();
        assert_eq!(result, CallbackType::NoOp);
    }

    #[test]
    fn test_callback_for_field_included_excluded_fixed32() {
        let tag = make_tag(1, WireType::Fixed32);
        let result = callback_for_field_included(FieldIncluded::No, tag, 0).unwrap();
        assert_eq!(result, CallbackType::NoOp);
    }

    #[test]
    fn test_callback_for_field_included_excluded_fixed64() {
        let tag = make_tag(1, WireType::Fixed64);
        let result = callback_for_field_included(FieldIncluded::No, tag, 0).unwrap();
        assert_eq!(result, CallbackType::NoOp);
    }

    #[test]
    fn test_select_callback_nodes_only_with_projection() {
        // Without projection: no SelectCallback nodes.
        use crate::transpose::encoder::TransposeChunkEncoder;
        let record = vec![0x08, 0x01]; // field 1 varint = 1
        let mut enc = TransposeChunkEncoder::new(CompressionType::None);
        enc.add_record(&record).unwrap();
        let chunk = enc.encode().unwrap();

        // Decode without projection.
        let mut dec = TransposeChunkDecoder::new(chunk.clone()).unwrap();
        let r = dec.read_record().unwrap().unwrap();
        assert_eq!(r, record);

        // Decode with projection including field 1 — should produce identical output.
        let proj = FieldProjection::new().add_field(Field::new(vec![1]));
        let mut dec2 = TransposeChunkDecoder::new_with_projection(chunk, Some(&proj)).unwrap();
        let r2 = dec2.read_record().unwrap().unwrap();
        assert_eq!(r2, record);
    }

    #[test]
    fn test_projection_with_submessages_identical_output() {
        // Encode a record with top-level field 1 = varint 42 and field 2 = varint 99.
        use crate::transpose::encoder::TransposeChunkEncoder;
        let mut record = Vec::new();
        record.push(0x08); // field 1, varint
        record.push(42);
        record.push(0x10); // field 2, varint
        record.push(99);

        let mut enc = TransposeChunkEncoder::new(CompressionType::None);
        enc.add_record(&record).unwrap();
        let chunk = enc.encode().unwrap();

        // Decode without projection — should get full record.
        let mut dec1 = TransposeChunkDecoder::new(chunk.clone()).unwrap();
        let r1 = dec1.read_record().unwrap().unwrap();
        assert_eq!(r1, record);

        // Decode with projection [1] — should include only field 1.
        let proj = FieldProjection::new().add_field(Field::new(vec![1]));
        let mut dec2 =
            TransposeChunkDecoder::new_with_projection(chunk.clone(), Some(&proj)).unwrap();
        let r2 = dec2.read_record().unwrap().unwrap();
        // The post-decode apply() filter removes field 2.
        assert_eq!(r2, vec![0x08, 42]);

        // Decode with all() projection — byte-identical to non-projected.
        let proj_all = FieldProjection::all();
        let mut dec3 = TransposeChunkDecoder::new_with_projection(chunk, Some(&proj_all)).unwrap();
        let r3 = dec3.read_record().unwrap().unwrap();
        assert_eq!(r3, record);
    }

    #[test]
    fn test_resolve_nested_submessage_path_2_3_5() {
        // Verify that resolve_select_callback correctly handles field path [2, 3, 5].
        // Build an IncludeFieldMap for [2, 3, 5].
        let proj = FieldProjection::new().add_field(Field::new(vec![2, 3, 5]));
        let include_map = IncludeFieldMap::from_projection(&proj).unwrap();

        // Simulate a submessage stack with entries for field 2 and field 3.
        // The submessage stack stores SubmessageEnd nodes whose tag_data
        // contains the tag bytes for the enclosing field.
        let tag_field2 = make_tag(2, WireType::LengthDelimited);
        let tag_field3 = make_tag(3, WireType::LengthDelimited);
        let tag_field5 = make_tag(5, WireType::Varint);

        let node_for_field2 = StateMachineNode {
            tag_data: encode_u32(tag_field2),
            tag_data_size: encode_u32(tag_field2).len(),
            callback_type: CallbackType::SubmessageEnd,
            buffer_index: None,
            next_node_index: 0,
            is_implicit: false,
            node_template_index: None,
        };
        let node_for_field3 = StateMachineNode {
            tag_data: encode_u32(tag_field3),
            tag_data_size: encode_u32(tag_field3).len(),
            callback_type: CallbackType::SubmessageEnd,
            buffer_index: None,
            next_node_index: 0,
            is_implicit: false,
            node_template_index: None,
        };

        let nodes = vec![node_for_field2, node_for_field3];
        let submessage_stack = vec![
            SubmessageFrame {
                end_pos: 0,
                node_index: 0, // field 2
            },
            SubmessageFrame {
                end_pos: 0,
                node_index: 1, // field 3
            },
        ];

        // Create a SelectCallback node for field 5 (varint).
        let node_templates = vec![NodeTemplate {
            tag: tag_field5,
            subtype: 0, // VARINT_1
            tag_length: 1,
        }];
        let select_node = StateMachineNode {
            tag_data: encode_u32(tag_field5),
            tag_data_size: encode_u32(tag_field5).len(),
            callback_type: CallbackType::SelectCallback,
            buffer_index: Some(0),
            next_node_index: 0,
            is_implicit: false,
            node_template_index: Some(0),
        };

        // Resolve: field 5 under submessage stack [field2, field3] with
        // projection [2, 3, 5] should resolve to included (FieldIncluded::Yes).
        let result = resolve_select_callback(
            &select_node,
            &node_templates,
            &include_map,
            &submessage_stack,
            &nodes,
            0, // skipped_submessage_level
        )
        .unwrap();

        // Should be a Varint callback (included).
        assert_eq!(
            result,
            CallbackType::Varint { data_length: 1 },
            "field at path [2, 3, 5] should resolve to included Varint"
        );
    }

    #[test]
    fn test_resolve_excluded_field() {
        // Projection: [1]. Resolve field 2 at top level -> excluded.
        let proj = FieldProjection::new().add_field(Field::new(vec![1]));
        let include_map = IncludeFieldMap::from_projection(&proj).unwrap();

        let tag_field2 = make_tag(2, WireType::Varint);
        let node_templates = vec![NodeTemplate {
            tag: tag_field2,
            subtype: 0,
            tag_length: 1,
        }];
        let select_node = StateMachineNode {
            tag_data: encode_u32(tag_field2),
            tag_data_size: encode_u32(tag_field2).len(),
            callback_type: CallbackType::SelectCallback,
            buffer_index: Some(0),
            next_node_index: 0,
            is_implicit: false,
            node_template_index: Some(0),
        };

        let result = resolve_select_callback(
            &select_node,
            &node_templates,
            &include_map,
            &[], // empty submessage stack (top level)
            &[],
            0,
        )
        .unwrap();

        assert_eq!(
            result,
            CallbackType::NoOp,
            "field 2 should be excluded (NoOp)"
        );
    }

    #[test]
    fn test_resolve_submessage_start_when_skipped() {
        let proj = FieldProjection::new().add_field(Field::new(vec![1]));
        let include_map = IncludeFieldMap::from_projection(&proj).unwrap();

        let node_templates = vec![NodeTemplate {
            tag: message_id::START_OF_SUBMESSAGE,
            subtype: 0,
            tag_length: 0,
        }];
        let select_node = StateMachineNode {
            tag_data: Vec::new(),
            tag_data_size: 0,
            callback_type: CallbackType::SelectCallback,
            buffer_index: None,
            next_node_index: 0,
            is_implicit: false,
            node_template_index: Some(0),
        };

        // When skipped_submessage_level > 0, should resolve to SkippedSubmessageStart.
        let result = resolve_select_callback(
            &select_node,
            &node_templates,
            &include_map,
            &[],
            &[],
            1, // skipped level > 0
        )
        .unwrap();
        assert_eq!(result, CallbackType::SkippedSubmessageStart);

        // When skipped_submessage_level == 0, should resolve to SubmessageStart.
        let result2 =
            resolve_select_callback(&select_node, &node_templates, &include_map, &[], &[], 0)
                .unwrap();
        assert_eq!(result2, CallbackType::SubmessageStart);
    }
}
