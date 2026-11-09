//! Pebble (Let's Encrypt's test CA) and `pebble-challtestsrv` (its DNS server), running
//! locally for the ACME tests. Shared with `apitorrentd`'s DNS-01 test, which includes this
//! file by path.
//!
//! The programs are looked for in `$APITORRENT_PEBBLE_DIR`, then `~/go/bin`, then `PATH`:
//!
//! ```text
//! go install github.com/letsencrypt/pebble/v2/cmd/pebble@latest
//! go install github.com/letsencrypt/pebble/v2/cmd/pebble-challtestsrv@latest
//! ```
//!
//! Everything binds to 127.0.0.1 and runs without a window. `pebble-challtestsrv` answers
//! every name with 127.0.0.1, so Pebble's validator connects to the test's own listeners for
//! TLS-ALPN-01 and HTTP-01, and it serves the TXT records a test sets through its
//! management API for DNS-01.

#![allow(dead_code)]

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use apitorrent_tls::acme::{AcmeConfig, ChallengeKind};
use apitorrent_tls::{CertSource, CertStore};

pub fn find(name: &str) -> PathBuf {
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
        .unwrap_or_else(|| panic!("{exe} not found; see tests/pebble/mod.rs to install it"))
}

pub fn free_tcp_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

pub fn free_udp_port() -> u16 {
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
pub struct Pebble {
    _dns: tokio::process::Child,
    _pebble: tokio::process::Child,
    pub directory: String,
    pub roots: Vec<apitorrent_tls::CertificateDer<'static>>,
    pub logs: PathBuf,
    /// `pebble-challtestsrv`'s DNS server, which Pebble's validator asks.
    pub dns: SocketAddr,
    /// Its management API (`http://127.0.0.1:<port>`).
    pub dns_management: SocketAddr,
}

impl Pebble {
    pub async fn start(dir: &Path, http_port: u16, tls_port: u16) -> Self {
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
            dns: dns_addr.parse().unwrap(),
            dns_management: SocketAddr::from(([127, 0, 0, 1], dns_mgmt)),
        };
        // Wait for the directory and the DNS management API to answer.
        for port in [listen, dns_mgmt] {
            let mut up = false;
            for _ in 0..100 {
                if tokio::net::TcpStream::connect(("127.0.0.1", port))
                    .await
                    .is_ok()
                {
                    up = true;
                    break;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            assert!(up, "pebble didn't start; see {}", this.logs.display());
        }
        this
    }

    pub fn config(&self, domains: &[&str], state: &Path, challenge: ChallengeKind) -> AcmeConfig {
        let mut c = AcmeConfig::new(
            domains.iter().map(|d| (*d).to_owned()).collect(),
            state.to_owned(),
            challenge,
        );
        c.directory_url.clone_from(&self.directory);
        c.directory_roots.clone_from(&self.roots);
        c.contact_email = Some("admin@apitorrent.test".into());
        c.poll_timeout = Duration::from_secs(60);
        c
    }

    /// POST JSON to `pebble-challtestsrv`'s management API.
    async fn manage(&self, path: &str, body: &serde_json::Value) {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
        let body = body.to_string();
        let mut s = tokio::net::TcpStream::connect(self.dns_management)
            .await
            .unwrap();
        let req = format!(
            "POST {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Type: application/json\r\n\
             Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        s.write_all(req.as_bytes()).await.unwrap();
        let mut out = Vec::new();
        s.read_to_end(&mut out).await.unwrap();
        let text = String::from_utf8_lossy(&out);
        assert!(text.starts_with("HTTP/1.1 200"), "{path}: {text}");
    }

    /// Serve a TXT value for `host` from the DNS server (in addition to any already set).
    pub async fn set_txt(&self, host: &str, value: &str) {
        let host = format!("{}.", host.trim_end_matches('.'));
        self.manage(
            "/set-txt",
            &serde_json::json!({ "host": host, "value": value }),
        )
        .await;
    }

    /// Stop serving every TXT value for `host`.
    pub async fn clear_txt(&self, host: &str) {
        let host = format!("{}.", host.trim_end_matches('.'));
        self.manage("/clear-txt", &serde_json::json!({ "host": host }))
            .await;
    }
}

pub async fn wait_for_acme_cert(store: &CertStore, domains: &[&str], logs: &Path) {
    for _ in 0..600 {
        if let Some(c) = store.current()
            && c.source == CertSource::Acme
            && c.info.dns_names == domains
        {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!(
        "no certificate for {domains:?} within 60 s; see the logs in {}",
        logs.display()
    );
}
