//! Tests for the serialized-message reader and its field handler dispatch
//! framework: static and dynamic handler sets, skipping, error propagation,
//! and nested submessage reading.

use std::cell::RefCell;

use riegeli::proto::{
    encode_varint32, encode_varint64, make_tag, read_message, DynamicHandlerSet, EmptyHandlerSet,
    FieldHandler, SerializedMessageWriter, StaticHandlerSet, WireType,
};
use riegeli::RiegeliError;

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
// Handler for field 1 varint receives correct value;
//                 fields 2 and 3 are skipped.
// ---------------------------------------------------------------------------

#[test]
fn varint_handler_receives_value_others_skipped() {
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
fn only_matching_field_invoked() {
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
// Handler for length-delimited field receives matching bytes.
// ---------------------------------------------------------------------------

#[test]
fn length_delimited_handler_receives_bytes() {
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
fn string_field_received_as_bytes() {
    let mut w = SerializedMessageWriter::new();
    w.write_string(2, "proto string").unwrap();
    let data = w.finish().unwrap();

    let mut handler = StaticHandlerSet::new(Field2BytesCollector::new());
    read_message(&data, &mut handler).unwrap();

    assert_eq!(handler.h1.values[0], b"proto string");
}

// ---------------------------------------------------------------------------
// Two handlers for different field numbers — no cross-talk.
// ---------------------------------------------------------------------------

#[test]
fn two_handlers_no_crosstalk() {
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
fn three_handlers_isolation() {
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
// Handler error stops reading immediately.
// ---------------------------------------------------------------------------

/// A handler that errors on the first invocation.
struct ErroringHandler;

impl FieldHandler for ErroringHandler {
    const FIELD_NUMBER: u32 = 1;

    fn handle_varint(&mut self, _value: u64) -> Result<(), RiegeliError> {
        Err(RiegeliError::MalformedData("handler error".into()))
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
fn handler_error_stops_immediately() {
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
fn length_delimited_handler_error_stops_processing() {
    // An error raised from a length-delimited handler (rather than a varint
    // handler) propagates out of read_message, and handlers for later fields
    // are never invoked.
    struct ErrorHandler;
    impl FieldHandler for ErrorHandler {
        const FIELD_NUMBER: u32 = 2;
        fn handle_length_delimited(&mut self, _: &[u8]) -> Result<(), RiegeliError> {
            Err(RiegeliError::MalformedData(
                "deliberate handler error".into(),
            ))
        }
    }

    struct CountHandler<'a> {
        count: &'a mut u32,
    }
    impl<'a> FieldHandler for CountHandler<'a> {
        const FIELD_NUMBER: u32 = 3;
        fn handle_varint(&mut self, _: u64) -> Result<(), RiegeliError> {
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

#[test]
fn error_propagates_to_caller() {
    let mut w = SerializedMessageWriter::new();
    w.write_uint64(1, 1).unwrap();
    let data = w.finish().unwrap();

    let mut handler = StaticHandlerSet::new(ErroringHandler);
    let result = read_message(&data, &mut handler);
    assert!(matches!(result, Err(RiegeliError::MalformedData(_))));
}

// ---------------------------------------------------------------------------
// No matching handlers — all fields skipped without error.
// ---------------------------------------------------------------------------

#[test]
fn no_matching_handlers_skips_all() {
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
fn empty_message_no_error() {
    let data = vec![];
    let mut handler = StaticHandlerSet::new(Field1VarintCollector::new());
    read_message(&data, &mut handler).unwrap();
}

// ---------------------------------------------------------------------------
// Repeated fields — handler invoked once per occurrence.
// ---------------------------------------------------------------------------

#[test]
fn repeated_fields_invoked_per_occurrence() {
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
fn repeated_interleaved_with_other_fields() {
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
// Nested submessage — handler recursively applies reader.
// ---------------------------------------------------------------------------

#[test]
fn nested_submessage_recursive_read() {
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
fn nested_submessage_with_static_handlers() {
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
// Dynamic handlers for fields 1, 3, 5 dispatch correctly;
//                 fields 2, 4, 6 are skipped.
// ---------------------------------------------------------------------------

#[test]
fn dynamic_handlers_selective_dispatch() {
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
fn dynamic_no_extra_invocations() {
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
// Two dynamic handlers for same field number, different
//                 wire types, dispatch based on wire type.
// ---------------------------------------------------------------------------

#[test]
fn same_field_different_wire_types() {
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
fn varint_and_length_delimited_same_field() {
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
// Empty handler set — all fields skipped without error.
// ---------------------------------------------------------------------------

#[test]
fn empty_handler_set_skips_all() {
    let mut w = SerializedMessageWriter::new();
    w.write_uint64(1, 1).unwrap();
    w.write_bytes(2, b"data").unwrap();
    w.write_fixed64(3, 999).unwrap();
    let data = w.finish().unwrap();

    let mut handlers = EmptyHandlerSet;
    read_message(&data, &mut handlers).unwrap();
}

#[test]
fn empty_dynamic_handler_set() {
    let mut w = SerializedMessageWriter::new();
    w.write_uint64(1, 1).unwrap();
    w.write_bytes(2, b"data").unwrap();
    let data = w.finish().unwrap();

    let mut handlers = DynamicHandlerSet::new();
    read_message(&data, &mut handlers).unwrap();
}

#[test]
fn empty_message_empty_handlers() {
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
            Err(RiegeliError::MalformedData("dynamic error".into()))
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

#[test]
fn error_on_middle_field_runs_earlier_handlers_only() {
    // A handler error on a middle field propagates, handlers for fields
    // before it have already run, and handlers for fields after it never run.
    let mut w = SerializedMessageWriter::new();
    w.write_uint64(1, 100).unwrap();
    w.write_uint64(2, 200).unwrap();
    w.write_uint64(3, 300).unwrap();
    let data = w.finish().unwrap();

    let seen = RefCell::new(Vec::new());
    {
        let s = &seen;
        let mut handlers = DynamicHandlerSet::new();
        handlers.on_varint(1, |v| {
            s.borrow_mut().push(v);
            Ok(())
        });
        handlers.on_varint(2, |_| {
            Err(RiegeliError::MalformedData("boom on middle".into()))
        });
        handlers.on_varint(3, |v| {
            s.borrow_mut().push(v);
            Ok(())
        });
        let result = read_message(&data, &mut handlers);
        assert!(result.is_err());
    }
    // Only field 1 should have been seen.
    assert_eq!(*seen.borrow(), vec![100]);
}

// ---------------------------------------------------------------------------
// Global invocation order across handlers
// ---------------------------------------------------------------------------

#[test]
fn handlers_invoked_in_wire_order() {
    // Handlers for different fields are invoked in the exact order the
    // fields appear on the wire, interleaved across handlers.
    let mut w = SerializedMessageWriter::new();
    w.write_uint64(1, 10).unwrap();
    w.write_bytes(2, b"alpha").unwrap();
    w.write_uint64(1, 20).unwrap();
    w.write_bytes(2, b"beta").unwrap();
    w.write_uint64(1, 30).unwrap();
    let data = w.finish().unwrap();

    // Track the global order of all handler invocations.
    let order = RefCell::new(Vec::<String>::new());
    {
        let o = &order;
        let mut handlers = DynamicHandlerSet::new();
        handlers.on_varint(1, move |v| {
            o.borrow_mut().push(format!("varint1:{}", v));
            Ok(())
        });
        let o2 = &order;
        handlers.on_length_delimited(2, move |d| {
            o2.borrow_mut()
                .push(format!("bytes2:{}", String::from_utf8_lossy(d)));
            Ok(())
        });
        read_message(&data, &mut handlers).unwrap();
    }

    assert_eq!(
        *order.borrow(),
        vec![
            "varint1:10".to_string(),
            "bytes2:alpha".to_string(),
            "varint1:20".to_string(),
            "bytes2:beta".to_string(),
            "varint1:30".to_string(),
        ]
    );
}

// ---------------------------------------------------------------------------
// Empty message with handlers registered
// ---------------------------------------------------------------------------

#[test]
fn empty_message_invokes_no_handlers() {
    let data: Vec<u8> = vec![];
    let called = RefCell::new(false);
    {
        let c = &called;
        let mut handlers = DynamicHandlerSet::new();
        handlers.on_varint(1, |_| {
            *c.borrow_mut() = true;
            Ok(())
        });
        handlers.on_length_delimited(2, |_| {
            *c.borrow_mut() = true;
            Ok(())
        });
        read_message(&data, &mut handlers).unwrap();
    }
    assert!(
        !*called.borrow(),
        "No handlers should be invoked on empty message"
    );
}

// ---------------------------------------------------------------------------
// StartGroup/EndGroup handler dispatch
// ---------------------------------------------------------------------------

#[test]
fn start_and_end_group_handlers_dispatched() {
    // Manually construct a message with StartGroup field 1, varint field 2
    // inside, EndGroup field 1.
    let mut data = Vec::new();
    // StartGroup for field 1: tag = (1 << 3) | 3 = 0x0B
    encode_varint32(&mut data, make_tag(1, WireType::StartGroup));
    // Varint field 2 = 42 inside the group
    encode_varint32(&mut data, make_tag(2, WireType::Varint));
    encode_varint64(&mut data, 42);
    // EndGroup for field 1: tag = (1 << 3) | 4 = 0x0C
    encode_varint32(&mut data, make_tag(1, WireType::EndGroup));

    let events = RefCell::new(Vec::<String>::new());
    {
        let e = &events;
        let mut handlers = DynamicHandlerSet::new();
        handlers.on_start_group(1, || {
            e.borrow_mut().push("start_group:1".into());
            Ok(())
        });
        let e2 = &events;
        handlers.on_varint(2, move |v| {
            e2.borrow_mut().push(format!("varint2:{}", v));
            Ok(())
        });
        let e3 = &events;
        handlers.on_end_group(1, move || {
            e3.borrow_mut().push("end_group:1".into());
            Ok(())
        });
        read_message(&data, &mut handlers).unwrap();
    }

    assert_eq!(
        *events.borrow(),
        vec![
            "start_group:1".to_string(),
            "varint2:42".to_string(),
            "end_group:1".to_string(),
        ]
    );
}

// ---------------------------------------------------------------------------
// Maximum field number dispatch
// ---------------------------------------------------------------------------

#[test]
fn max_field_number_round_trip_dispatch() {
    // Proto allows field numbers up to 2^29 - 1 = 536870911; handler dispatch
    // keyed on the maximum field number must still match.
    let large_fn = 536870911u32;
    let mut w = SerializedMessageWriter::new();
    w.write_uint64(large_fn, 9999).unwrap();
    let data = w.finish().unwrap();

    let collected = RefCell::new(Vec::new());
    {
        let c = &collected;
        let mut handlers = DynamicHandlerSet::new();
        handlers.on_varint(large_fn, move |v| {
            c.borrow_mut().push(v);
            Ok(())
        });
        read_message(&data, &mut handlers).unwrap();
    }
    assert_eq!(*collected.borrow(), vec![9999]);
}

// ---------------------------------------------------------------------------
// Empty handler set skips every wire type
// ---------------------------------------------------------------------------

#[test]
fn empty_handler_set_skips_all_wire_types() {
    // A message exercising every wire type — varint, fixed32, fixed64,
    // length-delimited, and group tags — with extreme values is fully
    // skipped without error when no handlers are registered.
    let mut w = SerializedMessageWriter::new();
    w.write_uint64(1, u64::MAX).unwrap();
    w.write_fixed32(2, u32::MAX).unwrap();
    w.write_fixed64(3, u64::MAX).unwrap();
    w.write_bytes(4, &[0u8; 1000]).unwrap();
    let data_with_scalars = w.finish().unwrap();

    // Add group fields manually
    let mut data = data_with_scalars;
    encode_varint32(&mut data, make_tag(5, WireType::StartGroup));
    encode_varint32(&mut data, make_tag(5, WireType::EndGroup));

    let mut handlers = EmptyHandlerSet;
    read_message(&data, &mut handlers).unwrap();
}
