//! End to end against a GitHub-shaped server on loopback, with a key pair made here.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use apitorrent_update::{
    Manifest, ManifestAsset, Source, UpdateError, UpdateKey, Updater, UpdaterConfig, manifest,
};
use axum::Router;
use axum::extract::{Path as UrlPath, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::get;
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use sha2::Digest as _;

const TARGET: &str = "test-target";

struct Key {
    sk: minisign::SecretKey,
    pk: minisign::PublicKey,
}

impl Key {
    fn new() -> Self {
        let minisign::KeyPair { pk, sk } =
            minisign::KeyPair::generate_unencrypted_keypair().unwrap();
        Self { sk, pk }
    }
    fn public(&self) -> UpdateKey {
        UpdateKey::parse(&STANDARD.encode(self.pk.to_box().unwrap().into_string())).unwrap()
    }
    fn sign(&self, data: &[u8]) -> String {
        let sig = minisign::sign(Some(&self.pk), &self.sk, data, None, None).unwrap();
        STANDARD.encode(sig.into_string())
    }
}

/// What the fake release server serves.
#[derive(Default)]
struct Files {
    release_json: String,
    files: Vec<(String, Vec<u8>)>,
}

type Shared = Arc<Mutex<Files>>;

async fn latest(State(f): State<Shared>) -> impl IntoResponse {
    let json = f.lock().unwrap().release_json.clone();
    if json.is_empty() {
        return (StatusCode::NOT_FOUND, String::new());
    }
    (StatusCode::OK, json)
}

async fn file(State(f): State<Shared>, UrlPath(name): UrlPath<String>) -> impl IntoResponse {
    let f = f.lock().unwrap();
    match f.files.iter().find(|(n, _)| *n == name) {
        Some((_, b)) => (StatusCode::OK, b.clone()),
        None => (StatusCode::NOT_FOUND, Vec::new()),
    }
}

struct Server {
    addr: SocketAddr,
    files: Shared,
}

impl Server {
    async fn start() -> Self {
        let files: Shared = Arc::default();
        let app = Router::new()
            .route("/repos/{owner}/{repo}/releases/latest", get(latest))
            .route("/dl/{name}", get(file))
            .with_state(files.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        Self { addr, files }
    }

    fn url(&self, name: &str) -> String {
        format!("http://{}/dl/{name}", self.addr)
    }

    /// Publish release `version` with `binary`, its manifest signed by `signer`.
    /// `tamper` changes the served binary after the manifest has hashed it.
    fn publish(&self, version: &str, binary: &[u8], signer: &Key, tamper: bool) {
        let m = Manifest {
            product: manifest::PRODUCT.into(),
            version: version.into(),
            pub_date: None,
            notes_url: None,
            assets: vec![ManifestAsset {
                target: TARGET.into(),
                url: self.url("apitorrentd-bin"),
                sha256: hex::encode(sha2::Sha256::digest(binary)),
                size: binary.len() as u64,
            }],
        };
        let manifest = serde_json::to_vec(&m).unwrap();
        let sig = signer.sign(&manifest);
        let mut served = binary.to_vec();
        if tamper {
            served[0] ^= 0xff;
        }
        let release = serde_json::json!({
            "tag_name": format!("v{version}"),
            "html_url": format!("https://github.com/cinderblock/apitorrent/releases/tag/v{version}"),
            "body": "Notes",
            "published_at": "2026-11-09T20:54:00Z",
            "assets": [
                { "name": manifest::MANIFEST_NAME, "browser_download_url": self.url("m") },
                { "name": manifest::SIGNATURE_NAME, "browser_download_url": self.url("s") },
            ]
        });
        *self.files.lock().unwrap() = Files {
            release_json: release.to_string(),
            files: vec![
                ("m".into(), manifest),
                ("s".into(), sig.into_bytes()),
                ("apitorrentd-bin".into(), served),
            ],
        };
    }
}

fn updater(server: &Server, dir: &Path, key: Option<UpdateKey>) -> (Updater, PathBuf) {
    let bin = dir.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let exe = bin.join(if cfg!(windows) {
        "apitorrentd.exe"
    } else {
        "apitorrentd"
    });
    std::fs::write(&exe, b"the old binary").unwrap();
    let u = Updater::new(UpdaterConfig {
        data_dir: dir.to_path_buf(),
        current_version: semver::Version::parse("1.0.0").unwrap(),
        exe: exe.clone(),
        key,
        source: Source::loopback_for_tests(format!("http://{}/", server.addr).parse().unwrap()),
        target: TARGET.into(),
    })
    .unwrap();
    (u, exe)
}

/// Files in the executable's directory other than the executable.
fn leftovers(exe: &Path) -> Vec<String> {
    std::fs::read_dir(exe.parent().unwrap())
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|n| *n != exe.file_name().unwrap().to_string_lossy())
        .collect()
}

#[tokio::test]
async fn a_signed_newer_release_installs() {
    let server = Server::start().await;
    let key = Key::new();
    let dir = tempfile::tempdir().unwrap();
    let (u, exe) = updater(&server, dir.path(), Some(key.public()));

    // No release yet.
    let s = u.check().await.unwrap();
    assert!(!s.available && s.latest.is_none());

    server.publish("1.1.0", b"the new binary", &key, false);
    let s = u.check().await.unwrap();
    assert!(s.available);
    assert!(s.installs_enabled);
    assert_eq!(s.latest.as_ref().unwrap().version, "1.1.0");
    assert_eq!(
        s.latest.as_ref().unwrap().published_ms,
        Some(1_794_257_640_000)
    );

    let mut installed = u.installed();
    let done = u.install().await.unwrap();
    assert_eq!(done.to.to_string(), "1.1.0");
    assert_eq!(std::fs::read(&exe).unwrap(), b"the new binary");
    assert!(installed.borrow_and_update().is_some());
    assert_eq!(
        u.status().phase,
        apitorrent_types::update::UpdatePhase::Restarting
    );
    // Windows leaves the old binary aside for the next start; Unix leaves nothing.
    let left = leftovers(&exe);
    if cfg!(windows) {
        assert_eq!(left.len(), 1, "{left:?}");
        assert!(left[0].ends_with(".old"));
    } else {
        assert!(left.is_empty(), "{left:?}");
    }
    // The check time persisted.
    assert!(dir.path().join("update.json").exists());
}

#[tokio::test]
async fn a_tampered_binary_is_refused() {
    let server = Server::start().await;
    let key = Key::new();
    let dir = tempfile::tempdir().unwrap();
    let (u, exe) = updater(&server, dir.path(), Some(key.public()));
    server.publish("1.1.0", b"the new binary", &key, true);
    let e = u.install().await.unwrap_err();
    assert!(matches!(e, UpdateError::Mismatch(_)), "{e}");
    assert_eq!(std::fs::read(&exe).unwrap(), b"the old binary");
    assert!(leftovers(&exe).is_empty(), "the bad download was deleted");
    assert!(u.status().last_error.is_some());
}

#[tokio::test]
async fn a_signature_by_another_key_is_refused() {
    let server = Server::start().await;
    let key = Key::new();
    let attacker = Key::new();
    let dir = tempfile::tempdir().unwrap();
    let (u, exe) = updater(&server, dir.path(), Some(key.public()));
    server.publish("1.1.0", b"evil", &attacker, false);
    let e = u.install().await.unwrap_err();
    assert!(matches!(e, UpdateError::Signature(_)), "{e}");
    assert_eq!(std::fs::read(&exe).unwrap(), b"the old binary");
}

#[tokio::test]
async fn a_corrupted_signature_is_refused() {
    let server = Server::start().await;
    let key = Key::new();
    let dir = tempfile::tempdir().unwrap();
    let (u, exe) = updater(&server, dir.path(), Some(key.public()));
    server.publish("1.1.0", b"new", &key, false);
    // Replace the manifest after signing: the signature no longer matches.
    {
        let mut f = server.files.lock().unwrap();
        let m = &mut f.files.iter_mut().find(|(n, _)| n == "m").unwrap().1;
        let s = String::from_utf8(m.clone())
            .unwrap()
            .replace("1.1.0", "1.1.1");
        *m = s.into_bytes();
    }
    let e = u.install().await.unwrap_err();
    assert!(matches!(e, UpdateError::Signature(_)), "{e}");
    assert_eq!(std::fs::read(&exe).unwrap(), b"the old binary");
}

#[tokio::test]
async fn an_older_or_equal_release_is_refused() {
    let server = Server::start().await;
    let key = Key::new();
    let dir = tempfile::tempdir().unwrap();
    let (u, exe) = updater(&server, dir.path(), Some(key.public()));
    for v in ["0.9.0", "1.0.0"] {
        server.publish(v, b"old", &key, false);
        let s = u.check().await.unwrap();
        assert!(!s.available, "{v}");
        let e = u.install().await.unwrap_err();
        assert!(matches!(e, UpdateError::NotNewer { .. }), "{v}: {e}");
    }
    assert_eq!(std::fs::read(&exe).unwrap(), b"the old binary");
}

#[tokio::test]
async fn without_a_key_installs_are_off_but_checks_work() {
    let server = Server::start().await;
    let key = Key::new();
    let dir = tempfile::tempdir().unwrap();
    let (u, exe) = updater(&server, dir.path(), None);
    server.publish("1.1.0", b"new", &key, false);
    let s = u.check().await.unwrap();
    assert!(s.available);
    assert!(!s.installs_enabled);
    assert!(s.installs_disabled_reason.is_some());
    let e = u.install().await.unwrap_err();
    assert!(matches!(e, UpdateError::Disabled(_)), "{e}");
    assert_eq!(std::fs::read(&exe).unwrap(), b"the old binary");
}

#[tokio::test]
async fn settings_persist() {
    let server = Server::start().await;
    let dir = tempfile::tempdir().unwrap();
    let (u, _) = updater(&server, dir.path(), None);
    assert!(u.settings().check_daily);
    assert!(!u.settings().auto_install);
    let s = apitorrent_types::update::UpdateSettings {
        check_daily: false,
        auto_install: true,
    };
    u.set_settings(s).unwrap();
    let (again, _) = updater(&server, dir.path(), None);
    assert_eq!(again.settings(), s);
}

/// A running executable can be swapped out (on Windows, by moving it aside first). The
/// "running executable" is a copy of this test binary, started with an ignored test that
/// just sleeps.
#[test]
fn a_running_executable_is_swapped() {
    let dir = tempfile::tempdir().unwrap();
    let exe = dir
        .path()
        .join(if cfg!(windows) { "copy.exe" } else { "copy" });
    std::fs::copy(std::env::current_exe().unwrap(), &exe).unwrap();
    let mut child = std::process::Command::new(&exe)
        .args([
            "--ignored",
            "--exact",
            "sleep_for_the_swap_test",
            "--nocapture",
        ])
        .env("APITORRENT_SWAP_TEST_SLEEP", "1")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    std::thread::sleep(std::time::Duration::from_millis(500));
    assert!(child.try_wait().unwrap().is_none(), "the copy is running");

    if cfg!(windows) {
        // What the aside move works around: Windows won't overwrite a running exe.
        assert!(std::fs::write(&exe, b"x").is_err());
    }
    let new = dir.path().join("new.tmp");
    std::fs::write(&new, b"the new binary").unwrap();
    let r = apitorrent_update::install::swap_file(&new, &exe);
    child.kill().unwrap();
    child.wait().unwrap();
    r.unwrap();
    assert_eq!(std::fs::read(&exe).unwrap(), b"the new binary");
    apitorrent_update::install::remove_leftover(&exe);
    assert!(!apitorrent_update::install::aside_path(&exe).exists());
}

#[test]
#[ignore = "run by a_running_executable_is_swapped"]
fn sleep_for_the_swap_test() {
    if std::env::var_os("APITORRENT_SWAP_TEST_SLEEP").is_some() {
        std::thread::sleep(std::time::Duration::from_secs(30));
    }
}
