//! A real DHT of 20 nodes on loopback sockets: one node announces, another finds it. No
//! internet access: bootstrapping uses the first node only.

use std::net::SocketAddr;
use std::time::Duration;

use apitorrent_dht::{Dht, DhtConfig, NodeConfig, PersistedState};
use futures_util::StreamExt;

const NODES: usize = 20;

fn config(bootstrap: Vec<String>, seed: u64) -> DhtConfig {
    DhtConfig {
        bind_v4: Some("127.0.0.1:0".parse().unwrap()),
        bind_v6: None,
        bootstrap,
        state: None,
        node: NodeConfig {
            rng_seed: Some(seed),
            query_timeout: Duration::from_secs(2),
            ..NodeConfig::default()
        },
    }
}

async fn network() -> Vec<Dht> {
    let first = Dht::start(config(Vec::new(), 0)).await.unwrap();
    let boot = first.local_addr_v4().unwrap().to_string();
    let mut nodes = vec![first];
    for i in 1..NODES {
        let dht = Dht::start(config(vec![boot.clone()], i as u64))
            .await
            .unwrap();
        nodes.push(dht);
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    // Wait for every routing table to hold a handful of nodes. Usually a second or two;
    // a busy machine can take longer, and sparse tables re-bootstrap after 30 s.
    for _ in 0..900 {
        let ready = nodes
            .iter()
            .all(|d| d.stats().ipv4.is_some_and(|t| t.nodes >= 4));
        if ready {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    for (i, d) in nodes.iter().enumerate() {
        let t = d.stats().ipv4.unwrap();
        assert!(t.nodes >= 4, "node {i} has {} nodes", t.nodes);
    }
    nodes
}

async fn collect(mut s: apitorrent_dht::PeerStream) -> Vec<SocketAddr> {
    let mut out = Vec::new();
    while let Some(p) = s.next().await {
        out.push(p);
    }
    out
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn twenty_nodes_announce_and_find() {
    tokio::time::timeout(Duration::from_secs(60), async {
        let nodes = network().await;
        let info_hash = [0x42; 20];

        // Explicit port.
        collect(nodes[3].announce(info_hash, Some(4444))).await;
        // Implied port: the announcer's DHT socket port.
        collect(nodes[9].announce(info_hash, None)).await;

        let found = collect(nodes[17].get_peers(info_hash)).await;
        let explicit: SocketAddr = "127.0.0.1:4444".parse().unwrap();
        let implied = nodes[9].local_addr_v4().unwrap();
        assert!(found.contains(&explicit), "{found:?}");
        assert!(found.contains(&implied), "{found:?}");
        assert_eq!(found.len(), 2, "deduplicated: {found:?}");

        // Scrape sees the downloaders and no seeds. BEP 33 counts IP addresses, and both
        // announcers are 127.0.0.1, so that is one.
        let est = nodes[12]
            .scrape(info_hash)
            .await
            .unwrap()
            .expect("estimate");
        assert!((est.peers - 1.0).abs() < 0.5, "{est:?}");
        assert!(est.seeds < 0.5, "{est:?}");

        // An unknown info-hash finds nothing, and the stream still ends.
        assert!(collect(nodes[5].get_peers([0x13; 20])).await.is_empty());

        // Stats are a snapshot refreshed every 250 ms.
        tokio::time::sleep(Duration::from_millis(400)).await;
        assert!(nodes[0].stats().queries_received > 0);
        let stored: usize = nodes.iter().map(|d| d.stats().stored_peers).sum();
        assert!(stored >= 2, "announces were stored somewhere: {stored}");

        // Persistence: restart node 17 from its saved state, with no bootstrap routers.
        let saved = nodes[17].export_state().await.unwrap();
        let old_id = nodes[17].stats().node_id;
        nodes[17].shutdown().await;
        let mut cfg = config(Vec::new(), 1717);
        cfg.state = Some(PersistedState::from_bytes(&saved).unwrap());
        let reborn = Dht::start(cfg).await.unwrap();
        assert_eq!(reborn.stats().node_id, old_id);
        let found = collect(reborn.get_peers(info_hash)).await;
        assert!(found.contains(&explicit), "after restart: {found:?}");

        reborn.shutdown().await;
        for (i, d) in nodes.iter().enumerate() {
            if i != 17 {
                d.shutdown().await;
            }
        }
        // Calls after shutdown fail cleanly.
        assert!(nodes[0].export_state().await.is_err());
        assert!(collect(nodes[0].get_peers(info_hash)).await.is_empty());
    })
    .await
    .expect("test timed out");
}

/// The same over IPv6 loopback, when the host has it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ipv6_loopback() {
    let v6 = |bootstrap: Vec<String>, seed| DhtConfig {
        bind_v4: None,
        bind_v6: Some("[::1]:0".parse().unwrap()),
        bootstrap,
        state: None,
        node: NodeConfig {
            rng_seed: Some(seed),
            ..NodeConfig::default()
        },
    };
    let Ok(first) = Dht::start(v6(Vec::new(), 100)).await else {
        eprintln!("no IPv6 loopback here; skipping");
        return;
    };
    tokio::time::timeout(Duration::from_secs(30), async {
        let boot = first.local_addr_v6().unwrap().to_string();
        let mut nodes = vec![first];
        for i in 0..7 {
            nodes.push(Dht::start(v6(vec![boot.clone()], 101 + i)).await.unwrap());
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        for _ in 0..100 {
            if nodes
                .iter()
                .all(|d| d.stats().ipv6.is_some_and(|t| t.nodes >= 2))
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(nodes.iter().all(|d| d.stats().ipv4.is_none()));
        collect(nodes[2].announce([7; 20], Some(5555))).await;
        let found = collect(nodes[6].get_peers([7; 20])).await;
        assert_eq!(found, vec!["[::1]:5555".parse::<SocketAddr>().unwrap()]);
        for d in &nodes {
            d.shutdown().await;
        }
    })
    .await
    .expect("test timed out");
}
