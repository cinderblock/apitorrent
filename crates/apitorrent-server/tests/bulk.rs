//! `PATCH /torrents` and `POST /torrents/settings`: changing many torrents at once.

mod common;

use axum::http::StatusCode;
use serde_json::json;

use apitorrent_auth::PasswordMode;
use apitorrent_server::AuthMode;
use apitorrent_types::UnwantedPolicy;
use apitorrent_types::bulk::{BulkReport, TorrentSettings};

use common::{AuthFixture, Req, app, core, send, torrent_json_named};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bulk_edit_changes_each_torrent_and_reports_failures() {
    let root = tempfile::tempdir().unwrap();
    let core = core(root.path()).await;
    let f = AuthFixture::new(root.path(), PasswordMode::Fallback, &[]);
    let token = f.admin_token();
    let app = app(&core, AuthMode::Enabled(f.auth.clone()));

    let mut ids = Vec::new();
    for name in ["a.txt", "b.txt"] {
        let resp = send(
            &app,
            Req::post("/api/v1/torrents")
                .bearer(&token)
                .raw_json(torrent_json_named(name)),
        )
        .await;
        assert_eq!(resp.status, StatusCode::CREATED);
        ids.push(resp.json()["id"].as_str().unwrap().to_owned());
    }
    // One torrent has its own download limit, which an upload-only edit must keep.
    let resp = send(
        &app,
        Req::new("PUT", format!("/api/v1/torrents/{}/limits", ids[0]))
            .bearer(&token)
            .json(&json!({ "downloadRate": 1000, "uploadRate": 0, "maxConnections": null })),
    )
    .await;
    assert_eq!(resp.status, StatusCode::NO_CONTENT);

    let resp = send(
        &app,
        Req::new("PATCH", "/api/v1/torrents")
            .bearer(&token)
            .json(&json!({
                "ids": [ids[0], ids[1], "nope"],
                "label": " linux ",
                "limits": { "uploadRate": 2048, "maxConnections": 40 },
                "seedLimits": { "limits": { "ratio": 2.0, "seedMinutes": 0, "action": "pause" } },
                "trackers": { "add": ["http://tracker.example/announce"] },
                "unwantedPolicy": "delete",
            })),
    )
    .await;
    assert_eq!(resp.status, StatusCode::OK);
    let report: BulkReport = resp.typed();
    assert_eq!(report.updated, ids);
    assert_eq!(report.failed.len(), 1);
    assert_eq!(report.failed[0].id, "nope");
    assert_eq!(report.failed[0].code, "not_found");

    let resp = send(
        &app,
        Req::post("/api/v1/torrents/settings")
            .bearer(&token)
            .json(&json!({ "ids": [ids[0], ids[1], "nope"] })),
    )
    .await;
    assert_eq!(resp.status, StatusCode::OK);
    let settings: Vec<TorrentSettings> = resp.typed();
    assert_eq!(settings.len(), 2);
    for s in &settings {
        assert_eq!(s.label.as_deref(), Some("linux"));
        assert_eq!(s.limits.upload_rate, 2048);
        assert_eq!(s.limits.max_connections, Some(40));
        assert_eq!(s.seed_limits.map(|l| l.ratio), Some(2.0));
        assert!(
            s.trackers
                .iter()
                .flatten()
                .any(|u| u == "http://tracker.example/announce")
        );
        assert_eq!(s.unwanted_policy, UnwantedPolicy::Delete);
    }
    assert_eq!(settings[0].limits.download_rate, 1000);
    assert_eq!(settings[1].limits.download_rate, 0);

    // Removing the label, and taking the tracker off again.
    let resp = send(
        &app,
        Req::new("PATCH", "/api/v1/torrents")
            .bearer(&token)
            .json(&json!({
                "ids": ids,
                "label": "",
                "trackers": { "remove": ["http://tracker.example/announce"] },
                "seedLimits": { "limits": null },
            })),
    )
    .await;
    let report: BulkReport = resp.typed();
    assert!(report.failed.is_empty(), "{report:?}");
    let resp = send(
        &app,
        Req::post("/api/v1/torrents/settings")
            .bearer(&token)
            .json(&json!({ "ids": ids })),
    )
    .await;
    let settings: Vec<TorrentSettings> = resp.typed();
    for s in &settings {
        assert_eq!(s.label, None);
        assert_eq!(s.seed_limits, None);
        assert!(
            s.trackers
                .iter()
                .flatten()
                .all(|u| u != "http://tracker.example/announce")
        );
    }
    core.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn recheck_verifies_again() {
    let root = tempfile::tempdir().unwrap();
    let core = core(root.path()).await;
    let f = AuthFixture::new(root.path(), PasswordMode::Fallback, &[]);
    let token = f.admin_token();
    let app = app(&core, AuthMode::Enabled(f.auth.clone()));
    let resp = send(
        &app,
        Req::post("/api/v1/torrents")
            .bearer(&token)
            .raw_json(torrent_json_named("c.txt")),
    )
    .await;
    let id = resp.json()["id"].as_str().unwrap().to_owned();
    let resp = send(
        &app,
        Req::post(format!("/api/v1/torrents/{id}/recheck")).bearer(&token),
    )
    .await;
    assert_eq!(resp.status, StatusCode::NO_CONTENT);
    let resp = send(
        &app,
        Req::post("/api/v1/torrents/nope/recheck").bearer(&token),
    )
    .await;
    assert_eq!(resp.status, StatusCode::NOT_FOUND);
    core.shutdown().await.unwrap();
}

/// Wait until the torrent `id` has settled in a state other than checking or moving.
async fn settled(app: &axum::Router, token: &str, id: &str) -> apitorrent_types::TorrentSummary {
    use apitorrent_types::TorrentState as S;
    for _ in 0..200 {
        let resp = send(
            app,
            Req::get(format!("/api/v1/torrents/{id}")).bearer(token),
        )
        .await;
        let d: apitorrent_types::TorrentDetail = resp.typed();
        if !matches!(d.summary.state, S::Checking | S::Moving) {
            return d.summary;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    panic!("{id} never settled");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn moving_files_keeps_what_was_downloaded() {
    let root = tempfile::tempdir().unwrap();
    let core = core(root.path()).await;
    let f = AuthFixture::new(root.path(), PasswordMode::Fallback, &[]);
    let token = f.admin_token();
    let app = app(&core, AuthMode::Enabled(f.auth.clone()));
    // The torrent's content is already in the download folder, so it checks complete.
    std::fs::create_dir_all(root.path().join("downloads")).unwrap();
    std::fs::write(root.path().join("downloads").join("m.txt"), b"hello world").unwrap();
    let resp = send(
        &app,
        Req::post("/api/v1/torrents")
            .bearer(&token)
            .raw_json(torrent_json_named("m.txt")),
    )
    .await;
    let id = resp.json()["id"].as_str().unwrap().to_owned();
    // Paused, it shows as paused even while it checks: wait for what the check finds.
    let mut checked = false;
    for _ in 0..200 {
        if settled(&app, &token, &id).await.progress >= 1.0 {
            checked = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert!(checked, "the file on disk was never found complete");

    let dest = root.path().join("elsewhere");
    let resp = send(
        &app,
        Req::new("PATCH", "/api/v1/torrents")
            .bearer(&token)
            .json(&json!({
                "ids": [id],
                "savePath": dest.to_string_lossy(),
            })),
    )
    .await;
    let report: BulkReport = resp.typed();
    assert!(report.failed.is_empty(), "{report:?}");
    let s = settled(&app, &token, &id).await;
    assert_eq!(s.error, None);
    assert_eq!(s.save_path, dest.to_string_lossy());
    assert!(s.progress >= 1.0, "nothing to download again");
    assert_eq!(std::fs::read(dest.join("m.txt")).unwrap(), b"hello world");
    assert!(!root.path().join("downloads").join("m.txt").exists());

    // Back again, but something is in the way: refused before anything moves.
    std::fs::write(root.path().join("downloads").join("m.txt"), b"other").unwrap();
    let resp = send(
        &app,
        Req::new("PATCH", "/api/v1/torrents")
            .bearer(&token)
            .json(&json!({
                "ids": [id],
                "savePath": root.path().join("downloads").to_string_lossy(),
            })),
    )
    .await;
    let report: BulkReport = resp.typed();
    assert_eq!(report.failed.len(), 1, "{report:?}");
    assert!(
        report.failed[0].message.contains("already exists"),
        "{report:?}"
    );
    assert_eq!(std::fs::read(dest.join("m.txt")).unwrap(), b"hello world");

    // A relative path is refused too.
    let resp = send(
        &app,
        Req::new("PATCH", "/api/v1/torrents")
            .bearer(&token)
            .json(&json!({
                "ids": [id],
                "savePath": "relative/folder",
            })),
    )
    .await;
    let report: BulkReport = resp.typed();
    assert_eq!(report.failed[0].code, "invalid_request");
    core.shutdown().await.unwrap();
}
