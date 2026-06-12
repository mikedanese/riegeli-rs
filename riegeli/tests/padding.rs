//! Integration tests for file padding alignment and padded-file concatenation.
use std::io::Cursor;

use riegeli::{
    CompressionType, ReaderOptions, RecordReader, RecordWriter, RiegeliError, WriterOptions,
};

/// Write records to a Vec<u8> and return the bytes.
fn write_records(records: &[&[u8]], opts: WriterOptions) -> Vec<u8> {
    let mut cursor = Cursor::new(Vec::<u8>::new());
    {
        let mut w = RecordWriter::new(&mut cursor, opts).expect("writer::new");
        for rec in records {
            w.write_record(rec).expect("write_record");
        }
        w.flush().expect("flush");
    }
    cursor.into_inner()
}

/// Read all records from a byte slice.
fn read_all(data: Vec<u8>) -> Vec<Vec<u8>> {
    let mut reader =
        RecordReader::new(Cursor::new(data), ReaderOptions::new()).expect("reader::new");
    let mut records = Vec::new();
    while let Some(rec) = reader.read_record().expect("read_record") {
        records.push(rec);
    }
    records
}

// Block size constant for padding assertions (matches the wire format spec).
const BLOCK_SIZE: u64 = 65536;

// -----------------------------------------------------------------------
// initial_padding(65536) produces file_size % 65536 == 0
// -----------------------------------------------------------------------

#[test]
fn initial_padding_aligns_empty_file() {
    let data = write_records(&[], WriterOptions::new().initial_padding(BLOCK_SIZE));
    assert_eq!(
        data.len() as u64 % BLOCK_SIZE,
        0,
        "empty file with initial_padding({BLOCK_SIZE}) must have size % {BLOCK_SIZE} == 0, got {}",
        data.len()
    );
}

#[test]
fn initial_padding_aligns_small_file() {
    let data = write_records(
        &[b"hello", b"world"],
        WriterOptions::new().initial_padding(BLOCK_SIZE),
    );
    assert_eq!(
        data.len() as u64 % BLOCK_SIZE,
        0,
        "small file with initial_padding({BLOCK_SIZE}) must have size % {BLOCK_SIZE} == 0, got {}",
        data.len()
    );
}

#[test]
fn initial_padding_aligns_multi_block_file() {
    // Write enough data to span multiple blocks.
    let record: Vec<u8> = vec![0xAB; 1000];
    let records: Vec<&[u8]> = (0..300).map(|_| record.as_slice()).collect();
    let data = write_records(&records, WriterOptions::new().initial_padding(BLOCK_SIZE));
    assert_eq!(
        data.len() as u64 % BLOCK_SIZE,
        0,
        "multi-block file with initial_padding({BLOCK_SIZE}) must have size % {BLOCK_SIZE} == 0, got {}",
        data.len()
    );
    // Verify records are still readable.
    let got = read_all(data);
    assert_eq!(got.len(), 300);
    for (i, rec) in got.iter().enumerate() {
        assert_eq!(rec.as_slice(), record.as_slice(), "record {i} mismatch");
    }
}

// -----------------------------------------------------------------------
// two padded files can be concatenated and read as one
// -----------------------------------------------------------------------

#[test]
fn concatenated_padded_files_readable() {
    let records_a: &[&[u8]] = &[b"file_a_rec1", b"file_a_rec2", b"file_a_rec3"];
    let records_b: &[&[u8]] = &[b"file_b_rec1", b"file_b_rec2"];

    let opts = WriterOptions::new().initial_padding(BLOCK_SIZE);
    let file_a = write_records(records_a, opts.clone());
    let file_b = write_records(records_b, opts);

    // Both files must be block-aligned.
    assert_eq!(
        file_a.len() as u64 % BLOCK_SIZE,
        0,
        "file_a not block-aligned"
    );
    assert_eq!(
        file_b.len() as u64 % BLOCK_SIZE,
        0,
        "file_b not block-aligned"
    );

    // Concatenate at byte level.
    let mut combined = file_a;
    combined.extend_from_slice(&file_b);

    // Read all records from the concatenated file.
    let got = read_all(combined);

    // Should contain records from both files.
    let expected: Vec<&[u8]> = records_a.iter().chain(records_b.iter()).copied().collect();
    assert_eq!(
        got.len(),
        expected.len(),
        "concatenated file: expected {} records, got {}",
        expected.len(),
        got.len()
    );
    for (i, (expected_rec, got_rec)) in expected.iter().zip(got.iter()).enumerate() {
        assert_eq!(
            got_rec.as_slice(),
            *expected_rec,
            "record {i} mismatch in concatenated file"
        );
    }
}

