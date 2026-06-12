//! Integration tests for file metadata (RecordsMetadata) read and write.

// ---------------------------------------------------------------------------
// Re-implement make_records() logic from the bench (we can't import bench code)
// ---------------------------------------------------------------------------

const NUM_RECORDS: usize = 10_000;
const RECORD_SIZE: usize = 1024;
const TOTAL_BYTES: u64 = (NUM_RECORDS * RECORD_SIZE) as u64;

fn make_records() -> Vec<Vec<u8>> {
    (0..NUM_RECORDS)
        .map(|i| {
            let mut rec = Vec::with_capacity(RECORD_SIZE);
            rec.push(0x08);
            let mut v = i as u64;
            loop {
                if v < 0x80 {
                    rec.push(v as u8);
                    break;
                }
                rec.push((v as u8 & 0x7f) | 0x80);
                v >>= 7;
            }
            rec.push(0x12);
            let remaining = RECORD_SIZE.saturating_sub(rec.len() + 2);
            let len = remaining;
            if len < 0x80 {
                rec.push(len as u8);
            } else {
                rec.push((len as u8 & 0x7f) | 0x80);
                rec.push((len >> 7) as u8);
            }
            let pattern: [u8; 8] = [
                (i & 0xff) as u8,
                ((i >> 8) & 0xff) as u8,
                0xAA,
                0x55,
                (i & 0xff) as u8,
                0xBB,
                0x33,
                ((i >> 4) & 0xff) as u8,
            ];
            let fill_len = RECORD_SIZE.saturating_sub(rec.len());
            for j in 0..fill_len {
                rec.push(pattern[j % pattern.len()]);
            }
            rec.truncate(RECORD_SIZE);
            while rec.len() < RECORD_SIZE {
                rec.push(0);
            }
            rec
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Adversarial 16.1: make_records() produces exactly the right count and size
// ---------------------------------------------------------------------------

/// Each generated record must be exactly RECORD_SIZE bytes.
/// A size mismatch would invalidate the throughput calculation (TOTAL_BYTES
/// would not match actual payload, making MB/s numbers misleading).
#[test]
fn adv_16_make_records_size_invariant() {
    let records = make_records();
    assert_eq!(
        records.len(),
        NUM_RECORDS,
        "expected {NUM_RECORDS} records, got {}",
        records.len()
    );
    for (i, rec) in records.iter().enumerate() {
        assert_eq!(
            rec.len(),
            RECORD_SIZE,
            "record[{i}] has length {} != {RECORD_SIZE}",
            rec.len()
        );
    }
    // Sanity: total bytes equals TOTAL_BYTES constant
    let actual_total: u64 = records.iter().map(|r| r.len() as u64).sum();
    assert_eq!(actual_total, TOTAL_BYTES);
}

/// Records must not all be identical — distinct records exercise the compressor
/// more realistically and exercise transpose field splitting.
#[test]
fn adv_16_make_records_are_distinct() {
    let records = make_records();
    // Check first vs last — they should differ (different index varint)
    assert_ne!(
        records[0],
        records[NUM_RECORDS - 1],
        "first and last records should differ"
    );
    // Spot-check a handful of pairs
    for i in [0usize, 1, 100, 999, 9999] {
        if i + 1 < NUM_RECORDS {
            assert_ne!(
                records[i],
                records[i + 1],
                "records[{i}] and records[{i}+1] are identical"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Cross-language metadata wire format: the FileMetadata chunk carries a
// transpose-encoded RecordsMetadata (with num_records = 0 and
// decoded_data_size = serialized size), so metadata written by either
// implementation must be readable by the other.
// ---------------------------------------------------------------------------

fn sample_serialized_metadata() -> Vec<u8> {
    // A small but non-trivial serialized proto payload:
    // field 1 (record_type_name): "some.package.Message"
    let mut bytes = vec![0x0A, 20];
    bytes.extend_from_slice(b"some.package.Message");
    bytes
}

#[test]
fn metadata_written_by_rust_is_readable_by_cpp() {
    use std::io::Cursor;

    let metadata = sample_serialized_metadata();
    let mut buf = Cursor::new(Vec::<u8>::new());
    {
        let mut w = riegeli::RecordWriter::new(
            &mut buf,
            riegeli::WriterOptions::new().set_serialized_metadata(metadata.clone()),
        )
        .expect("rust writer");
        w.write_record(b"payload-record").expect("write_record");
        w.flush().expect("flush");
    }
    let file_bytes = buf.into_inner();

    let mut reader = riegeli_ffi::RecordReader::new(&file_bytes).expect("cpp reader");
    let got = reader
        .read_serialized_metadata()
        .expect("cpp read_serialized_metadata");
    assert_eq!(
        got.as_deref(),
        Some(metadata.as_slice()),
        "metadata read back by the reference reader differs"
    );
    // Records must still be readable after the metadata chunk.
    let rec = reader.read_record().expect("cpp read_record");
    assert_eq!(rec.as_deref(), Some(&b"payload-record"[..]));
}

// The reference writer compresses the metadata chunk with its default codec
// (Brotli), so decoding it requires the brotli feature.
#[test]
#[cfg(feature = "brotli")]
fn metadata_written_by_cpp_is_readable_by_rust() {
    use std::io::Cursor;

    let metadata = sample_serialized_metadata();
    // Leave the reference writer's default compression in place: the rust
    // reader must handle a compressed transpose-encoded metadata chunk.
    let mut w = riegeli_ffi::RecordWriter::new(
        riegeli_ffi::WriterOptions::new().serialized_metadata(&metadata),
    )
    .expect("cpp writer");
    w.write_record(b"payload-record").expect("cpp write_record");
    let file_bytes = w.close().expect("cpp close");

    let mut reader =
        riegeli::RecordReader::new(Cursor::new(file_bytes), riegeli::ReaderOptions::new())
            .expect("rust reader");
    let got = reader
        .read_serialized_metadata()
        .expect("rust read_serialized_metadata");
    assert_eq!(
        got.as_deref(),
        Some(metadata.as_slice()),
        "metadata read back by the rust reader differs"
    );
    let rec = reader.read_record().expect("rust read_record");
    assert_eq!(rec.as_deref(), Some(&b"payload-record"[..]));
}

// Uncompressed variant of the test above: covers the C++-written metadata
// read path in builds without the brotli feature.
#[test]
fn metadata_written_by_cpp_uncompressed_is_readable_by_rust() {
    use std::io::Cursor;

    let metadata = sample_serialized_metadata();
    let mut w = riegeli_ffi::RecordWriter::new(
        riegeli_ffi::WriterOptions::new()
            .compression(riegeli_ffi::Compression::None)
            .serialized_metadata(&metadata),
    )
    .expect("cpp writer");
    w.write_record(b"payload-record").expect("cpp write_record");
    let file_bytes = w.close().expect("cpp close");

    let mut reader =
        riegeli::RecordReader::new(Cursor::new(file_bytes), riegeli::ReaderOptions::new())
            .expect("rust reader");
    let got = reader
        .read_serialized_metadata()
        .expect("rust read_serialized_metadata");
    assert_eq!(
        got.as_deref(),
        Some(metadata.as_slice()),
        "metadata read back by the rust reader differs"
    );
    let rec = reader.read_record().expect("rust read_record");
    assert_eq!(rec.as_deref(), Some(&b"payload-record"[..]));
}

#[test]
fn metadata_file_bytes_match_cpp_writer() {
    use std::io::Cursor;

    // With identical options the metadata chunk must be byte-identical to the
    // reference writer's output (transpose-encoded chunk data, num_records 0).
    let metadata = sample_serialized_metadata();
    let mut buf = Cursor::new(Vec::<u8>::new());
    {
        let mut w = riegeli::RecordWriter::new(
            &mut buf,
            riegeli::WriterOptions::new().set_serialized_metadata(metadata.clone()),
        )
        .expect("rust writer");
        w.write_record(b"payload-record").expect("write_record");
        w.flush().expect("flush");
    }
    let rust_bytes = buf.into_inner();

    let mut w = riegeli_ffi::RecordWriter::new(
        riegeli_ffi::WriterOptions::new()
            .compression(riegeli_ffi::Compression::None)
            .serialized_metadata(&metadata),
    )
    .expect("cpp writer");
    w.write_record(b"payload-record").expect("cpp write_record");
    let cpp_bytes = w.close().expect("cpp close");

    assert_eq!(rust_bytes, cpp_bytes, "file bytes differ from reference");
}
