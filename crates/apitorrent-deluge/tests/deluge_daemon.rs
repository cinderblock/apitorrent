//! Against a real, running Deluge 2 daemon. Ignored unless asked for:
//!
//! ```text
//! APITORRENT_DELUGE_TEST_CONFIG=<deluged's config dir> \
//!     cargo test -p apitorrent-deluge --test deluge_daemon -- --ignored --nocapture
//! ```
//!
//! The daemon must be running on this machine with that config dir (`deluged -c <dir>`); the
//! port, password and certificate are read from it, as apitorrent does. The test adds one
//! small torrent of its own (seeding a file in a temporary directory), pauses, resumes and
//! removes it; it doesn't touch any other torrent. Use a throwaway daemon, not one you care
//! about: see `plans/deluge-coexistence.md` for how it was run.

use std::path::{Path, PathBuf};
use std::time::Duration;

use apitorrent_bencode::OwnedValue;
use apitorrent_deluge::rpc::{
    ConnectOptions, Credentials, DelugeClient, PinnedCert, RpcError, TorrentState, Value,
};
use base64::Engine;
use sha1::{Digest, Sha1};

fn config_dir() -> Option<PathBuf> {
    std::env::var_os("APITORRENT_DELUGE_TEST_CONFIG").map(PathBuf::from)
}

/// A one-piece torrent of `data`, named uniquely so its info-hash is new each run.
fn make_torrent(name: &str, data: &[u8]) -> Vec<u8> {
    let info = OwnedValue::dict()
        .insert("name", OwnedValue::Bytes(name.as_bytes().to_vec()))
        .insert("piece length", OwnedValue::Int(16384))
        .insert(
            "length",
            OwnedValue::Int(i64::try_from(data.len()).unwrap()),
        )
        .insert("pieces", OwnedValue::Bytes(Sha1::digest(data).to_vec()));
    OwnedValue::dict()
        .insert(
            "announce",
            OwnedValue::Bytes(b"http://tracker.invalid/announce".to_vec()),
        )
        .insert("info", info)
        .to_bytes()
}

