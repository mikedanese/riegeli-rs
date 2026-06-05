//! Adversarial tests for Sprint 27: SerializedMessageReader with Field Handlers.
//!
//! Written by the Evaluator to probe edge cases the Generator's tests may miss.

// Some imports are used only by feature-gated tests; in reduced-feature
// builds they would otherwise trip unused_imports.
#![cfg_attr(
    not(all(feature = "brotli", feature = "zstd", feature = "snappy")),
    allow(unused_imports)
)]
use std::cell::RefCell;
use std::sync::atomic::{AtomicUsize, Ordering};

use riegeli::RiegeliError;
use riegeli::proto::{
    DynamicHandlerSet, EmptyHandlerSet, FieldHandler, HandleField, ProtoField,
    SerializedMessageWriter, StaticHandlerSet, WireType, make_tag, read_message,
};

// ---------------------------------------------------------------------------
// 1. Error propagation: error on the very first field
// ---------------------------------------------------------------------------

#[test]
fn adv_error_on_first_field() {
    let mut w = SerializedMessageWriter::new();
    w.write_uint64(1, 1).unwrap();
    w.write_uint64(2, 2).unwrap();
    w.write_uint64(3, 3).unwrap();
    let data = w.finish().unwrap();

    let invocations = RefCell::new(Vec::new());
    {
        let inv = &invocations;
        let mut handlers = DynamicHandlerSet::new();
        handlers.on_varint(1, |_| {
            Err(RiegeliError::MalformedData("boom on first".into()))
        });
        handlers.on_varint(2, |v| {
            inv.borrow_mut().push(v);
            Ok(())
        });
        handlers.on_varint(3, |v| {
            inv.borrow_mut().push(v);
            Ok(())
        });
        let result = read_message(&data, &mut handlers);
        assert!(result.is_err());
    }
    // No subsequent handlers should have been invoked.
    assert!(invocations.borrow().is_empty());
}

// ---------------------------------------------------------------------------
// 2. Error propagation: error on the last field
// ---------------------------------------------------------------------------

#[test]
fn adv_error_on_last_field() {
    let mut w = SerializedMessageWriter::new();
    w.write_uint64(1, 10).unwrap();
    w.write_uint64(2, 20).unwrap();
    w.write_uint64(3, 30).unwrap();
    let data = w.finish().unwrap();

    let seen = RefCell::new(Vec::new());
    {
        let s = &seen;
        let mut handlers = DynamicHandlerSet::new();
        handlers.on_varint(1, |v| {
            s.borrow_mut().push((1u32, v));
            Ok(())
        });
        handlers.on_varint(2, |v| {
            s.borrow_mut().push((2, v));
            Ok(())
        });
        handlers.on_varint(3, |_| {
            Err(RiegeliError::MalformedData("boom on last".into()))
        });
        let result = read_message(&data, &mut handlers);
        assert!(result.is_err());
    }
    // Fields 1 and 2 should have been seen before the error on field 3.
    assert_eq!(*seen.borrow(), vec![(1, 10), (2, 20)]);
}

// ---------------------------------------------------------------------------
// 3. Error propagation: error on a middle field
// ---------------------------------------------------------------------------

