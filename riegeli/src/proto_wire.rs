//! Proto wire format primitives.
//!
//! Provides the wire type enum and tag composition/decomposition functions
//! matching the C++ `riegeli/messages/message_wire_format.h`, plus
//! `is_proto_message` for canonical proto binary validation, and a zero-copy
//! field iterator for streaming proto field access.

use std::collections::HashMap;

use crate::varint;

/// The part of a field tag which denotes the representation of the field value
/// that follows the tag.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum WireType {
    /// Variable-length integer.
    Varint = 0,
    /// 64-bit fixed-width value.
    Fixed64 = 1,
    /// Length-delimited bytes.
    LengthDelimited = 2,
    /// Start of a group (deprecated but valid).
    StartGroup = 3,
    /// End of a group (deprecated but valid).
    EndGroup = 4,
    /// 32-bit fixed-width value.
    Fixed32 = 5,
}

impl WireType {
    /// Converts a raw 3-bit wire type value to a `WireType`, returning `None`
    /// for invalid values (6 and 7).
    pub fn from_raw(value: u32) -> Option<WireType> {
        match value {
            0 => Some(WireType::Varint),
            1 => Some(WireType::Fixed64),
            2 => Some(WireType::LengthDelimited),
            3 => Some(WireType::StartGroup),
            4 => Some(WireType::EndGroup),
            5 => Some(WireType::Fixed32),
            _ => None,
        }
    }
}

/// Composes a proto field tag from a field number and wire type.
///
/// The tag is `(field_number << 3) | wire_type`.
#[inline]
pub fn make_tag(field_number: u32, wire_type: WireType) -> u32 {
    (field_number << 3) | (wire_type as u32)
}

/// Extracts the wire type from a proto field tag.
///
/// Returns `None` if the low 3 bits encode an invalid wire type (6 or 7).
#[inline]
pub fn tag_wire_type(tag: u32) -> Option<WireType> {
    WireType::from_raw(tag & 7)
}

/// Extracts the field number from a proto field tag.
#[inline]
pub fn tag_field_number(tag: u32) -> u32 {
    tag >> 3
}

/// Returns `true` if `tag` is a valid proto field tag (field number >= 1 and
/// wire type in 0..=5).
///
/// Tags with field number 0 (i.e. values less than 8) are reserved message IDs
/// in the transpose state machine, not valid proto tags.
pub(crate) fn is_valid_proto_tag(tag: u32) -> bool {
    if tag < 8 {
        return false;
    }
    tag_wire_type(tag).is_some()
}

// ---------------------------------------------------------------------------
// Canonical varint helpers (private)
// ---------------------------------------------------------------------------

/// Maximum encoded length of a varint32 (5 bytes).
const MAX_VARINT32_LEN: usize = 5;

/// Maximum encoded length of a varint64 (10 bytes).
const MAX_VARINT64_LEN: usize = 10;

/// Reads a canonical varint32 from `data[pos..]`.
///
/// Returns `Some((value, bytes_consumed))` on success, `None` if the varint
/// is missing, truncated, overlong, or non-canonical (last byte is zero in a
/// multi-byte encoding, or more than 5 bytes).
fn read_canonical_varint32(data: &[u8], pos: usize) -> Option<(u32, usize)> {
    let remaining = &data[pos..];
    if remaining.is_empty() {
        return None;
    }

    let mut result: u32 = 0;
    for i in 0..MAX_VARINT32_LEN {
        if i >= remaining.len() {
            // Truncated varint.
            return None;
        }
        let byte = remaining[i];
        let low7 = (byte & 0x7F) as u32;
        // On the 5th byte (i==4), only the lowest 4 bits are valid for u32.
        if i == 4 && low7 > 0x0F {
            return None;
        }
        result |= low7 << (7 * i);

        if byte < 0x80 {
            // Last byte of the varint.
            // Canonical check: in a multi-byte varint, the last byte must not be 0.
            if i > 0 && byte == 0 {
                return None;
            }
            return Some((result, i + 1));
        }
    }
    // More than MAX_VARINT32_LEN bytes with continuation bits set.
    None
}

/// Skips a canonical varint64 at `data[pos..]`.
///
/// Returns the number of bytes consumed, or `None` if the varint is invalid.
fn skip_canonical_varint64(data: &[u8], pos: usize) -> Option<usize> {
    let remaining = &data[pos..];
    if remaining.is_empty() {
        return None;
    }

    for i in 0..MAX_VARINT64_LEN {
        if i >= remaining.len() {
            return None;
        }
        let byte = remaining[i];
        if byte < 0x80 {
            // Canonical check: last byte must not be 0 in multi-byte varint.
            if i > 0 && byte == 0 {
                return None;
            }
            return Some(i + 1);
        }
    }
    // More than 10 bytes.
    None
}

// ---------------------------------------------------------------------------
// Public varint helpers
// ---------------------------------------------------------------------------

/// Reads a canonical varint64 from `data[pos..]`, returning the decoded value
/// and the number of bytes consumed.
///
/// Returns `None` if the varint is missing, truncated, overlong (>10 bytes),
/// or non-canonical (trailing zero byte in a multi-byte encoding).
pub fn read_canonical_varint64(data: &[u8], pos: usize) -> Option<(u64, usize)> {
    if pos > data.len() {
        return None;
    }
    let remaining = &data[pos..];
    if remaining.is_empty() {
        return None;
    }

    let mut result: u64 = 0;
    for i in 0..MAX_VARINT64_LEN {
        if i >= remaining.len() {
            return None;
        }
        let byte = remaining[i];
        let low7 = (byte & 0x7F) as u64;
        // On the 10th byte (i==9), only the lowest bit is valid for u64.
        if i == 9 && low7 > 1 {
            return None;
        }
        result |= low7 << (7 * i);

        if byte < 0x80 {
            // Canonical check: last byte must not be 0 in multi-byte varint.
            if i > 0 && byte == 0 {
                return None;
            }
            return Some((result, i + 1));
        }
    }
    // More than 10 bytes.
    None
}

