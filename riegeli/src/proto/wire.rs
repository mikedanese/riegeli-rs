//! Proto wire format primitives.
//!
//! Provides the wire type enum, tag composition/decomposition, canonical varint
//! reading/encoding, zigzag encoding, and `is_proto_message` validation.

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
pub(crate) fn read_canonical_varint32(data: &[u8], pos: usize) -> Option<(u32, usize)> {
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
        // On the 10th byte (i==9), only the lowest bit is valid for u64;
        // a larger byte encodes a value that does not fit (C++ `SkipVarint`
        // rejects `byte >= 2` on the last possible byte).
        if i == 9 && (byte & 0x7F) > 1 {
            return None;
        }
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
// Proto message validation
// ---------------------------------------------------------------------------

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
                // Checked add: `length` is attacker-controlled (up to
                // u32::MAX); on a 32-bit target a wrapping add would let the
                // truncation check pass and move `pos` backward, re-parsing
                // earlier bytes forever. C++ uses `record.Skip(length)`,
                // which fails cleanly on truncation.
                match pos.checked_add(length as usize) {
                    Some(end) if end <= data.len() => pos = end,
                    _ => return false,
                }
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

/// Returns `true` if `data` parses as a protobuf message under the permissive
/// rules of standard proto parsers, which accept non-canonical (overlong)
/// varint encodings that [`is_proto_message`] rejects.
///
/// This distinguishes records that are merely encoded non-canonically — still
/// a valid message to every downstream proto parser — from records that are
/// not protobuf data at all. The streaming field filter uses it to refuse to
/// pass the former through unfiltered.
pub(crate) fn is_parseable_proto_message(data: &[u8]) -> bool {
    let mut pos: usize = 0;
    let mut started_groups: Vec<u32> = Vec::new();

    while pos < data.len() {
        // Read the tag, accepting overlong encodings. Tags fit in 32 bits
        // (field number <= 2^29 - 1 plus 3 wire-type bits).
        let Ok((tag, consumed)) = varint::decode_u64(&data[pos..]) else {
            return false;
        };
        if tag > u64::from(u32::MAX) {
            return false;
        }
        let tag = tag as u32;
        pos += consumed;

        let field_number = tag_field_number(tag);
        if field_number == 0 {
            return false;
        }

        let Some(wire_type) = tag_wire_type(tag) else {
            return false;
        };

        match wire_type {
            WireType::Varint => match varint::decode_u64(&data[pos..]) {
                Ok((_, consumed)) => pos += consumed,
                Err(_) => return false,
            },
            WireType::Fixed32 => {
                if data.len() - pos < 4 {
                    return false;
                }
                pos += 4;
            }
            WireType::Fixed64 => {
                if data.len() - pos < 8 {
                    return false;
                }
                pos += 8;
            }
            WireType::LengthDelimited => {
                let Ok((length, consumed)) = varint::decode_u64(&data[pos..]) else {
                    return false;
                };
                pos += consumed;
                if length > (data.len() - pos) as u64 {
                    return false;
                }
                pos += length as usize;
            }
            WireType::StartGroup => started_groups.push(field_number),
            WireType::EndGroup => {
                if started_groups.last() != Some(&field_number) {
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

    // ---- is_parseable_proto_message (permissive spec-level walk) ----

    #[test]
    fn test_parseable_accepts_overlong_varint_value() {
        // Field 1 = varint 0 encoded in two bytes, then field 2 = bytes
        // "secret". Strict canonical validation rejects this, but every
        // standard proto parser accepts it.
        let mut data = vec![0x08, 0x80, 0x00, 0x12, 0x06];
        data.extend_from_slice(b"secret");
        assert!(!is_proto_message(&data));
        assert!(is_parseable_proto_message(&data));
    }

    #[test]
    fn test_parseable_accepts_overlong_tag() {
        // Tag 0x08 (field 1, varint) encoded in two bytes [0x88, 0x00],
        // followed by varint value 7.
        let data = [0x88, 0x00, 0x07];
        assert!(!is_proto_message(&data));
        assert!(is_parseable_proto_message(&data));
    }

    #[test]
    fn test_parseable_accepts_everything_canonical_accepts() {
        let mut data = Vec::new();
        encode_tag(&mut data, 1, WireType::Varint);
        encode_varint64(&mut data, 150);
        encode_tag(&mut data, 2, WireType::LengthDelimited);
        encode_varint32(&mut data, 3);
        data.extend_from_slice(b"abc");
        encode_tag(&mut data, 3, WireType::StartGroup);
        encode_tag(&mut data, 4, WireType::Fixed32);
        data.extend_from_slice(&7u32.to_le_bytes());
        encode_tag(&mut data, 3, WireType::EndGroup);
        assert!(is_proto_message(&data));
        assert!(is_parseable_proto_message(&data));
    }

    #[test]
    fn test_parseable_rejects_non_proto_data() {
        // Wire type 7 in the first tag: not proto under any parser.
        assert!(!is_parseable_proto_message(&[0x0F, 0xFF, 0x00]));
        // Field number 0.
        assert!(!is_parseable_proto_message(&[0x00]));
        // Truncated length-delimited field.
        assert!(!is_parseable_proto_message(&[0x12, 0x05, b'a']));
        // Unclosed group.
        assert!(!is_parseable_proto_message(&[0x0B]));
        // Mismatched end-group.
        assert!(!is_parseable_proto_message(&[0x0B, 0x14]));
        // Truncated varint value.
        assert!(!is_parseable_proto_message(&[0x08, 0x80]));
    }

    #[test]
    fn test_make_tag_and_decompose() {
        let tag = make_tag(1, WireType::Varint);
        assert_eq!(tag_wire_type(tag), Some(WireType::Varint));
        assert_eq!(tag_field_number(tag), 1);

        let tag2 = make_tag(5, WireType::Fixed32);
        assert_eq!(tag_wire_type(tag2), Some(WireType::Fixed32));
        assert_eq!(tag_field_number(tag2), 5);
    }

    #[test]
    fn test_tag_values() {
        assert_eq!(make_tag(1, WireType::Varint), 0x08);
        assert_eq!(make_tag(1, WireType::Fixed64), 0x09);
        assert_eq!(make_tag(2, WireType::LengthDelimited), 0x12);
    }

    #[test]
    fn test_wire_type_from_raw_invalid() {
        assert_eq!(WireType::from_raw(6), None);
        assert_eq!(WireType::from_raw(7), None);
        assert_eq!(tag_wire_type(6), None);
        assert_eq!(tag_wire_type(7), None);
    }

    // ---- is_proto_message ----

    #[test]
    fn test_empty_is_valid() {
        assert!(is_proto_message(b""));
    }

    #[test]
    fn test_valid_proto_mixed_fields() {
        let mut data = Vec::new();
        data.push(0x08);
        data.push(0x01);
        data.push(0x09);
        data.extend_from_slice(&[0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]);
        data.push(0x12);
        data.push(0x03);
        data.extend_from_slice(b"abc");
        assert!(is_proto_message(&data));
    }

    #[test]
    fn test_overlong_varint_rejected() {
        let data = [0x08, 0x80, 0x80, 0x00];
        assert!(!is_proto_message(&data));
    }

    #[test]
    fn test_overlong_varint_tag_rejected() {
        let data = [0x80, 0x00];
        assert!(!is_proto_message(&data));
    }

    #[test]
    fn test_unclosed_start_group() {
        let data = [0x0B];
        assert!(!is_proto_message(&data));
    }

    #[test]
    fn test_mismatched_end_group() {
        let data = [0x0B, 0x14];
        assert!(!is_proto_message(&data));
    }

    #[test]
    fn test_matched_group() {
        let data = [0x0B, 0x0C];
        assert!(is_proto_message(&data));
    }

    #[test]
    fn test_wire_type_6_rejected() {
        let data = [0x0E];
        assert!(!is_proto_message(&data));
    }

    #[test]
    fn test_wire_type_7_rejected() {
        let data = [0x0F];
        assert!(!is_proto_message(&data));
    }

    #[test]
    fn test_field_number_zero_rejected() {
        let data = [0x00];
        assert!(!is_proto_message(&data));
    }

    #[test]
    fn test_truncated_fixed32() {
        let data = [0x0D, 0x00, 0x00];
        assert!(!is_proto_message(&data));
    }

    #[test]
    fn test_truncated_fixed64() {
        let data = [0x09, 0x00, 0x00, 0x00, 0x00];
        assert!(!is_proto_message(&data));
    }

    #[test]
    fn test_length_delimited_overflow() {
        let data = [0x12, 0x64, 0x00, 0x00, 0x00];
        assert!(!is_proto_message(&data));
    }

    #[test]
    fn test_length_delimited_huge_length_rejected() {
        // Field 2, LengthDelimited, canonical 5-byte length 0xFFFFFFFA
        // (u32::MAX - 5). The remaining data is far shorter, so this must be
        // rejected. On a 32-bit target an unchecked `pos + length` would
        // wrap to 0, pass the truncation check, and loop forever; C++ fails
        // via `record.Skip(length)`.
        let data = [0x12, 0xFA, 0xFF, 0xFF, 0xFF, 0x0F];
        assert!(!is_proto_message(&data));
        // Same with the maximum encodable length.
        let data = [0x12, 0xFF, 0xFF, 0xFF, 0xFF, 0x0F];
        assert!(!is_proto_message(&data));
    }

    #[test]
    fn test_truncated_varint_at_end() {
        let data = [0x88];
        assert!(!is_proto_message(&data));
    }

    #[test]
    fn test_valid_fixed32_field() {
        let data = [0x0D, 0x01, 0x02, 0x03, 0x04];
        assert!(is_proto_message(&data));
    }

    #[test]
    fn test_too_long_varint32_tag() {
        let data = [0x80, 0x80, 0x80, 0x80, 0x80, 0x01];
        assert!(!is_proto_message(&data));
    }

    #[test]
    fn test_end_group_without_start() {
        let data = [0x0C];
        assert!(!is_proto_message(&data));
    }

    #[test]
    fn test_nested_groups() {
        let data = [0x0B, 0x13, 0x14, 0x0C];
        assert!(is_proto_message(&data));
    }

    #[test]
    fn test_varint_max_length_valid() {
        let mut data = vec![0x08];
        data.extend_from_slice(&[0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x01]);
        assert!(is_proto_message(&data));
    }

    #[test]
    fn test_varint_10th_byte_overflowing_u64_rejected() {
        // A 10-byte varint whose last byte is 2..=0x7F encodes a value that
        // does not fit in u64. C++ `SkipVarint` rejects this on the last
        // possible byte (`byte >= 1 << (64 - 9 * 7)`, i.e. byte >= 2), so
        // `IsProtoMessage` classifies such records as non-proto.
        for last in [0x02u8, 0x03, 0x7F] {
            let mut data = vec![0x08];
            data.extend_from_slice(&[0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF]);
            data.push(last);
            assert!(
                !is_proto_message(&data),
                "10-byte varint ending in {last:#04x} overflows u64 and must be rejected"
            );
        }
    }

    #[test]
    fn test_varint_11_bytes_rejected() {
        let mut data = vec![0x08];
        data.extend_from_slice(&[0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF]);
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
        assert!(!is_proto_message(&[0x0B]));
        assert!(!is_proto_message(&[0x0B, 0x14]));
        assert!(!is_proto_message(&[0x0B, 0x13, 0x14]));
        assert!(!is_proto_message(&[0x0B, 0x0C, 0x0C]));
    }

    #[test]
    fn test_invalid_wire_type_rejected() {
        assert!(!is_proto_message(&[0x0E]));
        assert!(!is_proto_message(&[0x0F]));
        assert!(!is_proto_message(&[0xA6, 0x06]));
        assert!(!is_proto_message(&[0x08, 0x01, 0x0F]));
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

    #[test]
    fn test_fixed64_field_valid() {
        let mut data = vec![0x09];
        data.extend_from_slice(&[0x00; 8]);
        assert!(is_proto_message(&data));
    }

    #[test]
    fn test_length_delimited_field_valid() {
        assert!(is_proto_message(&[0x12, 0x03, b'a', b'b', b'c']));
    }

    // ---- read_canonical_varint64 ----

    #[test]
    fn read_varint64_offset_past_end_none() {
        // An offset beyond the end of the buffer must return None rather
        // than panicking on the out-of-range slice.
        let data = [0x01];
        assert_eq!(read_canonical_varint64(&data, 100), None);
    }

    #[test]
    fn read_varint64_tenth_byte_overflow_rejected() {
        // The 10th byte of a varint64 may only carry the lowest bit; a value
        // above 0x01 overflows u64 and must be rejected.
        let data = [0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x02];
        assert_eq!(read_canonical_varint64(&data, 0), None);
    }

    // ---- make_tag overflow ----

    #[test]
    fn make_tag_field_number_overflow_wraps() {
        // Field numbers above the 29-bit maximum shift bits out of the u32
        // tag; the result wraps rather than panicking.
        let tag = make_tag(0x20000000, WireType::Varint);
        assert_eq!(tag, 0, "overflow wraps to 0");
    }

    // ---- varint encoders agree ----

    #[test]
    fn encode_varint32_64_agree() {
        for &val in &[0u32, 1, 127, 128, 16383, 16384, u32::MAX] {
            let mut buf32 = Vec::new();
            let mut buf64 = Vec::new();
            encode_varint32(&mut buf32, val);
            encode_varint64(&mut buf64, val as u64);
            assert_eq!(
                buf32, buf64,
                "encode_varint32 and encode_varint64 must agree for value {}",
                val
            );
        }
    }

    #[test]
    fn test_mixed_valid_fields_all_types() {
        let mut data = Vec::new();
        data.extend_from_slice(&[0x08, 0x96, 0x01]);
        data.push(0x11);
        data.extend_from_slice(&[0xFF; 8]);
        data.push(0x1A);
        data.push(0x00);
        data.push(0x25);
        data.extend_from_slice(&[0xAA; 4]);
        data.push(0x2B);
        data.push(0x2C);
        assert!(is_proto_message(&data));
    }
}
