//! Whether peers on the internet can connect to us, judged from what happens, and what
//! our public address is.
//!
//! An incoming connection from a public address proves a port is open. TCP is clean
//! proof: a NAT or stateful firewall never lets an unrequested SYN through without a
//! forward. uTP and the DHT are UDP, where our own packets to an address open a hole for
//! its answers, so only connections from IPs we haven't sent to lately count (see
//! [`apitorrent_dht::Contacted`]). No proof for [`PROBABLY_CLOSED_AFTER`] after the
//! swarms learned our address (a successful announce) suggests the port is closed; that
//! is never certain, since a quiet swarm sends no one.
//!
//! Our public address comes from outside: DHT nodes (BEP 42 votes), peers (the BEP 10
//! `yourip` field, by majority) and HTTP trackers (BEP 24 `external ip`). The router's own
//! public address (from NAT-PMP or UPnP) is kept apart: when it isn't the one the internet
//! sees, another NAT sits in front of the router, often the ISP's, and a forward on the
//! router can't open the port.
//!
//! When the user asks, a public service connects back to the TCP port
//! ([`crate::port_check`]). Its answer counts until newer evidence says otherwise: one that
//! got through proves the port open, one that didn't says it's closed.
//!
//! When the public address of a family changes, its proof starts over.

use std::collections::VecDeque;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use apitorrent_dht::Contacted;

use crate::port_check::PortChecker;

/// How long to wait for proof after the swarms learned our address before calling a port
/// probably closed.
pub const PROBABLY_CLOSED_AFTER: Duration = Duration::from_secs(30 * 60);
/// Votes on our address kept per family, one per peer IP.
const MAX_VOTES: usize = 32;
/// Votes needed before peers' majority counts.
const MIN_VOTES: usize = 3;

/// What the evidence says about one port.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PortState {
    /// A connection came in on its own since the public address last changed.
    Open,
    /// None yet, and it's too soon to say.
    Waiting,
    /// None in [`PROBABLY_CLOSED_AFTER`] since the swarms learned our address.
    ProbablyClosed,
    /// A port check couldn't connect, and nothing has come in since.
    Closed,
}

/// What a port check service found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PortCheck {
    pub checker: PortChecker,
    pub open: bool,
    pub at: Instant,
}

/// One port, one family.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PortReport {
    pub port: u16,
    pub state: PortState,
    /// Connections that proved it open since the session started.
    pub incoming: u64,
    pub last_incoming: Option<Instant>,
    /// Since when we've been waiting for proof; `None` while no swarm knows our address.
    pub waiting_since: Option<Instant>,
    /// The last port check since the public address last changed (TCP only).
    pub check: Option<PortCheck>,
}

/// One address family.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct FamilyReport {
    /// Our public address, as the internet sees it: the DHT's vote, else peers', else the
    /// last tracker's.
    pub external_ip: Option<IpAddr>,
    pub from_dht: Option<IpAddr>,
    pub from_peers: Option<IpAddr>,
    pub from_tracker: Option<IpAddr>,
    /// The router's own public address (NAT-PMP or UPnP).
    pub from_router: Option<IpAddr>,
    /// The router's public address isn't the one the internet sees, or isn't public at
    /// all: another NAT is in front of it.
    pub double_nat: bool,
    /// `None` when we don't listen on this family (or uTP or the DHT is off).
    pub tcp: Option<PortReport>,
    pub utp: Option<PortReport>,
    pub dht: Option<PortReport>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ReachabilityReport {
    pub ipv4: FamilyReport,
    pub ipv6: FamilyReport,
}

/// What the rest of the session knows, passed in when a report is made.
#[derive(Debug, Clone, Default)]
pub(crate) struct Inputs {
    /// The peer port, and which families it listens on (index 0 is IPv4).
    pub peer_port: u16,
    pub tcp_families: [bool; 2],
    pub utp_families: [bool; 2],
    /// The DHT's port per family, where it runs.
    pub dht_ports: [Option<u16>; 2],
    pub dht: Option<apitorrent_dht::DhtStats>,
    pub router_ip: Option<IpAddr>,
}

