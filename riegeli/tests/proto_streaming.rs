//! Sprint 29 tests: End-to-End Streaming and Cross-Language Conformance.

mod common;

use std::io::Cursor;

use riegeli::proto_stream::{
    StreamError, extract_varint_column, filter_fields_to_writer, for_each_proto_record,
};
use riegeli::proto_wire::{
    DynamicHandlerSet, FieldValue, FilteredFieldIter, HandleField, ProtoField, ProtoFieldIter,
    SerializedMessageWriter, copy_fields, is_proto_message, read_message,
};
use riegeli::{ReaderOptions, RecordReader, RecordWriter, WriterOptions};
use riegeli_ffi::{
    Compression, RecordReader as FfiReader, RecordWriter as FfiWriter,
    WriterOptions as FfiWriterOptions,
};

// ---------------------------------------------------------------------------
// Helper: build a test proto message with fields 1 (varint), 2 (bytes), 3 (fixed32)
// ---------------------------------------------------------------------------

fn build_test_message(i: u64) -> Vec<u8> {
    let mut w = SerializedMessageWriter::new();
    w.write_uint64(1, i).unwrap();
    w.write_bytes(2, format!("name_{i}").as_bytes()).unwrap();
    w.write_fixed32(3, (i as u32).wrapping_mul(7)).unwrap();
    w.finish().unwrap()
}

/// Write `records` using the Rust writer with the given options, returning the
/// riegeli file bytes.
fn rust_write_records(records: &[Vec<u8>], opts: WriterOptions) -> Vec<u8> {
    let mut buf = Cursor::new(Vec::new());
    {
        let mut w = RecordWriter::new(&mut buf, opts).unwrap();
        for rec in records {
            w.write_record(rec).unwrap();
        }
        w.flush().unwrap();
    }
    buf.into_inner()
}

/// Read all records from riegeli file bytes using the Rust reader.
fn rust_read_records(data: &[u8]) -> Vec<Vec<u8>> {
    let mut reader = RecordReader::new(Cursor::new(data), ReaderOptions::new()).unwrap();
    let mut out = Vec::new();
    while let Some(rec) = reader.read_record().unwrap() {
        out.push(rec);
    }
    out
}

// ===========================================================================
// Criterion 29.1: Streaming columnar extraction of a single varint field
//   from 1000 records produces a Vec of 1000 values matching full deser.
// ===========================================================================

#[test]
fn criterion_29_1_columnar_extraction_1000_records() {
    let records: Vec<Vec<u8>> = (0..1000u64).map(build_test_message).collect();
    let file_bytes = rust_write_records(&records, WriterOptions::new());

    let mut reader = RecordReader::new(Cursor::new(&file_bytes), ReaderOptions::new()).unwrap();
    let values = extract_varint_column(&mut reader, 1).unwrap();

    assert_eq!(values.len(), 1000, "expected 1000 extracted values");
    for (i, &v) in values.iter().enumerate() {
        assert_eq!(v, i as u64, "value mismatch at index {i}");
    }
}

#[test]
fn criterion_29_1_columnar_extraction_matches_manual_deserialization() {
    let records: Vec<Vec<u8>> = (0..1000u64).map(build_test_message).collect();
    let file_bytes = rust_write_records(&records, WriterOptions::new());

    // Extract via streaming API.
    let mut reader = RecordReader::new(Cursor::new(&file_bytes), ReaderOptions::new()).unwrap();
    let streaming_values = extract_varint_column(&mut reader, 1).unwrap();

    // Extract via manual full deserialization.
    let mut manual_values = Vec::new();
    for rec in &records {
        for field in ProtoFieldIter::new(rec) {
            let field = field.unwrap();
            if field.field_number == 1 {
                if let FieldValue::Varint(v) = field.value {
                    manual_values.push(v);
                }
            }
        }
    }

    assert_eq!(streaming_values, manual_values);
}

// ===========================================================================
// Criterion 29.2: Field filter for {1, 2} read-write pipeline produces
//   records containing only fields 1 and 2.
// ===========================================================================

