//! Varint decoders plus a decode/encode/decode agreement property.
#![no_main]
use libfuzzer_sys::fuzz_target;
use riegeli::varint::{decode_u32, decode_u64, encode_u64, length_varint_u64};

fuzz_target!(|data: &[u8]| {
    let _ = decode_u32(data);
    if let Ok((v, consumed)) = decode_u64(data) {
        // The canonical re-encoding can never be longer than what decode
        // consumed, and decoding it must yield the same value exactly.
        let enc = encode_u64(v);
        assert!(enc.len() <= consumed);
        assert_eq!(length_varint_u64(v), enc.len());
        let (v2, c2) = decode_u64(&enc).expect("re-decode of canonical encoding");
        assert_eq!(v, v2);
        assert_eq!(c2, enc.len());
    }
});