/// Appends a varint-encoded `u64` value to the given buffer.
pub fn encode_varint64(buf: &mut Vec<u8>, v: u64) {
    let encoded = varint::encode_u64(v);
    buf.extend_from_slice(&encoded);
}

/// Appends a varint-encoded `u32` value to the given buffer.
pub fn encode_varint32(buf: &mut Vec<u8>, v: u32) {
    let encoded = varint::encode_u32(v);
    buf.extend_from_slice(&encoded);
}

/// Appends a proto field tag (field_number + wire_type) as a varint to the
/// given buffer.
pub fn encode_tag(buf: &mut Vec<u8>, field_number: u32, wire_type: WireType) {
    encode_varint32(buf, make_tag(field_number, wire_type));
}

// ---------------------------------------------------------------------------
// Zigzag encoding helpers
// ---------------------------------------------------------------------------

/// Encodes a signed 32-bit integer using zigzag encoding.
///
/// Maps signed integers to unsigned: 0 -> 0, -1 -> 1, 1 -> 2, -2 -> 3, ...
pub fn zigzag_encode_i32(v: i32) -> u32 {
    ((v << 1) ^ (v >> 31)) as u32
}

/// Encodes a signed 64-bit integer using zigzag encoding.
///
/// Maps signed integers to unsigned: 0 -> 0, -1 -> 1, 1 -> 2, -2 -> 3, ...
pub fn zigzag_encode_i64(v: i64) -> u64 {
    ((v << 1) ^ (v >> 63)) as u64
}

// ---------------------------------------------------------------------------
// Proto field iterator
// ---------------------------------------------------------------------------

/// The value component of a decoded proto field.
///
/// For variable-length types (`LengthDelimited`), the value borrows from the
/// input slice. For fixed-width types and varints, the value is stored inline.
/// Group markers (`StartGroup`, `EndGroup`) carry no value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FieldValue<'a> {
    /// A decoded varint value.
    Varint(u64),
    /// A 32-bit fixed-width value (little-endian).
    Fixed32(u32),
    /// A 64-bit fixed-width value (little-endian).
    Fixed64(u64),
    /// A length-delimited byte slice borrowed from the input.
    LengthDelimited(&'a [u8]),
    /// Start of a group (deprecated but valid). No associated value.
    StartGroup,
    /// End of a group (deprecated but valid). No associated value.
    EndGroup,
}

/// A single decoded field from a serialized protobuf message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProtoField<'a> {
    /// The proto field number (1+).
    pub field_number: u32,
    /// The wire type of this field.
    pub wire_type: WireType,
    /// The decoded value.
    pub value: FieldValue<'a>,
}

/// A zero-copy iterator over proto fields in a serialized message.
///
/// Yields `Result<ProtoField, crate::RiegeliError>` for each field encountered.
/// On error (truncation, invalid wire type, etc.), the error is returned once
/// and subsequent calls to `next()` return `None`.
///
/// Group fields (`StartGroup` / `EndGroup`) are yielded as flat events; the
/// iterator does not automatically consume group contents.
pub struct ProtoFieldIter<'a> {
    data: &'a [u8],
    pos: usize,
    errored: bool,
}

impl<'a> ProtoFieldIter<'a> {
    /// Creates a new field iterator over the given serialized proto bytes.
    pub fn new(data: &'a [u8]) -> Self {
        ProtoFieldIter {
            data,
            pos: 0,
            errored: false,
        }
    }
}

