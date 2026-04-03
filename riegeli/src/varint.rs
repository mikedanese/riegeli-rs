//! Variable-length integer encoding/decoding (LEB128, protobuf-compatible).
//!
//! Encoding uses the standard unsigned LEB128 format: the low 7 bits of each
//! byte carry data, and the high bit signals that more bytes follow. This is
//! identical to the protobuf varint wire format.

/// Maximum number of bytes needed to encode a `u64` value.
const MAX_U64_VARINT_LEN: usize = 10;
/// Maximum number of bytes needed to encode a `u32` value.
const MAX_U32_VARINT_LEN: usize = 5;

/// Error type for varint decoding failures.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VarintError {
    /// The buffer ended before the varint was complete.
    UnexpectedEof,
    /// The varint encoding overflowed the target integer type.
    Overflow,
}

impl std::fmt::Display for VarintError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            VarintError::UnexpectedEof => write!(f, "buffer ended before varint was complete"),
            VarintError::Overflow => write!(f, "varint overflows target integer type"),
        }
    }
}

impl std::error::Error for VarintError {}

/// Encode a `u64` value as a LEB128 varint into a `Vec<u8>`.
pub fn encode_u64(mut v: u64) -> Vec<u8> {
    let mut buf = Vec::with_capacity(MAX_U64_VARINT_LEN);
    loop {
        let byte = (v & 0x7f) as u8;
        v >>= 7;
        if v == 0 {
            buf.push(byte);
            break;
        } else {
            buf.push(byte | 0x80);
        }
    }
    buf
}

/// Encode a `u32` value as a LEB128 varint into a `Vec<u8>`.
pub fn encode_u32(v: u32) -> Vec<u8> {
    encode_u64(v as u64)
}

/// Decode a `u64` LEB128 varint from a byte slice.
///
/// Returns `(value, bytes_consumed)` on success.
pub fn decode_u64(buf: &[u8]) -> Result<(u64, usize), VarintError> {
    let mut result: u64 = 0;
    let mut shift = 0u32;
    for (i, &byte) in buf.iter().enumerate() {
        if i == MAX_U64_VARINT_LEN {
            return Err(VarintError::Overflow);
        }
        let low7 = (byte & 0x7f) as u64;
        // On the 10th byte (i==9), only the lowest bit is valid for u64.
        if i == 9 && low7 > 1 {
            return Err(VarintError::Overflow);
        }
        result |= low7 << shift;
        shift += 7;
        if byte & 0x80 == 0 {
            return Ok((result, i + 1));
        }
    }
    Err(VarintError::UnexpectedEof)
}

/// Decode a `u32` LEB128 varint from a byte slice.
///
/// Returns `(value, bytes_consumed)` on success.
pub fn decode_u32(buf: &[u8]) -> Result<(u32, usize), VarintError> {
    let mut result: u32 = 0;
    let mut shift = 0u32;
    for (i, &byte) in buf.iter().enumerate() {
        if i == MAX_U32_VARINT_LEN {
            return Err(VarintError::Overflow);
        }
        let low7 = (byte & 0x7f) as u32;
        // On the 5th byte (i==4), only the lowest 4 bits are valid for u32.
        if i == 4 && low7 > 0x0f {
            return Err(VarintError::Overflow);
        }
        result |= low7 << shift;
        shift += 7;
        if byte & 0x80 == 0 {
            return Ok((result, i + 1));
        }
    }
    Err(VarintError::UnexpectedEof)
}

