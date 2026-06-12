//! Zero-copy proto field iterator and filtered iterator.

use super::wire::{
    encode_tag, encode_varint32, encode_varint64, read_canonical_varint32, read_canonical_varint64,
    tag_field_number, tag_wire_type, WireType,
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
                    // Checked add: `length` is attacker-controlled (up to
                    // u32::MAX); on a 32-bit target a wrapping add would let
                    // the truncation check pass and the slice below panic.
                    let end = self
                        .pos
                        .checked_add(length)
                        .filter(|&end| end <= self.data.len());
                    let end = match end {
                        Some(end) => end,
                        None => {
                            self.errored = true;
                            return Some(Err(crate::RiegeliError::MalformedData(format!(
                                "length-delimited field at offset {} declares length {} but only {} bytes remain",
                                self.pos - consumed,
                                length,
                                self.data.len() - self.pos
                            ).into())));
                        }
                    };
                    let slice = &self.data[self.pos..end];
                    self.pos = end;
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

#[cfg(test)]
mod tests {
    use super::*;

    // ---- field number boundaries ----

    #[test]
    fn field_number_max_29bit_parsed() {
        // Max proto field number is 2^29 - 1 = 536870911 (0x1FFFFFFF), which
        // requires a 5-byte tag varint.
        let field_num: u32 = 0x1FFFFFFF;
        let mut data = Vec::new();
        encode_tag(&mut data, field_num, WireType::Varint);
        encode_varint64(&mut data, 42);

        let fields: Vec<_> = ProtoFieldIter::new(&data)
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(fields.len(), 1);
        assert_eq!(fields[0].field_number, field_num);
        assert_eq!(fields[0].value, FieldValue::Varint(42));
    }

    #[test]
    fn field_number_zero_rejected() {
        // Field number 0 is invalid; the iterator must yield an error.
        let data = [0x00];
        let results: Vec<_> = ProtoFieldIter::new(&data).collect();
        assert_eq!(results.len(), 1);
        assert!(results[0].is_err());
    }

    // ---- varint boundary values ----

    #[test]
    fn varint_u64_max_parsed() {
        // The 10-byte varint maximum must decode to the exact value.
        let mut data = Vec::new();
        encode_tag(&mut data, 1, WireType::Varint);
        encode_varint64(&mut data, u64::MAX);

        let fields: Vec<_> = ProtoFieldIter::new(&data)
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(fields[0].value, FieldValue::Varint(u64::MAX));
    }

    #[test]
    fn varint_length_boundaries_round_trip() {
        // Values at every varint encoding-length transition must parse to the
        // correct value and re-serialize byte-identically.
        let boundary_values: &[u64] = &[
            127,       // max 1-byte
            128,       // min 2-byte
            16383,     // max 2-byte
            16384,     // min 3-byte
            2097151,   // max 3-byte
            2097152,   // min 4-byte
            268435455, // max 4-byte
            268435456, // min 5-byte
        ];

        for &val in boundary_values {
            let mut data = Vec::new();
            encode_tag(&mut data, 1, WireType::Varint);
            encode_varint64(&mut data, val);

            let fields: Vec<_> = ProtoFieldIter::new(&data)
                .collect::<Result<Vec<_>, _>>()
                .unwrap();
            assert_eq!(
                fields[0].value,
                FieldValue::Varint(val),
                "boundary value {} failed",
                val
            );

            let mut reserialized = Vec::new();
            serialize_field(&mut reserialized, &fields[0]);
            assert_eq!(
                data, reserialized,
                "round-trip failed for boundary value {}",
                val
            );
        }
    }

    // ---- non-canonical varints rejected ----

    #[test]
    fn overlong_varint_tag_rejected() {
        // Tag encoded as [0x80, 0x00] is an overlong encoding of 0; the
        // iterator must yield exactly one error.
        let data = [0x80, 0x00];
        let results: Vec<_> = ProtoFieldIter::new(&data).collect();
        assert_eq!(results.len(), 1);
        assert!(results[0].is_err());
    }

    #[test]
    fn varint_11_bytes_rejected() {
        // A varint value longer than 10 bytes is always invalid.
        let mut data = vec![0x08]; // field 1, varint
        data.extend_from_slice(&[0xFF; 10]); // 10 continuation bytes, no terminator
        let results: Vec<_> = ProtoFieldIter::new(&data).collect();
        assert_eq!(results.len(), 1);
        assert!(results[0].is_err());
    }

    #[test]
    fn noncanonical_ten_byte_varint_rejected() {
        // A 10-byte varint whose final byte is 0x00 is non-canonical and must
        // surface as an error from the iterator.
        let mut data = vec![0x08]; // tag: field 1, varint
        data.extend_from_slice(&[0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x00]);

        let results: Vec<_> = ProtoFieldIter::new(&data).collect();
        assert_eq!(results.len(), 1);
        assert!(
            results[0].is_err(),
            "10-byte varint with trailing 0x00 should be rejected as non-canonical"
        );
    }

    #[test]
    fn tag_varint32_overflow_rejected() {
        // A 5-byte tag varint whose 5th byte carries bits beyond u32 range
        // must be rejected.
        let data = [0x80, 0x80, 0x80, 0x80, 0x10];
        let results: Vec<_> = ProtoFieldIter::new(&data).collect();
        assert_eq!(results.len(), 1);
        assert!(
            results[0].is_err(),
            "varint32 overflow on 5th byte should error"
        );
    }

    // ---- fixed types truncated exactly one byte short ----

    #[test]
    fn fixed32_truncated_by_one_rejected() {
        // 3 of 4 data bytes present: exactly one byte short of the fixed32
        // bounds check.
        let mut data = Vec::new();
        encode_tag(&mut data, 1, WireType::Fixed32);
        data.extend_from_slice(&[0xFF, 0xFF, 0xFF]); // only 3 bytes

        let results: Vec<_> = ProtoFieldIter::new(&data).collect();
        assert_eq!(results.len(), 1);
        assert!(results[0].is_err());
    }

    #[test]
    fn fixed64_truncated_by_one_rejected() {
        // 7 of 8 data bytes present: exactly one byte short of the fixed64
        // bounds check.
        let mut data = Vec::new();
        encode_tag(&mut data, 1, WireType::Fixed64);
        data.extend_from_slice(&[0xFF; 7]); // only 7 bytes

        let results: Vec<_> = ProtoFieldIter::new(&data).collect();
        assert_eq!(results.len(), 1);
        assert!(results[0].is_err());
    }

    // ---- length-delimited edge cases ----

    #[test]
    fn truncated_length_prefix_rejected() {
        // The length-prefix varint itself is truncated (continuation bit set
        // with no following byte).
        let mut data = Vec::new();
        encode_tag(&mut data, 1, WireType::LengthDelimited);
        data.push(0x80); // continuation bit set, no next byte

        let results: Vec<_> = ProtoFieldIter::new(&data).collect();
        assert_eq!(results.len(), 1);
        assert!(results[0].is_err());
    }

    #[test]
    fn length_delimited_multibyte_length_round_trip() {
        // A 300-byte payload forces a 2-byte length-prefix varint through
        // serialize_field's length encoding path.
        let payload = vec![0xABu8; 300];
        let mut original = Vec::new();
        encode_tag(&mut original, 1, WireType::LengthDelimited);
        encode_varint32(&mut original, 300);
        original.extend_from_slice(&payload);

        let fields: Vec<_> = ProtoFieldIter::new(&original)
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(fields.len(), 1);
        match &fields[0].value {
            FieldValue::LengthDelimited(data) => assert_eq!(data.len(), 300),
            _ => panic!("expected LengthDelimited"),
        }

        let mut reserialized = Vec::new();
        for f in &fields {
            serialize_field(&mut reserialized, f);
        }
        assert_eq!(original, reserialized);
    }

    // ---- groups ----

    #[test]
    fn end_group_without_start_yielded_flat() {
        // The iterator does not validate group balance: an EndGroup with no
        // matching StartGroup is yielded as a flat event (unlike
        // is_proto_message, which rejects this input).
        let mut data = Vec::new();
        encode_tag(&mut data, 1, WireType::EndGroup);

        let fields: Vec<_> = ProtoFieldIter::new(&data)
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(fields.len(), 1);
        assert_eq!(fields[0].wire_type, WireType::EndGroup);
        assert_eq!(fields[0].field_number, 1);
    }

    // ---- repeated fields ----

    #[test]
    fn repeated_field_number_all_yielded() {
        // Repeated occurrences of the same field number are all yielded; the
        // iterator performs no deduplication.
        let mut data = Vec::new();
        for _ in 0..5 {
            encode_tag(&mut data, 1, WireType::Varint);
            encode_varint64(&mut data, 42);
        }

        let fields: Vec<_> = ProtoFieldIter::new(&data)
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(fields.len(), 5);
        for f in &fields {
            assert_eq!(f.field_number, 1);
            assert_eq!(f.value, FieldValue::Varint(42));
        }
    }

    // ---- no panic on arbitrary input ----

    #[test]
    fn no_panic_on_all_single_byte_inputs() {
        for b in 0u8..=255 {
            let _: Vec<_> = ProtoFieldIter::new(&[b]).collect();
        }
    }

    #[test]
    fn no_panic_on_two_byte_inputs() {
        for hi in 0u8..=255 {
            for lo in (0u8..=255).step_by(17) {
                let _: Vec<_> = ProtoFieldIter::new(&[hi, lo]).collect();
            }
        }
    }

    // ---- derived trait impls ----

    #[test]
    fn proto_field_clone_eq_debug() {
        let mut data = Vec::new();
        encode_tag(&mut data, 1, WireType::Varint);
        encode_varint64(&mut data, 42);

        let fields: Vec<_> = ProtoFieldIter::new(&data)
            .collect::<Result<Vec<_>, _>>()
            .unwrap();

        let cloned = fields[0].clone();
        assert_eq!(fields[0], cloned);
        let _ = format!("{:?}", fields[0]);
    }

    #[test]
    fn length_delimited_huge_length_returns_err() {
        // Field 1 varint 1, then field 2 length-delimited declaring length
        // u32::MAX with no payload. The bounds check must not compute
        // `pos + length` with an unchecked add: on a 32-bit target that sum
        // wraps, the truncation check passes, and the slice panics. The
        // iterator must return Err, never panic, on corrupt input.
        let data: &[u8] = &[0x08, 0x01, 0x12, 0xFF, 0xFF, 0xFF, 0xFF, 0x0F];
        let mut iter = ProtoFieldIter::new(data);
        let first = iter.next().expect("first field present");
        assert!(first.is_ok());
        let second = iter.next().expect("second field present");
        assert!(second.is_err(), "oversized length must yield Err");
        assert!(iter.next().is_none());
    }
}
