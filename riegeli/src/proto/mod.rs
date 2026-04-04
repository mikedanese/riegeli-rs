//! Protobuf field-level streaming I/O.
//!
//! This module provides zero-copy proto wire format primitives, a field
//! iterator, message writer, handler framework, and streaming integration
//! with `RecordReader`/`RecordWriter`.

mod field_iter;
mod handler;
mod stream;
mod wire;
mod writer;

pub use field_iter::{
    FieldValue, FilteredFieldIter, ProtoField, ProtoFieldIter, copy_fields, serialize_field,
};
pub use handler::{
    DynamicHandlerSet, EmptyHandlerSet, FieldHandler, HandleField, StaticHandlerSet,
    StaticHandlerSet1, StaticHandlerSet2, StaticHandlerSet3, read_message,
};
pub use stream::{
    StreamError, extract_varint_column, filter_fields_to_writer, for_each_proto_record,
};
pub use wire::{
    WireType, encode_tag, encode_varint32, encode_varint64, is_proto_message, make_tag,
    read_canonical_varint64, tag_field_number, tag_wire_type, zigzag_encode_i32, zigzag_encode_i64,
};
pub use writer::SerializedMessageWriter;

// Re-export crate-internal items needed by other modules.
pub(crate) use wire::is_valid_proto_tag;
