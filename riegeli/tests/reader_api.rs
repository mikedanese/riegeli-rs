//! Integration tests for RecordReader API: size(), seek_back(), check_file_format().

use std::io::Cursor;

use riegeli::{
    CompressionType, ReaderOptions, RecordReader, RecordWriter, RiegeliError, WriterOptions,
};

/// Write records to a Vec<u8> and return the bytes.
fn write_records(records: &[&[u8]], opts: WriterOptions) -> Vec<u8> {
    let mut buf = Cursor::new(Vec::<u8>::new());
    {
        let mut w = RecordWriter::new(&mut buf, opts).expect("writer new");
        for rec in records {
            w.write_record(rec).expect("write_record");
        }
        w.close().expect("close");
    }
    buf.into_inner()
}

/// Open a RecordReader on a Vec<u8>.
fn open_reader(data: Vec<u8>) -> RecordReader<Cursor<Vec<u8>>> {
    RecordReader::new(Cursor::new(data), ReaderOptions::new()).expect("reader new")
}

// ─── Criterion 1 ───────────────────────────────────────────────────────────────
/// A file written with set_metadata is read back via read_metadata().
#[test]
fn criterion_1_set_metadata_round_trip() {
    let payload = b"test-schema".to_vec();
    let data = write_records(
        &[b"record1", b"record2"],
        WriterOptions::new().set_serialized_metadata(payload.clone()),
    );

    let mut reader = open_reader(data);
    let meta = reader.read_serialized_metadata().expect("read_metadata");
    assert_eq!(meta, Some(payload));
}

// ─── Criterion 3 ───────────────────────────────────────────────────────────────
/// last_record_is_valid() is true after a valid read, false after recovery.
#[test]
fn criterion_3_last_record_is_valid() {
    // Part A: valid reads set last_record_is_valid = true.
    let data = write_records(&[b"a", b"b", b"c"], WriterOptions::new());
    let mut reader = open_reader(data);

    assert!(
        reader.last_record_is_valid(),
        "should be valid before any read"
    );
    reader.read_record().expect("read 1").expect("got record");
    assert!(
        reader.last_record_is_valid(),
        "should be valid after valid read"
    );
    reader.read_record().expect("read 2").expect("got record");
    assert!(reader.last_record_is_valid(), "should still be valid");

    // Part B: recovery callback sets last_record_is_valid = false.
    let mut raw = write_records(&[b"x", b"y", b"z"], WriterOptions::new());

    // Corrupt a byte in the chunk data (after the header at offset 64).
    let corrupt_offset = 64 + 40 + 2;
    if corrupt_offset < raw.len() {
        raw[corrupt_offset] ^= 0xFF;
    }

    let recovery_fired = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let recovery_fired_clone = recovery_fired.clone();
    let opts = ReaderOptions::new().recovery(move |_pos, _err| {
        recovery_fired_clone.store(true, std::sync::atomic::Ordering::SeqCst);
    });

    let mut reader2 = RecordReader::new(Cursor::new(raw), opts).expect("reader new");
    let _ = reader2.read_record();

    assert!(
        recovery_fired.load(std::sync::atomic::Ordering::SeqCst),
        "recovery callback should have fired"
    );
    assert!(
        !reader2.last_record_is_valid(),
        "last_record_is_valid should be false after recovery"
    );
}

// ─── Criterion 4 ───────────────────────────────────────────────────────────────
/// seek_back() after reading record 5 of 10 returns Ok(true) and re-reads record 5.
#[test]
fn criterion_4_seek_back_returns_previous_record() {
    let records: Vec<Vec<u8>> = (0u64..10).map(|i| i.to_le_bytes().to_vec()).collect();
    let record_refs: Vec<&[u8]> = records.iter().map(|v| v.as_slice()).collect();
    let data = write_records(&record_refs, WriterOptions::new());
    let mut reader = open_reader(data);

    // Read 5 records.
    let mut fifth = None;
    for i in 0..5 {
        let rec = reader.read_record().expect("read").expect("got record");
        if i == 4 {
            fifth = Some(rec);
        }
    }
    let fifth = fifth.unwrap();

    // seek_back() should return Ok(true).
    let result = reader.seek_back().expect("seek_back");
    assert!(result, "seek_back should return true");

    // The next read should return the 5th record again.
    let re_read = reader.read_record().expect("re-read").expect("got record");
    assert_eq!(re_read, fifth, "seek_back should re-read the 5th record");
}

// ─── Criterion 5 ───────────────────────────────────────────────────────────────
/// seek_back() at the very first record returns Ok(false).
#[test]
fn criterion_5_seek_back_at_start_returns_false() {
    let data = write_records(&[b"first", b"second"], WriterOptions::new());
    let mut reader = open_reader(data);

    // Before reading anything, seek_back() should return false.
    let result = reader.seek_back().expect("seek_back");
    assert!(!result, "seek_back before any read should return false");
}

