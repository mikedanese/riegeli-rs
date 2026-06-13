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
use super::wire::{WireType, is_parseable_proto_message, is_proto_message};
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
/// Only top-level occurrences of `field_number` are extracted. A field with
/// the same number nested inside a group belongs to the group's scope (it is
/// a different field, just as a field inside a length-delimited submessage
/// is) and is not included.
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
            // `ProtoFieldIter` yields group contents as flat events between
            // StartGroup/EndGroup markers. Track the nesting depth so that
            // group-scoped fields are not misattributed to the top-level
            // column. Groups are balanced here because the record passed
            // `is_proto_message`.
            let mut group_depth: usize = 0;
            for result in iter {
                let field = result.map_err(|e| StreamError {
                    record_index,
                    source: e,
                })?;
                match field.wire_type {
                    WireType::StartGroup => group_depth += 1,
                    WireType::EndGroup => group_depth = group_depth.saturating_sub(1),
                    _ => {
                        if group_depth == 0
                            && field.field_number == field_number
                            && let FieldValue::Varint(v) = field.value
                        {
                            values.push(v);
                        }
                    }
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
///
/// Also returns an error for a record that parses as a proto message under
/// the permissive rules of standard proto parsers but uses non-canonical
/// (overlong) varint encodings. Such a record is a valid message to every
/// downstream consumer, yet it cannot be filtered faithfully; passing it
/// through unchanged would silently retain the fields the caller asked to
/// drop, so the function fails instead.
pub fn filter_fields_to_writer<R, W>(
    reader: &mut RecordReader<R>,
    writer: &mut RecordWriter<W>,
    field_numbers: &[u32],
) -> Result<(), StreamError>
where
    R: Read + Seek,
    W: Write,
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
        } else if is_parseable_proto_message(&record) {
            // The record is a valid proto message to standard parsers but is
            // encoded non-canonically, so the canonical field iterator cannot
            // filter it. Writing it through unchanged would silently leak the
            // fields the caller asked to drop; fail loudly instead.
            return Err(StreamError {
                record_index,
                source: RiegeliError::MalformedData(
                    "record parses as a proto message but uses non-canonical varint \
                     encoding; refusing to pass it through unfiltered"
                        .into(),
                ),
            });
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

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::{StreamError, extract_varint_column, filter_fields_to_writer};
    use crate::error::RiegeliError;
    use crate::record_reader::{ReaderOptions, RecordReader};
    use crate::record_writer::{RecordWriter, WriterOptions};

    /// Writes `records` to an in-memory riegeli file (no compression).
    fn write_records_file(records: &[Vec<u8>]) -> Vec<u8> {
        let mut buf = Cursor::new(Vec::new());
        {
            let mut w = RecordWriter::new(&mut buf, WriterOptions::new()).unwrap();
            for rec in records {
                w.write_record(rec).unwrap();
            }
            w.flush().unwrap();
        }
        buf.into_inner()
    }

    /// Reads all records back from in-memory riegeli file bytes.
    fn read_records_file(data: &[u8]) -> Vec<Vec<u8>> {
        let mut reader = RecordReader::new(Cursor::new(data), ReaderOptions::new()).unwrap();
        let mut out = Vec::new();
        while let Some(rec) = reader.read_record().unwrap() {
            out.push(rec);
        }
        out
    }

    #[test]
    fn extract_varint_column_ignores_group_scoped_fields() {
        // StartGroup(2) { field 1: varint 42 } EndGroup(2), then top-level
        // field 1: varint 7. The occurrence of field number 1 inside group 2
        // belongs to the group's nested scope — it is a different field from
        // top-level field 1 and must not be extracted into its column.
        let group_record = vec![0x13, 0x08, 42, 0x14, 0x08, 0x07];
        let file = write_records_file(&[group_record]);

        let mut reader = RecordReader::new(Cursor::new(file), ReaderOptions::new()).unwrap();
        let values = extract_varint_column(&mut reader, 1).unwrap();
        assert_eq!(values, vec![7]);
    }

    #[test]
    fn filter_refuses_to_pass_noncanonical_proto_record_through() {
        // Field 1 = varint 0 in an overlong two-byte encoding [0x80, 0x00],
        // then field 2 = bytes "secret". Standard proto parsers accept this
        // record, but the canonical-encoding gate (is_proto_message) does
        // not, so it cannot be filtered faithfully. Passing it through
        // unchanged would silently leak field 2 — exactly the field the
        // caller asked to drop — so filtering must fail instead.
        let mut record = vec![0x08, 0x80, 0x00, 0x12, 0x06];
        record.extend_from_slice(b"secret");
        let file = write_records_file(&[record]);

        let mut reader = RecordReader::new(Cursor::new(file), ReaderOptions::new()).unwrap();
        let mut out = Cursor::new(Vec::new());
        let mut writer = RecordWriter::new(&mut out, WriterOptions::new()).unwrap();
        let err = filter_fields_to_writer(&mut reader, &mut writer, &[1])
            .expect_err("a non-canonical proto record must not bypass field filtering");
        assert_eq!(err.record_index, 0);
    }

    #[test]
    fn filter_still_passes_genuinely_non_proto_records_through() {
        // A record that no proto parser accepts (wire type 7 in the first
        // tag) is genuinely non-proto data and passes through unchanged.
        let non_proto = vec![0x0F, 0xFF, 0x00];
        let proto = vec![0x08, 0x07]; // field 1 = varint 7
        let file = write_records_file(&[non_proto.clone(), proto.clone()]);

        let mut reader = RecordReader::new(Cursor::new(file), ReaderOptions::new()).unwrap();
        let mut out = Cursor::new(Vec::new());
        {
            let mut writer = RecordWriter::new(&mut out, WriterOptions::new()).unwrap();
            filter_fields_to_writer(&mut reader, &mut writer, &[1]).unwrap();
            writer.flush().unwrap();
        }
        let records = read_records_file(&out.into_inner());
        assert_eq!(records, vec![non_proto, proto]);
    }

    #[test]
    fn extract_varint_column_skips_record_with_field_only_in_group() {
        // The record's only occurrence of field 1 is inside group 2, so the
        // record contributes no value to the column.
        let group_only_record = vec![0x13, 0x08, 42, 0x14];
        let file = write_records_file(&[group_only_record]);

        let mut reader = RecordReader::new(Cursor::new(file), ReaderOptions::new()).unwrap();
        let values = extract_varint_column(&mut reader, 1).unwrap();
        assert!(values.is_empty());
    }

    /// StreamError's Display includes the record index and the inner error,
    /// and Error::source exposes the inner RiegeliError.
    #[test]
    fn stream_error_display_and_source() {
        use std::error::Error;

        let err = StreamError {
            record_index: 7,
            source: RiegeliError::MalformedData("inner error".into()),
        };

        // Display includes record index
        let display = format!("{err}");
        assert!(display.contains("record index 7"));
        assert!(display.contains("inner error"));

        // Error::source returns the inner RiegeliError
        let source = err.source().unwrap();
        let source_display = format!("{source}");
        assert!(source_display.contains("inner error"));
    }
}