#[derive(Debug, Clone, Copy, Default)]
struct Proof {
    count: u64,
    last: Option<Instant>,
}

impl Proof {
    fn record(&mut self, now: Instant) {
        self.count += 1;
        self.last = Some(now);
    }
}

#[derive(Debug)]
struct State {
    started: Instant,
    /// When an announce first told a swarm where we are.
    advertised: Option<Instant>,
    tcp: [Proof; 2],
    utp: [Proof; 2],
    /// The last port check of the TCP port, per family.
    checks: [Option<PortCheck>; 2],
    /// IPs we dialed over uTP lately: their connections back prove nothing.
    utp_dialed: Contacted,
    /// (voter, what it says our address is), oldest first.
    votes: [VecDeque<(IpAddr, IpAddr)>; 2],
    tracker_ip: [Option<IpAddr>; 2],
    /// The public address of each family when we last looked, and when it last changed.
    address: [Option<IpAddr>; 2],
    changed: [Option<Instant>; 2],
}

/// The evidence, shared by the session's parts.
#[derive(Debug)]
pub(crate) struct Reachability {
    state: Mutex<State>,
}

impl Reachability {
    pub(crate) fn new(now: Instant) -> Self {
        Self {
            state: Mutex::new(State {
                started: now,
                advertised: None,
                tcp: [Proof::default(); 2],
                utp: [Proof::default(); 2],
                checks: [None, None],
                utp_dialed: Contacted::new(now),
                votes: [VecDeque::new(), VecDeque::new()],
                tracker_ip: [None, None],
                address: [None, None],
                changed: [None, None],
            }),
        }
    }

    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// A peer at `from` connected to us and completed the BitTorrent handshake.
    pub(crate) fn incoming(&self, utp: bool, from: SocketAddr, now: Instant) {
        let ip = from.ip().to_canonical();
        if !is_public(ip) {
            return;
        }
        let mut s = self.state();
        let fam = family(ip);
        // Our own public address: we connected to ourselves, through the router's hairpin
        // or this machine's own stack.
        if s.address[fam] == Some(ip) || s.tracker_ip[fam] == Some(ip) {
            return;
        }
        if utp {
            s.utp_dialed.tick(now);
            if s.utp_dialed.unsolicited(ip) == Some(true) {
                s.utp[fam].record(now);
            }
        } else {
            s.tcp[fam].record(now);
        }
    }

    /// We started a uTP connection to `to`.
    pub(crate) fn dialed_utp(&self, to: IpAddr, now: Instant) {
        let mut s = self.state();
        s.utp_dialed.tick(now);
        s.utp_dialed.note(to);
    }

    /// A peer at `voter` says our address is `ip` (BEP 10 `yourip`).
    pub(crate) fn peer_says(&self, voter: IpAddr, ip: IpAddr) {
        let (voter, ip) = (voter.to_canonical(), ip.to_canonical());
        if !is_public(voter) || !is_public(ip) || family(voter) != family(ip) {
            return;
        }
        let mut s = self.state();
        let votes = &mut s.votes[family(ip)];
        votes.retain(|(v, _)| *v != voter);
        if votes.len() >= MAX_VOTES {
            votes.pop_front();
        }
        votes.push_back((voter, ip));
    }

    /// A tracker announce worked; it saw us at `external_ip` if it said.
    pub(crate) fn announced(&self, external_ip: Option<IpAddr>, now: Instant) {
        let mut s = self.state();
        s.advertised.get_or_insert(now);
        if let Some(ip) = external_ip.map(|ip| ip.to_canonical())
            && is_public(ip)
        {
            s.tracker_ip[family(ip)] = Some(ip);
        }
    }

    /// `checker` found the TCP port open or not, in `family` (0 is IPv4).
    pub(crate) fn checked(&self, family: usize, checker: PortChecker, open: bool, now: Instant) {
        self.state().checks[family] = Some(PortCheck {
            checker,
            open,
            at: now,
        });
    }

    /// A DHT announce finished.
    pub(crate) fn announced_on_dht(&self, now: Instant) {
        self.state().advertised.get_or_insert(now);
    }

