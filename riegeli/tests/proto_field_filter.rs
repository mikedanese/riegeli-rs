//! Sprint 28: Field Filtering, Copying, and Composition tests.

// Some imports are used only by feature-gated tests; in reduced-feature
// builds they would otherwise trip unused_imports.
#![cfg_attr(
    not(all(feature = "brotli", feature = "zstd", feature = "snappy")),
    allow(unused_imports)
)]
use riegeli::proto::{
    DynamicHandlerSet, FieldHandler, FieldValue, FilteredFieldIter, ProtoField, ProtoFieldIter,
    SerializedMessageWriter, StaticHandlerSet, WireType, copy_fields, read_message,
    serialize_field,
};

// ---------------------------------------------------------------------------
// Helper: build a test message with multiple field types
// ---------------------------------------------------------------------------

/// Creates a test message with:
/// - Field 1: varint 100
/// - Field 2: bytes "hello"
/// - Field 3: varint 200
/// - Field 4: fixed32 0xDEADBEEF
/// - Field 5: bytes "world"
fn build_test_message() -> Vec<u8> {
    let mut w = SerializedMessageWriter::new();
    w.write_uint64(1, 100).unwrap();
    w.write_bytes(2, b"hello").unwrap();
    w.write_uint64(3, 200).unwrap();
    w.write_fixed32(4, 0xDEADBEEF).unwrap();
    w.write_bytes(5, b"world").unwrap();
    w.finish().unwrap()
}

// ===========================================================================
// Filtering for fields {1, 3} yields only those fields
// ===========================================================================

#[test]
fn filter_yields_only_allowed_fields() {
    let data = build_test_message();
    let fields: Vec<ProtoField> = FilteredFieldIter::new(&data, &[1, 3])
        .collect::<Result<Vec<_>, _>>()
        .unwrap();

    assert_eq!(fields.len(), 2);
    assert_eq!(fields[0].field_number, 1);
    assert_eq!(fields[0].wire_type, WireType::Varint);
    assert_eq!(fields[0].value, FieldValue::Varint(100));
    assert_eq!(fields[1].field_number, 3);
    assert_eq!(fields[1].wire_type, WireType::Varint);
    assert_eq!(fields[1].value, FieldValue::Varint(200));
}

#[test]
fn filter_skips_fields_not_in_set() {
    let data = build_test_message();
    // Only request field 2 — fields 1, 3, 4, 5 should be skipped.
    let fields: Vec<ProtoField> = FilteredFieldIter::new(&data, &[2])
        .collect::<Result<Vec<_>, _>>()
        .unwrap();

    assert_eq!(fields.len(), 1);
    assert_eq!(fields[0].field_number, 2);
    assert_eq!(fields[0].value, FieldValue::LengthDelimited(b"hello"));
}

#[test]
fn filter_single_field_from_many() {
    let data = build_test_message();
    let fields: Vec<ProtoField> = FilteredFieldIter::new(&data, &[4])
        .collect::<Result<Vec<_>, _>>()
        .unwrap();

    assert_eq!(fields.len(), 1);
    assert_eq!(fields[0].field_number, 4);
    assert_eq!(fields[0].value, FieldValue::Fixed32(0xDEADBEEF));
}

#[test]
fn filter_all_fields_yields_same_as_unfiltered() {
    let data = build_test_message();
    let filtered: Vec<ProtoField> = FilteredFieldIter::new(&data, &[1, 2, 3, 4, 5])
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    let unfiltered: Vec<ProtoField> = ProtoFieldIter::new(&data)
        .collect::<Result<Vec<_>, _>>()
        .unwrap();

    assert_eq!(filtered, unfiltered);
}

// ===========================================================================
// Filtering for absent field yields empty result
// ===========================================================================

#[test]
fn filter_absent_field_yields_empty() {
    let data = build_test_message();
    let fields: Vec<ProtoField> = FilteredFieldIter::new(&data, &[99])
        .collect::<Result<Vec<_>, _>>()
        .unwrap();

    assert!(fields.is_empty());
}

#[test]
fn filter_empty_message_yields_empty() {
    let data: Vec<u8> = Vec::new();
    let fields: Vec<ProtoField> = FilteredFieldIter::new(&data, &[1, 2, 3])
        .collect::<Result<Vec<_>, _>>()
        .unwrap();

    assert!(fields.is_empty());
}

#[test]
fn filter_empty_field_set_yields_empty() {
    let data = build_test_message();
    let fields: Vec<ProtoField> = FilteredFieldIter::new(&data, &[])
        .collect::<Result<Vec<_>, _>>()
        .unwrap();

    assert!(fields.is_empty());
}

