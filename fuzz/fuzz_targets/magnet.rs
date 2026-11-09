//! A magnet link from a web page or the clipboard.
#![no_main]
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Ok(s) = std::str::from_utf8(data) {
        let _ = apitorrent_metainfo::Magnet::parse(s);
    }
});
