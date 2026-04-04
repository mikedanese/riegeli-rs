//! Adversarial tests for Sprint 26: SerializedMessageWriter
//!
//! These tests probe edge cases, boundary conditions, and misuse patterns
//! that the Generator's tests may not cover.

use riegeli::proto::{
    self, FieldValue, ProtoField, ProtoFieldIter, SerializedMessageWriter, WireType,
};

// ===========================================================================
// Edge cases: zigzag encoding boundary values
// ===========================================================================

#[test]
fn zigzag_i32_min() {
    assert_eq!(proto::zigzag_encode_i32(i32::MIN), u32::MAX);
}

#[test]
fn zigzag_i32_max() {
    assert_eq!(proto::zigzag_encode_i32(i32::MAX), (u32::MAX - 1));
}

#[test]
fn zigzag_i64_min() {
    assert_eq!(proto::zigzag_encode_i64(i64::MIN), u64::MAX);
}

#[test]
fn zigzag_i64_max() {
    assert_eq!(proto::zigzag_encode_i64(i64::MAX), u64::MAX - 1);
}

#[test]
fn sint32_boundary_values_round_trip() {
    for value in [i32::MIN, i32::MAX, 0, 1, -1, i32::MIN + 1, i32::MAX - 1] {
        let mut w = SerializedMessageWriter::new();
        w.write_sint32(1, value).unwrap();
        let bytes = w.finish().unwrap();
        let fields: Vec<_> = ProtoFieldIter::new(&bytes)
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(fields.len(), 1);
        assert_eq!(fields[0].wire_type, WireType::Varint);
        // Verify the zigzag value
        if let FieldValue::Varint(v) = fields[0].value {
            assert_eq!(v, proto::zigzag_encode_i32(value) as u64);
        } else {
            panic!("expected Varint");
        }
    }
}

#[test]
fn sint64_boundary_values_round_trip() {
    for value in [i64::MIN, i64::MAX, 0, 1, -1, i64::MIN + 1, i64::MAX - 1] {
        let mut w = SerializedMessageWriter::new();
        w.write_sint64(1, value).unwrap();
        let bytes = w.finish().unwrap();
        let fields: Vec<_> = ProtoFieldIter::new(&bytes)
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(fields.len(), 1);
        if let FieldValue::Varint(v) = fields[0].value {
            assert_eq!(v, proto::zigzag_encode_i64(value));
        } else {
            panic!("expected Varint");
        }
    }
}

// ===========================================================================
// Close without open
// ===========================================================================

#[test]
fn close_without_open_is_error() {
    let mut w = SerializedMessageWriter::new();
    let err = w.close_length_delimited();
    assert!(err.is_err());
}

#[test]
fn double_close_is_error() {
    let mut w = SerializedMessageWriter::new();
    w.open_length_delimited(1).unwrap();
    w.write_uint64(1, 42).unwrap();
    w.close_length_delimited().unwrap();
    // Second close should fail
    assert!(w.close_length_delimited().is_err());
}

// ===========================================================================
// Empty submessage
// ===========================================================================

#[test]
fn empty_submessage_produces_zero_length() {
    let mut w = SerializedMessageWriter::new();
    w.open_length_delimited(1).unwrap();
    w.close_length_delimited().unwrap();
    let bytes = w.finish().unwrap();
    // Tag for field 1, LD = (1 << 3) | 2 = 0x0A, then length 0x00
    assert_eq!(bytes, vec![0x0A, 0x00]);
}

// ===========================================================================
// Field number edge cases
// ===========================================================================

#[test]
fn field_number_1_varint() {
    let mut w = SerializedMessageWriter::new();
    w.write_uint64(1, 0).unwrap();
    let bytes = w.finish().unwrap();
    // tag = (1 << 3) | 0 = 8
    assert_eq!(bytes[0], 0x08);
}

#[test]
fn max_field_number_all_wire_types() {
    let max_fn = (1u32 << 29) - 1; // 536_870_911
    let mut w = SerializedMessageWriter::new();
    w.write_uint64(max_fn, 1).unwrap();
    w.write_fixed32(max_fn, 1).unwrap();
    w.write_fixed64(max_fn, 1).unwrap();
    w.write_bytes(max_fn, b"x").unwrap();
    w.write_start_group(max_fn).unwrap();
    w.write_end_group(max_fn).unwrap();
    let bytes = w.finish().unwrap();

    let fields: Vec<_> = ProtoFieldIter::new(&bytes)
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(fields.len(), 6);
    for f in &fields {
        assert_eq!(f.field_number, max_fn);
    }
}

