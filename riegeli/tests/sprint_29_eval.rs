//! Sprint 29 Evaluator adversarial tests.
//!
//! These tests probe edge cases, off-by-one errors, and boundary conditions
//! in the proto streaming module.

mod common;

use std::cell::{Cell, RefCell};
use std::io::Cursor;

use riegeli::proto_stream::{
    StreamError, extract_varint_column, filter_fields_to_writer, for_each_proto_record,
};
use riegeli::proto_wire::{
    DynamicHandlerSet, FieldValue, ProtoField, ProtoFieldIter, SerializedMessageWriter,
};
use riegeli::{ReaderOptions, RecordReader, RecordWriter, WriterOptions};
use riegeli_ffi::{
    Compression, RecordReader as FfiReader, RecordWriter as FfiWriter,
    WriterOptions as FfiWriterOptions,
};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn build_test_message(i: u64) -> Vec<u8> {
    let mut w = SerializedMessageWriter::new();
    w.write_uint64(1, i).unwrap();
    w.write_bytes(2, format!("name_{i}").as_bytes()).unwrap();
    w.write_fixed32(3, (i as u32).wrapping_mul(7)).unwrap();
    w.finish().unwrap()
}

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

fn rust_read_records(data: &[u8]) -> Vec<Vec<u8>> {
    let mut reader = RecordReader::new(Cursor::new(data), ReaderOptions::new()).unwrap();
    let mut out = Vec::new();
    while let Some(rec) = reader.read_record().unwrap() {
        out.push(rec);
    }
    out
}

// ===========================================================================
// Edge case: zero records (empty file)
// ===========================================================================

#[test]
fn eval_29_empty_file_for_each() {
    let file_bytes = rust_write_records(&[], WriterOptions::new());
    let mut reader = RecordReader::new(Cursor::new(&file_bytes), ReaderOptions::new()).unwrap();

    let count = Cell::new(0usize);
    let mut handlers = DynamicHandlerSet::new();
    handlers.on_varint(1, |_v| {
        count.set(count.get() + 1);
        Ok(())
    });

    for_each_proto_record::<_, _, fn(usize, &[u8])>(&mut reader, &mut handlers, None).unwrap();
    assert_eq!(count.get(), 0);
}

#[test]
fn eval_29_empty_file_filter_fields() {
    let file_bytes = rust_write_records(&[], WriterOptions::new());
    let mut output_buf = Cursor::new(Vec::new());
    {
        let mut reader = RecordReader::new(Cursor::new(&file_bytes), ReaderOptions::new()).unwrap();
        let mut writer = RecordWriter::new(&mut output_buf, WriterOptions::new()).unwrap();
        filter_fields_to_writer(&mut reader, &mut writer, &[1, 2]).unwrap();
        writer.flush().unwrap();
    }
    let filtered = rust_read_records(&output_buf.into_inner());
    assert!(filtered.is_empty());
}

// ===========================================================================
// Mixed proto and non-proto records with for_each_proto_record
// ===========================================================================

#[test]
fn eval_29_mixed_records_for_each() {
    // Interleave: non-proto, proto, non-proto, proto, non-proto
    let mut records = Vec::new();
    records.push(b"not a proto".to_vec()); // 0: non-proto
    records.push(build_test_message(100)); // 1: proto
    records.push(vec![0xFF, 0xFF, 0xFF]); // 2: non-proto (invalid tag)
    records.push(build_test_message(200)); // 3: proto
    records.push(b"also not proto".to_vec()); // 4: non-proto

    let file_bytes = rust_write_records(&records, WriterOptions::new());
    let mut reader = RecordReader::new(Cursor::new(&file_bytes), ReaderOptions::new()).unwrap();

    let proto_vals = RefCell::new(Vec::new());
    let fallback_indices = RefCell::new(Vec::new());

    let mut handlers = DynamicHandlerSet::new();
    handlers.on_varint(1, |v| {
        proto_vals.borrow_mut().push(v);
        Ok(())
    });

    let mut fallback = |idx: usize, _data: &[u8]| {
        fallback_indices.borrow_mut().push(idx);
    };

    for_each_proto_record(&mut reader, &mut handlers, Some(&mut fallback)).unwrap();

    assert_eq!(*proto_vals.borrow(), vec![100u64, 200]);
    assert_eq!(*fallback_indices.borrow(), vec![0, 2, 4]);
}