#[test]
fn criterion_29_2_field_filter_read_write_pipeline() {
    let records: Vec<Vec<u8>> = (0..50u64).map(build_test_message).collect();
    let file_bytes = rust_write_records(&records, WriterOptions::new());

    // Filter pipeline: read -> filter to fields {1, 2} -> write.
    let mut output_buf = Cursor::new(Vec::new());
    {
        let mut reader = RecordReader::new(Cursor::new(&file_bytes), ReaderOptions::new()).unwrap();
        let mut writer = RecordWriter::new(&mut output_buf, WriterOptions::new()).unwrap();
        filter_fields_to_writer(&mut reader, &mut writer, &[1, 2]).unwrap();
        writer.flush().unwrap();
    }
    let filtered_file = output_buf.into_inner();

    // Read back filtered records and verify they contain only fields 1 and 2.
    let filtered_records = rust_read_records(&filtered_file);
    assert_eq!(filtered_records.len(), 50);

    for (i, rec) in filtered_records.iter().enumerate() {
        assert!(is_proto_message(rec), "record {i} is not valid proto");

        let fields: Vec<ProtoField> = ProtoFieldIter::new(rec)
            .collect::<Result<Vec<_>, _>>()
            .unwrap();

        // Should contain only fields 1 and 2 (field 3 should be filtered out).
        for f in &fields {
            assert!(
                f.field_number == 1 || f.field_number == 2,
                "record {i}: unexpected field number {}",
                f.field_number
            );
        }

        // Verify field 1 has correct value.
        let field1 = fields.iter().find(|f| f.field_number == 1).unwrap();
        assert_eq!(field1.value, FieldValue::Varint(i as u64));

        // Verify field 2 has correct bytes.
        let field2 = fields.iter().find(|f| f.field_number == 2).unwrap();
        let expected_name = format!("name_{i}");
        assert_eq!(
            field2.value,
            FieldValue::LengthDelimited(expected_name.as_bytes())
        );
    }
}

// ===========================================================================
// Criterion 29.3: Streaming API works with both simple and transpose chunk
//   encodings.
// ===========================================================================

#[test]
fn criterion_29_3_simple_encoding() {
    let records: Vec<Vec<u8>> = (0..100u64).map(build_test_message).collect();
    let file_bytes = rust_write_records(&records, WriterOptions::new());

    let mut reader = RecordReader::new(Cursor::new(&file_bytes), ReaderOptions::new()).unwrap();
    let values = extract_varint_column(&mut reader, 1).unwrap();
    assert_eq!(values.len(), 100);
    for (i, &v) in values.iter().enumerate() {
        assert_eq!(v, i as u64);
    }
}

#[test]
fn criterion_29_3_transpose_encoding() {
    let records: Vec<Vec<u8>> = (0..100u64).map(build_test_message).collect();
    let file_bytes = rust_write_records(&records, WriterOptions::new().transpose(true));

    let mut reader = RecordReader::new(Cursor::new(&file_bytes), ReaderOptions::new()).unwrap();
    let values = extract_varint_column(&mut reader, 1).unwrap();
    assert_eq!(values.len(), 100);
    for (i, &v) in values.iter().enumerate() {
        assert_eq!(v, i as u64);
    }
}

#[test]
fn criterion_29_3_filter_with_transpose() {
    let records: Vec<Vec<u8>> = (0..50u64).map(build_test_message).collect();
    let file_bytes = rust_write_records(&records, WriterOptions::new().transpose(true));

    let mut output_buf = Cursor::new(Vec::new());
    {
        let mut reader = RecordReader::new(Cursor::new(&file_bytes), ReaderOptions::new()).unwrap();
        let mut writer = RecordWriter::new(&mut output_buf, WriterOptions::new()).unwrap();
        filter_fields_to_writer(&mut reader, &mut writer, &[1, 2]).unwrap();
        writer.flush().unwrap();
    }
    let filtered_file = output_buf.into_inner();

    let filtered_records = rust_read_records(&filtered_file);
    assert_eq!(filtered_records.len(), 50);

    for (i, rec) in filtered_records.iter().enumerate() {
        let fields: Vec<ProtoField> = ProtoFieldIter::new(rec)
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        for f in &fields {
            assert!(
                f.field_number == 1 || f.field_number == 2,
                "record {i}: unexpected field number {} in transpose mode",
                f.field_number
            );
        }
    }
}