// ─── Criterion 6 ───────────────────────────────────────────────────────────────
/// size() returns 1000 for a 1000-record file and does not change the read position.
#[test]
fn criterion_6_size_returns_count_and_preserves_position() {
    let records: Vec<Vec<u8>> = (0u32..1000).map(|i| i.to_le_bytes().to_vec()).collect();
    let record_refs: Vec<&[u8]> = records.iter().map(|v| v.as_slice()).collect();
    let data = write_records(&record_refs, WriterOptions::new().chunk_size(1024));
    let mut reader = open_reader(data);

    // Read 5 records to establish a position.
    for _ in 0..5 {
        reader.read_record().expect("read").expect("got record");
    }

    // Record what the 6th record will be.
    let expected_next = reader.read_record().expect("peek 6").expect("got record");

    // Now seek back to before the 6th.
    reader.seek_back().expect("seek_back");

    // Call size() — should return 1000.
    let total = reader.size().expect("size");
    assert_eq!(total, 1000, "size() should return 1000");

    // After size(), the next read_record() should return the same record as before.
    let after_size = reader
        .read_record()
        .expect("after size")
        .expect("got record");
    assert_eq!(
        after_size, expected_next,
        "size() must not change the current read position"
    );
}

// ─── Criterion 7 ───────────────────────────────────────────────────────────────
/// check_file_format() returns Ok on a valid file, Err on corrupted data hash.
#[test]
fn criterion_7_check_file_format_valid_and_corrupted() {
    // Part A: valid file.
    let data = write_records(&[b"hello", b"world"], WriterOptions::new());
    let mut reader = open_reader(data);
    reader
        .check_file_format()
        .expect("check_file_format should succeed on valid file");

    // Part B: corrupt a byte in the chunk data (after the header at offset 64+40).
    let mut corrupted2 = write_records(&[b"foo", b"bar"], WriterOptions::new());
    let data_payload_pos = 64 + 40; // first byte of chunk data
    if data_payload_pos < corrupted2.len() {
        corrupted2[data_payload_pos] ^= 0xFF;
    }

    let mut reader2 = open_reader(corrupted2);
    let result = reader2.check_file_format();
    assert!(
        result.is_err(),
        "check_file_format should fail on corrupted data"
    );
    match result {
        Err(RiegeliError::MalformedData(_)) => {}
        Err(e) => panic!("expected MalformedData, got: {e:?}"),
        Ok(_) => panic!("expected error"),
    }
}

// ─── Bonus: metadata does not break normal record reading ─────────────────────
#[test]
fn metadata_file_records_readable() {
    let records = vec![b"foo".as_ref(), b"bar".as_ref(), b"baz".as_ref()];
    let data = write_records(
        &records,
        WriterOptions::new().set_serialized_metadata(b"my-schema".to_vec()),
    );
    let mut reader = open_reader(data);

    for expected in &records {
        let got = reader.read_record().expect("read").expect("got record");
        assert_eq!(got, *expected);
    }
    assert_eq!(reader.read_record().expect("eof"), None);
}

// ─── Bonus: size() on empty file returns 0 ────────────────────────────────────
#[test]
fn size_empty_file() {
    let data = write_records(&[], WriterOptions::new());
    let mut reader = open_reader(data);
    let total = reader.size().expect("size");
    assert_eq!(total, 0);
}

// ─── Bonus: size() with transpose encoding ───────────────────────────────────
#[test]
fn size_transpose_encoding() {
    let records: Vec<Vec<u8>> = (0u32..50)
        .map(|i| {
            let mut rec = vec![0x08];
            let mut v = i as u64;
            loop {
                let b = (v & 0x7F) as u8;
                v >>= 7;
                if v == 0 {
                    rec.push(b);
                    break;
                }
                rec.push(b | 0x80);
            }
            rec
        })
        .collect();
    let record_refs: Vec<&[u8]> = records.iter().map(|v| v.as_slice()).collect();
    let data = write_records(
        &record_refs,
        WriterOptions::new().transpose(true).chunk_size(512),
    );
    let mut reader = open_reader(data);
    let total = reader.size().expect("size");
    assert_eq!(total, 50);
}

// ─── Bonus: check_file_format on file with metadata chunk ────────────────────
#[test]
fn check_file_format_with_metadata() {
    let data = write_records(
        &[b"hello"],
        WriterOptions::new().set_serialized_metadata(b"meta".to_vec()),
    );
    let mut reader = open_reader(data);
    reader
        .check_file_format()
        .expect("check_file_format should succeed on file with metadata chunk");
}

// ─── Bonus: brotli-compressed file check_file_format ─────────────────────────
#[test]
fn check_file_format_brotli() {
    let data = write_records(
        &[b"hello world, this is a test record with some length to trigger compression"],
        WriterOptions::new().compression(CompressionType::Brotli),
    );
    let mut reader = open_reader(data);
    reader
        .check_file_format()
        .expect("check_file_format on Brotli file");
}
