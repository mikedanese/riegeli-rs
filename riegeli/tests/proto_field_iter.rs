//! Sprint 25 tests: Proto field iterator and public wire helpers.

use riegeli::proto_wire::{
    FieldValue, ProtoField, ProtoFieldIter, WireType, encode_tag, encode_varint32, encode_varint64,
    make_tag, serialize_field,
};

// ---------------------------------------------------------------------------
// Criterion 25.1: Iterating over a message with varint, fixed32, fixed64,
// length-delimited, and group fields yields the correct field number, wire
// type, and value for each field, in order.
// ---------------------------------------------------------------------------

#[test]
fn criterion_25_1_all_wire_types() {
    let mut data = Vec::new();

    // Field 1, varint, value 150
    encode_tag(&mut data, 1, WireType::Varint);
    encode_varint64(&mut data, 150);

    // Field 2, fixed64, value 0x0102030405060708
    encode_tag(&mut data, 2, WireType::Fixed64);
    data.extend_from_slice(&0x0102030405060708u64.to_le_bytes());

    // Field 3, length-delimited, "hello"
    encode_tag(&mut data, 3, WireType::LengthDelimited);
    encode_varint32(&mut data, 5);
    data.extend_from_slice(b"hello");

    // Field 4, fixed32, value 0xDEADBEEF
    encode_tag(&mut data, 4, WireType::Fixed32);
    data.extend_from_slice(&0xDEADBEEFu32.to_le_bytes());

    // Field 5, start group
    encode_tag(&mut data, 5, WireType::StartGroup);

    // Field 5, end group
    encode_tag(&mut data, 5, WireType::EndGroup);

    let fields: Vec<_> = ProtoFieldIter::new(&data)
        .collect::<Result<Vec<_>, _>>()
        .expect("should parse all fields");

    assert_eq!(fields.len(), 6);

    assert_eq!(fields[0].field_number, 1);
    assert_eq!(fields[0].wire_type, WireType::Varint);
    assert_eq!(fields[0].value, FieldValue::Varint(150));

    assert_eq!(fields[1].field_number, 2);
    assert_eq!(fields[1].wire_type, WireType::Fixed64);
    assert_eq!(fields[1].value, FieldValue::Fixed64(0x0102030405060708));

    assert_eq!(fields[2].field_number, 3);
    assert_eq!(fields[2].wire_type, WireType::LengthDelimited);
    assert_eq!(fields[2].value, FieldValue::LengthDelimited(b"hello"));

    assert_eq!(fields[3].field_number, 4);
    assert_eq!(fields[3].wire_type, WireType::Fixed32);
    assert_eq!(fields[3].value, FieldValue::Fixed32(0xDEADBEEF));

    assert_eq!(fields[4].field_number, 5);
    assert_eq!(fields[4].wire_type, WireType::StartGroup);
    assert_eq!(fields[4].value, FieldValue::StartGroup);

    assert_eq!(fields[5].field_number, 5);
    assert_eq!(fields[5].wire_type, WireType::EndGroup);
    assert_eq!(fields[5].value, FieldValue::EndGroup);
}

// ---------------------------------------------------------------------------
// Criterion 25.2: Iterating over an empty slice yields zero items and no error.
// ---------------------------------------------------------------------------

#[test]
fn criterion_25_2_empty_slice() {
    let fields: Vec<_> = ProtoFieldIter::new(&[])
        .collect::<Result<Vec<_>, _>>()
        .expect("empty slice should produce no errors");
    assert!(fields.is_empty());
}

// ---------------------------------------------------------------------------
// Criterion 25.3: Iterating over truncated input yields an error at the point
// of truncation, not a panic.
// ---------------------------------------------------------------------------