impl<'a> Iterator for ProtoFieldIter<'a> {
    type Item = Result<ProtoField<'a>, crate::RiegeliError>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.errored || self.pos >= self.data.len() {
            return None;
        }

        // Read the tag.
        let (tag, tag_len) = match read_canonical_varint32(self.data, self.pos) {
            Some(v) => v,
            None => {
                self.errored = true;
                return Some(Err(crate::RiegeliError::MalformedData(format!(
                    "invalid or truncated tag at offset {}",
                    self.pos
                ))));
            }
        };

        let field_number = tag_field_number(tag);
        if field_number == 0 {
            self.errored = true;
            return Some(Err(crate::RiegeliError::MalformedData(format!(
                "field number 0 at offset {}",
                self.pos
            ))));
        }

        let wire_type = match tag_wire_type(tag) {
            Some(wt) => wt,
            None => {
                self.errored = true;
                return Some(Err(crate::RiegeliError::MalformedData(format!(
                    "invalid wire type {} at offset {}",
                    tag & 7,
                    self.pos
                ))));
            }
        };

        self.pos += tag_len;

        let value = match wire_type {
            WireType::Varint => match read_canonical_varint64(self.data, self.pos) {
                Some((v, consumed)) => {
                    self.pos += consumed;
                    FieldValue::Varint(v)
                }
                None => {
                    self.errored = true;
                    return Some(Err(crate::RiegeliError::MalformedData(format!(
                        "invalid or truncated varint value at offset {}",
                        self.pos
                    ))));
                }
            },
            WireType::Fixed32 => {
                if self.pos + 4 > self.data.len() {
                    self.errored = true;
                    return Some(Err(crate::RiegeliError::MalformedData(format!(
                        "truncated fixed32 at offset {}",
                        self.pos
                    ))));
                }
                let v = u32::from_le_bytes([
                    self.data[self.pos],
                    self.data[self.pos + 1],
                    self.data[self.pos + 2],
                    self.data[self.pos + 3],
                ]);
                self.pos += 4;
                FieldValue::Fixed32(v)
            }
            WireType::Fixed64 => {
                if self.pos + 8 > self.data.len() {
                    self.errored = true;
                    return Some(Err(crate::RiegeliError::MalformedData(format!(
                        "truncated fixed64 at offset {}",
                        self.pos
                    ))));
                }
                let v = u64::from_le_bytes([
                    self.data[self.pos],
                    self.data[self.pos + 1],
                    self.data[self.pos + 2],
                    self.data[self.pos + 3],
                    self.data[self.pos + 4],
                    self.data[self.pos + 5],
                    self.data[self.pos + 6],
                    self.data[self.pos + 7],
                ]);
                self.pos += 8;
                FieldValue::Fixed64(v)
            }
            WireType::LengthDelimited => match read_canonical_varint32(self.data, self.pos) {
                Some((length, consumed)) => {
                    self.pos += consumed;
                    let length = length as usize;
                    if self.pos + length > self.data.len() {
                        self.errored = true;
                        return Some(Err(crate::RiegeliError::MalformedData(format!(
                            "length-delimited field at offset {} declares length {} but only {} bytes remain",
                            self.pos - consumed,
                            length,
                            self.data.len() - self.pos
                        ))));
                    }
                    let slice = &self.data[self.pos..self.pos + length];
                    self.pos += length;
                    FieldValue::LengthDelimited(slice)
                }
                None => {
                    self.errored = true;
                    return Some(Err(crate::RiegeliError::MalformedData(format!(
                        "invalid or truncated length prefix at offset {}",
                        self.pos
                    ))));
                }
            },
            WireType::StartGroup => FieldValue::StartGroup,
            WireType::EndGroup => FieldValue::EndGroup,
        };

        Some(Ok(ProtoField {
            field_number,
            wire_type,
            value,
        }))
    }
}

/// Serializes a `ProtoField` back to wire format, appending to the given buffer.
///
/// This enables round-trip: iterate fields then re-serialize each one.
pub fn serialize_field(buf: &mut Vec<u8>, field: &ProtoField<'_>) {
    encode_tag(buf, field.field_number, field.wire_type);
    match &field.value {
        FieldValue::Varint(v) => encode_varint64(buf, *v),
        FieldValue::Fixed32(v) => buf.extend_from_slice(&v.to_le_bytes()),
        FieldValue::Fixed64(v) => buf.extend_from_slice(&v.to_le_bytes()),
        FieldValue::LengthDelimited(data) => {
            encode_varint32(buf, data.len() as u32);
            buf.extend_from_slice(data);
        }
        FieldValue::StartGroup | FieldValue::EndGroup => {
            // Tag already written, no additional value bytes.
        }
    }
}

