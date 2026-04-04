//! `FieldProjection` — read-time column pruning for transpose-encoded files.
//!
//! When a `FieldProjection` is set on `ReaderOptions`, the `TransposeChunkDecoder`
//! skips decompression of data buckets whose buffers do not contribute to any
//! projected field. Non-proto records always pass through unchanged.
//!
//! ## Usage
//!
//! ```rust,ignore
//! let projection = FieldProjection::new()
//!     .add_field(Field::new(vec![1]))           // include field 1
//!     .add_field(Field::new(vec![2, 3]));        // include field 3 nested in field 2
//!
//! let opts = ReaderOptions::new().field_projection(projection);
//! let mut reader = RecordReader::new(file, opts)?;
//! ```

use crate::proto::{WireType, tag_field_number, tag_wire_type};
use crate::varint::{decode_u32, encode_u32};

/// A proto field path from the root message, represented as a sequence of
/// field numbers.
///
/// For example, `Field::new(vec![1, 2, 3])` selects field 3 within the
/// submessage at field 2 within the submessage at field 1.
///
/// `Field::new(vec![1])` selects field 1 directly.
#[derive(Debug, Clone)]
pub struct Field {
    /// Field numbers from root to this field.
    pub path: Vec<u32>,
    /// If `true`, the field's tag is preserved in the output but its value
    /// is zeroed (existence-only mode).
    pub existence_only: bool,
}

impl Field {
    /// Create a new field path with the given sequence of field numbers.
    pub fn new(path: Vec<u32>) -> Self {
        Self {
            path,
            existence_only: false,
        }
    }

    /// Mark this field as existence-only: the field tag is preserved in the
    /// output record but the value is set to zero.
    pub fn existence_only(mut self) -> Self {
        self.existence_only = true;
        self
    }
}

/// A set of proto field paths used to prune columns at read time.
///
/// When set to `all()` (the default), all fields are returned unchanged.
/// When fields are explicitly added, only those fields are included in the
/// output (for proto records in transpose chunks). Non-proto records always
/// pass through unchanged.
#[derive(Debug, Clone)]
pub struct FieldProjection {
    /// `None` means all fields (pass-through mode).
    /// `Some(vec)` means only these specific field paths.
    fields: Option<Vec<Field>>,
}

impl Default for FieldProjection {
    fn default() -> Self {
        Self::all()
    }
}

impl FieldProjection {
    /// Create a projection that includes all fields (pass-through, no filtering).
    pub fn all() -> Self {
        Self { fields: None }
    }

    /// Create an empty projection. Fields must be added via `add_field`.
    ///
    /// An empty projection (no fields added) will return empty records for all
    /// proto records.
    pub fn new() -> Self {
        Self {
            fields: Some(Vec::new()),
        }
    }

    /// Add a field to this projection and return `self` for chaining.
    pub fn add_field(mut self, field: Field) -> Self {
        if let Some(ref mut fields) = self.fields {
            fields.push(field);
        } else {
            // Was all() — switch to filtered mode with this field.
            self.fields = Some(vec![field]);
        }
        self
    }

    /// Returns `true` if this is an all-fields (pass-through) projection.
    pub fn is_all(&self) -> bool {
        self.fields.is_none()
    }

    /// Returns the list of explicitly projected fields, or `None` if all fields.
    pub fn fields(&self) -> Option<&[Field]> {
        self.fields.as_deref()
    }

    /// Returns `true` if a field number at the top level is included in this
    /// projection.
    pub(crate) fn includes_top_level_field(&self, field_number: u32) -> bool {
        match &self.fields {
            None => true,
            Some(fields) => fields.iter().any(|f| f.path.first() == Some(&field_number)),
        }
    }

    /// Returns the sub-projection for fields nested under `field_number`.
    ///
    /// Used when descending into a submessage: the returned projection contains
    /// all fields from the current projection that start with `field_number`,
    /// with the first element of each path stripped.
    pub(crate) fn sub_projection(&self, field_number: u32) -> FieldProjection {
        match &self.fields {
            None => FieldProjection::all(),
            Some(fields) => {
                let sub_fields: Vec<Field> = fields
                    .iter()
                    .filter(|f| f.path.first() == Some(&field_number) && f.path.len() > 1)
                    .map(|f| Field {
                        path: f.path[1..].to_vec(),
                        existence_only: f.existence_only,
                    })
                    .collect();
                FieldProjection {
                    fields: Some(sub_fields),
                }
            }
        }
    }

