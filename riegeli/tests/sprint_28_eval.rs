//! Sprint 28 Evaluator adversarial tests: field filtering, copying, composition.

use riegeli::proto::{
    FieldHandler, FieldValue, FilteredFieldIter, HandleField, ProtoField, ProtoFieldIter,
    SerializedMessageWriter, StaticHandlerSet, WireType, copy_fields, read_message,
};

// ---------------------------------------------------------------------------
// Helper: build a message with many field numbers
// ---------------------------------------------------------------------------

fn build_large_message(n: u32) -> Vec<u8> {
    let mut w = SerializedMessageWriter::new();
    for i in 1..=n {
        w.write_uint64(i, i as u64 * 100).unwrap();
    }
    w.finish().unwrap()
}

// ===========================================================================
// ADV-28-03: Copy fields from message with groups
// ===========================================================================

#[test]
fn adv_copy_fields_with_groups() {
    let mut w = SerializedMessageWriter::new();
    w.write_uint64(1, 42).unwrap();
    w.write_start_group(2).unwrap();
    w.write_end_group(2).unwrap();
    w.write_uint64(3, 99).unwrap();
    let data = w.finish().unwrap();

    // Copy only varint fields, skipping group
    let mut writer = SerializedMessageWriter::new();
    copy_fields(&data, &[1, 3], &mut writer).unwrap();
    let output = writer.finish().unwrap();

    let fields: Vec<ProtoField> = ProtoFieldIter::new(&output)
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert_eq!(fields.len(), 2);
    assert_eq!(fields[0].field_number, 1);
    assert_eq!(fields[0].value, FieldValue::Varint(42));
    assert_eq!(fields[1].field_number, 3);
    assert_eq!(fields[1].value, FieldValue::Varint(99));
}

// ===========================================================================
// ADV-28-04: Copy fields including group start/end preserves them
// ===========================================================================

#[test]
fn adv_copy_fields_includes_group_tags() {
    let mut w = SerializedMessageWriter::new();
    w.write_uint64(1, 10).unwrap();
    w.write_start_group(2).unwrap();
    w.write_end_group(2).unwrap();
    w.write_uint64(3, 30).unwrap();
    let data = w.finish().unwrap();

    // Copy group field 2 (both start and end tags share field_number=2)
    let mut writer = SerializedMessageWriter::new();
    copy_fields(&data, &[2], &mut writer).unwrap();
    let output = writer.finish().unwrap();

    let fields: Vec<ProtoField> = ProtoFieldIter::new(&output)
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert_eq!(fields.len(), 2);
    assert_eq!(fields[0].wire_type, WireType::StartGroup);
    assert_eq!(fields[0].field_number, 2);
    assert_eq!(fields[1].wire_type, WireType::EndGroup);
    assert_eq!(fields[1].field_number, 2);
}

// ===========================================================================
// ADV-28-05: Copy fields preserving repeated field order
// ===========================================================================

#[test]
fn adv_copy_preserves_repeated_interleaved_order() {
    let mut w = SerializedMessageWriter::new();
    // Interleaved repeated fields 1 and 2
    w.write_uint64(1, 1).unwrap();
    w.write_uint64(2, 2).unwrap();
    w.write_uint64(1, 3).unwrap();
    w.write_uint64(2, 4).unwrap();
    w.write_uint64(1, 5).unwrap();
    w.write_uint64(2, 6).unwrap();
    let data = w.finish().unwrap();

    // Copy both fields — output must preserve interleaved order
    let mut writer = SerializedMessageWriter::new();
    copy_fields(&data, &[1, 2], &mut writer).unwrap();
    let output = writer.finish().unwrap();

    // Must be byte-identical to input since we selected all fields
    assert_eq!(output, data);

    let fields: Vec<ProtoField> = ProtoFieldIter::new(&output)
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    let values: Vec<u64> = fields
        .iter()
        .map(|f| match f.value {
            FieldValue::Varint(v) => v,
            _ => panic!("expected varint"),
        })
        .collect();
    assert_eq!(values, vec![1, 2, 3, 4, 5, 6]);
}

// ===========================================================================
// ADV-28-06: Composition with 3 handlers capturing different mutable contexts
// ===========================================================================

