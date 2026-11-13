//! Deluge's daemon RPC: a framed, zlib-compressed rencode message, as the daemon (or anything
//! answering on its port) sends it. Decoding must never panic, recurse unboundedly or
//! allocate past the limits; what decodes must re-encode and decode to the same value.
#![no_main]
use apitorrent_deluge::rpc::{FrameLimits, frame, rencode};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let limits = FrameLimits {
        max_compressed: 1 << 20,
        max_decompressed: 4 << 20,
        ..FrameLimits::default()
    };
    let _ = frame::decode(data, &limits);
    if let Ok(v) = rencode::decode(data, limits.rencode) {
        let again = rencode::encode(&v).expect("a decoded value encodes");
        let back = rencode::decode(&again, limits.rencode).expect("our encoding decodes");
        // NaN != NaN, so compare encodings.
        assert_eq!(rencode::encode(&back).unwrap(), again);
    }
});
