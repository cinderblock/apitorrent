//! ACME end to end against Pebble, Let's Encrypt's test CA, running locally.
//!
//! Ignored by default because it needs two Go programs:
//!
//! ```text
//! go install github.com/letsencrypt/pebble/v2/cmd/pebble@latest
//! go install github.com/letsencrypt/pebble/v2/cmd/pebble-challtestsrv@latest
//! cargo test -p apitorrent-server --test acme_pebble -- --ignored
//! ```
//!
//! They are looked for in `$APITORRENT_PEBBLE_DIR`, then `~/go/bin`, then `PATH` (see
//! `pebble/mod.rs`, which `apitorrentd`'s DNS-01 test shares). Everything binds to
//! 127.0.0.1 and runs without a window. `pebble-challtestsrv` is only the DNS server here:
//! it answers every name with 127.0.0.1, so Pebble's validator connects to our own
//! listeners for both TLS-ALPN-01 and HTTP-01.

mod common;
mod pebble;

use std::time::Duration;

use apitorrent_auth::PasswordMode;
use apitorrent_server::{AuthMode, TlsServe, TlsServer};
use apitorrent_tls::CertStore;
use apitorrent_tls::acme::{AcmeManager, ChallengeKind};
use common::{AuthFixture, core, server_config};
use pebble::{Pebble, wait_for_acme_cert};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs pebble and pebble-challtestsrv (see the file's docs)"]
async fn acme_issues_with_tls_alpn_01_and_http_01() {
    let root = tempfile::tempdir().unwrap();
    let core = core(root.path()).await;
    let f = AuthFixture::new(root.path(), PasswordMode::Fallback, &[]);

    // Our server first, so Pebble can be told its ports.
    let store = CertStore::new();
    let tls = TlsServe {
        store: store.clone(),
        acme: None,
        http_redirect: Some("127.0.0.1:0".parse().unwrap()),
        https_port: 443,
    };
    let server = TlsServer::bind(
        core.clone(),
        server_config(AuthMode::Enabled(f.auth.clone())),
        tls,
    )
    .await
    .unwrap();
    let tls_port = server.local_addr().unwrap().port();
    let http_port = server.redirect_addr().unwrap().unwrap().port();
    let running = tokio::spawn(server.run(std::future::pending()));

    let pebble = Pebble::start(&root.path().join("pebble"), http_port, tls_port).await;
    let state = root.path().join("acme");

    // TLS-ALPN-01: the validator talks to the HTTPS listener with `acme-tls/1`.
    let manager = AcmeManager::new(
        store.clone(),
        pebble.config(&["alpn.apitorrent.test"], &state, ChallengeKind::TlsAlpn01),
    )
    .unwrap();
    let status = manager.status();
    let handle = manager.start();
    wait_for_acme_cert(&store, &["alpn.apitorrent.test"], &pebble.logs).await;
    handle.stop();
    let s = status.borrow().clone();
    assert_eq!(s.last_error, None);
    assert!(s.last_issued_ms.is_some());

    // The account and certificate were saved, and are reused: a new manager installs the
    // stored certificate without asking the CA.
    let ca_dir = std::fs::read_dir(&state)
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    assert!(ca_dir.join("account.json").is_file());
    assert!(
        ca_dir
            .join("certs")
            .join("alpn.apitorrent.test.pem")
            .is_file()
    );
    let fresh = CertStore::new();
    let again = AcmeManager::new(
        fresh.clone(),
        pebble.config(&["alpn.apitorrent.test"], &state, ChallengeKind::TlsAlpn01),
    )
    .unwrap();
    assert_eq!(
        fresh.current().unwrap().info.spki_sha256,
        store.current().unwrap().info.spki_sha256
    );
    // Renewal is scheduled for 2/3 of the way through (Pebble issues 90-day certificates
    // by default; allow for jitter and Pebble's own validity choices).
    let handle = again.start();
    let mut next = None;
    for _ in 0..50 {
        next = handle.status().borrow().next_attempt_ms;
        if next.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    handle.stop();
    let info = fresh.current().unwrap().info.clone();
    let as_ms = |t: std::time::SystemTime| {
        i64::try_from(t.duration_since(std::time::UNIX_EPOCH).unwrap().as_millis()).unwrap()
    };
    let (nb, na) = (as_ms(info.not_before), as_ms(info.not_after));
    let next = next.expect("a renewal time");
    let at_two_thirds = nb + (na - nb) * 2 / 3;
    assert!(next <= at_two_thirds && next >= at_two_thirds - (na - nb) / 30 - 1000);

    // HTTP-01: the validator fetches the token from the plain-HTTP listener.
    let manager = AcmeManager::new(
        store.clone(),
        pebble.config(&["http.apitorrent.test"], &state, ChallengeKind::Http01),
    )
    .unwrap();
    let handle = manager.start();
    wait_for_acme_cert(&store, &["http.apitorrent.test"], &pebble.logs).await;
    handle.stop();

    running.abort();
    core.shutdown().await.unwrap();
}
