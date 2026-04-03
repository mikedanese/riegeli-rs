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
