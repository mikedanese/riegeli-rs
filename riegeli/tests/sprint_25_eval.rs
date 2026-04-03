//! Adversarial tests for Sprint 25: Proto Field Iterator and Public Wire Helpers.
//!
//! These tests probe edge cases the Generator may have missed.

use riegeli::proto_wire::{
    FieldValue, ProtoField, ProtoFieldIter, WireType, encode_tag, encode_varint32, encode_varint64,
    make_tag, read_canonical_varint64, serialize_field,
};

// ---------------------------------------------------------------------------
// Edge case: field number boundaries
// ---------------------------------------------------------------------------

#[test]
fn adv_field_number_1_smallest_valid() {
    // Field number 1 is the smallest valid field number.
    let mut data = Vec::new();
    encode_tag(&mut data, 1, WireType::Varint);
    encode_varint64(&mut data, 0);

    let fields: Vec<_> = ProtoFieldIter::new(&data)
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert_eq!(fields.len(), 1);
    assert_eq!(fields[0].field_number, 1);
}

#[test]
fn adv_field_number_max_29bit() {
    // Max proto field number is 2^29 - 1 = 536870911 (0x1FFFFFFF).
    // Tag = (0x1FFFFFFF << 3) | 0 = 0xFFFFFFF8.
    // This requires a 5-byte varint32 for the tag.
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
    // Tag = (0 << 3) | 0 = 0x00 as varint.
    let data = [0x00];
    let results: Vec<_> = ProtoFieldIter::new(&data).collect();
    assert_eq!(results.len(), 1);
    assert!(results[0].is_err());
}

// ---------------------------------------------------------------------------
// Edge case: varint boundary values
// ---------------------------------------------------------------------------

#[test]
fn adv_varint_single_byte_zero() {
    let mut data = Vec::new();
    encode_tag(&mut data, 1, WireType::Varint);
    encode_varint64(&mut data, 0);

    let fields: Vec<_> = ProtoFieldIter::new(&data)
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert_eq!(fields[0].value, FieldValue::Varint(0));
}

#[test]
fn adv_varint_single_byte_max() {
    let mut data = Vec::new();
    encode_tag(&mut data, 1, WireType::Varint);
    encode_varint64(&mut data, 127);

    let fields: Vec<_> = ProtoFieldIter::new(&data)
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert_eq!(fields[0].value, FieldValue::Varint(127));
}

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
fn adv_overlong_varint_value_rejected() {
    // Tag for field 1 varint, followed by overlong varint: [0x80, 0x00]
    // (encodes 0 in two bytes, non-canonical because last byte is 0).
    let data = [0x08, 0x80, 0x00];
    let results: Vec<_> = ProtoFieldIter::new(&data).collect();
    assert_eq!(results.len(), 1);
    assert!(
        results[0].is_err(),
        "overlong varint value should be rejected"
    );
}

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
// Edge case: length-delimited fields
// ---------------------------------------------------------------------------

#[test]
fn adv_empty_length_delimited_field() {
    let mut data = Vec::new();
    encode_tag(&mut data, 1, WireType::LengthDelimited);
    encode_varint32(&mut data, 0); // zero length

    let fields: Vec<_> = ProtoFieldIter::new(&data)
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert_eq!(fields.len(), 1);
    assert_eq!(fields[0].value, FieldValue::LengthDelimited(&[]));
}

#[test]
fn adv_length_delimited_exact_remaining() {
    // Length-delimited field where declared length exactly equals remaining bytes.
    let mut data = Vec::new();
    encode_tag(&mut data, 1, WireType::LengthDelimited);
    encode_varint32(&mut data, 5);
    data.extend_from_slice(b"exact");

    let fields: Vec<_> = ProtoFieldIter::new(&data)
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert_eq!(fields.len(), 1);
    assert_eq!(fields[0].value, FieldValue::LengthDelimited(b"exact"));
}

#[test]
fn adv_length_delimited_off_by_one_overflow() {
    // Declared length is 1 byte more than available.
    let mut data = Vec::new();
    encode_tag(&mut data, 1, WireType::LengthDelimited);
    encode_varint32(&mut data, 6); // says 6 bytes
    data.extend_from_slice(b"exact"); // only 5 bytes

    let results: Vec<_> = ProtoFieldIter::new(&data).collect();
    assert_eq!(results.len(), 1);
    assert!(results[0].is_err());
}