#[test]
fn criterion_29_3_for_each_with_both_encodings() {
    use std::cell::Cell;

    for transpose in [false, true] {
        let records: Vec<Vec<u8>> = (0..20u64).map(build_test_message).collect();
        let opts = if transpose {
            WriterOptions::new().transpose(true)
        } else {
            WriterOptions::new()
        };
        let file_bytes = rust_write_records(&records, opts);

        let mut reader = RecordReader::new(Cursor::new(&file_bytes), ReaderOptions::new()).unwrap();
        let count = Cell::new(0usize);
        let mut handlers = DynamicHandlerSet::new();
        handlers.on_varint(1, |_v| {
            count.set(count.get() + 1);
            Ok(())
        });
        for_each_proto_record::<_, _, fn(usize, &[u8])>(&mut reader, &mut handlers, None).unwrap();
        assert_eq!(
            count.get(),
            20,
            "for_each_proto_record should process 20 records (transpose={transpose})"
        );
    }
}

// ===========================================================================
// Criterion 29.4: Non-proto records are not dispatched to field handlers
//   but can be handled by a fallback callback.
// ===========================================================================

#[test]
fn criterion_29_4_non_proto_records_fallback() {
    use std::cell::RefCell;

    // Write a mix of proto and non-proto records.
    let mut all_records = Vec::new();
    // Record 0: valid proto
    all_records.push(build_test_message(0));
    // Record 1: non-proto (just plain text)
    all_records.push(b"hello world, not a proto".to_vec());
    // Record 2: valid proto
    all_records.push(build_test_message(2));
    // Record 3: non-proto (invalid wire format: wire type 6)
    all_records.push(vec![0x0E, 0x01]);
    // Record 4: valid proto
    all_records.push(build_test_message(4));

    let file_bytes = rust_write_records(&all_records, WriterOptions::new());

    let mut reader = RecordReader::new(Cursor::new(&file_bytes), ReaderOptions::new()).unwrap();

    let proto_values = RefCell::new(Vec::new());
    let mut non_proto_indices = Vec::new();

    let mut handlers = DynamicHandlerSet::new();
    handlers.on_varint(1, |v| {
        proto_values.borrow_mut().push(v);
        Ok(())
    });

    let mut fallback = |idx: usize, _data: &[u8]| {
        non_proto_indices.push(idx);
    };

    for_each_proto_record(&mut reader, &mut handlers, Some(&mut fallback)).unwrap();

    // Proto records: indices 0, 2, 4 with varint field 1 values 0, 2, 4
    assert_eq!(*proto_values.borrow(), vec![0u64, 2, 4]);
    // Non-proto records: indices 1 and 3
    assert_eq!(non_proto_indices, vec![1, 3]);
}

#[test]
fn criterion_29_4_non_proto_skipped_without_fallback() {
    use std::cell::Cell;

    let mut all_records = Vec::new();
    all_records.push(build_test_message(42));
    all_records.push(b"not proto".to_vec());

    let file_bytes = rust_write_records(&all_records, WriterOptions::new());
    let mut reader = RecordReader::new(Cursor::new(&file_bytes), ReaderOptions::new()).unwrap();

    let proto_count = Cell::new(0usize);
    let mut handlers = DynamicHandlerSet::new();
    handlers.on_varint(1, |_v| {
        proto_count.set(proto_count.get() + 1);
        Ok(())
    });

    // No fallback: non-proto records are silently skipped.
    for_each_proto_record::<_, _, fn(usize, &[u8])>(&mut reader, &mut handlers, None).unwrap();
    assert_eq!(proto_count.get(), 1);
}

// ===========================================================================
// Criterion 29.5: Handler error mid-stream includes record index context.
// ===========================================================================

