//! Reading a synthesized Deluge 2 config directory. `torrents.state` is a real pickle made
//! by Python (see `fixtures/make_state.py`); the rest is written here.

use apitorrent_bencode::OwnedValue;
use apitorrent_metainfo::{FileSource, TorrentBuilder, Version};

const A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

fn fake_deluge() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("state");
    std::fs::create_dir(&state).unwrap();
    std::fs::copy(
        concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/torrents.state"),
        state.join("torrents.state"),
    )
    .unwrap();
    let torrent = |name: &str| {
        TorrentBuilder::new(name, Version::V1)
            .piece_length(16 * 1024)
            .file(FileSource::from_bytes(&[], vec![1; 40_000]))
            .build()
            .unwrap()
    };
    std::fs::write(state.join(format!("{A}.torrent")), torrent("ubuntu.iso")).unwrap();
    std::fs::write(state.join(format!("{B}.torrent")), torrent("pack")).unwrap();
    // Not a torrent of Deluge's: ignored.
    std::fs::write(state.join("notes.torrent"), b"x").unwrap();

    // Deluge 2 stores each resume entry as a bencoded string.
    let resume_a = OwnedValue::dict()
        .insert("save_path", "/srv/torrents/iso")
        .insert(
            "file_priority",
            OwnedValue::List(vec![OwnedValue::from(4i64)]),
        )
        .insert("pieces", vec![1u8, 1, 0])
        .insert("added_time", 1_700_000_000i64)
        .insert("total_downloaded", 120_000i64)
        .insert("total_uploaded", 999i64)
        .insert(
            "mapped_files",
            OwnedValue::List(vec![OwnedValue::from("renamed\\ubuntu.iso")]),
        )
        .to_bytes();
    let resume = OwnedValue::dict()
        .insert(A, resume_a)
        .insert(B, OwnedValue::dict().insert("paused", 1i64).to_bytes())
        .to_bytes();
    std::fs::write(state.join("torrents.fastresume"), resume).unwrap();

    std::fs::write(
        dir.path().join("label.conf"),
        format!(
            r#"{{"file": 1, "format": 1}}{{"torrent_labels": {{"{A}": "linux", "{B}": ""}}, "labels": {{"linux": {{}}}}}}"#
        ),
    )
    .unwrap();
    std::fs::write(
        dir.path().join("core.conf"),
        r#"{
    "file": 1,
    "format": 1
}{
    "download_location": "/srv/torrents",
    "max_download_speed": 2048.0,
    "max_upload_speed": -1.0,
    "max_connections_global": 200,
    "max_active_downloading": 3,
    "max_active_seeding": 5,
    "max_active_limit": 8,
    "dont_count_slow_torrents": true,
    "stop_seed_at_ratio": true,
    "stop_seed_ratio": 1.5,
    "remove_seed_at_ratio": false
}"#,
    )
    .unwrap();
    dir
}

#[test]
fn reads_a_deluge_directory() {
    let dir = fake_deluge();
    let state = apitorrent_deluge::load(dir.path()).unwrap();
    assert!(state.warnings.is_empty(), "{:?}", state.warnings);
    assert_eq!(state.torrents.len(), 2);

    // Deluge's queue order: B (0) before A (1).
    let (b, a) = (&state.torrents[0], &state.torrents[1]);
    assert_eq!((b.id.as_str(), a.id.as_str()), (B, A));

    assert_eq!(a.save_path.as_deref(), Some("/srv/torrents/iso"));
    assert!(!a.paused);
    assert_eq!(a.label.as_deref(), Some("linux"));
    assert_eq!(a.queue, Some(1));
    assert_eq!(a.pieces.as_deref(), Some(&[1u8, 1, 0][..]));
    assert_eq!(a.added_at, Some(1_700_000_000));
    assert_eq!((a.total_downloaded, a.total_uploaded), (120_000, 999));
    assert_eq!(
        a.renamed.get(&0).map(String::as_str),
        Some("renamed/ubuntu.iso")
    );
    assert_eq!(a.max_download_rate, Some(500 * 1024));
    assert_eq!(a.max_upload_rate, None);
    assert_eq!(a.max_connections, Some(40));
    assert_eq!(
        a.trackers.as_deref(),
        Some(
            &[
                vec!["https://torrent.ubuntu.com/announce".to_owned()],
                vec!["https://ipv6.torrent.ubuntu.com/announce".to_owned()]
            ][..]
        )
    );

    assert!(b.paused);
    assert_eq!(b.label, None, "an empty label is no label");
    assert_eq!(b.file_priorities.as_deref(), Some(&[4u8, 0][..]));

    let s = &state.settings;
    assert_eq!(s.download_location.as_deref(), Some("/srv/torrents"));
    assert_eq!(s.max_download_rate, Some(2048 * 1024));
    assert_eq!(s.max_upload_rate, None);
    assert_eq!(s.max_connections, Some(200));
    assert_eq!(
        (
            s.max_active_downloading,
            s.max_active_seeding,
            s.max_active_limit
        ),
        (Some(3), Some(5), Some(8))
    );
    assert!(s.dont_count_slow);
    assert_eq!(s.stop_at_ratio, Some((1.5, false)));
}

