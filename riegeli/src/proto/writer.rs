//! Builder for incrementally constructing serialized protobuf messages.

use super::field_iter::{ProtoField, serialize_field};
use super::wire::{
    WireType, encode_tag, encode_varint32, encode_varint64, zigzag_encode_i32, zigzag_encode_i64,
};

/// The maximum length of a length-delimited field body (2 GiB, the proto limit).
const MAX_LENGTH_DELIMITED: usize = i32::MAX as usize;

/// A builder for incrementally constructing serialized protobuf messages.
///
/// Appends tag+value pairs to an internal `Vec<u8>` buffer. Supports nested
/// submessages via open/close length-delimited scoping, where the length prefix
/// is back-patched after the contents are written.
///
/// # Example
///
/// ```
/// use riegeli::proto::SerializedMessageWriter;
///
/// let mut w = SerializedMessageWriter::new();
/// w.write_uint64(1, 150).unwrap();
/// let bytes = w.finish().unwrap();
/// assert_eq!(bytes, vec![0x08, 0x96, 0x01]);
/// ```
pub struct SerializedMessageWriter {
    buf: Vec<u8>,
    /// Stack of saved positions for open length-delimited scopes.
    /// Each entry is the buffer offset where the submessage content begins
    /// (i.e., right after the tag; the length varint will be inserted here on close).
    scope_stack: Vec<usize>,
}

impl SerializedMessageWriter {
    /// Creates a new writer with an empty buffer.
    pub fn new() -> Self {
        SerializedMessageWriter {
            buf: Vec::new(),
            scope_stack: Vec::new(),
        }
    }

    /// Creates a new writer with the given initial buffer capacity.
    pub fn with_capacity(capacity: usize) -> Self {
        SerializedMessageWriter {
            buf: Vec::with_capacity(capacity),
            scope_stack: Vec::new(),
        }
    }

    /// Returns a reference to the bytes written so far.
    pub fn as_bytes(&self) -> &[u8] {
        &self.buf
    }

    /// Consumes the writer and returns the serialized bytes.
    ///
    /// Returns an error if there are unclosed length-delimited scopes.
    pub fn finish(self) -> Result<Vec<u8>, crate::RiegeliError> {
        if !self.scope_stack.is_empty() {
            return Err(crate::RiegeliError::MalformedData(
                format!(
                    "finish() called with {} unclosed length-delimited scope(s)",
                    self.scope_stack.len()
                )
                .into(),
            ));
        }
        Ok(self.buf)
    }

    // -- Varint wire type fields --

    /// Writes a uint64 field (wire type Varint).
    pub fn write_uint64(
        &mut self,
        field_number: u32,
        value: u64,
    ) -> Result<(), crate::RiegeliError> {
        encode_tag(&mut self.buf, field_number, WireType::Varint);
        encode_varint64(&mut self.buf, value);
        Ok(())
    }

    /// Writes a uint32 field (wire type Varint).
    pub fn write_uint32(
        &mut self,
        field_number: u32,
        value: u32,
    ) -> Result<(), crate::RiegeliError> {
        encode_tag(&mut self.buf, field_number, WireType::Varint);
        encode_varint64(&mut self.buf, value as u64);
        Ok(())
    }

    /// Writes an int64 field (wire type Varint).
    ///
    /// Negative values are encoded as 10-byte varints (sign-extended to u64).
    pub fn write_int64(
        &mut self,
        field_number: u32,
        value: i64,
    ) -> Result<(), crate::RiegeliError> {
        encode_tag(&mut self.buf, field_number, WireType::Varint);
        encode_varint64(&mut self.buf, value as u64);
        Ok(())
    }

    /// Writes an int32 field (wire type Varint).
    ///
    /// Negative values are sign-extended to i64 then encoded as 10-byte varints,
    /// matching the proto spec.
    pub fn write_int32(
        &mut self,
        field_number: u32,
        value: i32,
    ) -> Result<(), crate::RiegeliError> {
        encode_tag(&mut self.buf, field_number, WireType::Varint);
        encode_varint64(&mut self.buf, value as i64 as u64);
        Ok(())
    }

    /// Writes a sint32 field (wire type Varint, zigzag encoded).
    pub fn write_sint32(
        &mut self,
        field_number: u32,
        value: i32,
    ) -> Result<(), crate::RiegeliError> {
        encode_tag(&mut self.buf, field_number, WireType::Varint);
        encode_varint64(&mut self.buf, zigzag_encode_i32(value) as u64);
        Ok(())
    }

    /// Writes a sint64 field (wire type Varint, zigzag encoded).
    pub fn write_sint64(
        &mut self,
        field_number: u32,
        value: i64,
    ) -> Result<(), crate::RiegeliError> {
        encode_tag(&mut self.buf, field_number, WireType::Varint);
        encode_varint64(&mut self.buf, zigzag_encode_i64(value));
        Ok(())
    }

