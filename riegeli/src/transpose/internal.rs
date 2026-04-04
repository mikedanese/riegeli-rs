//! Internal data model types for transpose chunk encoding/decoding.
//!
//! Defines `MessageId` reserved constants, `Subtype` constants, the
//! `SUBMESSAGE_WIRE_TYPE` sentinel, and predicates `has_subtype` and
//! `has_data_buffer` that mirror the C++ `transpose_internal.h`.

use crate::proto::{WireType, tag_wire_type};

// ---------------------------------------------------------------------------
// MessageId
// ---------------------------------------------------------------------------

/// Reserved message IDs used in the transpose state machine.
///
/// Values 0..=4 are reserved; IDs >= `ROOT + 1` are assigned sequentially
/// to `NodeId` entries.
pub mod message_id {
    /// No operation — follow `next_node` only.
    pub const NO_OP: u32 = 0;
    /// Non-proto record — read from nonproto buffer + lengths buffer.
    pub const NON_PROTO: u32 = 1;
    /// Start of a submessage — push submessage frame.
    pub const START_OF_SUBMESSAGE: u32 = 2;
    /// Start of a new record.
    pub const START_OF_MESSAGE: u32 = 3;
    /// Root node (in-memory only; never encoded).
    pub const ROOT: u32 = 4;
}

// ---------------------------------------------------------------------------
// Subtype
// ---------------------------------------------------------------------------

/// Subtype constants for transpose state machine nodes.
///
/// The meaning of a subtype depends on the wire type of the tag in the
/// corresponding state. For `WireType::Varint`, subtypes encode either the
/// varint byte-length (buffered) or the value itself (inline). For
/// `WireType::LengthDelimited`, subtypes distinguish strings from
/// submessage start/end markers.
pub mod subtype {
    /// Trivial subtype (no additional semantics).
    pub const TRIVIAL: u8 = 0;

    // -- Subtypes of WireType::Varint --

    /// Varint of length 1, stored in data buffer.
    pub const VARINT_1: u8 = 0;
    /// Varint of length 10, stored in data buffer.
    pub const VARINT_10: u8 = 9;

    /// Maximum buffered varint subtype (VARINT_10).
    pub const VARINT_MAX: u8 = VARINT_10;

    /// Inline varint value 0 (stored in subtype, not in data buffer).
    pub const VARINT_INLINE_0: u8 = VARINT_MAX + 1; // 10

    /// Maximum inline varint value (127), giving subtype 10 + 127 = 137.
    pub const VARINT_INLINE_MAX: u8 = VARINT_INLINE_0 + 0x7F; // 137

    // -- Subtypes of WireType::LengthDelimited --

    /// Plain string / bytes field.
    pub const LENGTH_DELIMITED_STRING: u8 = 0;
    /// Start of a submessage (length-delimited encoding).
    pub const LENGTH_DELIMITED_START_OF_SUBMESSAGE: u8 = 1;
    /// End of a submessage (used with `SUBMESSAGE_WIRE_TYPE`).
    pub const LENGTH_DELIMITED_END_OF_SUBMESSAGE: u8 = 2;
}

// ---------------------------------------------------------------------------
// SUBMESSAGE_WIRE_TYPE
// ---------------------------------------------------------------------------

/// Synthetic wire type value (6) used in state tags to mark submessage-end
/// nodes, distinguishing them from plain `WireType::LengthDelimited` string
/// nodes.
pub const SUBMESSAGE_WIRE_TYPE: u32 = 6;

// ---------------------------------------------------------------------------
// Predicates
// ---------------------------------------------------------------------------

/// Returns `true` if a state with the given `tag` is followed by a subtype
/// byte in the transpose header.
///
/// Only `WireType::Varint` tags have subtypes. Although
/// `WireType::LengthDelimited` nodes use subtypes internally, submessage
/// start is encoded via `MessageId::StartOfSubmessage` and submessage end
/// uses `SUBMESSAGE_WIRE_TYPE`, so the subtype is determined structurally
/// rather than read from the stream.
pub fn has_subtype(tag: u32) -> bool {
    matches!(tag_wire_type(tag), Some(WireType::Varint))
}