// ---------------------------------------------------------------------------
// SerializedMessageWriter
// ---------------------------------------------------------------------------

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
/// use riegeli::proto_wire::SerializedMessageWriter;
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
            return Err(crate::RiegeliError::MalformedData(format!(
                "finish() called with {} unclosed length-delimited scope(s)",
                self.scope_stack.len()
            )));
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
            return Err(crate::RiegeliError::MalformedData(format!(
                "length-delimited field length {} exceeds 2 GiB limit",
                data.len()
            )));
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
    /// - The content exceeds the 2 GiB proto limit.
    pub fn close_length_delimited(&mut self) -> Result<(), crate::RiegeliError> {
        let content_start = self.scope_stack.pop().ok_or_else(|| {
            crate::RiegeliError::MalformedData(
                "close_length_delimited() called without matching open".to_string(),
            )
        })?;

        let content_len = self.buf.len() - content_start;
        if content_len > MAX_LENGTH_DELIMITED {
            return Err(crate::RiegeliError::MalformedData(format!(
                "length-delimited field length {} exceeds 2 GiB limit",
                content_len
            )));
        }

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

// ---------------------------------------------------------------------------
// SerializedMessageReader — Field Handler Framework
// ---------------------------------------------------------------------------

/// Trait for dispatching proto fields during message reading.
///
/// Implementors receive decoded fields and decide whether to handle them.
/// The `dispatch` method returns:
/// - `Ok(true)` if the field was handled,
/// - `Ok(false)` if the field should be skipped (no matching handler),
/// - `Err(...)` to abort reading immediately.
///
/// Both static (compile-time) and dynamic (runtime) handler sets implement
/// this trait, so `read_message` works uniformly with either.
pub trait HandleField {
    /// Dispatch a decoded field to the appropriate handler.
    ///
    /// Returns `Ok(true)` if handled, `Ok(false)` if no handler matched
    /// (the reader will skip the field), or `Err` to abort.
    fn dispatch(&mut self, field: &ProtoField<'_>) -> Result<bool, crate::RiegeliError>;
}

/// A handler for a specific proto field number, using the `FieldHandler` trait
/// for static (compile-time) dispatch.
///
/// Implement this trait to handle fields with a known field number. Override
/// only the wire-type methods you care about; unimplemented methods default
/// to `Ok(())` (skip).
pub trait FieldHandler {
    /// The proto field number this handler is registered for.
    const FIELD_NUMBER: u32;

    /// Called when a varint field is encountered.
    fn handle_varint(&mut self, _value: u64) -> Result<(), crate::RiegeliError> {
        Ok(())
    }

    /// Called when a fixed32 field is encountered.
    fn handle_fixed32(&mut self, _value: u32) -> Result<(), crate::RiegeliError> {
        Ok(())
    }

    /// Called when a fixed64 field is encountered.
    fn handle_fixed64(&mut self, _value: u64) -> Result<(), crate::RiegeliError> {
        Ok(())
    }

    /// Called when a length-delimited field is encountered.
    fn handle_length_delimited(&mut self, _data: &[u8]) -> Result<(), crate::RiegeliError> {
        Ok(())
    }

    /// Called when a start-group tag is encountered.
    fn handle_start_group(&mut self) -> Result<(), crate::RiegeliError> {
        Ok(())
    }

    /// Called when an end-group tag is encountered.
    fn handle_end_group(&mut self) -> Result<(), crate::RiegeliError> {
        Ok(())
    }
}

/// Dispatches a `ProtoField` to a `FieldHandler` if the field number matches.
///
/// Returns `Ok(true)` if the field was handled, `Ok(false)` if the field number
/// didn't match.
fn dispatch_to_handler<H: FieldHandler>(
    handler: &mut H,
    field: &ProtoField<'_>,
) -> Result<bool, crate::RiegeliError> {
    if field.field_number != H::FIELD_NUMBER {
        return Ok(false);
    }
    match &field.value {
        FieldValue::Varint(v) => handler.handle_varint(*v)?,
        FieldValue::Fixed32(v) => handler.handle_fixed32(*v)?,
        FieldValue::Fixed64(v) => handler.handle_fixed64(*v)?,
        FieldValue::LengthDelimited(data) => handler.handle_length_delimited(data)?,
        FieldValue::StartGroup => handler.handle_start_group()?,
        FieldValue::EndGroup => handler.handle_end_group()?,
    }
    Ok(true)
}

/// A static handler set holding one handler.
///
/// Use `StaticHandlerSet::new(handler)` and chain `.and(handler2)` to build
/// a set of up to any number of handlers. Each handler is dispatched by its
/// compile-time `FIELD_NUMBER`.
pub struct StaticHandlerSet1<H1: FieldHandler> {
    /// The handler.
    pub h1: H1,
}

impl<H1: FieldHandler> HandleField for StaticHandlerSet1<H1> {
    fn dispatch(&mut self, field: &ProtoField<'_>) -> Result<bool, crate::RiegeliError> {
        dispatch_to_handler(&mut self.h1, field)
    }
}

/// A static handler set holding two handlers.
pub struct StaticHandlerSet2<H1: FieldHandler, H2: FieldHandler> {
    /// The first handler.
    pub h1: H1,
    /// The second handler.
    pub h2: H2,
}

impl<H1: FieldHandler, H2: FieldHandler> HandleField for StaticHandlerSet2<H1, H2> {
    fn dispatch(&mut self, field: &ProtoField<'_>) -> Result<bool, crate::RiegeliError> {
        if dispatch_to_handler(&mut self.h1, field)? {
            return Ok(true);
        }
        dispatch_to_handler(&mut self.h2, field)
    }
}

/// A static handler set holding three handlers.
pub struct StaticHandlerSet3<H1: FieldHandler, H2: FieldHandler, H3: FieldHandler> {
    /// The first handler.
    pub h1: H1,
    /// The second handler.
    pub h2: H2,
    /// The third handler.
    pub h3: H3,
}

impl<H1: FieldHandler, H2: FieldHandler, H3: FieldHandler> HandleField
    for StaticHandlerSet3<H1, H2, H3>
{
    fn dispatch(&mut self, field: &ProtoField<'_>) -> Result<bool, crate::RiegeliError> {
        if dispatch_to_handler(&mut self.h1, field)? {
            return Ok(true);
        }
        if dispatch_to_handler(&mut self.h2, field)? {
            return Ok(true);
        }
        dispatch_to_handler(&mut self.h3, field)
    }
}

/// Builder for creating static handler sets.
pub struct StaticHandlerSet;

impl StaticHandlerSet {
    /// Creates a handler set with one handler.
    pub fn new<H1: FieldHandler>(h1: H1) -> StaticHandlerSet1<H1> {
        StaticHandlerSet1 { h1 }
    }
}

impl<H1: FieldHandler> StaticHandlerSet1<H1> {
    /// Adds a second handler to the set.
    pub fn and<H2: FieldHandler>(self, h2: H2) -> StaticHandlerSet2<H1, H2> {
        StaticHandlerSet2 { h1: self.h1, h2 }
    }
}

impl<H1: FieldHandler, H2: FieldHandler> StaticHandlerSet2<H1, H2> {
    /// Adds a third handler to the set.
    pub fn and<H3: FieldHandler>(self, h3: H3) -> StaticHandlerSet3<H1, H2, H3> {
        StaticHandlerSet3 {
            h1: self.h1,
            h2: self.h2,
            h3,
        }
    }
}

// ---------------------------------------------------------------------------
// Dynamic handler dispatch
// ---------------------------------------------------------------------------

/// A key for dynamic handler dispatch: `(field_number, wire_type_raw)`.
///
/// The raw wire type value (0..=5) is used rather than `WireType` to avoid
/// needing `Hash`/`Eq` on the enum.
type DynamicHandlerKey = (u32, u8);

/// A dynamic handler set that maps `(field_number, wire_type)` pairs to
/// boxed closures at runtime.
///
/// Each closure receives the decoded `FieldValue` and returns
/// `Result<(), RiegeliError>`. Register handlers with `on_varint`,
/// `on_fixed32`, etc., then pass to `read_message`.
///
/// # Example
///
/// ```
/// use riegeli::proto_wire::{DynamicHandlerSet, read_message, SerializedMessageWriter};
///
/// let mut w = SerializedMessageWriter::new();
/// w.write_uint64(1, 42).unwrap();
/// let data = w.finish().unwrap();
///
/// let mut result = 0u64;
/// {
///     let mut handlers = DynamicHandlerSet::new();
///     handlers.on_varint(1, |v| { result = v; Ok(()) });
///     read_message(&data, &mut handlers).unwrap();
/// }
/// assert_eq!(result, 42);
/// ```
pub struct DynamicHandlerSet<'a> {
    handlers: HashMap<
        DynamicHandlerKey,
        Box<dyn FnMut(&FieldValue<'_>) -> Result<(), crate::RiegeliError> + 'a>,
    >,
}

impl<'a> DynamicHandlerSet<'a> {
    /// Creates an empty dynamic handler set.
    pub fn new() -> Self {
        DynamicHandlerSet {
            handlers: HashMap::new(),
        }
    }

    /// Registers a handler for varint fields with the given field number.
    pub fn on_varint<F>(&mut self, field_number: u32, mut f: F)
    where
        F: FnMut(u64) -> Result<(), crate::RiegeliError> + 'a,
    {
        self.handlers.insert(
            (field_number, WireType::Varint as u8),
            Box::new(move |value| {
                if let FieldValue::Varint(v) = value {
                    f(*v)
                } else {
                    Ok(())
                }
            }),
        );
    }

    /// Registers a handler for fixed32 fields with the given field number.
    pub fn on_fixed32<F>(&mut self, field_number: u32, mut f: F)
    where
        F: FnMut(u32) -> Result<(), crate::RiegeliError> + 'a,
    {
        self.handlers.insert(
            (field_number, WireType::Fixed32 as u8),
            Box::new(move |value| {
                if let FieldValue::Fixed32(v) = value {
                    f(*v)
                } else {
                    Ok(())
                }
            }),
        );
    }

    /// Registers a handler for fixed64 fields with the given field number.
    pub fn on_fixed64<F>(&mut self, field_number: u32, mut f: F)
    where
        F: FnMut(u64) -> Result<(), crate::RiegeliError> + 'a,
    {
        self.handlers.insert(
            (field_number, WireType::Fixed64 as u8),
            Box::new(move |value| {
                if let FieldValue::Fixed64(v) = value {
                    f(*v)
                } else {
                    Ok(())
                }
            }),
        );
    }

    /// Registers a handler for length-delimited fields with the given field number.
    pub fn on_length_delimited<F>(&mut self, field_number: u32, mut f: F)
    where
        F: FnMut(&[u8]) -> Result<(), crate::RiegeliError> + 'a,
    {
        self.handlers.insert(
            (field_number, WireType::LengthDelimited as u8),
            Box::new(move |value| {
                if let FieldValue::LengthDelimited(data) = value {
                    f(data)
                } else {
                    Ok(())
                }
            }),
        );
    }

    /// Registers a handler for start-group fields with the given field number.
    pub fn on_start_group<F>(&mut self, field_number: u32, mut f: F)
    where
        F: FnMut() -> Result<(), crate::RiegeliError> + 'a,
    {
        self.handlers.insert(
            (field_number, WireType::StartGroup as u8),
            Box::new(move |value| {
                if let FieldValue::StartGroup = value {
                    f()
                } else {
                    Ok(())
                }
            }),
        );
    }

    /// Registers a handler for end-group fields with the given field number.
    pub fn on_end_group<F>(&mut self, field_number: u32, mut f: F)
    where
        F: FnMut() -> Result<(), crate::RiegeliError> + 'a,
    {
        self.handlers.insert(
            (field_number, WireType::EndGroup as u8),
            Box::new(move |value| {
                if let FieldValue::EndGroup = value {
                    f()
                } else {
                    Ok(())
                }
            }),
        );
    }
}

impl<'a> Default for DynamicHandlerSet<'a> {
    fn default() -> Self {
        Self::new()
    }
}

impl<'a> HandleField for DynamicHandlerSet<'a> {
    fn dispatch(&mut self, field: &ProtoField<'_>) -> Result<bool, crate::RiegeliError> {
        let wire_type_raw = field.wire_type as u8;
        let key = (field.field_number, wire_type_raw);
        if let Some(handler) = self.handlers.get_mut(&key) {
            handler(&field.value)?;
            Ok(true)
        } else {
            Ok(false)
        }
    }
}

/// An empty handler set that skips all fields.
///
/// This is useful as a no-op reader that validates message structure without
/// processing any fields.
pub struct EmptyHandlerSet;

impl HandleField for EmptyHandlerSet {
    fn dispatch(&mut self, _field: &ProtoField<'_>) -> Result<bool, crate::RiegeliError> {
        Ok(false)
    }
}

/// Reads a serialized proto message, dispatching each field to the given
/// handler set.
///
/// Fields not matched by any handler are silently skipped. If a handler
/// returns an error, reading stops immediately and the error is propagated.
///
/// Uses `ProtoFieldIter` internally, so all canonical varint and wire format
/// validation applies.
pub fn read_message<H: HandleField>(
    data: &[u8],
    handlers: &mut H,
) -> Result<(), crate::RiegeliError> {
    let iter = ProtoFieldIter::new(data);
    for result in iter {
        let field = result?;
        handlers.dispatch(&field)?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Field Filtering
// ---------------------------------------------------------------------------

/// A filtered iterator over proto fields that yields only fields whose field
/// numbers appear in a given set.
///
/// Fields not in the allowed set are skipped without copying their bytes.
/// The underlying `ProtoFieldIter` still parses and advances past skipped
/// fields (this is necessary to find the next field boundary), but their
/// values are not delivered to the caller.
///
/// # Example
///
/// ```
/// use riegeli::proto_wire::{FilteredFieldIter, SerializedMessageWriter};
///
/// let mut w = SerializedMessageWriter::new();
/// w.write_uint64(1, 10).unwrap();
/// w.write_uint64(2, 20).unwrap();
/// w.write_uint64(3, 30).unwrap();
/// let data = w.finish().unwrap();
///
/// let fields: Vec<_> = FilteredFieldIter::new(&data, &[1, 3])
///     .collect::<Result<Vec<_>, _>>()
///     .unwrap();
/// assert_eq!(fields.len(), 2);
/// assert_eq!(fields[0].field_number, 1);
/// assert_eq!(fields[1].field_number, 3);
/// ```
pub struct FilteredFieldIter<'a> {
    inner: ProtoFieldIter<'a>,
    allowed: &'a [u32],
}

impl<'a> FilteredFieldIter<'a> {
    /// Creates a new filtered iterator that yields only fields whose field
    /// numbers appear in `allowed`.
    ///
    /// The `allowed` slice does not need to be sorted; membership is checked
    /// via linear scan, which is efficient for the small sets typical in proto
    /// field filtering.
    pub fn new(data: &'a [u8], allowed: &'a [u32]) -> Self {
        FilteredFieldIter {
            inner: ProtoFieldIter::new(data),
            allowed,
        }
    }
}

impl<'a> Iterator for FilteredFieldIter<'a> {
    type Item = Result<ProtoField<'a>, crate::RiegeliError>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            match self.inner.next() {
                Some(Ok(field)) => {
                    if self.allowed.contains(&field.field_number) {
                        return Some(Ok(field));
                    }
                    // Skip this field — continue to the next one.
                }
                Some(Err(e)) => return Some(Err(e)),
                None => return None,
            }
        }
    }
}