// ===========================================================================
// Error at record 0, middle, and last record -- verify exact index
// ===========================================================================

#[test]
fn eval_29_error_at_record_0() {
    let records: Vec<Vec<u8>> = (0..5u64).map(build_test_message).collect();
    let file_bytes = rust_write_records(&records, WriterOptions::new());
    let mut reader = RecordReader::new(Cursor::new(&file_bytes), ReaderOptions::new()).unwrap();

    let mut handlers = DynamicHandlerSet::new();
    handlers.on_varint(1, |_v| {
        Err(riegeli::RiegeliError::MalformedData("fail at 0".into()))
    });

    let err = for_each_proto_record::<_, _, fn(usize, &[u8])>(&mut reader, &mut handlers, None)
        .unwrap_err();
    assert_eq!(err.record_index, 0, "error should be at record 0");
}

#[test]
fn eval_29_error_at_last_record() {
    let n = 10u64;
    let records: Vec<Vec<u8>> = (0..n).map(build_test_message).collect();
    let file_bytes = rust_write_records(&records, WriterOptions::new());
    let mut reader = RecordReader::new(Cursor::new(&file_bytes), ReaderOptions::new()).unwrap();

    let mut handlers = DynamicHandlerSet::new();
    handlers.on_varint(1, |v| {
        if v == n - 1 {
            Err(riegeli::RiegeliError::MalformedData("fail at last".into()))
        } else {
            Ok(())
        }
    });

    let err = for_each_proto_record::<_, _, fn(usize, &[u8])>(&mut reader, &mut handlers, None)
        .unwrap_err();
    assert_eq!(
        err.record_index,
        (n - 1) as usize,
        "error should be at last record index"
    );
}

#[test]
fn eval_29_error_at_middle_record() {
    let n = 20u64;
    let records: Vec<Vec<u8>> = (0..n).map(build_test_message).collect();
    let file_bytes = rust_write_records(&records, WriterOptions::new());
    let mut reader = RecordReader::new(Cursor::new(&file_bytes), ReaderOptions::new()).unwrap();

    let target = n / 2;
    let mut handlers = DynamicHandlerSet::new();
    handlers.on_varint(1, |v| {
        if v == target {
            Err(riegeli::RiegeliError::MalformedData("fail at mid".into()))
        } else {
            Ok(())
        }
    });

    let err = for_each_proto_record::<_, _, fn(usize, &[u8])>(&mut reader, &mut handlers, None)
        .unwrap_err();
    assert_eq!(err.record_index, target as usize);
}

// ===========================================================================
// Extract column for field that doesn't exist in any record
// ===========================================================================

#[test]
fn eval_29_extract_nonexistent_field() {
    let records: Vec<Vec<u8>> = (0..100u64).map(build_test_message).collect();
    let file_bytes = rust_write_records(&records, WriterOptions::new());
    let mut reader = RecordReader::new(Cursor::new(&file_bytes), ReaderOptions::new()).unwrap();

    // Field 999 doesn't exist in any record.
    let values = extract_varint_column(&mut reader, 999).unwrap();
    assert!(
        values.is_empty(),
        "extracting nonexistent field should yield empty vec"
    );
}

// ===========================================================================
// Filter fields from message with only unmatched fields -> empty records
// ===========================================================================

#[test]
fn eval_29_filter_all_fields_unmatched() {
    // build_test_message has fields 1, 2, 3. Filter to {99, 100} -> empty records.
    let records: Vec<Vec<u8>> = (0..10u64).map(build_test_message).collect();
    let file_bytes = rust_write_records(&records, WriterOptions::new());

    let mut output_buf = Cursor::new(Vec::new());
    {
        let mut reader = RecordReader::new(Cursor::new(&file_bytes), ReaderOptions::new()).unwrap();
        let mut writer = RecordWriter::new(&mut output_buf, WriterOptions::new()).unwrap();
        filter_fields_to_writer(&mut reader, &mut writer, &[99, 100]).unwrap();
        writer.flush().unwrap();
    }
    let filtered = rust_read_records(&output_buf.into_inner());
    assert_eq!(filtered.len(), 10, "should still have 10 records");
    for (i, rec) in filtered.iter().enumerate() {
        assert!(rec.is_empty(), "record {i} should be empty after filtering");
    }
}