#[test]
fn criterion_29_5_handler_error_includes_record_index() {
    let records: Vec<Vec<u8>> = (0..10u64).map(build_test_message).collect();
    let file_bytes = rust_write_records(&records, WriterOptions::new());

    let mut reader = RecordReader::new(Cursor::new(&file_bytes), ReaderOptions::new()).unwrap();

    let mut handlers = DynamicHandlerSet::new();
    handlers.on_varint(1, |v| {
        if v == 5 {
            Err(riegeli::RiegeliError::MalformedData(
                "intentional test error".to_string(),
            ))
        } else {
            Ok(())
        }
    });

    let err = for_each_proto_record::<_, _, fn(usize, &[u8])>(&mut reader, &mut handlers, None)
        .unwrap_err();

    assert_eq!(err.record_index, 5, "error should report record index 5");
    let display = format!("{err}");
    assert!(
        display.contains("record index 5"),
        "display should mention record index: {display}"
    );
    assert!(
        display.contains("intentional test error"),
        "display should contain the original error: {display}"
    );
}

#[test]
fn criterion_29_5_error_at_first_record() {
    let records: Vec<Vec<u8>> = (0..5u64).map(build_test_message).collect();
    let file_bytes = rust_write_records(&records, WriterOptions::new());

    let mut reader = RecordReader::new(Cursor::new(&file_bytes), ReaderOptions::new()).unwrap();

    let mut handlers = DynamicHandlerSet::new();
    handlers.on_varint(1, |_v| {
        Err(riegeli::RiegeliError::MalformedData(
            "fail immediately".to_string(),
        ))
    });

    let err = for_each_proto_record::<_, _, fn(usize, &[u8])>(&mut reader, &mut handlers, None)
        .unwrap_err();
    assert_eq!(err.record_index, 0);
}

// ===========================================================================
// Criterion 29.6: Field iteration over a proto message serialized by the C++
//   protobuf library produces results consistent with the C++ riegeli field
//   iteration, verified via the existing FFI bridge.
// ===========================================================================

#[test]
fn criterion_29_6_cpp_written_records_field_iteration_consistent() {
    // Build proto messages in Rust and write them via C++ FFI writer.
    let records: Vec<Vec<u8>> = (0..100u64).map(build_test_message).collect();

    let mut cpp_writer =
        FfiWriter::new(FfiWriterOptions::new().compression(Compression::None)).unwrap();
    for rec in &records {
        cpp_writer.write_record(rec).unwrap();
    }
    let cpp_file = cpp_writer.close().unwrap();

    // Read back via Rust reader and verify field iteration matches.
    let mut reader = RecordReader::new(Cursor::new(&cpp_file), ReaderOptions::new()).unwrap();
    let mut record_index = 0usize;

    while let Some(rec) = reader.read_record().unwrap() {
        assert!(
            is_proto_message(&rec),
            "record {record_index} from C++ file should be valid proto"
        );

        // Verify field iteration matches the original record.
        let original = &records[record_index];
        let fields_from_cpp: Vec<ProtoField> = ProtoFieldIter::new(&rec)
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        let fields_from_original: Vec<ProtoField> = ProtoFieldIter::new(original)
            .collect::<Result<Vec<_>, _>>()
            .unwrap();

        assert_eq!(
            fields_from_cpp.len(),
            fields_from_original.len(),
            "record {record_index}: field count mismatch"
        );

        for (j, (cpp_field, orig_field)) in fields_from_cpp
            .iter()
            .zip(fields_from_original.iter())
            .enumerate()
        {
            assert_eq!(
                cpp_field.field_number, orig_field.field_number,
                "record {record_index}, field {j}: field number mismatch"
            );
            assert_eq!(
                cpp_field.wire_type, orig_field.wire_type,
                "record {record_index}, field {j}: wire type mismatch"
            );
            assert_eq!(
                cpp_field.value, orig_field.value,
                "record {record_index}, field {j}: value mismatch"
            );
        }

        record_index += 1;
    }

    assert_eq!(record_index, 100);
}