    pub(crate) fn report(&self, inputs: &Inputs, now: Instant) -> ReachabilityReport {
        let mut s = self.state();
        let mut families = [FamilyReport::default(), FamilyReport::default()];
        for (fam, out) in families.iter_mut().enumerate() {
            let from_dht = inputs.dht.as_ref().and_then(|d| {
                if fam == 0 {
                    d.external_ipv4
                } else {
                    d.external_ipv6
                }
            });
            let from_peers = majority(&s.votes[fam]);
            let from_tracker = s.tracker_ip[fam];
            let from_router = inputs
                .router_ip
                .map(|ip| ip.to_canonical())
                .filter(|ip| family(*ip) == fam);
            let external_ip = from_dht.or(from_peers).or(from_tracker);
            if let Some(ip) = external_ip {
                if s.address[fam].is_some_and(|old| old != ip) {
                    s.changed[fam] = Some(now);
                }
                s.address[fam] = Some(ip);
            }
            let double_nat =
                from_router.is_some_and(|r| !is_public(r) || external_ip.is_some_and(|ip| ip != r));

            // Proof counts only after the last address change, and the wait starts again.
            let changed = s.changed[fam];
            let since = |t: Option<Instant>| match (t, changed) {
                (Some(t), Some(c)) => Some(t.max(c)),
                (t, _) => t,
            };
            let current = |t: Instant| changed.is_none_or(|c| t >= c);
            let port = |port: u16,
                        proof: Proof,
                        waiting_since: Option<Instant>,
                        check: Option<PortCheck>| {
                let proven = proof.last.filter(|l| current(*l));
                let check = check.filter(|c| current(c.at));
                // The newer of a port check and the last incoming connection decides.
                let state = if let Some(c) = check
                    && proven.is_none_or(|l| c.at >= l)
                {
                    if c.open {
                        PortState::Open
                    } else {
                        PortState::Closed
                    }
                } else if proven.is_some() {
                    PortState::Open
                } else if waiting_since
                    .is_some_and(|w| now.saturating_duration_since(w) >= PROBABLY_CLOSED_AFTER)
                {
                    PortState::ProbablyClosed
                } else {
                    PortState::Waiting
                };
                PortReport {
                    port,
                    state,
                    incoming: proof.count,
                    last_incoming: proof.last,
                    waiting_since,
                    check,
                }
            };
            let advertised = since(s.advertised);
            let tcp = inputs.tcp_families[fam]
                .then(|| port(inputs.peer_port, s.tcp[fam], advertised, s.checks[fam]));
            let utp = inputs.utp_families[fam]
                .then(|| port(inputs.peer_port, s.utp[fam], advertised, None));
            // DHT nodes learn of us as soon as we query them, so its wait starts with the
            // session.
            let dht = inputs.dht_ports[fam].map(|p| {
                let stats = inputs.dht.as_ref();
                let proof = Proof {
                    count: stats.map_or(0, |d| d.unsolicited_queries[fam]),
                    last: stats.and_then(|d| d.last_unsolicited[fam]),
                };
                port(p, proof, since(Some(s.started)), None)
            });
            *out = FamilyReport {
                external_ip,
                from_dht,
                from_peers,
                from_tracker,
                from_router,
                double_nat,
                tcp,
                utp,
                dht,
            };
        }
        let [ipv4, ipv6] = families;
        ReachabilityReport { ipv4, ipv6 }
    }
}

/// The address most votes agree on, when there are enough votes and a clear majority.
fn majority(votes: &VecDeque<(IpAddr, IpAddr)>) -> Option<IpAddr> {
    if votes.len() < MIN_VOTES {
        return None;
    }
    let mut counts: Vec<(IpAddr, usize)> = Vec::new();
    for (_, ip) in votes {
        match counts.iter_mut().find(|(c, _)| c == ip) {
            Some((_, n)) => *n += 1,
            None => counts.push((*ip, 1)),
        }
    }
    let (ip, n) = counts.into_iter().max_by_key(|(_, n)| *n)?;
    (n * 2 > votes.len()).then_some(ip)
}

/// 0 for IPv4 (IPv4-mapped included), 1 for IPv6.
fn family(ip: IpAddr) -> usize {
    usize::from(ip.to_canonical().is_ipv6())
}

