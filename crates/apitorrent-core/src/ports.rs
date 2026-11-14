//! The peer and DHT ports: chosen at random the first time, then kept.
//!
//! The BitTorrent ports most clients default to (6881 and up) are the ones another client
//! on the same machine is most likely to hold already. So when the daemon or the desktop
//! app isn't told a port, [`resolve`] picks a free one from a high range and stores it in
//! the instance settings: trackers, peers and port forwards then see the same port on
//! every start. An explicit port (`--peer-listen`, `--dht-port`, a desktop profile's
//! port) always wins and is never stored.

use std::net::{Ipv4Addr, SocketAddr, TcpListener, UdpSocket};
use std::path::Path;

use apitorrent_engine::SessionConfig;
use apitorrent_types::Settings;
use rand::RngExt;

use crate::db::Database;
use crate::{CoreError, SETTINGS_KEY, settings};

/// Ports picked at random come from here: above the registered ports most services use,
/// and below the range operating systems hand out for outgoing connections.
pub const RANGE: std::ops::RangeInclusive<u16> = 20_000..=48_999;

/// How many random ports to try before giving up and letting the OS choose.
const ATTEMPTS: usize = 64;

/// Fill in `session`'s automatic ports (port 0 on the peer listen address or the DHT's
/// bind addresses): the stored one if it is still free, else a new free one, which is
/// stored. Run before `Core::start`, with the daemon not running.
///
/// # Errors
///
/// The database can't be opened or written.
pub fn resolve(data_dir: &Path, session: &mut SessionConfig) -> Result<(), CoreError> {
    let peer_auto = session.listen.port() == 0;
    let dht_auto = session
        .dht
        .as_ref()
        .is_some_and(|d| d.bind_v4.or(d.bind_v6).is_some_and(|a| a.port() == 0));
    if !peer_auto && !dht_auto {
        return Ok(());
    }
    std::fs::create_dir_all(data_dir)?;
    let db = Database::open(&data_dir.join("apitorrent.db"))?;
    let mut s: Settings = db
        .meta(SETTINGS_KEY)?
        .and_then(|j| serde_json::from_str(&j).ok())
        .unwrap_or_else(|| settings::defaults(session));
    let before = (s.peer_port, s.dht_port);
    if peer_auto {
        let ip = session.listen.ip();
        let stored = s.peer_port;
        let port = if stored != 0 && peer_port_free(ip, stored, session.utp) {
            stored
        } else {
            let port = pick(|p| peer_port_free(ip, p, session.utp) && p != s.dht_port);
            if stored != 0 {
                tracing::warn!(
                    old = stored,
                    new = port,
                    "the stored peer port is taken; using a new one (update any port forward)"
                );
            }
            port
        };
        session.listen.set_port(port);
        s.peer_port = port;
    }
    if dht_auto && let Some(dht) = session.dht.as_mut() {
        let stored = s.dht_port;
        let peer = session.listen.port();
        let port = if stored != 0 && stored != peer && udp_free(stored) {
            stored
        } else {
            let port = pick(|p| p != peer && udp_free(p));
            if stored != 0 {
                tracing::warn!(
                    old = stored,
                    new = port,
                    "the stored DHT port is taken; using a new one"
                );
            }
            port
        };
        for a in [dht.bind_v4.as_mut(), dht.bind_v6.as_mut()]
            .into_iter()
            .flatten()
        {
            if a.port() == 0 {
                a.set_port(port);
            }
        }
        s.dht_port = port;
    }
    if (s.peer_port, s.dht_port) != before {
        let json = serde_json::to_string(&s).expect("settings serialize");
        db.set_meta(SETTINGS_KEY, &json)?;
        tracing::info!(peer = s.peer_port, dht = s.dht_port, "chose ports");
    }
    Ok(())
}

/// A random port in [`RANGE`] that `free` accepts; 0 (the OS chooses, not kept) if none
/// does after a good number of tries.
fn pick(free: impl Fn(u16) -> bool) -> u16 {
    let mut rng = rand::rng();
    (0..ATTEMPTS)
        .map(|_| rng.random_range(RANGE))
        .find(|&p| free(p))
        .unwrap_or(0)
}

/// Whether TCP (and UDP, for uTP) can bind `port` on `ip` right now.
fn peer_port_free(ip: std::net::IpAddr, port: u16, utp: bool) -> bool {
    let addr = SocketAddr::new(ip, port);
    let Ok(tcp) = TcpListener::bind(addr) else {
        return false;
    };
    let ok = !utp || UdpSocket::bind(addr).is_ok();
    drop(tcp);
    ok
}

/// Whether UDP can bind `port` on all IPv4 addresses right now.
fn udp_free(port: u16) -> bool {
    UdpSocket::bind((Ipv4Addr::UNSPECIFIED, port)).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn auto_session() -> SessionConfig {
        SessionConfig {
            listen: SocketAddr::from(([127, 0, 0, 1], 0)),
            dht: Some(apitorrent_dht::DhtConfig {
                bind_v4: Some(SocketAddr::from(([0, 0, 0, 0], 0))),
                bind_v6: None,
                ..apitorrent_dht::DhtConfig::default()
            }),
            ..SessionConfig::default()
        }
    }

    #[test]
    fn picks_once_then_keeps() {
        let dir = tempfile::tempdir().unwrap();
        let mut first = auto_session();
        resolve(dir.path(), &mut first).unwrap();
        let peer = first.listen.port();
        let dht = first.dht.as_ref().unwrap().bind_v4.unwrap().port();
        assert!(RANGE.contains(&peer), "{peer}");
        assert!(RANGE.contains(&dht), "{dht}");
        assert_ne!(peer, dht);

        let mut second = auto_session();
        resolve(dir.path(), &mut second).unwrap();
        assert_eq!(second.listen.port(), peer);
        assert_eq!(second.dht.as_ref().unwrap().bind_v4.unwrap().port(), dht);
    }

    #[test]
    fn a_taken_port_is_replaced() {
        let dir = tempfile::tempdir().unwrap();
        let mut first = auto_session();
        resolve(dir.path(), &mut first).unwrap();
        let peer = first.listen.port();
        // Something else takes it before the next start.
        let _held = TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], peer))).unwrap();
        let mut second = auto_session();
        resolve(dir.path(), &mut second).unwrap();
        assert_ne!(second.listen.port(), peer);
        let mut third = auto_session();
        resolve(dir.path(), &mut third).unwrap();
        assert_eq!(
            third.listen.port(),
            second.listen.port(),
            "the new one is kept"
        );
    }

    #[test]
    fn explicit_ports_are_left_alone_and_not_stored() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = SessionConfig {
            listen: SocketAddr::from(([127, 0, 0, 1], 6881)),
            dht: None,
            ..SessionConfig::default()
        };
        resolve(dir.path(), &mut s).unwrap();
        assert_eq!(s.listen.port(), 6881);
        assert!(!dir.path().join("apitorrent.db").exists());
    }
}