// ===========================================================================
// Verify record index in error is exact (not off-by-one) with non-proto gaps
// ===========================================================================

#[test]
fn eval_29_record_index_with_non_proto_gaps() {
    // Mix: [non-proto, proto(0), non-proto, proto(1), proto(2)]
    // Error at proto value=1, which is record index 3
    let mut records = Vec::new();
    records.push(b"text".to_vec()); // 0
    records.push(build_test_message(0)); // 1
    records.push(b"text2".to_vec()); // 2
    records.push(build_test_message(1)); // 3 -- error here
    records.push(build_test_message(2)); // 4

    let file_bytes = rust_write_records(&records, WriterOptions::new());
    let mut reader = RecordReader::new(Cursor::new(&file_bytes), ReaderOptions::new()).unwrap();

    let mut handlers = DynamicHandlerSet::new();
    handlers.on_varint(1, |v| {
        if v == 1 {
            Err(riegeli::RiegeliError::MalformedData("boom".into()))
        } else {
            Ok(())
        }
    });

    let mut fallback = |_idx: usize, _data: &[u8]| {};
    let err = for_each_proto_record(&mut reader, &mut handlers, Some(&mut fallback)).unwrap_err();
    assert_eq!(
        err.record_index, 3,
        "record index should be 3 (accounting for non-proto records)"
    );
}

// ===========================================================================
// Large record count -- verify no unexpected memory growth
// ===========================================================================

#[test]
fn eval_29_large_record_count_columnar_extraction() {
    let n = 5000u64;
    let records: Vec<Vec<u8>> = (0..n)
        .map(|i| {
            let mut w = SerializedMessageWriter::new();
            w.write_uint64(1, i).unwrap();
            w.finish().unwrap()
        })
        .collect();
    let file_bytes = rust_write_records(&records, WriterOptions::new());

    let mut reader = RecordReader::new(Cursor::new(&file_bytes), ReaderOptions::new()).unwrap();
    let values = extract_varint_column(&mut reader, 1).unwrap();
    assert_eq!(values.len(), n as usize);
    for (i, &v) in values.iter().enumerate() {
        assert_eq!(v, i as u64);
    }
}

// ===========================================================================
// FFI conformance: verify that criterion 29.6/29.7 tests actually exercise
// the C++ bridge (not just Rust-only round-trips)
// ===========================================================================

#[test]
fn eval_29_ffi_bridge_bidirectional() {
    // Write via C++ FFI
    let records: Vec<Vec<u8>> = (0..20u64).map(build_test_message).collect();

    let mut cpp_writer =
        FfiWriter::new(FfiWriterOptions::new().compression(Compression::None)).unwrap();
    for rec in &records {
        cpp_writer.write_record(rec).unwrap();
    }
    let cpp_file = cpp_writer.close().unwrap();

    // Read via Rust, round-trip each record
    let mut reader = RecordReader::new(Cursor::new(&cpp_file), ReaderOptions::new()).unwrap();
    let mut rewritten = Vec::new();
    while let Some(rec) = reader.read_record().unwrap() {
        let mut w = SerializedMessageWriter::new();
        for f in ProtoFieldIter::new(&rec) {
            w.write_field(&f.unwrap()).unwrap();
        }
        let rt = w.finish().unwrap();
        assert_eq!(rt, rec, "round-trip should be byte-identical");
        rewritten.push(rt);
    }
    assert_eq!(rewritten.len(), 20);

    // Write round-tripped records via Rust writer
    let rust_file = rust_write_records(&rewritten, WriterOptions::new());

    // Read back via C++ FFI reader
    let mut cpp_reader = FfiReader::new(&rust_file).unwrap();
    for (i, expected) in records.iter().enumerate() {
        let rec = cpp_reader.read_record().unwrap().unwrap();
        assert_eq!(rec, *expected, "C++ reader: mismatch at record {i}");
    }
    assert!(cpp_reader.read_record().unwrap().is_none());
    cpp_reader.close().unwrap();
}