/// Copies selected fields from a source message to a writer.
///
/// Iterates over the source bytes, and for each field whose field number is in
/// `field_numbers`, writes it to `writer` using `write_field`. Fields not in
/// the set are skipped. The output preserves the ordering of fields from the
/// source message.
///
/// # Errors
///
/// Returns an error if the source bytes are malformed or if writing to the
/// writer fails.
pub fn copy_fields(
    source: &[u8],
    field_numbers: &[u32],
    writer: &mut SerializedMessageWriter,
) -> Result<(), crate::RiegeliError> {
    let iter = FilteredFieldIter::new(source, field_numbers);
    for result in iter {
        let field = result?;
        writer.write_field(&field)?;
    }
    Ok(())
}

/// Validates that `data` is a canonical proto binary encoding.
///
/// Returns `true` if:
/// - All varints are canonically encoded (no overlong encodings).
/// - All started groups are properly closed with matching EndGroup tags.
/// - Length-delimited fields do not overflow the buffer.
/// - All field numbers are non-zero.
/// - No wire types 6 or 7 appear.
///
/// An empty slice is considered a valid (empty) proto message.
///
/// This matches the C++ `IsProtoMessage` function in `transpose_encoder.cc`.
pub fn is_proto_message(data: &[u8]) -> bool {
    let mut pos: usize = 0;
    let mut started_groups: Vec<u32> = Vec::new();

    while pos < data.len() {
        // Read canonical varint32 tag.
        let (tag, consumed) = match read_canonical_varint32(data, pos) {
            Some(v) => v,
            None => return false,
        };
        pos += consumed;

        let field_number = tag_field_number(tag);
        if field_number == 0 {
            return false;
        }

        let Some(wire_type) = tag_wire_type(tag) else {
            // Wire types 6 and 7 are invalid.
            return false;
        };

        match wire_type {
            WireType::Varint => {
                // Varint: skip a canonical varint64 value.
                match skip_canonical_varint64(data, pos) {
                    Some(n) => pos += n,
                    None => return false,
                }
            }
            WireType::Fixed32 => {
                // Fixed32: skip 4 bytes.
                if pos + 4 > data.len() {
                    return false;
                }
                pos += 4;
            }
            WireType::Fixed64 => {
                // Fixed64: skip 8 bytes.
                if pos + 8 > data.len() {
                    return false;
                }
                pos += 8;
            }
            WireType::LengthDelimited => {
                // Length-delimited: read canonical varint32 length, then skip.
                let (length, consumed) = match read_canonical_varint32(data, pos) {
                    Some(v) => v,
                    None => return false,
                };
                pos += consumed;
                if pos + (length as usize) > data.len() {
                    return false;
                }
                pos += length as usize;
            }
            WireType::StartGroup => {
                // StartGroup: push field number.
                started_groups.push(field_number);
            }
            WireType::EndGroup => {
                // EndGroup: must match most recent StartGroup.
                if started_groups.is_empty() || *started_groups.last().unwrap() != field_number {
                    return false;
                }
                started_groups.pop();
            }
        }
    }

    started_groups.is_empty()
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- tag composition/decomposition ----

    #[test]
    fn test_make_tag_and_decompose() {
        // Criterion 8.1
        let tag = make_tag(1, WireType::Varint);
        assert_eq!(tag_wire_type(tag), Some(WireType::Varint));
        assert_eq!(tag_field_number(tag), 1);

        let tag2 = make_tag(5, WireType::Fixed32);
        assert_eq!(tag_wire_type(tag2), Some(WireType::Fixed32));
        assert_eq!(tag_field_number(tag2), 5);
    }

    #[test]
    fn test_tag_values() {
        // Varint field 1 = 0x08
        assert_eq!(make_tag(1, WireType::Varint), 0x08);
        // Fixed64 field 1 = 0x09
        assert_eq!(make_tag(1, WireType::Fixed64), 0x09);
        // LengthDelimited field 2 = 0x12
        assert_eq!(make_tag(2, WireType::LengthDelimited), 0x12);
    }

    #[test]
    fn test_wire_type_from_raw_invalid() {
        assert_eq!(WireType::from_raw(6), None);
        assert_eq!(WireType::from_raw(7), None);
        assert_eq!(tag_wire_type(6), None); // wire type 6
        assert_eq!(tag_wire_type(7), None); // wire type 7
    }

    // ---- is_proto_message ----

    #[test]
    fn test_empty_is_valid() {
        // Criterion 8.4
        assert!(is_proto_message(b""));
    }

    #[test]
    fn test_valid_proto_mixed_fields() {
        // Criterion 8.5
        // varint field 1 = 1: tag=0x08, value=0x01
        // fixed64 field 1: tag=0x09, 8 bytes
        // length-delimited field 2: tag=0x12, length=3, "abc"
        let mut data = Vec::new();
        // Field 1, varint, value 1
        data.push(0x08);
        data.push(0x01);
        // Field 1, fixed64, value = 0
        data.push(0x09);
        data.extend_from_slice(&[0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]);
        // Field 2, length-delimited, "abc"
        data.push(0x12);
        data.push(0x03);
        data.extend_from_slice(b"abc");
        assert!(is_proto_message(&data));
    }

    #[test]
    fn test_overlong_varint_rejected() {
        // Criterion 8.6
        // [0x80, 0x80, 0x00] is an overlong varint encoding of 0 — the last
        // byte is 0 in a multi-byte varint, which is non-canonical.
        // We wrap it as a varint field: tag=0x08 (field 1, varint), then the bad varint.
        let data = [0x08, 0x80, 0x80, 0x00];
        assert!(!is_proto_message(&data));
    }

    #[test]
    fn test_overlong_varint_tag_rejected() {
        // A tag encoded as [0x80, 0x00] is an overlong encoding of 0 —
        // non-canonical tag.
        let data = [0x80, 0x00];
        assert!(!is_proto_message(&data));
    }

    #[test]
    fn test_unclosed_start_group() {
        // Criterion 8.7
        // StartGroup for field 1: tag = (1 << 3) | 3 = 0x0B
        let data = [0x0B];
        assert!(!is_proto_message(&data));
    }

    #[test]
    fn test_mismatched_end_group() {
        // StartGroup field 1, EndGroup field 2 — mismatch.
        let data = [0x0B, 0x14]; // start field 1, end field 2
        assert!(!is_proto_message(&data));
    }

    #[test]
    fn test_matched_group() {
        // StartGroup field 1, EndGroup field 1 — valid.
        let data = [0x0B, 0x0C]; // (1<<3)|3=0x0B start, (1<<3)|4=0x0C end
        assert!(is_proto_message(&data));
    }

    #[test]
    fn test_wire_type_6_rejected() {
        // Criterion 8.8
        // Wire type 6 for field 1: (1 << 3) | 6 = 0x0E
        let data = [0x0E];
        assert!(!is_proto_message(&data));
    }

    #[test]
    fn test_wire_type_7_rejected() {
        // Wire type 7 for field 1: (1 << 3) | 7 = 0x0F
        let data = [0x0F];
        assert!(!is_proto_message(&data));
    }

    #[test]
    fn test_field_number_zero_rejected() {
        // Tag with field_number=0 is invalid. tag=0x00 is varint with field 0.
        let data = [0x00];
        assert!(!is_proto_message(&data));
    }

    #[test]
    fn test_truncated_fixed32() {
        // Fixed32 field 1 = tag 0x0D, but only 2 bytes of data.
        let data = [0x0D, 0x00, 0x00];
        assert!(!is_proto_message(&data));
    }

    #[test]
    fn test_truncated_fixed64() {
        // Fixed64 field 1 = tag 0x09, but only 4 bytes.
        let data = [0x09, 0x00, 0x00, 0x00, 0x00];
        assert!(!is_proto_message(&data));
    }

    #[test]
    fn test_length_delimited_overflow() {
        // Field 2 length-delimited, length=100 but only 3 bytes available.
        let data = [0x12, 0x64, 0x00, 0x00, 0x00];
        assert!(!is_proto_message(&data));
    }

    #[test]
    fn test_truncated_varint_at_end() {
        // A tag byte with continuation bit set, but no following byte.
        let data = [0x88];
        assert!(!is_proto_message(&data));
    }

    #[test]
    fn test_valid_fixed32_field() {
        // Fixed32 field 1: tag=0x0D, 4 bytes
        let data = [0x0D, 0x01, 0x02, 0x03, 0x04];
        assert!(is_proto_message(&data));
    }

    #[test]
    fn test_too_long_varint32_tag() {
        // A varint that is 6 bytes long cannot be a valid uint32 tag.
        let data = [0x80, 0x80, 0x80, 0x80, 0x80, 0x01];
        assert!(!is_proto_message(&data));
    }

    #[test]
    fn test_end_group_without_start() {
        // EndGroup for field 1 without a preceding StartGroup.
        let data = [0x0C]; // (1 << 3) | 4
        assert!(!is_proto_message(&data));
    }

    #[test]
    fn test_nested_groups() {
        // StartGroup field 1, StartGroup field 2, EndGroup field 2, EndGroup field 1.
        let data = [0x0B, 0x13, 0x14, 0x0C];
        assert!(is_proto_message(&data));
    }

    #[test]
    fn test_varint_max_length_valid() {
        // A 10-byte varint64 value that is canonical (last byte != 0).
        // This is a varint encoding of u64::MAX.
        let mut data = vec![0x08]; // tag: field 1, varint
        data.extend_from_slice(&[0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x01]);
        assert!(is_proto_message(&data));
    }

    #[test]
    fn test_varint_11_bytes_rejected() {
        // An 11-byte varint is always invalid.
        let mut data = vec![0x08]; // tag: field 1, varint
        data.extend_from_slice(&[0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF]);
        // 10 continuation bytes — too long.
        assert!(!is_proto_message(&data));
    }

    #[test]
    fn test_tag_round_trip_all_wire_types() {
        let wire_types = [
            WireType::Varint,
            WireType::Fixed64,
            WireType::LengthDelimited,
            WireType::StartGroup,
            WireType::EndGroup,
            WireType::Fixed32,
        ];
        for &wt in &wire_types {
            for field in [1u32, 2, 127, 1000, 0x1FFFFFFF] {
                let tag = make_tag(field, wt);
                assert_eq!(tag_wire_type(tag), Some(wt));
                assert_eq!(tag_field_number(tag), field);
            }
        }
    }

    #[test]
    fn test_unclosed_group_rejected() {
        // StartGroup with no EndGroup
        assert!(!is_proto_message(&[0x0B]));
        // Mismatched field numbers
        assert!(!is_proto_message(&[0x0B, 0x14]));
        // Nested: outer group still open
        assert!(!is_proto_message(&[0x0B, 0x13, 0x14]));
        // Extra EndGroup after closed group
        assert!(!is_proto_message(&[0x0B, 0x0C, 0x0C]));
    }

    #[test]
    fn test_invalid_wire_type_rejected() {
        assert!(!is_proto_message(&[0x0E])); // wire type 6
        assert!(!is_proto_message(&[0x0F])); // wire type 7
        assert!(!is_proto_message(&[0xA6, 0x06])); // field 100, wire type 6
        assert!(!is_proto_message(&[0x08, 0x01, 0x0F])); // valid field then wire type 7
    }

    #[test]
    fn test_malformed_input_no_panic_truncated() {
        let valid = [
            0x08, 0x96, 0x01, 0x12, 0x03, b'a', b'b', b'c', 0x0D, 0x01, 0x02, 0x03, 0x04,
        ];
        for i in 1..valid.len() {
            let _ = is_proto_message(&valid[..i]);
        }
    }

    #[test]
    fn test_malformed_input_no_panic_single_bytes() {
        for b in 0u8..=255 {
            let _ = is_proto_message(&[b]);
        }
    }

    #[test]
    fn test_malformed_input_no_panic_random() {
        let cases: &[&[u8]] = &[
            &[0xFF, 0xFF, 0xFF, 0xFF, 0xFF],
            &[0x00],
            &[0x80],
            &[0x80, 0x80, 0x80, 0x80, 0x80, 0x80],
            &[
                0x08, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80,
            ],
        ];
        for case in cases {
            let _ = is_proto_message(case);
        }
    }

    // -------------------------------------------------------------------------
    // Sprint 8 adversarial: additional proto wire tests
    // -------------------------------------------------------------------------

    #[test]
    fn test_fixed64_field_valid() {
        // Field 1, fixed64 (8 zero bytes)
        let mut data = vec![0x09];
        data.extend_from_slice(&[0x00; 8]);
        assert!(is_proto_message(&data));
    }

    #[test]
    fn test_length_delimited_field_valid() {
        // Field 2, length-delimited, "abc"
        assert!(is_proto_message(&[0x12, 0x03, b'a', b'b', b'c']));
    }

    #[test]
    fn test_mixed_valid_fields_all_types() {
        let mut data = Vec::new();
        // Varint field 1 = 150
        data.extend_from_slice(&[0x08, 0x96, 0x01]);
        // Fixed64 field 2 (all 0xFF)
        data.push(0x11);
        data.extend_from_slice(&[0xFF; 8]);
        // Length-delimited field 3, empty
        data.push(0x1A);
        data.push(0x00);
        // Fixed32 field 4 (0xAA bytes)
        data.push(0x25);
        data.extend_from_slice(&[0xAA; 4]);
        // Group field 5: start + end
        data.push(0x2B); // (5 << 3) | 3
        data.push(0x2C); // (5 << 3) | 4
        assert!(is_proto_message(&data));
    }
}
