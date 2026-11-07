//! Property tests: the packet codec round-trips and never panics, and a connection fed
//! arbitrary packets and timer events never panics and keeps its invariants.

use std::time::{Duration, Instant};

use apitorrent_utp::packet::{self, Header, Packet, PacketType, SelectiveAck};
use apitorrent_utp::{Config, Connection};
use proptest::prelude::*;

fn packet_type() -> impl Strategy<Value = PacketType> {
    prop_oneof![
        Just(PacketType::Data),
        Just(PacketType::Fin),
        Just(PacketType::State),
        Just(PacketType::Reset),
        Just(PacketType::Syn),
    ]
}

prop_compose! {
    fn header()(
        ty in packet_type(),
        connection_id in any::<u16>(),
        timestamp in any::<u32>(),
        timestamp_diff in any::<u32>(),
        wnd_size in any::<u32>(),
        seq_nr in any::<u16>(),
        ack_nr in any::<u16>(),
    ) -> Header {
        Header { ty, connection_id, timestamp, timestamp_diff, wnd_size, seq_nr, ack_nr }
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]

    #[test]
    fn decode_never_panics(data in proptest::collection::vec(any::<u8>(), 0..400)) {
        let _ = Packet::decode(&data);
    }

    #[test]
    fn encode_decode_round_trip(
        h in header(),
        sack_bits in proptest::collection::vec(0usize..256, 0..20),
        payload in proptest::collection::vec(any::<u8>(), 0..300),
    ) {
        let sack = (!sack_bits.is_empty()).then(|| {
            let mut s = SelectiveAck::with_capacity(sack_bits.iter().max().unwrap() + 1);
            for &b in &sack_bits {
                s.set(b);
            }
            s
        });
        let mut out = Vec::new();
        packet::encode(&h, sack.as_ref(), &payload, &mut out);
        prop_assert!(packet::looks_like_utp(&out));
        let p = Packet::decode(&out).unwrap();
        prop_assert_eq!(p.header, h);
        prop_assert_eq!(p.payload, &payload[..]);
        prop_assert_eq!(p.sack, sack);
        for b in sack_bits {
            prop_assert!(p.sack.unwrap().get(b));
        }
    }

    /// Hostile or confused peers: random packets, some aimed close to the connection's
    /// real sequence numbers, interleaved with timers and application calls.
    #[test]
    fn connections_survive_arbitrary_packets(
        acceptor in any::<bool>(),
        events in proptest::collection::vec(
            (0u8..6, header(), proptest::collection::vec(any::<u8>(), 0..64),
             proptest::option::of(proptest::collection::vec(0usize..64, 1..6)),
             0u64..3_000),
            1..120,
        ),
    ) {
        let cfg = Config::default();
        let start = Instant::now();
        let mut now = start;
        let syn = Header {
            ty: PacketType::Syn,
            connection_id: 10,
            timestamp: 0,
            timestamp_diff: 0,
            wnd_size: 100_000,
            seq_nr: 500,
            ack_nr: 0,
        };
        let mut c = if acceptor {
            Connection::accept(&cfg, now, start, &syn, 900)
        } else {
            Connection::connect(&cfg, now, start, 10, 900)
        };
        let mut buf = [0u8; 4096];
        for (kind, mut h, payload, sack_bits, advance_ms) in events {
            now += Duration::from_millis(advance_ms);
            match kind {
                0 | 1 => {
                    // Aim near the real numbers half the time, so the packet gets past
                    // the validity checks.
                    if kind == 0 {
                        h.seq_nr = 500u16.wrapping_add(h.seq_nr % 8);
                        h.ack_nr = 900u16.wrapping_add(h.ack_nr % 8).wrapping_sub(1);
                    }
                    let sack = sack_bits.map(|bits| {
                        let mut s = SelectiveAck::with_capacity(64);
                        for b in bits {
                            s.set(b);
                        }
                        s
                    });
                    let p = Packet { header: h, sack, payload: &payload };
                    c.handle_packet(now, &p);
                }
                2 => c.handle_timeout(now),
                3 => {
                    let _ = c.write(&payload);
                }
                4 => {
                    let _ = c.read(&mut buf);
                }
                _ => c.close(),
            }
            let mut sent = 0;
            while let Some(d) = c.poll_transmit(now) {
                prop_assert!(d.len() <= cfg.packet_size + 40);
                prop_assert!(Packet::decode(&d).is_ok());
                sent += 1;
                prop_assert!(sent < 10_000, "poll_transmit never stops");
            }
            let s = c.stats();
            prop_assert!(s.packets_in_flight <= 4096);
            if let Some(t) = c.poll_timeout() {
                prop_assert!(t >= start);
            }
        }
    }
}