/// Returns `true` if a state with the given `tag` and `subtype` reads from
/// a data buffer.
pub fn has_data_buffer(tag: u32, subtype: u8) -> bool {
    match tag_wire_type(tag) {
        Some(WireType::Varint) => {
            // Buffered varints (subtype < VARINT_INLINE_0) read from a buffer.
            // Inline varints (subtype >= VARINT_INLINE_0) store the value in the
            // subtype itself.
            subtype < subtype::VARINT_INLINE_0
        }
        Some(WireType::Fixed32) | Some(WireType::Fixed64) => true,
        Some(WireType::LengthDelimited) => {
            // Only plain string/bytes read from a buffer.
            // StartOfSubmessage and EndOfSubmessage do not.
            subtype == subtype::LENGTH_DELIMITED_STRING
        }
        Some(WireType::StartGroup) | Some(WireType::EndGroup) => false,
        None => false,
    }
}

// ---------------------------------------------------------------------------
// Additional constants for future sprints
// ---------------------------------------------------------------------------

/// Maximum depth of nested submessages to decompose. Deeper nesting is
/// treated as opaque strings.
pub const MAX_RECURSION_DEPTH: usize = 100;

/// Maximum varint value stored inline in the subtype (values 0..=3).
pub const MAX_VARINT_INLINE: u8 = 3;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto::make_tag;

    // ---- has_subtype ----

    #[test]
    fn test_has_subtype_varint() {
        // Criterion 8.2: only varint wire type has subtypes.
        assert!(has_subtype(make_tag(1, WireType::Varint)));
        assert!(has_subtype(make_tag(100, WireType::Varint)));
    }

    #[test]
    fn test_has_subtype_non_varint() {
        // Criterion 8.2
        assert!(!has_subtype(make_tag(1, WireType::Fixed32)));
        assert!(!has_subtype(make_tag(1, WireType::Fixed64)));
        assert!(!has_subtype(make_tag(1, WireType::LengthDelimited)));
        assert!(!has_subtype(make_tag(1, WireType::StartGroup)));
        assert!(!has_subtype(make_tag(1, WireType::EndGroup)));
    }

    // ---- has_data_buffer ----

    #[test]
    fn test_has_data_buffer_varint_buffered() {
        // Criterion 8.3: VARINT_1 has a data buffer.
        assert!(has_data_buffer(
            make_tag(1, WireType::Varint),
            subtype::VARINT_1
        ));
        assert!(has_data_buffer(
            make_tag(1, WireType::Varint),
            subtype::VARINT_10
        ));
    }

    #[test]
    fn test_has_data_buffer_varint_inline() {
        // Criterion 8.3: VARINT_INLINE_0 does NOT have a data buffer.
        assert!(!has_data_buffer(
            make_tag(1, WireType::Varint),
            subtype::VARINT_INLINE_0
        ));
        assert!(!has_data_buffer(
            make_tag(1, WireType::Varint),
            subtype::VARINT_INLINE_MAX
        ));
    }

    #[test]
    fn test_has_data_buffer_fixed() {
        assert!(has_data_buffer(
            make_tag(1, WireType::Fixed32),
            subtype::TRIVIAL
        ));
        assert!(has_data_buffer(
            make_tag(1, WireType::Fixed64),
            subtype::TRIVIAL
        ));
    }

    #[test]
    fn test_has_data_buffer_length_delimited_string() {
        // Criterion 8.3: LENGTH_DELIMITED_STRING has a data buffer.
        assert!(has_data_buffer(
            make_tag(1, WireType::LengthDelimited),
            subtype::LENGTH_DELIMITED_STRING
        ));
    }

    #[test]
    fn test_has_data_buffer_length_delimited_submessage() {
        // Criterion 8.3: END_OF_SUBMESSAGE does NOT have a data buffer.
        assert!(!has_data_buffer(
            make_tag(1, WireType::LengthDelimited),
            subtype::LENGTH_DELIMITED_END_OF_SUBMESSAGE
        ));
        assert!(!has_data_buffer(
            make_tag(1, WireType::LengthDelimited),
            subtype::LENGTH_DELIMITED_START_OF_SUBMESSAGE
        ));
    }

    #[test]
    fn test_has_data_buffer_groups() {
        assert!(!has_data_buffer(
            make_tag(1, WireType::StartGroup),
            subtype::TRIVIAL
        ));
        assert!(!has_data_buffer(
            make_tag(1, WireType::EndGroup),
            subtype::TRIVIAL
        ));
    }

    // ---- MessageId constants ----

    #[test]
    fn test_message_id_values() {
        assert_eq!(message_id::NO_OP, 0);
        assert_eq!(message_id::NON_PROTO, 1);
        assert_eq!(message_id::START_OF_SUBMESSAGE, 2);
        assert_eq!(message_id::START_OF_MESSAGE, 3);
        assert_eq!(message_id::ROOT, 4);
        // All reserved IDs must be < 8 so they don't collide with valid proto tags.
        assert!(message_id::ROOT < 8);
    }

    // ---- Subtype constants ----

    #[test]
    fn test_subtype_values() {
        assert_eq!(subtype::VARINT_1, 0);
        assert_eq!(subtype::VARINT_10, 9);
        assert_eq!(subtype::VARINT_INLINE_0, 10);
        assert_eq!(subtype::VARINT_INLINE_MAX, 137);
        assert_eq!(subtype::LENGTH_DELIMITED_STRING, 0);
        assert_eq!(subtype::LENGTH_DELIMITED_START_OF_SUBMESSAGE, 1);
        assert_eq!(subtype::LENGTH_DELIMITED_END_OF_SUBMESSAGE, 2);
    }

    #[test]
    fn test_submessage_wire_type() {
        assert_eq!(SUBMESSAGE_WIRE_TYPE, 6);
    }

    // -------------------------------------------------------------------------
    // Sprint 8 adversarial: additional internal predicate tests
    // -------------------------------------------------------------------------

    #[test]
    fn test_has_subtype_high_field_number() {
        // Subtype depends only on wire type, not field number.
        assert!(has_subtype(make_tag(0x1FFFFFFF, WireType::Varint)));
        assert!(!has_subtype(make_tag(0x1FFFFFFF, WireType::Fixed64)));
    }

    #[test]
    fn test_has_subtype_invalid_wire_type_tag() {
        // A raw tag with wire type 6 (submessage sentinel) — should return false.
        let tag_wt6 = (1u32 << 3) | 6;
        assert!(!has_subtype(tag_wt6));
    }

    #[test]
    fn test_has_data_buffer_varint_all_buffered_subtypes() {
        let tag = make_tag(1, WireType::Varint);
        // All buffered subtypes (0..=9) should have data buffer.
        for s in 0..=subtype::VARINT_MAX {
            assert!(
                has_data_buffer(tag, s),
                "subtype {s} should have data buffer"
            );
        }
        // All inline subtypes (10..=137) should NOT have data buffer.
        for s in subtype::VARINT_INLINE_0..=subtype::VARINT_INLINE_MAX {
            assert!(
                !has_data_buffer(tag, s),
                "subtype {s} should NOT have data buffer"
            );
        }
    }

    #[test]
    fn test_has_data_buffer_invalid_wire_type_returns_false() {
        // Tag with wire type 6 (submessage wire type sentinel)
        let tag_wt6 = (1u32 << 3) | 6;
        assert!(!has_data_buffer(tag_wt6, 0));
        // Tag with wire type 7
        let tag_wt7 = (1u32 << 3) | 7;
        assert!(!has_data_buffer(tag_wt7, 0));
    }
}
