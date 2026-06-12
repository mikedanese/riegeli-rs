//! Field handler framework for proto message reading.
//!
//! Provides both static (compile-time field number) and dynamic (runtime
//! `Box<dyn FnMut(...)>`) handler dispatch over serialized proto messages.

use std::collections::HashMap;

use super::field_iter::{FieldValue, ProtoField, ProtoFieldIter};
use super::wire::WireType;

/// Trait for dispatching proto fields during message reading.
///
/// Implementors receive decoded fields and decide whether to handle them.
/// The `dispatch` method returns:
/// - `Ok(true)` if the field was handled,
/// - `Ok(false)` if the field should be skipped (no matching handler),
/// - `Err(...)` to abort reading immediately.
///
/// Both static (compile-time) and dynamic (runtime) handler sets implement
/// this trait, so `read_message` works uniformly with either.
pub trait HandleField {
    /// Dispatch a decoded field to the appropriate handler.
    ///
    /// Returns `Ok(true)` if handled, `Ok(false)` if no handler matched
    /// (the reader will skip the field), or `Err` to abort.
    fn dispatch(&mut self, field: &ProtoField<'_>) -> Result<bool, crate::RiegeliError>;
}

/// A handler for a specific proto field number, using the `FieldHandler` trait
/// for static (compile-time) dispatch.
///
/// Implement this trait to handle fields with a known field number. Override
/// only the wire-type methods you care about; unimplemented methods default
/// to `Ok(())` (skip).
pub trait FieldHandler {
    /// The proto field number this handler is registered for.
    const FIELD_NUMBER: u32;

    /// Called when a varint field is encountered.
    fn handle_varint(&mut self, _value: u64) -> Result<(), crate::RiegeliError> {
        Ok(())
    }

    /// Called when a fixed32 field is encountered.
    fn handle_fixed32(&mut self, _value: u32) -> Result<(), crate::RiegeliError> {
        Ok(())
    }

    /// Called when a fixed64 field is encountered.
    fn handle_fixed64(&mut self, _value: u64) -> Result<(), crate::RiegeliError> {
        Ok(())
    }

    /// Called when a length-delimited field is encountered.
    fn handle_length_delimited(&mut self, _data: &[u8]) -> Result<(), crate::RiegeliError> {
        Ok(())
    }

    /// Called when a start-group tag is encountered.
    fn handle_start_group(&mut self) -> Result<(), crate::RiegeliError> {
        Ok(())
    }