// ---------------------------------------------------------------------------
// Edge case: fixed types at slice boundary
// ---------------------------------------------------------------------------

#[test]
fn adv_fixed32_exact_fit() {
    let mut data = Vec::new();
    encode_tag(&mut data, 1, WireType::Fixed32);
    data.extend_from_slice(&[0xFF, 0xFF, 0xFF, 0xFF]);

    let fields: Vec<_> = ProtoFieldIter::new(&data)
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert_eq!(fields[0].value, FieldValue::Fixed32(0xFFFFFFFF));
}

#[test]
fn adv_fixed64_exact_fit() {
    let mut data = Vec::new();
    encode_tag(&mut data, 1, WireType::Fixed64);
    data.extend_from_slice(&[0xFF; 8]);

    let fields: Vec<_> = ProtoFieldIter::new(&data)
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert_eq!(fields[0].value, FieldValue::Fixed64(u64::MAX));
}

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
// Edge case: wire types 6 and 7 at various positions
// ---------------------------------------------------------------------------

#[test]
fn adv_wire_type_6_large_field_number() {
    // Wire type 6 for a large field number (100).
    // tag = (100 << 3) | 6 = 806 = 0x326
    // As varint: [0xA6, 0x06]
    let data = [0xA6, 0x06];
    let results: Vec<_> = ProtoFieldIter::new(&data).collect();
    assert_eq!(results.len(), 1);
    assert!(results[0].is_err());
}

