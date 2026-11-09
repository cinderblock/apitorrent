//! DHT messages arrive from anyone on the internet.
#![no_main]
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let _ = apitorrent_dht::krpc::decode(data);
    let _ = apitorrent_dht::krpc::parse_compact_nodes(data, 26);
    let _ = apitorrent_dht::krpc::parse_compact_nodes(data, 38);
    let _ = apitorrent_dht::PersistedState::from_bytes(data);
});
