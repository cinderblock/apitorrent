//! Taking an imported torrent over from the client it came from: one owner at a time.
//!
//! An imported torrent's files are the other client's files. While Deluge is running and
//! has the torrent active, apitorrent leaves its copy stopped; it runs it only after
//! [`pause_in_deluge`] has paused Deluge's copy and seen it stop, or found no daemon
//! answering for that config directory. The rule is kept in three places: the import
//! (a torrent it would start is taken over first, or imported paused), resuming an
//! imported torrent here, and the bulk "take over" action.
//!
//! The daemon is reached through `apitorrent_deluge::rpc`: port, certificate (pinned) and
//! `localclient` password all come from the config directory, read each time and never
//! stored. A running `deluged` whose command line moved its RPC (`--port`,
//! `--ui-interface`) is reached where it says.
//!
//! "Stopped" is Deluge's own answer, polled after the pause: every torrent must report
//! `Paused` (or `Error`, which Deluge won't restart by itself). Asking is not enough.
//! Then the trackers must have answered Deluge's `stopped` (or 20 s passed), so the tracker
//! sees one client quit before another starts. Deluge's totals stay out of announces: the
//! engine reports only bytes moved since its own `started`.
//! "Not running" needs two things: nothing answers, *and* no `deluged` process uses that
//! config directory. A daemon that runs but can't be reached is an error, never a
//! take-over.

use std::path::Path;
use std::time::Duration;

use apitorrent_deluge::discover::{self, Places};
use apitorrent_deluge::rpc::{AuthFileError, CertError, ConnectOptions, DelugeClient, RpcError};

/// How long Deluge gets to stop the torrents it was asked to pause.
const STOP_TIMEOUT: Duration = Duration::from_secs(30);

/// How long Deluge's trackers get to answer its `stopped` before apitorrent starts anyway:
/// the engine's own wait between a `stopped` and the next `started` (`announce::STOP_WAIT`).
const ANNOUNCE_WAIT: Duration = Duration::from_secs(20);

/// How long a torrent that never showed an announce in flight is watched for one.
const ANNOUNCE_SETTLE: Duration = Duration::from_secs(2);

/// How taking over went.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TakeOver {
    /// Deluge paused them (or no longer had them) and they have stopped.
    Paused,
    /// No daemon answers for that config directory (not running, or the directory and its
    /// files are gone): nothing else runs them.
    NotRunning,
}

/// Pause `ids` (Deluge's ids) in the Deluge daemon that owns `config_dir`, and wait until
/// they have stopped.
///
/// # Errors
///
/// A message for the user: Deluge is running but couldn't be reached properly (wrong
/// certificate, password refused, a timeout) or didn't stop them in time. The caller must
/// then leave its copies stopped.
pub async fn pause_in_deluge(config_dir: &Path, ids: &[String]) -> Result<TakeOver, String> {
    pause_in_deluge_on(&Places::system(), config_dir, ids).await
}

async fn pause_in_deluge_on(
    places: &Places,
    config_dir: &Path,
    ids: &[String],
) -> Result<TakeOver, String> {
    if ids.is_empty() {
        return Ok(TakeOver::Paused);
    }
    let running = discover::running_daemon(places, config_dir);
    let mut client = match connect(config_dir, running.as_ref()).await {
        Ok(c) => c,
        Err(e) if not_running(&e) && running.is_none() => {
            tracing::info!(
                dir = %config_dir.display(),
                "no Deluge daemon answers or runs for this config directory; taking over"
            );
            return Ok(TakeOver::NotRunning);
        }
        Err(e) if not_running(&e) => {
            return Err(format!(
                "a Deluge daemon is running with {} but couldn't be reached to pause it there: {e}",
                config_dir.display()
            ));
        }
        Err(e) => return Err(format!("couldn't reach Deluge to pause it there: {e}")),
    };
    let paused = client
        .pause(ids)
        .await
        .map_err(|e| format!("Deluge refused to pause it: {e}"))?;
    let running = client
        .wait_until_stopped(&paused, STOP_TIMEOUT)
        .await
        .map_err(|e| format!("couldn't check that Deluge stopped it: {e}"))?;
    if let Some(t) = running.first() {
        return Err(format!(
            "Deluge still runs {} ({:?}) {}s after being asked to pause it",
            t.name,
            t.state,
            STOP_TIMEOUT.as_secs()
        ));
    }
    // Deluge's `stopped` goes before apitorrent's `started`: one client quits, then
    // another starts. A tracker that never answers only delays the take-over.
    match client
        .wait_until_announced(&paused, ANNOUNCE_SETTLE, ANNOUNCE_WAIT)
        .await
    {
        Ok(waiting) if !waiting.is_empty() => tracing::warn!(
            dir = %config_dir.display(),
            waiting = waiting.len(),
            "Deluge's trackers didn't answer its stop in {}s; taking over anyway",
            ANNOUNCE_WAIT.as_secs()
        ),
        Ok(_) => {}
        Err(e) => tracing::warn!(
            dir = %config_dir.display(),
            "couldn't check that Deluge told its trackers it stopped ({e}); taking over anyway"
        ),
    }
    tracing::info!(
        dir = %config_dir.display(),
        paused = paused.len(),
        "paused in Deluge to take over"
    );
    Ok(TakeOver::Paused)
}

