//! DNS-01: wait until a published TXT record is visible before the CA looks for it.
//!
//! A DNS provider's API accepting a record isn't the record being served: zones take a
//! moment to reach every name server, and a validator that asks one that hasn't caught up
//! fails the order. So after a provider publishes a record, the ACME manager asks the
//! zone's **authoritative** servers, all of them, until each that answers serves the value.
//!
//! The authoritative servers are found by iterative resolution from the root servers
//! (following referrals, with the system resolver only for name servers given without
//! glue, and following a CNAME on `_acme-challenge` names, a common way to delegate them).
//! Asking them directly avoids resolver caches, which can hold a stale negative answer for
//! minutes. Where outbound DNS is restricted, or for split-horizon setups, name the
//! resolvers to ask instead (`--acme-dns-resolver`). If no server can be reached at all,
//! the check gives up quickly, waits a fixed while and lets the CA try.

use std::net::SocketAddr;
use std::time::Duration;

use super::dns::{DnsError, TxtRecord};
use super::wire::{self, Message, RData, WireError};

/// The root servers' IPv4 and IPv6 addresses (IANA root hints).
const ROOTS: &[&str] = &[
    "198.41.0.4:53",
    "170.247.170.2:53",
    "192.33.4.12:53",
    "199.7.91.13:53",
    "192.203.230.10:53",
    "192.5.5.241:53",
    "192.112.36.4:53",
    "198.97.190.53:53",
    "192.36.148.17:53",
    "192.58.128.30:53",
    "193.0.14.129:53",
    "199.7.83.42:53",
    "202.12.27.33:53",
    "[2001:503:ba3e::2:30]:53",
    "[2801:1b8:10::b]:53",
    "[2001:500:2::c]:53",
    "[2001:500:2d::d]:53",
    "[2001:500:a8::e]:53",
    "[2001:500:2f::f]:53",
    "[2001:500:12::d0d]:53",
    "[2001:500:1::53]:53",
    "[2001:7fe::53]:53",
    "[2001:503:c27::2:30]:53",
    "[2001:7fd::1]:53",
    "[2001:500:9f::42]:53",
    "[2001:dc3::35]:53",
];

/// How the ACME manager checks that a DNS-01 record is visible.
#[derive(Debug, Clone)]
pub struct DnsPropagation {
    /// Ask these resolvers (with recursion) instead of the zone's authoritative servers.
    pub resolvers: Vec<SocketAddr>,
    /// How long a record may take to appear everywhere before the order fails.
    pub timeout: Duration,
    /// Between checks.
    pub interval: Duration,
    /// Where iterative resolution starts: the root servers, unless a test says otherwise.
    pub roots: Vec<SocketAddr>,
    /// The port of name servers found by referral.
    pub port: u16,
    /// When no DNS server answers at all: wait this long instead, then let the CA try.
    pub unreachable_wait: Duration,
    /// How long one query waits for an answer.
    pub query_timeout: Duration,
}

impl Default for DnsPropagation {
    fn default() -> Self {
        Self {
            resolvers: Vec::new(),
            timeout: Duration::from_secs(300),
            interval: Duration::from_secs(5),
            roots: ROOTS.iter().filter_map(|r| r.parse().ok()).collect(),
            port: 53,
            unreachable_wait: Duration::from_secs(60),
            query_timeout: Duration::from_secs(3),
        }
    }
}

/// One look at the record.
#[derive(Debug, PartialEq, Eq)]
enum Look {
    /// Every server that answered serves the value.
    Visible,
    /// Servers answered, and some lack the value (named in the message).
    Missing(String),
    /// No server could be asked.
    Unreachable(String),
}

/// The first of `servers` to answer.
async fn ask_any(
    servers: &[SocketAddr],
    name: &str,
    qtype: u16,
    recursion: bool,
    timeout: Duration,
) -> Result<Message, String> {
    let mut last = String::from("no servers to ask");
    for &s in servers {
        match wire::query(s, name, qtype, recursion, timeout).await {
            Ok(m) => return Ok(m),
            Err(e) => last = format!("{s}: {e}"),
        }
    }
    Err(last)
}