async fn wait_for_state(
    c: &mut DelugeClient,
    id: &str,
    done: impl Fn(&TorrentState) -> bool,
) -> TorrentState {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        let ts = c.torrents(Some(&[id])).await.unwrap();
        let state = ts[0].state.clone();
        if done(&state) || tokio::time::Instant::now() > deadline {
            return state;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

#[tokio::test]
#[ignore = "needs a running Deluge daemon: set APITORRENT_DELUGE_TEST_CONFIG"]
async fn real_daemon() {
    let Some(dir) = config_dir() else {
        eprintln!("APITORRENT_DELUGE_TEST_CONFIG is not set; skipping");
        return;
    };
    let dir = dir.as_path();

    // Connect the way apitorrent will.
    let mut c = DelugeClient::connect_config_dir(dir).await.unwrap();
    let level = c.auth_level().unwrap();
    println!("logged in as localclient, auth level {level}");
    assert_eq!(level, 10, "localclient is an admin account");

    let v = c.versions().await.unwrap();
    println!("Deluge {}, libtorrent {}", v.deluge, v.libtorrent);
    assert!(v.deluge.starts_with('2'), "{v:?}");
    assert!(!v.libtorrent.is_empty());

    let port = c.config_value("daemon_port").await.unwrap();
    assert_eq!(
        port.as_i64(),
        Some(i64::from(ConnectOptions::from_config_dir(dir).port))
    );
    let listen = c.config_value("listen_ports").await.unwrap();
    println!("listen_ports = {listen:?}");
    assert_eq!(listen.as_list().map(<[Value]>::len), Some(2));

    let before = c.torrents::<&str>(None).await.unwrap();
    println!("{} torrents in the daemon before the test", before.len());

    // Add our own torrent, seeding from a temporary directory.
    let data_dir = tempfile::tempdir().unwrap();
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let name = format!("apitorrent-deluge-test-{nonce}");
    let data: Vec<u8> = (0..16384u32).map(|i| (i % 251) as u8).collect();
    std::fs::write(data_dir.path().join(&name), &data).unwrap();
    let torrent = make_torrent(&name, &data);
    let save_path = data_dir.path().to_string_lossy().into_owned();
    let id = c
        .call(
            "core.add_torrent_file",
            vec![
                format!("{name}.torrent").into(),
                base64::engine::general_purpose::STANDARD
                    .encode(&torrent)
                    .into(),
                Value::dict([
                    ("download_location", save_path.clone().into()),
                    ("add_paused", false.into()),
                ]),
            ],
            vec![],
        )
        .await
        .unwrap();
    let id = id.as_str().expect("the new torrent's id").to_owned();
    println!("added {id}");

    let state = wait_for_state(&mut c, &id, |s| *s == TorrentState::Seeding).await;
    assert_eq!(state, TorrentState::Seeding);

    // The Label plugin's label, when the plugin is on.
    let plugins = c
        .call("core.get_enabled_plugins", vec![], vec![])
        .await
        .unwrap();
    let has_label = plugins
        .as_list()
        .unwrap()
        .iter()
        .any(|p| p.as_str() == Some("Label"));
    if has_label {
        match c
            .call("label.add", vec!["apitorrent-test".into()], vec![])
            .await
        {
            Ok(_) => {}
            Err(RpcError::Remote(r)) => println!("label.add: {r} (already there?)"),
            Err(e) => panic!("{e}"),
        }
        c.call(
            "label.set_torrent",
            vec![id.as_str().into(), "apitorrent-test".into()],
            vec![],
        )
        .await
        .unwrap();
    }

    // Listing by id, with an id Deluge doesn't have.
    let unknown = "0000000000000000000000000000000000000000";
    let listed = c.torrents(Some(&[id.as_str(), unknown])).await.unwrap();
    assert_eq!(listed.len(), 1, "the unknown id is left out");
    let t = &listed[0];
    println!("{t:#?}");
    assert_eq!(t.id, id);
    assert_eq!(t.name, name);
    assert!((t.progress - 100.0).abs() < 0.01);
    assert!(t.is_finished);
    assert!(!t.paused);
    assert_eq!(t.total_size, 16384);
    assert_eq!(Path::new(&t.save_path), data_dir.path());
    assert_eq!(t.trackers.len(), 1);
    assert_eq!(t.trackers[0].url, "http://tracker.invalid/announce");
    assert_eq!(t.tracker_host, "tracker.invalid");
    if has_label {
        assert_eq!(t.label.as_deref(), Some("apitorrent-test"));
    }

    // An empty pause touches nothing (Deluge itself would pause everything).
    assert!(c.pause::<&str>(&[]).await.unwrap().is_empty());
    let still = c.torrents(Some(&[id.as_str()])).await.unwrap();
    assert_eq!(still[0].state, TorrentState::Seeding);

    // Pause, and see it stop.
    assert_eq!(
        c.pause(&[id.as_str()]).await.unwrap(),
        std::slice::from_ref(&id)
    );
    let running = c
        .wait_until_stopped(&[id.as_str()], Duration::from_secs(10))
        .await
        .unwrap();
    assert!(running.is_empty(), "still running: {running:?}");
    let t = &c.torrents(Some(&[id.as_str()])).await.unwrap()[0];
    assert_eq!(t.state, TorrentState::Paused);
    assert!(t.paused);

    // Resume, and see it seed again.
    c.resume(&[id.as_str()]).await.unwrap();
    let state = wait_for_state(&mut c, &id, |s| *s == TorrentState::Seeding).await;
    assert_eq!(state, TorrentState::Seeding);

    // Deluge itself fails a pause at the first unknown id, with a bare KeyError...
    let e = c
        .call(
            "core.pause_torrent",
            vec![Value::str_list([unknown])],
            vec![],
        )
        .await
        .unwrap_err();
    println!("core.pause_torrent([unknown]): {e}");
    assert_eq!(e.remote().unwrap().exception, "KeyError");
    // ...so pause() skips the ids Deluge doesn't have.
    assert!(c.pause(&[unknown]).await.unwrap().is_empty());
    let still = c.torrents(Some(&[id.as_str()])).await.unwrap();
    assert_eq!(still[0].state, TorrentState::Seeding, "nothing was paused");
    assert_eq!(
        c.pause(&[unknown, id.as_str()]).await.unwrap(),
        std::slice::from_ref(&id)
    );
    let running = c
        .wait_until_stopped(&[id.as_str()], Duration::from_secs(10))
        .await
        .unwrap();
    assert!(running.is_empty(), "still running: {running:?}");

    // Remove it, keeping the data.
    c.remove_keep_data(&id).await.unwrap();
    assert!(c.torrents(Some(&[id.as_str()])).await.unwrap().is_empty());
    assert!(data_dir.path().join(&name).exists(), "the data is kept");
    let e = c.remove_keep_data(&id).await.unwrap_err();
    println!("removing it again: {e}");
    assert_eq!(e.remote().unwrap().exception, "InvalidTorrentError");

    let after = c.torrents::<&str>(None).await.unwrap();
    assert_eq!(after.len(), before.len());

    // A wrong password.
    let opts = ConnectOptions::from_config_dir(dir);
    let pin = PinnedCert::from_config_dir(dir).unwrap();
    let mut bad = DelugeClient::connect(&opts, &pin).await.unwrap();
    let e = bad
        .login(&Credentials::new("localclient", "not the password"))
        .await
        .unwrap_err();
    println!("wrong password: {e}");
    assert_eq!(e.remote().unwrap().exception, "BadLoginError");
    assert!(!e.to_string().contains("not the password"));

    // Another certificate pinned: refused before anything is sent.
    let other = PinnedCert::from_pem_file(
        &Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/ssl/b-daemon.cert"),
    )
    .unwrap();
    let e = DelugeClient::connect(&opts, &other).await.unwrap_err();
    println!("another certificate: {e}");
    assert!(matches!(e, RpcError::Tls { .. }));
}
