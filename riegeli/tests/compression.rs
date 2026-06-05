//! Integration tests for all compression codecs (Brotli, Zstd, Snappy) via RecordWriter/RecordReader.

// Some imports are used only by feature-gated tests; in reduced-feature
// builds they would otherwise trip unused_imports.
#![cfg_attr(
    not(all(feature = "brotli", feature = "zstd", feature = "snappy")),
    allow(unused_imports)
)]
use std::cell::RefCell;
use std::io::Cursor;
use std::rc::Rc;

use riegeli::{
    CompressionType, ReaderOptions, RecordPosition, RecordReader, RecordWriter, WriterOptions,
};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Write records to a Vec<u8> and return the bytes.
fn write_records(records: &[&[u8]], opts: WriterOptions) -> Vec<u8> {
    let mut buf = Cursor::new(Vec::<u8>::new());
    {
        let mut w = RecordWriter::new(&mut buf, opts).expect("writer new");
        for rec in records {
            w.write_record(rec).expect("write_record");
        }
        w.flush().expect("flush");
    }
    buf.into_inner()
}

/// Read all records from a byte buffer via RecordReader (no recovery).
fn read_all(data: &[u8]) -> Vec<Vec<u8>> {
    let cursor = Cursor::new(data.to_vec());
    let mut reader = RecordReader::new(cursor, ReaderOptions::new()).expect("reader new");
    let mut out = Vec::new();
    while let Some(rec) = reader.read_record().expect("read_record") {
        out.push(rec);
    }
    out
}

// ---------------------------------------------------------------------------
// Adversarial probe: multi-block file (>65536 bytes) — no records lost
// ---------------------------------------------------------------------------
#[test]
fn multi_block_no_records_lost() {
    // 200 records of 500 bytes each = 100 KB of record data.
    // With chunk_size=2048, there will be many chunks spanning multiple blocks.
    let records: Vec<Vec<u8>> = (0u16..200)
        .map(|i| {
            let mut v = vec![0u8; 500];
            v[0] = (i >> 8) as u8;
            v[1] = (i & 0xFF) as u8;
            v
        })
        .collect();
    let refs: Vec<&[u8]> = records.iter().map(|r| r.as_slice()).collect();
    let data = write_records(&refs, WriterOptions::new().chunk_size(2048));

    // File must span at least two 64 KiB blocks.
    assert!(
        data.len() > 65536,
        "file should span multiple blocks, got {} bytes",
        data.len()
    );

    let got = read_all(&data);
    assert_eq!(
        got.len(),
        records.len(),
        "expected {} records, got {}",
        records.len(),
        got.len()
    );
    for (i, (g, e)) in got.iter().zip(records.iter()).enumerate() {
        assert_eq!(g, e, "record {i} mismatch in multi-block read");
    }
}

// ---------------------------------------------------------------------------
// Adversarial probe: corrupt block header hash at a block boundary, recovery
// ---------------------------------------------------------------------------
#[test]
fn corrupt_block_header_with_recovery() {
    // Write enough data to have a block boundary at 65536.
    let record: Vec<u8> = vec![0xCC; 500];
    let refs: Vec<&[u8]> = (0..200).map(|_| record.as_slice()).collect();
    let mut data = write_records(&refs, WriterOptions::new().chunk_size(2048));

    assert!(
        data.len() > 65536 + 24,
        "need data past first non-zero block boundary"
    );

    // Corrupt the block header at offset 65536 (flip hash bytes 0..7).
    for i in 0..8 {
        data[65536 + i] ^= 0xFF;
    }

    let recovered: Rc<RefCell<Vec<u64>>> = Rc::new(RefCell::new(Vec::new()));
    let rc = Rc::clone(&recovered);

    let cursor = Cursor::new(data.clone());
    let opts = ReaderOptions::new().recovery(move |region| {
        rc.borrow_mut().push(region.begin());
        true
    });
    let mut reader = RecordReader::new(cursor, opts).expect("reader new");

    let mut all = Vec::new();
    loop {
        match reader.read_record() {
            Ok(Some(rec)) => all.push(rec),
            Ok(None) => break,
            Err(e) => panic!("unexpected error with recovery: {e}"),
        }
    }

    // Should have read some records (before and/or after corruption).
    assert!(!all.is_empty(), "should recover some records");
    // Recovery should have been invoked.
    assert!(
        !recovered.borrow().is_empty(),
        "recovery callback should have been called for corrupt block header"
    );
}