/// A `.torrent` file Deluge doesn't list (no resume data, not in `torrents.state`) still
/// comes back, marked as a leftover, added when the file was written.
#[test]
fn leftover_torrent_files() {
    let dir = fake_deluge();
    let state_dir = dir.path().join("state");
    let c = "cccccccccccccccccccccccccccccccccccccccc";
    let path = state_dir.join(format!("{c}.torrent"));
    std::fs::copy(state_dir.join(format!("{A}.torrent")), &path).unwrap();
    let when = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_778_744_594);
    std::fs::File::options()
        .write(true)
        .open(&path)
        .unwrap()
        .set_modified(when)
        .unwrap();

    let state = apitorrent_deluge::load(dir.path()).unwrap();
    let get = |id: &str| state.torrents.iter().find(|t| t.id == id).unwrap();
    assert!(get(c).leftover);
    assert_eq!(get(c).added_at, Some(1_778_744_594));
    assert!(!get(A).leftover);
    assert!(
        !get(B).leftover,
        "resume data alone is enough to be Deluge's"
    );
    assert_eq!(
        get(A).added_at,
        Some(1_700_000_000),
        "resume data's time wins"
    );
}

#[test]
fn missing_pieces_are_warnings_not_failures() {
    let dir = fake_deluge();
    std::fs::remove_file(dir.path().join("state").join("torrents.state")).unwrap();
    std::fs::write(
        dir.path().join("state").join("torrents.fastresume"),
        b"garbage",
    )
    .unwrap();
    let state = apitorrent_deluge::load(dir.path()).unwrap();
    assert_eq!(state.torrents.len(), 2);
    assert_eq!(state.warnings.len(), 2, "{:?}", state.warnings);
}

#[test]
fn not_a_deluge_directory() {
    let dir = tempfile::tempdir().unwrap();
    assert!(apitorrent_deluge::load(dir.path()).is_err());
}

/// Deluge's `proxy` section, for each of libtorrent's proxy types.
#[test]
fn reads_the_proxy() {
    use apitorrent_deluge::{DelugeProxy, DelugeProxyKind};
    let dir = fake_deluge();
    let load = |proxy: &str| {
        std::fs::write(
            dir.path().join("core.conf"),
            format!(r#"{{"file": 1, "format": 1}}{{"proxy": {proxy}}}"#),
        )
        .unwrap();
        let state = apitorrent_deluge::load(dir.path()).unwrap();
        (state.settings.proxy, state.label_options)
    };
    let full = |ty: u8| {
        format!(
            r#"{{"type": {ty}, "hostname": " proxy.example ", "port": 1080, "username": "me",
                "password": "secret", "proxy_hostnames": false, "proxy_peer_connections": true,
                "proxy_tracker_connections": false, "force_proxy": true, "anonymous_mode": false}}"#
        )
    };
    let want = |kind, username: &str, password: &str| {
        Some(DelugeProxy {
            kind,
            hostname: "proxy.example".into(),
            port: 1080,
            username: username.into(),
            password: password.into(),
            proxy_hostnames: false,
            proxy_peer_connections: true,
            proxy_tracker_connections: false,
            force_proxy: true,
        })
    };

    // Deluge's default: no proxy, nothing to say.
    let (p, notes) = load(r#"{"type": 0, "hostname": "", "port": 8080}"#);
    assert_eq!((p, notes.len()), (None, 0));
    // The credentials come over only for the types that use them (SOCKS4: the user id).
    assert_eq!(load(&full(1)).0, want(DelugeProxyKind::Socks4, "me", ""));
    assert_eq!(load(&full(2)).0, want(DelugeProxyKind::Socks5, "", ""));
    assert_eq!(
        load(&full(3)).0,
        want(DelugeProxyKind::Socks5, "me", "secret")
    );
    assert_eq!(load(&full(4)).0, want(DelugeProxyKind::Http, "", ""));
    assert_eq!(
        load(&full(5)).0,
        want(DelugeProxyKind::Http, "me", "secret")
    );
    // I2P, and a proxy without a host, can't come over, and say so.
    let (p, notes) = load(&full(6));
    assert_eq!((p, notes), (None, vec!["the I2P proxy".to_owned()]));
    let (p, notes) = load(r#"{"type": 2, "hostname": "", "port": 1080, "anonymous_mode": true}"#);
    assert_eq!(p, None);
    assert_eq!(notes, ["anonymous mode", "the proxy (no host or port set)"]);
}
