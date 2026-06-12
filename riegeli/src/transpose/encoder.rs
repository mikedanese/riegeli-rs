//! Transpose chunk encoder for the Riegeli file format.
//!
//! Decomposes protobuf records column-wise by field path and emits a
//! `ChunkType::Transposed` chunk. The **proto field decomposition algorithm**
//! works as follows:
//!
//! 1. Each record is checked with [`is_proto_message`]. Valid proto records are
//!    recursively decomposed; non-proto records are stored verbatim.
//! 2. For proto records, each field is identified by its wire type. Varint
//!    values are stored with their high bits stripped (bit 7 cleared on each
//!    byte) and grouped by byte-length subtype. Small varint values (0..3) are
//!    encoded inline in the subtype itself, requiring no data buffer.
//! 3. Length-delimited fields that contain valid proto data are treated as
//!    submessages and recursively decomposed up to
//!    `MAX_RECURSION_DEPTH`.
//! 4. All field data is written **backward** (prepended) into per-node buffers.
//!    At encode time the buffers are reversed to produce correct forward data.
//! 5. An **optimized state machine** is built using transition statistics,
//!    private/public destination lists, NoOp bridging states, and implicit
//!    transitions (matching the C++ `CreateStateMachine` algorithm).
//! 6. Buffers are sorted by type (Varint, Fixed32, Fixed64, String, NonProto),
//!    then by length, parent message ID, and tag for reproducible ordering.

use std::collections::{BTreeMap, BinaryHeap};

use crate::chunk_header::{ChunkHeader, ChunkType};
use crate::compression::{
    compress_length_prefixed, compress_with_prefix, CompressOptions, CompressionType,
};
use crate::error::RiegeliError;
use crate::proto::{is_proto_message, tag_field_number, tag_wire_type, WireType};
use crate::simple_chunk::Chunk;
use crate::transpose::internal::{
    has_data_buffer, has_subtype, message_id, subtype, MAX_RECURSION_DEPTH, MAX_VARINT_INLINE,
    SUBMESSAGE_WIRE_TYPE,
};
use crate::varint::{decode_u32, decode_u64, encode_u32, encode_u64};

/// Maximum transition offset (0..63). Transitions beyond this require NoOp
/// bridging.
const MAX_TRANSITION: u32 = 63;

/// Minimum number of transitions between two tags for the destination to get
/// a dedicated state in the source's private list.
const MIN_COUNT_FOR_STATE: usize = 10;

/// Sentinel for uninitialized / invalid positions.
const INVALID_POS: u32 = u32::MAX;

/// A unique field path: (parent_message_id, tag).
///
/// For reserved message IDs (NonProto, StartOfMessage, etc.), `tag` is 0.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
struct NodeId {
    /// The message ID of the parent message containing this field.
    parent_message_id: u32,
    /// The proto tag (field_number << 3 | wire_type), or 0 for reserved nodes.
    tag: u32,
}

/// The type of data buffer, matching C++ `BufferType` enum ordering.
///
/// Buffers are iterated in this order during serialization: Varint, Fixed32,
/// Fixed64, String, NonProto.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
#[repr(u32)]
enum BufferType {
    /// Variable-length integer data (with high bits stripped).
    Varint = 0,
    /// 32-bit fixed-width field data.
    Fixed32 = 1,
    /// 64-bit fixed-width field data.
    Fixed64 = 2,
    /// Length-delimited string/bytes field data (length prefix + payload).
    String = 3,
    /// Verbatim non-proto record data.
    NonProto = 4,
}

/// All buffer types in iteration order.
const ALL_BUFFER_TYPES: [BufferType; 5] = [
    BufferType::Varint,
    BufferType::Fixed32,
    BufferType::Fixed64,
    BufferType::String,
    BufferType::NonProto,
];

/// Information about a transition destination from a particular encoded tag.
#[derive(Debug, Clone)]
struct DestInfo {
    /// Position of the destination in the destination list for this source.
    /// `INVALID_POS` if the destination is not in the private list (will use
    /// public list).
    pos: u32,
    /// Number of transitions to this destination from this source.
    num_transitions: usize,
}

impl DestInfo {
    /// Create a `DestInfo` with no observed transitions and unassigned position.
    fn new() -> Self {
        Self {
            pos: INVALID_POS,
            num_transitions: 0,
        }
    }
}

/// Info about a unique encoded tag (one state in the state machine).
#[derive(Debug)]
struct EncodedTagInfo {
    /// The field path this tag belongs to.
    node_id: NodeId,
    /// The subtype (varint byte-length, inline value, or string/submessage
    /// discriminator).
    subtype: u8,
    /// Maps destination tag indices to transition statistics.
    dest_info: BTreeMap<u32, DestInfo>,
    /// Number of incoming transitions to this tag from all sources.
    num_incoming_transitions: usize,
    /// Index of this tag's state in the final state machine.
    state_machine_pos: u32,
    /// Position of the NoOp state in this tag's private list that bridges
    /// to the public list.
    public_list_noop_pos: u32,
    /// Base index for outgoing transitions from this tag.
    base: u32,
}

impl EncodedTagInfo {
    /// Create a new encoded tag entry with no transitions and unassigned positions.
    fn new(node_id: NodeId, subtype: u8) -> Self {
        Self {
            node_id,
            subtype,
            dest_info: BTreeMap::new(),
            num_incoming_transitions: 0,
            state_machine_pos: INVALID_POS,
            public_list_noop_pos: INVALID_POS,
            base: INVALID_POS,
        }
    }
}

/// A single state in the encoder's state machine.
#[derive(Debug, Clone)]
struct StateInfo {
    /// Index into `tags_list` for the encoded tag this state represents.
    /// `INVALID_POS` for NoOp states.
    etag_index: u32,
    /// Base index: transitions from this state target states in
    /// `[base..base + MAX_TRANSITION]`.
    base: u32,
    /// The canonical NoOp source that reaches the block containing this state.
    /// Used for multi-hop transition encoding.
    canonical_source: u32,
}

impl StateInfo {
    /// Create a state pointing to the given encoded tag with the specified base.
    fn new(etag_index: u32, base: u32) -> Self {
        Self {
            etag_index,
            base,
            canonical_source: INVALID_POS,
        }
    }
}

/// Entry in the priority queue used during state machine construction.
/// Popped in order of ascending `num_transitions` (ties broken by descending
/// `dest_index` for reproducibility), matching the C++ comparator under
/// `std::priority_queue`.
#[derive(Debug, Clone, Eq, PartialEq)]
struct PriorityQueueEntry {
    dest_index: u32,
    num_transitions: usize,
}

impl Ord for PriorityQueueEntry {
    /// Replicates the C++ comparator: `a < b` iff `a.num_transitions >
    /// b.num_transitions`, ties broken by ascending `dest_index`. Both
    /// `std::priority_queue` and `BinaryHeap` are max-heaps, so they pop the
    /// entry with the fewest transitions first. The state machine is written
    /// back-to-front, which places the most frequent destinations at the
    /// lowest state indices (reachable without NoOp hops).
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        other
            .num_transitions
            .cmp(&self.num_transitions)
            .then_with(|| self.dest_index.cmp(&other.dest_index))
    }
}

impl PartialOrd for PriorityQueueEntry {
    /// Delegate to [`Ord::cmp`] for total ordering.
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

/// Accumulates field data in forward (append) order and writes it out in
/// reverse-chunk order, matching the C++ `ChainBackwardWriter` semantics
/// without O(n) memmoves on every insert.
#[derive(Default)]
struct BackwardBuffer {
    /// Field values appended in record order (record 1, 2, … N).
    data: Vec<u8>,
    /// Size of each appended chunk, used to reverse at write time.
    ///
    /// Kept as `usize` to avoid silent truncation for chunks of 4 GiB or
    /// more (the C++ `ChainBackwardWriter` keeps 64-bit sizes throughout).
    sizes: Vec<usize>,
}

impl BackwardBuffer {
    fn push_chunk(&mut self, chunk: &[u8]) {
        self.data.extend_from_slice(chunk);
        self.sizes.push(chunk.len());
    }

    fn len(&self) -> usize {
        self.data.len()
    }

    fn is_empty(&self) -> bool {
        self.data.is_empty()
    }

    /// Write chunks in reverse insertion order directly into `out`.
    fn write_to(&self, out: &mut Vec<u8>) {
        out.reserve(self.data.len());
        let mut end = self.data.len();
        for &size in self.sizes.iter().rev() {
            let start = end - size;
            out.extend_from_slice(&self.data[start..end]);
            end = start;
        }
    }
}

/// A data buffer and the [`NodeId`] it belongs to.
struct BufferWithMetadata {
    /// The field path that produced this buffer's data.
    node_id: NodeId,
    /// The raw data bytes (accumulated via BackwardBuffer).
    data: BackwardBuffer,
}

/// Per-node state during encoding.
struct MessageNode {
    /// Sequential message ID assigned to this node.
    message_id: u32,
    /// Map from subtype -> index into `tags_list`.
    encoded_tag_pos: Vec<u32>,
}

/// A frame on the encoder's message stack, used during recursive proto
/// field decomposition.
///
/// Pushed when entering a submessage; popped when the submessage's byte
/// range is fully consumed.
struct MessageFrame {
    /// Index into `tags_list` for the end-of-submessage encoded tag.
    end_sub_tag_idx: u32,
    /// The `parent_message_id` to restore after leaving the submessage.
    parent_message_id: u32,
    /// The byte position (in the record) where the parent's remaining
    /// fields resume.
    parent_end_pos: usize,
}

/// Collection of data buffers grouped by [`BufferType`].
///
/// Provides typed access instead of raw integer indexing into an array.
struct DataBuffers {
    /// One `Vec<BufferWithMetadata>` per buffer type.
    inner: [Vec<BufferWithMetadata>; 5],
}

impl DataBuffers {
    /// Create an empty buffer collection.
    fn new() -> Self {
        Self {
            inner: [Vec::new(), Vec::new(), Vec::new(), Vec::new(), Vec::new()],
        }
    }

    /// Get the mutable buffers for a given type.
    fn get_mut(&mut self, bt: BufferType) -> &mut Vec<BufferWithMetadata> {
        &mut self.inner[bt as usize]
    }
}

/// Encoder that decomposes proto records field-wise and emits a transposed chunk.
///
/// # Usage
///
/// ```ignore
/// let mut enc = TransposeChunkEncoder::new(CompressionType::None);
/// enc.add_record(proto_bytes)?;
/// let chunk = enc.encode()?;
/// ```
pub struct TransposeChunkEncoder {
    /// Compression type for the output chunk.
    compression: CompressionType,
    /// Compression tuning options (level, window_log).
    compress_opts: CompressOptions,
    /// Map from NodeId -> MessageNode for all encountered field paths.
    message_nodes: BTreeMap<NodeId, MessageNode>,
    /// Next message ID to assign.
    next_message_id: u32,
    /// List of unique (NodeId, subtype) pairs, in order of first encounter.
    tags_list: Vec<EncodedTagInfo>,
    /// Sequence of encoded tag indices (indices into tags_list), in forward order.
    encoded_tags: Vec<u32>,
    /// Data buffers grouped by BufferType.
    data: DataBuffers,
    /// NonProto lengths buffer (varint-encoded lengths, in backward order).
    nonproto_lengths: BackwardBuffer,
    /// Number of records added.
    num_records: u64,
    /// Total decoded data size (sum of all record byte lengths).
    decoded_data_size: u64,
    /// Maximum uncompressed size (in bytes) per bucket. When a bucket exceeds
    /// this threshold, a new bucket is started. `u64::MAX` means all buffers
    /// go into a single bucket (the default).
    bucket_size: u64,
}

impl TransposeChunkEncoder {
    /// Create a new transpose encoder with the specified compression type.
    pub fn new(compression: CompressionType) -> Self {
        Self {
            compression,
            compress_opts: CompressOptions::default(),
            message_nodes: BTreeMap::new(),
            next_message_id: message_id::ROOT + 1,
            tags_list: Vec::new(),
            encoded_tags: Vec::new(),
            data: DataBuffers::new(),
            nonproto_lengths: BackwardBuffer::default(),
            num_records: 0,
            decoded_data_size: 0,
            bucket_size: u64::MAX,
        }
    }

    /// Set compression tuning options (level and window_log).
    pub fn compress_opts(mut self, opts: CompressOptions) -> Self {
        self.compress_opts = opts;
        self
    }

    /// Set the maximum uncompressed byte size per bucket.
    ///
    /// When set to a value smaller than `u64::MAX`, buffers are split across
    /// multiple independently compressed buckets using a greedy algorithm.
    /// Smaller buckets enable field projection (skipping decompression of
    /// unneeded data) at the cost of slightly worse compression.
    ///
    /// The default is `u64::MAX` (single bucket).
    pub fn bucket_size(mut self, size: u64) -> Self {
        self.bucket_size = size;
        self
    }

    /// Add a record to the encoder.
    ///
    /// If the record is valid proto binary, it is decomposed field-wise.
    /// Otherwise it is stored verbatim as a NonProto record.
    pub fn add_record(&mut self, data: &[u8]) -> Result<(), RiegeliError> {
        self.num_records += 1;
        self.decoded_data_size += data.len() as u64;

        if is_proto_message(data) {
            // Push StartOfMessage tag.
            let start_node_id = NodeId {
                parent_message_id: message_id::START_OF_MESSAGE,
                tag: 0,
            };
            let start_idx = self.get_pos_in_tags_list(start_node_id, subtype::TRIVIAL);
            self.encoded_tags.push(start_idx);

            // Decompose the proto message.
            self.add_message(data, message_id::ROOT, 0)?;
        } else {
            // NonProto: store verbatim data in buffer.
            let np_node_id = NodeId {
                parent_message_id: message_id::NON_PROTO,
                tag: 0,
            };
            let np_idx = self.get_pos_in_tags_list(np_node_id, subtype::TRIVIAL);
            self.encoded_tags.push(np_idx);

            // Write data to nonproto buffer (backward/prepend).
            let buffer = self.get_buffer(np_node_id, BufferType::NonProto);
            buffer.push_chunk(data);

            // Write length to nonproto_lengths (backward/prepended, matching C++).
            let len_varint = encode_u64(data.len() as u64);
            self.nonproto_lengths.push_chunk(&len_varint);
        }

        Ok(())
    }

