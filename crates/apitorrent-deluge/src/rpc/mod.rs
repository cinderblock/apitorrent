//! A client for Deluge 2's daemon RPC: the protocol `deluged` speaks to its GTK and console
//! clients (port 58846), not the web UI's JSON-RPC.
//!
//! apitorrent uses it to pause a torrent in Deluge before taking it over, and to read
//! Deluge's and libtorrent's versions. Checked against Deluge 2.1.1's source
//! (`deluge/transfer.py`, `core/rpcserver.py`, `core/core.py`, `core/authmanager.py`,
//! `crypto_utils.py`, `ui/client.py`) and against a running 2.1.1 daemon (see
//! `tests/deluge_daemon.rs`).
//!
//! # The protocol
//!
//! TLS (1.2 or 1.3) over TCP. Each message is a 5-byte header, version `1` and a big-endian
//! `u32` length, then `zlib(rencode(value))` ([`frame`], [`rencode`]). A request message is a
//! list of `[request_id, "object.method", args, kwargs]`; Deluge answers each with
//! `[1, id, result]` or `[2, id, exception_class, exception_args, exception_kwargs,
//! traceback]`, and sends `[3, event_name, args]` to clients that asked for events (we
//! don't). The first call must be `daemon.login(username, password, client_version=…)`.
//!
//! # Trust and credentials
//!
//! Everything comes from the daemon's config directory, read on each connect and never
//! stored: the port from `core.conf`, the `localclient` password from `auth`
//! ([`Credentials`]), and the daemon's self-signed certificate from `ssl/daemon.cert`, which
//! is pinned: no other certificate is accepted ([`PinnedCert`]).
//!
//! ```no_run
//! # async fn f() -> Result<(), apitorrent_deluge::rpc::RpcError> {
//! use std::time::Duration;
//! use apitorrent_deluge::rpc::DelugeClient;
//!
//! let mut deluge = DelugeClient::connect_config_dir("/home/me/.config/deluge".as_ref()).await?;
//! let ids = ["0123456789abcdef0123456789abcdef01234567"];
//! deluge.pause(&ids).await?;
//! let still_running = deluge.wait_until_stopped(&ids, Duration::from_secs(10)).await?;
//! assert!(still_running.is_empty());
//! # Ok(()) }
//! ```
//!
//! # Deluge's sharp edges, handled here
//!
//! - `core.pause_torrent([])` and `core.resume_torrent([])` act on **every** torrent, and
//!   both stop at the first id Deluge doesn't have with a bare `KeyError`.
//!   [`DelugeClient::pause`] and [`DelugeClient::resume`] look the ids up first, act only on
//!   the known ones, and send nothing when there are none.
//! - Pausing is asynchronous: the call returns before libtorrent has paused.
//!   [`DelugeClient::wait_until_stopped`] polls until the state is `Paused`.
//! - The daemon's certificate is X.509 v1, which webpki can't parse; see [`tls`].

pub mod auth;
pub mod client;
pub mod frame;
pub mod rencode;
pub mod tls;

pub use auth::{AuthFileError, Credentials, LOCALCLIENT};
pub use client::{
    ConnectOptions, DEFAULT_PORT, DelugeClient, RemoteError, RpcError, TorrentState, TorrentStatus,
    Tracker, Versions,
};
pub use frame::FrameLimits;
pub use rencode::Value;
pub use tls::{CertError, PinnedCert};
