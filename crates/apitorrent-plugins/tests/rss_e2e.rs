//! End to end: the first-party `rss` plugin, built with Bun from `plugins/rss`, running in
//! the V8 host against a feed served on 127.0.0.1.
//!
//! Needs `bun` on the PATH (the repository's JavaScript toolchain). The test installs the
//! workspace's packages first if they're missing (`bun install --frozen-lockfile`).

mod common;

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};

use serde_json::json;

use apitorrent_plugin_api::{PluginBundle, PluginRole, PluginService, PluginStatus};
use apitorrent_types::AddSource;

use common::{FakeApp, call, host, serve, wait_status};

fn repo() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .unwrap()
}

fn bun(args: &[&str], dir: &Path) {
    let out = Command::new("bun")
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap_or_else(|e| {
            panic!(
                "this test needs bun on the PATH (https://bun.sh), the repository's JavaScript \
                 toolchain: {e}"
            )
        });
    assert!(
        out.status.success(),
        "bun {args:?} failed:\n{}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

/// Build `plugins/<name>` into `out` and load the bundle.
fn build_plugin(name: &str, out: &Path) -> PluginBundle {
    // Tests run in parallel; two `bun install`s at once trip over each other's links.
    static INSTALL: Mutex<()> = Mutex::new(());
    let root = repo();
    {
        let _one_at_a_time = INSTALL
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !root.join("node_modules/@apitorrent/plugin-sdk").exists() {
            bun(&["install", "--frozen-lockfile"], &root);
        }
    }
    let build = root.join("packages/plugin-sdk/src/build.ts");
    let plugin = root.join("plugins").join(name);
    bun(
        &[
            build.to_str().unwrap(),
            plugin.to_str().unwrap(),
            "--out",
            out.to_str().unwrap(),
        ],
        &root,
    );
    let file = std::fs::read_dir(out)
        .unwrap()
        .map(|e| e.unwrap().path())
        .find(|p| p.to_string_lossy().ends_with(".plugin.json"))
        .expect("a bundle");
    serde_json::from_slice(&std::fs::read(file).unwrap()).unwrap()
}

fn item(n: u32, title: &str) -> String {
    format!(
        "<item><title>{title}</title><guid>ep{n}</guid>\
         <link>magnet:?xt=urn:btih:{}</link></item>",
        format!("{n}").repeat(40)
    )
}

#[tokio::test(flavor = "multi_thread")]
async fn rss_plugin_polls_a_local_feed() {
    let dir = tempfile::tempdir().unwrap();
    let bundle = build_plugin("rss", &dir.path().join("dist"));
    assert_eq!(bundle.manifest.id, "rss");

    // The feed: what it lists can change between polls.
    let feed = Arc::new(Mutex::new(String::new()));
    let other = serve(|_| (200, vec![], b"d4:infod4:name1:xee".to_vec())).await;
    let served = feed.clone();
    let port = serve(move |path| match path {
        "/feed.xml" => (
            200,
            vec![("content-type".into(), "application/rss+xml".into())],
            format!(
                "<?xml version=\"1.0\"?><rss version=\"2.0\"><channel><title>t</title>{}</channel></rss>",
                served.lock().unwrap()
            )
            .into_bytes(),
        ),
        "/files/ep5.torrent" => (200, vec![], b"d4:infod4:name3:ep5ee".to_vec()),
        _ => (404, vec![], vec![]),
    })
    .await;
    *feed.lock().unwrap() = [item(1, "Show S01E01 1080p"), item(2, "Show S01E02 720p")].concat();

    let app = FakeApp::new();
    let h = host(dir.path(), app.clone());
    let r = h.install(bundle).await.unwrap();
    assert!(r.needs_approval);
    let wants = &r.plugin.pending.as_ref().unwrap().new_capabilities;
    assert!(
        wants.iter().any(|w| w.contains("feeds[].url")),
        "approval shows the settings-based grant: {wants:?}"
    );
    let version = r.plugin.pending.as_ref().unwrap().manifest.version.clone();
    h.approve("rss".into(), version).await.unwrap();
    let feed_url = format!("http://127.0.0.1:{port}/feed.xml");
    h.set_settings(
        "rss".into(),
        1,
        json!({
            "feeds": [{ "url": feed_url, "include": "1080p", "label": "tv" }],
            "backfill": true,
        }),
    )
    .await
    .unwrap();
    h.set_enabled("rss".into(), true).await.unwrap();
    wait_status(&h, "rss", PluginStatus::Running).await;

    let poll = async || {
        let r = call(&h, "rss", "POST", "/poll", b"")
            .await
            .expect("poll answers");
        assert_eq!(r.status, 200, "{}", String::from_utf8_lossy(&r.body));
        serde_json::from_slice::<serde_json::Value>(&r.body).unwrap()
    };
    let status = poll().await;
    assert_eq!(
        status["feeds"][0]["lastError"],
        serde_json::Value::Null,
        "{status:#}"
    );
    // Backfill on: the existing item that passes the filter is added, the other isn't.
    assert_eq!(
        app.magnets(),
        [format!("magnet:?xt=urn:btih:{}", "1".repeat(40))]
    );
    assert_eq!(app.added.lock().unwrap()[0].label.as_deref(), Some("tv"));

    // A new item, a .torrent on the feed's host, and one on a host the admin didn't name.
    *feed.lock().unwrap() = [
        format!(
            "<item><title>Show S01E05 1080p</title><guid>ep5</guid><enclosure \
             url=\"http://127.0.0.1:{port}/files/ep5.torrent\" type=\"application/x-bittorrent\"/></item>"
        ),
        format!(
            "<item><title>Show S01E06 1080p</title><guid>ep6</guid><enclosure \
             url=\"http://127.0.0.1:{other}/ep6.torrent\" type=\"application/x-bittorrent\"/></item>"
        ),
        item(3, "Show S01E03 1080p"),
        item(1, "Show S01E01 1080p"),
        item(2, "Show S01E02 720p"),
    ]
    .concat();
    poll().await;
    let added = app.added.lock().unwrap().clone();
    assert_eq!(added.len(), 3, "{added:#?}");
    assert!(matches!(&added[1].source, AddSource::Magnet { uri } if uri.contains(&"3".repeat(40))));
    assert!(matches!(&added[2].source, AddSource::TorrentFile { .. }));
    let logs = h.logs("rss", 1, None).unwrap();
    assert!(
        logs.iter()
            .any(|l| l.message.contains("ep6") || l.message.contains("Show S01E06")),
        "the refused download is logged: {logs:#?}"
    );

    // Naming the other host in the settings lets the retry through; nothing is added twice.
    h.set_settings(
        "rss".into(),
        1,
        json!({
            "feeds": [{ "url": feed_url, "include": "1080p", "label": "tv" }],
            "backfill": true,
            "downloadHosts": [format!("http://127.0.0.1:{other}")],
        }),
    )
    .await
    .unwrap();
    poll().await;
    poll().await;
    assert_eq!(app.added.lock().unwrap().len(), 4);

    // Items that pass the filters were offered to the rules as candidates, with absolute
    // .torrent links; the 720p one wasn't.
    let offered: Vec<String> = app
        .candidates
        .lock()
        .unwrap()
        .iter()
        .filter(|(source, _)| source == "rss")
        .map(|(_, c)| c.id.clone())
        .collect();
    for id in ["ep1", "ep3", "ep5", "ep6"] {
        assert!(offered.iter().any(|o| o == id), "{id} offered: {offered:?}");
    }
    assert!(!offered.iter().any(|o| o == "ep2"), "{offered:?}");
    let ep5 = app
        .candidates
        .lock()
        .unwrap()
        .iter()
        .find(|(_, c)| c.id == "ep5")
        .map(|(_, c)| c.links.torrent.clone())
        .unwrap();
    assert_eq!(
        ep5,
        Some(format!("http://127.0.0.1:{port}/files/ep5.torrent"))
    );

    // The source role lists what the feed offered.
    let candidates = h
        .call_role("rss", PluginRole::Source, "candidates", json!([10]))
        .await
        .unwrap();
    let ids: Vec<&str> = candidates
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|c| c["id"].as_str())
        .collect();
    assert!(ids.contains(&"ep5") && !ids.contains(&"ep2"), "{ids:?}");

    // Seen items survive a restart of the host.
    drop(h);
    let h = host(dir.path(), app.clone());
    wait_status(&h, "rss", PluginStatus::Running).await;
    let r = call(&h, "rss", "POST", "/poll", b"").await.unwrap();
    assert_eq!(r.status, 200);
    assert_eq!(app.added.lock().unwrap().len(), 4);
}

/// Every first-party plugin's manifest and settings schema pass the host's checks. (Not
/// enabled: `ubuntu-releases` would reach the real Ubuntu servers.)
#[tokio::test(flavor = "multi_thread")]
async fn first_party_plugins_install() {
    let dir = tempfile::tempdir().unwrap();
    let h = host(dir.path(), FakeApp::new());
    for name in ["rss", "ubuntu-releases"] {
        let bundle = build_plugin(name, &dir.path().join(name));
        let (id, version) = (bundle.manifest.id.clone(), bundle.manifest.version.clone());
        assert_eq!(id, name);
        h.install(bundle).await.unwrap();
        h.approve(id.clone(), version).await.unwrap();
        let settings = h.settings(&id, 1).unwrap();
        assert!(settings.schema.is_some());
        // The defaults are valid settings.
        h.set_settings(id.clone(), 1, settings.values)
            .await
            .unwrap();
    }
}