struct VarintAccum<'a> {
    vals: &'a mut Vec<u64>,
}
impl<'a> FieldHandler for VarintAccum<'a> {
    const FIELD_NUMBER: u32 = 1;
    fn handle_varint(&mut self, value: u64) -> Result<(), riegeli::RiegeliError> {
        self.vals.push(value);
        Ok(())
    }
}

struct BytesAccum<'a> {
    lens: &'a mut Vec<usize>,
}
impl<'a> FieldHandler for BytesAccum<'a> {
    const FIELD_NUMBER: u32 = 2;
    fn handle_length_delimited(&mut self, data: &[u8]) -> Result<(), riegeli::RiegeliError> {
        self.lens.push(data.len());
        Ok(())
    }
}

struct Fixed64Accum<'a> {
    vals: &'a mut Vec<u64>,
}
impl<'a> FieldHandler for Fixed64Accum<'a> {
    const FIELD_NUMBER: u32 = 3;
    fn handle_fixed64(&mut self, value: u64) -> Result<(), riegeli::RiegeliError> {
        self.vals.push(value);
        Ok(())
    }
}

#[test]
fn adv_three_handler_composition_disjoint_contexts() {
    let mut w = SerializedMessageWriter::new();
    w.write_uint64(1, 10).unwrap();
    w.write_bytes(2, b"abc").unwrap();
    w.write_fixed64(3, 0xCAFE).unwrap();
    w.write_uint64(1, 20).unwrap();
    w.write_bytes(2, b"defgh").unwrap();
    w.write_fixed64(3, 0xBEEF).unwrap();
    w.write_uint64(4, 9999).unwrap(); // unhandled
    let data = w.finish().unwrap();

    let mut varints: Vec<u64> = Vec::new();
    let mut byte_lens: Vec<usize> = Vec::new();
    let mut fixed64s: Vec<u64> = Vec::new();

    {
        let h1 = VarintAccum { vals: &mut varints };
        let h2 = BytesAccum {
            lens: &mut byte_lens,
        };
        let h3 = Fixed64Accum {
            vals: &mut fixed64s,
        };
        let mut handlers = StaticHandlerSet::new(h1).and(h2).and(h3);
        read_message(&data, &mut handlers).unwrap();
    }

    assert_eq!(varints, vec![10, 20]);
    assert_eq!(byte_lens, vec![3, 5]);
    assert_eq!(fixed64s, vec![0xCAFE, 0xBEEF]);
}

// ===========================================================================
// ADV-28-07: Large message filtering for single field
// ===========================================================================

#[test]
fn adv_large_message_filter_single_field() {
    let data = build_large_message(1000);
    let fields: Vec<ProtoField> = FilteredFieldIter::new(&data, &[500])
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert_eq!(fields.len(), 1);
    assert_eq!(fields[0].field_number, 500);
    assert_eq!(fields[0].value, FieldValue::Varint(50000));
}

// ===========================================================================
// ADV-28-08: Round-trip: original -> filter -> copy -> iterate matches subset
// ===========================================================================

#[test]
fn adv_roundtrip_filter_copy_iterate() {
    let mut w = SerializedMessageWriter::new();
    w.write_uint64(1, 111).unwrap();
    w.write_bytes(2, b"test data").unwrap();
    w.write_fixed32(3, 0xABCD).unwrap();
    w.write_fixed64(4, 0x123456789ABCDEF0).unwrap();
    w.write_bytes(5, b"more bytes").unwrap();
    let data = w.finish().unwrap();

    // Step 1: filter for {2, 4}
    let filtered: Vec<ProtoField> = FilteredFieldIter::new(&data, &[2, 4])
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert_eq!(filtered.len(), 2);

    // Step 2: copy those fields to a new writer
    let mut writer = SerializedMessageWriter::new();
    copy_fields(&data, &[2, 4], &mut writer).unwrap();
    let copied = writer.finish().unwrap();

    // Step 3: iterate the copied output
    let result: Vec<ProtoField> = ProtoFieldIter::new(&copied)
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert_eq!(result.len(), 2);
    assert_eq!(result[0].field_number, 2);
    assert_eq!(result[0].value, FieldValue::LengthDelimited(b"test data"));
    assert_eq!(result[1].field_number, 4);
    assert_eq!(result[1].value, FieldValue::Fixed64(0x123456789ABCDEF0));

    // Step 4: verify result matches filtered
    assert_eq!(result, filtered);
}

