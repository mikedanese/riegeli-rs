//! Adversarial tests for Sprint 25: Proto Field Iterator and Public Wire Helpers.
//!
//! These tests probe edge cases the Generator may have missed.

use riegeli::proto::{
    FieldValue, ProtoField, ProtoFieldIter, WireType, encode_tag, encode_varint32, encode_varint64,
    make_tag, read_canonical_varint64, serialize_field,
};

// ---------------------------------------------------------------------------
// Edge case: field number boundaries
// ---------------------------------------------------------------------------

#[test]
fn adv_field_number_max_29bit() {
    // Max proto field number is 2^29 - 1 = 536870911 (0x1FFFFFFF).
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
fn adv_field_number_zero_rejected() {
    // Field number 0 is invalid. Manually encode tag with field_number=0.
    let data = [0x00];
    let results: Vec<_> = ProtoFieldIter::new(&data).collect();
    assert_eq!(results.len(), 1);
    assert!(results[0].is_err());
}

// ---------------------------------------------------------------------------
// Edge case: varint boundary values
// ---------------------------------------------------------------------------

#[test]
fn adv_varint_two_byte_min() {
    let mut data = Vec::new();
    encode_tag(&mut data, 1, WireType::Varint);
    encode_varint64(&mut data, 128);

    let fields: Vec<_> = ProtoFieldIter::new(&data)
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert_eq!(fields[0].value, FieldValue::Varint(128));
}

#[test]
fn adv_varint_u64_max() {
    let mut data = Vec::new();
    encode_tag(&mut data, 1, WireType::Varint);
    encode_varint64(&mut data, u64::MAX);

    let fields: Vec<_> = ProtoFieldIter::new(&data)
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert_eq!(fields[0].value, FieldValue::Varint(u64::MAX));
}

#[test]
fn adv_varint_u64_max_round_trip() {
    let mut data = Vec::new();
    encode_tag(&mut data, 1, WireType::Varint);
    encode_varint64(&mut data, u64::MAX);

    let fields: Vec<_> = ProtoFieldIter::new(&data)
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    let mut reserialized = Vec::new();
    for f in &fields {
        serialize_field(&mut reserialized, f);
    }
    assert_eq!(data, reserialized);
}

// ---------------------------------------------------------------------------
// Edge case: non-canonical varints rejected
// ---------------------------------------------------------------------------

#[test]
fn adv_overlong_varint_tag_rejected() {
    // Tag encoded as [0x80, 0x00] is overlong encoding of 0 (field 0 varint).
    let data = [0x80, 0x00];
    let results: Vec<_> = ProtoFieldIter::new(&data).collect();
    assert_eq!(results.len(), 1);
    assert!(results[0].is_err());
}

#[test]
fn adv_11_byte_varint_rejected() {
    // Tag field 1 varint, then 11 continuation bytes — always invalid.
    let mut data = vec![0x08]; // field 1, varint
    data.extend_from_slice(&[0xFF; 10]); // 10 continuation bytes, no terminator
    let results: Vec<_> = ProtoFieldIter::new(&data).collect();
    assert_eq!(results.len(), 1);
    assert!(results[0].is_err());
}

// ---------------------------------------------------------------------------
// Edge case: fixed types at slice boundary (truncation)
// ---------------------------------------------------------------------------

#[test]
fn adv_fixed32_truncated_by_one() {
    let mut data = Vec::new();
    encode_tag(&mut data, 1, WireType::Fixed32);
    data.extend_from_slice(&[0xFF, 0xFF, 0xFF]); // only 3 bytes

    let results: Vec<_> = ProtoFieldIter::new(&data).collect();
    assert_eq!(results.len(), 1);
    assert!(results[0].is_err());
}

#[test]
fn adv_fixed64_truncated_by_one() {
    let mut data = Vec::new();
    encode_tag(&mut data, 1, WireType::Fixed64);
    data.extend_from_slice(&[0xFF; 7]); // only 7 bytes

    let results: Vec<_> = ProtoFieldIter::new(&data).collect();
    assert_eq!(results.len(), 1);
    assert!(results[0].is_err());
}

// ---------------------------------------------------------------------------
// Edge case: multiple fields then error — verify partial iteration works
// ---------------------------------------------------------------------------

#[test]
fn adv_partial_iteration_valid_fields_then_error() {
    let mut data = Vec::new();
    for i in 1..=3u32 {
        encode_tag(&mut data, i, WireType::Varint);
        encode_varint64(&mut data, i as u64 * 100);
    }
    // Then a truncated fixed64
    encode_tag(&mut data, 4, WireType::Fixed64);
    data.extend_from_slice(&[0x00; 3]); // only 3 of 8 bytes

    let results: Vec<_> = ProtoFieldIter::new(&data).collect();
    assert_eq!(results.len(), 4); // 3 Ok + 1 Err
    for r in &results[..3] {
        assert!(r.is_ok());
    }
    assert!(results[3].is_err());
}

// ---------------------------------------------------------------------------
// Edge case: sticky error behavior
// ---------------------------------------------------------------------------

#[test]
fn adv_after_error_yields_none_forever() {
    let data = [0x0E]; // wire type 6
    let mut iter = ProtoFieldIter::new(&data);
    assert!(iter.next().unwrap().is_err());
    assert!(iter.next().is_none());
    assert!(iter.next().is_none());
    assert!(iter.next().is_none());
}

// ---------------------------------------------------------------------------
// Edge case: groups — various nesting patterns
// ---------------------------------------------------------------------------

#[test]
fn adv_deeply_nested_groups() {
    let mut data = Vec::new();
    for i in 1..=10u32 {
        encode_tag(&mut data, i, WireType::StartGroup);
    }
    for i in (1..=10u32).rev() {
        encode_tag(&mut data, i, WireType::EndGroup);
    }

    let fields: Vec<_> = ProtoFieldIter::new(&data)
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert_eq!(fields.len(), 20); // 10 start + 10 end
}

#[test]
fn adv_groups_with_interleaved_fields() {
    let mut data = Vec::new();
    encode_tag(&mut data, 1, WireType::StartGroup);
    encode_tag(&mut data, 2, WireType::Varint);
    encode_varint64(&mut data, 42);
    encode_tag(&mut data, 3, WireType::Fixed32);
    data.extend_from_slice(&[0x01, 0x02, 0x03, 0x04]);
    encode_tag(&mut data, 1, WireType::EndGroup);

    let fields: Vec<_> = ProtoFieldIter::new(&data)
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert_eq!(fields.len(), 4);
    assert_eq!(fields[0].wire_type, WireType::StartGroup);
    assert_eq!(fields[1].wire_type, WireType::Varint);
    assert_eq!(fields[2].wire_type, WireType::Fixed32);
    assert_eq!(fields[3].wire_type, WireType::EndGroup);
}

// ---------------------------------------------------------------------------
// Edge case: EndGroup without StartGroup — iterator doesn't validate balance
// ---------------------------------------------------------------------------

#[test]
fn adv_end_group_without_start_is_yielded_flat() {
    let mut data = Vec::new();
    encode_tag(&mut data, 1, WireType::EndGroup);

    let fields: Vec<_> = ProtoFieldIter::new(&data)
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert_eq!(fields.len(), 1);
    assert_eq!(fields[0].wire_type, WireType::EndGroup);
    assert_eq!(fields[0].field_number, 1);
}

// ---------------------------------------------------------------------------
// Edge case: read_canonical_varint64 edge cases
// ---------------------------------------------------------------------------

#[test]
fn adv_read_varint64_offset_far_past_end_returns_none() {
    let data = [0x01];
    assert_eq!(read_canonical_varint64(&data, 100), None);
}

#[test]
fn adv_read_varint64_offset_at_len_returns_none() {
    let data = [0x01];
    assert_eq!(read_canonical_varint64(&data, 1), None);
}

#[test]
fn adv_read_varint64_max_10_byte_with_bit1() {
    let data = [0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x01];
    assert_eq!(read_canonical_varint64(&data, 0), Some((u64::MAX, 10)));
}

#[test]
fn adv_read_varint64_10th_byte_too_large() {
    let data = [0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x02];
    assert_eq!(read_canonical_varint64(&data, 0), None);
}

// ---------------------------------------------------------------------------
// Round-trip: large message with many fields
// ---------------------------------------------------------------------------

#[test]
fn adv_round_trip_mixed_types_many_fields() {
    let mut original = Vec::new();

    for i in 1..=20u32 {
        encode_tag(&mut original, i, WireType::Varint);
        encode_varint64(&mut original, i as u64);

        encode_tag(&mut original, i + 100, WireType::Fixed32);
        original.extend_from_slice(&(i.wrapping_mul(0x01010101)).to_le_bytes());

        encode_tag(&mut original, i + 200, WireType::Fixed64);
        original.extend_from_slice(&(i as u64 * 0x0101010101010101u64).to_le_bytes());

        encode_tag(&mut original, i + 300, WireType::LengthDelimited);
        let payload = format!("field_{}", i);
        encode_varint32(&mut original, payload.len() as u32);
        original.extend_from_slice(payload.as_bytes());
    }

    let fields: Vec<_> = ProtoFieldIter::new(&original)
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert_eq!(fields.len(), 80);

    let mut reserialized = Vec::new();
    for f in &fields {
        serialize_field(&mut reserialized, f);
    }
    assert_eq!(original, reserialized);
}

// ---------------------------------------------------------------------------
// Fuzz-like: no panic on arbitrary input
// ---------------------------------------------------------------------------

#[test]
fn adv_no_panic_on_all_single_bytes() {
    for b in 0u8..=255 {
        let _: Vec<_> = ProtoFieldIter::new(&[b]).collect();
    }
}

#[test]
fn adv_no_panic_on_all_two_byte_combos() {
    for hi in 0u8..=255 {
        for lo in (0u8..=255).step_by(17) {
            let _: Vec<_> = ProtoFieldIter::new(&[hi, lo]).collect();
        }
    }
}

// ---------------------------------------------------------------------------
// Edge case: serialize_field for LengthDelimited with large data
// ---------------------------------------------------------------------------

#[test]
fn adv_serialize_length_delimited_round_trip() {
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

// ---------------------------------------------------------------------------
// Edge case: repeated same field number
// ---------------------------------------------------------------------------

#[test]
fn adv_repeated_field_number_all_yielded() {
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

// ---------------------------------------------------------------------------
// Edge case: tag requires multi-byte varint
// ---------------------------------------------------------------------------

#[test]
fn adv_tag_two_byte_varint() {
    let mut data = Vec::new();
    encode_tag(&mut data, 16, WireType::Varint);
    encode_varint64(&mut data, 1);

    let fields: Vec<_> = ProtoFieldIter::new(&data)
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert_eq!(fields[0].field_number, 16);
}

// ---------------------------------------------------------------------------
// Edge case: truncated length prefix (varint itself is truncated)
// ---------------------------------------------------------------------------

#[test]
fn adv_truncated_length_prefix_varint() {
    let mut data = Vec::new();
    encode_tag(&mut data, 1, WireType::LengthDelimited);
    data.push(0x80); // continuation bit set, no next byte

    let results: Vec<_> = ProtoFieldIter::new(&data).collect();
    assert_eq!(results.len(), 1);
    assert!(results[0].is_err());
}

// ===========================================================================
// EVALUATOR-ADDED ADVERSARIAL TESTS
// ===========================================================================

#[test]
fn eval_varint64_10th_byte_zero_noncanonical() {
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
fn eval_tag_varint32_5th_byte_overflow() {
    let data = [0x80, 0x80, 0x80, 0x80, 0x10];
    let results: Vec<_> = ProtoFieldIter::new(&data).collect();
    assert_eq!(results.len(), 1);
    assert!(
        results[0].is_err(),
        "varint32 overflow on 5th byte should error"
    );
}

#[test]
fn eval_length_delimited_huge_length() {
    let mut data = Vec::new();
    encode_tag(&mut data, 1, WireType::LengthDelimited);
    encode_varint32(&mut data, 0x7FFFFFFF);
    data.extend_from_slice(b"tiny");

    let results: Vec<_> = ProtoFieldIter::new(&data).collect();
    assert_eq!(results.len(), 1);
    assert!(results[0].is_err());
}

#[test]
fn eval_make_tag_field_number_overflow_wraps() {
    let tag = make_tag(0x20000000, WireType::Varint);
    assert_eq!(tag, 0, "overflow wraps to 0");
}

#[test]
fn eval_length_delimited_binary_zeros() {
    let payload = vec![0u8; 10];
    let mut data = Vec::new();
    encode_tag(&mut data, 1, WireType::LengthDelimited);
    encode_varint32(&mut data, 10);
    data.extend_from_slice(&payload);

    let fields: Vec<_> = ProtoFieldIter::new(&data)
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert_eq!(fields[0].value, FieldValue::LengthDelimited(&[0u8; 10]));
}

#[test]
fn eval_consecutive_length_delimited() {
    let mut data = Vec::new();
    for i in 1..=3u32 {
        encode_tag(&mut data, i, WireType::LengthDelimited);
        let payload = vec![i as u8; i as usize];
        encode_varint32(&mut data, i);
        data.extend_from_slice(&payload);
    }

    let fields: Vec<_> = ProtoFieldIter::new(&data)
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert_eq!(fields.len(), 3);
    assert_eq!(fields[0].value, FieldValue::LengthDelimited(&[1]));
    assert_eq!(fields[1].value, FieldValue::LengthDelimited(&[2, 2]));
    assert_eq!(fields[2].value, FieldValue::LengthDelimited(&[3, 3, 3]));
}

#[test]
fn eval_group_containing_length_delimited() {
    let mut data = Vec::new();
    encode_tag(&mut data, 1, WireType::StartGroup);
    encode_tag(&mut data, 2, WireType::LengthDelimited);
    encode_varint32(&mut data, 3);
    data.extend_from_slice(b"abc");
    encode_tag(&mut data, 1, WireType::EndGroup);

    let fields: Vec<_> = ProtoFieldIter::new(&data)
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert_eq!(fields.len(), 3);
    assert_eq!(fields[0].wire_type, WireType::StartGroup);
    assert_eq!(fields[1].wire_type, WireType::LengthDelimited);
    assert_eq!(fields[1].value, FieldValue::LengthDelimited(b"abc"));
    assert_eq!(fields[2].wire_type, WireType::EndGroup);
}

#[test]
fn eval_round_trip_group_with_inner_fields() {
    let mut original = Vec::new();
    encode_tag(&mut original, 1, WireType::StartGroup);
    encode_tag(&mut original, 5, WireType::Varint);
    encode_varint64(&mut original, 999);
    encode_tag(&mut original, 6, WireType::Fixed64);
    original.extend_from_slice(&0xCAFEBABEu64.to_le_bytes());
    encode_tag(&mut original, 7, WireType::LengthDelimited);
    encode_varint32(&mut original, 4);
    original.extend_from_slice(b"test");
    encode_tag(&mut original, 1, WireType::EndGroup);

    let fields: Vec<ProtoField> = ProtoFieldIter::new(&original)
        .collect::<Result<Vec<_>, _>>()
        .unwrap();

    let mut reserialized = Vec::new();
    for f in &fields {
        serialize_field(&mut reserialized, f);
    }
    assert_eq!(original, reserialized);
}

#[test]
fn eval_encode_varint32_64_agree() {
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
fn eval_varint_byte_boundaries() {
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

#[test]
fn eval_proto_field_clone_eq() {
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