#[test]
fn criterion_29_6_cpp_transpose_field_iteration() {
    // Same test with transpose encoding.
    let records: Vec<Vec<u8>> = (0..50u64).map(build_test_message).collect();

    let mut cpp_writer = FfiWriter::new(
        FfiWriterOptions::new()
            .transpose(true)
            .compression(Compression::None),
    )
    .unwrap();
    for rec in &records {
        cpp_writer.write_record(rec).unwrap();
    }
    let cpp_file = cpp_writer.close().unwrap();

    let mut reader = RecordReader::new(Cursor::new(&cpp_file), ReaderOptions::new()).unwrap();
    let mut record_index = 0usize;

    while let Some(rec) = reader.read_record().unwrap() {
        let original = &records[record_index];

        let fields_from_cpp: Vec<ProtoField> = ProtoFieldIter::new(&rec)
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        let fields_from_original: Vec<ProtoField> = ProtoFieldIter::new(original)
            .collect::<Result<Vec<_>, _>>()
            .unwrap();

        assert_eq!(
            fields_from_cpp, fields_from_original,
            "record {record_index}: field iteration mismatch (transpose)"
        );

        record_index += 1;
    }

    assert_eq!(record_index, 50);
}

// ===========================================================================
// Criterion 29.7: Round-trip (iterate -> write) of messages written by C++
//   riegeli produces byte-identical output to the original, verified via FFI.
// ===========================================================================

#[test]
fn criterion_29_7_roundtrip_iterate_write_byte_identical() {
    // Write records via C++ FFI writer.
    let records: Vec<Vec<u8>> = (0..100u64).map(build_test_message).collect();

    let mut cpp_writer =
        FfiWriter::new(FfiWriterOptions::new().compression(Compression::None)).unwrap();
    for rec in &records {
        cpp_writer.write_record(rec).unwrap();
    }
    let cpp_file = cpp_writer.close().unwrap();

    // Read from C++ file, iterate+rewrite each record, write to new file.
    let mut reader = RecordReader::new(Cursor::new(&cpp_file), ReaderOptions::new()).unwrap();
    let mut rewritten_records = Vec::new();

    while let Some(rec) = reader.read_record().unwrap() {
        // Round-trip: iterate all fields and rewrite them.
        let mut writer = SerializedMessageWriter::new();
        for field_result in ProtoFieldIter::new(&rec) {
            let field = field_result.unwrap();
            writer.write_field(&field).unwrap();
        }
        let rewritten = writer.finish().unwrap();

        // Verify byte-identical.
        assert_eq!(
            rewritten, rec,
            "round-trip produced different bytes for a record"
        );

        rewritten_records.push(rewritten);
    }

    assert_eq!(rewritten_records.len(), 100);

    // Write the rewritten records to a new riegeli file via Rust.
    let new_file = rust_write_records(&rewritten_records, WriterOptions::new());

    // Verify via C++ FFI reader that the new file is readable and matches.
    let mut cpp_reader = FfiReader::new(&new_file).unwrap();
    for (i, expected) in records.iter().enumerate() {
        let rec = cpp_reader
            .read_record()
            .unwrap()
            .unwrap_or_else(|| panic!("C++ reader: expected record {i}, got EOF"));
        assert_eq!(
            rec, *expected,
            "C++ reader: record {i} mismatch after round-trip"
        );
    }
    assert!(cpp_reader.read_record().unwrap().is_none());
    cpp_reader.close().unwrap();
}