// ===========================================================================
// ADV-28-09: Filter with duplicate field numbers in allowed set
// ===========================================================================

#[test]
fn adv_filter_duplicate_allowed_numbers() {
    let mut w = SerializedMessageWriter::new();
    w.write_uint64(1, 42).unwrap();
    w.write_uint64(2, 99).unwrap();
    let data = w.finish().unwrap();

    // Allowed set has duplicates — should still work correctly
    let fields: Vec<ProtoField> = FilteredFieldIter::new(&data, &[1, 1, 1])
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert_eq!(fields.len(), 1);
    assert_eq!(fields[0].field_number, 1);
    assert_eq!(fields[0].value, FieldValue::Varint(42));
}

// ===========================================================================
// ADV-28-11: Filter preserves byte-identity for length-delimited values
// ===========================================================================

#[test]
fn adv_filter_length_delimited_byte_identity() {
    let payload = vec![0u8; 1024]; // 1KB of zeros
    let mut w = SerializedMessageWriter::new();
    w.write_bytes(1, &payload).unwrap();
    w.write_uint64(2, 7).unwrap();
    let data = w.finish().unwrap();

    let fields: Vec<ProtoField> = FilteredFieldIter::new(&data, &[1])
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert_eq!(fields.len(), 1);
    match &fields[0].value {
        FieldValue::LengthDelimited(d) => {
            assert_eq!(d.len(), 1024);
            assert!(d.iter().all(|&b| b == 0));
        }
        _ => panic!("expected LengthDelimited"),
    }
}

// ===========================================================================
// ADV-28-12: Copy-then-copy produces same result as single copy
// ===========================================================================

#[test]
fn adv_copy_idempotent() {
    let mut w = SerializedMessageWriter::new();
    w.write_uint64(1, 100).unwrap();
    w.write_bytes(2, b"hello").unwrap();
    w.write_fixed32(3, 42).unwrap();
    let data = w.finish().unwrap();

    // First copy: fields {1, 2}
    let mut w1 = SerializedMessageWriter::new();
    copy_fields(&data, &[1, 2], &mut w1).unwrap();
    let copy1 = w1.finish().unwrap();

    // Second copy from copy1: fields {1, 2} again (should be identity)
    let mut w2 = SerializedMessageWriter::new();
    copy_fields(&copy1, &[1, 2], &mut w2).unwrap();
    let copy2 = w2.finish().unwrap();

    assert_eq!(copy1, copy2, "double-copy must be idempotent");
}

// ===========================================================================
// ADV-28-13: Handler error during composition stops processing
// ===========================================================================

#[test]
fn adv_handler_error_during_composition_stops() {
    struct ErrorHandler;
    impl FieldHandler for ErrorHandler {
        const FIELD_NUMBER: u32 = 2;
        fn handle_length_delimited(&mut self, _: &[u8]) -> Result<(), riegeli::RiegeliError> {
            Err(riegeli::RiegeliError::MalformedData(
                "deliberate handler error".to_string(),
            ))
        }
    }

    struct CountHandler<'a> {
        count: &'a mut u32,
    }
    impl<'a> FieldHandler for CountHandler<'a> {
        const FIELD_NUMBER: u32 = 3;
        fn handle_varint(&mut self, _: u64) -> Result<(), riegeli::RiegeliError> {
            *self.count += 1;
            Ok(())
        }
    }

    let mut w = SerializedMessageWriter::new();
    w.write_uint64(1, 1).unwrap();
    w.write_bytes(2, b"boom").unwrap(); // triggers error
    w.write_uint64(3, 999).unwrap(); // should NOT be reached
    let data = w.finish().unwrap();

    let mut count: u32 = 0;
    {
        let h1 = ErrorHandler;
        let h2 = CountHandler { count: &mut count };
        let mut handlers = StaticHandlerSet::new(h1).and(h2);
        let result = read_message(&data, &mut handlers);
        assert!(result.is_err());
    }
    assert_eq!(
        count, 0,
        "field 3 handler should not have been invoked after error"
    );
}