#[test]
fn criterion_25_3_truncated_length_delimited() {
    // Field 3, length-delimited, declared length 100 but only 3 bytes available.
    let mut data = Vec::new();
    encode_tag(&mut data, 3, WireType::LengthDelimited);
    encode_varint32(&mut data, 100);
    data.extend_from_slice(b"abc");

    let results: Vec<_> = ProtoFieldIter::new(&data).collect();
    assert_eq!(results.len(), 1);
    assert!(
        results[0].is_err(),
        "should yield an error for truncated input"
    );
}

#[test]
fn criterion_25_3_truncated_fixed32() {
    // Field 4, fixed32, but only 2 data bytes.
    let mut data = Vec::new();
    encode_tag(&mut data, 4, WireType::Fixed32);
    data.extend_from_slice(&[0x00, 0x00]);

    let results: Vec<_> = ProtoFieldIter::new(&data).collect();
    assert_eq!(results.len(), 1);
    assert!(results[0].is_err());
}

#[test]
fn criterion_25_3_truncated_fixed64() {
    // Field 2, fixed64, but only 4 data bytes.
    let mut data = Vec::new();
    encode_tag(&mut data, 2, WireType::Fixed64);
    data.extend_from_slice(&[0x00; 4]);

    let results: Vec<_> = ProtoFieldIter::new(&data).collect();
    assert_eq!(results.len(), 1);
    assert!(results[0].is_err());
}

#[test]
fn criterion_25_3_truncated_varint() {
    // Field 1, varint, but the value has a continuation bit with no following byte.
    let mut data = Vec::new();
    encode_tag(&mut data, 1, WireType::Varint);
    data.push(0x80); // continuation bit set, no next byte

    let results: Vec<_> = ProtoFieldIter::new(&data).collect();
    assert_eq!(results.len(), 1);
    assert!(results[0].is_err());
}

#[test]
fn criterion_25_3_truncated_tag() {
    // A tag byte with continuation bit set, nothing following.
    let data = [0x80];
    let results: Vec<_> = ProtoFieldIter::new(&data).collect();
    assert_eq!(results.len(), 1);
    assert!(results[0].is_err());
}

#[test]
fn criterion_25_3_no_panic_on_all_truncations() {
    // Build a valid message, then iterate over every prefix.
    let mut valid = Vec::new();
    encode_tag(&mut valid, 1, WireType::Varint);
    encode_varint64(&mut valid, 150);
    encode_tag(&mut valid, 2, WireType::LengthDelimited);
    encode_varint32(&mut valid, 3);
    valid.extend_from_slice(b"abc");
    encode_tag(&mut valid, 3, WireType::Fixed32);
    valid.extend_from_slice(&[0x01, 0x02, 0x03, 0x04]);

    for i in 1..valid.len() {
        // Must not panic.
        let _: Vec<_> = ProtoFieldIter::new(&valid[..i]).collect();
    }
}

// ---------------------------------------------------------------------------
// Criterion 25.4: Iterating over input containing wire types 6 or 7 yields
// an error for the invalid field.
// ---------------------------------------------------------------------------

#[test]
fn criterion_25_4_wire_type_6() {
    // Wire type 6 for field 1: (1 << 3) | 6 = 0x0E
    let data = [0x0E];
    let results: Vec<_> = ProtoFieldIter::new(&data).collect();
    assert_eq!(results.len(), 1);
    assert!(results[0].is_err());
}

#[test]
fn criterion_25_4_wire_type_7() {
    // Wire type 7 for field 1: (1 << 3) | 7 = 0x0F
    let data = [0x0F];
    let results: Vec<_> = ProtoFieldIter::new(&data).collect();
    assert_eq!(results.len(), 1);
    assert!(results[0].is_err());
}

#[test]
fn criterion_25_4_valid_then_invalid_wire_type() {
    // A valid varint field followed by an invalid wire type 6.
    let mut data = Vec::new();
    encode_tag(&mut data, 1, WireType::Varint);
    encode_varint64(&mut data, 42);
    data.push(0x0E); // field 1, wire type 6

    let results: Vec<_> = ProtoFieldIter::new(&data).collect();
    assert_eq!(results.len(), 2);
    assert!(results[0].is_ok());
    assert!(results[1].is_err());
}

