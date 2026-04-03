# Transpose chunk format

Technical documentation for the Riegeli transpose chunk encoding, identified by
`ChunkType::Transposed = 't'` (0x74).

## Overview and motivation

Riegeli's transpose encoding decomposes protobuf records column-wise: all values
for the same field across all records in a chunk are stored together in a single
data buffer. When these homogeneous buffers are then compressed (Brotli, Zstd),
the compressor sees a long run of similar data instead of interleaved
heterogeneous bytes, yielding much better compression ratios.

A simple chunk stores `[record_0][record_1]...[record_N]` -- each record is a
self-contained protobuf. A transpose chunk instead stores
`[all field-1 values][all field-2 values]...` plus a state machine that
describes how to reassemble individual records.

---

## Chunk wire format

After the standard 40-byte `ChunkHeader`, the raw chunk data has five sections:

```
[compression_type : u8]
[header_length    : varint64]          // compressed byte count of the header section
[compressed_header: header_length bytes]
[bucket_0         : bucket_sizes[0] bytes]
  ...
[bucket_N         : bucket_sizes[N] bytes]
[transitions      : remaining bytes]   // compressed transition stream
```

### Section 1: compression type

A single byte identifying the compression algorithm applied to the header,
buckets, and transition stream. Same encoding as simple chunks:

| Value  | Algorithm |
|--------|-----------|
| `0x00` | None      |
| `0x62` | Brotli    |
| `0x7a` | Zstd      |
| `0x73` | Snappy    |

### Section 2: header length

A varint64-encoded value giving the compressed byte count of the header blob
that immediately follows.

### Section 3: compressed header

A compressed (or uncompressed, if compression type is None) blob whose
decompressed contents are structured as follows, decoded in order:

| # | Field                        | Encoding    | Count                   |
|---|------------------------------|-------------|-------------------------|
| 1 | `num_buckets`                | varint64    | 1                       |
| 2 | `num_buffers`                | varint64    | 1                       |
| 3 | `bucket_compressed_size[i]`  | varint64    | `num_buckets` times     |
| 4 | `buffer_uncompressed_size[i]`| varint64    | `num_buffers` times     |
| 5 | `num_states`                 | varint64    | 1                       |
| 6 | `tag[i]`                     | varint32    | `num_states` times      |
| 7 | `subtype[i]`                 | u8          | once per varint-type state |
| 8 | `buffer_index[i]`            | varint32    | once per state that reads data |
| 9 | `next_node_index[i]`         | varint32    | `num_states` times      |
| 10| `first_node`                 | varint32    | 1                       |

The **tag** encodes a proto field number and wire type (or a reserved sentinel).
The **subtype** further qualifies varint and length-delimited fields (see
constants below). The **buffer_index** links a state to the decompressed buffer
it reads from. The **next_node_index** gives the default successor state.

### Section 4: buckets

`num_buckets` consecutive compressed blobs, each `bucket_compressed_size[i]`
bytes long. Each bucket decompresses to one or more buffers concatenated
together; the per-buffer boundaries are recovered from `buffer_uncompressed_size`
values (see Buckets and buffers below).

### Section 5: transitions

All remaining bytes after the last bucket form the compressed transition stream.
Once decompressed, this is a flat byte array consumed one byte at a time by the
state machine whenever a non-implicit transition fires.

---

## Buckets and buffers

Buffers hold the actual field data. They are grouped into **buckets** for
compression efficiency -- multiple buffers may be concatenated into one bucket
so the compressor can exploit cross-buffer similarity.

During decode, each bucket is decompressed once and then sliced by the
per-buffer `buffer_uncompressed_size` values to yield individual buffer byte
slices.

Buffer types, in canonical order:

| Type     | Content                               |
|----------|---------------------------------------|
| Varint   | Varint field values (high bit stripped)|
| Fixed32  | 4-byte fixed fields                   |
| Fixed64  | 8-byte fixed fields                   |
| String   | Length-delimited field payloads        |
| NonProto | Entire non-proto records              |

### Varint encoding

Varint field values are stored with the high-continuation bit (bit 7) stripped
from every byte. During decode the high bit is restored to all bytes except the
last one. This strips one bit per byte but allows efficient buffer packing
because the byte length completely determines the value range.

### Inline varint optimization

Small varint values in the range 0 through 127 can be stored **inline** in the
subtype byte rather than in a data buffer. Subtypes `VARINT_INLINE_0` (10)
through `VARINT_INLINE_MAX` (137) cover inline values 0--127.

Buffered varints use subtypes `VARINT_1` (0) through `VARINT_10` (9), encoding
byte lengths 1--10. A state with an inline subtype does not consume buffer bytes.

---

## State machine

The transpose decoder is driven by a state machine. Each state is a
`StateMachineNode` with the following fields:

