//! Regression tests verifying transpose codec correctness across edge cases.

use riegeli::{CompressionType, ReaderOptions, RecordReader, RecordWriter, WriterOptions};
use std::io::Cursor;

/// Round-trip helper: write records with transpose encoding, read them back.
fn transpose_roundtrip(records: &[&[u8]], compression: CompressionType) -> Vec<Vec<u8>> {
    let mut buf = Cursor::new(Vec::<u8>::new());
    let opts = WriterOptions::default()
        .transpose(true)
        .compression(compression);
    {
        let mut writer = RecordWriter::new(&mut buf, opts).expect("writer::new");
        for r in records {
            writer.write_record(r).expect("write_record");
        }
        writer.flush().expect("flush");
    }
    let data = buf.into_inner();
    let mut reader =
        RecordReader::new(Cursor::new(&data), ReaderOptions::new()).expect("reader::new");
    let mut out = Vec::new();
    while let Some(rec) = reader.read_record().expect("read_record") {
        out.push(rec);
    }
    out
}

// ---- behavioral correctness preserved ----

#[test]
fn adversarial_empty_records_transpose_roundtrip() {
    let records: Vec<&[u8]> = vec![b"", b"", b""];
    let result = transpose_roundtrip(&records, CompressionType::None);
    assert_eq!(result.len(), 3);
    for r in &result {
        assert!(r.is_empty());
    }
}

#[test]
fn adversarial_single_byte_records() {
    let data: Vec<Vec<u8>> = (0..=255u8).map(|b| vec![b]).collect();
    let refs: Vec<&[u8]> = data.iter().map(|v| v.as_slice()).collect();
    let result = transpose_roundtrip(&refs, CompressionType::None);
    assert_eq!(result.len(), 256);
    for (i, r) in result.iter().enumerate() {
        assert_eq!(r, &[i as u8], "record {i} mismatch");
    }
}

#[test]
fn adversarial_proto_varint_field_range() {
    let mut raw_records = Vec::new();
    for val in [0u64, 1, 2, 3, 4, 127, 128, 16383, 300, u64::MAX >> 1] {
        let mut rec = Vec::new();
        rec.push(0x08); // field 1, wire type 0 (varint)
        let mut v = val;
        loop {
            if v < 0x80 {
                rec.push(v as u8);
                break;
            }
            rec.push((v as u8) | 0x80);
            v >>= 7;
        }
        raw_records.push(rec);
    }
    let refs: Vec<&[u8]> = raw_records.iter().map(|v| v.as_slice()).collect();
    let result = transpose_roundtrip(&refs, CompressionType::None);
    assert_eq!(result.len(), refs.len());
    for (i, (got, expected)) in result.iter().zip(raw_records.iter()).enumerate() {
        assert_eq!(got, expected, "varint record {i} mismatch");
    }
}

#[test]
fn adversarial_proto_fixed32_field() {
    let mut records = Vec::new();
    for val in [0u32, 1, 0xDEADBEEF, u32::MAX] {
        let mut rec = Vec::new();
        rec.push(0x0D); // field 1, wire type 5 (fixed32)
        rec.extend_from_slice(&val.to_le_bytes());
        records.push(rec);
    }
    let refs: Vec<&[u8]> = records.iter().map(|v| v.as_slice()).collect();
    let result = transpose_roundtrip(&refs, CompressionType::None);
    assert_eq!(result.len(), refs.len());
    for (i, (got, expected)) in result.iter().zip(records.iter()).enumerate() {
        assert_eq!(got, expected, "fixed32 record {i} mismatch");
    }
}

#[test]
fn adversarial_proto_fixed64_field() {
    let mut records = Vec::new();
    for val in [0u64, 1, 0xCAFEBABEDEADBEEF, u64::MAX] {
        let mut rec = Vec::new();
        rec.push(0x09); // field 1, wire type 1 (fixed64)
        rec.extend_from_slice(&val.to_le_bytes());
        records.push(rec);
    }
    let refs: Vec<&[u8]> = records.iter().map(|v| v.as_slice()).collect();
    let result = transpose_roundtrip(&refs, CompressionType::None);
    assert_eq!(result.len(), refs.len());
    for (i, (got, expected)) in result.iter().zip(records.iter()).enumerate() {
        assert_eq!(got, expected, "fixed64 record {i} mismatch");
    }
}

#[test]
fn adversarial_proto_string_field() {
    let mut records = Vec::new();
    for s in [&b""[..], b"a", b"hello world", &[0xFF; 300]] {
        let mut rec = Vec::new();
        rec.push(0x0A); // field 1, wire type 2 (length-delimited)
        let mut len = s.len();
        loop {
            if len < 0x80 {
                rec.push(len as u8);
                break;
            }
            rec.push((len as u8) | 0x80);
            len >>= 7;
        }
        rec.extend_from_slice(s);
        records.push(rec);
    }
    let refs: Vec<&[u8]> = records.iter().map(|v| v.as_slice()).collect();
    let result = transpose_roundtrip(&refs, CompressionType::None);
    assert_eq!(result.len(), refs.len());
    for (i, (got, expected)) in result.iter().zip(records.iter()).enumerate() {
        assert_eq!(got, expected, "string record {i} mismatch");
    }
}

#[test]
fn adversarial_mixed_proto_nonproto_interleaved() {
    let proto1 = vec![0x08, 0x01];
    let nonproto = vec![0xFF, 0xFE, 0xFD];
    let proto2 = vec![0x08, 0x02];
    let records: Vec<&[u8]> = vec![&proto1, &nonproto, &proto2, &nonproto];
    let result = transpose_roundtrip(&records, CompressionType::None);
    assert_eq!(result.len(), 4);
    assert_eq!(result[0], proto1);
    assert_eq!(result[1], nonproto);
    assert_eq!(result[2], proto2);
    assert_eq!(result[3], nonproto);
}

#[test]
fn adversarial_nested_submessage_roundtrip() {
    let inner = vec![0x08, 0x2A]; // field 1, varint 42
    let mut outer = Vec::new();
    outer.push(0x0A); // field 1, wire type 2
    outer.push(inner.len() as u8);
    outer.extend_from_slice(&inner);
    let records: Vec<&[u8]> = vec![outer.as_slice(); 5];
    let result = transpose_roundtrip(&records, CompressionType::None);
    assert_eq!(result.len(), 5);
    for r in &result {
        assert_eq!(r, &outer);
    }
}

#[test]
fn adversarial_zero_records_transpose() {
    let result = transpose_roundtrip(&[], CompressionType::None);
    assert!(result.is_empty());
}

#[test]
fn adversarial_many_identical_records_state_machine() {
    let record = vec![0x08, 0x01, 0x15, 0x00, 0x00, 0x80, 0x3F];
    let records: Vec<&[u8]> = vec![record.as_slice(); 500];
    let result = transpose_roundtrip(&records, CompressionType::None);
    assert_eq!(result.len(), 500);
    for r in &result {
        assert_eq!(r, &record);
    }
}

#[test]
#[cfg(feature = "brotli")]
fn adversarial_brotli_compressed_transpose() {
    let record = vec![0x08, 0x2A];
    let records: Vec<&[u8]> = vec![record.as_slice(); 100];
    let result = transpose_roundtrip(&records, CompressionType::Brotli);
    assert_eq!(result.len(), 100);
    for r in &result {
        assert_eq!(r, &record);
    }
}