/// Return the number of bytes needed to encode `v` as a LEB128 varint.
///
/// This is equivalent to `encode_u64(v).len()` but does not allocate.
pub fn length_varint_u64(v: u64) -> usize {
    if v == 0 {
        return 1;
    }
    // Number of bits needed: 64 - leading_zeros
    let bits_needed = 64 - v.leading_zeros() as usize;
    // Each byte carries 7 bits; ceil(bits_needed / 7)
    bits_needed.div_ceil(7)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{self, Write};

    fn write_varint_u64(w: &mut impl Write, v: u64) -> io::Result<()> {
        let encoded = encode_u64(v);
        w.write_all(&encoded)
    }

    fn write_varint_u32(w: &mut impl Write, v: u32) -> io::Result<()> {
        let encoded = encode_u32(v);
        w.write_all(&encoded)
    }

    // --- encode_u64 known vectors ---

    #[test]
    fn test_encode_u64_zero() {
        assert_eq!(encode_u64(0), vec![0x00]);
    }

    #[test]
    fn test_encode_u64_one() {
        assert_eq!(encode_u64(1), vec![0x01]);
    }

    #[test]
    fn test_encode_u64_127() {
        assert_eq!(encode_u64(127), vec![0x7f]);
    }

    #[test]
    fn test_encode_u64_128() {
        assert_eq!(encode_u64(128), vec![0x80, 0x01]);
    }

    #[test]
    fn test_encode_u64_16383() {
        assert_eq!(encode_u64(16383), vec![0xff, 0x7f]);
    }

    #[test]
    fn test_encode_u64_16384() {
        assert_eq!(encode_u64(16384), vec![0x80, 0x80, 0x01]);
    }

    #[test]
    fn test_encode_u64_max() {
        assert_eq!(
            encode_u64(u64::MAX),
            vec![0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x01]
        );
    }

    // --- decode_u64 round-trips ---

    fn roundtrip_u64(v: u64) {
        let encoded = encode_u64(v);
        let (decoded, consumed) = decode_u64(&encoded).unwrap();
        assert_eq!(decoded, v, "round-trip failed for {v}");
        assert_eq!(consumed, encoded.len(), "wrong consumed for {v}");
    }

    #[test]
    fn test_decode_u64_roundtrip_representative_set() {
        for &v in &[0u64, 1, 127, 128, 16383, 16384, u64::MAX] {
            roundtrip_u64(v);
        }
    }

    #[test]
    fn test_decode_u64_partial_buffer() {
        // decode_u64 on a buffer with trailing data returns correct consumed count
        let mut buf = encode_u64(300);
        buf.push(0xab); // extra trailing byte
        let (val, consumed) = decode_u64(&buf).unwrap();
        assert_eq!(val, 300);
        assert_eq!(consumed, 2);
    }

    #[test]
    fn test_decode_u64_unexpected_eof() {
        // Incomplete: byte with continuation bit set but no next byte
        let buf = vec![0x80];
        assert_eq!(decode_u64(&buf), Err(VarintError::UnexpectedEof));
    }

    #[test]
    fn test_decode_u64_overflow() {
        // 11 bytes, all with continuation bits
        let buf = vec![0xff; 11];
        assert_eq!(decode_u64(&buf), Err(VarintError::Overflow));
    }

    // --- decode_u32 ---

    fn roundtrip_u32(v: u32) {
        let encoded = encode_u32(v);
        let (decoded, consumed) = decode_u32(&encoded).unwrap();
        assert_eq!(decoded, v, "round-trip failed for {v}");
        assert_eq!(consumed, encoded.len(), "wrong consumed for {v}");
    }

    #[test]
    fn test_decode_u32_roundtrip() {
        for &v in &[0u32, 1, 127, 128, u32::MAX] {
            roundtrip_u32(v);
        }
    }

    #[test]
    fn test_decode_u32_overflow() {
        // Encoding u64::MAX as varint, then trying to decode as u32 should overflow
        let buf = encode_u64(u64::MAX);
        assert_eq!(decode_u32(&buf), Err(VarintError::Overflow));
    }

    // --- length_varint_u64 ---

    #[test]
    fn test_length_varint_matches_encode_len() {
        for &v in &[0u64, 1, 127, 128, 16383, 16384, u64::MAX] {
            let expected = encode_u64(v).len();
            let got = length_varint_u64(v);
            assert_eq!(
                got, expected,
                "length_varint_u64({v}) = {got}, expected {expected}"
            );
        }
    }

    // --- write_varint ---

    #[test]
    fn test_write_varint_u64() {
        let mut buf = Vec::new();
        write_varint_u64(&mut buf, 128).unwrap();
        assert_eq!(buf, vec![0x80, 0x01]);
    }

    #[test]
    fn test_write_varint_u32() {
        let mut buf = Vec::new();
        write_varint_u32(&mut buf, 0).unwrap();
        assert_eq!(buf, vec![0x00]);
    }
}