#[test]
fn filter_allowed_set_with_duplicates() {
    let mut w = SerializedMessageWriter::new();
    w.write_uint64(1, 42).unwrap();
    w.write_uint64(2, 99).unwrap();
    let data = w.finish().unwrap();

    // Duplicate entries in the allowed set must not change the result.
    let fields: Vec<ProtoField> = FilteredFieldIter::new(&data, &[1, 1, 1])
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert_eq!(fields.len(), 1);
    assert_eq!(fields[0].field_number, 1);
    assert_eq!(fields[0].value, FieldValue::Varint(42));
}

#[test]
fn filter_absent_and_present_fields() {
    let data = build_test_message();
    // Field 1 exists, field 99 does not.
    let fields: Vec<ProtoField> = FilteredFieldIter::new(&data, &[1, 99])
        .collect::<Result<Vec<_>, _>>()
        .unwrap();

    assert_eq!(fields.len(), 1);
    assert_eq!(fields[0].field_number, 1);
}

// ===========================================================================
// Field copier produces valid proto with byte-identical values
// ===========================================================================

#[test]
fn copy_fields_produces_valid_proto_with_selected_fields() {
    let data = build_test_message();
    let mut writer = SerializedMessageWriter::new();
    copy_fields(&data, &[1, 3], &mut writer).unwrap();
    let output = writer.finish().unwrap();

    // Parse the output and verify it contains exactly fields 1 and 3.
    let fields: Vec<ProtoField> = ProtoFieldIter::new(&output)
        .collect::<Result<Vec<_>, _>>()
        .unwrap();

    assert_eq!(fields.len(), 2);
    assert_eq!(fields[0].field_number, 1);
    assert_eq!(fields[0].value, FieldValue::Varint(100));
    assert_eq!(fields[1].field_number, 3);
    assert_eq!(fields[1].value, FieldValue::Varint(200));
}

#[test]
fn copy_fields_byte_identical_values() {
    let data = build_test_message();

    // Copy fields {2, 5} (bytes fields).
    let mut writer = SerializedMessageWriter::new();
    copy_fields(&data, &[2, 5], &mut writer).unwrap();
    let output = writer.finish().unwrap();

    let fields: Vec<ProtoField> = ProtoFieldIter::new(&output)
        .collect::<Result<Vec<_>, _>>()
        .unwrap();

    assert_eq!(fields.len(), 2);
    assert_eq!(fields[0].value, FieldValue::LengthDelimited(b"hello"));
    assert_eq!(fields[1].value, FieldValue::LengthDelimited(b"world"));

    // Also verify byte-level identity: build the expected output manually.
    let mut expected = SerializedMessageWriter::new();
    expected.write_bytes(2, b"hello").unwrap();
    expected.write_bytes(5, b"world").unwrap();
    let expected_bytes = expected.finish().unwrap();
    assert_eq!(output, expected_bytes);
}

#[test]
fn copy_fields_empty_selection_produces_empty_message() {
    let data = build_test_message();
    let mut writer = SerializedMessageWriter::new();
    copy_fields(&data, &[], &mut writer).unwrap();
    let output = writer.finish().unwrap();

    assert!(output.is_empty());
}

#[test]
fn copy_fields_absent_field_produces_empty_message() {
    let data = build_test_message();
    let mut writer = SerializedMessageWriter::new();
    copy_fields(&data, &[99], &mut writer).unwrap();
    let output = writer.finish().unwrap();

    assert!(output.is_empty());
}

// ===========================================================================
// Field copier preserves field ordering
// ===========================================================================

#[test]
fn copy_fields_preserves_ordering() {
    let data = build_test_message();
    // Request fields in reverse order of their appearance, but the output
    // should still follow the input order.
    let mut writer = SerializedMessageWriter::new();
    copy_fields(&data, &[5, 3, 1], &mut writer).unwrap();
    let output = writer.finish().unwrap();

    let fields: Vec<ProtoField> = ProtoFieldIter::new(&output)
        .collect::<Result<Vec<_>, _>>()
        .unwrap();

    assert_eq!(fields.len(), 3);
    assert_eq!(fields[0].field_number, 1); // first in input
    assert_eq!(fields[1].field_number, 3); // second in input
    assert_eq!(fields[2].field_number, 5); // third in input
}

