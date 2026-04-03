//! Proto wire format primitives.
//!
//! Provides the wire type enum and tag composition/decomposition functions
//! matching the C++ `riegeli/messages/message_wire_format.h`, plus
//! `is_proto_message` for canonical proto binary validation.

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
#[cfg(test)]
#[inline]
pub(crate) fn make_tag(field_number: u32, wire_type: WireType) -> u32 {
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