/// The name the TXT record really lives at (after CNAMEs) and the addresses of its zone's
/// authoritative servers.
async fn authoritative(
    name: &str,
    cfg: &DnsPropagation,
) -> Result<(String, Vec<SocketAddr>), String> {
    let mut target = wire::canonical(name);
    for _ in 0..8 {
        let mut servers = cfg.roots.clone();
        let mut next_target = None;
        for _ in 0..16 {
            let m = ask_any(&servers, &target, wire::TYPE_TXT, false, cfg.query_timeout).await?;
            if m.authoritative {
                let has_txt = m
                    .answers
                    .iter()
                    .any(|r| r.name == target && matches!(r.data, RData::Txt(_)));
                let cname = m.answers.iter().find_map(|r| match &r.data {
                    RData::Cname(c) if r.name == target => Some(c.clone()),
                    _ => None,
                });
                match cname {
                    Some(c) if !has_txt => next_target = Some(c),
                    _ => return Ok((target, servers)),
                }
                break;
            }
            if m.rcode == wire::RCODE_NXDOMAIN {
                return Err(format!(
                    "{target} doesn't exist (NXDOMAIN from a non-authoritative server)"
                ));
            }
            let names: Vec<String> = m
                .authority
                .iter()
                .filter_map(|r| match &r.data {
                    RData::Ns(n) => Some(n.clone()),
                    _ => None,
                })
                .collect();
            if names.is_empty() {
                return Err(format!("no referral for {target}"));
            }
            let mut next: Vec<SocketAddr> = m
                .additional
                .iter()
                .filter(|r| names.contains(&r.name))
                .filter_map(|r| match r.data {
                    RData::A(ip) => Some(SocketAddr::new(ip.into(), cfg.port)),
                    RData::Aaaa(ip) => Some(SocketAddr::new(ip.into(), cfg.port)),
                    _ => None,
                })
                .collect();
            if next.is_empty() {
                for n in &names {
                    if let Ok(addrs) = tokio::net::lookup_host((n.as_str(), cfg.port)).await {
                        next.extend(addrs);
                    }
                }
            }
            if next.is_empty() {
                return Err(format!(
                    "no addresses for the name servers of {target}: {}",
                    names.join(", ")
                ));
            }
            // IPv4 first: more hosts can reach it.
            next.sort_by_key(SocketAddr::is_ipv6);
            next.dedup();
            servers = next;
        }
        match next_target {
            Some(t) => target = t,
            None => return Err(format!("too many referrals for {target}")),
        }
    }
    Err(format!("too many CNAMEs from {name}"))
}

fn txt_values(m: &Message) -> Vec<&str> {
    m.answers
        .iter()
        .filter_map(|r| match &r.data {
            RData::Txt(t) => Some(t.as_str()),
            _ => None,
        })
        .collect()
}

/// Ask each of `servers` for the TXT record; who has the value, who doesn't, who didn't
/// answer.
async fn look_at(
    servers: &[SocketAddr],
    name: &str,
    value: &str,
    recursion: bool,
    cfg: &DnsPropagation,
) -> Look {
    let mut answered = 0;
    let mut missing = Vec::new();
    let mut errors = Vec::new();
    for &s in servers {
        match wire::query(s, name, wire::TYPE_TXT, recursion, cfg.query_timeout).await {
            Ok(m) => {
                answered += 1;
                if !txt_values(&m).contains(&value) {
                    missing.push(s.to_string());
                }
            }
            Err(e @ (WireError::Io(_) | WireError::Timeout(_) | WireError::Malformed)) => {
                errors.push(format!("{s}: {e}"));
            }
        }
    }
    if answered == 0 {
        Look::Unreachable(errors.join("; "))
    } else if missing.is_empty() {
        Look::Visible
    } else {
        Look::Missing(format!("not yet on {}", missing.join(", ")))
    }
}

async fn look(record: &TxtRecord, cfg: &DnsPropagation) -> Look {
    if !cfg.resolvers.is_empty() {
        return look_at(&cfg.resolvers, &record.name, &record.value, true, cfg).await;
    }
    match authoritative(&record.name, cfg).await {
        Ok((name, servers)) => look_at(&servers, &name, &record.value, false, cfg).await,
        Err(e) => Look::Unreachable(e),
    }
}