    /// Writes a bool field (wire type Varint).
    pub fn write_bool(
        &mut self,
        field_number: u32,
        value: bool,
    ) -> Result<(), crate::RiegeliError> {
        encode_tag(&mut self.buf, field_number, WireType::Varint);
        encode_varint64(&mut self.buf, if value { 1 } else { 0 });
        Ok(())
    }

    // -- Fixed-width fields --

    /// Writes a fixed32 field (wire type Fixed32).
    pub fn write_fixed32(
        &mut self,
        field_number: u32,
        value: u32,
    ) -> Result<(), crate::RiegeliError> {
        encode_tag(&mut self.buf, field_number, WireType::Fixed32);
        self.buf.extend_from_slice(&value.to_le_bytes());
        Ok(())
    }

    /// Writes a fixed64 field (wire type Fixed64).
    pub fn write_fixed64(
        &mut self,
        field_number: u32,
        value: u64,
    ) -> Result<(), crate::RiegeliError> {
        encode_tag(&mut self.buf, field_number, WireType::Fixed64);
        self.buf.extend_from_slice(&value.to_le_bytes());
        Ok(())
    }

    /// Writes an sfixed32 field (wire type Fixed32).
    pub fn write_sfixed32(
        &mut self,
        field_number: u32,
        value: i32,
    ) -> Result<(), crate::RiegeliError> {
        encode_tag(&mut self.buf, field_number, WireType::Fixed32);
        self.buf.extend_from_slice(&value.to_le_bytes());
        Ok(())
    }

    /// Writes an sfixed64 field (wire type Fixed64).
    pub fn write_sfixed64(
        &mut self,
        field_number: u32,
        value: i64,
    ) -> Result<(), crate::RiegeliError> {
        encode_tag(&mut self.buf, field_number, WireType::Fixed64);
        self.buf.extend_from_slice(&value.to_le_bytes());
        Ok(())
    }

    /// Writes a float field (wire type Fixed32).
    pub fn write_float(
        &mut self,
        field_number: u32,
        value: f32,
    ) -> Result<(), crate::RiegeliError> {
        encode_tag(&mut self.buf, field_number, WireType::Fixed32);
        self.buf.extend_from_slice(&value.to_le_bytes());
        Ok(())
    }

    /// Writes a double field (wire type Fixed64).
    pub fn write_double(
        &mut self,
        field_number: u32,
        value: f64,
    ) -> Result<(), crate::RiegeliError> {
        encode_tag(&mut self.buf, field_number, WireType::Fixed64);
        self.buf.extend_from_slice(&value.to_le_bytes());
        Ok(())
    }

    // -- Length-delimited fields --

    /// Writes a bytes field (wire type LengthDelimited).
    ///
    /// Returns an error if the data exceeds the 2 GiB proto limit.
    pub fn write_bytes(
        &mut self,
        field_number: u32,
        data: &[u8],
    ) -> Result<(), crate::RiegeliError> {
        if data.len() > MAX_LENGTH_DELIMITED {
            return Err(crate::RiegeliError::MalformedData(
                format!(
                    "length-delimited field length {} exceeds 2 GiB limit",
                    data.len()
                )
                .into(),
            ));
        }
        encode_tag(&mut self.buf, field_number, WireType::LengthDelimited);
        encode_varint32(&mut self.buf, data.len() as u32);
        self.buf.extend_from_slice(data);
        Ok(())
    }

    /// Writes a string field (wire type LengthDelimited).
    ///
    /// Returns an error if the string exceeds the 2 GiB proto limit.
    pub fn write_string(&mut self, field_number: u32, s: &str) -> Result<(), crate::RiegeliError> {
        self.write_bytes(field_number, s.as_bytes())
    }

    /// Opens a length-delimited scope for writing a nested submessage.
    ///
    /// Writes the tag for the field and saves the current buffer position.
    /// After writing the submessage contents, call `close_length_delimited()`
    /// to back-patch the length prefix.
    pub fn open_length_delimited(&mut self, field_number: u32) -> Result<(), crate::RiegeliError> {
        encode_tag(&mut self.buf, field_number, WireType::LengthDelimited);
        // Save position where the length varint + content will start.
        self.scope_stack.push(self.buf.len());
        Ok(())
    }