#[test]
fn criterion_29_7_roundtrip_with_complex_messages() {
    // Build more complex messages with nested submessages.
    let mut records = Vec::new();
    for i in 0..50u64 {
        let mut w = SerializedMessageWriter::new();
        w.write_uint64(1, i).unwrap();
        w.write_sint64(2, -(i as i64)).unwrap();
        w.write_fixed64(3, i * 1000).unwrap();
        w.write_bytes(4, &vec![i as u8; (i % 10 + 1) as usize])
            .unwrap();
        w.write_bool(5, i % 2 == 0).unwrap();
        records.push(w.finish().unwrap());
    }

    let mut cpp_writer =
        FfiWriter::new(FfiWriterOptions::new().compression(Compression::None)).unwrap();
    for rec in &records {
        cpp_writer.write_record(rec).unwrap();
    }
    let cpp_file = cpp_writer.close().unwrap();

    // Read and round-trip each record.
    let mut reader = RecordReader::new(Cursor::new(&cpp_file), ReaderOptions::new()).unwrap();
    let mut idx = 0;
    while let Some(rec) = reader.read_record().unwrap() {
        let mut writer = SerializedMessageWriter::new();
        for field_result in ProtoFieldIter::new(&rec) {
            writer.write_field(&field_result.unwrap()).unwrap();
        }
        let rewritten = writer.finish().unwrap();
        assert_eq!(
            rewritten, rec,
            "complex message round-trip mismatch at record {idx}"
        );
        idx += 1;
    }
    assert_eq!(idx, 50);
}

// ===========================================================================
// Additional adversarial tests
// ===========================================================================

#[test]
fn adversarial_empty_file_no_records() {
    let file_bytes = rust_write_records(&[], WriterOptions::new());
    let mut reader = RecordReader::new(Cursor::new(&file_bytes), ReaderOptions::new()).unwrap();
    let values = extract_varint_column(&mut reader, 1).unwrap();
    assert!(values.is_empty());
}

#[test]
fn adversarial_all_non_proto_records() {
    let records: Vec<Vec<u8>> = (0..10)
        .map(|i| format!("plain text {i}").into_bytes())
        .collect();
    let file_bytes = rust_write_records(&records, WriterOptions::new());

    let mut reader = RecordReader::new(Cursor::new(&file_bytes), ReaderOptions::new()).unwrap();
    let values = extract_varint_column(&mut reader, 1).unwrap();
    assert!(
        values.is_empty(),
        "non-proto records should yield no varint values"
    );
}

#[test]
fn adversarial_mixed_proto_non_proto_columnar() {
    let mut records = Vec::new();
    for i in 0..10u64 {
        if i % 3 == 0 {
            records.push(format!("text_{i}").into_bytes());
        } else {
            records.push(build_test_message(i));
        }
    }
    let file_bytes = rust_write_records(&records, WriterOptions::new());

    let mut reader = RecordReader::new(Cursor::new(&file_bytes), ReaderOptions::new()).unwrap();
    let values = extract_varint_column(&mut reader, 1).unwrap();
    // Non-proto records at indices 0, 3, 6, 9 should be skipped.
    // Proto records at indices 1, 2, 4, 5, 7, 8 with values 1, 2, 4, 5, 7, 8.
    assert_eq!(values, vec![1, 2, 4, 5, 7, 8]);
}

#[test]
fn adversarial_filter_preserves_empty_proto_records() {
    // An empty byte slice is a valid proto message.
    let records = vec![vec![], build_test_message(1), vec![]];
    let file_bytes = rust_write_records(&records, WriterOptions::new());

    let mut output_buf = Cursor::new(Vec::new());
    {
        let mut reader = RecordReader::new(Cursor::new(&file_bytes), ReaderOptions::new()).unwrap();
        let mut writer = RecordWriter::new(&mut output_buf, WriterOptions::new()).unwrap();
        filter_fields_to_writer(&mut reader, &mut writer, &[1]).unwrap();
        writer.flush().unwrap();
    }
    let filtered_records = rust_read_records(&output_buf.into_inner());
    assert_eq!(filtered_records.len(), 3);
    // Empty records should remain empty after filtering.
    assert!(filtered_records[0].is_empty());
    assert!(filtered_records[2].is_empty());
}

#[test]
fn adversarial_stream_error_display() {
    let err = StreamError {
        record_index: 42,
        source: riegeli::RiegeliError::MalformedData("test".to_string()),
    };
    let s = format!("{err}");
    assert!(s.contains("record index 42"));
    assert!(s.contains("test"));
}
