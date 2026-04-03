//! Tests for Sprint 27: SerializedMessageReader with Field Handlers.
//!
//! Covers all 10 success criteria for the handler dispatch framework.

use std::cell::RefCell;

use riegeli::RiegeliError;
use riegeli::proto_wire::{
    DynamicHandlerSet, EmptyHandlerSet, FieldHandler, SerializedMessageWriter, StaticHandlerSet,
    read_message,
};

// ---------------------------------------------------------------------------
// Helper: static handler implementations
// ---------------------------------------------------------------------------

/// A static handler that collects varint values for field 1.
struct Field1VarintCollector {
    values: Vec<u64>,
}

impl Field1VarintCollector {
    fn new() -> Self {
        Self { values: Vec::new() }
    }
}

impl FieldHandler for Field1VarintCollector {
    const FIELD_NUMBER: u32 = 1;

    fn handle_varint(&mut self, value: u64) -> Result<(), RiegeliError> {
        self.values.push(value);
        Ok(())
    }
}

/// A static handler that collects length-delimited bytes for field 2.
struct Field2BytesCollector {
    values: Vec<Vec<u8>>,
}

impl Field2BytesCollector {
    fn new() -> Self {
        Self { values: Vec::new() }
    }
}

impl FieldHandler for Field2BytesCollector {
    const FIELD_NUMBER: u32 = 2;

    fn handle_length_delimited(&mut self, data: &[u8]) -> Result<(), RiegeliError> {
        self.values.push(data.to_vec());
        Ok(())
    }
}

/// A static handler for field 3 that collects fixed32 values.
struct Field3Fixed32Collector {
    values: Vec<u32>,
}

impl Field3Fixed32Collector {
    fn new() -> Self {
        Self { values: Vec::new() }
    }
}

impl FieldHandler for Field3Fixed32Collector {
    const FIELD_NUMBER: u32 = 3;

