# Transpose Chunk Encoding

Transpose encoding is a columnar decomposition of protobuf records. Instead of
storing each record as a contiguous blob, it groups field values by their field
path across all records in a chunk. This produces columns of homogeneous data
that compress far better than row-oriented storage -- a column of int64
timestamps or a column of fixed32 IDs compresses dramatically with general-
purpose codecs like zstd.

The format is defined by the C++ riegeli library. This document describes the
Rust implementation in `riegeli/src/transpose/`.

## Concepts

### Field paths and NodeIds

A **field path** identifies a specific location in a (possibly nested) proto
message. For example, in a message where field 2 is a submessage containing
field 5, the path to that inner field is `(root, tag(2, LengthDelimited), tag(5, ...))`. The encoder assigns each unique field path a **NodeId** -- an integer starting at 5 (values 0-4 are reserved for structural markers).

### Reserved message IDs


| ID  | Name              | Purpose                                       |
| --- | ----------------- | --------------------------------------------- |
| 0   | NoOp              | No operation; used for state machine bridging |
| 1   | NonProto          | Non-proto record (stored verbatim)            |
| 2   | StartOfSubmessage | Push a submessage frame                       |
| 3   | StartOfMessage    | Record boundary marker                        |
| 4   | Root              | Root node (in-memory only, never serialized)  |


### Subtypes

Varint fields carry a **subtype** byte that encodes either the byte-length of
the buffered value (`VARINT_1..VARINT_10`, values 0-9) or a small inline value
(`VARINT_INLINE_0..VARINT_INLINE_MAX`, values 10-137 representing 0-127).
Inline varints require no data buffer -- the value is recovered from the
subtype alone.

Length-delimited fields use subtypes to distinguish strings (0), submessage
starts (1), and submessage ends (2). The submessage-end node uses a synthetic
wire type 6 (`SUBMESSAGE_WIRE_TYPE`) to differentiate it from real proto wire
types.

### Buffers and buffer types

Each unique `(NodeId, subtype)` pair that carries data gets its own buffer.
Buffers are sorted by type for grouping:


| Order | Type     | Contents                                         |
| ----- | -------- | ------------------------------------------------ |
| 0     | Varint   | Variable-length integers with high bits stripped |
| 1     | Fixed32  | Raw 32-bit values                                |
| 2     | Fixed64  | Raw 64-bit values                                |
| 3     | String   | Length-prefixed byte strings                     |
| 4     | NonProto | Verbatim non-proto record data                   |


### Buckets

Buffers are packed into **buckets** for compression. Each bucket is compressed
independently, which enables the decoder to skip decompressing buckets that
contain no fields of interest (field projection). The encoder uses a greedy
split: a new bucket starts when adding the next buffer would push the current
bucket past `bucket_size / 2`.

## Wire format

A transpose chunk contains five sections:

```
[1] Compression type         (u8)
[2] Header length            (varint64)
[3] Compressed header        (variable)
[4] Bucket data              (variable)
[5] Transitions              (variable)
```

**Section 3 (header)** contains, after decompression:

- `num_buckets` (varint32)
- `num_buffers` (varint32)
- Bucket compressed sizes (varint64 x num_buckets)
- Buffer uncompressed sizes (varint64 x num_buffers)
- State machine metadata:
  - `num_states` (varint32)
  - `tags[num_states]` (varint32 each) -- proto tags or reserved message IDs
  - `next_node_indices[num_states]` (varint32 each) -- implicit flag in high bit
  - `subtypes[]` (u8 each) -- only for varint-wire-type tags
  - `buffer_indices[]` (varint32 each) -- only for states that read data
  - `first_node` (varint32) -- initial state index

**Section 4** contains `num_buckets` independently compressed blobs, each
holding concatenated buffer data.

**Section 5** is a compressed stream of transition bytes that drive state
machine execution.

## Encoding

`TransposeChunkEncoder` accepts records via `add_record()` and produces a
`Chunk` via `encode()`.

### Proto decomposition

Each record is recursively decomposed into proto fields. The encoder walks the
serialized bytes, identifies field numbers and wire types, and appends each
field's value to the buffer for its `(NodeId, subtype)`. Submessage boundaries
produce `StartOfSubmessage` / submessage-end markers. Records that fail the
`is_proto_message` check are stored verbatim as NonProto.

Varint values have their high bits (continuation bits) stripped before storage.
The subtype records either the byte-length (for values >= 4) or the value
itself (for values 0-3, stored inline).

All field data is written **backward** into per-node buffers and reversed at
flush time. This avoids O(n) shifts when prepending.

### State machine construction