// ===========================================================================
// Float NaN and infinity
// ===========================================================================

#[test]
fn float_nan_round_trips() {
    let mut w = SerializedMessageWriter::new();
    w.write_float(1, f32::NAN).unwrap();
    let bytes = w.finish().unwrap();
    let fields: Vec<_> = ProtoFieldIter::new(&bytes)
        .collect::<Result<_, _>>()
        .unwrap();
    if let FieldValue::Fixed32(bits) = fields[0].value {
        assert!(f32::from_bits(bits).is_nan());
    } else {
        panic!("expected Fixed32");
    }
}

#[test]
fn float_infinity_round_trips() {
    let mut w = SerializedMessageWriter::new();
    w.write_float(1, f32::INFINITY).unwrap();
    w.write_float(2, f32::NEG_INFINITY).unwrap();
    let bytes = w.finish().unwrap();
    let fields: Vec<_> = ProtoFieldIter::new(&bytes)
        .collect::<Result<_, _>>()
        .unwrap();
    if let FieldValue::Fixed32(bits) = fields[0].value {
        assert_eq!(f32::from_bits(bits), f32::INFINITY);
    } else {
        panic!("expected Fixed32");
    }
    if let FieldValue::Fixed32(bits) = fields[1].value {
        assert_eq!(f32::from_bits(bits), f32::NEG_INFINITY);
    } else {
        panic!("expected Fixed32");
    }
}

#[test]
fn double_nan_round_trips() {
    let mut w = SerializedMessageWriter::new();
    w.write_double(1, f64::NAN).unwrap();
    let bytes = w.finish().unwrap();
    let fields: Vec<_> = ProtoFieldIter::new(&bytes)
        .collect::<Result<_, _>>()
        .unwrap();
    if let FieldValue::Fixed64(bits) = fields[0].value {
        assert!(f64::from_bits(bits).is_nan());
    } else {
        panic!("expected Fixed64");
    }
}

#[test]
fn double_infinity_round_trips() {
    let mut w = SerializedMessageWriter::new();
    w.write_double(1, f64::INFINITY).unwrap();
    w.write_double(2, f64::NEG_INFINITY).unwrap();
    let bytes = w.finish().unwrap();
    let fields: Vec<_> = ProtoFieldIter::new(&bytes)
        .collect::<Result<_, _>>()
        .unwrap();
    if let FieldValue::Fixed64(bits) = fields[0].value {
        assert_eq!(f64::from_bits(bits), f64::INFINITY);
    } else {
        panic!("expected Fixed64");
    }
    if let FieldValue::Fixed64(bits) = fields[1].value {
        assert_eq!(f64::from_bits(bits), f64::NEG_INFINITY);
    } else {
        panic!("expected Fixed64");
    }
}

// ===========================================================================
// Interleaved open/close with siblings
// ===========================================================================

#[test]
fn sibling_submessages_both_correct_lengths() {
    let mut w = SerializedMessageWriter::new();
    // First submessage: field 1
    w.open_length_delimited(1).unwrap();
    w.write_uint64(1, 42).unwrap(); // [0x08, 0x2A] = 2 bytes
    w.close_length_delimited().unwrap();
    // Second submessage: field 2
    w.open_length_delimited(2).unwrap();
    w.write_uint64(1, 100).unwrap(); // [0x08, 0x64] = 2 bytes
    w.close_length_delimited().unwrap();
    let bytes = w.finish().unwrap();

    let fields: Vec<_> = ProtoFieldIter::new(&bytes)
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(fields.len(), 2);

    // First LD field
    assert_eq!(fields[0].field_number, 1);
    if let FieldValue::LengthDelimited(inner) = &fields[0].value {
        let inner_fields: Vec<_> = ProtoFieldIter::new(inner)
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(inner_fields[0].value, FieldValue::Varint(42));
    } else {
        panic!("expected LD");
    }

    // Second LD field
    assert_eq!(fields[1].field_number, 2);
    if let FieldValue::LengthDelimited(inner) = &fields[1].value {
        let inner_fields: Vec<_> = ProtoFieldIter::new(inner)
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(inner_fields[0].value, FieldValue::Varint(100));
    } else {
        panic!("expected LD");
    }
}

// ===========================================================================
// Many fields
// ===========================================================================

