//! Construct and drain a TransposeChunkDecoder from arbitrary chunk data —
//! the decoder where hostile-input findings concentrated.
//! Reaches the crate-private decoder via the cfg(fuzzing)-only fuzz module.
#![no_main]
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    riegeli::fuzz::transpose_decode(data);
});