#[test]
fn adv_error_on_middle_field() {
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
// 4. Dynamic handler with 10+ fields
// ---------------------------------------------------------------------------

#[test]
fn adv_dynamic_many_fields() {
    let mut w = SerializedMessageWriter::new();
    for i in 1..=15u32 {
        w.write_uint64(i, i as u64 * 100).unwrap();
    }
    let data = w.finish().unwrap();

    let collected = RefCell::new(Vec::new());
    {
        let c = &collected;
        let mut handlers = DynamicHandlerSet::new();
        // Register handlers for all 15 fields.
        for field_num in 1..=15u32 {
            let c2 = &*c;
            handlers.on_varint(field_num, move |v| {
                c2.borrow_mut().push((field_num, v));
                Ok(())
            });
        }
        read_message(&data, &mut handlers).unwrap();
    }

    let result = collected.borrow().clone();
    assert_eq!(result.len(), 15);
    for i in 1..=15u32 {
        assert_eq!(result[(i - 1) as usize], (i, i as u64 * 100));
    }
}

// ---------------------------------------------------------------------------
// 5. Deeply nested recursive submessage parsing (3 levels)
// ---------------------------------------------------------------------------

#[test]
fn adv_three_level_nested_submessage() {
    // Outer: field 1 = varint 1, field 2 = submessage {
    //   field 1 = varint 2, field 2 = submessage {
    //     field 1 = varint 3
    //   }
    // }
    let mut w = SerializedMessageWriter::new();
    w.write_uint64(1, 1).unwrap();
    w.open_length_delimited(2).unwrap();
    {
        w.write_uint64(1, 2).unwrap();
        w.open_length_delimited(2).unwrap();
        {
            w.write_uint64(1, 3).unwrap();
        }
        w.close_length_delimited().unwrap();
    }
    w.close_length_delimited().unwrap();
    let data = w.finish().unwrap();

    let all_varints = RefCell::new(Vec::new());

    fn read_recursive(
        data: &[u8],
        depth: u32,
        collector: &RefCell<Vec<(u32, u64)>>,
    ) -> Result<(), RiegeliError> {
        let mut handlers = DynamicHandlerSet::new();
        let c = collector;
        let d = depth;
        handlers.on_varint(1, move |v| {
            c.borrow_mut().push((d, v));
            Ok(())
        });
        let c2 = collector;
        handlers.on_length_delimited(2, move |submsg: &[u8]| read_recursive(submsg, d + 1, c2));
        read_message(data, &mut handlers)
    }

    read_recursive(&data, 0, &all_varints).unwrap();

    let result = all_varints.borrow().clone();
    // depth 0 -> varint 1, depth 1 -> varint 2, depth 2 -> varint 3
    assert_eq!(result, vec![(0, 1), (1, 2), (2, 3)]);
}

// ---------------------------------------------------------------------------
// 6. Handler that verifies invocation order via mutation
// ---------------------------------------------------------------------------

#[test]
fn adv_invocation_order_verified() {
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
// 7. Empty message with handlers registered
// ---------------------------------------------------------------------------

#[test]
fn adv_empty_message_with_handlers() {
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
// 8. Message with only unhandled fields
// ---------------------------------------------------------------------------

#[test]
fn adv_all_fields_unhandled() {
    let mut w = SerializedMessageWriter::new();
    w.write_uint64(10, 100).unwrap();
    w.write_bytes(20, b"unhandled").unwrap();
    w.write_fixed32(30, 0xDEAD).unwrap();
    w.write_fixed64(40, 0xBEEF).unwrap();
    let data = w.finish().unwrap();

    // Register handlers for completely different fields.
    let called = RefCell::new(false);
    {
        let c = &called;
        let mut handlers = DynamicHandlerSet::new();
        handlers.on_varint(1, |_| {
            *c.borrow_mut() = true;
            Ok(())
        });
        handlers.on_varint(2, |_| {
            *c.borrow_mut() = true;
            Ok(())
        });
        read_message(&data, &mut handlers).unwrap();
    }
    assert!(!*called.borrow());
}

// ---------------------------------------------------------------------------
// 9. Handler for StartGroup/EndGroup wire types
// ---------------------------------------------------------------------------

#[test]
fn adv_group_handlers() {
    // Manually construct a message with StartGroup field 1, varint field 2 inside, EndGroup field 1.
    use riegeli::proto::encode_varint32;

    let mut data = Vec::new();
    // StartGroup for field 1: tag = (1 << 3) | 3 = 0x0B
    encode_varint32(&mut data, make_tag(1, WireType::StartGroup));
    // Varint field 2 = 42 inside the group
    encode_varint32(&mut data, make_tag(2, WireType::Varint));
    riegeli::proto::encode_varint64(&mut data, 42);
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
// 10. Very large field numbers
// ---------------------------------------------------------------------------

#[test]
fn adv_large_field_number() {
    // Proto allows field numbers up to 2^29 - 1 = 536870911.
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
// 11. Static handler with wire type mismatch (varint handler, fixed32 field)
// ---------------------------------------------------------------------------

struct VarintOnlyField1 {
    values: Vec<u64>,
}

impl FieldHandler for VarintOnlyField1 {
    const FIELD_NUMBER: u32 = 1;
    fn handle_varint(&mut self, value: u64) -> Result<(), RiegeliError> {
        self.values.push(value);
        Ok(())
    }
}

#[test]
fn adv_static_handler_wire_type_mismatch_still_dispatches() {
    // Field 1 is fixed32, but handler only overrides handle_varint.
    // The default handle_fixed32 returns Ok(()), so the field is "handled" (dispatched)
    // but the varint collector should be empty.
    let mut w = SerializedMessageWriter::new();
    w.write_fixed32(1, 0xABCD).unwrap();
    let data = w.finish().unwrap();

    let mut handler = StaticHandlerSet::new(VarintOnlyField1 { values: vec![] });
    read_message(&data, &mut handler).unwrap();

    // The handler should not have collected any varint values
    assert!(handler.h1.values.is_empty());
}

// ---------------------------------------------------------------------------
// 12. Dynamic handler set: re-registering same key replaces previous handler
// ---------------------------------------------------------------------------

#[test]
fn adv_dynamic_handler_replacement() {
    let mut w = SerializedMessageWriter::new();
    w.write_uint64(1, 42).unwrap();
    let data = w.finish().unwrap();

    let result = RefCell::new(0u64);
    {
        let r = &result;
        let mut handlers = DynamicHandlerSet::new();
        // Register first handler
        handlers.on_varint(1, |_| {
            panic!("first handler should have been replaced");
        });
        // Replace with second handler
        handlers.on_varint(1, move |v| {
            *r.borrow_mut() = v;
            Ok(())
        });
        read_message(&data, &mut handlers).unwrap();
    }
    assert_eq!(*result.borrow(), 42);
}

// ---------------------------------------------------------------------------
// 13. EmptyHandlerSet with a complex message (all wire types)
// ---------------------------------------------------------------------------

#[test]
fn adv_empty_handler_set_complex_message() {
    use riegeli::proto::encode_varint32;

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

// ---------------------------------------------------------------------------
// 14. Multiple repeated fields interleaved, verify each handler's count
// ---------------------------------------------------------------------------

#[test]
fn adv_multiple_repeated_fields_counts() {
    let mut w = SerializedMessageWriter::new();
    // 5 occurrences of field 1, 3 of field 2, 7 of field 3
    for _ in 0..5 {
        w.write_uint64(1, 1).unwrap();
    }
    for _ in 0..3 {
        w.write_bytes(2, b"x").unwrap();
    }
    for _ in 0..7 {
        w.write_fixed32(3, 99).unwrap();
    }
    let data = w.finish().unwrap();

    let f1_count = RefCell::new(0usize);
    let f2_count = RefCell::new(0usize);
    let f3_count = RefCell::new(0usize);
    {
        let c1 = &f1_count;
        let c2 = &f2_count;
        let c3 = &f3_count;
        let mut handlers = DynamicHandlerSet::new();
        handlers.on_varint(1, move |_| {
            *c1.borrow_mut() += 1;
            Ok(())
        });
        handlers.on_length_delimited(2, move |_| {
            *c2.borrow_mut() += 1;
            Ok(())
        });
        handlers.on_fixed32(3, move |_| {
            *c3.borrow_mut() += 1;
            Ok(())
        });
        read_message(&data, &mut handlers).unwrap();
    }
    assert_eq!(*f1_count.borrow(), 5);
    assert_eq!(*f2_count.borrow(), 3);
    assert_eq!(*f3_count.borrow(), 7);
}

// ---------------------------------------------------------------------------
// 15. Dynamic handler error in recursive submessage propagates to outer reader
// ---------------------------------------------------------------------------

#[test]
fn adv_recursive_submessage_error_propagates() {
    let mut w = SerializedMessageWriter::new();
    w.open_length_delimited(1).unwrap();
    w.write_uint64(1, 42).unwrap();
    w.close_length_delimited().unwrap();
    let data = w.finish().unwrap();

    let mut handlers = DynamicHandlerSet::new();
    handlers.on_length_delimited(1, |submsg: &[u8]| {
        let mut inner = DynamicHandlerSet::new();
        inner.on_varint(1, |_| {
            Err(RiegeliError::MalformedData("inner error".into()))
        });
        read_message(submsg, &mut inner)
    });
    let result = read_message(&data, &mut handlers);
    assert!(result.is_err());
    let err_msg = format!("{}", result.unwrap_err());
    assert!(err_msg.contains("inner error"));
}