The encoder builds an optimized state machine that tells the decoder which
action to perform for each field. The key insight is that proto messages with
a consistent schema produce highly regular transition patterns -- field 1 is
almost always followed by field 2, then field 3, etc. The state machine
exploits this regularity.

**Private lists**: For transitions that occur >= `MIN_COUNT_FOR_STATE` (10)
times, the encoder creates private destination lists -- short lists of likely
next-states reachable with a single transition byte.

**Public list**: All remaining destinations go into a shared public list.

**NoOp bridging**: When a private or public list exceeds `MAX_TRANSITION + 1`
(64) entries, it is split into blocks connected by NoOp states. Each block
holds up to 64 entries; a NoOp at the end of one block points to the next.

**Implicit transitions**: When a state has exactly one possible successor, no
transition byte is needed -- the decoder just follows `next_node`. The encoder
marks these with a flag in the high bit of `next_node_index`. The last state
in the sequence is forced explicit (`ensure_last_state_explicit`) to prevent
decoder ambiguity about when transitions are exhausted.

### Transition encoding

Each explicit transition is encoded as one or more bytes:

```
byte = (offset << 2) | repeat_count
```

- `offset` (6 bits): index within the current block (0..63)
- `repeat_count` (2 bits): number of additional zero-offset hops (0..3)

Multi-hop transitions traverse NoOp chains to reach distant states.

## Decoding

`TransposeChunkDecoder` parses a chunk and yields records one at a time via
`read_record()`.

### Header parsing

The decoder reads the compression type, decompresses the header, and extracts
bucket/buffer sizes and the state machine metadata (tags, next-node indices,
subtypes, buffer indices, first node).

### Bucket decompression

Each bucket is decompressed and split into individual buffer cursors. With
field projection, the decoder first scans the state machine metadata to
determine which buffers are needed, then only decompresses buckets that contain
at least one needed buffer. Unneeded buffers get a pruned stub cursor.

### State machine execution

The decoder walks the state machine, consuming transition bytes from
section 5. At each node it performs one of these actions:


| Callback        | Action                                                           |
| --------------- | ---------------------------------------------------------------- |
| NoOp            | Skip; follow next_node                                           |
| MessageStart    | Mark a record boundary                                           |
| SubmessageStart | Pop submessage frame; write tag + length header                  |
| SubmessageEnd   | Push (end_pos, node_index) onto submessage stack                 |
| NonProto        | Read length then data from buffer; mark record boundary          |
| CopyTag         | Write pre-encoded tag bytes (used for inline varints)            |
| Varint          | Read raw bytes from buffer, restore high bits, write tag + value |
| Fixed32         | Read 4 bytes, write tag + data                                   |
| Fixed64         | Read 8 bytes, write tag + data                                   |
| StringField     | Read length varint + data, write tag + length + data             |


### Backward writing and finalization

Like the encoder, the decoder writes records **backward** -- the last record's
bytes come first in the output buffer. After all transitions are consumed, the
buffer is reversed and record boundaries (tracked during execution) are
complemented to produce forward-order offsets.

### Field projection

When constructed with a `FieldProjection`, the decoder optimizes reads:

1. **Buffer pruning**: Scan the state machine to identify which buffers
  correspond to projected fields. Mark all others as pruned.
2. **Bucket skipping**: Only decompress buckets containing needed buffers.
3. **Post-decode filtering**: After full record reconstruction,
  `FieldProjection::apply()` re-parses each record and strips non-projected
   fields.

## Module structure


| File          | Lines  | Purpose                                                        |
| ------------- | ------ | -------------------------------------------------------------- |
| `internal.rs` | ~500   | Constants (message IDs, subtypes, wire types), predicates      |
| `encoder.rs`  | ~3,200 | `TransposeChunkEncoder`: decomposition, state machine, buckets |
| `decoder.rs`  | ~2,200 | `TransposeChunkDecoder`: parsing, execution, record yielding   |
| `mod.rs`      | ~10    | Re-exports                                                     |


The Rust line counts are higher than the C++equivalents (~1,390 for encoder,
~1,618 for decoder) partly because Rust includes inline tests and doc comments
that live in separate files in C++.

## Gaps vs. C++ reference

The Rust implementation produces byte-compatible output and passes cross-
language conformance tests (Sprint 29). The following are areas where the Rust
implementation diverges from C++ in approach or is missing optimizations.

### Gap 1 (P0): Projection happens post-decode, not during decode

**C++ approach**: The decoder resolves field inclusion *during* state machine
execution. Each node's callback type is determined lazily via a
`kSelectCallback` that checks whether the field (and its ancestor submessages)
are in the projection. Excluded fields get `kSkippedSubmessageStart` /
`kSkippedSubmessageEnd` callbacks that track `skipped_submessage_level` to
suppress all nested output.