/// Wait until `record` is visible, as configured; see the module docs.
pub(crate) async fn wait_for(record: &TxtRecord, cfg: &DnsPropagation) -> Result<(), DnsError> {
    let deadline = tokio::time::Instant::now() + cfg.timeout;
    let mut reached = false;
    loop {
        match look(record, cfg).await {
            Look::Visible => {
                tracing::info!(record = %record.name, "the ACME TXT record is visible");
                return Ok(());
            }
            Look::Missing(why) => {
                reached = true;
                if tokio::time::Instant::now() >= deadline {
                    return Err(DnsError(format!(
                        "the TXT record {} didn't appear within {} s: {why}",
                        record.name,
                        cfg.timeout.as_secs()
                    )));
                }
                tracing::debug!(record = %record.name, %why, "waiting for the ACME TXT record");
            }
            Look::Unreachable(why) if !reached => {
                tracing::warn!(
                    record = %record.name,
                    error = %why,
                    wait_secs = cfg.unreachable_wait.as_secs(),
                    "could not ask any DNS server whether the ACME TXT record is visible; \
                     waiting instead (name reachable resolvers with --acme-dns-resolver)"
                );
                tokio::time::sleep(cfg.unreachable_wait).await;
                return Ok(());
            }
            Look::Unreachable(why) => {
                if tokio::time::Instant::now() >= deadline {
                    return Err(DnsError(format!(
                        "lost touch with the DNS servers while waiting for {}: {why}",
                        record.name
                    )));
                }
            }
        }
        tokio::time::sleep(cfg.interval).await;
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::net::{IpAddr, Ipv4Addr};
    use std::sync::{Arc, Mutex};

    use tokio::net::UdpSocket;

    use super::*;
    use crate::acme::wire::tests::{name_bytes, question, respond, txt};

    type Rr = (String, u16, Vec<u8>);
    type Answer = (bool, u8, [Vec<Rr>; 3]);

    /// A fake name server on `ip:port` answering with `f(name, type)`.
    async fn server(
        ip: Ipv4Addr,
        port: u16,
        f: impl Fn(&str, u16) -> Answer + Send + Sync + 'static,
    ) -> Option<SocketAddr> {
        let sock = UdpSocket::bind((ip, port)).await.ok()?;
        let addr = sock.local_addr().ok()?;
        tokio::spawn(async move {
            let mut buf = [0u8; 1500];
            while let Ok((n, from)) = sock.recv_from(&mut buf).await {
                let q = &buf[..n];
                let (name, qtype) = question(q);
                let (aa, rcode, [an, ns, ad]) = f(&name, qtype);
                let _ = sock
                    .send_to(&respond(q, aa, rcode, [&an, &ns, &ad]), from)
                    .await;
            }
        });
        Some(addr)
    }

    fn a(ip: Ipv4Addr) -> Vec<u8> {
        ip.octets().to_vec()
    }

    /// Three servers on one port of 127.0.0.2-4: a root that refers `example.test` and
    /// `delegate.test` to two authoritative servers, which serve TXT values from `zone`.
    /// `lagging` keeps the second server behind until it is cleared.
    struct Fake {
        root: SocketAddr,
        port: u16,
        zone: Arc<Mutex<HashMap<String, Vec<String>>>>,
        lagging: Arc<Mutex<bool>>,
    }

    async fn fake() -> Option<Fake> {
        let zone: Arc<Mutex<HashMap<String, Vec<String>>>> = Arc::default();
        let lagging = Arc::new(Mutex::new(true));
        for _ in 0..20 {
            let root = server(Ipv4Addr::new(127, 0, 0, 2), 0, |name, _| {
                let ns = |n: &str| (name.to_owned(), wire::TYPE_NS, name_bytes(n));
                let glue = |n: &str, ip| (n.to_owned(), wire::TYPE_A, a(ip));
                let zone = if name.ends_with("example.test") {
                    "example.test"
                } else {
                    "delegate.test"
                };
                (
                    false,
                    0,
                    [
                        vec![],
                        vec![
                            (zone.to_owned(), wire::TYPE_NS, ns("ns1.dns.test").2),
                            (zone.to_owned(), wire::TYPE_NS, ns("ns2.dns.test").2),
                        ],
                        vec![
                            glue("ns1.dns.test", Ipv4Addr::new(127, 0, 0, 3)),
                            glue("ns2.dns.test", Ipv4Addr::new(127, 0, 0, 4)),
                        ],
                    ],
                )
            })
            .await?;
            let port = root.port();
            let authority = |behind: Option<Arc<Mutex<bool>>>| {
                let zone = zone.clone();
                move |name: &str, _qtype: u16| -> Answer {
                    if name == "_acme-challenge.example.test" {
                        let target = "_acme-challenge.delegate.test";
                        return (
                            true,
                            0,
                            [
                                vec![(name.to_owned(), wire::TYPE_CNAME, name_bytes(target))],
                                vec![],
                                vec![],
                            ],
                        );
                    }
                    if behind.as_ref().is_some_and(|b| *b.lock().unwrap()) {
                        return (true, 0, [vec![], vec![], vec![]]);
                    }
                    let values = zone.lock().unwrap().get(name).cloned().unwrap_or_default();
                    let rcode = if values.is_empty() {
                        wire::RCODE_NXDOMAIN
                    } else {
                        0
                    };
                    let an = values
                        .iter()
                        .map(|v| (name.to_owned(), wire::TYPE_TXT, txt(v)))
                        .collect();
                    (true, rcode, [an, vec![], vec![]])
                }
            };
            let ns1 = server(Ipv4Addr::new(127, 0, 0, 3), port, authority(None)).await;
            let ns2 = server(
                Ipv4Addr::new(127, 0, 0, 4),
                port,
                authority(Some(lagging.clone())),
            )
            .await;
            if ns1.is_some() && ns2.is_some() {
                return Some(Fake {
                    root,
                    port,
                    zone,
                    lagging,
                });
            }
        }
        None
    }

    fn cfg(f: &Fake) -> DnsPropagation {
        DnsPropagation {
            roots: vec![f.root],
            port: f.port,
            timeout: Duration::from_secs(5),
            interval: Duration::from_millis(50),
            unreachable_wait: Duration::from_millis(10),
            query_timeout: Duration::from_millis(500),
            ..DnsPropagation::default()
        }
    }

    fn record(value: &str) -> TxtRecord {
        TxtRecord {
            domain: "example.test".into(),
            name: "_acme-challenge.example.test".into(),
            value: value.into(),
        }
    }

    #[tokio::test]
    async fn waits_for_every_authoritative_server_through_a_cname() {
        let Some(f) = fake().await else {
            eprintln!("skipped: can't bind 127.0.0.2-4 here");
            return;
        };
        f.zone.lock().unwrap().insert(
            "_acme-challenge.delegate.test".into(),
            vec!["other".into(), "v1".into()],
        );
        // ns2 lags behind; the check waits for it.
        let lagging = f.lagging.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(300)).await;
            *lagging.lock().unwrap() = false;
        });
        let started = tokio::time::Instant::now();
        wait_for(&record("v1"), &cfg(&f)).await.unwrap();
        assert!(
            started.elapsed() >= Duration::from_millis(250),
            "it waited for ns2"
        );

        // A value that never appears fails once the time is up.
        let c = DnsPropagation {
            timeout: Duration::from_millis(300),
            ..cfg(&f)
        };
        let e = wait_for(&record("never"), &c).await.unwrap_err();
        assert!(e.0.contains("didn't appear"), "{e}");
    }

    #[tokio::test]
    async fn named_resolvers_are_asked_instead() {
        let Some(f) = fake().await else {
            eprintln!("skipped: can't bind 127.0.0.2-4 here");
            return;
        };
        f.zone
            .lock()
            .unwrap()
            .insert("_acme-challenge.example.test".into(), vec!["v2".into()]);
        let ns1 = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 3)), f.port);
        // A "resolver" that answers the name directly (no CNAME for this one in the test).
        let r = server(Ipv4Addr::LOCALHOST, 0, {
            let zone = f.zone.clone();
            move |name, _| {
                let values = zone.lock().unwrap().get(name).cloned().unwrap_or_default();
                (
                    false,
                    0,
                    [
                        values
                            .iter()
                            .map(|v| (name.to_owned(), wire::TYPE_TXT, txt(v)))
                            .collect(),
                        vec![],
                        vec![],
                    ],
                )
            }
        })
        .await
        .unwrap();
        let c = DnsPropagation {
            resolvers: vec![r],
            ..cfg(&f)
        };
        wait_for(&record("v2"), &c).await.unwrap();
        let _ = ns1;
    }

    #[tokio::test]
    async fn unreachable_dns_waits_and_carries_on() {
        // Nothing listens on this port.
        let port = std::net::UdpSocket::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let c = DnsPropagation {
            roots: vec![SocketAddr::from(([127, 0, 0, 1], port))],
            port,
            timeout: Duration::from_secs(5),
            interval: Duration::from_millis(10),
            unreachable_wait: Duration::from_millis(50),
            query_timeout: Duration::from_millis(200),
            ..DnsPropagation::default()
        };
        let started = tokio::time::Instant::now();
        wait_for(&record("x"), &c).await.unwrap();
        assert!(started.elapsed() < Duration::from_secs(3));
    }
}