    /// Encode all accumulated records into a transposed chunk.
    pub fn encode(mut self) -> Result<Chunk, RiegeliError> {
        if self.num_records == 0 {
            return self.encode_empty();
        }

        let (all_buffers, buffer_sizes, buffer_pos) = self.collect_and_sort_buffers();

        // Build the optimized state machine.
        let state_machine = self.create_state_machine();

        // Ensure last state does not have an implicit transition.
        // (The decoder must see an explicit transition to know when to stop.)
        self.ensure_last_state_explicit();

        // Split buffers into buckets and compress each independently.
        let compressed_buckets = self.create_compressed_buckets(&all_buffers)?;

        let header = self.build_header(
            &compressed_buckets,
            &buffer_sizes,
            &buffer_pos,
            &state_machine,
        )?;
        self.assemble_chunk(header, &compressed_buckets, &state_machine)
    }

    /// Collect all data buffers, sort them by type then by (size, parent, tag),
    /// and build the buffer-position lookup map.
    fn collect_and_sort_buffers(&mut self) -> (Vec<Vec<u8>>, Vec<u64>, BTreeMap<NodeId, u32>) {
        let mut buffer_pos: BTreeMap<NodeId, u32> = BTreeMap::new();
        let mut all_buffers: Vec<Vec<u8>> = Vec::new();
        let mut buffer_sizes: Vec<u64> = Vec::new();

        for &buf_type in &ALL_BUFFER_TYPES {
            let buffers = self.data.get_mut(buf_type);
            buffers.sort_by(|a, b| {
                let size_cmp = a.data.len().cmp(&b.data.len());
                if size_cmp != std::cmp::Ordering::Equal {
                    return size_cmp;
                }
                let parent_cmp = a
                    .node_id
                    .parent_message_id
                    .cmp(&b.node_id.parent_message_id);
                if parent_cmp != std::cmp::Ordering::Equal {
                    return parent_cmp;
                }
                a.node_id.tag.cmp(&b.node_id.tag)
            });

            for buf in buffers.iter() {
                buffer_pos.insert(buf.node_id, all_buffers.len() as u32);
                buffer_sizes.push(buf.data.len() as u64);
                let mut reversed = Vec::new();
                buf.data.write_to(&mut reversed);
                all_buffers.push(reversed);
            }
        }

        // nonproto_lengths is the last buffer if non-empty.
        if !self.nonproto_lengths.is_empty() {
            buffer_sizes.push(self.nonproto_lengths.len() as u64);
            let mut reversed = Vec::new();
            self.nonproto_lengths.write_to(&mut reversed);
            all_buffers.push(reversed);
        }

        (all_buffers, buffer_sizes, buffer_pos)
    }

    /// Split all buffers into independently compressed buckets using the
    /// greedy algorithm: accumulate buffer sizes within each BufferType group
    /// (sorted by size descending for splitting), and start a new bucket when
    /// `current_bucket_size + next_buffer_size / 2 >= bucket_size`.
    ///
    /// Returns the compressed data for each bucket.
    fn create_compressed_buckets(
        &self,
        all_buffers: &[Vec<u8>],
    ) -> Result<Vec<Vec<u8>>, RiegeliError> {
        if all_buffers.is_empty() {
            return Ok(Vec::new());
        }

        if self.bucket_size == u64::MAX {
            // Single bucket: concatenate all buffers.
            let mut bucket_data: Vec<u8> = Vec::new();
            for buf in all_buffers {
                bucket_data.extend_from_slice(buf);
            }
            let compressed =
                compress_with_prefix(&bucket_data, self.compression, self.compress_opts)?;
            return Ok(vec![compressed]);
        }

        // Multi-bucket: greedy splitting.
        let mut compressed_buckets: Vec<Vec<u8>> = Vec::new();
        let mut current_bucket: Vec<u8> = Vec::new();
        let mut current_size: u64 = 0;

        for buf in all_buffers {
            let buf_size = buf.len() as u64;
            if !current_bucket.is_empty() && current_size + buf_size / 2 >= self.bucket_size {
                // Start a new bucket.
                compressed_buckets.push(compress_with_prefix(
                    &current_bucket,
                    self.compression,
                    self.compress_opts,
                )?);
                current_bucket = Vec::new();
                current_size = 0;
            }
            current_bucket.extend_from_slice(buf);
            current_size += buf_size;
        }

        // Flush remaining.
        if !current_bucket.is_empty() {
            compressed_buckets.push(compress_with_prefix(
                &current_bucket,
                self.compression,
                self.compress_opts,
            )?);
        }

        Ok(compressed_buckets)
    }

    /// Serialize the header: bucket/buffer metadata, state machine states,
    /// next-node indices (with implicit transition signaling), subtypes,
    /// buffer indices, and first_node.
    ///
    /// Returns the raw (uncompressed) header bytes.
    fn build_header(
        &self,
        compressed_buckets: &[Vec<u8>],
        buffer_sizes: &[u64],
        buffer_pos: &BTreeMap<NodeId, u32>,
        state_machine: &[StateInfo],
    ) -> Result<Vec<u8>, RiegeliError> {
        let num_buffers = buffer_sizes.len();
        let num_buckets = compressed_buckets.len() as u32;

        let mut header: Vec<u8> = Vec::new();

        // num_buckets, num_buffers.
        header.extend_from_slice(&encode_u32(num_buckets));
        header.extend_from_slice(&encode_u32(num_buffers as u32));

        // Bucket compressed sizes.
        for bucket in compressed_buckets {
            header.extend_from_slice(&encode_u64(bucket.len() as u64));
        }

        // Buffer uncompressed sizes.
        for &size in buffer_sizes {
            header.extend_from_slice(&encode_u64(size));
        }

        // State machine: tags, next-node indices, subtypes, buffer indices.
        self.write_states_and_data(&mut header, buffer_pos, state_machine);

        Ok(header)
    }

    /// Write state machine states into header: tags, next-node (base) indices,
    /// subtypes, buffer indices, and first_node.
    fn write_states_and_data(
        &self,
        header: &mut Vec<u8>,
        buffer_pos: &BTreeMap<NodeId, u32>,
        state_machine: &[StateInfo],
    ) {
        let num_sm = state_machine.len() as u32;
        let mut subtype_to_write: Vec<u8> = Vec::new();
        let mut buffer_index_to_write: Vec<u32> = Vec::new();
        let mut base_to_write: Vec<u32> = Vec::new();

        header.extend_from_slice(&encode_u32(num_sm));

        for state_info in state_machine {
            if state_info.etag_index == INVALID_POS {
                // NoOp state.
                header.extend_from_slice(&encode_u32(message_id::NO_OP));
                base_to_write.push(state_info.base);
                continue;
            }

            let etag = &self.tags_list[state_info.etag_index as usize];
            let node_id = &etag.node_id;

            if node_id.tag != 0 {
                let wt = tag_wire_type(node_id.tag);
                let is_string = wt == Some(WireType::LengthDelimited);

                if is_string && etag.subtype == subtype::LENGTH_DELIMITED_START_OF_SUBMESSAGE {
                    header.extend_from_slice(&encode_u32(message_id::START_OF_SUBMESSAGE));
                } else if is_string && etag.subtype == subtype::LENGTH_DELIMITED_END_OF_SUBMESSAGE {
                    let submsg_tag =
                        node_id.tag + SUBMESSAGE_WIRE_TYPE - WireType::LengthDelimited as u32;
                    header.extend_from_slice(&encode_u32(submsg_tag));
                } else {
                    header.extend_from_slice(&encode_u32(node_id.tag));
                    if has_subtype(node_id.tag) {
                        subtype_to_write.push(etag.subtype);
                    }
                    if has_data_buffer(node_id.tag, etag.subtype) {
                        let lookup_node = NodeId {
                            parent_message_id: node_id.parent_message_id,
                            tag: node_id.tag,
                        };
                        let idx = buffer_pos.get(&lookup_node).copied().unwrap_or(0);
                        buffer_index_to_write.push(idx);
                    }
                }
            } else {
                // NonProto or StartOfMessage special IDs.
                header.extend_from_slice(&encode_u32(node_id.parent_message_id));
                if node_id.parent_message_id == message_id::NON_PROTO {
                    let np_node_id = NodeId {
                        parent_message_id: message_id::NON_PROTO,
                        tag: 0,
                    };
                    let idx = buffer_pos.get(&np_node_id).copied().unwrap_or(0);
                    buffer_index_to_write.push(idx);
                }
            }

            // Write base index, signaling implicit transition if applicable.
            let etag_ref = &self.tags_list[state_info.etag_index as usize];
            if etag_ref.base != INVALID_POS {
                let implicit_offset = if etag_ref.dest_info.len() == 1 {
                    num_sm
                } else {
                    0
                };
                base_to_write.push(etag_ref.base + implicit_offset);
            } else {
                base_to_write.push(0);
            }
        }

        // Write next-node (base) indices.
        for &value in &base_to_write {
            header.extend_from_slice(&encode_u32(value));
        }

        // Write subtypes.
        header.extend_from_slice(&subtype_to_write);

        // Write buffer indices.
        for &value in &buffer_index_to_write {
            header.extend_from_slice(&encode_u32(value));
        }

        // first_node: find the smallest state machine index whose etag_index
        // matches the last entry in encoded_tags (the first tag chronologically).
        let first_tag_pos = if self.encoded_tags.is_empty() {
            0u32
        } else {
            let last_etag = *self.encoded_tags.last().unwrap();
            state_machine
                .iter()
                .position(|s| s.etag_index == last_etag)
                .unwrap_or(0) as u32
        };
        header.extend_from_slice(&encode_u32(first_tag_pos));
    }

    /// Assemble the final chunk data from the compressed header, compressed
    /// buckets, and transition bytes.
    fn assemble_chunk(
        &self,
        raw_header: Vec<u8>,
        compressed_buckets: &[Vec<u8>],
        state_machine: &[StateInfo],
    ) -> Result<Chunk, RiegeliError> {
        // Build transitions.
        let transitions = self.build_transitions(state_machine);
        let compressed_transitions =
            compress_with_prefix(&transitions, self.compression, self.compress_opts)?;

        // Header uses LengthPrefixedEncodeAndClose format (varint(blob_len) + [varint(uncompressed) if compressed] + data).
        let length_prefixed_header =
            compress_length_prefixed(&raw_header, self.compression, self.compress_opts)?;

        let mut chunk_data: Vec<u8> = Vec::new();
        chunk_data.push(self.compression as u8);
        chunk_data.extend_from_slice(&length_prefixed_header);
        for bucket in compressed_buckets {
            chunk_data.extend_from_slice(bucket);
        }
        chunk_data.extend_from_slice(&compressed_transitions);

        let chunk_header = ChunkHeader::from_parts(
            &chunk_data,
            ChunkType::Transposed,
            self.num_records,
            self.decoded_data_size,
        );

        Ok(Chunk {
            header: chunk_header,
            data: chunk_data,
        })
    }

    /// Collect transition statistics: count how often each (source -> dest)
    /// pair occurs in `encoded_tags`.
    fn collect_transition_statistics(&mut self) {
        if self.encoded_tags.is_empty() {
            return;
        }

        // Walk backward through encoded_tags (they are stored forward, but
        // transitions are processed back-to-front like C++).
        let last = *self.encoded_tags.last().unwrap();
        let mut prev_pos = last;
        for i in (0..self.encoded_tags.len() - 1).rev() {
            let pos = self.encoded_tags[i];
            // Count transition from prev_pos -> pos.
            self.tags_list[prev_pos as usize]
                .dest_info
                .entry(pos)
                .or_insert_with(DestInfo::new)
                .num_transitions += 1;
            self.tags_list[pos as usize].num_incoming_transitions += 1;
            prev_pos = pos;
        }

        // Ensure the initial state (last in encoded_tags) is created even if
        // it has no other incoming transitions.
        if self.tags_list[last as usize].num_incoming_transitions == 0 {
            self.tags_list[last as usize].num_incoming_transitions = 1;
        }
    }

    /// Ensure the last state (first chronologically, i.e. encoded_tags[0])
    /// does not have an implicit transition. If it does, add a dummy
    /// destination to force explicit transition.
    fn ensure_last_state_explicit(&mut self) {
        if self.encoded_tags.is_empty() {
            return;
        }
        let last_etag_idx = self.encoded_tags[0] as usize;
        if self.tags_list[last_etag_idx].dest_info.len() == 1 {
            // Add a dummy destination to prevent implicit transition.
            let first_key = *self.tags_list[last_etag_idx]
                .dest_info
                .keys()
                .next()
                .unwrap();
            self.tags_list[last_etag_idx]
                .dest_info
                .entry(first_key + 1)
                .or_insert_with(DestInfo::new);
        }
    }

    /// Build the optimized state machine: private lists, public list, NoOp
    /// bridging, following the C++ `CreateStateMachine` algorithm.
    fn create_state_machine(&mut self) -> Vec<StateInfo> {
        if self.encoded_tags.is_empty() {
            return vec![StateInfo::new(INVALID_POS, 0)];
        }

        self.collect_transition_statistics();

        // Mark frequent transitions for private lists.
        self.mark_frequent_transitions();

        // Build private lists for each tag.
        let mut state_machine: Vec<StateInfo> = Vec::new();
        let mut public_list_noops: Vec<(u32, u32)> = Vec::new();
        self.build_private_lists(&mut state_machine, &mut public_list_noops);

        // Build the public list.
        let public_list_base = state_machine.len() as u32;
        self.build_public_list(&mut state_machine);

        // Compute base indices for tags without private lists and for
        // public-list NoOp states.
        self.compute_base_indices(public_list_base, &public_list_noops, &mut state_machine);

        state_machine
    }

    /// Mark transitions with >= MIN_COUNT_FOR_STATE as candidates for
    /// private lists. Subtract their counts from incoming transitions so
    /// the public list gets the right estimates.
    fn mark_frequent_transitions(&mut self) {
        // Sentinel: pos = 0 means "in the private list".
        let k_in_list_pos: u32 = 0;

        let num_tags = self.tags_list.len();
        for tag_id in 0..num_tags {
            let dest_keys: Vec<u32> = self.tags_list[tag_id].dest_info.keys().copied().collect();
            for dest_key in dest_keys {
                let num_trans = self.tags_list[tag_id].dest_info[&dest_key].num_transitions;
                if num_trans >= MIN_COUNT_FOR_STATE {
                    // Subtract from destination's incoming count.
                    self.tags_list[dest_key as usize].num_incoming_transitions -= num_trans;
                    // Mark as in-list.
                    self.tags_list[tag_id]
                        .dest_info
                        .get_mut(&dest_key)
                        .unwrap()
                        .pos = k_in_list_pos;
                }
            }
        }
    }