// ---------------------------------------------------------------------------
// Criterion 25.5: make_tag(field_number, wire_type) is callable from an
// external crate (it is a public, non-cfg(test) function).
// ---------------------------------------------------------------------------

#[test]
fn criterion_25_5_make_tag_public() {
    // This test itself proves make_tag is public and callable from an external
    // crate (this test file is in tests/, which is a separate crate).
    let tag = make_tag(1, WireType::Varint);
    assert_eq!(tag, 0x08);

    let tag2 = make_tag(5, WireType::Fixed32);
    assert_eq!(tag2, (5 << 3) | 5);
}

// ---------------------------------------------------------------------------
// Criterion 25.6: Round-trip: iterating over valid proto bytes and
// re-serializing each field using the public encoding helpers produces
// byte-identical output.
// ---------------------------------------------------------------------------

#[test]
fn criterion_25_6_round_trip() {
    let mut original = Vec::new();

    // Varint field 1 = 150
    encode_tag(&mut original, 1, WireType::Varint);
    encode_varint64(&mut original, 150);

    // Fixed64 field 2 = 0xDEADCAFEBAADF00D
    encode_tag(&mut original, 2, WireType::Fixed64);
    original.extend_from_slice(&0xDEADCAFEBAADF00Du64.to_le_bytes());

    // Length-delimited field 3 = "world"
    encode_tag(&mut original, 3, WireType::LengthDelimited);
    encode_varint32(&mut original, 5);
    original.extend_from_slice(b"world");

    // Fixed32 field 4 = 0x12345678
    encode_tag(&mut original, 4, WireType::Fixed32);
    original.extend_from_slice(&0x12345678u32.to_le_bytes());

    // Group field 5 (start + end)
    encode_tag(&mut original, 5, WireType::StartGroup);
    encode_tag(&mut original, 5, WireType::EndGroup);

    // Iterate and re-serialize.
    let fields: Vec<ProtoField> = ProtoFieldIter::new(&original)
        .collect::<Result<Vec<_>, _>>()
        .expect("valid message should parse");

    let mut reserialized = Vec::new();
    for field in &fields {
        serialize_field(&mut reserialized, field);
    }

    assert_eq!(original, reserialized, "round-trip must be byte-identical");
}

#[test]
fn criterion_25_6_round_trip_edge_values() {
    let mut original = Vec::new();

    // Varint field 1 = 0 (single byte)
    encode_tag(&mut original, 1, WireType::Varint);
    encode_varint64(&mut original, 0);

    // Varint field 1 = u64::MAX (10 bytes)
    encode_tag(&mut original, 1, WireType::Varint);
    encode_varint64(&mut original, u64::MAX);

    // Empty length-delimited field 2
    encode_tag(&mut original, 2, WireType::LengthDelimited);
    encode_varint32(&mut original, 0);

    // Large field number (16000)
    encode_tag(&mut original, 16000, WireType::Varint);
    encode_varint64(&mut original, 1);

    let fields: Vec<ProtoField> = ProtoFieldIter::new(&original)
        .collect::<Result<Vec<_>, _>>()
        .expect("valid message should parse");

    let mut reserialized = Vec::new();
    for field in &fields {
        serialize_field(&mut reserialized, field);
    }

    assert_eq!(original, reserialized);
}

// ---------------------------------------------------------------------------
// Criterion 25.7: Nested groups (StartGroup/EndGroup) are yielded as flat
// field events — the iterator does not consume group contents automatically,
// allowing the caller to handle nesting.
// ---------------------------------------------------------------------------