// ---------------------------------------------------------------------------
// Adversarial probe: seek to position 0 (before signature chunk)
// ---------------------------------------------------------------------------
#[test]
fn seek_to_position_zero() {
    let data = write_records(&[b"alpha", b"beta"], WriterOptions::new());
    let cursor = Cursor::new(data);
    let mut reader = RecordReader::new(cursor, ReaderOptions::new()).expect("reader new");

    // Seeking to numeric position 0 should either:
    // - return an error, or
    // - resolve to the first data record (since no chunk has chunk_begin <= 0 that is a Simple chunk)
    // The important thing: no panic.
    let result = reader.seek_numeric(0);
    // Whether it errors or succeeds, it must not panic.
    match result {
        Ok(()) => {
            // If it succeeds, reading should give us a record or None.
            let _ = reader.read_record();
        }
        Err(_) => {
            // Error is acceptable for an invalid position.
        }
    }
}

// ---------------------------------------------------------------------------
// Adversarial probe: seek to exact start of last record, read it, then Ok(None)
// ---------------------------------------------------------------------------
#[test]
fn seek_to_last_record_then_eof() {
    let records: Vec<Vec<u8>> = (0..5u8).map(|i| vec![i; 30]).collect();
    let refs: Vec<&[u8]> = records.iter().map(|r| r.as_slice()).collect();
    let data = write_records(&refs, WriterOptions::new().chunk_size(1 << 20));

    let cursor = Cursor::new(data.clone());
    let mut reader = RecordReader::new(cursor, ReaderOptions::new()).expect("reader new");

    // Read all records to discover the last position.
    let mut positions = Vec::new();
    while let Some(_rec) = reader.read_record().expect("read") {
        positions.push(reader.last_pos());
    }
    assert_eq!(positions.len(), 5);

    let last_pos = positions[4];

    // Now seek to last record using seek().
    let cursor2 = Cursor::new(data);
    let mut reader2 = RecordReader::new(cursor2, ReaderOptions::new()).expect("reader new");
    reader2.seek(last_pos).expect("seek to last record");
    let rec = reader2
        .read_record()
        .expect("read")
        .expect("should have last record");
    assert_eq!(rec, vec![4u8; 30], "last record mismatch");

    // Next read should be Ok(None).
    let after = reader2.read_record().expect("read after last");
    assert!(after.is_none(), "expected None after last record");
}

// ---------------------------------------------------------------------------
// Adversarial probe: write 1 record, read it, then read twice more → Ok(None)
// ---------------------------------------------------------------------------
#[test]
fn single_record_then_two_nones() {
    let data = write_records(&[b"only-one"], WriterOptions::new());
    let cursor = Cursor::new(data);
    let mut reader = RecordReader::new(cursor, ReaderOptions::new()).expect("reader new");

    let rec = reader.read_record().expect("first read").expect("record");
    assert_eq!(rec.as_slice(), b"only-one");

    let r1 = reader.read_record().expect("second read");
    assert!(r1.is_none(), "first None after EOF");

    let r2 = reader.read_record().expect("third read");
    assert!(r2.is_none(), "second None after EOF");
}

// ---------------------------------------------------------------------------
// Adversarial: distinct records verify ordering preserved across chunks
// ---------------------------------------------------------------------------
#[test]
fn distinct_records_ordering_across_chunks() {
    // 100 distinct records, small chunk_size to force many chunks
    let records: Vec<Vec<u8>> = (0u32..100)
        .map(|i| format!("record-{i:04}").into_bytes())
        .collect();
    let refs: Vec<&[u8]> = records.iter().map(|r| r.as_slice()).collect();
    let data = write_records(&refs, WriterOptions::new().chunk_size(64));

    let got = read_all(&data);
    assert_eq!(got.len(), 100, "should have 100 records");
    for (i, (g, e)) in got.iter().zip(records.iter()).enumerate() {
        assert_eq!(g, e, "record {i} out of order or corrupted");
    }
}

// ---------------------------------------------------------------------------
// Adversarial: seek_numeric to various positions and verify no panic
// ---------------------------------------------------------------------------
#[test]
fn seek_numeric_edge_positions() {
    let records: Vec<Vec<u8>> = (0..20u8).map(|i| vec![i; 40]).collect();
    let refs: Vec<&[u8]> = records.iter().map(|r| r.as_slice()).collect();
    let data = write_records(&refs, WriterOptions::new().chunk_size(1 << 20));

    // Seek to position past all records (very large)
    let cursor = Cursor::new(data.clone());
    let mut reader = RecordReader::new(cursor, ReaderOptions::new()).expect("reader new");
    let _ = reader.seek_numeric(u64::MAX / 2); // should not panic

    // Seek to position 24 (the signature chunk offset)
    let cursor = Cursor::new(data.clone());
    let mut reader = RecordReader::new(cursor, ReaderOptions::new()).expect("reader new");
    let result = reader.seek_numeric(24);
    match result {
        Ok(()) => {
            // Should still be able to read something or get None
            let _ = reader.read_record();
        }
        Err(_) => {} // acceptable
    }

    // Seek to position 64 (first data chunk)
    let cursor = Cursor::new(data.clone());
    let mut reader = RecordReader::new(cursor, ReaderOptions::new()).expect("reader new");
    reader.seek_numeric(64).expect("seek to 64");
    let rec = reader
        .read_record()
        .expect("read")
        .expect("should have record");
    assert_eq!(rec, vec![0u8; 40], "first record at position 64");
}