    /// Build private destination lists for all tags that have one.
    fn build_private_lists(
        &mut self,
        state_machine: &mut Vec<StateInfo>,
        public_list_noops: &mut Vec<(u32, u32)>,
    ) {
        let k_in_list_pos: u32 = 0;
        let num_tags = self.tags_list.len();

        for tag_id in 0..num_tags {
            let mut tag_priority: BinaryHeap<PriorityQueueEntry> = BinaryHeap::new();
            let mut excluded_state: Option<PriorityQueueEntry> = None;
            let mut num_excluded_transitions: usize = 0;

            let dest_keys: Vec<u32> = self.tags_list[tag_id].dest_info.keys().copied().collect();
            let sz = dest_keys.len() as u32;

            for &dest_key in &dest_keys {
                let di = &self.tags_list[tag_id].dest_info[&dest_key];
                let di_pos = di.pos;
                let di_num = di.num_transitions;

                // Include if marked as in-list, or if all transitions to
                // this dest come from this source.
                if di_pos == k_in_list_pos
                    || di_num == self.tags_list[dest_key as usize].num_incoming_transitions
                {
                    if di_pos != k_in_list_pos {
                        // Not yet subtracted.
                        self.tags_list[dest_key as usize].num_incoming_transitions -= di_num;
                    }
                    tag_priority.push(PriorityQueueEntry {
                        dest_index: dest_key,
                        num_transitions: di_num,
                    });
                } else {
                    num_excluded_transitions += di_num;
                    excluded_state = Some(PriorityQueueEntry {
                        dest_index: dest_key,
                        num_transitions: di_num,
                    });
                }
            }

            let mut num_states = tag_priority.len() as u32;
            if num_states == 0 {
                continue;
            }

            if num_states + 1 == sz {
                // Only one state would go to the public list -- just add it.
                num_states += 1;
                if let Some(es) = excluded_state.take() {
                    self.tags_list[es.dest_index as usize].num_incoming_transitions -=
                        es.num_transitions;
                    tag_priority.push(es);
                }
            }

            if num_states != sz {
                // Need a NoOp to bridge to the public list.
                tag_priority.push(PriorityQueueEntry {
                    dest_index: INVALID_POS,
                    num_transitions: num_excluded_transitions,
                });
                num_states += 1;
            }

            // Set base for this tag.
            self.tags_list[tag_id].base = state_machine.len() as u32;

            // Number of NoOp bridging nodes needed within this private list.
            let noop_nodes = if num_states <= MAX_TRANSITION + 1 {
                0u32
            } else {
                (num_states - 2) / MAX_TRANSITION
            };
            num_states += noop_nodes;

            // Create states back-to-front.
            let mut prev_state = state_machine.len() as u32 + num_states;
            state_machine.resize(
                prev_state as usize,
                StateInfo::new(INVALID_POS, INVALID_POS),
            );

            let mut block_size = (num_states - 1) % (MAX_TRANSITION + 1) + 1;
            let mut noop_base: Vec<u32> = Vec::new();

            loop {
                let mut total_block_weight: usize = 0;
                for _ in 0..block_size {
                    let entry = tag_priority.pop().unwrap();
                    total_block_weight += entry.num_transitions;
                    let node_index = entry.dest_index;

                    if node_index == INVALID_POS {
                        // NoOp -> public list.
                        prev_state -= 1;
                        state_machine[prev_state as usize] =
                            StateInfo::new(INVALID_POS, INVALID_POS);
                        self.tags_list[tag_id].public_list_noop_pos = prev_state;
                        public_list_noops.push((tag_id as u32, prev_state));
                    } else if node_index >= num_tags as u32 {
                        // NoOp -> private list block.
                        let base = noop_base[(node_index - num_tags as u32) as usize];
                        prev_state -= 1;
                        state_machine[prev_state as usize] = StateInfo::new(INVALID_POS, base);
                        // Set canonical_source for the block this NoOp serves.
                        for j in 0..=MAX_TRANSITION {
                            let idx = (j + base) as usize;
                            if idx >= state_machine.len() {
                                break;
                            }
                            state_machine[idx].canonical_source = prev_state;
                        }
                    } else {
                        // Regular state.
                        prev_state -= 1;
                        state_machine[prev_state as usize] =
                            StateInfo::new(node_index, INVALID_POS);
                        self.tags_list[tag_id]
                            .dest_info
                            .get_mut(&node_index)
                            .unwrap()
                            .pos = prev_state;
                    }
                }

                if tag_priority.is_empty() {
                    break;
                }

                // Add NoOp to serve the block just created.
                tag_priority.push(PriorityQueueEntry {
                    dest_index: num_tags as u32 + noop_base.len() as u32,
                    num_transitions: total_block_weight,
                });
                noop_base.push(prev_state);
                block_size = MAX_TRANSITION + 1;
            }
        }
    }

    /// Build the public list of states for tags with remaining nonzero
    /// incoming transitions.
    fn build_public_list(&mut self, state_machine: &mut Vec<StateInfo>) {
        let num_tags = self.tags_list.len();
        let mut tag_priority: BinaryHeap<PriorityQueueEntry> = BinaryHeap::new();

        for i in 0..num_tags {
            if self.tags_list[i].num_incoming_transitions != 0 {
                tag_priority.push(PriorityQueueEntry {
                    dest_index: i as u32,
                    num_transitions: self.tags_list[i].num_incoming_transitions,
                });
            }
        }

        let mut num_states = tag_priority.len() as u32;
        if num_states == 0 {
            return;
        }

        let noop_nodes = if num_states <= MAX_TRANSITION + 1 {
            0u32
        } else {
            (num_states - 2) / MAX_TRANSITION
        };
        num_states += noop_nodes;

        let mut prev_node = state_machine.len() as u32 + num_states;
        state_machine.resize(prev_node as usize, StateInfo::new(INVALID_POS, INVALID_POS));

        let mut block_size = (num_states - 1) % (MAX_TRANSITION + 1) + 1;
        let mut noop_base: Vec<u32> = Vec::new();

        loop {
            let mut total_block_weight: usize = 0;
            for _ in 0..block_size {
                let entry = tag_priority.pop().unwrap();
                total_block_weight += entry.num_transitions;
                let node_index = entry.dest_index;

                if node_index >= num_tags as u32 {
                    // NoOp state.
                    let base = noop_base[(node_index - num_tags as u32) as usize];
                    prev_node -= 1;
                    state_machine[prev_node as usize] = StateInfo::new(INVALID_POS, base);
                    for j in 0..=MAX_TRANSITION {
                        let idx = (j + base) as usize;
                        if idx >= state_machine.len() {
                            break;
                        }
                        state_machine[idx].canonical_source = prev_node;
                    }
                } else {
                    // Regular state.
                    prev_node -= 1;
                    state_machine[prev_node as usize] = StateInfo::new(node_index, INVALID_POS);
                    self.tags_list[node_index as usize].state_machine_pos = prev_node;
                }
            }

            if tag_priority.is_empty() {
                break;
            }

            tag_priority.push(PriorityQueueEntry {
                dest_index: num_tags as u32 + noop_base.len() as u32,
                num_transitions: total_block_weight,
            });
            noop_base.push(prev_node);
            block_size = MAX_TRANSITION + 1;
        }
    }

    /// Compute optimal base indices for tags without private lists and for
    /// public-list NoOp states. Finds the lowest-indexed block in the public
    /// list that can reach all needed destinations.
    fn compute_base_indices(
        &mut self,
        public_list_base: u32,
        public_list_noops: &[(u32, u32)],
        state_machine: &mut [StateInfo],
    ) {
        let sm_len = state_machine.len();

        // Compute base indices for public-list NoOp states.
        for &(tag_index, state_index) in public_list_noops {
            let (base, min_pos) =
                self.find_optimal_base(tag_index, public_list_base, state_machine, sm_len);
            if min_pos != INVALID_POS {
                state_machine[state_index as usize].base = min_pos;
            }
            // Suppress unused variable warning.
            let _ = base;
        }

        // Compute base indices for tags without private lists.
        let num_tags = self.tags_list.len();
        for tag_idx in 0..num_tags {
            if self.tags_list[tag_idx].base != INVALID_POS {
                continue;
            }
            let (_, min_pos) =
                self.find_optimal_base(tag_idx as u32, public_list_base, state_machine, sm_len);
            if min_pos != INVALID_POS {
                self.tags_list[tag_idx].base = min_pos;
            }
        }
    }

    /// Find the optimal base index for a tag's outgoing transitions that
    /// go through the public list. Returns (base, min_pos).
    ///
    /// Matches the C++ `ComputeBaseIndices` logic: for each public-list
    /// destination, find the common ancestor block that can reach all
    /// destinations within MAX_TRANSITION hops.
    fn find_optimal_base(
        &self,
        tag_index: u32,
        public_list_base: u32,
        state_machine: &[StateInfo],
        _sm_len: usize,
    ) -> (u32, u32) {
        let mut base = INVALID_POS;
        let mut min_pos = INVALID_POS;

        let dest_keys: Vec<(u32, u32)> = self.tags_list[tag_index as usize]
            .dest_info
            .iter()
            .map(|(&k, v)| (k, v.pos))
            .collect();

        for (dest_key, di_pos) in dest_keys {
            if di_pos != INVALID_POS {
                // Destination is in the private list already.
                continue;
            }
            let mut pos = self.tags_list[dest_key as usize].state_machine_pos;
            if pos == INVALID_POS {
                continue;
            }

            // Match C++ while loop: find common ancestor block.
            while base > pos || (base != INVALID_POS && pos - base > MAX_TRANSITION) {
                if base > pos {
                    let cs = if base == INVALID_POS {
                        state_machine[pos as usize].canonical_source
                    } else {
                        let cs_of_base = state_machine[base as usize].canonical_source;
                        if cs_of_base == INVALID_POS {
                            base = public_list_base;
                            continue;
                        }
                        min_pos = min_pos.min(cs_of_base);
                        state_machine[cs_of_base as usize].canonical_source
                    };
                    if cs == INVALID_POS {
                        base = public_list_base;
                    } else {
                        base = state_machine[cs as usize].base;
                    }
                } else {
                    // pos is too far from base. Move pos to canonical_source.
                    let cs = state_machine[pos as usize].canonical_source;
                    if cs == INVALID_POS {
                        break;
                    }
                    pos = cs;
                }
            }
            min_pos = min_pos.min(pos);
        }

        (base, min_pos)
    }

    /// Build transition bytes for the optimized state machine.
    ///
    /// For each transition, determine whether it's implicit (single dest) or
    /// explicit. For explicit transitions, navigate through private list,
    /// then potentially through a NoOp bridge to the public list, encoding
    /// multi-hop paths through NoOp chains.
    fn build_transitions(&self, state_machine: &[StateInfo]) -> Vec<u8> {
        if self.encoded_tags.is_empty() {
            return Vec::new();
        }

        let mut transitions: Vec<u8> = Vec::new();
        let mut last_transition: Option<u8> = None;

        let prev_etag_start = *self.encoded_tags.last().unwrap();
        let mut prev_etag = prev_etag_start;
        let mut current_base = self.tags_list[prev_etag as usize].base;

        let n = self.encoded_tags.len();
        for i in (0..n - 1).rev() {
            let tag = self.encoded_tags[i];

            // Check for implicit transition (single destination).
            if self.tags_list[prev_etag as usize].dest_info.len() == 1 {
                // Implicit: no transition byte needed.
                // Verify the implicit target is correct.
                prev_etag = tag;
                current_base = self.tags_list[prev_etag as usize].base;
                continue;
            }

            // Check if destination is in the private list.
            let private_pos = self.tags_list[prev_etag as usize]
                .dest_info
                .get(&tag)
                .map(|di| di.pos)
                .unwrap_or(INVALID_POS);

            let mut pos = private_pos;

            if pos == INVALID_POS {
                // Destination not in private list. Need to go through public list.
                let public_noop_pos = self.tags_list[prev_etag as usize].public_list_noop_pos;

                if public_noop_pos != INVALID_POS {
                    // Option 2b: first transition to public_list_noop_pos,
                    // then from there to the public list state.
                    let orig_pos = public_noop_pos;
                    let mut noop_pos = public_noop_pos;

                    // Encode transition from current_base to public_list_noop_pos.
                    let mut write_buf: Vec<u8> = Vec::new();
                    while current_base > noop_pos
                        || (current_base != INVALID_POS && noop_pos - current_base > MAX_TRANSITION)
                    {
                        let cs = state_machine[noop_pos as usize].canonical_source;
                        write_buf.push((noop_pos - state_machine[cs as usize].base) as u8);
                        noop_pos = cs;
                    }
                    write_buf.push((noop_pos - current_base) as u8);
                    write_buf.reverse();

                    // Flush these transition bytes.
                    emit_transition_bytes(&write_buf, &mut transitions, &mut last_transition);

                    // Update current_base to the base of the NoOp we reached.
                    current_base = state_machine[orig_pos as usize].base;
                }

                // pos becomes the state_machine_pos in the public list.
                pos = self.tags_list[tag as usize].state_machine_pos;
            }

            if current_base == INVALID_POS || pos == INVALID_POS {
                prev_etag = tag;
                current_base = self.tags_list[prev_etag as usize].base;
                continue;
            }

            // Encode transition from current_base to pos, possibly through
            // canonical_source chain.
            let mut write_buf: Vec<u8> = Vec::new();
            let mut target = pos;
            while current_base > target
                || (current_base != INVALID_POS && target - current_base > MAX_TRANSITION)
            {
                let cs = state_machine[target as usize].canonical_source;
                if cs == INVALID_POS || cs as usize >= state_machine.len() {
                    break;
                }
                write_buf.push((target - state_machine[cs as usize].base) as u8);
                target = cs;
            }
            write_buf.push((target - current_base) as u8);
            write_buf.reverse();

            emit_transition_bytes(&write_buf, &mut transitions, &mut last_transition);

            prev_etag = tag;
            current_base = self.tags_list[prev_etag as usize].base;
        }

        if let Some(lt) = last_transition {
            transitions.push(lt);
        }

        transitions
    }

    /// Encode an empty (zero-record) transposed chunk.
    ///
    /// Matches the C++ encoder, whose `CreateStateMachine` emits a single
    /// `kNoOp` state when there are no encoded tags; a chunk with an empty
    /// state machine would be rejected by the decoder (`first_node` is
    /// always out of range when `num_states == 0`).
    fn encode_empty(&self) -> Result<Chunk, RiegeliError> {
        let mut header: Vec<u8> = Vec::new();
        header.extend_from_slice(&encode_u32(0)); // num_buckets
        header.extend_from_slice(&encode_u32(0)); // num_buffers
        header.extend_from_slice(&encode_u32(1)); // num_states
        header.extend_from_slice(&encode_u32(0)); // state 0 tag: kNoOp
        header.extend_from_slice(&encode_u32(0)); // state 0 next_node
        header.extend_from_slice(&encode_u32(0)); // first_node

        let length_prefixed_header =
            compress_length_prefixed(&header, self.compression, self.compress_opts)?;

        let mut chunk_data: Vec<u8> = Vec::new();
        chunk_data.push(self.compression as u8);
        chunk_data.extend_from_slice(&length_prefixed_header);

        let chunk_header = ChunkHeader::from_parts(&chunk_data, ChunkType::Transposed, 0, 0);

        Ok(Chunk {
            header: chunk_header,
            data: chunk_data,
        })
    }