| Field              | Description                                               |
|--------------------|-----------------------------------------------------------|
| `tag_data`         | Pre-encoded varint tag bytes                              |
| `callback_type`    | Action to take (see below)                                |
| `buffer_index`     | Optional index into the decompressed buffer array         |
| `next_node_index`  | Default successor state                                   |
| `is_implicit`      | Whether the transition consumes no byte from the stream   |

### Callback types

| Callback          | Action                                                     |
|-------------------|------------------------------------------------------------|
| `NoOp`            | No output; used for bridging (see optimized state machine) |
| `MessageStart`    | Begin a new record                                         |
| `SubmessageStart` | Open a nested submessage                                   |
| `SubmessageEnd`   | Close a nested submessage                                  |
| `NonProto`        | Emit a non-proto record from its buffer                    |
| `CopyTag`         | Emit the pre-encoded tag bytes                             |
| `Varint { len }`  | Read `len` bytes from the varint buffer, restore high bits |
| `Fixed32`         | Read 4 bytes from the fixed32 buffer                       |
| `Fixed64`         | Read 8 bytes from the fixed64 buffer                       |
| `StringField`     | Read a length-prefixed blob from the string buffer         |
| `Failure`         | Unreachable state; error if entered                        |

### Transitions

After executing a node's callback, the machine transitions to the next state.
Two mechanisms:

1. **Implicit transition.** If `next_node_index >= num_states`, the transition
   is implicit -- no byte is consumed from the transition stream. The actual
   target index is `next_node_index - num_states`. This is used when a node has
   only one possible successor.

2. **Explicit transition.** A byte is read from the decompressed transition
   stream. The byte encodes a relative offset within the destination's public
   list (0-indexed), bounded by `MAX_TRANSITION = 63`.

---

## Optimized state machine

The encoder builds an optimized state machine to minimize the size of the
transition stream. This mirrors the C++ `CreateStateMachine` algorithm.

### Transition statistics

During encoding, the encoder collects transition statistics: for each source
node, it counts how many times each destination was visited.

### Private list

Destinations visited `>= MIN_COUNT_FOR_STATE` (10) times from a given source
get a dedicated slot in that source's **private list**. Private list entries get
`next_node_index` values `< MAX_TRANSITION` (63), so transitions to them do
**not** consume a transition byte. Instead, the most frequent destination
becomes the implicit transition (encoded as `next_node_index = target +
num_states`).

### Public list

All destination nodes share a single **global public list** ordered by
frequency. A transition byte encodes an index into this list. The encoder
pre-computes a `base_index` for each source node (its starting position in the
global public list). A transition byte `b` from state S selects global position
`base_index[S] + b`.

### NoOp bridging

If a node's public list index exceeds `MAX_TRANSITION` (63), intermediate
**NoOp** states are inserted to bridge the gap. Each NoOp has an implicit
transition to the next NoOp or the final destination. This ensures every
reachable destination is addressable within the single-byte transition encoding.

### Summary of dispatch

```
transition from state S:
  if next_node_index >= num_states:
      target = next_node_index - num_states    // implicit, no byte consumed
  else:
      byte = read_transition_byte()
      target = public_list[base_index[S] + byte]
```

---

## Backward-writing decode pattern

Records are reconstructed by **prepending** (not appending) field data. The
state machine runs forward through states, but each callback operation prepends
tag + value bytes to an accumulation buffer.

The decoder maintains:

| Structure          | Purpose                                                  |
|--------------------|----------------------------------------------------------|
| `backward_data`    | `Vec<u8>` -- accumulates field bytes in reverse          |
| `limits`           | `Vec<usize>` -- record boundary positions (stored backward) |
| `submessage_stack` | `Vec<usize>` -- tracks open submessage lengths           |
| Per-buffer cursors | `read_pos: usize` -- current read position in each buffer |

At the end of each record, the accumulated backward data is reversed and the
record boundaries (stored as backward limits) are complemented and reversed to
yield correct forward slicing positions.

---

## Proto field decomposition (encoder)

The encoder decomposes each record as follows:

1. **Proto check.** Call `is_proto_message(record)`. If false, store the entire
   record in the NonProto buffer and emit a NonProto state transition.

2. **Field extraction.** For each field in the proto, extract the tag and wire
   type:

   - **Varint:** Strip the high continuation bits from each byte. If the value
     is in 0..=127, use an inline subtype (`VARINT_INLINE_0 + value`) with no
     buffer write. Otherwise store in a per-(parent_message_id, tag, subtype)
     varint buffer.

   - **Fixed32:** Write 4 bytes to the Fixed32 buffer.

   - **Fixed64:** Write 8 bytes to the Fixed64 buffer.

   - **LengthDelimited:** Call `is_proto_message(content)` recursively. If the
     content is a valid submessage and recursion depth <
     `MAX_RECURSION_DEPTH` (100), recurse into it. Otherwise store the content
     in a String buffer with a varint32 length prefix.