// ---------------------------------------------------------------------------
// Adversarial: pos() tracks correctly through multiple reads
// ---------------------------------------------------------------------------
#[test]
fn pos_tracks_correctly_through_reads() {
    let records: Vec<Vec<u8>> = (0..5u8).map(|i| vec![i; 20]).collect();
    let refs: Vec<&[u8]> = records.iter().map(|r| r.as_slice()).collect();
    let data = write_records(&refs, WriterOptions::new().chunk_size(1 << 20));

    let cursor = Cursor::new(data);
    let mut reader = RecordReader::new(cursor, ReaderOptions::new()).expect("reader new");

    // pos() at start should be {24, 0}
    assert_eq!(reader.pos().chunk_begin, 24);
    assert_eq!(reader.pos().record_index, 0);

    // After reading first record, pos should advance
    let _r0 = reader.read_record().expect("read").expect("r0");
    let pos_after_0 = reader.pos();
    // pos should indicate the next record
    assert_eq!(
        pos_after_0.record_index, 1,
        "after reading record 0, pos.record_index should be 1"
    );

    // last_pos should point at the record just read
    let lp = reader.last_pos();
    assert_eq!(lp.record_index, 0, "last_pos should point at record 0");
}

// ---------------------------------------------------------------------------
// Adversarial: empty file (only signature, no data records) → Ok(None) immediately
// ---------------------------------------------------------------------------
#[test]
fn empty_file_returns_none() {
    let data = write_records(&[], WriterOptions::new());
    let cursor = Cursor::new(data);
    let mut reader = RecordReader::new(cursor, ReaderOptions::new()).expect("reader new");
    let r = reader.read_record().expect("read");
    assert!(r.is_none(), "empty file should return None immediately");
}

// ---------------------------------------------------------------------------
// Adversarial: seek after EOF, then read → Ok(None)
// ---------------------------------------------------------------------------
#[test]
fn seek_after_eof_then_read() {
    let data = write_records(&[b"one"], WriterOptions::new());
    let cursor = Cursor::new(data.clone());
    let mut reader = RecordReader::new(cursor, ReaderOptions::new()).expect("reader new");

    // Read to EOF
    let _ = reader.read_record().expect("read");
    let r = reader.read_record().expect("read");
    assert!(r.is_none());

    // Seek to start and read again (verify at_eof resets)
    let first_data_pos = RecordPosition::new(64, 0);
    reader.seek(first_data_pos).expect("seek");
    let rec = reader
        .read_record()
        .expect("read")
        .expect("should have record");
    assert_eq!(rec.as_slice(), b"one");
}

// ---------------------------------------------------------------------------
// Adversarial: multi-block with brotli compression (use high-entropy data)
// ---------------------------------------------------------------------------
#[test]
#[cfg(feature = "brotli")]
fn multi_block_brotli_roundtrip() {
    // Use pseudo-random high-entropy data so brotli cannot compress it much.
    // 500 records of 200 bytes each with varying content.
    let records: Vec<Vec<u8>> = (0u16..500)
        .map(|i| {
            // Generate pseudo-random bytes seeded by record index
            let mut v = Vec::with_capacity(200);
            let mut state: u32 = (i as u32).wrapping_mul(2654435761);
            for _ in 0..200 {
                state = state.wrapping_mul(1103515245).wrapping_add(12345);
                v.push((state >> 16) as u8);
            }
            v
        })
        .collect();
    let refs: Vec<&[u8]> = records.iter().map(|r| r.as_slice()).collect();
    let data = write_records(
        &refs,
        WriterOptions::new()
            .compression(CompressionType::Brotli)
            .chunk_size(4096),
    );

    assert!(
        data.len() > 65536,
        "should span multiple blocks even with brotli, got {} bytes",
        data.len()
    );

    let got = read_all(&data);
    assert_eq!(
        got.len(),
        records.len(),
        "record count mismatch with brotli"
    );
    for (i, (g, e)) in got.iter().zip(records.iter()).enumerate() {
        assert_eq!(g, e, "brotli record {i} mismatch");
    }
}