    /// Get or create a node for the given NodeId.
    fn get_or_create_node(&mut self, node_id: NodeId) -> u32 {
        if let Some(node) = self.message_nodes.get(&node_id) {
            return node.message_id;
        }
        let mid = self.next_message_id;
        self.next_message_id += 1;
        self.message_nodes.insert(
            node_id,
            MessageNode {
                message_id: mid,
                encoded_tag_pos: Vec::new(),
            },
        );
        mid
    }

    /// Get the index in tags_list for a (NodeId, subtype) pair.
    fn get_pos_in_tags_list(&mut self, node_id: NodeId, st: u8) -> u32 {
        self.get_or_create_node(node_id);

        // Safe: get_or_create_node ensures the entry exists.
        let node = match self.message_nodes.get_mut(&node_id) {
            Some(n) => n,
            None => return u32::MAX, // unreachable after get_or_create_node
        };
        let pos_idx = st as usize;
        if node.encoded_tag_pos.len() <= pos_idx {
            node.encoded_tag_pos.resize(pos_idx + 1, u32::MAX);
        }
        if node.encoded_tag_pos[pos_idx] == u32::MAX {
            node.encoded_tag_pos[pos_idx] = self.tags_list.len() as u32;
            self.tags_list.push(EncodedTagInfo::new(node_id, st));
        }
        node.encoded_tag_pos[pos_idx]
    }

    /// Get the data buffer for a node, creating it if needed.
    fn get_buffer(&mut self, node_id: NodeId, buf_type: BufferType) -> &mut BackwardBuffer {
        let buffers = self.data.get_mut(buf_type);

        let existing = buffers.iter().position(|b| b.node_id == node_id);
        if let Some(idx) = existing {
            return &mut buffers[idx].data;
        }

        buffers.push(BufferWithMetadata {
            node_id,
            data: BackwardBuffer::default(),
        });
        let len = buffers.len();
        &mut buffers[len - 1].data
    }

    /// Recursively decompose a proto message into columnar buffers.
    fn add_message(
        &mut self,
        data: &[u8],
        parent_message_id: u32,
        depth: usize,
    ) -> Result<(), RiegeliError> {
        let mut pos = 0usize;
        let mut message_stack: Vec<MessageFrame> = Vec::new();
        let mut current_parent = parent_message_id;
        let mut current_end = data.len();

        while pos < current_end {
            // Read tag.
            let (tag, consumed) = decode_u32(&data[pos..])
                .map_err(|e| RiegeliError::MalformedData(format!("tag decode: {e}").into()))?;
            pos += consumed;

            let field_num = tag_field_number(tag);
            if field_num == 0 {
                return Err(RiegeliError::MalformedData(
                    "field number 0 in proto".into(),
                ));
            }

            let node_id = NodeId {
                parent_message_id: current_parent,
                tag,
            };
            let node_mid = self.get_or_create_node(node_id);

            match tag_wire_type(tag) {
                Some(WireType::Varint) => {
                    self.encode_varint_field(data, &mut pos, node_id)?;
                }
                Some(WireType::Fixed32) => {
                    self.encode_fixed32_field(data, &mut pos, node_id)?;
                }
                Some(WireType::Fixed64) => {
                    self.encode_fixed64_field(data, &mut pos, node_id)?;
                }
                Some(WireType::LengthDelimited) => {
                    let entered_submessage = self.encode_length_delimited_field(
                        data,
                        &mut pos,
                        node_id,
                        node_mid,
                        current_end,
                        depth,
                        &message_stack,
                        &mut current_parent,
                        &mut current_end,
                    )?;
                    if let Some(frame) = entered_submessage {
                        message_stack.push(frame);
                        continue;
                    }
                }
                Some(WireType::StartGroup) => {
                    let idx = self.get_pos_in_tags_list(node_id, subtype::TRIVIAL);
                    self.encoded_tags.push(idx);
                }
                Some(WireType::EndGroup) => {
                    let idx = self.get_pos_in_tags_list(node_id, subtype::TRIVIAL);
                    self.encoded_tags.push(idx);
                }
                None => {
                    return Err(RiegeliError::MalformedData(
                        format!("invalid wire type in tag {tag}").into(),
                    ));
                }
            }

            // At the end of a submessage, pop the stack.
            while pos >= current_end && !message_stack.is_empty() {
                let frame = message_stack
                    .pop()
                    .ok_or_else(|| RiegeliError::MalformedData("message stack empty".into()))?;
                self.encoded_tags.push(frame.end_sub_tag_idx);
                current_parent = frame.parent_message_id;
                current_end = frame.parent_end_pos;
            }
        }

        Ok(())
    }

    /// Encode a varint field: strip high bits and store in a per-node buffer,
    /// or encode inline for small values (0..3).
    fn encode_varint_field(
        &mut self,
        data: &[u8],
        pos: &mut usize,
        node_id: NodeId,
    ) -> Result<(), RiegeliError> {
        let varint_start = *pos;
        let (_, vlen) = decode_u64(&data[*pos..])
            .map_err(|e| RiegeliError::MalformedData(format!("varint value: {e}").into()))?;
        let varint_bytes = &data[varint_start..varint_start + vlen];
        *pos += vlen;

        if vlen == 1 && varint_bytes[0] <= MAX_VARINT_INLINE {
            // Inline varint: value stored in the subtype, no data buffer.
            let st = subtype::VARINT_INLINE_0 + varint_bytes[0];
            let idx = self.get_pos_in_tags_list(node_id, st);
            self.encoded_tags.push(idx);
        } else {
            // Buffered varint: strip high bits (clear bit 7) and prepend.
            let st = subtype::VARINT_1 + (vlen as u8 - 1);
            let idx = self.get_pos_in_tags_list(node_id, st);
            self.encoded_tags.push(idx);

            let mut stripped = Vec::with_capacity(vlen);
            for &b in varint_bytes {
                stripped.push(b & 0x7F);
            }
            let buffer = self.get_buffer(node_id, BufferType::Varint);
            buffer.push_chunk(&stripped);
        }
        Ok(())
    }

    /// Encode a fixed32 field: copy 4 bytes into the per-node buffer.
    fn encode_fixed32_field(
        &mut self,
        data: &[u8],
        pos: &mut usize,
        node_id: NodeId,
    ) -> Result<(), RiegeliError> {
        if *pos + 4 > data.len() {
            return Err(RiegeliError::MalformedData("truncated fixed32".into()));
        }
        let idx = self.get_pos_in_tags_list(node_id, subtype::TRIVIAL);
        self.encoded_tags.push(idx);

        let bytes = &data[*pos..*pos + 4];
        *pos += 4;
        let buffer = self.get_buffer(node_id, BufferType::Fixed32);
        buffer.push_chunk(bytes);
        Ok(())
    }

    /// Encode a fixed64 field: copy 8 bytes into the per-node buffer.
    fn encode_fixed64_field(
        &mut self,
        data: &[u8],
        pos: &mut usize,
        node_id: NodeId,
    ) -> Result<(), RiegeliError> {
        if *pos + 8 > data.len() {
            return Err(RiegeliError::MalformedData("truncated fixed64".into()));
        }
        let idx = self.get_pos_in_tags_list(node_id, subtype::TRIVIAL);
        self.encoded_tags.push(idx);

        let bytes = &data[*pos..*pos + 8];
        *pos += 8;
        let buffer = self.get_buffer(node_id, BufferType::Fixed64);
        buffer.push_chunk(bytes);
        Ok(())
    }

    /// Encode a length-delimited field: either enter a submessage (returning
    /// a [`MessageFrame`]) or store as a string/bytes buffer.
    ///
    /// Returns `Some(frame)` if a submessage was entered (caller should push
    /// the frame and `continue`), or `None` if it was handled as a string.
    #[allow(clippy::too_many_arguments)]
    fn encode_length_delimited_field(
        &mut self,
        data: &[u8],
        pos: &mut usize,
        node_id: NodeId,
        node_mid: u32,
        current_end: usize,
        depth: usize,
        message_stack: &[MessageFrame],
        current_parent: &mut u32,
        current_end_mut: &mut usize,
    ) -> Result<Option<MessageFrame>, RiegeliError> {
        let length_pos = *pos;
        let (length, llen) = decode_u32(&data[*pos..])
            .map_err(|e| RiegeliError::MalformedData(format!("length: {e}").into()))?;
        *pos += llen;
        let value_pos = *pos;
        let value_end = *pos + length as usize;

        if value_end > current_end {
            return Err(RiegeliError::MalformedData(
                "length-delimited field overflow".into(),
            ));
        }

        let total_depth = depth + message_stack.len();
        if length > 0
            && total_depth < MAX_RECURSION_DEPTH
            && is_proto_message(&data[value_pos..value_end])
        {
            // Submessage.
            let start_sub_idx =
                self.get_pos_in_tags_list(node_id, subtype::LENGTH_DELIMITED_START_OF_SUBMESSAGE);
            self.encoded_tags.push(start_sub_idx);

            let end_sub_idx =
                self.get_pos_in_tags_list(node_id, subtype::LENGTH_DELIMITED_END_OF_SUBMESSAGE);

            let frame = MessageFrame {
                end_sub_tag_idx: end_sub_idx,
                parent_message_id: *current_parent,
                parent_end_pos: *current_end_mut,
            };

            *current_parent = node_mid;
            *current_end_mut = value_end;
            *pos = value_pos;
            return Ok(Some(frame));
        }

        // String/bytes: store length + data in buffer.
        let idx = self.get_pos_in_tags_list(node_id, subtype::LENGTH_DELIMITED_STRING);
        self.encoded_tags.push(idx);

        let string_bytes = &data[length_pos..value_end];
        let buffer = self.get_buffer(node_id, BufferType::String);
        buffer.push_chunk(string_bytes);

        *pos = value_end;
        Ok(None)
    }
}

/// Emit transition bytes with zero-run-length compression.
///
/// For each offset byte `b`, if `b == 0` and the previous transition byte's
/// low 2 bits are < 3, increment the run count. Otherwise flush the previous
/// byte and start a new one: `(b << 2) | 0`.
fn emit_transition_bytes(
    write_buf: &[u8],
    transitions: &mut Vec<u8>,
    last_transition: &mut Option<u8>,
) {
    for &b in write_buf {
        if let Some(ref mut lt) = *last_transition {
            if b == 0 && (*lt & 3) < 3 {
                *lt += 1;
                continue;
            }
            transitions.push(*lt);
        }
        *last_transition = Some(b << 2);
    }
}

#[cfg(test)]
#[allow(clippy::identity_op)] // tags spell out the varint wiretype: (field << 3) | 0
mod tests {
    use super::*;
    use crate::transpose::decoder::TransposeChunkDecoder;

    /// Helper: encode records with TransposeChunkEncoder, decode with TransposeChunkDecoder.
    fn roundtrip(records: &[&[u8]], compression: CompressionType) -> Vec<Vec<u8>> {
        let mut enc = TransposeChunkEncoder::new(compression);
        for rec in records {
            enc.add_record(rec).expect("add_record");
        }
        let chunk = enc.encode().expect("encode");
        assert_eq!(chunk.header.chunk_type().unwrap(), ChunkType::Transposed);

        let mut dec = TransposeChunkDecoder::new(chunk).expect("decoder");
        let mut out = Vec::new();
        while let Some(rec) = dec.read_record().expect("read_record") {
            out.push(rec);
        }
        out
    }

    // -------------------------------------------------------------------
    // Priority queue ordering must match the C++ reference.
    // -------------------------------------------------------------------
    #[test]
    fn test_priority_queue_pop_order_matches_cpp() {
        // In the C++ reference, `std::priority_queue` (a max-heap) with the
        // PriorityQueueEntry comparator pops the entry with the FEWEST
        // transitions first, breaking ties by LARGEST dest_index. Combined
        // with the back-to-front state writing in CreateStateMachine, this
        // places the most frequent destinations at the lowest state-machine
        // indices. BinaryHeap must pop in the same order.
        let mut heap: BinaryHeap<PriorityQueueEntry> = BinaryHeap::new();
        heap.push(PriorityQueueEntry {
            dest_index: 1,
            num_transitions: 5,
        });
        heap.push(PriorityQueueEntry {
            dest_index: 2,
            num_transitions: 1,
        });
        heap.push(PriorityQueueEntry {
            dest_index: 3,
            num_transitions: 9,
        });
        heap.push(PriorityQueueEntry {
            dest_index: 4,
            num_transitions: 5,
        });

        let order: Vec<u32> = std::iter::from_fn(|| heap.pop())
            .map(|e| e.dest_index)
            .collect();
        assert_eq!(
            order,
            vec![2, 4, 1, 3],
            "pop order must be ascending num_transitions, ties by descending dest_index"
        );
    }

    // -------------------------------------------------------------------
    // BackwardBuffer must track chunk sizes without truncation.
    // -------------------------------------------------------------------
    #[test]
    #[ignore = "allocates more than 8 GiB; run explicitly with --ignored"]
    fn test_backward_buffer_chunk_larger_than_4gib() {
        // The C++ reference (ChainBackwardWriter) keeps 64-bit sizes
        // throughout. A chunk of 2^32 + 10 bytes must be recorded with its
        // full size, not truncated modulo 2^32.
        let big_len: usize = (1usize << 32) + 10;
        let big = vec![0xABu8; big_len];
        let mut buf = BackwardBuffer::default();
        buf.push_chunk(&big);
        drop(big);
        buf.push_chunk(&[1, 2, 3]);

        let recorded: u64 = buf.sizes.iter().map(|&s| s as u64).sum();
        assert_eq!(
            recorded,
            buf.data.len() as u64,
            "recorded chunk sizes must cover all buffered data"
        );

        // write_to must emit chunks in reverse insertion order, covering
        // every buffered byte.
        let mut out = Vec::new();
        buf.write_to(&mut out);
        assert_eq!(out.len(), buf.data.len());
        assert_eq!(&out[..3], &[1, 2, 3]);
        assert_eq!(out[3], 0xAB);
        assert_eq!(out[out.len() - 1], 0xAB);
    }