The C++ `CallbackType` enum has ~100 variants generated via macro
(`TYPES_FOR_TAG_LEN`), encoding both the operation *and* the tag varint length
(1-6 bytes) into a single dispatch. Projection-specific variants include
`kFixed32Existence_{1..5}`, `kStartProjectionGroup_{1..5}`,
`kSelectCallback`, `kSkippedSubmessageStart`, and `kSkippedSubmessageEnd`.

**Rust approach**: The decoder has 11 `CallbackType` variants with no
projection awareness. All records are fully decoded, then
`FieldProjection::apply()` post-processes each record by re-parsing the proto
wire format and filtering fields.

**Impact**: Rust always pays the full cost of decoding every field, then
re-parses each record to filter. For workloads that project a small subset of
fields from wide messages, this is significantly slower than C++.

### Gap 2 (P1): Bucket decompression is not fully lazy

**C++ approach**: `ParseBuffersForFiltering()` tracks which buckets contain at
least one needed buffer. `GetBuffer()` decompresses on first access. Buckets
with no needed buffers are never decompressed.

**Rust approach**: `compute_needed_buffers_from_scan()` determines which
*buffers* are needed, then `decompress_into_buffers_with_pruning()` iterates
over *every bucket sequentially*. Unneeded buffers within a decompressed bucket
get a `BufferCursor::pruned()` stub, but the bucket decompression itself
happens unconditionally in the sequential loop.

**Impact**: If a bucket contains 10 buffers and only 1 is needed, Rust still
decompresses the entire bucket. This is correct but wastes work when most
buffers in a bucket are unused.

### Gap 3 (P2): No `skipped_submessage_level` tracking

**C++ approach**: During projected decoding, `skipped_submessage_level` tracks
depth inside excluded submessages. When > 0, the decoder suppresses all output
for nested fields, avoiding both data reads and tag writes.

**Rust approach**: No equivalent. The decoder always reconstructs the full
record, then the post-decode filter strips excluded submessages.

**Impact**: Wasteful for deeply nested messages where only top-level fields are
projected. The decoder writes bytes that are immediately discarded.

### Gap 4 (P3): No existence-only support in the decoder

**C++ approach**: When a field is marked existence-only in the projection, the
decoder uses `kFixed32Existence` / `kFixed64Existence` callbacks that write the
tag but zero the value *during* record reconstruction.

**Rust approach**: `Field::existence_only` is supported in the API but applied
in the post-decode `apply_projection_inner` function, which re-parses the fully
decoded record and rewrites fields with zeroed values.

**Impact**: Correct but slow -- the decode loop does full work, then the filter
pass re-parses and rewrites.

### Gap 5 (P4): No tag-length-specialized callbacks

**C++ approach**: The `CallbackType` enum encodes the tag varint length (1-5
bytes) into the callback variant itself (e.g., `kVarint_3_2` means "read 3
varint bytes, tag is 2 bytes"). This eliminates a branch on tag length in the
hot decode loop.

**Rust approach**: Tag bytes are stored in `StateMachineNode::tag_bytes`
(`Vec<u8>`) and their length is determined at runtime.

**Impact**: Minor. Eliminates one branch per field in the hot loop, which
matters at scale but is not a correctness issue.

### Gap 6 (P5): No `canonical_source` optimization during transition encoding

**C++ approach**: `StateInfo::canonical_source` tracks NoOp chains for O(1)
base offset lookup during transition writing.

**Rust approach**: `canonical_source` is set during private list construction
but the encoder uses direct iteration in `write_transitions` rather than
leveraging it for O(1) lookups.

**Impact**: Potentially slower encoding for large state machines with many NoOp
bridges, but produces identical output.

### Summary


| Priority | Gap                              | Nature       | Impact                                                      |
| -------- | -------------------------------- | ------------ | ----------------------------------------------------------- |
| P0       | Projection during decode         | Architecture | Eliminates post-decode re-parse for all projected reads     |
| P1       | Lazy bucket decompression        | Performance  | Skips decompression of entire buckets with no needed fields |
| P2       | Skipped submessage tracking      | Performance  | Avoids writing bytes for excluded nested subtrees           |
| P3       | Existence-only in decoder        | Feature      | Single-pass existence-only without post-decode rewrite      |
| P4       | Tag-length-specialized callbacks | Performance  | Eliminates one branch per field in hot loop                 |
| P5       | `canonical_source` usage         | Performance  | Faster transition encoding for large state machines         |


