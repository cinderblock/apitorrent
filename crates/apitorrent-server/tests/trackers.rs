//! Each space's entry of a torrent has its own tracker list, which the space's users may
//! edit: `PUT /torrents/{id}/trackers`.

mod common;

use axum::http::StatusCode;
use serde_json::json;

use apitorrent_auth::{ClientMeta, CreateTokenRequest, PasswordMode, Principal, Role, TokenScope};
use apitorrent_server::AuthMode;
use apitorrent_types::TorrentDetail;
use apitorrent_types::spaces::{DEFAULT_SPACE, SpaceRole};

use common::{AuthFixture, PASSWORD, Req, app, core, send, torrent_json};

const A: &str = "udp://tracker-a.invalid:6969/announce";
const B: &str = "https://tracker-b.invalid/announce";

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_spaces_users_edit_their_entrys_trackers() {
    let root = tempfile::tempdir().unwrap();
    let core = core(root.path()).await;
    let f = AuthFixture::new(root.path(), PasswordMode::Fallback, &[]);
    let meta = ClientMeta::default();
    let session = f
        .auth
        .setup_with_password(&f.setup_token(), "admin", None, PASSWORD, &meta)
        .unwrap();
    let admin = f
        .auth
        .authenticate_session(&session.secret)
        .unwrap()
        .unwrap();
    let token = |p: &Principal, space, scope| {
        f.auth
            .create_token_in(
                p,
                space,
                &CreateTokenRequest {
                    name: "t".into(),
                    scope,
                    expires_in_days: None,
                },
                &meta,
            )
            .unwrap()
            .token
    };
    let app = app(&core, AuthMode::Enabled(f.auth.clone()));
    let admin_default = token(&admin, DEFAULT_SPACE, TokenScope::Full);
    let r = send(
        &app,
        Req::post("/api/v1/spaces")
            .bearer(&admin_default)
            .json(&json!({ "slug": "tv", "name": "TV" })),
    )
    .await;
    let tv = r.json()["id"].as_i64().unwrap();

    // Bob uses tv; Vic only views it.
    let member = |name: &str, role| {
        let link = f
            .auth
            .create_invitation(&admin, Role::User, &[(tv, role)], None, None, &meta)
            .unwrap();
        let s = f
            .auth
            .enroll_with_password(&link.secret, name, None, PASSWORD, &meta, None)
            .unwrap();
        f.auth.authenticate_session(&s.secret).unwrap().unwrap()
    };
    let bob = member("bob", SpaceRole::User);
    let vic = member("vic", SpaceRole::Viewer);
    let bob_tv = token(&bob, tv, TokenScope::Full);
    let bob_read = token(&bob, tv, TokenScope::Read);
    let vic_tv = token(&vic, tv, TokenScope::Full);

    // The same torrent in both spaces, with no trackers of its own.
    for t in [&admin_default, &bob_tv] {
        let r = send(
            &app,
            Req::post("/api/v1/torrents")
                .bearer(t)
                .raw_json(torrent_json()),
        )
        .await;
        assert_eq!(r.status, StatusCode::CREATED, "{}", r.json());
    }
    let id = core.list(None)[0].id.clone();
    let path = format!("/api/v1/torrents/{id}/trackers");
    let put = |t: &str, body: serde_json::Value| {
        let app = app.clone();
        let (t, path) = (t.to_owned(), path.clone());
        async move { send(&app, Req::new("PUT", path).bearer(&t).json(&body)).await }
    };
    let detail = |t: &str| {
        let app = app.clone();
        let t = t.to_owned();
        let id = id.clone();
        async move {
            send(&app, Req::get(format!("/api/v1/torrents/{id}")).bearer(&t))
                .await
                .typed::<TorrentDetail>()
        }
    };

    // A viewer, or a read-only token, can't.
    let tiers = json!({ "tiers": [[A], [" ", B], []] });
    for t in [&vic_tv, &bob_read] {
        assert_eq!(put(t, tiers.clone()).await.status, StatusCode::FORBIDDEN);
    }
    // A user of the space can; tiers are cleaned.
    let r = put(&bob_tv, tiers.clone()).await;
    assert_eq!(r.status, StatusCode::NO_CONTENT, "{}", r.json());
    let d = detail(&bob_tv).await;
    assert_eq!(d.trackers, [vec![A.to_owned()], vec![B.to_owned()]]);
    let shown: Vec<_> = d
        .tracker_status
        .iter()
        .map(|t| (t.tier, t.url.as_str()))
        .collect();
    assert_eq!(shown, [(0, A), (1, B)]);
    assert!(d.magnet.contains("tracker-a.invalid"), "{}", d.magnet);
    // The other space's entry keeps its own list.
    assert!(detail(&admin_default).await.trackers.is_empty());
    // Bob can't reach default's entry at all.
    let r = send(
        &app,
        Req::new("PUT", format!("/s/default{path}"))
            .bearer(&bob_tv)
            .json(&tiers),
    )
    .await;
    assert_eq!(
        r.status,
        StatusCode::FORBIDDEN,
        "a tv token, in another space"
    );

    // Bad lists are refused and change nothing.
    for bad in [
        json!({ "tiers": [[A], [A]] }),
        json!({ "tiers": [["ftp://x/announce"]] }),
        json!({ "tiers": [["not a url"]] }),
    ] {
        let r = put(&bob_tv, bad.clone()).await;
        assert_eq!(r.status, StatusCode::BAD_REQUEST, "{bad}");
    }
    assert_eq!(detail(&bob_tv).await.trackers.len(), 2);
    let r = send(
        &app,
        Req::new("PUT", "/api/v1/torrents/0000/trackers")
            .bearer(&bob_tv)
            .json(&tiers),
    )
    .await;
    assert_eq!(r.status, StatusCode::NOT_FOUND);

    // Reordered, and kept across a restart.
    let r = put(&bob_tv, json!({ "tiers": [[B], [A]] })).await;
    assert_eq!(r.status, StatusCode::NO_CONTENT);
    core.shutdown().await.unwrap();
    let core = common::core(root.path()).await;
    let app = common::app(&core, AuthMode::Enabled(f.auth.clone()));
    let d: TorrentDetail = send(
        &app,
        Req::get(format!("/api/v1/torrents/{id}")).bearer(&bob_tv),
    )
    .await
    .typed();
    assert_eq!(d.trackers, [vec![B.to_owned()], vec![A.to_owned()]]);
    let d: TorrentDetail = send(
        &app,
        Req::get(format!("/api/v1/torrents/{id}")).bearer(&admin_default),
    )
    .await
    .typed();
    assert!(d.trackers.is_empty());
    // An empty list announces nowhere.
    let r = send(
        &app,
        Req::new("PUT", format!("/api/v1/torrents/{id}/trackers"))
            .bearer(&bob_tv)
            .json(&json!({ "tiers": [] })),
    )
    .await;
    assert_eq!(r.status, StatusCode::NO_CONTENT);
    let d: TorrentDetail = send(
        &app,
        Req::get(format!("/api/v1/torrents/{id}")).bearer(&bob_tv),
    )
    .await
    .typed();
    assert!(d.trackers.is_empty() && d.tracker_status.is_empty());
    core.shutdown().await.unwrap();
}