#[test]
fn copy_fields_preserves_repeated_field_ordering() {
    // Build a message with repeated field 1.
    let mut w = SerializedMessageWriter::new();
    w.write_uint64(1, 10).unwrap();
    w.write_uint64(2, 20).unwrap();
    w.write_uint64(1, 30).unwrap();
    w.write_uint64(2, 40).unwrap();
    w.write_uint64(1, 50).unwrap();
    let data = w.finish().unwrap();

    let mut writer = SerializedMessageWriter::new();
    copy_fields(&data, &[1], &mut writer).unwrap();
    let output = writer.finish().unwrap();

    let fields: Vec<ProtoField> = ProtoFieldIter::new(&output)
        .collect::<Result<Vec<_>, _>>()
        .unwrap();

    assert_eq!(fields.len(), 3);
    assert_eq!(fields[0].value, FieldValue::Varint(10));
    assert_eq!(fields[1].value, FieldValue::Varint(30));
    assert_eq!(fields[2].value, FieldValue::Varint(50));
}

// ===========================================================================
// Copying around group fields
// ===========================================================================

#[test]
fn copy_fields_skips_unselected_group() {
    let mut w = SerializedMessageWriter::new();
    w.write_uint64(1, 42).unwrap();
    w.write_start_group(2).unwrap();
    w.write_end_group(2).unwrap();
    w.write_uint64(3, 99).unwrap();
    let data = w.finish().unwrap();

    // Copy only the varint fields; the unselected group must be skipped.
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

#[test]
fn copy_fields_preserves_group_start_end_tags() {
    let mut w = SerializedMessageWriter::new();
    w.write_uint64(1, 10).unwrap();
    w.write_start_group(2).unwrap();
    w.write_end_group(2).unwrap();
    w.write_uint64(3, 30).unwrap();
    let data = w.finish().unwrap();

    // Copy group field 2 — both the start and end tags share field_number=2
    // and both must survive the copy.
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
// Filter / copy round-trip agreement
// ===========================================================================

#[test]
fn filtered_fields_match_copied_output() {
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

    // Step 4: the re-parsed copy must match the filtered view.
    assert_eq!(result, filtered);
}

// ===========================================================================
// Two handlers with different mutable contexts compose
// ===========================================================================

/// Handler that accumulates varint sums for a given field number.
struct VarintSumHandler<'a> {
    sum: &'a mut u64,
}

impl<'a> FieldHandler for VarintSumHandler<'a> {
    const FIELD_NUMBER: u32 = 1;

    fn handle_varint(&mut self, value: u64) -> Result<(), riegeli::RiegeliError> {
        *self.sum += value;
        Ok(())
    }
}

/// Handler that collects string (length-delimited) values for a given field number.
struct StringCollector<'a> {
    strings: &'a mut Vec<Vec<u8>>,
}

impl<'a> FieldHandler for StringCollector<'a> {
    const FIELD_NUMBER: u32 = 2;

    fn handle_length_delimited(&mut self, data: &[u8]) -> Result<(), riegeli::RiegeliError> {
        self.strings.push(data.to_vec());
        Ok(())
    }
}

#[test]
fn compose_two_handlers_different_contexts_single_pass() {
    // Build a message with interleaved field 1 (varint) and field 2 (bytes).
    let mut w = SerializedMessageWriter::new();
    w.write_uint64(1, 10).unwrap();
    w.write_bytes(2, b"alpha").unwrap();
    w.write_uint64(1, 20).unwrap();
    w.write_bytes(2, b"beta").unwrap();
    w.write_uint64(1, 30).unwrap();
    let data = w.finish().unwrap();

    // Two separate mutable contexts.
    let mut varint_sum: u64 = 0;
    let mut collected_strings: Vec<Vec<u8>> = Vec::new();

    // Compose using StaticHandlerSet2 — both handlers in a single pass.
    {
        let h1 = VarintSumHandler {
            sum: &mut varint_sum,
        };
        let h2 = StringCollector {
            strings: &mut collected_strings,
        };
        let mut handlers = StaticHandlerSet::new(h1).and(h2);
        read_message(&data, &mut handlers).unwrap();
    }

    assert_eq!(varint_sum, 60); // 10 + 20 + 30
    assert_eq!(collected_strings.len(), 2);
    assert_eq!(collected_strings[0], b"alpha");
    assert_eq!(collected_strings[1], b"beta");
}