// -----------------------------------------------------------------------
// TryFrom<u8> for CompressionType (public API)
// -----------------------------------------------------------------------

/// Small alignments (below the 40-byte chunk-header size) must still land
/// exactly on an alignment multiple: the old single-step fallback saturated
/// the padding data size to zero and overshot the boundary.
#[test]
fn small_alignment_padding_lands_on_multiples() {
    for alignment in [8u64, 15, 16, 24, 32, 48, 64] {
        let mut buf = Cursor::new(Vec::<u8>::new());
        {
            let mut w = RecordWriter::new(&mut buf, WriterOptions::new().final_padding(alignment))
                .expect("writer new ok");
            w.write_record(b"x").expect("write ok");
            w.close().expect("close ok");
        }
        let data = buf.into_inner();
        assert_eq!(
            data.len() as u64 % alignment,
            0,
            "alignment {alignment}: file len {} not a multiple",
            data.len()
        );
        let mut reader =
            RecordReader::new(Cursor::new(data), ReaderOptions::new()).expect("reader ok");
        assert_eq!(
            reader.read_record().expect("read ok").as_deref(),
            Some(&b"x"[..])
        );
        assert_eq!(reader.read_record().expect("read ok"), None);
    }
}

#[test]
fn compression_type_try_from_unknown_returns_err() {
    let unknown_bytes: &[u8] = &[0x01, 0x7f, 0xfe, 0xff, b'x', b'r'];
    for &b in unknown_bytes {
        let result = CompressionType::try_from(b);
        assert!(
            matches!(result, Err(RiegeliError::UnknownCompressionType(_))),
            "expected UnknownCompressionType for byte {b:#04x}, got {result:?}"
        );
    }
}

// -----------------------------------------------------------------------
// Alignment targets that land inside a block-header shadow must be advanced
// to the next multiple that is a possible chunk boundary, matching the C++
// records_internal::PosAfterPadding behavior.
// -----------------------------------------------------------------------

#[test]
fn final_padding_target_inside_block_header_shadow() {
    // With no records the writer is at position 64. The first multiple of
    // 65546 is 65546 = 65536 + 10, which falls inside the 24-byte block header
    // at offset 65536 and cannot be a chunk boundary. The next multiples,
    // 131092 (block offset 20) and only then 196638 (block offset 30), must be
    // tried in turn; the file must end at 196638.
    let alignment = 65_546u64;
    let data = write_records(&[], WriterOptions::new().final_padding(alignment));
    assert_eq!(
        data.len() as u64 % alignment,
        0,
        "file size {} is not a multiple of {alignment}",
        data.len()
    );
    assert_eq!(data.len() as u64, 3 * alignment);
    // The file must still parse cleanly.
    assert!(read_all(data).is_empty());
}

#[test]
fn initial_padding_target_inside_block_header_shadow() {
    let alignment = 65_546u64;
    let record = vec![0x5Au8; 100];
    let data = write_records(
        &[record.as_slice()],
        WriterOptions::new().initial_padding(alignment),
    );
    assert_eq!(
        data.len() as u64 % alignment,
        0,
        "file size {} is not a multiple of {alignment}",
        data.len()
    );
    let got = read_all(data);
    assert_eq!(got.len(), 1);
    assert_eq!(got[0], record);
}

#[test]
fn final_padding_shadow_alignment_matches_cpp_writer() {
    // Differential check against the reference implementation: identical
    // options must produce a byte-identical file (signature chunk followed by
    // one zero-filled padding chunk ending at 196638).
    let alignment = 65_546u64;
    let rust_bytes = write_records(&[], WriterOptions::new().final_padding(alignment));
    let cpp_bytes = {
        let opts = riegeli_ffi::WriterOptions::new().final_padding(alignment);
        let w = riegeli_ffi::RecordWriter::new(opts).expect("cpp writer");
        w.close().expect("cpp close")
    };
    assert_eq!(rust_bytes.len(), cpp_bytes.len(), "file sizes differ");
    assert_eq!(rust_bytes, cpp_bytes, "file bytes differ");
}
