//! Deluge's `torrents.state` is a Python pickle, read by our own restricted unpickler.
#![no_main]
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let _ = apitorrent_deluge::pickle::load(data);
});
