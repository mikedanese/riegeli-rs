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

    /// True if `field_number` appears anywhere in any projected path (or the
    /// projection includes everything).
    ///
    /// Used by transpose buffer pruning: nested fields' states carry the
    /// INNER field's tag, so pruning by top-level field number alone starves
    /// the buffers behind paths of length >= 2 and their values silently
    /// decode as zeros. Matching at any depth is conservative — it may keep
    /// a buffer that projection later drops, but it can never starve one.
    /// True when any path's terminal include is a full include (not
    /// existence-only): the subtree under that terminus is included
    /// wholesale, so fields inside it carry no mention of their own and
    /// static buffer pruning cannot safely drop ANY buffer.
    pub(crate) fn has_full_include(&self) -> bool {
        match &self.fields {
            None => true,
            Some(fields) => fields.iter().any(|f| !f.existence_only),
        }
    }

    pub(crate) fn mentions_field(&self, field_number: u32) -> bool {
        match &self.fields {
            None => true,
            Some(fields) => fields
                .iter()
                .any(|f| f.path.is_empty() || f.path.contains(&field_number)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builders_and_accessors() {
        assert!(FieldProjection::all().is_all());
        assert!(FieldProjection::all().fields().is_none());
        assert!(!FieldProjection::new().is_all());
        assert_eq!(FieldProjection::new().fields().map(|f| f.len()), Some(0));

        let proj = FieldProjection::new()
            .add_field(Field::new(vec![1]))
            .add_field(Field::new(vec![2, 3]).existence_only());
        let fields = proj.fields().expect("explicit projection has fields");
        assert_eq!(fields.len(), 2);
        assert!(!proj.is_all());
    }

    #[test]
    fn has_full_include_semantics() {
        // all(): everything is included.
        assert!(FieldProjection::all().has_full_include());
        // Any non-existence-only terminal makes static pruning unsafe.
        let full = FieldProjection::new().add_field(Field::new(vec![1, 2]));
        assert!(full.has_full_include());
        // Purely existence-only projections have no full include.
        let eo = FieldProjection::new()
            .add_field(Field::new(vec![1]).existence_only())
            .add_field(Field::new(vec![2, 3]).existence_only());
        assert!(!eo.has_full_include());
    }

    #[test]
    fn mentions_field_matches_any_depth() {
        assert!(FieldProjection::all().mentions_field(7));

        let proj = FieldProjection::new().add_field(Field::new(vec![1, 5]));
        assert!(proj.mentions_field(1));
        assert!(proj.mentions_field(5)); // nested mention counts (buffer pruning)
        assert!(!proj.mentions_field(2));

        // An empty path means include-everything and must match every field.
        let empty_path = FieldProjection::new().add_field(Field::new(vec![]));
        assert!(empty_path.mentions_field(42));
    }
}