    /// Closes the most recently opened length-delimited scope.
    ///
    /// Computes the length of the content written since `open_length_delimited`,
    /// inserts the length varint before the content, and pops the scope stack.
    ///
    /// Returns an error if:
    /// - No scope is currently open.
    /// - The content exceeds the 2 GiB proto limit. In this case the scope
    ///   remains open, so a later `finish()` reports the unclosed scope
    ///   instead of returning bytes with a missing length prefix.
    pub fn close_length_delimited(&mut self) -> Result<(), crate::RiegeliError> {
        let content_start = *self.scope_stack.last().ok_or_else(|| {
            crate::RiegeliError::MalformedData(
                "close_length_delimited() called without matching open".into(),
            )
        })?;

        let content_len = self.buf.len() - content_start;
        if content_len > MAX_LENGTH_DELIMITED {
            // Validate before consuming the scope: at this point the buffer
            // holds the field's tag followed by raw content with no length
            // prefix, so popping the scope would let finish() succeed and
            // return structurally corrupt bytes.
            return Err(crate::RiegeliError::MalformedData(
                format!(
                    "length-delimited field length {} exceeds 2 GiB limit",
                    content_len
                )
                .into(),
            ));
        }
        self.scope_stack.pop();

        // Encode the length varint into a temporary buffer, then splice it in.
        let mut len_varint = Vec::new();
        encode_varint32(&mut len_varint, content_len as u32);

        // Insert the length varint at content_start, shifting content forward.
        self.buf
            .splice(content_start..content_start, len_varint.iter().copied());

        Ok(())
    }

    // -- Group fields --

    /// Writes a StartGroup tag for the given field number.
    pub fn write_start_group(&mut self, field_number: u32) -> Result<(), crate::RiegeliError> {
        encode_tag(&mut self.buf, field_number, WireType::StartGroup);
        Ok(())
    }

    /// Writes an EndGroup tag for the given field number.
    pub fn write_end_group(&mut self, field_number: u32) -> Result<(), crate::RiegeliError> {
        encode_tag(&mut self.buf, field_number, WireType::EndGroup);
        Ok(())
    }

    /// Writes a raw `ProtoField` directly to the output buffer.
    ///
    /// This appends the field's tag and value bytes exactly as they would appear
    /// in the wire format, enabling pass-through copying of fields from one
    /// message to another without re-encoding.
    pub fn write_field(&mut self, field: &ProtoField<'_>) -> Result<(), crate::RiegeliError> {
        serialize_field(&mut self.buf, field);
        Ok(())
    }
}

impl Default for SerializedMessageWriter {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn close_after_all_scopes_closed_is_error() {
        let mut w = SerializedMessageWriter::new();
        w.open_length_delimited(1).unwrap();
        w.write_uint64(1, 42).unwrap();
        w.close_length_delimited().unwrap();
        // Second close should fail: no scope is open anymore.
        assert!(w.close_length_delimited().is_err());
    }

    #[test]
    fn finish_with_partially_closed_nesting_is_error() {
        let mut w = SerializedMessageWriter::new();
        w.open_length_delimited(1).unwrap();
        w.open_length_delimited(2).unwrap();
        w.write_uint64(1, 1).unwrap();
        w.close_length_delimited().unwrap(); // closes field 2
        // field 1 still open
        assert!(w.finish().is_err());
    }

    #[test]
    fn field_number_zero_not_validated() {
        // The writer doesn't validate field numbers, so field_number=0
        // produces a message that is_proto_message rejects.
        let mut w = SerializedMessageWriter::new();
        w.write_uint64(0, 1).unwrap();
        let bytes = w.finish().unwrap();
        // tag = (0 << 3) | 0 = 0, which is not a valid proto tag
        assert!(!crate::proto::is_proto_message(&bytes));
    }

    #[test]
    fn close_length_delimited_over_limit_leaves_scope_open() {
        // Closing a scope whose content exceeds the 2 GiB proto limit must
        // fail without consuming the scope. At that point the buffer holds
        // the field's LengthDelimited tag followed by raw content with no
        // length prefix, so if the scope were popped, a subsequent finish()
        // would return structurally corrupt bytes. With the scope left open,
        // finish() reports the inconsistency instead.
        let mut w = SerializedMessageWriter::new();
        w.open_length_delimited(1).unwrap();
        // Simulate over-limit content by growing the internal buffer
        // directly; only the content length of the open scope matters.
        let target_len = w.buf.len() + MAX_LENGTH_DELIMITED + 1;
        w.buf.resize(target_len, 0);
        assert!(w.close_length_delimited().is_err());
        assert!(
            w.finish().is_err(),
            "finish() must not bless a buffer holding a dangling length-delimited tag"
        );
    }

    #[test]
    fn as_bytes_reflects_open_scope_buffer() {
        let mut w = SerializedMessageWriter::new();
        w.write_uint64(1, 1).unwrap();
        let before_open = w.as_bytes().len();
        w.open_length_delimited(2).unwrap();
        // After open, buffer has the tag but no length varint yet
        let after_open = w.as_bytes().len();
        assert!(after_open > before_open);
        w.write_uint64(1, 2).unwrap();
        w.close_length_delimited().unwrap();
        // After close, the length is spliced in
        let after_close = w.as_bytes().len();
        assert!(after_close > after_open);
        let _ = w.finish().unwrap();
    }
}