#[test]
fn criterion_25_7_nested_groups_flat() {
    let mut data = Vec::new();

    // Outer group field 1: start
    encode_tag(&mut data, 1, WireType::StartGroup);
    // Inner group field 2: start
    encode_tag(&mut data, 2, WireType::StartGroup);
    // A varint field inside the inner group
    encode_tag(&mut data, 10, WireType::Varint);
    encode_varint64(&mut data, 42);
    // Inner group field 2: end
    encode_tag(&mut data, 2, WireType::EndGroup);
    // Outer group field 1: end
    encode_tag(&mut data, 1, WireType::EndGroup);

    let fields: Vec<ProtoField> = ProtoFieldIter::new(&data)
        .collect::<Result<Vec<_>, _>>()
        .expect("valid message should parse");

    // All 5 events yielded flat.
    assert_eq!(fields.len(), 5);

    assert_eq!(fields[0].field_number, 1);
    assert_eq!(fields[0].wire_type, WireType::StartGroup);

    assert_eq!(fields[1].field_number, 2);
    assert_eq!(fields[1].wire_type, WireType::StartGroup);

    assert_eq!(fields[2].field_number, 10);
    assert_eq!(fields[2].wire_type, WireType::Varint);
    assert_eq!(fields[2].value, FieldValue::Varint(42));

    assert_eq!(fields[3].field_number, 2);
    assert_eq!(fields[3].wire_type, WireType::EndGroup);

    assert_eq!(fields[4].field_number, 1);
    assert_eq!(fields[4].wire_type, WireType::EndGroup);
}

#[test]
fn criterion_25_7_nested_groups_round_trip() {
    let mut data = Vec::new();
    encode_tag(&mut data, 1, WireType::StartGroup);
    encode_tag(&mut data, 2, WireType::StartGroup);
    encode_tag(&mut data, 3, WireType::Varint);
    encode_varint64(&mut data, 99);
    encode_tag(&mut data, 2, WireType::EndGroup);
    encode_tag(&mut data, 1, WireType::EndGroup);

    let fields: Vec<ProtoField> = ProtoFieldIter::new(&data)
        .collect::<Result<Vec<_>, _>>()
        .unwrap();

    let mut reserialized = Vec::new();
    for field in &fields {
        serialize_field(&mut reserialized, field);
    }
    assert_eq!(data, reserialized, "nested groups must round-trip");
}

// ---------------------------------------------------------------------------
// Additional: read_canonical_varint64 direct tests
// ---------------------------------------------------------------------------

#[test]
fn test_read_varint64_basic() {
    use riegeli::proto_wire::read_canonical_varint64;

    // Single byte: value 0
    assert_eq!(read_canonical_varint64(&[0x00], 0), Some((0, 1)));
    // Single byte: value 127
    assert_eq!(read_canonical_varint64(&[0x7F], 0), Some((127, 1)));
    // Two bytes: value 128
    assert_eq!(read_canonical_varint64(&[0x80, 0x01], 0), Some((128, 2)));
    // Ten bytes: u64::MAX
    let max_bytes = [0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x01];
    assert_eq!(read_canonical_varint64(&max_bytes, 0), Some((u64::MAX, 10)));
    // Empty input
    assert_eq!(read_canonical_varint64(&[], 0), None);
    // Non-canonical (trailing zero in multi-byte)
    assert_eq!(read_canonical_varint64(&[0x80, 0x00], 0), None);
}

// ---------------------------------------------------------------------------
// Additional: error is sticky (after error, next() returns None)
// ---------------------------------------------------------------------------

#[test]
fn test_error_is_sticky() {
    // Invalid wire type followed by valid data — second call returns None.
    let mut data = vec![0x0E]; // wire type 6
    encode_tag(&mut data, 1, WireType::Varint);
    encode_varint64(&mut data, 1);

    let mut iter = ProtoFieldIter::new(&data);
    let first = iter.next();
    assert!(first.is_some());
    assert!(first.unwrap().is_err());
    // After error, iterator is exhausted.
    assert!(iter.next().is_none());
}
