//! uTP, the micro transport protocol ([BEP 29]): reliable, ordered byte streams over UDP
//! with LEDBAT congestion control, so BitTorrent traffic yields to everything else on the
//! link.
//!
//! Two layers, as in the rest of apitorrent:
//!
//! - [`Connection`]: one connection as a sans-IO state machine (handshake, selective ACKs,
//!   fast retransmit, RTO backoff, LEDBAT, receive windows, FIN and RESET). It takes
//!   packets and time, and returns datagrams and deadlines, so it is tested on simulated
//!   time.
//! - [`UtpSocket`]: a tokio driver that multiplexes every connection on one UDP port and
//!   hands out [`UtpStream`]s, which implement `AsyncRead + AsyncWrite`.
//!
//! ```no_run
//! # async fn demo() -> std::io::Result<()> {
//! use apitorrent_utp::UtpSocket;
//! use tokio::io::AsyncWriteExt;
//! let socket = UtpSocket::bind("0.0.0.0:6881".parse().unwrap()).await?;
//! let mut stream = socket.connect("192.0.2.1:6881".parse().unwrap()).await?;
//! stream.write_all(b"\x13BitTorrent protocol").await?;
//! # Ok(()) }
//! ```
//!
//! # Sharing the port with the DHT
//!
//! BitTorrent clients run uTP and the DHT on the same UDP port. Two ways to do that:
//!
//! - Let [`UtpSocket::bind`] own the socket. Datagrams that aren't uTP (a DHT message is a
//!   bencoded dictionary, so it starts with `d`, which is never a valid uTP first byte)
//!   arrive on [`UtpSocket::take_unrecognized`], and [`UtpSocket::send_to`] sends on the
//!   same socket.
//! - Let the other protocol own it. Build the socket with [`UtpSocket::with_transport`],
//!   giving it a [`Transport`] to send with, and pass incoming uTP datagrams to
//!   [`UtpSocket::handle_datagram`] ([`looks_like_utp`] tells them apart).
//!
//! # Path MTU
//!
//! Packets are at most [`Config::packet_size`] bytes, 1400 by default: that fits Ethernet,
//! `PPPoE` and common VPN paths for IPv4 and IPv6. We don't probe for a larger path MTU.
//! Probing only works with the don't-fragment bit set: otherwise an oversized probe is
//! fragmented and still arrives, which looks like success. tokio doesn't expose that
//! socket option, and setting it ourselves needs unsafe FFI, which the workspace denies.
//! The size is configurable from 1232 (safe on any IPv6 path) to 1472.
//!
//! [BEP 29]: https://www.bittorrent.org/beps/bep_0029.html

mod config;
mod congestion;
mod conn;
pub mod packet;
mod socket;

pub use config::Config;
pub use congestion::{Ledbat, Rtt};
pub use conn::{ConnError, Connection, State, Stats};
pub use packet::looks_like_utp;
pub use socket::{Transport, UtpReadHalf, UtpSocket, UtpStream, UtpWriteHalf};
