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
//! stored.

use std::path::Path;
use std::time::Duration;

use apitorrent_deluge::rpc::{AuthFileError, CertError, DelugeClient, RpcError};

/// How long Deluge gets to stop the torrents it was asked to pause.
const STOP_TIMEOUT: Duration = Duration::from_secs(30);

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
    if ids.is_empty() {
        return Ok(TakeOver::Paused);
    }
    let mut client = match DelugeClient::connect_config_dir(config_dir).await {
        Ok(c) => c,
        Err(e) if not_running(&e) => {
            tracing::info!(
                dir = %config_dir.display(),
                "no Deluge daemon answers for this config directory; taking over"
            );
            return Ok(TakeOver::NotRunning);
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
    tracing::info!(
        dir = %config_dir.display(),
        paused = paused.len(),
        "paused in Deluge to take over"
    );
    Ok(TakeOver::Paused)
}

/// Errors that mean no Deluge daemon is there to compete: nothing listens on its port, or
/// the config directory's certificate or auth file is gone.
fn not_running(e: &RpcError) -> bool {
    match e {
        RpcError::Connect { source, .. } => {
            source.kind() == std::io::ErrorKind::ConnectionRefused
        }
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

    #[tokio::test]
    async fn a_closed_port_is_not_running() {
        // A config dir whose daemon port nothing listens on.
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
        assert_eq!(
            pause_in_deluge(dir.path(), &["aa".repeat(20)]).await,
            Ok(TakeOver::NotRunning)
        );
    }
}