#[test]
fn adv_wire_type_7_large_field_number() {
    // Wire type 7 for field number 100.
    // tag = (100 << 3) | 7 = 807 = 0x327
    // As varint: [0xA7, 0x06]
    let data = [0xA7, 0x06];
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
    // 3 valid varint fields
    for i in 1..=3u32 {
        encode_tag(&mut data, i, WireType::Varint);
        encode_varint64(&mut data, i as u64 * 100);
    }
    // Then a truncated fixed64
    encode_tag(&mut data, 4, WireType::Fixed64);
    data.extend_from_slice(&[0x00; 3]); // only 3 of 8 bytes

    let results: Vec<_> = ProtoFieldIter::new(&data).collect();
    assert_eq!(results.len(), 4); // 3 Ok + 1 Err
    for i in 0..3 {
        assert!(results[i].is_ok());
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
    // 10 levels of nested groups.
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
    // The iterator yields flat events — it does NOT validate group balance.
    // An EndGroup without a preceding StartGroup should still be yielded.
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
// Edge case: make_tag round-trips with all wire types
// ---------------------------------------------------------------------------

#[test]
fn adv_make_tag_all_wire_types_all_field_numbers() {
    use riegeli::proto_wire::{tag_field_number, tag_wire_type};

    let wire_types = [
        WireType::Varint,
        WireType::Fixed64,
        WireType::LengthDelimited,
        WireType::StartGroup,
        WireType::EndGroup,
        WireType::Fixed32,
    ];

    for &wt in &wire_types {
        for &field in &[1u32, 2, 15, 16, 2047, 2048, 0x1FFFFFFF] {
            let tag = make_tag(field, wt);
            assert_eq!(tag_wire_type(tag), Some(wt));
            assert_eq!(tag_field_number(tag), field);
        }
    }
}

// ---------------------------------------------------------------------------
// Edge case: read_canonical_varint64 edge cases
// ---------------------------------------------------------------------------

#[test]
fn adv_read_varint64_at_offset() {
    // Read from a non-zero offset.
    let data = [0xFF, 0xFF, 0x00, 0x96, 0x01]; // varint 150 starts at index 3
    let result = read_canonical_varint64(&data, 3);
    assert_eq!(result, Some((150, 2)));
}

// BUG: read_canonical_varint64 panics when pos > data.len() instead of
// Previously panicked with out-of-range index. Now correctly returns None.
#[test]
fn adv_read_varint64_offset_far_past_end_returns_none() {
    let data = [0x01];
    assert_eq!(read_canonical_varint64(&data, 100), None);
}

#[test]
fn adv_read_varint64_offset_at_len_returns_none() {
    // pos == data.len() should return None (empty remaining slice).
    let data = [0x01];
    assert_eq!(read_canonical_varint64(&data, 1), None);
}

#[test]
fn adv_read_varint64_max_10_byte_with_bit1() {
    // 10-byte varint where the 10th byte is 0x01 — u64::MAX.
    let data = [0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x01];
    assert_eq!(read_canonical_varint64(&data, 0), Some((u64::MAX, 10)));
}

#[test]
fn adv_read_varint64_10th_byte_too_large() {
    // 10th byte is 0x02 — would overflow u64.
    let data = [0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x02];
    assert_eq!(read_canonical_varint64(&data, 0), None);
}

// ---------------------------------------------------------------------------
// Round-trip: large message with many fields
// ---------------------------------------------------------------------------

#[test]
fn adv_round_trip_100_fields() {
    let mut original = Vec::new();
    for i in 1..=100u32 {
        encode_tag(&mut original, i, WireType::Varint);
        encode_varint64(&mut original, i as u64 * 1000);
    }

    let fields: Vec<_> = ProtoFieldIter::new(&original)
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert_eq!(fields.len(), 100);

    let mut reserialized = Vec::new();
    for f in &fields {
        serialize_field(&mut reserialized, f);
    }
    assert_eq!(original, reserialized);
}

#[test]
fn adv_round_trip_mixed_types_many_fields() {
    let mut original = Vec::new();

    for i in 1..=20u32 {
        // Varint
        encode_tag(&mut original, i, WireType::Varint);
        encode_varint64(&mut original, i as u64);

        // Fixed32
        encode_tag(&mut original, i + 100, WireType::Fixed32);
        original.extend_from_slice(&(i.wrapping_mul(0x01010101)).to_le_bytes());

        // Fixed64
        encode_tag(&mut original, i + 200, WireType::Fixed64);
        original.extend_from_slice(&(i as u64 * 0x0101010101010101u64).to_le_bytes());

        // Length-delimited
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
// Edge case: all single-byte inputs (fuzz-like)
// ---------------------------------------------------------------------------

#[test]
fn adv_no_panic_on_all_single_bytes() {
    for b in 0u8..=255 {
        let _: Vec<_> = ProtoFieldIter::new(&[b]).collect();
    }
}

#[test]
fn adv_no_panic_on_all_two_byte_combos() {
    // Sample two-byte combinations — exhaustive would be 65536 but we sample.
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
    // Field 16, varint: tag = (16 << 3) | 0 = 128 = 0x80 -> needs 2-byte varint [0x80, 0x01]
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
    // Field 1, length-delimited, but the length varint itself is truncated.
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

// ---------------------------------------------------------------------------
// Edge: varint64 with 10th byte = 0x00 (non-canonical trailing zero)
// ---------------------------------------------------------------------------

#[test]
fn eval_varint64_10th_byte_zero_noncanonical() {
    // 9 continuation bytes followed by 0x00 -- this is a 10-byte varint whose
    // last byte is 0, which is non-canonical.
    let mut data = vec![0x08]; // tag: field 1, varint
    data.extend_from_slice(&[0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x00]);

    let results: Vec<_> = ProtoFieldIter::new(&data).collect();
    assert_eq!(results.len(), 1);
    assert!(
        results[0].is_err(),
        "10-byte varint with trailing 0x00 should be rejected as non-canonical"
    );
}

// ---------------------------------------------------------------------------
// Edge: varint32 tag with 5th byte overflow (value > 0x0F)
// ---------------------------------------------------------------------------

#[test]
fn eval_tag_varint32_5th_byte_overflow() {
    // A 5-byte varint32 where the 5th byte has value 0x10 (only 0x00-0x0F valid).
    // This would represent a tag > u32::MAX.
    let data = [0x80, 0x80, 0x80, 0x80, 0x10];
    let results: Vec<_> = ProtoFieldIter::new(&data).collect();
    assert_eq!(results.len(), 1);
    assert!(
        results[0].is_err(),
        "varint32 overflow on 5th byte should error"
    );
}

// ---------------------------------------------------------------------------
// Edge: length-delimited with maximum varint32 length (just under 4GB)
// This should error because remaining slice is tiny.
// ---------------------------------------------------------------------------

#[test]
fn eval_length_delimited_huge_length() {
    let mut data = Vec::new();
    encode_tag(&mut data, 1, WireType::LengthDelimited);
    // Encode length = u32::MAX / 2 = 2147483647 (large but valid varint32)
    encode_varint32(&mut data, 0x7FFFFFFF);
    data.extend_from_slice(b"tiny"); // only 4 bytes of payload

    let results: Vec<_> = ProtoFieldIter::new(&data).collect();
    assert_eq!(results.len(), 1);
    assert!(results[0].is_err());
}

// ---------------------------------------------------------------------------
// Edge: field number wraps around in make_tag
// make_tag(0x20000000, Varint) = 0x100000000 which truncates to 0 as u32.
// ---------------------------------------------------------------------------

#[test]
fn eval_make_tag_field_number_overflow_wraps() {
    // Field number 0x20000000 = 2^29 is one past the max valid proto field number.
    // make_tag will produce (0x20000000 << 3) which overflows u32 to 0.
    let tag = make_tag(0x20000000, WireType::Varint);
    // The tag wraps around. This is caller responsibility to avoid, but we
    // document the behavior: it wraps to 0.
    assert_eq!(tag, 0, "overflow wraps to 0");
}

// ---------------------------------------------------------------------------
// Edge: fixed32 with value 0 (all zero bytes)
// ---------------------------------------------------------------------------

#[test]
fn eval_fixed32_zero() {
    let mut data = Vec::new();
    encode_tag(&mut data, 1, WireType::Fixed32);
    data.extend_from_slice(&0u32.to_le_bytes());

    let fields: Vec<_> = ProtoFieldIter::new(&data)
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert_eq!(fields[0].value, FieldValue::Fixed32(0));
}

// ---------------------------------------------------------------------------
// Edge: fixed64 with value 0 (all zero bytes)
// ---------------------------------------------------------------------------

#[test]
fn eval_fixed64_zero() {
    let mut data = Vec::new();
    encode_tag(&mut data, 1, WireType::Fixed64);
    data.extend_from_slice(&0u64.to_le_bytes());

    let fields: Vec<_> = ProtoFieldIter::new(&data)
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert_eq!(fields[0].value, FieldValue::Fixed64(0));
}

// ---------------------------------------------------------------------------
// Edge: length-delimited field containing binary zeros
// ---------------------------------------------------------------------------

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

// ---------------------------------------------------------------------------
// Edge: consecutive length-delimited fields (no gap)
// ---------------------------------------------------------------------------

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

// ---------------------------------------------------------------------------
// Edge: group containing only length-delimited field
// ---------------------------------------------------------------------------

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

// ---------------------------------------------------------------------------
// Edge: round-trip of group-containing message
// ---------------------------------------------------------------------------

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

// ---------------------------------------------------------------------------
// Edge: verify encode_varint32 and encode_varint64 agree for values < 2^32
// ---------------------------------------------------------------------------

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

// ---------------------------------------------------------------------------
// Edge: varint at exact byte boundaries (127, 16383, 2097151, etc.)
// ---------------------------------------------------------------------------

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

        // Also round-trip
        let mut reserialized = Vec::new();
        serialize_field(&mut reserialized, &fields[0]);
        assert_eq!(
            data, reserialized,
            "round-trip failed for boundary value {}",
            val
        );
    }
}

// ---------------------------------------------------------------------------
// Edge: ProtoField Debug/Clone/PartialEq derive verification
// ---------------------------------------------------------------------------

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
    // Verify Debug doesn't panic
    let _ = format!("{:?}", fields[0]);
}

// ---------------------------------------------------------------------------
// Edge: WireType::from_raw for all valid and invalid values
// ---------------------------------------------------------------------------

#[test]
fn eval_wire_type_from_raw_exhaustive() {
    // Valid: 0-5
    assert!(WireType::from_raw(0).is_some());
    assert!(WireType::from_raw(1).is_some());
    assert!(WireType::from_raw(2).is_some());
    assert!(WireType::from_raw(3).is_some());
    assert!(WireType::from_raw(4).is_some());
    assert!(WireType::from_raw(5).is_some());
    // Invalid: 6, 7
    assert!(WireType::from_raw(6).is_none());
    assert!(WireType::from_raw(7).is_none());
    // Out of range
    assert!(WireType::from_raw(8).is_none());
    assert!(WireType::from_raw(255).is_none());
    assert!(WireType::from_raw(u32::MAX).is_none());
}
