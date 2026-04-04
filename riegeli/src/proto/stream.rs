//! Streaming proto field processing over riegeli files.
//!
//! This module composes the zero-copy field-level APIs from the `proto` module
//! with the [`RecordReader`] to enable streaming columnar extraction, field
//! filtering, and handler-driven processing of proto records in riegeli files.
//!
//! The field-level APIs operate on `&[u8]` record bytes and are **not** coupled
//! to `RecordReader` internals. This module simply reads records as bytes and
//! applies the field-level operations to each one.

use std::fmt;
use std::io::{Read, Seek, Write};

use crate::error::RiegeliError;
use crate::record_reader::RecordReader;
use crate::record_writer::RecordWriter;

use super::field_iter::{FieldValue, ProtoFieldIter, copy_fields};
use super::handler::HandleField;
use super::wire::is_proto_message;
use super::writer::SerializedMessageWriter;

/// An error that occurred while processing a record stream, annotated with the
/// record index where the error was encountered.
#[derive(Debug)]
pub struct StreamError {
    /// The zero-based index of the record that triggered the error.
    pub record_index: usize,
    /// The underlying error.
    pub source: RiegeliError,
}

impl fmt::Display for StreamError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "error at record index {}: {}",
            self.record_index, self.source
        )
    }
}

impl std::error::Error for StreamError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.source)
    }
}

/// Reads all records from a `RecordReader`, dispatching each valid proto record
/// to the given field handlers. Non-proto records are routed to an optional
/// fallback callback.
///
/// # Arguments
///
/// - `reader`: A `RecordReader` to read records from.
/// - `handlers`: A mutable reference to a `HandleField` implementation that
///   receives each field of every valid proto record.
/// - `fallback`: An optional callback invoked for records that fail
///   `is_proto_message()` validation. Receives `(record_index, &[u8])`.
///
/// # Errors
///
/// Returns a `StreamError` if:
/// - A handler returns an error during dispatch (annotated with the record index).
/// - The underlying `RecordReader` returns an I/O or format error (annotated
///   with the record index at which it occurred).
pub fn for_each_proto_record<R, H, F>(
    reader: &mut RecordReader<R>,
    handlers: &mut H,
    mut fallback: Option<&mut F>,
) -> Result<(), StreamError>
where
    R: Read + Seek,
    H: HandleField,
    F: FnMut(usize, &[u8]),
{
    let mut record_index: usize = 0;

    loop {
        let record = reader.read_record().map_err(|e| StreamError {
            record_index,
            source: e,
        })?;

        let record = match record {
            Some(r) => r,
            None => break, // EOF
        };

        if is_proto_message(&record) {
            super::handler::read_message(&record, handlers).map_err(|e| StreamError {
                record_index,
                source: e,
            })?;
        } else if let Some(fb) = fallback.as_deref_mut() {
            fb(record_index, &record);
        }

        record_index += 1;
    }

    Ok(())
}

/// Extracts all values of a specific varint field from every proto record in a
/// riegeli file, producing a columnar `Vec<u64>`.
///
/// Records that are not valid proto messages are skipped (not an error).
/// If a record is a valid proto message but does not contain `field_number`,
/// no value is appended for that record.
///
/// # Arguments
///
/// - `reader`: A `RecordReader` to read records from.
/// - `field_number`: The proto field number to extract (varint wire type).
///
/// # Returns
///
/// A vector of extracted varint values, one per occurrence across all records.
/// If a single record contains multiple occurrences of the field, all are included.
pub fn extract_varint_column<R: Read + Seek>(
    reader: &mut RecordReader<R>,
    field_number: u32,
) -> Result<Vec<u64>, StreamError> {
    let mut values = Vec::new();
    let mut record_index: usize = 0;

    loop {
        let record = reader.read_record().map_err(|e| StreamError {
            record_index,
            source: e,
        })?;

        let record = match record {
            Some(r) => r,
            None => break,
        };

        if is_proto_message(&record) {
            let iter = ProtoFieldIter::new(&record);
            for result in iter {
                let field = result.map_err(|e| StreamError {
                    record_index,
                    source: e,
                })?;
                if field.field_number == field_number
                    && let FieldValue::Varint(v) = field.value
                {
                    values.push(v);
                }
            }
        }

        record_index += 1;
    }

    Ok(values)
}

/// Reads all records from `reader`, filters each proto record to only the
/// specified field numbers, and writes the filtered records to `writer`.
///
/// Non-proto records are written through unchanged (pass-through).
///
/// # Arguments
///
/// - `reader`: Source `RecordReader`.
/// - `writer`: Destination `RecordWriter`.
/// - `field_numbers`: The set of field numbers to keep.
///
/// # Errors
///
/// Returns a `StreamError` if reading, filtering, or writing fails, annotated
/// with the record index.
pub fn filter_fields_to_writer<R, W>(
    reader: &mut RecordReader<R>,
    writer: &mut RecordWriter<W>,
    field_numbers: &[u32],
) -> Result<(), StreamError>
where
    R: Read + Seek,
    W: Write + Seek,
{
    let mut record_index: usize = 0;

    loop {
        let record = reader.read_record().map_err(|e| StreamError {
            record_index,
            source: e,
        })?;

        let record = match record {
            Some(r) => r,
            None => break,
        };

        if is_proto_message(&record) {
            // Filter to selected fields and write the filtered record.
            let mut msg_writer = SerializedMessageWriter::new();
            copy_fields(&record, field_numbers, &mut msg_writer).map_err(|e| StreamError {
                record_index,
                source: e,
            })?;
            let filtered = msg_writer.finish().map_err(|e| StreamError {
                record_index,
                source: e,
            })?;
            writer.write_record(&filtered).map_err(|e| StreamError {
                record_index,
                source: e,
            })?;
        } else {
            // Non-proto records pass through unchanged.
            writer.write_record(&record).map_err(|e| StreamError {
                record_index,
                source: e,
            })?;
        }

        record_index += 1;
    }

    Ok(())
}