    /// Called when an end-group tag is encountered.
    fn handle_end_group(&mut self) -> Result<(), crate::RiegeliError> {
        Ok(())
    }
}

/// Dispatches a `ProtoField` to a `FieldHandler` if the field number matches.
///
/// Returns `Ok(true)` if the field was handled, `Ok(false)` if the field number
/// didn't match.
fn dispatch_to_handler<H: FieldHandler>(
    handler: &mut H,
    field: &ProtoField<'_>,
) -> Result<bool, crate::RiegeliError> {
    if field.field_number != H::FIELD_NUMBER {
        return Ok(false);
    }
    match &field.value {
        FieldValue::Varint(v) => handler.handle_varint(*v)?,
        FieldValue::Fixed32(v) => handler.handle_fixed32(*v)?,
        FieldValue::Fixed64(v) => handler.handle_fixed64(*v)?,
        FieldValue::LengthDelimited(data) => handler.handle_length_delimited(data)?,
        FieldValue::StartGroup => handler.handle_start_group()?,
        FieldValue::EndGroup => handler.handle_end_group()?,
    }
    Ok(true)
}

/// A static handler set holding one handler.
///
/// Use `StaticHandlerSet::new(handler)` and chain `.and(handler2)` to build
/// a set of up to any number of handlers. Each handler is dispatched by its
/// compile-time `FIELD_NUMBER`.
pub struct StaticHandlerSet1<H1: FieldHandler> {
    /// The handler.
    pub h1: H1,
}

impl<H1: FieldHandler> HandleField for StaticHandlerSet1<H1> {
    fn dispatch(&mut self, field: &ProtoField<'_>) -> Result<bool, crate::RiegeliError> {
        dispatch_to_handler(&mut self.h1, field)
    }
}

/// A static handler set holding two handlers.
pub struct StaticHandlerSet2<H1: FieldHandler, H2: FieldHandler> {
    /// The first handler.
    pub h1: H1,
    /// The second handler.
    pub h2: H2,
}

impl<H1: FieldHandler, H2: FieldHandler> HandleField for StaticHandlerSet2<H1, H2> {
    fn dispatch(&mut self, field: &ProtoField<'_>) -> Result<bool, crate::RiegeliError> {
        if dispatch_to_handler(&mut self.h1, field)? {
            return Ok(true);
        }
        dispatch_to_handler(&mut self.h2, field)
    }
}

/// A static handler set holding three handlers.
pub struct StaticHandlerSet3<H1: FieldHandler, H2: FieldHandler, H3: FieldHandler> {
    /// The first handler.
    pub h1: H1,
    /// The second handler.
    pub h2: H2,
    /// The third handler.
    pub h3: H3,
}

impl<H1: FieldHandler, H2: FieldHandler, H3: FieldHandler> HandleField
    for StaticHandlerSet3<H1, H2, H3>
{
    fn dispatch(&mut self, field: &ProtoField<'_>) -> Result<bool, crate::RiegeliError> {
        if dispatch_to_handler(&mut self.h1, field)? {
            return Ok(true);
        }
        if dispatch_to_handler(&mut self.h2, field)? {
            return Ok(true);
        }
        dispatch_to_handler(&mut self.h3, field)
    }
}

/// Builder for creating static handler sets.
pub struct StaticHandlerSet;

impl StaticHandlerSet {
    /// Creates a handler set with one handler.
    #[allow(clippy::new_ret_no_self)] // entry point of the typed builder chain
    pub fn new<H1: FieldHandler>(h1: H1) -> StaticHandlerSet1<H1> {
        StaticHandlerSet1 { h1 }
    }
}

impl<H1: FieldHandler> StaticHandlerSet1<H1> {
    /// Adds a second handler to the set.
    pub fn and<H2: FieldHandler>(self, h2: H2) -> StaticHandlerSet2<H1, H2> {
        StaticHandlerSet2 { h1: self.h1, h2 }
    }
}

impl<H1: FieldHandler, H2: FieldHandler> StaticHandlerSet2<H1, H2> {
    /// Adds a third handler to the set.
    pub fn and<H3: FieldHandler>(self, h3: H3) -> StaticHandlerSet3<H1, H2, H3> {
        StaticHandlerSet3 {
            h1: self.h1,
            h2: self.h2,
            h3,
        }
    }
}

// ---------------------------------------------------------------------------
// Dynamic handler dispatch
// ---------------------------------------------------------------------------

/// A key for dynamic handler dispatch: `(field_number, wire_type_raw)`.
///
/// The raw wire type value (0..=5) is used rather than `WireType` to avoid
/// needing `Hash`/`Eq` on the enum.
type DynamicHandlerKey = (u32, u8);

/// A dynamic handler set that maps `(field_number, wire_type)` pairs to
/// boxed closures at runtime.
///
/// Each closure receives the decoded `FieldValue` and returns
/// `Result<(), RiegeliError>`. Register handlers with `on_varint`,
/// `on_fixed32`, etc., then pass to `read_message`.
///
/// # Example
///
/// ```
/// use riegeli::proto::{DynamicHandlerSet, read_message, SerializedMessageWriter};
///
/// let mut w = SerializedMessageWriter::new();
/// w.write_uint64(1, 42).unwrap();
/// let data = w.finish().unwrap();
///
/// let mut result = 0u64;
/// {
///     let mut handlers = DynamicHandlerSet::new();
///     handlers.on_varint(1, |v| { result = v; Ok(()) });
///     read_message(&data, &mut handlers).unwrap();
/// }
/// assert_eq!(result, 42);
/// ```
pub struct DynamicHandlerSet<'a> {
    handlers: HashMap<
        DynamicHandlerKey,
        Box<dyn FnMut(&FieldValue<'_>) -> Result<(), crate::RiegeliError> + 'a>,
    >,
}

impl<'a> DynamicHandlerSet<'a> {
    /// Creates an empty dynamic handler set.
    pub fn new() -> Self {
        DynamicHandlerSet {
            handlers: HashMap::new(),
        }
    }

    /// Registers a handler for varint fields with the given field number.
    pub fn on_varint<F>(&mut self, field_number: u32, mut f: F)
    where
        F: FnMut(u64) -> Result<(), crate::RiegeliError> + 'a,
    {
        self.handlers.insert(
            (field_number, WireType::Varint as u8),
            Box::new(move |value| {
                if let FieldValue::Varint(v) = value {
                    f(*v)
                } else {
                    Ok(())
                }
            }),
        );
    }

    /// Registers a handler for fixed32 fields with the given field number.
    pub fn on_fixed32<F>(&mut self, field_number: u32, mut f: F)
    where
        F: FnMut(u32) -> Result<(), crate::RiegeliError> + 'a,
    {
        self.handlers.insert(
            (field_number, WireType::Fixed32 as u8),
            Box::new(move |value| {
                if let FieldValue::Fixed32(v) = value {
                    f(*v)
                } else {
                    Ok(())
                }
            }),
        );
    }