    /// Returns the `Field` entry for a top-level field number, if projected.
    pub(crate) fn field_for(&self, field_number: u32) -> Option<&Field> {
        self.fields
            .as_ref()?
            .iter()
            .find(|f| f.path.len() == 1 && f.path[0] == field_number)
    }

    /// Returns `true` if a top-level field is included at exactly this level
    /// (path length == 1), as opposed to only being a prefix of a deeper path.
    pub(crate) fn is_leaf_field(&self, field_number: u32) -> bool {
        match &self.fields {
            None => true,
            Some(fields) => fields
                .iter()
                .any(|f| f.path.len() == 1 && f.path[0] == field_number),
        }
    }

    /// Apply this projection to a decoded proto record.
    ///
    /// Returns the filtered record bytes. If this is an `all()` projection,
    /// returns the input unchanged (same allocation). Non-proto records are
    /// returned as-is.
    ///
    /// For proto records, only the top-level fields in the projection are included.
    /// Nested fields are handled via `sub_projection`.
    ///
    /// `existence_only` fields have their tag included but value zeroed.
    pub(crate) fn apply(&self, record: &[u8]) -> Vec<u8> {
        if self.is_all() {
            return record.to_vec();
        }
        if !crate::proto::is_proto_message(record) {
            return record.to_vec();
        }
        apply_projection_inner(record, self)
    }
}

// ---------------------------------------------------------------------------
// Post-decode record filtering (internal)
// ---------------------------------------------------------------------------

fn apply_projection_inner(record: &[u8], projection: &FieldProjection) -> Vec<u8> {
    let mut out = Vec::with_capacity(record.len());
    let mut pos = 0;

    while pos < record.len() {
        // Read the tag varint.
        let remaining = &record[pos..];
        let (tag, consumed) = match decode_u32(remaining) {
            Ok(v) => v,
            Err(_) => break,
        };
        let field_number = tag_field_number(tag);
        let wire_type = match tag_wire_type(tag) {
            Some(wt) => wt,
            None => break,
        };
        pos += consumed;

        // Check if this field should be included.
        let include = projection.includes_top_level_field(field_number);
        let existence_only = include
            && projection
                .field_for(field_number)
                .map(|f| f.existence_only)
                .unwrap_or(false);

        // Determine if this is a submessage that may need recursive projection.
        let needs_sub_projection =
            include && !existence_only && wire_type == WireType::LengthDelimited && {
                let sub = projection.sub_projection(field_number);
                !sub.is_all() || projection.is_leaf_field(field_number)
            };

        // Read the field value bytes.
        let (value_bytes, field_end) = match read_field_value(record, pos, wire_type) {
            Some(v) => v,
            None => break,
        };

        if !include {
            pos = field_end;
            continue;
        }

        let tag_bytes = encode_u32(tag);

        if existence_only {
            // Include tag + zero value.
            out.extend_from_slice(&tag_bytes);
            // Write a zero value appropriate for the wire type.
            match wire_type {
                WireType::Varint => out.push(0x00),
                WireType::Fixed32 => out.extend_from_slice(&[0u8; 4]),
                WireType::Fixed64 => out.extend_from_slice(&[0u8; 8]),
                WireType::LengthDelimited => {
                    out.push(0x00); // zero-length string
                }
                WireType::StartGroup | WireType::EndGroup => {
                    // For groups: just emit the start tag and end tag with zero content.
                    // Find the matching EndGroup.
                    out.extend_from_slice(&tag_bytes); // start group already in tag_bytes
                    let end_tag = encode_u32((field_number << 3) | (WireType::EndGroup as u32));
                    out.extend_from_slice(&end_tag);
                }
            }
        } else if needs_sub_projection && wire_type == WireType::LengthDelimited {
            // Recurse into the submessage with sub-projection.
            let sub = projection.sub_projection(field_number);
            let is_leaf = projection.is_leaf_field(field_number);
            let filtered_content = if !sub.is_all() && !is_leaf {
                // Only deeper fields are projected — recurse.
                apply_projection_inner(value_bytes, &sub)
            } else {
                // This is a leaf (exact match) — include the whole submessage.
                value_bytes.to_vec()
            };
            let len_varint = encode_u32(filtered_content.len() as u32);
            out.extend_from_slice(&tag_bytes);
            out.extend_from_slice(&len_varint);
            out.extend_from_slice(&filtered_content);
        } else {
            // Include tag + value unchanged.
            out.extend_from_slice(&tag_bytes);
            // Re-encode length prefix for length-delimited fields.
            if wire_type == WireType::LengthDelimited {
                let len_varint = encode_u32(value_bytes.len() as u32);
                out.extend_from_slice(&len_varint);
            }
            out.extend_from_slice(value_bytes);
        }

        pos = field_end;
    }

    out
}