/// Whether `ip` is reachable on the internet: not loopback, private, link-local, the
/// carrier-grade NAT range or another special-purpose range.
pub(crate) fn is_public(ip: IpAddr) -> bool {
    match ip.to_canonical() {
        IpAddr::V4(v4) => is_public_v4(v4),
        IpAddr::V6(v6) => is_public_v6(v6),
    }
}

fn is_public_v4(ip: Ipv4Addr) -> bool {
    let [a, b, c, _] = ip.octets();
    !(ip.is_unspecified()
        || ip.is_loopback()
        || ip.is_private()
        || ip.is_link_local()
        || ip.is_broadcast()
        || ip.is_documentation()
        || ip.is_multicast()
        || a == 0
        || (a == 100 && (64..128).contains(&b)) // shared address space (carrier-grade NAT)
        || (a == 192 && b == 0 && c == 0) // IETF protocol assignments
        || (a == 198 && (18..20).contains(&b)) // benchmarking
        || a >= 240) // reserved
}

fn is_public_v6(ip: Ipv6Addr) -> bool {
    let s0 = ip.segments()[0];
    !(ip.is_unspecified()
        || ip.is_loopback()
        || ip.is_multicast()
        || (s0 & 0xfe00) == 0xfc00 // unique local
        || (s0 & 0xffc0) == 0xfe80 // link local
        || (s0 == 0x2001 && ip.segments()[1] == 0x0db8)) // documentation
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    fn addr(s: &str) -> SocketAddr {
        s.parse().unwrap()
    }

    /// Listening on IPv4 only, with uTP, no DHT.
    fn inputs() -> Inputs {
        Inputs {
            peer_port: 40_000,
            tcp_families: [true, false],
            utp_families: [true, false],
            ..Inputs::default()
        }
    }

    #[test]
    fn tcp_waits_until_advertised_then_gives_up() {
        let t0 = Instant::now();
        let r = Reachability::new(t0);
        let rep = r.report(&inputs(), t0 + Duration::from_secs(3600));
        let tcp = rep.ipv4.tcp.unwrap();
        assert_eq!(
            tcp.state,
            PortState::Waiting,
            "nobody knows where we are yet"
        );
        assert_eq!(tcp.waiting_since, None);
        assert!(rep.ipv6.tcp.is_none(), "not listening on IPv6");

        let t1 = t0 + Duration::from_secs(60);
        r.announced(None, t1);
        let rep = r.report(
            &inputs(),
            t1 + PROBABLY_CLOSED_AFTER.saturating_sub(Duration::from_secs(1)),
        );
        assert_eq!(rep.ipv4.tcp.unwrap().state, PortState::Waiting);
        let rep = r.report(&inputs(), t1 + PROBABLY_CLOSED_AFTER);
        assert_eq!(rep.ipv4.tcp.unwrap().state, PortState::ProbablyClosed);
        assert_eq!(rep.ipv4.utp.unwrap().state, PortState::ProbablyClosed);
    }

    #[test]
    fn a_public_tcp_peer_proves_it_open_and_a_lan_one_doesnt() {
        let t0 = Instant::now();
        let r = Reachability::new(t0);
        r.incoming(false, addr("192.168.1.20:5000"), t0);
        r.incoming(false, addr("127.0.0.1:5000"), t0);
        let tcp = r.report(&inputs(), t0).ipv4.tcp.unwrap();
        assert_eq!(tcp.state, PortState::Waiting);
        assert_eq!(tcp.incoming, 0);

        r.incoming(false, addr("[::ffff:23.1.2.3]:5000"), t0);
        let rep = r.report(&inputs(), t0);
        let tcp = rep.ipv4.tcp.unwrap();
        assert_eq!(tcp.state, PortState::Open);
        assert_eq!(tcp.last_incoming, Some(t0));
        assert_eq!(
            rep.ipv4.utp.unwrap().state,
            PortState::Waiting,
            "TCP says nothing of uTP"
        );
    }

    #[test]
    fn a_connection_from_our_own_address_proves_nothing() {
        let t0 = Instant::now();
        let r = Reachability::new(t0);
        r.announced(Some(ip("203.1.1.1")), t0);
        r.incoming(false, addr("203.1.1.1:5000"), t0);
        assert_eq!(r.report(&inputs(), t0).ipv4.tcp.unwrap().incoming, 0);
    }

    #[test]
    fn utp_from_someone_we_dialed_proves_nothing() {
        let t0 = Instant::now();
        let r = Reachability::new(t0);
        r.dialed_utp(ip("23.1.2.3"), t0);
        r.incoming(true, addr("23.1.2.3:6881"), t0);
        assert_eq!(
            r.report(&inputs(), t0).ipv4.utp.unwrap().state,
            PortState::Waiting
        );
        r.incoming(true, addr("23.1.2.4:6881"), t0);
        assert_eq!(
            r.report(&inputs(), t0).ipv4.utp.unwrap().state,
            PortState::Open
        );
    }

    #[test]
    fn peers_majority_names_the_address() {
        let r = Reachability::new(Instant::now());
        r.peer_says(ip("23.0.0.1"), ip("198.51.1.1"));
        r.peer_says(ip("23.0.0.2"), ip("198.51.1.1"));
        let now = Instant::now();
        assert_eq!(
            r.report(&inputs(), now).ipv4.from_peers,
            None,
            "too few votes"
        );
        r.peer_says(ip("23.0.0.3"), ip("203.1.1.1"));
        assert_eq!(
            r.report(&inputs(), now).ipv4.from_peers,
            Some(ip("198.51.1.1"))
        );
        // One peer saying it again doesn't add a vote.
        r.peer_says(ip("23.0.0.3"), ip("203.1.1.1"));
        r.peer_says(ip("23.0.0.4"), ip("203.1.1.1"));
        assert_eq!(
            r.report(&inputs(), now).ipv4.from_peers,
            None,
            "no clear majority"
        );
        // Votes from the LAN or for a private address don't count.
        r.peer_says(ip("10.0.0.5"), ip("198.51.1.1"));
        r.peer_says(ip("23.0.0.5"), ip("10.1.1.1"));
        assert_eq!(r.report(&inputs(), now).ipv4.from_peers, None);
    }

    #[test]
    fn the_dht_wins_then_peers_then_trackers() {
        let now = Instant::now();
        let r = Reachability::new(now);
        r.announced(Some(ip("203.1.1.3")), now);
        let rep = r.report(&inputs(), now);
        assert_eq!(rep.ipv4.external_ip, Some(ip("203.1.1.3")));
        assert_eq!(rep.ipv4.from_tracker, Some(ip("203.1.1.3")));
        for v in 1..=3 {
            r.peer_says(ip(&format!("23.0.0.{v}")), ip("203.1.1.2"));
        }
        assert_eq!(
            r.report(&inputs(), now).ipv4.external_ip,
            Some(ip("203.1.1.2"))
        );
        let with_dht = Inputs {
            dht: Some(apitorrent_dht::DhtStats {
                external_ipv4: Some(ip("203.1.1.1")),
                ..apitorrent_dht::DhtStats::default()
            }),
            ..inputs()
        };
        assert_eq!(
            r.report(&with_dht, now).ipv4.external_ip,
            Some(ip("203.1.1.1"))
        );
    }

    #[test]
    fn a_router_behind_another_nat_is_flagged() {
        let now = Instant::now();
        let r = Reachability::new(now);
        let router = |ip_: &str| Inputs {
            router_ip: Some(ip(ip_)),
            ..inputs()
        };
        // Carrier-grade NAT range: flagged even before the internet says anything.
        assert!(r.report(&router("100.70.1.2"), now).ipv4.double_nat);
        assert!(!r.report(&router("203.1.1.1"), now).ipv4.double_nat);
        r.announced(Some(ip("203.1.1.1")), now);
        assert!(!r.report(&router("203.1.1.1"), now).ipv4.double_nat);
        assert!(r.report(&router("203.9.9.9"), now).ipv4.double_nat);
    }

    #[test]
    fn a_new_address_starts_the_proof_over() {
        let t0 = Instant::now();
        let r = Reachability::new(t0);
        r.announced(Some(ip("203.1.1.1")), t0);
        r.incoming(false, addr("23.1.2.3:5000"), t0);
        assert_eq!(
            r.report(&inputs(), t0).ipv4.tcp.unwrap().state,
            PortState::Open
        );

        let t1 = t0 + Duration::from_secs(600);
        r.announced(Some(ip("203.2.2.2")), t1);
        let tcp = r.report(&inputs(), t1).ipv4.tcp.unwrap();
        assert_eq!(tcp.state, PortState::Waiting);
        assert_eq!(tcp.waiting_since, Some(t1));
        let later = t1 + PROBABLY_CLOSED_AFTER;
        assert_eq!(
            r.report(&inputs(), later).ipv4.tcp.unwrap().state,
            PortState::ProbablyClosed
        );
        r.incoming(false, addr("23.1.2.3:5000"), later);
        assert_eq!(
            r.report(&inputs(), later).ipv4.tcp.unwrap().state,
            PortState::Open
        );
    }

    #[test]
    fn dht_proof_comes_from_its_unsolicited_queries() {
        let t0 = Instant::now();
        let r = Reachability::new(t0);
        let dht = |count: u64, last: Option<Instant>| Inputs {
            dht_ports: [Some(41_000), None],
            dht: Some(apitorrent_dht::DhtStats {
                unsolicited_queries: [count, 0],
                last_unsolicited: [last, None],
                ..apitorrent_dht::DhtStats::default()
            }),
            ..inputs()
        };
        let rep = r.report(&dht(0, None), t0 + PROBABLY_CLOSED_AFTER);
        assert_eq!(rep.ipv4.dht.unwrap().state, PortState::ProbablyClosed);
        assert!(rep.ipv6.dht.is_none());
        let rep = r.report(&dht(2, Some(t0)), t0 + PROBABLY_CLOSED_AFTER);
        let d = rep.ipv4.dht.unwrap();
        assert_eq!((d.state, d.incoming, d.port), (PortState::Open, 2, 41_000));
    }

    #[test]
    fn a_port_check_counts_until_newer_evidence() {
        let t0 = Instant::now();
        let r = Reachability::new(t0);
        r.announced(Some(ip("203.1.1.1")), t0);
        let state = |t| r.report(&inputs(), t).ipv4.tcp.unwrap().state;
        r.checked(0, PortChecker::Ifconfig, false, t0);
        assert_eq!(state(t0), PortState::Closed);
        let t1 = t0 + Duration::from_secs(60);
        r.incoming(false, addr("23.1.2.3:5000"), t1);
        assert_eq!(state(t1), PortState::Open, "a peer came in since");
        let t2 = t1 + Duration::from_secs(60);
        r.checked(0, PortChecker::Transmission, false, t2);
        assert_eq!(state(t2), PortState::Closed, "the forward went away");
        r.checked(0, PortChecker::Transmission, true, t2);
        let tcp = r.report(&inputs(), t2).ipv4.tcp.unwrap();
        assert_eq!(tcp.state, PortState::Open);
        assert_eq!(
            tcp.check.map(|c| c.checker),
            Some(PortChecker::Transmission)
        );
        assert!(
            r.report(&inputs(), t2).ipv4.utp.unwrap().check.is_none(),
            "the services test TCP"
        );

        // A new address: the check was of the old one.
        let t3 = t2 + Duration::from_secs(60);
        r.announced(Some(ip("203.2.2.2")), t3);
        let tcp = r.report(&inputs(), t3).ipv4.tcp.unwrap();
        assert_eq!((tcp.state, tcp.check), (PortState::Waiting, None));
    }

    #[test]
    fn public_addresses() {
        for p in ["23.1.2.3", "2600:1700::1", "::ffff:23.1.2.3"] {
            assert!(is_public(ip(p)), "{p}");
        }
        for p in [
            "10.0.0.1",
            "100.64.0.1",
            "127.0.0.1",
            "169.254.1.1",
            "192.168.0.1",
            "::1",
            "fd00::1",
            "fe80::1",
            "2001:db8::1",
            "::ffff:10.0.0.1",
        ] {
            assert!(!is_public(ip(p)), "{p}");
        }
    }
}