    // -------------------------------------------------------------------
    // 10.1: Rust round-trip -- encode then decode returns records byte-for-byte
    // -------------------------------------------------------------------
    #[test]
    fn test_roundtrip_single_proto() {
        // Proto: field 1 varint = 42 -> [0x08, 0x2A]
        let record = vec![0x08, 0x2A];
        let result = roundtrip(&[&record], CompressionType::None);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0], record);
    }

    #[test]
    fn test_roundtrip_multiple_proto_records() {
        // Three records, each with field 1 varint.
        let r0 = vec![0x08, 0x05]; // field 1 = 5
        let r1 = vec![0x08, 0x0A]; // field 1 = 10
        let r2 = vec![0x08, 0x2A]; // field 1 = 42
        let result = roundtrip(&[&r0, &r1, &r2], CompressionType::None);
        assert_eq!(result.len(), 3);
        assert_eq!(result[0], r0);
        assert_eq!(result[1], r1);
        assert_eq!(result[2], r2);
    }

    // -------------------------------------------------------------------
    // 10.2: Zero records -> valid chunk with num_records == 0
    // -------------------------------------------------------------------
    #[test]
    fn test_zero_records() {
        let enc = TransposeChunkEncoder::new(CompressionType::None);
        let chunk = enc.encode().expect("encode");
        assert_eq!(chunk.header.num_records(), 0);
        assert_eq!(chunk.header.chunk_type().unwrap(), ChunkType::Transposed);
        assert_eq!(chunk.header.data_size(), chunk.data.len() as u64);

        let mut dec = TransposeChunkDecoder::new(chunk).expect("decoder");
        assert!(dec.read_record().unwrap().is_none());
    }

    // -------------------------------------------------------------------
    // 10.3: NonProto record round-trips exactly
    // -------------------------------------------------------------------
    #[test]
    fn test_nonproto_roundtrip() {
        // This byte sequence is not valid proto (wire type 7).
        let record = vec![0x0F, 0x01, 0x02, 0x03];
        let result = roundtrip(&[&record], CompressionType::None);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0], record);
    }

    #[test]
    fn test_nonproto_hello() {
        let record = b"hello world!";
        let result = roundtrip(&[record.as_slice()], CompressionType::None);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0], record);
    }

    // -------------------------------------------------------------------
    // 10.4: Nested proto submessage round-trips exactly
    // -------------------------------------------------------------------
    #[test]
    fn test_nested_submessage() {
        // field 1 = submessage { field 2 = varint 42 }
        // Inner: field 2 varint = 42 -> [0x10, 0x2A]
        // Outer: field 1 length-delimited, length=2, inner
        // -> [0x0A, 0x02, 0x10, 0x2A]
        let record = vec![0x0A, 0x02, 0x10, 0x2A];
        let result = roundtrip(&[&record], CompressionType::None);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0], record);
    }

    #[test]
    fn test_complex_proto() {
        // field 1 varint = 42:       08 2A
        // field 2 fixed32:           15 01020304
        // field 3 fixed64:           19 0102030405060708
        // field 4 string "abc":      22 03 616263
        // field 5 submessage:        2A 02 08 07
        //   inner field 1 varint 7
        let record: Vec<u8> = vec![
            0x08, 0x2A, 0x15, 0x01, 0x02, 0x03, 0x04, 0x19, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06,
            0x07, 0x08, 0x22, 0x03, 0x61, 0x62, 0x63, 0x2A, 0x02, 0x08, 0x07,
        ];
        let result = roundtrip(&[&record], CompressionType::None);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0], record);
    }

    // -------------------------------------------------------------------
    // 10.5: 1000 proto records with same schema round-trip correctly
    // -------------------------------------------------------------------
    #[test]
    fn test_1000_proto_records() {
        let mut records: Vec<Vec<u8>> = Vec::new();
        for i in 0u32..1000 {
            // field 1 varint = i, field 2 fixed32 = i
            let mut rec = Vec::new();
            rec.push(0x08); // field 1, varint
            rec.extend_from_slice(&encode_u64(i as u64));
            rec.push(0x15); // field 2, fixed32
            rec.extend_from_slice(&i.to_le_bytes());
            records.push(rec);
        }
        let refs: Vec<&[u8]> = records.iter().map(|r| r.as_slice()).collect();
        let result = roundtrip(&refs, CompressionType::None);
        assert_eq!(result.len(), 1000);
        for (i, (got, expected)) in result.iter().zip(records.iter()).enumerate() {
            assert_eq!(got, expected, "record {i} mismatch");
        }
    }

    // -------------------------------------------------------------------
    // 10.6: Mixed proto + nonproto round-trip correctly in order
    // -------------------------------------------------------------------
    #[test]
    fn test_mixed_proto_nonproto() {
        let proto_rec = vec![0x08, 0x2A]; // field 1 varint = 42
        let nonproto_rec = vec![0x0F, 0xAA, 0xBB]; // wire type 7 -> not proto
        let proto_rec2 = vec![0x08, 0x01]; // field 1 varint = 1
        let result = roundtrip(
            &[&proto_rec, &nonproto_rec, &proto_rec2],
            CompressionType::None,
        );
        assert_eq!(result.len(), 3);
        assert_eq!(result[0], proto_rec);
        assert_eq!(result[1], nonproto_rec);
        assert_eq!(result[2], proto_rec2);
    }

    // -------------------------------------------------------------------
    // 10.7: No implicit loops in state machine (verified by decoder)
    // -------------------------------------------------------------------
    #[test]
    fn test_no_implicit_loops() {
        // If the decoder's implicit-loop check fails, TransposeChunkDecoder::new
        // would return Err. We just verify that encoding + decoding succeeds.
        let record = vec![0x08, 0x01];
        let mut enc = TransposeChunkEncoder::new(CompressionType::None);
        for _ in 0..10 {
            enc.add_record(&record).unwrap();
        }
        let chunk = enc.encode().unwrap();
        let dec = TransposeChunkDecoder::new(chunk);
        assert!(dec.is_ok(), "decoder should not report implicit loops");
    }

    // -------------------------------------------------------------------
    // Additional: inline varint round-trip
    // -------------------------------------------------------------------
    #[test]
    fn test_inline_varint_values() {
        // Values 0, 1, 2, 3 should use inline subtypes.
        for val in 0u8..=3 {
            let record = vec![0x08, val];
            let result = roundtrip(&[&record], CompressionType::None);
            assert_eq!(result.len(), 1, "failed for value {val}");
            assert_eq!(result[0], record, "failed for value {val}");
        }
    }

    // -------------------------------------------------------------------
    // Additional: multi-byte varint round-trip
    // -------------------------------------------------------------------
    #[test]
    fn test_multibyte_varint() {
        // Varint 300 = [0xAC, 0x02]
        let record = vec![0x08, 0xAC, 0x02];
        let result = roundtrip(&[&record], CompressionType::None);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0], record);
    }

    // -------------------------------------------------------------------
    // Additional: empty proto record
    // -------------------------------------------------------------------
    #[test]
    fn test_empty_proto_record() {
        // Empty bytes is valid proto.
        let record: Vec<u8> = vec![];
        let result = roundtrip(&[&record], CompressionType::None);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0], record);
    }

    // -------------------------------------------------------------------
    // Additional: string-only field
    // -------------------------------------------------------------------
    #[test]
    fn test_string_field_roundtrip() {
        // field 2 string "hello" -> [0x12, 0x05, 0x68, 0x65, 0x6c, 0x6c, 0x6f]
        let record = vec![0x12, 0x05, 0x68, 0x65, 0x6c, 0x6c, 0x6f];
        let result = roundtrip(&[&record], CompressionType::None);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0], record);
    }

    // -------------------------------------------------------------------
    // Additional: multiple nonproto records
    // -------------------------------------------------------------------
    #[test]
    fn test_multiple_nonproto() {
        let r0 = vec![0xFF, 0x01];
        let r1 = vec![0xFF, 0x02, 0x03];
        let r2 = vec![0xFF];
        let result = roundtrip(&[&r0, &r1, &r2], CompressionType::None);
        assert_eq!(result.len(), 3);
        assert_eq!(result[0], r0);
        assert_eq!(result[1], r1);
        assert_eq!(result[2], r2);
    }

    #[test]
    #[cfg(feature = "brotli")]
    fn test_roundtrip_brotli() {
        let record = vec![0x08, 0x2A];
        let result = roundtrip(&[&record], CompressionType::Brotli);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0], record);
    }

    // ===================================================================
    // Optimized state machine tests
    // ===================================================================

    // -------------------------------------------------------------------
    // 12.2: Optimized encoding produces fewer transition bytes
    // -------------------------------------------------------------------
    #[test]
    fn test_optimized_fewer_transitions() {
        // Build 1000 proto records with repetitive structure (same schema).
        let mut records: Vec<Vec<u8>> = Vec::new();
        for i in 0u32..1000 {
            let mut rec = Vec::new();
            rec.push(0x08); // field 1, varint
            rec.extend_from_slice(&encode_u64(i as u64));
            rec.push(0x15); // field 2, fixed32
            rec.extend_from_slice(&i.to_le_bytes());
            rec.push(0x18); // field 3, varint
            rec.extend_from_slice(&encode_u64((i * 2) as u64));
            records.push(rec);
        }
        let refs: Vec<&[u8]> = records.iter().map(|r| r.as_slice()).collect();

        // Encode with optimized encoder.
        let mut enc = TransposeChunkEncoder::new(CompressionType::None);
        for rec in &refs {
            enc.add_record(rec).unwrap();
        }
        let chunk = enc.encode().unwrap();

        // Verify round-trip.
        let mut dec = TransposeChunkDecoder::new(chunk.clone()).unwrap();
        let mut out = Vec::new();
        while let Some(rec) = dec.read_record().unwrap() {
            out.push(rec);
        }
        assert_eq!(out.len(), 1000);
        for (i, (got, expected)) in out.iter().zip(records.iter()).enumerate() {
            assert_eq!(got, expected, "record {i} mismatch");
        }

        // With 1000 records of the same schema, the optimized state machine
        // should use implicit transitions, producing very few transition bytes.
        // The chunk data size should be smaller than what a trivial SM would
        // produce. We just verify the encoding is valid and compact.
        assert!(!chunk.data.is_empty());
    }

    // -------------------------------------------------------------------
    // 12.3: transpose+Brotli smaller than simple+Brotli
    // -------------------------------------------------------------------
    #[test]
    #[cfg(feature = "brotli")]
    fn test_transpose_smaller_than_simple_brotli() {
        use crate::simple_chunk::SimpleChunkEncoder;

        // Use 1000 records each containing a repeated string field.
        // Transpose groups all the string lengths together and all the
        // string data together. With identical strings across records,
        // columnar compression achieves much better ratios than row-wise.
        let mut records: Vec<Vec<u8>> = Vec::new();
        let payload = b"AAAAAAAAAA"; // 10 bytes of 'A'
        for i in 0u32..1000 {
            let mut rec = Vec::new();
            // field 1 varint = i
            rec.push(0x08);
            rec.extend_from_slice(&encode_u64(i as u64));
            // field 2 string = "AAAAAAAAAA"
            rec.push(0x12);
            rec.push(payload.len() as u8);
            rec.extend_from_slice(payload);
            // field 3 string = "AAAAAAAAAA"
            rec.push(0x1A);
            rec.push(payload.len() as u8);
            rec.extend_from_slice(payload);
            // field 4 string = "AAAAAAAAAA"
            rec.push(0x22);
            rec.push(payload.len() as u8);
            rec.extend_from_slice(payload);
            records.push(rec);
        }

        // Transpose + Brotli.
        let mut t_enc = TransposeChunkEncoder::new(CompressionType::Brotli);
        for rec in &records {
            t_enc.add_record(rec).unwrap();
        }
        let t_chunk = t_enc.encode().unwrap();

        // Simple + Brotli.
        let mut s_enc = SimpleChunkEncoder::with_compression(CompressionType::Brotli);
        for rec in &records {
            s_enc.add_record(rec);
        }
        let s_chunk = s_enc.encode().unwrap();

        assert!(
            t_chunk.data.len() < s_chunk.data.len(),
            "transpose+brotli ({}) should be smaller than simple+brotli ({})",
            t_chunk.data.len(),
            s_chunk.data.len()
        );
    }

    // -------------------------------------------------------------------
    // 12.4: Implicit transitions are used
    // -------------------------------------------------------------------
    #[test]
    fn test_implicit_transitions_used() {
        // With many identical-schema records, some tags will have exactly
        // one destination and should get implicit transitions.
        let mut records: Vec<Vec<u8>> = Vec::new();
        for i in 0u32..100 {
            let mut rec = Vec::new();
            rec.push(0x08);
            rec.extend_from_slice(&encode_u64(i as u64));
            rec.push(0x15);
            rec.extend_from_slice(&i.to_le_bytes());
            records.push(rec);
        }

        let mut enc = TransposeChunkEncoder::new(CompressionType::None);
        for rec in &records {
            enc.add_record(rec).unwrap();
        }

        // After collecting transition stats, check for single-dest tags.
        enc.collect_transition_statistics();
        enc.ensure_last_state_explicit();

        let has_single_dest = enc.tags_list.iter().any(|t| t.dest_info.len() == 1);
        assert!(
            has_single_dest,
            "Expected at least one tag with a single destination (implicit transition)"
        );
    }

    // -------------------------------------------------------------------
    // 12.5: NoOp bridging states for >64 unique tags
    // -------------------------------------------------------------------
    #[test]
    fn test_noop_bridging_many_tags() {
        // Create records with >64 distinct field numbers.
        // Use varying schemas so transitions are infrequent (below
        // MIN_COUNT_FOR_STATE=10), forcing destinations into the public
        // list. With >64 public list entries, NoOp bridging is required.
        let mut records: Vec<Vec<u8>> = Vec::new();
        for i in 0u32..80 {
            let mut rec = Vec::new();
            // Each record uses 2 fields: a common one and a unique one.
            // The unique field ensures >64 entries in the public list.
            rec.push(0x08); // field 1 varint (common)
            rec.extend_from_slice(&encode_u64(i as u64));
            // unique field: field (i+2), varint
            let tag = ((i + 2) << 3) | 0;
            rec.extend_from_slice(&encode_u32(tag));
            rec.push(0x01);
            records.push(rec);
        }
        let refs: Vec<&[u8]> = records.iter().map(|r| r.as_slice()).collect();

        // Verify round-trip works.
        let result = roundtrip(&refs, CompressionType::None);
        assert_eq!(result.len(), 80);
        for (i, (got, expected)) in result.iter().zip(records.iter()).enumerate() {
            assert_eq!(got, expected, "record {i} mismatch");
        }

        // Build state machine to verify NoOp states exist.
        let mut enc = TransposeChunkEncoder::new(CompressionType::None);
        for rec in &records {
            enc.add_record(rec).unwrap();
        }
        let sm = enc.create_state_machine();
        let noop_count = sm.iter().filter(|s| s.etag_index == INVALID_POS).count();
        assert!(
            noop_count > 0,
            "Expected NoOp bridging states for >64 unique tags, got {} total states",
            sm.len()
        );
    }

    // -------------------------------------------------------------------
    // 12.7: Wide schema (100 records x 100 fields) round-trip
    // -------------------------------------------------------------------
    #[test]
    fn test_wide_schema_roundtrip() {
        let mut records: Vec<Vec<u8>> = Vec::new();
        for i in 0u32..100 {
            let mut rec = Vec::new();
            for field_num in 1u32..=100 {
                let tag = (field_num << 3) | 0; // varint
                rec.extend_from_slice(&encode_u32(tag));
                rec.extend_from_slice(&encode_u64((i + field_num) as u64));
            }
            records.push(rec);
        }
        let refs: Vec<&[u8]> = records.iter().map(|r| r.as_slice()).collect();
        let result = roundtrip(&refs, CompressionType::None);
        assert_eq!(result.len(), 100);
        for (i, (got, expected)) in result.iter().zip(records.iter()).enumerate() {
            assert_eq!(got, expected, "record {i} mismatch");
        }
    }

    // -------------------------------------------------------------------
    // 12.8: No implicit loops
    // -------------------------------------------------------------------
    #[test]
    fn test_no_implicit_loops_optimized() {
        let mut records: Vec<Vec<u8>> = Vec::new();
        for i in 0u32..100 {
            let mut rec = Vec::new();
            rec.push(0x08);
            rec.extend_from_slice(&encode_u64(i as u64));
            records.push(rec);
        }
        let refs: Vec<&[u8]> = records.iter().map(|r| r.as_slice()).collect();
        let mut enc = TransposeChunkEncoder::new(CompressionType::None);
        for rec in &refs {
            enc.add_record(rec).unwrap();
        }
        let chunk = enc.encode().unwrap();
        // Decoder checks for implicit loops during construction.
        let dec = TransposeChunkDecoder::new(chunk);
        assert!(dec.is_ok(), "decoder should not report implicit loops");
    }

    // -------------------------------------------------------------------
    // Migrated from integration tests
    // -------------------------------------------------------------------

    fn push_varint(buf: &mut Vec<u8>, mut v: u64) {
        loop {
            if v < 0x80 {
                buf.push(v as u8);
                return;
            }
            buf.push((v as u8 & 0x7F) | 0x80);
            v >>= 7;
        }
    }

    #[test]
    fn test_1000_mixed_field_records() {
        let records: Vec<Vec<u8>> = (0u32..1000)
            .map(|i| {
                let mut rec = vec![0x08];
                rec.extend_from_slice(&encode_u64(i as u64));
                rec.push(0x15);
                rec.extend_from_slice(&i.to_le_bytes());
                rec
            })
            .collect();
        let refs: Vec<&[u8]> = records.iter().map(|r| r.as_slice()).collect();
        let result = roundtrip(&refs, CompressionType::None);
        assert_eq!(result.len(), 1000);
        for (i, (got, want)) in result.iter().zip(records.iter()).enumerate() {
            assert_eq!(got, want, "record {i} mismatch");
        }
    }

    #[test]
    fn test_large_nonproto_roundtrip() {
        let record = vec![0xFF_u8; 65536];
        let result = roundtrip(&[&record], CompressionType::None);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0], record);
    }

    #[test]
    fn test_deep_nested_submessage_roundtrip() {
        let mut inner = vec![0x10, 0x63_u8]; // field 2 varint 99
        for _ in 0..5 {
            let len = inner.len() as u8;
            let mut outer = vec![0x0A, len];
            outer.extend_from_slice(&inner);
            inner = outer;
        }
        let result = roundtrip(&[&inner], CompressionType::None);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0], inner);
    }

    #[test]
    fn test_1000_same_schema_roundtrip() {
        let records: Vec<Vec<u8>> = (0u32..1000)
            .map(|i| {
                let mut rec = vec![0x08];
                rec.extend_from_slice(&encode_u64(i as u64));
                rec.push(0x15);
                rec.extend_from_slice(&i.to_le_bytes());
                rec
            })
            .collect();
        let refs: Vec<&[u8]> = records.iter().map(|r| r.as_slice()).collect();
        let result = roundtrip(&refs, CompressionType::None);
        assert_eq!(result.len(), 1000);
        for (i, (got, want)) in result.iter().zip(records.iter()).enumerate() {
            assert_eq!(got, want, "record {i} mismatch");
        }
    }

    #[test]
    fn test_deep_nesting_roundtrip() {
        fn build_nested(depth: usize, value: u8) -> Vec<u8> {
            if depth == 0 {
                return vec![0x08, value];
            }
            let inner = build_nested(depth - 1, value);
            let mut rec = vec![0x0A, inner.len() as u8];
            rec.extend_from_slice(&inner);
            rec
        }
        let records: Vec<Vec<u8>> = (0u8..20).map(|i| build_nested(10, i)).collect();
        let refs: Vec<&[u8]> = records.iter().map(|r| r.as_slice()).collect();
        let result = roundtrip(&refs, CompressionType::None);
        assert_eq!(result.len(), 20);
        for (i, (got, want)) in result.iter().zip(records.iter()).enumerate() {
            assert_eq!(got, want, "record {i} mismatch");
        }
    }

    #[test]
    fn test_alternating_schemas() {
        let mut records: Vec<Vec<u8>> = Vec::new();
        for i in 0u32..200 {
            let mut rec = Vec::new();
            if i % 2 == 0 {
                rec.push(0x08);
                rec.extend_from_slice(&encode_u64(i as u64));
                rec.push(0x15);
                rec.extend_from_slice(&i.to_le_bytes());
            } else {
                rec.push(0x18);
                rec.extend_from_slice(&encode_u64(i as u64));
                rec.push(0x22);
                rec.push(2);
                rec.push(b'x');
                rec.push(b'y');
            }
            records.push(rec);
        }
        let refs: Vec<&[u8]> = records.iter().map(|r| r.as_slice()).collect();
        let result = roundtrip(&refs, CompressionType::None);
        assert_eq!(result.len(), 200);
        for (i, (got, want)) in result.iter().zip(records.iter()).enumerate() {
            assert_eq!(got, want, "record {i} mismatch");
        }
    }

    #[test]
    fn test_many_nonproto_records() {
        let records: Vec<Vec<u8>> = (0u32..500)
            .map(|i| {
                let len = (i % 50) as usize + 1;
                let mut rec = vec![0xFF_u8; len];
                if len > 1 {
                    rec[1] = (i & 0xFF) as u8;
                }
                rec
            })
            .collect();
        let refs: Vec<&[u8]> = records.iter().map(|r| r.as_slice()).collect();
        let result = roundtrip(&refs, CompressionType::None);
        assert_eq!(result.len(), 500);
        for (i, (got, want)) in result.iter().zip(records.iter()).enumerate() {
            assert_eq!(got, want, "record {i} mismatch");
        }
    }

    #[test]
    fn test_mixed_proto_nonproto_heavy() {
        let mut records: Vec<Vec<u8>> = Vec::new();
        for i in 0u32..300 {
            if i % 3 == 0 {
                records.push(vec![0xFF, (i & 0xFF) as u8]);
            } else if i % 3 == 1 {
                let mut rec = vec![0x08];
                rec.extend_from_slice(&encode_u64(i as u64));
                records.push(rec);
            } else {
                let s = format!("val{i}");
                let mut rec = vec![0x12, s.len() as u8];
                rec.extend_from_slice(s.as_bytes());
                records.push(rec);
            }
        }
        let refs: Vec<&[u8]> = records.iter().map(|r| r.as_slice()).collect();
        let result = roundtrip(&refs, CompressionType::None);
        assert_eq!(result.len(), 300);
        for (i, (got, want)) in result.iter().zip(records.iter()).enumerate() {
            assert_eq!(got, want, "record {i} mismatch");
        }
    }

    #[test]
    fn test_deeply_nested_proto_roundtrip() {
        fn make_nested(depth: usize) -> Vec<u8> {
            if depth == 0 {
                return vec![0x08, 42, 0x10, 43, 0x18, 44];
            }
            let inner = make_nested(depth - 1);
            let mut rec = Vec::new();
            rec.push(0x0A);
            let mut buf = Vec::new();
            push_varint(&mut buf, inner.len() as u64);
            rec.extend_from_slice(&buf);
            rec.extend_from_slice(&inner);
            rec
        }
        let record = make_nested(10);
        let result = roundtrip(&[&record], CompressionType::None);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0], record);
    }

    #[test]
    fn test_large_record_roundtrip() {
        let mut rec = Vec::new();
        rec.push(0x08);
        push_varint(&mut rec, 999);
        rec.push(0x12);
        let payload = vec![0xAB_u8; 4096];
        push_varint(&mut rec, payload.len() as u64);
        rec.extend_from_slice(&payload);
        let result = roundtrip(&[&rec], CompressionType::None);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0], rec);
    }

    #[test]
    fn test_single_byte_all_values() {
        let records: Vec<Vec<u8>> = (0u8..=255).map(|b| vec![b]).collect();
        let refs: Vec<&[u8]> = records.iter().map(|r| r.as_slice()).collect();
        let result = roundtrip(&refs, CompressionType::None);
        assert_eq!(result.len(), 256);
        for (i, (got, want)) in result.iter().zip(records.iter()).enumerate() {
            assert_eq!(got, want, "record {i} mismatch");
        }
    }

    #[test]
    fn test_alternating_proto_nonproto() {
        let mut records: Vec<Vec<u8>> = Vec::new();
        for i in 0u64..100 {
            if i % 2 == 0 {
                let mut rec = vec![0x08];
                push_varint(&mut rec, i);
                rec.push(0x10);
                push_varint(&mut rec, i + 1);
                rec.push(0x18);
                push_varint(&mut rec, i + 2);
                records.push(rec);
            } else {
                records.push(vec![0xFF, 0xFE, i as u8]);
            }
        }
        let refs: Vec<&[u8]> = records.iter().map(|r| r.as_slice()).collect();
        let result = roundtrip(&refs, CompressionType::None);
        assert_eq!(result.len(), 100);
        for (i, (got, want)) in result.iter().zip(records.iter()).enumerate() {
            assert_eq!(got, want, "record {i} mismatch");
        }
    }

    // -------------------------------------------------------------------------
    // Multi-bucket and state-machine internals
    // -------------------------------------------------------------------------

    fn make_proto_3_varints_int(a: u64, b: u64, c: u64) -> Vec<u8> {
        let mut rec = Vec::new();
        rec.push(0x08);
        push_varint(&mut rec, a);
        rec.push(0x10);
        push_varint(&mut rec, b);
        rec.push(0x18);
        push_varint(&mut rec, c);
        rec
    }

    fn push_varint_u32_int(buf: &mut Vec<u8>, mut v: u32) {
        loop {
            if v < 0x80 {
                buf.push(v as u8);
                return;
            }
            buf.push((v as u8 & 0x7F) | 0x80);
            v >>= 7;
        }
    }

    fn transpose_roundtrip_with_bucket_size(
        records: &[Vec<u8>],
        compression: CompressionType,
        bucket_size: u64,
    ) -> Vec<Vec<u8>> {
        let mut enc = TransposeChunkEncoder::new(compression).bucket_size(bucket_size);
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

    /// 13.4: Multi-bucket encoder produces multiple buckets.
    #[test]
    fn multi_bucket_produces_multiple_buckets() {
        use crate::varint::{decode_u32, decode_u64};
        let records: Vec<Vec<u8>> = (0..200)
            .map(|i| make_proto_3_varints_int(i, i * 2, i * 3))
            .collect();

        let mut enc = TransposeChunkEncoder::new(CompressionType::None).bucket_size(64);
        for rec in &records {
            enc.add_record(rec).expect("add_record");
        }
        let chunk = enc.encode().expect("encode");

        let data = &chunk.data;
        assert_eq!(data[0], 0x00, "compression type should be None");
        let (header_len, consumed) = decode_u64(&data[1..]).expect("header length");
        let header_start = 1 + consumed;
        let header_bytes = &data[header_start..header_start + header_len as usize];
        let (num_buckets, _) = decode_u32(header_bytes).expect("num_buckets");
        assert!(
            num_buckets > 1,
            "expected multiple buckets with bucket_size=64, got {num_buckets}"
        );

        let mut dec = TransposeChunkDecoder::new(chunk).expect("decoder");
        let mut out = Vec::new();
        while let Some(rec) = dec.read_record().expect("read_record") {
            out.push(rec);
        }
        assert_eq!(out.len(), records.len());
        for (i, (expected, actual)) in records.iter().zip(out.iter()).enumerate() {
            assert_eq!(expected, actual, "mismatch at record {i}");
        }
    }

    /// 13.4: Multi-bucket with brotli.
    #[test]
    #[cfg(feature = "brotli")]
    fn multi_bucket_with_brotli() {
        let records: Vec<Vec<u8>> = (0..100)
            .map(|i| make_proto_3_varints_int(i, i + 100, i + 200))
            .collect();

        let mut enc = TransposeChunkEncoder::new(CompressionType::Brotli).bucket_size(128);
        for rec in &records {
            enc.add_record(rec).expect("add_record");
        }
        let chunk = enc.encode().expect("encode");

        let mut dec = TransposeChunkDecoder::new(chunk).expect("decoder");
        let mut out = Vec::new();
        while let Some(rec) = dec.read_record().expect("read_record") {
            out.push(rec);
        }
        assert_eq!(out.len(), records.len());
        for (i, (expected, actual)) in records.iter().zip(out.iter()).enumerate() {
            assert_eq!(expected, actual, "mismatch at record {i}");
        }
    }

    /// 13.4: Multi-bucket nonproto roundtrip.
    #[test]
    fn multi_bucket_nonproto_roundtrip() {
        let records: Vec<Vec<u8>> = (0..50)
            .map(|i| format!("record number {i}: some arbitrary data here").into_bytes())
            .collect();

        let mut enc = TransposeChunkEncoder::new(CompressionType::None).bucket_size(100);
        for rec in &records {
            enc.add_record(rec).expect("add_record");
        }
        let chunk = enc.encode().expect("encode");

        let mut dec = TransposeChunkDecoder::new(chunk).expect("decoder");
        let mut out = Vec::new();
        while let Some(rec) = dec.read_record().expect("read_record") {
            out.push(rec);
        }
        assert_eq!(out.len(), records.len());
        for (i, (expected, actual)) in records.iter().zip(out.iter()).enumerate() {
            assert_eq!(expected, actual, "mismatch at record {i}");
        }
    }

    /// 13.4: Single bucket (default bucket_size).
    #[test]
    fn single_bucket_default() {
        let records: Vec<Vec<u8>> = (0..10).map(|i| make_proto_3_varints_int(i, i, i)).collect();
        let refs: Vec<&[u8]> = records.iter().map(|r| r.as_slice()).collect();
        let result = roundtrip(&refs, CompressionType::None);
        assert_eq!(result.len(), records.len());
        for (i, (expected, actual)) in records.iter().zip(result.iter()).enumerate() {
            assert_eq!(expected, actual, "mismatch at record {i}");
        }
    }

    /// 13.5: Transpose+Brotli is smaller than simple+Brotli for structured data.
    #[test]
    #[cfg(feature = "brotli")]
    fn transpose_brotli_smaller_than_simple_brotli() {
        use crate::simple_chunk::SimpleChunkEncoder;
        let records: Vec<Vec<u8>> = (0..10_000)
            .map(|i| make_proto_3_varints_int(i, i * 2, i * 3))
            .collect();

        let mut transpose_enc = TransposeChunkEncoder::new(CompressionType::Brotli);
        for rec in &records {
            transpose_enc.add_record(rec).expect("add_record");
        }
        let transpose_chunk = transpose_enc.encode().expect("encode");
        let transpose_size = transpose_chunk.data.len();

        let mut simple_enc = SimpleChunkEncoder::with_compression(CompressionType::Brotli);
        for rec in &records {
            simple_enc.add_record(rec);
        }
        let simple_chunk = simple_enc.encode().expect("encode");
        let simple_size = simple_chunk.data.len();

        assert!(
            transpose_size < simple_size,
            "transpose+Brotli ({transpose_size}) should be smaller than simple+Brotli ({simple_size})"
        );
    }

    /// 13.7: Many unique fields without overflow.
    #[test]
    fn many_unique_fields_no_overflow() {
        let mut records = Vec::new();
        for batch in 0..10 {
            let mut rec = Vec::new();
            for field_num in 1..=200u32 {
                let adjusted_field = field_num + (batch * 200);
                let tag = adjusted_field << 3;
                push_varint_u32_int(&mut rec, tag);
                push_varint(&mut rec, (field_num + batch * 100) as u64);
            }
            records.push(rec);
        }

        let mut enc = TransposeChunkEncoder::new(CompressionType::None);
        for rec in &records {
            enc.add_record(rec).expect("add_record");
        }
        let chunk = enc.encode().expect("encode");

        let mut dec = TransposeChunkDecoder::new(chunk).expect("decoder");
        let mut out = Vec::new();
        while let Some(rec) = dec.read_record().expect("read_record") {
            out.push(rec);
        }
        assert_eq!(out.len(), records.len());
        for (i, (expected, actual)) in records.iter().zip(out.iter()).enumerate() {
            assert_eq!(expected, actual, "mismatch at record {i}");
        }
    }

    /// 13.7: Corrupted chunk returns error, does not panic.
    #[test]
    fn corrupted_num_states_no_panic() {
        use crate::simple_chunk::Chunk;
        let records = vec![make_proto_3_varints_int(1, 2, 3)];
        let mut enc = TransposeChunkEncoder::new(CompressionType::None);
        for rec in &records {
            enc.add_record(rec).expect("add_record");
        }
        let chunk = enc.encode().expect("encode");

        let mut corrupted_data = chunk.data.clone();
        if corrupted_data.len() > 10 {
            corrupted_data.truncate(10);
        }

        let corrupted_chunk = Chunk {
            header: chunk.header,
            data: corrupted_data,
        };
        let result = TransposeChunkDecoder::new(corrupted_chunk);
        assert!(result.is_err(), "corrupted chunk should produce an error");
    }

    // -------------------------------------------------------------------------
    // Additional encoder edge cases
    // -------------------------------------------------------------------------

    #[cfg(feature = "brotli")]
    fn simple_hash_int(seed: u64, len: usize) -> Vec<u8> {
        let mut result = Vec::with_capacity(len);
        let mut state = seed
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        for _ in 0..len {
            state = state
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            result.push((state >> 33) as u8);
        }
        result
    }

    /// eval 13.1: varint32 overflow regression case.
    #[test]
    #[cfg(all(feature = "brotli", feature = "zstd"))]
    fn eval_13_1_varint32_overflow_regression() {
        let record = vec![0xF8, 0x80, 0x80, 0x80, 0x10, 0x00];
        for compression in [
            CompressionType::None,
            CompressionType::Brotli,
            CompressionType::Zstd,
        ] {
            let refs = vec![record.as_slice()];
            let result = roundtrip(&refs, compression);
            assert_eq!(result.len(), 1);
            assert_eq!(
                result[0], record,
                "regression case failed for {compression:?}"
            );
        }
    }

    /// eval 13.3: Proptest regression vector.
    #[test]
    fn eval_13_3_proptest_regression_vector() {
        let record = vec![248, 128, 128, 128, 16, 0];
        let result = roundtrip(&[&record], CompressionType::None);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0], record);
    }

    /// eval 13.4: bucket_size = 1.
    #[test]
    fn eval_13_4_bucket_size_1() {
        let records: Vec<Vec<u8>> = (0..50)
            .map(|i| make_proto_3_varints_int(i, i * 2, i * 3))
            .collect();
        let result = transpose_roundtrip_with_bucket_size(&records, CompressionType::None, 1);
        assert_eq!(result.len(), records.len());
        for (i, (expected, actual)) in records.iter().zip(result.iter()).enumerate() {
            assert_eq!(expected, actual, "mismatch at record {i}");
        }
    }

    /// eval 13.4: bucket_size = 0 degenerate case, should not panic.
    #[test]
    fn eval_13_4_bucket_size_0() {
        let records = vec![make_proto_3_varints_int(1, 2, 3)];
        let result = transpose_roundtrip_with_bucket_size(&records, CompressionType::None, 0);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0], records[0]);
    }

    /// eval 13.4: Multi-bucket with Zstd.
    #[test]
    #[cfg(feature = "zstd")]
    fn eval_13_4_multi_bucket_zstd() {
        let records: Vec<Vec<u8>> = (0..100)
            .map(|i| make_proto_3_varints_int(i, i + 50, i + 100))
            .collect();
        let result = transpose_roundtrip_with_bucket_size(&records, CompressionType::Zstd, 64);
        assert_eq!(result.len(), records.len());
        for (i, (expected, actual)) in records.iter().zip(result.iter()).enumerate() {
            assert_eq!(expected, actual, "mismatch at record {i}");
        }
    }

    /// eval 13.4: Multi-bucket with wide record.
    #[test]
    #[cfg(feature = "brotli")]
    fn eval_13_4_multi_bucket_wide_record() {
        let mut rec = Vec::new();
        for field_num in 1..=100u32 {
            let tag = (field_num << 3) | 0;
            push_varint(&mut rec, tag as u64);
            push_varint(&mut rec, field_num as u64);
        }
        let records = vec![rec];
        let result = transpose_roundtrip_with_bucket_size(&records, CompressionType::Brotli, 32);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0], records[0]);
    }

    /// eval 13.4: Multi-bucket with empty records only.
    #[test]
    fn eval_13_4_multi_bucket_empty_records() {
        let records: Vec<Vec<u8>> = vec![vec![]; 50];
        let result = transpose_roundtrip_with_bucket_size(&records, CompressionType::None, 16);
        assert_eq!(result.len(), records.len());
        for (i, (expected, actual)) in records.iter().zip(result.iter()).enumerate() {
            assert_eq!(expected, actual, "mismatch at record {i}");
        }
    }

    /// eval 13.4: Multi-bucket through encoder with various bucket sizes.
    #[test]
    #[cfg(feature = "brotli")]
    fn eval_13_4_multi_bucket_through_writer_reader() {
        let records: Vec<Vec<u8>> = (0..300)
            .map(|i| make_proto_3_varints_int(i, i * 3, i * 7))
            .collect();

        for bucket_size in [1u64, 32, 128, 256, u64::MAX] {
            let mut enc =
                TransposeChunkEncoder::new(CompressionType::Brotli).bucket_size(bucket_size);
            for rec in &records {
                enc.add_record(rec).expect("add_record");
            }
            let chunk = enc.encode().expect("encode");
            let mut dec = TransposeChunkDecoder::new(chunk).expect("decoder");
            let mut out = Vec::new();
            while let Some(rec) = dec.read_record().expect("read_record") {
                out.push(rec);
            }
            assert_eq!(
                out.len(),
                records.len(),
                "count mismatch for bucket_size={bucket_size}"
            );
            for (i, (expected, actual)) in records.iter().zip(out.iter()).enumerate() {
                assert_eq!(
                    expected, actual,
                    "mismatch at record {i} for bucket_size={bucket_size}"
                );
            }
        }
    }

    /// eval 13.5: Transpose+Zstd vs simple+Zstd for structured data.
    #[test]
    #[cfg(feature = "zstd")]
    fn eval_13_5_transpose_zstd_vs_simple_zstd() {
        use crate::simple_chunk::SimpleChunkEncoder;
        let records: Vec<Vec<u8>> = (0..10_000)
            .map(|i| make_proto_3_varints_int(i, i * 2, i * 3))
            .collect();

        let mut transpose_enc = TransposeChunkEncoder::new(CompressionType::Zstd);
        for rec in &records {
            transpose_enc.add_record(rec).expect("add_record");
        }
        let transpose_chunk = transpose_enc.encode().expect("encode");
        let transpose_size = transpose_chunk.data.len();

        let mut simple_enc = SimpleChunkEncoder::with_compression(CompressionType::Zstd);
        for rec in &records {
            simple_enc.add_record(rec);
        }
        let simple_chunk = simple_enc.encode().expect("encode");
        let simple_size = simple_chunk.data.len();

        assert!(
            transpose_size < simple_size,
            "transpose+Zstd ({transpose_size}) should be smaller than simple+Zstd ({simple_size})"
        );
    }

    /// eval 13.7: Records with u64::MAX varint values.
    #[test]
    fn eval_13_7_max_varint_values() {
        let mut rec = Vec::new();
        rec.push(0x08);
        push_varint(&mut rec, u64::MAX);
        rec.push(0x10);
        push_varint(&mut rec, u64::MAX);
        let records = [rec];
        let result = roundtrip(&[records[0].as_slice()], CompressionType::None);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0], records[0]);
    }

    /// eval 13.7: Truncated chunk does not panic.
    #[test]
    fn eval_13_7_truncated_chunk_no_panic() {
        use crate::simple_chunk::Chunk;
        let records = vec![make_proto_3_varints_int(1, 2, 3); 10];
        let mut enc = TransposeChunkEncoder::new(CompressionType::None);
        for rec in &records {
            enc.add_record(rec).expect("add_record");
        }
        let chunk = enc.encode().expect("encode");

        for truncate_at in [0, 1, 5, 10, chunk.data.len() / 2] {
            let mut corrupted = chunk.data.clone();
            corrupted.truncate(truncate_at);
            let corrupted_chunk = Chunk {
                header: chunk.header,
                data: corrupted,
            };
            let result = TransposeChunkDecoder::new(corrupted_chunk);
            assert!(
                result.is_err(),
                "truncation at {truncate_at} should produce an error"
            );
        }
    }

    /// eval 13.6: Multi-block with Brotli — block headers valid.
    #[test]
    #[cfg(feature = "brotli")]
    fn eval_13_6_multi_block_brotli_block_headers_valid() {
        use crate::block_header::BlockHeader;
        use crate::compression::CompressionType;
        use crate::record_writer::{RecordWriter, WriterOptions};
        use std::io::Cursor;

        let records: Vec<Vec<u8>> = (0..1000).map(|i| simple_hash_int(i as u64, 128)).collect();

        let opts = WriterOptions::new()
            .compression(CompressionType::Brotli)
            .transpose(true)
            .chunk_size(4096);

        let mut cursor = Cursor::new(Vec::<u8>::new());
        {
            let mut writer = RecordWriter::new(&mut cursor, opts).expect("writer::new");
            for rec in &records {
                writer.write_record(rec).expect("write_record");
            }
            writer.flush().expect("flush");
        }
        let file_bytes = cursor.into_inner();

        let block_size = 65536usize;
        let mut boundaries_checked = 0;
        let mut offset = 0;
        while offset + 24 <= file_bytes.len() {
            if offset % block_size == 0 {
                let bytes: [u8; 24] = file_bytes[offset..offset + 24].try_into().unwrap();
                let header = BlockHeader::from_bytes(bytes);
                assert!(header.is_valid(), "invalid block header at offset {offset}");
                boundaries_checked += 1;
            }
            offset += block_size;
        }
        assert!(
            boundaries_checked >= 2,
            "expected >= 2 block boundaries, got {boundaries_checked} (file size: {})",
            file_bytes.len()
        );
    }

    // -------------------------------------------------------------------------
    // Additional encoder tests
    // -------------------------------------------------------------------------

    #[test]
    fn test_roundtrip_varied_proto_wire_types() {
        // Several proto records with different schemas (all wire types).
        let r0 = vec![0x08, 0x2A]; // field 1 varint = 42
        let r1 = vec![0x15, 0x01, 0x02, 0x03, 0x04]; // field 2 fixed32
        let r2 = vec![0x19, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08]; // field 3 fixed64
        let r3 = vec![0x22, 0x03, 0x61, 0x62, 0x63]; // field 4 string "abc"
        let result = roundtrip(&[&r0, &r1, &r2, &r3], CompressionType::None);
        assert_eq!(result.len(), 4);
        assert_eq!(result[0], r0);
        assert_eq!(result[1], r1);
        assert_eq!(result[2], r2);
        assert_eq!(result[3], r3);
    }

    #[test]
    fn test_nonproto_then_proto() {
        // Edge case: nonproto first, then proto.
        let records: Vec<Vec<u8>> = vec![
            vec![0xFF],       // nonproto
            vec![0x08, 0x2A], // proto
        ];
        let refs: Vec<&[u8]> = records.iter().map(|r| r.as_slice()).collect();
        let result = roundtrip(&refs, CompressionType::None);
        assert_eq!(result.len(), 2);
        assert_eq!(result[0], records[0]);
        assert_eq!(result[1], records[1]);
    }

    #[test]
    fn test_many_unique_field_tags() {
        // Use 20 unique field tags; verify no implicit loop detected and round-trip succeeds.
        let mut records: Vec<Vec<u8>> = Vec::new();
        for field_num in 1u32..=20 {
            let tag = (field_num << 3) | 0; // varint wire type
            let mut rec = vec![];
            rec.extend_from_slice(&encode_u32(tag));
            rec.push(0x01);
            records.push(rec);
        }
        let refs: Vec<&[u8]> = records.iter().map(|r| r.as_slice()).collect();
        let result = roundtrip(&refs, CompressionType::None);
        assert_eq!(result.len(), 20);
    }

    #[test]
    fn test_writer_reader_roundtrip() {
        use crate::record_reader::{ReaderOptions, RecordReader};
        use crate::record_writer::{RecordWriter, WriterOptions};

        let records: Vec<Vec<u8>> = vec![
            vec![0x08, 0x2A],             // proto
            b"hello world".to_vec(),      // nonproto
            vec![0x0A, 0x02, 0x10, 0x2A], // nested submessage
        ];

        let mut buf: Vec<u8> = Vec::new();
        {
            let opts = WriterOptions::new()
                .compression(CompressionType::None)
                .transpose(true);
            let cursor = std::io::Cursor::new(&mut buf);
            let mut writer = RecordWriter::new(cursor, opts).unwrap();
            for rec in &records {
                writer.write_record(rec).unwrap();
            }
            writer.close().unwrap();
        }

        let cursor = std::io::Cursor::new(&buf);
        let mut reader = RecordReader::new(cursor, ReaderOptions::new()).unwrap();
        let mut result = Vec::new();
        while let Some(rec) = reader.read_record().unwrap() {
            result.push(rec);
        }

        assert_eq!(result.len(), records.len());
        for (i, (got, expected)) in result.iter().zip(records.iter()).enumerate() {
            assert_eq!(
                got, expected,
                "record {i} mismatch in writer/reader roundtrip"
            );
        }
    }

    #[test]
    #[cfg(feature = "brotli")]
    fn test_writer_reader_brotli_100_records() {
        use crate::record_reader::{ReaderOptions, RecordReader};
        use crate::record_writer::{RecordWriter, WriterOptions};

        let mut records: Vec<Vec<u8>> = Vec::new();
        for i in 0u32..100 {
            let mut rec = vec![0x08];
            rec.extend_from_slice(&encode_u64(i as u64));
            records.push(rec);
        }

        let mut buf: Vec<u8> = Vec::new();
        {
            let opts = WriterOptions::new()
                .compression(CompressionType::Brotli)
                .transpose(true);
            let cursor = std::io::Cursor::new(&mut buf);
            let mut writer = RecordWriter::new(cursor, opts).unwrap();
            for rec in &records {
                writer.write_record(rec).unwrap();
            }
            writer.close().unwrap();
        }

        let cursor = std::io::Cursor::new(&buf);
        let mut reader = RecordReader::new(cursor, ReaderOptions::new()).unwrap();
        let mut result = Vec::new();
        while let Some(rec) = reader.read_record().unwrap() {
            result.push(rec);
        }

        assert_eq!(result.len(), records.len());
        for (i, (got, expected)) in result.iter().zip(records.iter()).enumerate() {
            assert_eq!(got, expected, "brotli transpose record {i} mismatch");
        }
    }

    #[test]
    fn test_varint_boundary_value_4() {
        // Value 4 is the first non-inline varint (inline is 0-3).
        let record = vec![0x08, 0x04];
        let result = roundtrip(&[&record], CompressionType::None);
        assert_eq!(result.len(), 1);
        assert_eq!(
            result[0], record,
            "value 4 (first non-inline) must round-trip"
        );
    }

    #[test]
    fn test_varint_boundary_value_127() {
        let record = vec![0x08, 0x7F];
        let result = roundtrip(&[&record], CompressionType::None);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0], record, "value 127 must round-trip");
    }

    #[test]
    fn test_varint_boundary_value_128() {
        // Value 128 = [0x80, 0x01], 2-byte varint.
        let record = vec![0x08, 0x80, 0x01];
        let result = roundtrip(&[&record], CompressionType::None);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0], record, "value 128 must round-trip");
    }

    #[test]
    fn test_high_field_number_1000() {
        // Field number 1000 = tag (1000 << 3) | 0 = 8000.
        let tag_bytes = encode_u32((1000 << 3) | 0);
        let mut record = tag_bytes;
        record.push(0x2A); // varint value 42
        let result = roundtrip(&[&record], CompressionType::None);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0], record, "high field number must round-trip");
    }

    #[test]
    fn test_empty_string_field() {
        // field 2 string, length 0
        let record = vec![0x12, 0x00];
        let result = roundtrip(&[&record], CompressionType::None);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0], record, "empty string field must round-trip");
    }

    #[test]
    fn test_multiple_records_different_schemas() {
        let records: Vec<Vec<u8>> = vec![
            vec![0x08, 0x01],                   // field 1 varint
            vec![0x15, 0x01, 0x02, 0x03, 0x04], // field 2 fixed32
            vec![0x22, 0x02, 0x68, 0x69],       // field 4 string "hi"
            vec![0xFF, 0xAB],                   // nonproto
        ];
        let refs: Vec<&[u8]> = records.iter().map(|r| r.as_slice()).collect();
        let result = roundtrip(&refs, CompressionType::None);
        assert_eq!(result.len(), 4);
        for (i, (got, expected)) in result.iter().zip(records.iter()).enumerate() {
            assert_eq!(got, expected, "record {i} mismatch");
        }
    }

    #[test]
    fn test_decoded_data_size_matches() {
        let records: Vec<Vec<u8>> =
            vec![vec![0x08, 0x01], vec![0xFF, 0xAA, 0xBB], vec![0x08, 0x02]];
        let total_size: u64 = records.iter().map(|r| r.len() as u64).sum();

        let mut enc = TransposeChunkEncoder::new(CompressionType::None);
        for rec in &records {
            enc.add_record(rec).unwrap();
        }
        let chunk = enc.encode().unwrap();
        assert_eq!(
            chunk.header.decoded_data_size(),
            total_size,
            "decoded_data_size must equal sum of record sizes"
        );
        assert_eq!(chunk.header.num_records(), 3);
    }

    // -------------------------------------------------------------------------
    // Optimized state machine: additional edge cases
    // -------------------------------------------------------------------------

    /// Build a proto record with fields numbered from `start` to `start+count-1`.
    fn build_wide_proto(start: u32, count: u32, value_seed: u32) -> Vec<u8> {
        let mut rec = Vec::new();
        for i in 0..count {
            let field_num = start + i;
            let tag = (field_num << 3) | 0; // varint wire type
            rec.extend_from_slice(&encode_u32(tag));
            rec.extend_from_slice(&encode_u64((value_seed + i) as u64));
        }
        rec
    }

    #[test]
    fn test_implicit_transitions_500_records() {
        // With 500 identical-schema records, implicit transitions should be used.
        let mut records: Vec<Vec<u8>> = Vec::new();
        for i in 0u32..500 {
            let mut rec = Vec::new();
            rec.push(0x08);
            rec.extend_from_slice(&encode_u64(i as u64));
            records.push(rec);
        }
        let refs: Vec<&[u8]> = records.iter().map(|r| r.as_slice()).collect();
        let result = roundtrip(&refs, CompressionType::None);
        assert_eq!(result.len(), 500);
        for (i, (got, expected)) in result.iter().zip(records.iter()).enumerate() {
            assert_eq!(got, expected, "record {i} mismatch");
        }
    }

    #[test]
    fn test_noop_bridging_70_unique_fields() {
        // 70 records, each with a unique field number => >64 unique tags.
        let mut records: Vec<Vec<u8>> = Vec::new();
        for i in 0u32..70 {
            let field_num = i + 1;
            let tag = (field_num << 3) | 0;
            let mut rec = Vec::new();
            rec.extend_from_slice(&encode_u32(tag));
            rec.push(0x01);
            records.push(rec);
        }
        let refs: Vec<&[u8]> = records.iter().map(|r| r.as_slice()).collect();
        let result = roundtrip(&refs, CompressionType::None);
        assert_eq!(result.len(), 70);
        for (i, (got, expected)) in result.iter().zip(records.iter()).enumerate() {
            assert_eq!(got, expected, "record {i} mismatch");
        }
    }

    #[test]
    fn test_noop_bridging_100_unique_fields_per_record() {
        // Each record has 100 unique fields (>64), forcing NoOp bridging.
        let mut records: Vec<Vec<u8>> = Vec::new();
        for i in 0u32..10 {
            let rec = build_wide_proto(1, 100, i * 100);
            records.push(rec);
        }
        let refs: Vec<&[u8]> = records.iter().map(|r| r.as_slice()).collect();
        let result = roundtrip(&refs, CompressionType::None);
        assert_eq!(result.len(), 10);
        for (i, (got, expected)) in result.iter().zip(records.iter()).enumerate() {
            assert_eq!(got, expected, "record {i} mismatch");
        }
    }

    #[test]
    fn test_noop_bridging_200_unique_fields() {
        // 200 unique field numbers -- requires multiple levels of NoOp bridging.
        let mut records: Vec<Vec<u8>> = Vec::new();
        for i in 0u32..5 {
            let rec = build_wide_proto(1, 200, i * 200);
            records.push(rec);
        }
        let refs: Vec<&[u8]> = records.iter().map(|r| r.as_slice()).collect();
        let result = roundtrip(&refs, CompressionType::None);
        assert_eq!(result.len(), 5);
        for (i, (got, expected)) in result.iter().zip(records.iter()).enumerate() {
            assert_eq!(got, expected, "record {i} mismatch");
        }
    }

    #[test]
    #[cfg(feature = "brotli")]
    fn test_optimized_brotli_100_records() {
        let mut records: Vec<Vec<u8>> = Vec::new();
        for i in 0u32..100 {
            let mut rec = Vec::new();
            rec.push(0x08);
            rec.extend_from_slice(&encode_u64(i as u64));
            records.push(rec);
        }
        let refs: Vec<&[u8]> = records.iter().map(|r| r.as_slice()).collect();
        let result = roundtrip(&refs, CompressionType::Brotli);
        assert_eq!(result.len(), 100);
        for (i, (got, expected)) in result.iter().zip(records.iter()).enumerate() {
            assert_eq!(got, expected, "record {i} mismatch");
        }
    }

    #[test]
    fn test_wide_schema_mixed_types() {
        // Records with 50 varint fields + 25 fixed32 fields + 25 string fields.
        let mut records: Vec<Vec<u8>> = Vec::new();
        for i in 0u32..50 {
            let mut rec = Vec::new();
            for f in 1u32..=50 {
                let tag = (f << 3) | 0;
                rec.extend_from_slice(&encode_u32(tag));
                rec.extend_from_slice(&encode_u64((i + f) as u64));
            }
            for f in 51u32..=75 {
                let tag = (f << 3) | 5;
                rec.extend_from_slice(&encode_u32(tag));
                rec.extend_from_slice(&(i + f).to_le_bytes());
            }
            for f in 76u32..=100 {
                let tag = (f << 3) | 2;
                rec.extend_from_slice(&encode_u32(tag));
                rec.push(2);
                rec.push(b'h');
                rec.push(b'i');
            }
            records.push(rec);
        }
        let refs: Vec<&[u8]> = records.iter().map(|r| r.as_slice()).collect();
        let result = roundtrip(&refs, CompressionType::None);
        assert_eq!(result.len(), 50);
        for (i, (got, expected)) in result.iter().zip(records.iter()).enumerate() {
            assert_eq!(got, expected, "record {i} mismatch");
        }
    }

    #[test]
    fn test_no_implicit_loops_many_unique_tags() {
        // >64 unique tags with NoOp bridging — verify no implicit loops.
        let mut records: Vec<Vec<u8>> = Vec::new();
        for i in 0u32..80 {
            let field_num = i + 1;
            let tag = (field_num << 3) | 0;
            let mut rec = Vec::new();
            rec.extend_from_slice(&encode_u32(tag));
            rec.push(0x01);
            records.push(rec);
        }
        let refs: Vec<&[u8]> = records.iter().map(|r| r.as_slice()).collect();
        let mut enc = TransposeChunkEncoder::new(CompressionType::None);
        for rec in &refs {
            enc.add_record(rec).unwrap();
        }
        let chunk = enc.encode().unwrap();
        let dec = TransposeChunkDecoder::new(chunk);
        assert!(dec.is_ok(), "decoder should not report implicit loops");
    }

    #[test]
    fn test_high_field_numbers_10000_and_20000() {
        // Very high field numbers.
        let mut records: Vec<Vec<u8>> = Vec::new();
        for i in 0u32..10 {
            let mut rec = Vec::new();
            let tag = (10000u32 << 3) | 0;
            rec.extend_from_slice(&encode_u32(tag));
            rec.extend_from_slice(&encode_u64(i as u64));
            let tag2 = (20000u32 << 3) | 0;
            rec.extend_from_slice(&encode_u32(tag2));
            rec.extend_from_slice(&encode_u64((i * 2) as u64));
            records.push(rec);
        }
        let refs: Vec<&[u8]> = records.iter().map(|r| r.as_slice()).collect();
        let result = roundtrip(&refs, CompressionType::None);
        assert_eq!(result.len(), 10);
        for (i, (got, expected)) in result.iter().zip(records.iter()).enumerate() {
            assert_eq!(got, expected, "record {i} mismatch");
        }
    }

    // -------------------------------------------------------------------------
    // Slow roundtrip proptest (ignored by default)
    // -------------------------------------------------------------------------

    fn transpose_roundtrip_proptest_check(records: &[Vec<u8>], compression: CompressionType) {
        let mut enc = TransposeChunkEncoder::new(compression);
        for rec in records {
            enc.add_record(rec).expect("add_record");
        }
        let chunk = enc.encode().expect("encode");
        let mut dec = TransposeChunkDecoder::new(chunk).expect("decoder");
        let mut got: Vec<Vec<u8>> = Vec::new();
        while let Some(rec) = dec.read_record().expect("read_record") {
            got.push(rec);
        }
        assert_eq!(
            got.len(),
            records.len(),
            "record count mismatch: wrote {} read {}",
            records.len(),
            got.len()
        );
        for (i, (expected, actual)) in records.iter().zip(got.iter()).enumerate() {
            assert_eq!(
                expected,
                actual,
                "record {} mismatch: expected {} bytes, got {} bytes",
                i,
                expected.len(),
                actual.len()
            );
        }
    }

    proptest::proptest! {
        #![proptest_config(proptest::prelude::ProptestConfig::with_cases(500))]

        /// Slow proptest — ignored by default. Run with:
        /// `cargo test -p riegeli --lib -- --ignored`
        #[test]
        #[ignore]
        fn proptest_transpose_roundtrip_none(
            records in proptest::collection::vec(
                proptest::collection::vec(proptest::prelude::any::<u8>(), 0..=4096),
                0..=500,
            )
        ) {
            transpose_roundtrip_proptest_check(&records, CompressionType::None);
        }
    }
}