/// Read a field value starting at `pos` in `data`, given the wire type.
///
/// Returns `(value_bytes, end_pos)` where `value_bytes` is the raw value
/// (without tag or length prefix) and `end_pos` is the position after the field.
/// Returns `None` on truncation.
fn read_field_value(data: &[u8], pos: usize, wire_type: WireType) -> Option<(&[u8], usize)> {
    match wire_type {
        WireType::Varint => {
            let mut end = pos;
            loop {
                if end >= data.len() {
                    return None;
                }
                let b = data[end];
                end += 1;
                if b < 0x80 {
                    break;
                }
            }
            Some((&data[pos..end], end))
        }
        WireType::Fixed32 => {
            if pos + 4 > data.len() {
                return None;
            }
            Some((&data[pos..pos + 4], pos + 4))
        }
        WireType::Fixed64 => {
            if pos + 8 > data.len() {
                return None;
            }
            Some((&data[pos..pos + 8], pos + 8))
        }
        WireType::LengthDelimited => {
            let remaining = &data[pos..];
            let (len, consumed) = decode_u32(remaining).ok()?;
            let data_start = pos + consumed;
            let data_end = data_start + len as usize;
            if data_end > data.len() {
                return None;
            }
            Some((&data[data_start..data_end], data_end))
        }
        WireType::StartGroup => {
            // Find the matching EndGroup tag.
            let field_number = {
                // We need the field number from the tag before pos — we get it from context.
                // Actually we can't recover it here without more context. For groups, we
                // scan forward to find EndGroup.
                0u32 // placeholder
            };
            let _ = field_number;
            // Scan forward to find the end of this group.
            let end = find_group_end(data, pos)?;
            Some((&data[pos..end], end))
        }
        WireType::EndGroup => {
            // EndGroup has no value.
            Some((&data[pos..pos], pos))
        }
    }
}