#[test]
fn compose_handlers_no_cross_talk() {
    // Verify that handler 1 never sees field 2's data and vice versa.
    let mut w = SerializedMessageWriter::new();
    w.write_uint64(1, 42).unwrap();
    w.write_bytes(2, b"test").unwrap();
    w.write_uint64(3, 999).unwrap(); // unhandled field
    let data = w.finish().unwrap();

    let mut varint_sum: u64 = 0;
    let mut collected_strings: Vec<Vec<u8>> = Vec::new();

    {
        let h1 = VarintSumHandler {
            sum: &mut varint_sum,
        };
        let h2 = StringCollector {
            strings: &mut collected_strings,
        };
        let mut handlers = StaticHandlerSet::new(h1).and(h2);
        read_message(&data, &mut handlers).unwrap();
    }

    // Only field 1's varint was accumulated; field 3 was skipped.
    assert_eq!(varint_sum, 42);
    // Only field 2's string was collected.
    assert_eq!(collected_strings, vec![b"test".to_vec()]);
}

#[test]
fn compose_three_handlers_different_contexts() {
    // Use StaticHandlerSet3 to compose three handlers.
    struct Fixed32Collector<'a> {
        values: &'a mut Vec<u32>,
    }

    impl<'a> FieldHandler for Fixed32Collector<'a> {
        const FIELD_NUMBER: u32 = 3;

        fn handle_fixed32(&mut self, value: u32) -> Result<(), riegeli::RiegeliError> {
            self.values.push(value);
            Ok(())
        }
    }

    let mut w = SerializedMessageWriter::new();
    w.write_uint64(1, 7).unwrap();
    w.write_bytes(2, b"x").unwrap();
    w.write_fixed32(3, 42).unwrap();
    w.write_uint64(1, 3).unwrap();
    w.write_fixed32(3, 99).unwrap();
    let data = w.finish().unwrap();

    let mut varint_sum: u64 = 0;
    let mut strings: Vec<Vec<u8>> = Vec::new();
    let mut fixed_values: Vec<u32> = Vec::new();

    {
        let h1 = VarintSumHandler {
            sum: &mut varint_sum,
        };
        let h2 = StringCollector {
            strings: &mut strings,
        };
        let h3 = Fixed32Collector {
            values: &mut fixed_values,
        };
        let mut handlers = StaticHandlerSet::new(h1).and(h2).and(h3);
        read_message(&data, &mut handlers).unwrap();
    }

    assert_eq!(varint_sum, 10);
    assert_eq!(strings, vec![b"x".to_vec()]);
    assert_eq!(fixed_values, vec![42, 99]);
}

// ===========================================================================
// Three composed handlers with disjoint mutable contexts, including a
// fixed64 handler, dispatch correctly in a single pass.
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
fn compose_handlers_fixed64_dispatch() {
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
// No per-field heap allocation
// ===========================================================================

// This criterion is structural: the static handler set uses generics and trait
// dispatch, not Box<dyn ...>. We verify this by:
// 1. Using StaticHandlerSet (compile-time monomorphized, no boxing).
// 2. Context is captured via &mut references in the handler structs.
// 3. The test below processes many fields and would show allocation pressure
//    if per-field boxing were happening. We verify correctness here; the
//    absence of per-field allocation is a design property of StaticHandlerSet.

#[test]
fn static_handler_composition_no_boxing() {
    // Process a large number of fields through static handlers to verify
    // that the composition works without any per-field allocation.
    let mut w = SerializedMessageWriter::new();
    for i in 0..1000u64 {
        w.write_uint64(1, i).unwrap();
        w.write_bytes(2, b"data").unwrap();
    }
    let data = w.finish().unwrap();

    let mut varint_sum: u64 = 0;
    let mut string_count: usize = 0;

    // Use closures captured in handler structs — purely stack-based context.
    struct SumHandler<'a> {
        sum: &'a mut u64,
    }
    impl<'a> FieldHandler for SumHandler<'a> {
        const FIELD_NUMBER: u32 = 1;
        fn handle_varint(&mut self, value: u64) -> Result<(), riegeli::RiegeliError> {
            *self.sum += value;
            Ok(())
        }
    }

    struct CountHandler<'a> {
        count: &'a mut usize,
    }
    impl<'a> FieldHandler for CountHandler<'a> {
        const FIELD_NUMBER: u32 = 2;
        fn handle_length_delimited(&mut self, _data: &[u8]) -> Result<(), riegeli::RiegeliError> {
            *self.count += 1;
            Ok(())
        }
    }

    {
        let h1 = SumHandler {
            sum: &mut varint_sum,
        };
        let h2 = CountHandler {
            count: &mut string_count,
        };
        let mut handlers = StaticHandlerSet::new(h1).and(h2);
        read_message(&data, &mut handlers).unwrap();
    }

    // Sum of 0..1000 = 999 * 1000 / 2 = 499500
    assert_eq!(varint_sum, 499500);
    assert_eq!(string_count, 1000);
}

