//! Port forwarding on the router, so peers outside can connect in: NAT-PMP (RFC 6886)
//! first, since it is quick and exact, then UPnP IGD.
//!
//! [`PortMapper::start`] keeps the forwards in place in the background: it finds the
//! gateway, asks for each port with a lease of [`LEASE`], renews at half the lease, and
//! starts over (after [`RETRY`]) when the router stops answering or refuses. Stopping
//! removes the forwards. Nothing here fails the caller: a network without a router that
//! takes part simply has no forwards, which [`Status`] says.

pub mod natpmp;

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{Notify, watch};

/// How long each forward is asked for. Routers keep the lease even if we go away without
/// removing it, so it stays short.
pub const LEASE: Duration = Duration::from_secs(3600);
/// How long to wait before trying again after no router took the forwards.
pub const RETRY: Duration = Duration::from_secs(300);
/// How long to look for a UPnP gateway.
const UPNP_SEARCH: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Protocol {
    Tcp,
    Udp,
}

/// A port on this machine to forward. The outside port is asked to be the same.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Request {
    pub protocol: Protocol,
    pub port: u16,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    pub natpmp: bool,
    pub upnp: bool,
    /// Shown in the router's list of UPnP forwards.
    pub description: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Method {
    NatPmp,
    Upnp,
}

/// A forward in place.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Forward {
    pub protocol: Protocol,
    pub internal: u16,
    pub external: u16,
}

/// Where the forwards stand.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Status {
    /// How the forwards were made; `None` while none are in place.
    pub method: Option<Method>,
    pub gateway: Option<IpAddr>,
    pub external_ip: Option<IpAddr>,
    pub forwards: Vec<Forward>,
    /// Why there are none, after the last attempt.
    pub error: Option<String>,
}

/// Keeps the forwards in place until [`PortMapper::stop`].
#[derive(Debug)]
pub struct PortMapper {
    status: watch::Receiver<Status>,
    stop: Arc<Notify>,
    task: tokio::task::JoinHandle<()>,
}

impl PortMapper {
    /// Start keeping `requests` forwarded. With both methods off it does nothing.
    pub fn start(config: Config, requests: Vec<Request>) -> Self {
        let (tx, status) = watch::channel(Status::default());
        let stop = Arc::new(Notify::new());
        let task = tokio::spawn(run(config, requests, tx, stop.clone()));
        Self { status, stop, task }
    }

    pub fn status(&self) -> Status {
        self.status.borrow().clone()
    }

    /// Remove the forwards and stop.
    pub async fn stop(self) {
        self.stop.notify_one();
        let _ = self.task.await;
    }
}

/// What is in place, to renew and to remove.
enum Held {
    NatPmp {
        server: SocketAddr,
        granted: Vec<(Protocol, natpmp::Granted)>,
    },
    Upnp {
        gateway: igd_next::aio::Gateway<igd_next::aio::tokio::Tokio>,
        forwards: Vec<Forward>,
    },
}

async fn run(config: Config, requests: Vec<Request>, tx: watch::Sender<Status>, stop: Arc<Notify>) {
    if !(config.natpmp || config.upnp) || requests.is_empty() {
        return;
    }
    let mut held: Option<Held> = None;
    loop {
        let wait = match attempt(&config, &requests).await {
            Ok((h, status)) => {
                tracing::info!(
                    method = ?status.method,
                    external = ?status.external_ip,
                    forwards = ?status.forwards,
                    "port forwards in place"
                );
                tx.send_replace(status);
                held = Some(h);
                LEASE / 2
            }
            Err(error) => {
                tracing::info!(%error, "no port forwards");
                if let Some(h) = held.take() {
                    release(h).await;
                }
                tx.send_replace(Status {
                    error: Some(error),
                    ..Status::default()
                });
                RETRY
            }
        };
        tokio::select! {
            () = tokio::time::sleep(wait) => {}
            () = stop.notified() => break,
        }
    }
    if let Some(h) = held {
        release(h).await;
    }
    tx.send_replace(Status::default());
}

/// The default gateway's IPv4 address.
async fn default_gateway() -> Result<Ipv4Addr, String> {
    tokio::task::spawn_blocking(|| {
        netdev::get_default_gateway()
            .map_err(|e| format!("no default gateway: {e}"))?
            .ipv4
            .first()
            .copied()
            .ok_or_else(|| "the default gateway has no IPv4 address".to_owned())
    })
    .await
    .map_err(|e| e.to_string())?
}