3. **Transition recording.** Record the field visit as a transition from the
   current state to the field's state machine node.

4. **Record boundary.** At the end of each message, emit a transition to the
   `START_OF_MESSAGE` node.

5. **Finalization.** All buffers are written backward (prepend) and reversed at
   encode-finish time. Transition statistics are used to build the optimized
   state machine.

### Submessage encoding

Length-delimited fields that contain valid proto data are treated as submessages
and recursively decomposed. A synthetic wire-type value
`SUBMESSAGE_WIRE_TYPE = 6` is used in state tags to mark submessage-end nodes.

Subtype constants for `WireType::LengthDelimited`:

| Constant                              | Value | Meaning                  |
|---------------------------------------|-------|--------------------------|
| `LENGTH_DELIMITED_STRING`             | 0     | Plain string/bytes field |
| `LENGTH_DELIMITED_START_OF_SUBMESSAGE`| 1     | Start of submessage      |
| `LENGTH_DELIMITED_END_OF_SUBMESSAGE`  | 2     | End of submessage        |

---

## Key constants

| Constant                     | Value | Description                                           |
|------------------------------|-------|-------------------------------------------------------|
| `BLOCK_SIZE`                 | 65536 | Block size in bytes                                   |
| `BLOCK_HEADER_SIZE`          | 24    | Block header size in bytes                            |
| `CHUNK_HEADER_SIZE`          | 40    | Chunk header size in bytes                            |
| `MAX_TRANSITION`             | 63    | Maximum transition byte value                         |
| `MIN_COUNT_FOR_STATE`        | 10    | Minimum transition count for private-list promotion   |
| `MAX_RECURSION_DEPTH`        | 100   | Maximum submessage nesting depth                      |
| `SUBMESSAGE_WIRE_TYPE`       | 6     | Synthetic wire type for submessage-end nodes          |
| `VARINT_1` .. `VARINT_10`    | 0--9  | Subtypes for buffered varints of 1--10 bytes          |
| `VARINT_INLINE_0`            | 10    | Subtype for inline varint value 0                     |
| `VARINT_INLINE_MAX`          | 137   | Subtype for inline varint value 127                   |
| `LENGTH_DELIMITED_STRING`    | 0     | Subtype: plain string/bytes                           |
| `LENGTH_DELIMITED_START_OF_SUBMESSAGE` | 1 | Subtype: submessage start                    |
| `LENGTH_DELIMITED_END_OF_SUBMESSAGE`   | 2 | Subtype: submessage end                      |
| `NO_OP` (message ID)        | 0     | NoOp bridging state                                   |
| `NON_PROTO` (message ID)    | 1     | Non-proto record                                      |
| `START_OF_SUBMESSAGE` (message ID) | 2 | Submessage start                                   |
| `START_OF_MESSAGE` (message ID) | 3  | Start of a new record                                 |
| `ROOT` (message ID)         | 4     | Root node (in-memory only)                            |

Message IDs >= 5 are assigned sequentially to `(parent_message_id, proto_tag)`
pairs as new field paths are encountered during encoding.

---

## Glossary

**Block** -- A 65 536-byte aligned region of the file. Every block boundary
carries a `BlockHeader`.

**Bucket** -- A compressed blob containing one or more concatenated buffers.
Decompressed once, then sliced by buffer sizes.

**Buffer** -- A byte slice holding all values for one field (or one type of
non-proto data) across all records in the chunk.

**Callback type** -- The action a state machine node performs: emit tag bytes,
read from a buffer, open/close a submessage, etc.

**Chunk** -- A self-contained unit of records within a Riegeli file, preceded by
a 40-byte `ChunkHeader`.

**Implicit transition** -- A state transition that consumes no byte from the
transition stream. Encoded by setting `next_node_index >= num_states`; the
actual target is `next_node_index - num_states`.

**Inline varint** -- A varint value (0--127) stored directly in the state's
subtype byte rather than in a data buffer.

**NoOp state** -- A state with no output, used to bridge public-list indices
that exceed `MAX_TRANSITION`.

**Private list** -- Per-source-node list of frequently visited destinations.
Transitions to private list entries are implicit (free).

**Public list** -- Global frequency-ordered list of all destination nodes.
Transitions to public list entries cost one byte.

**State machine node** -- A node in the transpose state machine, containing a
tag, callback type, buffer index, and successor index.

**Subtype** -- A byte qualifying a varint or length-delimited state. Determines
whether the value is inline, how many buffer bytes to read, or whether the field
is a submessage boundary.

**Transition byte** -- A single byte read from the transition stream, encoding a
relative index into the public list.

**Transpose** -- Columnar decomposition of protobuf records: same-field values
across records are grouped together for better compression.
