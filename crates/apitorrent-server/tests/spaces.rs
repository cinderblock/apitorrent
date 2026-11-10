//! Spaces over HTTP: which space a request lands in (token, `/s/<slug>/` prefix, last
//! space), what stays inside one, and the admin endpoints.

mod common;

use axum::http::StatusCode;

use apitorrent_auth::{ClientMeta, CreateTokenRequest, PasswordMode, TokenScope};
use apitorrent_server::AuthMode;
use apitorrent_types::TorrentSummary;
use apitorrent_types::spaces::{DEFAULT_SPACE, SpaceContext};

use common::{AuthFixture, PASSWORD, Req, app, core, send, torrent_json};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn requests_land_in_their_space() {
    let root = tempfile::tempdir().unwrap();
    let core = core(root.path()).await;
    let f = AuthFixture::new(root.path(), PasswordMode::Fallback, &[]);
    let meta = ClientMeta::default();
    let session = f
        .auth
        .setup_with_password(&f.setup_token(), "admin", None, PASSWORD, &meta)
        .unwrap();
    let cookie = format!("apitorrent-session={}", session.secret);
    let admin = f
        .auth
        .authenticate_session(&session.secret)
        .unwrap()
        .unwrap();
    let token_in = |space| {
        f.auth
            .create_token_in(
                &admin,
                space,
                &CreateTokenRequest {
                    name: format!("space {space}"),
                    scope: TokenScope::Full,
                    expires_in_days: None,
                },
                &meta,
            )
            .unwrap()
            .token
    };
    let default_token = token_in(DEFAULT_SPACE);
    let app = app(&core, AuthMode::Enabled(f.auth.clone()));

    // One space: everything as before, and the context says so.
    let r = send(&app, Req::get("/api/v1/space").bearer(&default_token)).await;
    assert_eq!(r.status, StatusCode::OK);
    let ctx: SpaceContext = r.typed();
    assert_eq!(ctx.current.slug, "default");
    assert_eq!(ctx.spaces.len(), 1);

    // A server admin makes a second space.
    let r = send(
        &app,
        Req::post("/api/v1/spaces")
            .bearer(&default_token)
            .json(&serde_json::json!({ "slug": "tv", "name": "TV" })),
    )
    .await;
    assert_eq!(r.status, StatusCode::CREATED, "{:?}", r.json());
    let tv = r.json()["id"].as_i64().unwrap();
    let tv_token = token_in(tv);

    // A token lands in its own space; its torrents stay there.
    let r = send(
        &app,
        Req::post("/api/v1/torrents")
            .bearer(&tv_token)
            .raw_json(torrent_json()),
    )
    .await;
    assert_eq!(r.status, StatusCode::CREATED);
    let id = r.json()["id"].as_str().unwrap().to_owned();
    let list = |r: common::Resp| r.typed::<Vec<TorrentSummary>>();
    let mine = list(send(&app, Req::get("/api/v1/torrents").bearer(&tv_token)).await);
    assert_eq!(mine.len(), 1);
    assert_eq!(mine[0].space, tv);
    let theirs = list(send(&app, Req::get("/api/v1/torrents").bearer(&default_token)).await);
    assert!(
        theirs.is_empty(),
        "the default space doesn't see tv's torrents"
    );
    // The prefix agrees with the token, or the request is refused.
    let r = send(&app, Req::get("/s/tv/api/v1/torrents").bearer(&tv_token)).await;
    assert_eq!(r.status, StatusCode::OK);
    let r = send(
        &app,
        Req::get("/s/default/api/v1/torrents").bearer(&tv_token),
    )
    .await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
    assert_eq!(r.code(), "wrong_space");
    // A torrent of another space is not found here.
    let r = send(
        &app,
        Req::get(format!("/api/v1/torrents/{id}")).bearer(&default_token),
    )
    .await;
    assert_eq!(r.status, StatusCode::NOT_FOUND);

    // The same torrent in the default space: not a duplicate, one swarm.
    let r = send(
        &app,
        Req::post("/api/v1/torrents")
            .bearer(&default_token)
            .raw_json(torrent_json()),
    )
    .await;
    assert_eq!(r.status, StatusCode::CREATED);
    assert_eq!(core.swarm_spaces(&id), [tv, DEFAULT_SPACE]);
    let all = list(
        send(
            &app,
            Req::get("/api/v1/torrents?space=all").bearer(&default_token),
        )
        .await,
    );
    assert_eq!(all.len(), 2);

    // A browser session moves between spaces by prefix; an unknown one is a 404.
    let r = send(&app, Req::get("/s/tv/api/v1/space").cookie(&cookie)).await;
    let ctx: SpaceContext = r.typed();
    assert_eq!(ctx.current.slug, "tv");
    assert!(ctx.server_admin);
    assert_eq!(ctx.spaces.len(), 2);
    let r = send(&app, Req::get("/s/nope/api/v1/torrents").cookie(&cookie)).await;
    assert_eq!(r.status, StatusCode::NOT_FOUND);
    assert_eq!(r.code(), "no_such_space");
    // Without a prefix it lands in the space it used last.
    let r = send(&app, Req::get("/api/v1/space").cookie(&cookie)).await;
    assert_eq!(r.typed::<SpaceContext>().current.slug, "tv");

    // A space with torrents can't be deleted; nor can `default`.
    let r = send(
        &app,
        Req::new("DELETE", "/api/v1/spaces/tv").bearer(&default_token),
    )
    .await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
    let r = send(
        &app,
        Req::new("DELETE", "/api/v1/spaces/default").bearer(&default_token),
    )
    .await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);

    // Members: the admin can list users and give one a role.
    let r = send(&app, Req::get("/api/v1/users").bearer(&default_token)).await;
    assert_eq!(r.status, StatusCode::OK);
    let user = r.json()[0]["id"].as_str().unwrap().to_owned();
    let r = send(
        &app,
        Req::new("PUT", format!("/api/v1/spaces/tv/members/{user}"))
            .bearer(&default_token)
            .json(&serde_json::json!({ "role": "viewer" })),
    )
    .await;
    assert_eq!(r.status, StatusCode::NO_CONTENT);
    let r = send(
        &app,
        Req::get("/api/v1/spaces/tv/members").bearer(&default_token),
    )
    .await;
    assert_eq!(r.json()[0]["role"], "viewer");
    core.shutdown().await.unwrap();
}
