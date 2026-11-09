//! Tracker responses, HTTP and UDP.
#![no_main]
use apitorrent_tracker::{http, udp};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let _ = http::parse_announce(data);
    let hashes = [apitorrent_metainfo::Sha1Hash([7; 20])];
    let _ = http::parse_scrape(data, &hashes);
    let _ = udp::decode_connect(data, 1);
    let _ = udp::decode_announce(data, 1, udp::PeerFamily::V4);
    let _ = udp::decode_announce(data, 1, udp::PeerFamily::V6);
    let _ = udp::decode_scrape(data, 1, &hashes);
});