/// Scan from `pos` to find the end of a StartGroup field (past the EndGroup tag).
///
/// Returns the position after the EndGroup tag, or `None` on truncation.
fn find_group_end(data: &[u8], pos: usize) -> Option<usize> {
    let mut p = pos;
    let mut depth = 1usize;
    while p < data.len() && depth > 0 {
        let remaining = &data[p..];
        let (tag, consumed) = decode_u32(remaining).ok()?;
        p += consumed;
        match tag_wire_type(tag) {
            Some(WireType::StartGroup) => depth += 1,
            Some(WireType::EndGroup) => {
                depth -= 1;
                if depth == 0 {
                    return Some(p);
                }
            }
            Some(WireType::Varint) => {
                // Skip varint value.
                while p < data.len() {
                    let b = data[p];
                    p += 1;
                    if b < 0x80 {
                        break;
                    }
                }
            }
            Some(WireType::Fixed32) => {
                if p + 4 > data.len() {
                    return None;
                }
                p += 4;
            }
            Some(WireType::Fixed64) => {
                if p + 8 > data.len() {
                    return None;
                }
                p += 8;
            }
            Some(WireType::LengthDelimited) => {
                let rem = &data[p..];
                let (len, consumed) = decode_u32(rem).ok()?;
                p += consumed + len as usize;
                if p > data.len() {
                    return None;
                }
            }
            None => return None,
        }
    }
    if depth == 0 { Some(p) } else { None }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto::make_tag;

    fn encode_varint_field(field_number: u32, value: u64) -> Vec<u8> {
        let tag = make_tag(field_number, WireType::Varint);
        let mut out = encode_u32(tag);
        // Encode value as varint.
        let mut v = value;
        loop {
            let byte = (v & 0x7F) as u8;
            v >>= 7;
            if v == 0 {
                out.push(byte);
                break;
            } else {
                out.push(byte | 0x80);
            }
        }
        out
    }

    fn encode_fixed32_field(field_number: u32, value: u32) -> Vec<u8> {
        let tag = make_tag(field_number, WireType::Fixed32);
        let mut out = encode_u32(tag);
        out.extend_from_slice(&value.to_le_bytes());
        out
    }

    fn encode_string_field(field_number: u32, value: &[u8]) -> Vec<u8> {
        let tag = make_tag(field_number, WireType::LengthDelimited);
        let mut out = encode_u32(tag);
        out.extend_from_slice(&encode_u32(value.len() as u32));
        out.extend_from_slice(value);
        out
    }

    #[test]
    fn test_all_projection_passthrough() {
        let mut record = Vec::new();
        record.extend_from_slice(&encode_varint_field(1, 42));
        record.extend_from_slice(&encode_fixed32_field(2, 100));
        record.extend_from_slice(&encode_string_field(3, b"hello"));

        let result = FieldProjection::all().apply(&record);
        assert_eq!(result, record);
    }

    #[test]
    fn test_field_projection_only_field1() {
        let mut record = Vec::new();
        record.extend_from_slice(&encode_varint_field(1, 42));
        record.extend_from_slice(&encode_fixed32_field(2, 100));
        record.extend_from_slice(&encode_string_field(3, b"hello"));

        let proj = FieldProjection::new().add_field(Field::new(vec![1]));
        let result = proj.apply(&record);

        // Result should contain only field 1.
        let expected = encode_varint_field(1, 42);
        assert_eq!(result, expected);
    }

    #[test]
    fn test_existence_only_varint() {
        let mut record = Vec::new();
        record.extend_from_slice(&encode_varint_field(1, 42));

        let proj = FieldProjection::new().add_field(Field::new(vec![1]).existence_only());
        let result = proj.apply(&record);

        // Should have tag 0x08 followed by value 0x00.
        let tag_bytes = encode_u32(make_tag(1, WireType::Varint));
        let mut expected = tag_bytes;
        expected.push(0x00);
        assert_eq!(result, expected);
    }

    #[test]
    fn test_nested_field_projection() {
        // Build: field 1 (submessage containing field 2 (submessage containing field 3))
        let inner_inner = encode_varint_field(3, 99);
        let inner = encode_string_field(2, &inner_inner);
        let outer = encode_string_field(1, &inner);

        // Project only [1, 2, 3].
        let proj = FieldProjection::new().add_field(Field::new(vec![1, 2, 3]));
        let result = proj.apply(&outer);

        // The result should contain field 1 > field 2 > field 3.
        // Re-parse and verify.
        assert!(!result.is_empty());

        // Parse result: should have field 1 (LengthDelimited).
        let (tag, consumed) = decode_u32(&result).expect("parse tag");
        assert_eq!(tag_field_number(tag), 1);
        assert_eq!(tag_wire_type(tag), Some(WireType::LengthDelimited));
        let rest = &result[consumed..];
        let (len, consumed2) = decode_u32(rest).expect("parse len");
        let submsg1 = &rest[consumed2..consumed2 + len as usize];

        // Parse submsg1: should have field 2 (LengthDelimited).
        let (tag2, c2) = decode_u32(submsg1).expect("parse tag2");
        assert_eq!(tag_field_number(tag2), 2);
        assert_eq!(tag_wire_type(tag2), Some(WireType::LengthDelimited));
        let rest2 = &submsg1[c2..];
        let (len2, c3) = decode_u32(rest2).expect("parse len2");
        let submsg2 = &rest2[c3..c3 + len2 as usize];

        // Parse submsg2: should have field 3 (varint) = 99.
        let (tag3, c4) = decode_u32(submsg2).expect("parse tag3");
        assert_eq!(tag_field_number(tag3), 3);
        assert_eq!(tag_wire_type(tag3), Some(WireType::Varint));
        let val = submsg2[c4];
        assert_eq!(val, 99);
    }

    #[test]
    fn test_includes_top_level_field() {
        let proj = FieldProjection::new()
            .add_field(Field::new(vec![1]))
            .add_field(Field::new(vec![3, 1]));
        assert!(proj.includes_top_level_field(1));
        assert!(!proj.includes_top_level_field(2));
        assert!(proj.includes_top_level_field(3));
    }

    // -------------------------------------------------------------------------
    // Migrated from sprint_18_adversarial: direct apply() tests
    // -------------------------------------------------------------------------

    #[test]
    fn test_ld_leaf_field_includes_whole_submessage() {
        let inner = {
            let mut b = Vec::new();
            b.extend_from_slice(&encode_varint_field(2, 42));
            b.extend_from_slice(&encode_string_field(3, b"inner_data"));
            b
        };
        let record = encode_string_field(1, &inner);

        let proj = FieldProjection::new().add_field(Field::new(vec![1]));
        let result = proj.apply(&record);
        assert_eq!(
            result, record,
            "leaf LengthDelimited field should be included unchanged"
        );
    }

    #[test]
    fn test_projection_on_empty_record() {
        let record: &[u8] = b"";
        let proj = FieldProjection::new().add_field(Field::new(vec![1]));
        let result = proj.apply(record);
        assert!(result.is_empty(), "empty record should project to empty");

        let result2 = FieldProjection::all().apply(record);
        assert!(
            result2.is_empty(),
            "all() projection on empty record should return empty"
        );
    }

    #[test]
    fn test_projection_large_field_number() {
        let record = encode_varint_field(200, 12345u64);
        let proj = FieldProjection::new().add_field(Field::new(vec![200]));
        let result = proj.apply(&record);
        assert_eq!(
            result, record,
            "large field number field should be included intact"
        );

        let result_all = FieldProjection::all().apply(&record);
        assert_eq!(result_all, record);
    }

    // -------------------------------------------------------------------------
    // Migrated from sprint_18_projection: nested field projection
    // -------------------------------------------------------------------------

    #[test]
    fn test_nested_field_projection_with_exclusion() {
        // field 1 (submessage): field 2 (submessage): field 3 (varint) = 42
        //                       field 4 (varint) = 99  -- excluded
        // field 5 (varint) = 999  -- excluded
        let inner_inner = {
            let mut b = Vec::new();
            b.extend_from_slice(&encode_varint_field(3, 42));
            b.extend_from_slice(&encode_varint_field(4, 99));
            b
        };
        let inner = encode_string_field(2, &inner_inner);
        let mut outer = Vec::new();
        outer.extend_from_slice(&encode_string_field(1, &inner));
        outer.extend_from_slice(&encode_varint_field(5, 999));

        let proj = FieldProjection::new().add_field(Field::new(vec![1, 2, 3]));
        let filtered = proj.apply(&outer);

        // Parse outer: field 1 should be present
        let (tag1, c1) = decode_u32(&filtered).expect("tag1");
        assert_eq!(tag_field_number(tag1), 1);
        let (len1, c1l) = decode_u32(&filtered[c1..]).expect("len1");
        let submsg1 = &filtered[c1 + c1l..c1 + c1l + len1 as usize];

        // Parse submsg1: field 2 should be present
        let (tag2, c2) = decode_u32(submsg1).expect("tag2");
        assert_eq!(tag_field_number(tag2), 2);
        let (len2, c2l) = decode_u32(&submsg1[c2..]).expect("len2");
        let submsg2 = &submsg1[c2 + c2l..c2 + c2l + len2 as usize];

        // Parse submsg2: field 3 = 42, field 4 absent
        let (tag3, c3) = decode_u32(submsg2).expect("tag3");
        assert_eq!(tag_field_number(tag3), 3);
        assert_eq!(tag_wire_type(tag3), Some(WireType::Varint));
        assert_eq!(submsg2[c3], 42);
        // No more data after field 3 value (field 4 excluded)
        assert_eq!(c3 + 1, submsg2.len(), "field 4 should be excluded");

        // field 5 should be absent from filtered
        assert_eq!(
            c1 + c1l + len1 as usize,
            filtered.len(),
            "field 5 should be excluded"
        );
    }
}
