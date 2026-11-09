//! A `.torrent` file: parsing never panics, whatever the file says.
#![no_main]
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Ok(t) = apitorrent_metainfo::Torrent::from_bytes(data) {
        let _ = t.info.info_hashes();
        for f in &t.info.files {
            let _ = f.display_path();
        }
    }
});
