//! Zero-copy proto field iterator and filtered iterator.

use super::wire::{
    WireType, encode_tag, encode_varint32, encode_varint64, read_canonical_varint32,
    read_canonical_varint64, tag_field_number, tag_wire_type,
};

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
                return Some(Err(crate::RiegeliError::MalformedData(
                    format!("invalid or truncated tag at offset {}", self.pos).into(),
                )));
            }
        };

        let field_number = tag_field_number(tag);
        if field_number == 0 {
            self.errored = true;
            return Some(Err(crate::RiegeliError::MalformedData(
                format!("field number 0 at offset {}", self.pos).into(),
            )));
        }

        let wire_type = match tag_wire_type(tag) {
            Some(wt) => wt,
            None => {
                self.errored = true;
                return Some(Err(crate::RiegeliError::MalformedData(
                    format!("invalid wire type {} at offset {}", tag & 7, self.pos).into(),
                )));
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
                    return Some(Err(crate::RiegeliError::MalformedData(
                        format!("invalid or truncated varint value at offset {}", self.pos).into(),
                    )));
                }
            },
            WireType::Fixed32 => {
                if self.pos + 4 > self.data.len() {
                    self.errored = true;
                    return Some(Err(crate::RiegeliError::MalformedData(
                        format!("truncated fixed32 at offset {}", self.pos).into(),
                    )));
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
                    return Some(Err(crate::RiegeliError::MalformedData(
                        format!("truncated fixed64 at offset {}", self.pos).into(),
                    )));
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
                        ).into())));
                    }
                    let slice = &self.data[self.pos..self.pos + length];
                    self.pos += length;
                    FieldValue::LengthDelimited(slice)
                }
                None => {
                    self.errored = true;
                    return Some(Err(crate::RiegeliError::MalformedData(
                        format!("invalid or truncated length prefix at offset {}", self.pos).into(),
                    )));
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
/// use riegeli::proto::{FilteredFieldIter, SerializedMessageWriter};
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
    writer: &mut super::writer::SerializedMessageWriter,
) -> Result<(), crate::RiegeliError> {
    let iter = FilteredFieldIter::new(source, field_numbers);
    for result in iter {
        let field = result?;
        writer.write_field(&field)?;
    }
    Ok(())
}