#[test]
fn hundred_fields_round_trip() {
    let mut w = SerializedMessageWriter::new();
    for i in 1..=100u32 {
        w.write_uint64(i, i as u64 * 1000).unwrap();
    }
    let bytes = w.finish().unwrap();

    let fields: Vec<ProtoField> = ProtoFieldIter::new(&bytes)
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(fields.len(), 100);
    for (i, f) in fields.iter().enumerate() {
        assert_eq!(f.field_number, (i + 1) as u32);
        assert_eq!(f.value, FieldValue::Varint((i + 1) as u64 * 1000));
    }

    // Re-serialize and verify byte-identity
    let mut rewritten = Vec::new();
    for f in &fields {
        proto::serialize_field(&mut rewritten, f);
    }
    assert_eq!(bytes, rewritten);
}

// ===========================================================================
// int32 boundary values (sign extension)
// ===========================================================================

#[test]
fn int32_min_sign_extends_to_10_bytes() {
    let mut w = SerializedMessageWriter::new();
    w.write_int32(1, i32::MIN).unwrap();
    let bytes = w.finish().unwrap();
    let fields: Vec<_> = ProtoFieldIter::new(&bytes)
        .collect::<Result<_, _>>()
        .unwrap();
    // i32::MIN as i64 as u64 = 0xFFFFFFFF80000000
    assert_eq!(fields[0].value, FieldValue::Varint(i32::MIN as i64 as u64));
}

#[test]
fn int32_positive_stays_small() {
    let mut w = SerializedMessageWriter::new();
    w.write_int32(1, 1).unwrap();
    let bytes = w.finish().unwrap();
    // tag 0x08, value 0x01 -- just 2 bytes
    assert_eq!(bytes.len(), 2);
}

// ===========================================================================
// Mixed nesting: submessage containing group
// ===========================================================================

#[test]
fn submessage_containing_group() {
    let mut w = SerializedMessageWriter::new();
    w.open_length_delimited(1).unwrap();
    w.write_start_group(2).unwrap();
    w.write_uint64(3, 99).unwrap();
    w.write_end_group(2).unwrap();
    w.close_length_delimited().unwrap();
    let bytes = w.finish().unwrap();

    let fields: Vec<_> = ProtoFieldIter::new(&bytes)
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(fields.len(), 1);
    assert_eq!(fields[0].wire_type, WireType::LengthDelimited);

    if let FieldValue::LengthDelimited(inner) = &fields[0].value {
        let inner_fields: Vec<_> = ProtoFieldIter::new(inner)
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(inner_fields.len(), 3);
        assert_eq!(inner_fields[0].wire_type, WireType::StartGroup);
        assert_eq!(inner_fields[1].value, FieldValue::Varint(99));
        assert_eq!(inner_fields[2].wire_type, WireType::EndGroup);
    } else {
        panic!("expected LD");
    }
}

// ===========================================================================
// Group containing submessage
// ===========================================================================

#[test]
fn group_containing_submessage() {
    let mut w = SerializedMessageWriter::new();
    w.write_start_group(1).unwrap();
    w.open_length_delimited(2).unwrap();
    w.write_uint64(3, 42).unwrap();
    w.close_length_delimited().unwrap();
    w.write_end_group(1).unwrap();
    let bytes = w.finish().unwrap();

    let fields: Vec<_> = ProtoFieldIter::new(&bytes)
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(fields.len(), 3);
    assert_eq!(fields[0].wire_type, WireType::StartGroup);
    assert_eq!(fields[0].field_number, 1);
    assert_eq!(fields[1].wire_type, WireType::LengthDelimited);
    assert_eq!(fields[1].field_number, 2);
    assert_eq!(fields[2].wire_type, WireType::EndGroup);
    assert_eq!(fields[2].field_number, 1);
}

// ===========================================================================
// Full round-trip: write -> iterate -> serialize -> compare
// ===========================================================================

