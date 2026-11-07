//! Property tests: the KRPC decoder and the node never panic on hostile input, and every
//! message we can build survives an encode/decode round trip.

use std::net::SocketAddr;
use std::time::Instant;

use apitorrent_dht::krpc::{self, Body, Message, NodeInfo, Query, Response, Want};
use apitorrent_dht::{Node, NodeConfig, NodeId};
use proptest::prelude::*;

fn node_id() -> impl Strategy<Value = NodeId> {
    any::<[u8; 20]>().prop_map(NodeId)
}

fn addr() -> impl Strategy<Value = SocketAddr> {
    prop_oneof![
        (any::<[u8; 4]>(), 1u16..).prop_map(|(ip, port)| SocketAddr::from((ip, port))),
        (any::<[u8; 16]>(), 1u16..).prop_map(|(ip, port)| SocketAddr::from((ip, port))),
    ]
    .prop_map(krpc::canonical)
    .prop_filter("specified", |a| !a.ip().is_unspecified())
}

fn v4_addr() -> impl Strategy<Value = SocketAddr> {
    (any::<[u8; 4]>(), 1u16..)
        .prop_map(|(ip, port)| SocketAddr::from((ip, port)))
        .prop_filter("specified", |a| !a.ip().is_unspecified())
}

fn v6_addr() -> impl Strategy<Value = SocketAddr> {
    (any::<[u8; 16]>(), 1u16..)
        .prop_map(|(ip, port)| SocketAddr::from((ip, port)))
        .prop_filter("real IPv6", |a| {
            krpc::canonical(*a).is_ipv6() && !a.ip().is_unspecified()
        })
}

fn want() -> impl Strategy<Value = Want> {
    (any::<bool>(), any::<bool>()).prop_map(|(v4, v6)| Want { v4, v6 })
}

fn query() -> impl Strategy<Value = Query> {
    prop_oneof![
        node_id().prop_map(|id| Query::Ping { id }),
        (node_id(), node_id(), want()).prop_map(|(id, target, want)| Query::FindNode {
            id,
            target,
            want
        }),
        (node_id(), node_id(), want(), any::<bool>(), any::<bool>()).prop_map(
            |(id, info_hash, want, noseed, scrape)| Query::GetPeers {
                id,
                info_hash,
                want,
                noseed,
                scrape
            }
        ),
        (
            node_id(),
            node_id(),
            1u16..,
            any::<bool>(),
            proptest::collection::vec(any::<u8>(), 0..=krpc::MAX_TOKEN_LEN),
            any::<bool>()
        )
            .prop_map(|(id, info_hash, port, implied_port, token, seed)| {
                Query::AnnouncePeer {
                    id,
                    info_hash,
                    port,
                    implied_port,
                    token,
                    seed,
                }
            }),
    ]
}

fn response() -> impl Strategy<Value = Response> {
    (
        node_id(),
        proptest::collection::vec((node_id(), v4_addr()), 0..8),
        proptest::collection::vec((node_id(), v6_addr()), 0..8),
        proptest::collection::vec(addr(), 0..20),
        proptest::option::of(proptest::collection::vec(any::<u8>(), 0..20)),
        any::<bool>(),
    )
        .prop_map(|(id, n4, n6, values, token, bloom)| {
            let to_info = |(id, addr)| NodeInfo { id, addr };
            Response {
                id,
                nodes: n4.into_iter().map(to_info).collect(),
                nodes6: n6.into_iter().map(to_info).collect(),
                values,
                token,
                bf_seeds: bloom.then(|| Box::new([0x5a; krpc::BLOOM_LEN])),
                bf_peers: bloom.then(|| Box::new([0xa5; krpc::BLOOM_LEN])),
            }
        })
}

fn message() -> impl Strategy<Value = Message> {
    (
        proptest::collection::vec(any::<u8>(), 0..=krpc::MAX_TRANSACTION_ID_LEN),
        proptest::option::of(proptest::collection::vec(
            any::<u8>(),
            0..=krpc::MAX_VERSION_LEN,
        )),
        any::<bool>(),
        proptest::option::of(addr()),
        prop_oneof![
            query().prop_map(Body::Query),
            response().prop_map(Body::Response),
            (any::<i64>(), "[ -~]{0,40}").prop_map(|(code, message)| Body::Error { code, message }),
        ],
    )
        .prop_map(|(transaction_id, version, read_only, ip, body)| Message {
            transaction_id,
            version,
            read_only,
            ip,
            body,
        })
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(2000))]

    #[test]
    fn roundtrip(m in message()) {
        let bytes = m.encode();
        prop_assert_eq!(krpc::decode(&bytes).unwrap(), m);
    }

    #[test]
    fn random_bytes_never_panic(data in proptest::collection::vec(any::<u8>(), 0..512)) {
        let _ = krpc::decode(&data);
    }

    /// Mutating valid messages explores the decoder far better than random bytes.
    #[test]
    fn mutated_messages_never_panic(
        m in message(),
        flips in proptest::collection::vec((any::<prop::sample::Index>(), any::<u8>()), 1..8),
        cut in any::<prop::sample::Index>(),
    ) {
        let mut bytes = m.encode();
        for (i, b) in flips {
            let i = i.index(bytes.len());
            bytes[i] = b;
        }
        let _ = krpc::decode(&bytes);
        let cut = cut.index(bytes.len() + 1);
        let _ = krpc::decode(&bytes[..cut]);
    }

    /// The whole node, not just the decoder, shrugs off hostile datagrams, including
    /// well-formed ones carrying nonsense.
    #[test]
    fn node_never_panics(
        msgs in proptest::collection::vec((message(), addr(), any::<bool>()), 1..20),
    ) {
        let now = Instant::now();
        let mut node = Node::new(NodeConfig { rng_seed: Some(1), ..NodeConfig::default() }, None, now);
        node.add_bootstrap(["1.2.3.4:6881".parse().unwrap()], now);
        for (m, from, garble) in msgs {
            let mut bytes = m.encode();
            if garble && !bytes.is_empty() {
                let mid = bytes.len() / 2;
                bytes[mid] ^= 0xff;
            }
            node.handle_datagram(from, &bytes, now);
            while node.poll_transmit().is_some() {}
            while node.poll_event().is_some() {}
        }
        node.handle_timeout(now + std::time::Duration::from_secs(30));
    }
}
