//! The response decoders must never panic on tracker input, and must respect the peer cap.

use apitorrent_metainfo::Sha1Hash;
use apitorrent_tracker::udp::{self, PeerFamily};
use apitorrent_tracker::{MAX_PEERS, http};
use proptest::prelude::*;

fn hashes(n: usize) -> Vec<Sha1Hash> {
    (0..n)
        .map(|i| Sha1Hash([u8::try_from(i % 256).unwrap(); 20]))
        .collect()
}

/// Bytes shaped like a bencoded announce response, so the parser gets past the first byte.
fn bencodeish() -> impl Strategy<Value = Vec<u8>> {
    let piece = prop_oneof![
        Just(b"d".to_vec()),
        Just(b"e".to_vec()),
        Just(b"l".to_vec()),
        Just(b"8:interval".to_vec()),
        Just(b"12:min interval".to_vec()),
        Just(b"5:peers".to_vec()),
        Just(b"6:peers6".to_vec()),
        Just(b"14:failure reason".to_vec()),
        Just(b"5:files".to_vec()),
        Just(b"2:ip".to_vec()),
        Just(b"4:port".to_vec()),
        Just(b"8:complete".to_vec()),
        any::<i64>().prop_map(|i| format!("i{i}e").into_bytes()),
        proptest::collection::vec(any::<u8>(), 0..40).prop_map(|b| [
            format!("{}:", b.len()).into_bytes(),
            b
        ]
        .concat()),
    ];
    proptest::collection::vec(piece, 0..24).prop_map(|v| v.concat())
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(3_000))]

    #[test]
    fn http_parsers_never_panic(data in proptest::collection::vec(any::<u8>(), 0..512)) {
        let _ = http::parse_announce(&data);
        let _ = http::parse_scrape(&data, &hashes(3));
    }

    #[test]
    fn http_parsers_never_panic_on_bencode(data in bencodeish()) {
        if let Ok(r) = http::parse_announce(&data) {
            prop_assert!(r.peers.len() <= MAX_PEERS);
        }
        let _ = http::parse_scrape(&data, &hashes(3));
    }

    #[test]
    fn udp_decoders_never_panic(
        data in proptest::collection::vec(any::<u8>(), 0..512),
        tx in any::<u32>(),
        n in 0usize..80,
    ) {
        let _ = udp::response_header(&data);
        let _ = udp::decode_connect(&data, tx);
        for family in [PeerFamily::V4, PeerFamily::V6] {
            if let Ok(r) = udp::decode_announce(&data, tx, family) {
                prop_assert!(r.peers.len() <= MAX_PEERS);
            }
        }
        if let Ok(r) = udp::decode_scrape(&data, tx, &hashes(n)) {
            prop_assert!(r.files.len() <= n);
        }
    }

    /// Valid headers with random bodies reach the body checks.
    #[test]
    fn udp_decoders_never_panic_with_valid_header(
        action in 0u32..5,
        tx in any::<u32>(),
        body in proptest::collection::vec(any::<u8>(), 0..400),
    ) {
        let mut data = action.to_be_bytes().to_vec();
        data.extend_from_slice(&tx.to_be_bytes());
        data.extend_from_slice(&body);
        let _ = udp::decode_connect(&data, tx);
        let _ = udp::decode_announce(&data, tx, PeerFamily::V4);
        let _ = udp::decode_announce(&data, tx, PeerFamily::V6);
        let _ = udp::decode_scrape(&data, tx, &hashes(5));
    }

    #[test]
    fn url_builders_never_panic(base in ".{0,80}") {
        let _ = http::scrape_base_url(&base);
        let _ = http::scrape_url(&base, &hashes(2));
    }
}