#[test]
fn full_round_trip_identity() {
    let mut w = SerializedMessageWriter::new();
    w.write_int32(1, -1).unwrap();
    w.write_int64(2, -100).unwrap();
    w.write_uint32(3, 300).unwrap();
    w.write_uint64(4, 150).unwrap();
    w.write_sint32(5, -50).unwrap();
    w.write_sint64(6, i64::MIN).unwrap();
    w.write_bool(7, false).unwrap();
    w.write_fixed32(8, u32::MAX).unwrap();
    w.write_fixed64(9, u64::MAX).unwrap();
    w.write_sfixed32(10, i32::MIN).unwrap();
    w.write_sfixed64(11, i64::MIN).unwrap();
    w.write_float(12, f32::MIN_POSITIVE).unwrap();
    w.write_double(13, f64::MAX).unwrap();
    w.write_bytes(14, &[0xFF; 256]).unwrap();
    w.write_string(15, "hello\x00world").unwrap(); // embedded null
    w.open_length_delimited(16).unwrap();
    w.write_uint64(1, 1).unwrap();
    w.open_length_delimited(2).unwrap();
    w.write_uint64(1, 2).unwrap();
    w.close_length_delimited().unwrap();
    w.close_length_delimited().unwrap();
    w.write_start_group(17).unwrap();
    w.write_uint64(1, 3).unwrap();
    w.write_end_group(17).unwrap();
    let bytes = w.finish().unwrap();

    // Iterate
    let fields: Vec<ProtoField> = ProtoFieldIter::new(&bytes)
        .collect::<Result<_, _>>()
        .unwrap();

    // Re-serialize
    let mut rewritten = Vec::new();
    for f in &fields {
        proto::serialize_field(&mut rewritten, f);
    }

    assert_eq!(bytes, rewritten, "round-trip must be byte-identical");
}

// ===========================================================================
// Verify output is valid proto (is_proto_message)
// ===========================================================================

#[test]
fn complex_message_passes_is_proto_message() {
    let mut w = SerializedMessageWriter::new();
    w.write_uint64(1, 150).unwrap();
    w.write_sint32(2, -1).unwrap();
    w.write_fixed32(3, 42).unwrap();
    w.write_fixed64(4, 100).unwrap();
    w.write_bytes(5, b"test data").unwrap();
    w.open_length_delimited(6).unwrap();
    w.open_length_delimited(7).unwrap();
    w.write_uint64(1, 99).unwrap();
    w.close_length_delimited().unwrap();
    w.close_length_delimited().unwrap();
    w.write_start_group(8).unwrap();
    w.write_end_group(8).unwrap();
    let bytes = w.finish().unwrap();
    assert!(proto::is_proto_message(&bytes));
}

// ===========================================================================
// Deeply nested submessages (stress test for splice correctness)
// ===========================================================================

#[test]
fn deeply_nested_10_levels() {
    let mut w = SerializedMessageWriter::new();
    for i in 1..=10u32 {
        w.open_length_delimited(i).unwrap();
    }
    w.write_uint64(1, 42).unwrap();
    for _ in 0..10 {
        w.close_length_delimited().unwrap();
    }
    let bytes = w.finish().unwrap();
    assert!(proto::is_proto_message(&bytes));

    // Drill down 10 levels
    let mut data: &[u8] = &bytes;
    for expected_fn in 1..=10u32 {
        let fields: Vec<_> = ProtoFieldIter::new(data).collect::<Result<_, _>>().unwrap();
        assert_eq!(
            fields.len(),
            1,
            "level {} should have one field",
            expected_fn
        );
        assert_eq!(fields[0].field_number, expected_fn);
        if expected_fn < 10 {
            if let FieldValue::LengthDelimited(inner) = &fields[0].value {
                data = inner;
            } else {
                panic!("expected LD at level {}", expected_fn);
            }
        }
    }
    // At level 10, the inner should have the varint field
    if let FieldValue::LengthDelimited(inner) =
        &ProtoFieldIter::new(data).next().unwrap().unwrap().value
    {
        let innermost: Vec<_> = ProtoFieldIter::new(inner)
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(innermost.len(), 1);
        assert_eq!(innermost[0].value, FieldValue::Varint(42));
    }
}

// ===========================================================================
// write_string with empty string
// ===========================================================================

#[test]
fn empty_string_field() {
    let mut w = SerializedMessageWriter::new();
    w.write_string(1, "").unwrap();
    let bytes = w.finish().unwrap();
    let fields: Vec<_> = ProtoFieldIter::new(&bytes)
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(fields.len(), 1);
    assert_eq!(fields[0].value, FieldValue::LengthDelimited(&[]));
}

// ===========================================================================
// Varint encoding of 0 produces exactly one byte (canonical)
// ===========================================================================

#[test]
fn varint_zero_is_single_byte() {
    let mut w = SerializedMessageWriter::new();
    w.write_uint64(1, 0).unwrap();
    let bytes = w.finish().unwrap();
    // tag=0x08 (1 byte), value=0x00 (1 byte)
    assert_eq!(bytes.len(), 2);
    assert_eq!(bytes, [0x08, 0x00]);
}

// ===========================================================================
// finish() consumes the writer (ownership check) -- compile test
// ===========================================================================

#[test]
fn finish_returns_owned_vec() {
    let w = SerializedMessageWriter::new();
    let v: Vec<u8> = w.finish().unwrap();
    // v is owned, writer is consumed
    assert!(v.is_empty());
}

