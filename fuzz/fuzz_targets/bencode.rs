//! Bencode: decoding never panics, and anything that decodes re-encodes to the same bytes
//! (the decoder accepts canonical bencode only).
#![no_main]
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Ok(v) = apitorrent_bencode::decode(data) {
        assert_eq!(v.encode(), data, "decoded input must be canonical");
    }
    let _ = apitorrent_bencode::decode_prefix(data, &apitorrent_bencode::DecodeOptions::lenient());
});
