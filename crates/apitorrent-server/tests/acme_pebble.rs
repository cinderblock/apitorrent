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
//! They are looked for in `$APITORRENT_PEBBLE_DIR`, then `~/go/bin`, then `PATH`. Everything
//! binds to 127.0.0.1 and runs without a window. `pebble-challtestsrv` is only the DNS
//! server here: it answers every name with 127.0.0.1, so Pebble's validator connects to our
//! own listeners for both TLS-ALPN-01 and HTTP-01.

mod common;

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use apitorrent_auth::PasswordMode;
use apitorrent_server::{AuthMode, TlsServe, TlsServer};
use apitorrent_tls::acme::{AcmeConfig, AcmeManager, ChallengeKind};
use apitorrent_tls::{CertSource, CertStore};
use common::{AuthFixture, core, server_config};

fn find(name: &str) -> PathBuf {
    let exe = format!("{name}{}", std::env::consts::EXE_SUFFIX);
    let mut dirs: Vec<PathBuf> = Vec::new();
    if let Some(d) = std::env::var_os("APITORRENT_PEBBLE_DIR") {
        dirs.push(d.into());
    }
    if let Some(home) = std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME")) {
        dirs.push(Path::new(&home).join("go").join("bin"));
    }
    if let Some(path) = std::env::var_os("PATH") {
        dirs.extend(std::env::split_paths(&path));
    }
    dirs.into_iter()
        .map(|d| d.join(&exe))
        .find(|p| p.is_file())
        .unwrap_or_else(|| panic!("{exe} not found; see this file's docs to install it"))
}

fn free_tcp_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn free_udp_port() -> u16 {
    std::net::UdpSocket::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn spawn(program: &Path, args: &[&str], log: &Path) -> tokio::process::Child {
    let mut cmd = tokio::process::Command::new(program);
    cmd.args(args)
        .env("PEBBLE_VA_NOSLEEP", "1")
        .env("PEBBLE_WFE_NONCEREJECT", "0")
        .env("PEBBLE_AUTHZREUSE", "0")
        .stdin(Stdio::null())
        .stdout(std::fs::File::create(log).unwrap())
        .stderr(std::fs::File::create(log.with_extension("err")).unwrap())
        .kill_on_drop(true);
    #[cfg(windows)]
    cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW: never put a console on screen.
    cmd.spawn().unwrap()
}

/// Pebble and its DNS server, killed on drop.
struct Pebble {
    _dns: tokio::process::Child,
    _pebble: tokio::process::Child,
    directory: String,
    roots: Vec<apitorrent_tls::CertificateDer<'static>>,
    logs: PathBuf,
}

impl Pebble {
    async fn start(dir: &Path, http_port: u16, tls_port: u16) -> Self {
        // Pebble's own HTTPS certificate, from a CA only this test trusts.
        let ca_key = rcgen::KeyPair::generate().unwrap();
        let mut ca_params = rcgen::CertificateParams::new(Vec::<String>::new()).unwrap();
        ca_params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
        let ca_cert = ca_params.self_signed(&ca_key).unwrap();
        let issuer = rcgen::Issuer::new(ca_params, ca_key);
        let key = rcgen::KeyPair::generate().unwrap();
        let leaf = rcgen::CertificateParams::new(vec!["127.0.0.1".to_owned()])
            .unwrap()
            .signed_by(&key, &issuer)
            .unwrap();
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(dir.join("pebble-cert.pem"), leaf.pem()).unwrap();
        std::fs::write(dir.join("pebble-key.pem"), key.serialize_pem()).unwrap();

        let dns_port = free_udp_port();
        let dns_mgmt = free_tcp_port();
        let listen = free_tcp_port();
        let mgmt = free_tcp_port();
        let config = serde_json::json!({
            "pebble": {
                "listenAddress": format!("127.0.0.1:{listen}"),
                "managementListenAddress": format!("127.0.0.1:{mgmt}"),
                "certificate": dir.join("pebble-cert.pem"),
                "privateKey": dir.join("pebble-key.pem"),
                "httpPort": http_port,
                "tlsPort": tls_port,
                "ocspResponderURL": "",
                "externalAccountBindingRequired": false,
                "retryAfter": { "authz": 1, "order": 1 },
            }
        });
        std::fs::write(dir.join("pebble.json"), config.to_string()).unwrap();

        let dns_addr = format!("127.0.0.1:{dns_port}");
        let dns = spawn(
            &find("pebble-challtestsrv"),
            &[
                "-dnsserver",
                &dns_addr,
                "-management",
                &format!("127.0.0.1:{dns_mgmt}"),
                "-defaultIPv4",
                "127.0.0.1",
                "-defaultIPv6",
                "",
                "-http01",
                "",
                "-https01",
                "",
                "-tlsalpn01",
                "",
                "-doh",
                "",
            ],
            &dir.join("challtestsrv.log"),
        );
        let pebble = spawn(
            &find("pebble"),
            &[
                "-config",
                dir.join("pebble.json").to_str().unwrap(),
                "-dnsserver",
                &dns_addr,
            ],
            &dir.join("pebble.log"),
        );
        let this = Self {
            _dns: dns,
            _pebble: pebble,
            directory: format!("https://127.0.0.1:{listen}/dir"),
            roots: vec![ca_cert.der().clone()],
            logs: dir.to_owned(),
        };
        // Wait for the directory to answer.
        for _ in 0..100 {
            if tokio::net::TcpStream::connect(("127.0.0.1", listen))
                .await
                .is_ok()
            {
                return this;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        panic!("pebble didn't start; see {}", this.logs.display());
    }

    fn config(&self, domain: &str, state: &Path, challenge: ChallengeKind) -> AcmeConfig {
        let mut c = AcmeConfig::new(vec![domain.to_owned()], state.to_owned(), challenge);
        c.directory_url.clone_from(&self.directory);
        c.directory_roots.clone_from(&self.roots);
        c.contact_email = Some("admin@apitorrent.test".into());
        c.poll_timeout = Duration::from_secs(60);
        c
    }
}

async fn wait_for_acme_cert(store: &CertStore, domain: &str, logs: &Path) {
    for _ in 0..600 {
        if let Some(c) = store.current()
            && c.source == CertSource::Acme
            && c.info.dns_names == [domain]
        {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!(
        "no certificate for {domain} within 60 s; see the logs in {}",
        logs.display()
    );
}

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
        pebble.config("alpn.apitorrent.test", &state, ChallengeKind::TlsAlpn01),
    )
    .unwrap();
    let status = manager.status();
    let handle = manager.start();
    wait_for_acme_cert(&store, "alpn.apitorrent.test", &pebble.logs).await;
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
        pebble.config("alpn.apitorrent.test", &state, ChallengeKind::TlsAlpn01),
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
        pebble.config("http.apitorrent.test", &state, ChallengeKind::Http01),
    )
    .unwrap();
    let handle = manager.start();
    wait_for_acme_cert(&store, "http.apitorrent.test", &pebble.logs).await;
    handle.stop();

    running.abort();
    core.shutdown().await.unwrap();
}