    /// Registers a handler for fixed64 fields with the given field number.
    pub fn on_fixed64<F>(&mut self, field_number: u32, mut f: F)
    where
        F: FnMut(u64) -> Result<(), crate::RiegeliError> + 'a,
    {
        self.handlers.insert(
            (field_number, WireType::Fixed64 as u8),
            Box::new(move |value| {
                if let FieldValue::Fixed64(v) = value {
                    f(*v)
                } else {
                    Ok(())
                }
            }),
        );
    }

    /// Registers a handler for length-delimited fields with the given field number.
    pub fn on_length_delimited<F>(&mut self, field_number: u32, mut f: F)
    where
        F: FnMut(&[u8]) -> Result<(), crate::RiegeliError> + 'a,
    {
        self.handlers.insert(
            (field_number, WireType::LengthDelimited as u8),
            Box::new(move |value| {
                if let FieldValue::LengthDelimited(data) = value {
                    f(data)
                } else {
                    Ok(())
                }
            }),
        );
    }

    /// Registers a handler for start-group fields with the given field number.
    pub fn on_start_group<F>(&mut self, field_number: u32, mut f: F)
    where
        F: FnMut() -> Result<(), crate::RiegeliError> + 'a,
    {
        self.handlers.insert(
            (field_number, WireType::StartGroup as u8),
            Box::new(move |value| {
                if let FieldValue::StartGroup = value {
                    f()
                } else {
                    Ok(())
                }
            }),
        );
    }

    /// Registers a handler for end-group fields with the given field number.
    pub fn on_end_group<F>(&mut self, field_number: u32, mut f: F)
    where
        F: FnMut() -> Result<(), crate::RiegeliError> + 'a,
    {
        self.handlers.insert(
            (field_number, WireType::EndGroup as u8),
            Box::new(move |value| {
                if let FieldValue::EndGroup = value {
                    f()
                } else {
                    Ok(())
                }
            }),
        );
    }
}

impl<'a> Default for DynamicHandlerSet<'a> {
    fn default() -> Self {
        Self::new()
    }
}

impl<'a> HandleField for DynamicHandlerSet<'a> {
    fn dispatch(&mut self, field: &ProtoField<'_>) -> Result<bool, crate::RiegeliError> {
        let wire_type_raw = field.wire_type as u8;
        let key = (field.field_number, wire_type_raw);
        if let Some(handler) = self.handlers.get_mut(&key) {
            handler(&field.value)?;
            Ok(true)
        } else {
            Ok(false)
        }
    }
}

/// An empty handler set that skips all fields.
///
/// This is useful as a no-op reader that validates message structure without
/// processing any fields.
pub struct EmptyHandlerSet;

impl HandleField for EmptyHandlerSet {
    fn dispatch(&mut self, _field: &ProtoField<'_>) -> Result<bool, crate::RiegeliError> {
        Ok(false)
    }
}

/// Reads a serialized proto message, dispatching each field to the given
/// handler set.
///
/// Fields not matched by any handler are silently skipped. If a handler
/// returns an error, reading stops immediately and the error is propagated.
///
/// Uses `ProtoFieldIter` internally, so all canonical varint and wire format
/// validation applies.
pub fn read_message<H: HandleField>(
    data: &[u8],
    handlers: &mut H,
) -> Result<(), crate::RiegeliError> {
    let iter = ProtoFieldIter::new(data);
    for result in iter {
        let field = result?;
        handlers.dispatch(&field)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto::SerializedMessageWriter;

    /// A handler for field 1 that only overrides `handle_varint`.
    struct VarintOnlyField1 {
        values: Vec<u64>,
    }

    impl FieldHandler for VarintOnlyField1 {
        const FIELD_NUMBER: u32 = 1;
        fn handle_varint(&mut self, value: u64) -> Result<(), crate::RiegeliError> {
            self.values.push(value);
            Ok(())
        }
    }

    #[test]
    fn default_handler_methods_ignore_other_wire_types() {
        // Field 1 is fixed32, but the handler only overrides handle_varint.
        // The default handle_fixed32 returns Ok(()), so the field counts as
        // handled (dispatched) while the varint collector stays empty.
        let mut w = SerializedMessageWriter::new();
        w.write_fixed32(1, 0xABCD).unwrap();
        let data = w.finish().unwrap();

        let mut handler = StaticHandlerSet::new(VarintOnlyField1 { values: vec![] });
        read_message(&data, &mut handler).unwrap();

        // The handler should not have collected any varint values.
        assert!(handler.h1.values.is_empty());
    }

    #[test]
    fn reregistering_dynamic_handler_replaces_previous() {
        // Registering a second handler for the same (field number, wire type)
        // key replaces the first.
        let mut w = SerializedMessageWriter::new();
        w.write_uint64(1, 42).unwrap();
        let data = w.finish().unwrap();

        let result = std::cell::RefCell::new(0u64);
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
}