// ===========================================================================
// Multiple opens with only partial closes
// ===========================================================================

#[test]
fn partial_close_leaves_scope_open() {
    let mut w = SerializedMessageWriter::new();
    w.open_length_delimited(1).unwrap();
    w.open_length_delimited(2).unwrap();
    w.write_uint64(1, 1).unwrap();
    w.close_length_delimited().unwrap(); // closes field 2
    // field 1 still open
    assert!(w.finish().is_err());
}

// ===========================================================================
// Verify uint32 max value
// ===========================================================================

#[test]
fn uint32_max() {
    let mut w = SerializedMessageWriter::new();
    w.write_uint32(1, u32::MAX).unwrap();
    let bytes = w.finish().unwrap();
    let fields: Vec<_> = ProtoFieldIter::new(&bytes)
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(fields[0].value, FieldValue::Varint(u32::MAX as u64));
}

// ===========================================================================
// Verify sfixed32/sfixed64 negative values
// ===========================================================================

#[test]
fn sfixed32_negative() {
    let mut w = SerializedMessageWriter::new();
    w.write_sfixed32(1, -1).unwrap();
    let bytes = w.finish().unwrap();
    let fields: Vec<_> = ProtoFieldIter::new(&bytes)
        .collect::<Result<_, _>>()
        .unwrap();
    if let FieldValue::Fixed32(bits) = fields[0].value {
        let val = bits as i32;
        assert_eq!(val, -1);
    } else {
        panic!("expected Fixed32");
    }
}

#[test]
fn sfixed64_min() {
    let mut w = SerializedMessageWriter::new();
    w.write_sfixed64(1, i64::MIN).unwrap();
    let bytes = w.finish().unwrap();
    let fields: Vec<_> = ProtoFieldIter::new(&bytes)
        .collect::<Result<_, _>>()
        .unwrap();
    if let FieldValue::Fixed64(bits) = fields[0].value {
        let val = bits as i64;
        assert_eq!(val, i64::MIN);
    } else {
        panic!("expected Fixed64");
    }
}

// ===========================================================================
// Field number 0 produces a tag with field_number 0 -- problematic
// This tests whether the writer validates field numbers
// ===========================================================================

#[test]
fn field_number_zero_produces_invalid_proto() {
    // The writer doesn't validate field numbers, so field_number=0
    // should produce a message that is_proto_message rejects.
    let mut w = SerializedMessageWriter::new();
    w.write_uint64(0, 1).unwrap();
    let bytes = w.finish().unwrap();
    // tag = (0 << 3) | 0 = 0, which is not a valid proto tag
    assert!(!proto::is_proto_message(&bytes));
}

// ===========================================================================
// Large bytes field (just under limit)
// ===========================================================================

#[test]
fn bytes_field_at_moderate_size() {
    let data = vec![0xABu8; 65536];
    let mut w = SerializedMessageWriter::new();
    w.write_bytes(1, &data).unwrap();
    let bytes = w.finish().unwrap();
    let fields: Vec<_> = ProtoFieldIter::new(&bytes)
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(fields.len(), 1);
    if let FieldValue::LengthDelimited(inner) = &fields[0].value {
        assert_eq!(inner.len(), 65536);
        assert!(inner.iter().all(|&b| b == 0xAB));
    } else {
        panic!("expected LD");
    }
}

// ===========================================================================
// Submessage with large inner content (length varint > 1 byte)
// ===========================================================================

#[test]
fn submessage_with_multi_byte_length_varint() {
    let mut w = SerializedMessageWriter::new();
    w.open_length_delimited(1).unwrap();
    // Write enough data to require a multi-byte length varint (>= 128 bytes)
    for i in 1..=20u32 {
        w.write_fixed64(i, i as u64).unwrap(); // each = 1 tag byte + 8 data = 9+ bytes
    }
    w.close_length_delimited().unwrap();
    let bytes = w.finish().unwrap();

    // Verify it round-trips
    let fields: Vec<_> = ProtoFieldIter::new(&bytes)
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(fields.len(), 1);
    if let FieldValue::LengthDelimited(inner) = &fields[0].value {
        let inner_fields: Vec<_> = ProtoFieldIter::new(inner)
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(inner_fields.len(), 20);
    } else {
        panic!("expected LD");
    }
}

// ===========================================================================
// as_bytes shows intermediate state during nesting
// ===========================================================================

#[test]
fn as_bytes_shows_raw_buffer_with_open_scope() {
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
