//! Two sans-IO connections over a simulated network, on simulated time: loss,
//! duplication, reordering, a bottleneck link with a queue, and resets. Seeded, so every
//! run is the same.

use std::cmp::Reverse;
use std::collections::BinaryHeap;
use std::time::{Duration, Instant};

use apitorrent_utp::packet::{Packet, PacketType};
use apitorrent_utp::{Config, ConnError, Connection, State};
use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};

#[derive(Debug, Clone, Copy)]
struct Link {
    /// One-way propagation delay.
    delay: Duration,
    /// Extra random delay, up to this much, which reorders packets.
    jitter: Duration,
    loss: f64,
    duplicate: f64,
    /// Bottleneck rate in bytes per second; packets queue behind each other.
    rate: Option<f64>,
}

impl Link {
    fn clean() -> Self {
        Self {
            delay: Duration::from_millis(20),
            jitter: Duration::ZERO,
            loss: 0.0,
            duplicate: 0.0,
            rate: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Side {
    A,
    B,
}

/// A packet in flight: arrival time, a tiebreak, destination and bytes.
type InFlight = Reverse<(Instant, u64, Side, Vec<u8>)>;

struct Net {
    rng: StdRng,
    link: Link,
    queue: BinaryHeap<InFlight>,
    serial: u64,
    /// When each direction's bottleneck is next free.
    busy_until: [Instant; 2],
    /// Queuing delay each packet saw at the bottleneck, A to B.
    queuing: Vec<Duration>,
}

impl Net {
    fn new(seed: u64, link: Link, now: Instant) -> Self {
        Self {
            rng: StdRng::seed_from_u64(seed),
            link,
            queue: BinaryHeap::new(),
            serial: 0,
            busy_until: [now; 2],
            queuing: Vec::new(),
        }
    }

    /// Send from `from` to the other side.
    fn send(&mut self, from: Side, data: &[u8], now: Instant) {
        if self.rng.random_bool(self.link.loss) {
            return;
        }
        let copies = if self.rng.random_bool(self.link.duplicate) {
            2
        } else {
            1
        };
        let to = if from == Side::A { Side::B } else { Side::A };
        let mut depart = now;
        if let Some(rate) = self.link.rate {
            let i = from as usize;
            let start = self.busy_until[i].max(now);
            #[allow(clippy::cast_precision_loss)]
            let tx = Duration::from_secs_f64(data.len() as f64 / rate);
            self.busy_until[i] = start + tx;
            depart = start + tx;
            if from == Side::A {
                self.queuing.push(start - now);
            }
        }
        for _ in 0..copies {
            let jitter = if self.link.jitter.is_zero() {
                Duration::ZERO
            } else {
                self.link.jitter.mul_f64(self.rng.random())
            };
            self.serial += 1;
            self.queue.push(Reverse((
                depart + self.link.delay + jitter,
                self.serial,
                to,
                data.to_vec(),
            )));
        }
    }

    fn next_delivery(&self) -> Option<Instant> {
        self.queue.peek().map(|Reverse((t, ..))| *t)
    }
}

struct Sim {
    now: Instant,
    epoch_a: Instant,
    epoch_b: Instant,
    cfg: Config,
    net: Net,
    a: Connection,
    b: Option<Connection>,
}

impl Sim {
    fn new(seed: u64, link: Link, cfg: Config) -> Self {
        let now = Instant::now();
        // B's clock is far off A's: only differences between timestamps may matter.
        let epoch_b = now.checked_sub(Duration::from_secs(3_000)).unwrap_or(now);
        Self {
            now,
            epoch_a: now,
            epoch_b,
            cfg,
            net: Net::new(seed, link, now),
            a: Connection::connect(&cfg, now, now, 0xfff0, 0xfffa),
            b: None,
        }
    }

    fn flush(&mut self) {
        while let Some(d) = self.a.poll_transmit(self.now) {
            self.net.send(Side::A, &d, self.now);
        }
        if let Some(b) = &mut self.b {
            while let Some(d) = b.poll_transmit(self.now) {
                self.net.send(Side::B, &d, self.now);
            }
        }
    }

    /// Advance to the next event and handle it. False when nothing is left to happen.
    fn step(&mut self) -> bool {
        self.flush();
        let next = [
            self.net.next_delivery(),
            self.a.poll_timeout(),
            self.b.as_ref().and_then(Connection::poll_timeout),
        ]
        .into_iter()
        .flatten()
        .min();
        let Some(next) = next else { return false };
        self.now = self.now.max(next);
        while self.net.next_delivery().is_some_and(|t| t <= self.now) {
            let Reverse((_, _, to, data)) = self.net.queue.pop().unwrap();
            let pkt = Packet::decode(&data).expect("we only send valid packets");
            match to {
                Side::A => self.a.handle_packet(self.now, &pkt),
                Side::B => match &mut self.b {
                    Some(b) => b.handle_packet(self.now, &pkt),
                    None if pkt.header.ty == PacketType::Syn => {
                        self.b = Some(Connection::accept(
                            &self.cfg,
                            self.now,
                            self.epoch_b,
                            &pkt.header,
                            0x0003,
                        ));
                    }
                    None => {}
                },
            }
        }
        if self.a.poll_timeout().is_some_and(|t| t <= self.now) {
            self.a.handle_timeout(self.now);
        }
        if let Some(b) = &mut self.b
            && b.poll_timeout().is_some_and(|t| t <= self.now)
        {
            b.handle_timeout(self.now);
        }
        true
    }

    fn elapsed(&self) -> Duration {
        self.now - self.epoch_a
    }
}

fn payload(len: usize, seed: u8) -> Vec<u8> {
    let mut rng = StdRng::seed_from_u64(u64::from(seed));
    let mut v = vec![0u8; len];
    rng.fill(&mut v[..]);
    v
}

/// Send `len` bytes from A to B and `len / 4` back, then close both ways. Returns the
/// simulated time it took.
fn transfer(sim: &mut Sim, len: usize) -> Duration {
    let to_b = payload(len, 1);
    let to_a = payload(len / 4, 2);
    let (mut sent_a, mut sent_b) = (0, 0);
    let mut got_b = Vec::with_capacity(len);
    let mut got_a = Vec::with_capacity(len / 4);
    let mut buf = vec![0u8; 64 * 1024];
    let (mut a_closed, mut b_closed) = (false, false);
    let limit = Duration::from_secs(3_600);
    loop {
        if sent_a < to_b.len() {
            sent_a += sim.a.write(&to_b[sent_a..]).unwrap();
        } else if !a_closed {
            sim.a.close();
            a_closed = true;
        }
        while let Some(n) = sim.a.read(&mut buf).unwrap() {
            if n == 0 {
                break;
            }
            got_a.extend_from_slice(&buf[..n]);
        }
        if let Some(b) = &mut sim.b {
            if sent_b < to_a.len() {
                sent_b += b.write(&to_a[sent_b..]).unwrap();
            } else if !b_closed {
                b.close();
                b_closed = true;
            }
            while let Some(n) = b.read(&mut buf).unwrap() {
                if n == 0 {
                    break;
                }
                got_b.extend_from_slice(&buf[..n]);
            }
        }
        if sim.a.state() == State::Closed
            && sim.b.as_ref().is_some_and(|b| b.state() == State::Closed)
        {
            break;
        }
        assert!(
            sim.elapsed() < limit,
            "transfer stalled: {:?}",
            sim.a.stats()
        );
        assert!(sim.a.error().is_none(), "A failed: {:?}", sim.a.error());
        assert!(
            sim.step(),
            "nothing left to do before the transfer finished"
        );
    }
    assert!(
        got_b == to_b,
        "A to B corrupted ({} of {} bytes)",
        got_b.len(),
        to_b.len()
    );
    assert!(
        got_a == to_a,
        "B to A corrupted ({} of {} bytes)",
        got_a.len(),
        to_a.len()
    );
    sim.elapsed()
}

fn fast_rto() -> Config {
    Config {
        min_rto: Duration::from_millis(100),
        ..Config::default()
    }
}

#[test]
fn clean_link() {
    let mut sim = Sim::new(1, Link::clean(), Config::default());
    let t = transfer(&mut sim, 4 << 20);
    assert_eq!(sim.a.stats().packets_retransmitted, 0);
    assert!(t < Duration::from_secs(30), "took {t:?}");
}

#[test]
fn lossy_link() {
    for (seed, loss) in [(2, 0.01), (3, 0.05), (4, 0.15)] {
        let link = Link {
            loss,
            ..Link::clean()
        };
        let mut sim = Sim::new(seed, link, fast_rto());
        transfer(&mut sim, 2 << 20);
        assert!(sim.a.stats().packets_retransmitted > 0);
    }
}

#[test]
fn reordering_and_duplication() {
    let link = Link {
        jitter: Duration::from_millis(30),
        duplicate: 0.05,
        ..Link::clean()
    };
    let mut sim = Sim::new(5, link, fast_rto());
    transfer(&mut sim, 2 << 20);
}

#[test]
fn everything_at_once() {
    for seed in 10..16 {
        let link = Link {
            delay: Duration::from_millis(40),
            jitter: Duration::from_millis(25),
            loss: 0.03,
            duplicate: 0.02,
            rate: Some(2_000_000.0),
        };
        let mut sim = Sim::new(seed, link, fast_rto());
        transfer(&mut sim, 1 << 20);
    }
}

#[test]
fn ledbat_keeps_the_bottleneck_queue_near_target() {
    // 1 MB/s bottleneck, 25 ms each way, an unbounded queue and no loss: only delay can
    // tell the sender to slow down. A loss-based controller would fill the queue without
    // limit. (Measured when written: mean 89 ms, worst 122 ms, 95% of the link rate.)
    let link = Link {
        delay: Duration::from_millis(25),
        rate: Some(1_000_000.0),
        ..Link::clean()
    };
    let mut sim = Sim::new(7, link, Config::default());
    let len = 16 << 20;
    let t = transfer(&mut sim, len);
    #[allow(clippy::cast_precision_loss)]
    let throughput = len as f64 / t.as_secs_f64();
    let q = &sim.net.queuing;
    let steady = &q[q.len() / 4..];
    let mean = steady.iter().sum::<Duration>() / u32::try_from(steady.len()).unwrap();
    let worst = steady.iter().max().unwrap();
    assert!(
        mean < Duration::from_millis(115),
        "mean queuing delay {mean:?}"
    );
    assert!(
        *worst < Duration::from_millis(200),
        "worst queuing delay {worst:?}"
    );
    assert!(throughput > 900_000.0, "throughput {throughput:.0} B/s");
}

#[test]
fn syn_is_retried_and_connect_times_out() {
    // Lose the first two SYNs: the connection still opens.
    let mut sim = Sim::new(8, Link::clean(), Config::default());
    sim.a.poll_transmit(sim.now).unwrap();
    sim.now += Duration::from_millis(1100);
    sim.a.handle_timeout(sim.now);
    sim.a.poll_transmit(sim.now).unwrap();
    while sim.a.state() == State::SynSent {
        assert!(sim.step());
    }
    assert_eq!(sim.a.state(), State::Connected);

    // Nobody answers at all.
    let cfg = Config {
        connect_timeout: Duration::from_secs(5),
        ..Config::default()
    };
    let start = Instant::now();
    let mut now = start;
    let mut a = Connection::connect(&cfg, now, now, 1, 1);
    let mut syns = 0;
    while a.state() == State::SynSent {
        while a.poll_transmit(now).is_some() {
            syns += 1;
        }
        now = a.poll_timeout().unwrap();
        a.handle_timeout(now);
    }
    assert_eq!(a.state(), State::Failed(ConnError::TimedOut));
    assert!(syns >= 3, "only {syns} SYNs");
    assert!(now - start >= Duration::from_secs(5));
}

#[test]
fn a_silent_peer_times_out_mid_transfer() {
    let mut sim = Sim::new(9, Link::clean(), fast_rto());
    while sim.b.is_none() || !sim.a.is_connected() {
        sim.step();
    }
    sim.a.write(&vec![7; 100_000]).unwrap();
    for _ in 0..50 {
        sim.step();
    }
    // The link goes dead with data still to send. (An idle connection has nothing to
    // time out on; BitTorrent's own keepalives give it data.)
    sim.net.link.loss = 1.0;
    sim.a.write(&[8; 10_000]).unwrap();
    let dead_at = sim.now;
    let mut steps = 0;
    while sim.a.error().is_none() {
        assert!(sim.step());
        steps += 1;
        assert!(steps < 100_000);
    }
    assert_eq!(sim.a.error(), Some(ConnError::TimedOut));
    // 7 timeouts from a 100 ms RTO, doubling: well under a minute.
    let took = sim.now - dead_at;
    assert!(
        took > Duration::from_secs(5) && took < Duration::from_secs(60),
        "{took:?}"
    );
}

#[test]
fn reset_ends_both_sides() {
    let mut sim = Sim::new(11, Link::clean(), Config::default());
    while sim.b.is_none() || !sim.a.is_connected() {
        sim.step();
    }
    let now = sim.now;
    let rst = sim.b.as_mut().unwrap().reset(now).unwrap();
    assert_eq!(sim.b.as_ref().unwrap().error(), Some(ConnError::Reset));
    sim.a.handle_packet(now, &Packet::decode(&rst).unwrap());
    assert_eq!(sim.a.error(), Some(ConnError::Reset));
    assert!(sim.a.write(b"x").is_err());
    let mut buf = [0u8; 4];
    assert!(sim.a.read(&mut buf).is_err());
}

#[test]
fn zero_window_recovers_when_the_reader_drains() {
    let cfg = Config {
        recv_buffer: 64 * 1024,
        min_rto: Duration::from_millis(100),
        ..Config::default()
    };
    let mut sim = Sim::new(12, Link::clean(), cfg);
    while sim.b.is_none() || !sim.a.is_connected() {
        sim.step();
    }
    let data = payload(1 << 20, 3);
    let mut sent = 0;
    // B doesn't read for 30 simulated seconds: A must stall, not time out.
    let pause_until = sim.now + Duration::from_secs(30);
    let mut got = Vec::new();
    let mut buf = vec![0u8; 8192];
    while got.len() < data.len() {
        if sent < data.len() {
            sent += sim.a.write(&data[sent..]).unwrap();
        }
        if sim.now >= pause_until {
            let b = sim.b.as_mut().unwrap();
            while let Some(n) = b.read(&mut buf).unwrap() {
                got.extend_from_slice(&buf[..n]);
            }
        }
        assert!(sim.a.error().is_none(), "{:?}", sim.a.error());
        assert!(sim.step());
        assert!(sim.elapsed() < Duration::from_secs(600));
    }
    assert!(got == data, "corrupted");
}

/// Both sides have finished, but our FIN's acknowledgement never arrives: the peer
/// forgot the connection after its linger, so the retransmissions go unanswered. With
/// every byte delivered both ways, that is a clean close, not a timeout. (Found as a flaky
/// socket test under a lossy proxy.)
#[test]
fn a_lost_final_ack_still_closes_cleanly() {
    let mut sim = Sim::new(21, Link::clean(), fast_rto());
    while sim.b.is_none() || !sim.a.is_connected() {
        sim.step();
    }
    sim.a.write(&[1; 5_000]).unwrap();
    // B sends everything and closes; A reads it all, up to B's FIN.
    sim.b.as_mut().unwrap().write(&[2; 3_000]).unwrap();
    sim.b.as_mut().unwrap().close();
    let mut buf = vec![0u8; 8192];
    let mut got = 0;
    while !sim.a.is_read_closed() {
        assert!(sim.step());
        while let Ok(Some(n)) = sim.a.read(&mut buf) {
            if n == 0 {
                break;
            }
            got += n;
        }
    }
    assert_eq!(got, 3_000);
    // Let A's data be acknowledged, then the link dies just as A closes.
    for _ in 0..200 {
        sim.step();
    }
    sim.net.link.loss = 1.0;
    sim.a.close();
    let mut steps = 0;
    while !sim.a.is_finished() {
        assert!(sim.step());
        steps += 1;
        assert!(steps < 100_000);
    }
    assert_eq!(
        sim.a.error(),
        None,
        "a clean close, not {:?}",
        sim.a.error()
    );
}