/// The address this machine reaches `peer` from.
async fn local_ip_towards(peer: IpAddr) -> Result<IpAddr, String> {
    let s = tokio::net::UdpSocket::bind(SocketAddr::from(([0, 0, 0, 0], 0)))
        .await
        .map_err(|e| e.to_string())?;
    s.connect(SocketAddr::new(peer, 9))
        .await
        .map_err(|e| e.to_string())?;
    Ok(s.local_addr().map_err(|e| e.to_string())?.ip())
}

/// Make (or renew) every forward: NAT-PMP, then UPnP.
async fn attempt(config: &Config, requests: &[Request]) -> Result<(Held, Status), String> {
    let mut errors = Vec::new();
    if config.natpmp {
        match try_natpmp(requests).await {
            Ok(r) => return Ok(r),
            Err(e) => errors.push(format!("NAT-PMP: {e}")),
        }
    }
    if config.upnp {
        match try_upnp(config, requests).await {
            Ok(r) => return Ok(r),
            Err(e) => errors.push(format!("UPnP: {e}")),
        }
    }
    Err(errors.join("; "))
}

async fn try_natpmp(requests: &[Request]) -> Result<(Held, Status), String> {
    let gateway = default_gateway().await?;
    let server = SocketAddr::from((gateway, natpmp::PORT));
    let external = natpmp::external_address(server)
        .await
        .map_err(|e| e.to_string())?;
    let lease = u32::try_from(LEASE.as_secs()).unwrap_or(u32::MAX);
    let mut granted = Vec::new();
    for r in requests {
        let g = natpmp::map(server, r.protocol, r.port, r.port, lease)
            .await
            .map_err(|e| format!("port {} {:?}: {e}", r.port, r.protocol))?;
        granted.push((r.protocol, g));
    }
    let status = Status {
        method: Some(Method::NatPmp),
        gateway: Some(gateway.into()),
        external_ip: Some(external.into()),
        forwards: granted
            .iter()
            .map(|(protocol, g)| Forward {
                protocol: *protocol,
                internal: g.internal,
                external: g.external,
            })
            .collect(),
        error: None,
    };
    Ok((Held::NatPmp { server, granted }, status))
}

async fn try_upnp(config: &Config, requests: &[Request]) -> Result<(Held, Status), String> {
    use igd_next::PortMappingProtocol as P;
    let mut search = igd_next::SearchOptions::default();
    search.timeout = Some(UPNP_SEARCH);
    let gateway = igd_next::aio::tokio::search_gateway(search)
        .await
        .map_err(|e| e.to_string())?;
    let local = local_ip_towards(gateway.addr.ip()).await?;
    let lease = u32::try_from(LEASE.as_secs()).unwrap_or(u32::MAX);
    let mut forwards = Vec::new();
    for r in requests {
        let protocol = match r.protocol {
            Protocol::Tcp => P::TCP,
            Protocol::Udp => P::UDP,
        };
        gateway
            .add_port(
                protocol,
                r.port,
                SocketAddr::new(local, r.port),
                lease,
                &config.description,
            )
            .await
            .map_err(|e| format!("port {} {:?}: {e}", r.port, r.protocol))?;
        forwards.push(Forward {
            protocol: r.protocol,
            internal: r.port,
            external: r.port,
        });
    }
    let external_ip = gateway.get_external_ip().await.ok();
    let status = Status {
        method: Some(Method::Upnp),
        gateway: Some(gateway.addr.ip()),
        external_ip,
        forwards: forwards.clone(),
        error: None,
    };
    Ok((Held::Upnp { gateway, forwards }, status))
}

/// Remove forwards made earlier. Failures only log: the lease ends them anyway.
async fn release(held: Held) {
    match held {
        Held::NatPmp { server, granted } => {
            for (protocol, g) in granted {
                if let Err(e) = natpmp::map(server, protocol, g.internal, 0, 0).await {
                    tracing::debug!(error = %e, port = g.internal, "NAT-PMP removal failed");
                }
            }
        }
        Held::Upnp { gateway, forwards } => {
            for f in forwards {
                let protocol = match f.protocol {
                    Protocol::Tcp => igd_next::PortMappingProtocol::TCP,
                    Protocol::Udp => igd_next::PortMappingProtocol::UDP,
                };
                if let Err(e) = gateway.remove_port(protocol, f.external).await {
                    tracing::debug!(error = %e, port = f.external, "UPnP removal failed");
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn off_does_nothing() {
        let m = PortMapper::start(
            Config {
                natpmp: false,
                upnp: false,
                description: "test".into(),
            },
            vec![Request {
                protocol: Protocol::Tcp,
                port: 1,
            }],
        );
        m.stop().await;
    }
}