#[test]
fn dynamic_handler_set_no_per_field_alloc() {
    // DynamicHandlerSet allocates Box closures at registration time, but
    // per-field dispatch is a HashMap lookup — no allocation. The closures
    // capture context via references, not per-invocation boxing.
    let mut w = SerializedMessageWriter::new();
    for i in 0..100u64 {
        w.write_uint64(1, i).unwrap();
    }
    let data = w.finish().unwrap();

    let mut sum: u64 = 0;
    {
        let mut handlers = DynamicHandlerSet::new();
        handlers.on_varint(1, |v| {
            sum += v;
            Ok(())
        });
        read_message(&data, &mut handlers).unwrap();
    }

    assert_eq!(sum, 4950); // 0 + 1 + ... + 99
}

// ===========================================================================
// Additional edge case tests
// ===========================================================================

#[test]
fn copy_fields_round_trip_all_wire_types() {
    // Build a message with all wire types, copy all of them, verify round-trip.
    let mut w = SerializedMessageWriter::new();
    w.write_uint64(1, 42).unwrap();
    w.write_fixed32(2, 0x12345678).unwrap();
    w.write_fixed64(3, 0xDEADBEEFCAFEBABE).unwrap();
    w.write_bytes(4, b"hello world").unwrap();
    w.write_start_group(5).unwrap();
    w.write_end_group(5).unwrap();
    let data = w.finish().unwrap();

    let mut writer = SerializedMessageWriter::new();
    copy_fields(&data, &[1, 2, 3, 4, 5], &mut writer).unwrap();
    let output = writer.finish().unwrap();

    // The output should be byte-identical to the input when all fields are copied.
    assert_eq!(output, data);
}

#[test]
fn filter_propagates_errors_on_malformed_input() {
    // Truncated input — should propagate the error.
    let data = [0x08]; // Tag for field 1 varint, but no value follows.
    let result: Result<Vec<_>, _> = FilteredFieldIter::new(&data, &[1]).collect();
    assert!(result.is_err());
}

#[test]
fn copy_fields_propagates_errors_on_malformed_input() {
    let data = [0x08]; // Truncated.
    let mut writer = SerializedMessageWriter::new();
    let result = copy_fields(&data, &[1], &mut writer);
    assert!(result.is_err());
}

#[test]
fn copy_fields_with_nested_submessage() {
    // Build a message with a nested submessage (field 2), copy only field 2.
    let mut w = SerializedMessageWriter::new();
    w.write_uint64(1, 10).unwrap();
    w.open_length_delimited(2).unwrap();
    w.write_uint64(1, 99).unwrap();
    w.close_length_delimited().unwrap();
    w.write_uint64(3, 30).unwrap();
    let data = w.finish().unwrap();

    let mut writer = SerializedMessageWriter::new();
    copy_fields(&data, &[2], &mut writer).unwrap();
    let output = writer.finish().unwrap();

    // Verify the output has only field 2 (submessage).
    let fields: Vec<ProtoField> = ProtoFieldIter::new(&output)
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert_eq!(fields.len(), 1);
    assert_eq!(fields[0].field_number, 2);

    // Verify the submessage content is intact.
    if let FieldValue::LengthDelimited(inner) = &fields[0].value {
        let inner_fields: Vec<ProtoField> = ProtoFieldIter::new(inner)
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(inner_fields.len(), 1);
        assert_eq!(inner_fields[0].field_number, 1);
        assert_eq!(inner_fields[0].value, FieldValue::Varint(99));
    } else {
        panic!("expected LengthDelimited for field 2");
    }
}

#[test]
fn write_field_method_on_writer() {
    // Verify that SerializedMessageWriter::write_field produces the same
    // bytes as serialize_field.
    let mut w = SerializedMessageWriter::new();
    w.write_uint64(1, 150).unwrap();
    let data = w.finish().unwrap();

    let fields: Vec<ProtoField> = ProtoFieldIter::new(&data)
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert_eq!(fields.len(), 1);

    // Use write_field to copy.
    let mut w2 = SerializedMessageWriter::new();
    w2.write_field(&fields[0]).unwrap();
    let output = w2.finish().unwrap();

    // And use serialize_field for comparison.
    let mut buf = Vec::new();
    serialize_field(&mut buf, &fields[0]);

    assert_eq!(output, buf);
    assert_eq!(output, data);
}