// ===========================================================================
// for_each_proto_record with empty records (valid proto)
// ===========================================================================

#[test]
fn eval_29_for_each_empty_proto_records() {
    // Empty bytes are valid proto messages (no fields).
    let records = vec![vec![], vec![], build_test_message(42), vec![]];
    let file_bytes = rust_write_records(&records, WriterOptions::new());
    let mut reader = RecordReader::new(Cursor::new(&file_bytes), ReaderOptions::new()).unwrap();

    let varint_values = RefCell::new(Vec::new());
    let mut handlers = DynamicHandlerSet::new();
    handlers.on_varint(1, |v| {
        varint_values.borrow_mut().push(v);
        Ok(())
    });

    for_each_proto_record::<_, _, fn(usize, &[u8])>(&mut reader, &mut handlers, None).unwrap();
    // Only record index 2 has field 1
    assert_eq!(*varint_values.borrow(), vec![42u64]);
}

// ===========================================================================
// filter_fields_to_writer with non-proto records (pass-through)
// ===========================================================================

#[test]
fn eval_29_filter_non_proto_passthrough() {
    let mut records = Vec::new();
    records.push(build_test_message(1));
    records.push(b"plain text record".to_vec());
    records.push(build_test_message(2));

    let file_bytes = rust_write_records(&records, WriterOptions::new());
    let mut output_buf = Cursor::new(Vec::new());
    {
        let mut reader = RecordReader::new(Cursor::new(&file_bytes), ReaderOptions::new()).unwrap();
        let mut writer = RecordWriter::new(&mut output_buf, WriterOptions::new()).unwrap();
        filter_fields_to_writer(&mut reader, &mut writer, &[1]).unwrap();
        writer.flush().unwrap();
    }
    let filtered = rust_read_records(&output_buf.into_inner());
    assert_eq!(filtered.len(), 3);

    // Record 0: proto, only field 1
    let fields: Vec<ProtoField> = ProtoFieldIter::new(&filtered[0])
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert_eq!(fields.len(), 1);
    assert_eq!(fields[0].field_number, 1);

    // Record 1: non-proto, passed through unchanged
    assert_eq!(filtered[1], b"plain text record".to_vec());

    // Record 2: proto, only field 1
    let fields2: Vec<ProtoField> = ProtoFieldIter::new(&filtered[2])
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert_eq!(fields2.len(), 1);
    assert_eq!(fields2[0].field_number, 1);
}

// ===========================================================================
// StreamError implements std::error::Error with source chain
// ===========================================================================

#[test]
fn eval_29_stream_error_source_chain() {
    use std::error::Error;

    let err = StreamError {
        record_index: 7,
        source: riegeli::RiegeliError::MalformedData("inner error".to_string()),
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

// ===========================================================================
// Transpose encoding with filter_fields_to_writer and non-proto mix
// ===========================================================================

#[test]
fn eval_29_transpose_filter_with_non_proto_mix() {
    let mut records = Vec::new();
    records.push(build_test_message(10));
    records.push(b"not proto".to_vec());
    records.push(build_test_message(20));

    let file_bytes = rust_write_records(&records, WriterOptions::new().transpose(true));
    let mut output_buf = Cursor::new(Vec::new());
    {
        let mut reader = RecordReader::new(Cursor::new(&file_bytes), ReaderOptions::new()).unwrap();
        let mut writer = RecordWriter::new(&mut output_buf, WriterOptions::new()).unwrap();
        filter_fields_to_writer(&mut reader, &mut writer, &[1]).unwrap();
        writer.flush().unwrap();
    }
    let filtered = rust_read_records(&output_buf.into_inner());
    assert_eq!(filtered.len(), 3);

    // Verify proto records only have field 1
    for idx in [0, 2] {
        let fields: Vec<ProtoField> = ProtoFieldIter::new(&filtered[idx])
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert!(
            fields.iter().all(|f| f.field_number == 1),
            "record {idx}: should only have field 1"
        );
    }
    // Non-proto record passed through
    assert_eq!(filtered[1], b"not proto".to_vec());
}