/// The identity of the Deluge that owns `config_dir`: its exact Deluge and libtorrent
/// versions when its daemon answers, else Deluge 2.1.1 on libtorrent 2.0.10.
pub async fn deluge_identity(config_dir: &Path) -> apitorrent_types::ClientIdentity {
    let fallback = apitorrent_types::ClientIdentity::deluge_default();
    let running = discover::running_daemon(&Places::system(), config_dir);
    let Ok(mut client) = connect(config_dir, running.as_ref()).await else {
        return fallback;
    };
    let Ok(v) = client.versions().await else {
        return fallback;
    };
    let found = apitorrent_types::ClientIdentity::Deluge {
        version: v.deluge,
        libtorrent: v.libtorrent,
    };
    // A version the peer id can't carry (a part above 35, a development suffix it
    // doesn't know) falls back to the release everyone has.
    if crate::settings::client_profile(&found).is_ok() {
        found
    } else {
        fallback
    }
}

/// Connect to the daemon for `config_dir`: where `core.conf` says, unless the running
/// process's command line moved it.
async fn connect(
    config_dir: &Path,
    running: Option<&discover::Running>,
) -> Result<DelugeClient, RpcError> {
    let mut opts = ConnectOptions::from_config_dir(config_dir);
    if let Some(r) = running {
        if let Some(port) = r.port {
            opts.port = port;
        }
        if let Some(i) = &r.interface {
            opts.host = connect_host(i);
        }
    }
    DelugeClient::connect_config_dir_with(config_dir, &opts).await
}

/// The address to reach a daemon listening on `interface` from this machine: loopback for
/// a wildcard, else the address itself.
fn connect_host(interface: &str) -> String {
    let i = interface.trim_matches(['[', ']']);
    match i {
        "" | "0.0.0.0" => "127.0.0.1".to_owned(),
        "::" => "::1".to_owned(),
        _ => i.to_owned(),
    }
}

/// Errors that mean no Deluge daemon is there to compete: nothing listens on its port, or
/// the config directory's certificate or auth file is gone.
fn not_running(e: &RpcError) -> bool {
    match e {
        RpcError::Connect { source, .. } => source.kind() == std::io::ErrorKind::ConnectionRefused,
        RpcError::Cert(CertError::Io { source, .. })
        | RpcError::AuthFile(AuthFileError::Io { source, .. }) => {
            source.kind() == std::io::ErrorKind::NotFound
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_missing_config_dir_is_not_running() {
        let dir = tempfile::tempdir().unwrap();
        let gone = dir.path().join("deluge");
        assert_eq!(
            pause_in_deluge(&gone, &["aa".repeat(20)]).await,
            Ok(TakeOver::NotRunning)
        );
        assert_eq!(pause_in_deluge(&gone, &[]).await, Ok(TakeOver::Paused));
    }

    /// A config dir whose daemon port nothing listens on.
    fn closed_port_dir() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        std::fs::write(
            dir.path().join("core.conf"),
            format!("{{\"file\": 1, \"format\": 1}}{{\"daemon_port\": {port}}}"),
        )
        .unwrap();
        std::fs::write(dir.path().join("auth"), "localclient:secret:10\n").unwrap();
        std::fs::create_dir(dir.path().join("ssl")).unwrap();
        std::fs::copy(
            concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../apitorrent-deluge/tests/fixtures/ssl/a-daemon.cert"
            ),
            dir.path().join("ssl/daemon.cert"),
        )
        .unwrap();
        dir
    }

    /// A machine with no `/proc`, or one whose `/proc` lists these `deluged` command lines.
    fn places(root: &Path, daemons: &[Vec<String>]) -> Places {
        for (pid, args) in daemons.iter().enumerate() {
            let p = root.join(format!("proc/{}", pid + 1));
            std::fs::create_dir_all(&p).unwrap();
            let mut raw = args.join("\0").into_bytes();
            raw.push(0);
            std::fs::write(p.join("cmdline"), raw).unwrap();
        }
        Places {
            proc: Some(root.join("proc")),
            passwd: None,
            unit_dirs: Vec::new(),
            home_parents: Vec::new(),
            candidates: Vec::new(),
        }
    }

    #[tokio::test]
    async fn a_closed_port_is_not_running() {
        let dir = closed_port_dir();
        let machine = tempfile::tempdir().unwrap();
        assert_eq!(
            pause_in_deluge_on(&places(machine.path(), &[]), dir.path(), &["aa".repeat(20)]).await,
            Ok(TakeOver::NotRunning)
        );
    }

    /// Nothing answers where `core.conf` says, but a `deluged` runs with that directory
    /// (here: told to listen on a port that is closed too). That is not "not running":
    /// apitorrent must not start its copy.
    #[tokio::test]
    async fn a_running_daemon_that_does_not_answer_is_an_error() {
        let dir = closed_port_dir();
        let machine = tempfile::tempdir().unwrap();
        let other = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let daemon = vec![
            "/usr/bin/deluged".to_owned(),
            "-c".to_owned(),
            dir.path().display().to_string(),
            "--port".to_owned(),
            other.to_string(),
        ];
        let got = pause_in_deluge_on(
            &places(machine.path(), &[daemon]),
            dir.path(),
            &["aa".repeat(20)],
        )
        .await;
        let e = got.expect_err("a running daemon was taken for a stopped one");
        assert!(e.contains("is running"), "{e}");
        assert!(e.contains(&other.to_string()), "{e}");
    }

    #[test]
    fn listen_addresses_to_connect_to() {
        assert_eq!(connect_host("0.0.0.0"), "127.0.0.1");
        assert_eq!(connect_host(""), "127.0.0.1");
        assert_eq!(connect_host("::"), "::1");
        assert_eq!(connect_host("[::]"), "::1");
        assert_eq!(connect_host("10.0.0.5"), "10.0.0.5");
    }
}