    fn handle_fixed32(&mut self, value: u32) -> Result<(), RiegeliError> {
        self.values.push(value);
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Criterion 27.1: Handler for field 1 varint receives correct value;
//                 fields 2 and 3 are skipped.
// ---------------------------------------------------------------------------

#[test]
fn criterion_27_1_varint_handler_receives_value_others_skipped() {
    // Build a message with fields 1 (varint=42), 2 (bytes="hello"), 3 (fixed32=99)
    let mut w = SerializedMessageWriter::new();
    w.write_uint64(1, 42).unwrap();
    w.write_bytes(2, b"hello").unwrap();
    w.write_fixed32(3, 99).unwrap();
    let data = w.finish().unwrap();

    let mut handler = StaticHandlerSet::new(Field1VarintCollector::new());
    read_message(&data, &mut handler).unwrap();

    assert_eq!(handler.h1.values, vec![42]);
}

#[test]
fn criterion_27_1_only_matching_field_invoked() {
    // Build message: field 2 (varint=100), field 3 (varint=200)
    let mut w = SerializedMessageWriter::new();
    w.write_uint64(2, 100).unwrap();
    w.write_uint64(3, 200).unwrap();
    let data = w.finish().unwrap();

    let mut handler = StaticHandlerSet::new(Field1VarintCollector::new());
    read_message(&data, &mut handler).unwrap();

    // Field 1 handler should receive nothing since no field 1 in message
    assert!(handler.h1.values.is_empty());
}

// ---------------------------------------------------------------------------
// Criterion 27.2: Handler for length-delimited field receives matching bytes.
// ---------------------------------------------------------------------------

#[test]
fn criterion_27_2_length_delimited_handler_receives_bytes() {
    let payload = b"binary\x00data\xff";
    let mut w = SerializedMessageWriter::new();
    w.write_bytes(2, payload).unwrap();
    let data = w.finish().unwrap();

    let mut handler = StaticHandlerSet::new(Field2BytesCollector::new());
    read_message(&data, &mut handler).unwrap();

    assert_eq!(handler.h1.values.len(), 1);
    assert_eq!(handler.h1.values[0], payload);
}

#[test]
fn criterion_27_2_string_field_received_as_bytes() {
    let mut w = SerializedMessageWriter::new();
    w.write_string(2, "proto string").unwrap();
    let data = w.finish().unwrap();

    let mut handler = StaticHandlerSet::new(Field2BytesCollector::new());
    read_message(&data, &mut handler).unwrap();

    assert_eq!(handler.h1.values[0], b"proto string");
}

// ---------------------------------------------------------------------------
// Criterion 27.3: Two handlers for different field numbers — no cross-talk.
// ---------------------------------------------------------------------------

#[test]
fn criterion_27_3_two_handlers_no_crosstalk() {
    let mut w = SerializedMessageWriter::new();
    w.write_uint64(1, 10).unwrap();
    w.write_bytes(2, b"abc").unwrap();
    w.write_uint64(1, 20).unwrap();
    w.write_bytes(2, b"def").unwrap();
    let data = w.finish().unwrap();

    let mut handlers =
        StaticHandlerSet::new(Field1VarintCollector::new()).and(Field2BytesCollector::new());
    read_message(&data, &mut handlers).unwrap();

    assert_eq!(handlers.h1.values, vec![10, 20]);
    assert_eq!(handlers.h2.values, vec![b"abc".to_vec(), b"def".to_vec()]);
}

#[test]
fn criterion_27_3_three_handlers_isolation() {
    let mut w = SerializedMessageWriter::new();
    w.write_uint64(1, 7).unwrap();
    w.write_bytes(2, b"x").unwrap();
    w.write_fixed32(3, 42).unwrap();
    let data = w.finish().unwrap();

    let mut handlers = StaticHandlerSet::new(Field1VarintCollector::new())
        .and(Field2BytesCollector::new())
        .and(Field3Fixed32Collector::new());
    read_message(&data, &mut handlers).unwrap();

    assert_eq!(handlers.h1.values, vec![7]);
    assert_eq!(handlers.h2.values, vec![b"x".to_vec()]);
    assert_eq!(handlers.h3.values, vec![42]);
}

// ---------------------------------------------------------------------------
// Criterion 27.4: Handler error stops reading immediately.
// ---------------------------------------------------------------------------

/// A handler that errors on the first invocation.
struct ErroringHandler;

impl FieldHandler for ErroringHandler {
    const FIELD_NUMBER: u32 = 1;

    fn handle_varint(&mut self, _value: u64) -> Result<(), RiegeliError> {
        Err(RiegeliError::MalformedData("handler error".to_string()))
    }
}

/// Counter handler for field 2 to verify it was NOT invoked after error.
struct Field2Counter {
    count: usize,
}

impl Field2Counter {
    fn new() -> Self {
        Self { count: 0 }
    }
}

impl FieldHandler for Field2Counter {
    const FIELD_NUMBER: u32 = 2;

    fn handle_varint(&mut self, _value: u64) -> Result<(), RiegeliError> {
        self.count += 1;
        Ok(())
    }
}

#[test]
fn criterion_27_4_handler_error_stops_immediately() {
    let mut w = SerializedMessageWriter::new();
    w.write_uint64(1, 999).unwrap();
    w.write_uint64(2, 888).unwrap(); // Should never be reached
    let data = w.finish().unwrap();

    let mut handlers = StaticHandlerSet::new(ErroringHandler).and(Field2Counter::new());
    let result = read_message(&data, &mut handlers);

    assert!(result.is_err());
    let err_msg = format!("{}", result.unwrap_err());
    assert!(err_msg.contains("handler error"));
    // Field 2 handler should not have been called
    assert_eq!(handlers.h2.count, 0);
}

#[test]
fn criterion_27_4_error_propagates_to_caller() {
    let mut w = SerializedMessageWriter::new();
    w.write_uint64(1, 1).unwrap();
    let data = w.finish().unwrap();

    let mut handler = StaticHandlerSet::new(ErroringHandler);
    let result = read_message(&data, &mut handler);
    assert!(matches!(result, Err(RiegeliError::MalformedData(_))));
}

// ---------------------------------------------------------------------------
// Criterion 27.5: No matching handlers — all fields skipped without error.
// ---------------------------------------------------------------------------

#[test]
fn criterion_27_5_no_matching_handlers_skips_all() {
    let mut w = SerializedMessageWriter::new();
    w.write_uint64(5, 100).unwrap();
    w.write_bytes(6, b"data").unwrap();
    w.write_fixed64(7, 0xDEADBEEF).unwrap();
    let data = w.finish().unwrap();

    // Register handler for field 1 only — none of {5,6,7} match
    let mut handler = StaticHandlerSet::new(Field1VarintCollector::new());
    read_message(&data, &mut handler).unwrap();
    assert!(handler.h1.values.is_empty());
}

#[test]
fn criterion_27_5_empty_message_no_error() {
    let data = vec![];
    let mut handler = StaticHandlerSet::new(Field1VarintCollector::new());
    read_message(&data, &mut handler).unwrap();
}

// ---------------------------------------------------------------------------
// Criterion 27.6: Repeated fields — handler invoked once per occurrence.
// ---------------------------------------------------------------------------

#[test]
fn criterion_27_6_repeated_fields_invoked_per_occurrence() {
    let mut w = SerializedMessageWriter::new();
    w.write_uint64(1, 10).unwrap();
    w.write_uint64(1, 20).unwrap();
    w.write_uint64(1, 30).unwrap();
    let data = w.finish().unwrap();

    let mut handler = StaticHandlerSet::new(Field1VarintCollector::new());
    read_message(&data, &mut handler).unwrap();

    assert_eq!(handler.h1.values, vec![10, 20, 30]);
}

#[test]
fn criterion_27_6_repeated_interleaved_with_other_fields() {
    let mut w = SerializedMessageWriter::new();
    w.write_uint64(1, 1).unwrap();
    w.write_bytes(2, b"a").unwrap();
    w.write_uint64(1, 2).unwrap();
    w.write_bytes(2, b"b").unwrap();
    w.write_uint64(1, 3).unwrap();
    let data = w.finish().unwrap();

    let mut handlers =
        StaticHandlerSet::new(Field1VarintCollector::new()).and(Field2BytesCollector::new());
    read_message(&data, &mut handlers).unwrap();

    assert_eq!(handlers.h1.values, vec![1, 2, 3]);
    assert_eq!(handlers.h2.values, vec![b"a".to_vec(), b"b".to_vec()]);
}

// ---------------------------------------------------------------------------
// Criterion 27.7: Nested submessage — handler recursively applies reader.
// ---------------------------------------------------------------------------

#[test]
fn criterion_27_7_nested_submessage_recursive_read() {
    // Build outer message with field 1 (varint=99) and field 2 (submessage
    // containing field 1 = varint 42 and field 3 = varint 77).
    let mut w = SerializedMessageWriter::new();
    w.write_uint64(1, 99).unwrap();

    // Nested submessage as field 2
    w.open_length_delimited(2).unwrap();
    w.write_uint64(1, 42).unwrap();
    w.write_uint64(3, 77).unwrap();
    w.close_length_delimited().unwrap();

    let data = w.finish().unwrap();

    // Use RefCell to share mutable state across closures
    let outer_varints = RefCell::new(Vec::new());
    let inner_varints = RefCell::new(Vec::new());

    {
        let mut handlers = DynamicHandlerSet::new();
        let ov = &outer_varints;
        handlers.on_varint(1, move |v| {
            ov.borrow_mut().push(v);
            Ok(())
        });
        let iv = &inner_varints;
        handlers.on_length_delimited(2, move |submsg_bytes: &[u8]| {
            // Recursively read the submessage
            let mut inner_handler = DynamicHandlerSet::new();
            let iv2 = &*iv;
            inner_handler.on_varint(1, |v| {
                iv2.borrow_mut().push(v);
                Ok(())
            });
            read_message(submsg_bytes, &mut inner_handler)
        });
        read_message(&data, &mut handlers).unwrap();
    }

    assert_eq!(*outer_varints.borrow(), vec![99]);
    assert_eq!(*inner_varints.borrow(), vec![42]);
}

#[test]
fn criterion_27_7_nested_submessage_with_static_handlers() {
    // Build message: field 10 (submessage { field 1 = varint 55 })
    let mut w = SerializedMessageWriter::new();
    w.open_length_delimited(10).unwrap();
    w.write_uint64(1, 55).unwrap();
    w.close_length_delimited().unwrap();
    let data = w.finish().unwrap();

    let inner_values = RefCell::new(Vec::new());
    {
        let iv = &inner_values;
        let mut handlers = DynamicHandlerSet::new();
        handlers.on_length_delimited(10, move |submsg: &[u8]| {
            let mut inner = DynamicHandlerSet::new();
            let iv2 = &*iv;
            inner.on_varint(1, |v| {
                iv2.borrow_mut().push(v);
                Ok(())
            });
            read_message(submsg, &mut inner)
        });
        read_message(&data, &mut handlers).unwrap();
    }
    assert_eq!(*inner_values.borrow(), vec![55]);
}

// ---------------------------------------------------------------------------
// Criterion 27.8: Dynamic handlers for fields 1, 3, 5 dispatch correctly;
//                 fields 2, 4, 6 are skipped.
// ---------------------------------------------------------------------------

#[test]
fn criterion_27_8_dynamic_handlers_selective_dispatch() {
    let mut w = SerializedMessageWriter::new();
    w.write_uint64(1, 10).unwrap();
    w.write_uint64(2, 20).unwrap();
    w.write_uint64(3, 30).unwrap();
    w.write_uint64(4, 40).unwrap();
    w.write_uint64(5, 50).unwrap();
    w.write_uint64(6, 60).unwrap();
    let data = w.finish().unwrap();

    let collected = RefCell::new(Vec::new());
    {
        let c = &collected;
        let mut handlers = DynamicHandlerSet::new();
        handlers.on_varint(1, |v| {
            c.borrow_mut().push((1u32, v));
            Ok(())
        });
        handlers.on_varint(3, |v| {
            c.borrow_mut().push((3, v));
            Ok(())
        });
        handlers.on_varint(5, |v| {
            c.borrow_mut().push((5, v));
            Ok(())
        });
        read_message(&data, &mut handlers).unwrap();
    }

    assert_eq!(*collected.borrow(), vec![(1, 10), (3, 30), (5, 50)]);
}

#[test]
fn criterion_27_8_dynamic_no_extra_invocations() {
    // Ensure fields 2, 4, 6 do not trigger any handler.
    let mut w = SerializedMessageWriter::new();
    w.write_uint64(2, 20).unwrap();
    w.write_uint64(4, 40).unwrap();
    w.write_uint64(6, 60).unwrap();
    let data = w.finish().unwrap();

    let call_count = RefCell::new(0usize);
    {
        let cnt = &call_count;
        let mut handlers = DynamicHandlerSet::new();
        handlers.on_varint(1, |_| {
            *cnt.borrow_mut() += 1;
            Ok(())
        });
        handlers.on_varint(3, |_| {
            *cnt.borrow_mut() += 1;
            Ok(())
        });
        handlers.on_varint(5, |_| {
            *cnt.borrow_mut() += 1;
            Ok(())
        });
        read_message(&data, &mut handlers).unwrap();
    }
    assert_eq!(*call_count.borrow(), 0);
}

// ---------------------------------------------------------------------------
// Criterion 27.9: Two dynamic handlers for same field number, different
//                 wire types, dispatch based on wire type.
// ---------------------------------------------------------------------------

#[test]
fn criterion_27_9_same_field_different_wire_types() {
    // Build message: field 1 as varint, then field 1 as fixed32
    let mut w = SerializedMessageWriter::new();
    w.write_uint64(1, 42).unwrap();
    w.write_fixed32(1, 0xBEEF).unwrap();
    let data = w.finish().unwrap();

    let varint_values = RefCell::new(Vec::new());
    let fixed32_values = RefCell::new(Vec::new());
    {
        let vr = &varint_values;
        let fr = &fixed32_values;
        let mut handlers = DynamicHandlerSet::new();
        handlers.on_varint(1, |v| {
            vr.borrow_mut().push(v);
            Ok(())
        });
        handlers.on_fixed32(1, |v| {
            fr.borrow_mut().push(v);
            Ok(())
        });
        read_message(&data, &mut handlers).unwrap();
    }

    assert_eq!(*varint_values.borrow(), vec![42u64]);
    assert_eq!(*fixed32_values.borrow(), vec![0xBEEFu32]);
}

#[test]
fn criterion_27_9_varint_and_length_delimited_same_field() {
    let mut w = SerializedMessageWriter::new();
    w.write_uint64(5, 100).unwrap();
    w.write_bytes(5, b"hello").unwrap();
    let data = w.finish().unwrap();

    let varints = RefCell::new(Vec::new());
    let bytes_vals = RefCell::new(Vec::<Vec<u8>>::new());
    {
        let vr = &varints;
        let br = &bytes_vals;
        let mut handlers = DynamicHandlerSet::new();
        handlers.on_varint(5, |v| {
            vr.borrow_mut().push(v);
            Ok(())
        });
        handlers.on_length_delimited(5, |d| {
            br.borrow_mut().push(d.to_vec());
            Ok(())
        });
        read_message(&data, &mut handlers).unwrap();
    }

    assert_eq!(*varints.borrow(), vec![100u64]);
    assert_eq!(*bytes_vals.borrow(), vec![b"hello".to_vec()]);
}

// ---------------------------------------------------------------------------
// Criterion 27.10: Empty handler set — all fields skipped without error.
// ---------------------------------------------------------------------------

#[test]
fn criterion_27_10_empty_handler_set_skips_all() {
    let mut w = SerializedMessageWriter::new();
    w.write_uint64(1, 1).unwrap();
    w.write_bytes(2, b"data").unwrap();
    w.write_fixed64(3, 999).unwrap();
    let data = w.finish().unwrap();

    let mut handlers = EmptyHandlerSet;
    read_message(&data, &mut handlers).unwrap();
}

#[test]
fn criterion_27_10_empty_dynamic_handler_set() {
    let mut w = SerializedMessageWriter::new();
    w.write_uint64(1, 1).unwrap();
    w.write_bytes(2, b"data").unwrap();
    let data = w.finish().unwrap();

    let mut handlers = DynamicHandlerSet::new();
    read_message(&data, &mut handlers).unwrap();
}

#[test]
fn criterion_27_10_empty_message_empty_handlers() {
    let data = vec![];
    let mut handlers = EmptyHandlerSet;
    read_message(&data, &mut handlers).unwrap();
}

// ---------------------------------------------------------------------------
// Additional: dynamic handler error propagation
// ---------------------------------------------------------------------------

#[test]
fn dynamic_handler_error_stops_reading() {
    let mut w = SerializedMessageWriter::new();
    w.write_uint64(1, 1).unwrap();
    w.write_uint64(2, 2).unwrap();
    let data = w.finish().unwrap();

    let field2_seen = RefCell::new(false);
    {
        let f2 = &field2_seen;
        let mut handlers = DynamicHandlerSet::new();
        handlers.on_varint(1, |_| {
            Err(RiegeliError::MalformedData("dynamic error".to_string()))
        });
        handlers.on_varint(2, |_| {
            *f2.borrow_mut() = true;
            Ok(())
        });
        let result = read_message(&data, &mut handlers);
        assert!(result.is_err());
    }
    assert!(!*field2_seen.borrow());
}
