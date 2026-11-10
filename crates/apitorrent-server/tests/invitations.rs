//! Invitations, recovery links and user management over HTTP: enrolling with a passkey or
//! a password, who may invite whom where, links that are expired, revoked or used, and what
//! disabling or deleting a user does to their credentials.

mod common;

use std::time::Duration;

use axum::http::StatusCode;
use serde_json::{Value, json};

use apitorrent_auth::PasswordMode;
use apitorrent_server::AuthMode;
use apitorrent_types::spaces::SpaceContext;
use apitorrent_types::users::{CreatedInvitation, ManagedUser};

use common::authenticator::SoftAuthenticator;
use common::{AuthFixture, PASSWORD, Req, Resp, app, core, send};

const ORIGIN: &str = "http://localhost:8742";
const DAY: Duration = Duration::from_secs(86_400);

async fn post(app: &axum::Router, path: &str, body: Value, cookie: Option<&str>) -> Resp {
    let mut r = Req::post(path).browser().json(&body);
    if let Some(c) = cookie {
        r = r.cookie(c);
    }
    send(app, r).await
}

async fn get(app: &axum::Router, path: &str, cookie: &str) -> Resp {
    send(app, Req::get(path).browser().cookie(cookie)).await
}

async fn call(
    app: &axum::Router,
    method: &'static str,
    path: &str,
    body: Value,
    cookie: &str,
) -> Resp {
    send(
        app,
        Req::new(method, path).browser().cookie(cookie).json(&body),
    )
    .await
}

/// The first admin, signed in with a password (fallback mode); returns the cookie.
fn bootstrap(f: &AuthFixture) -> String {
    let s = f
        .auth
        .setup_with_password(
            &f.setup_token(),
            "admin",
            None,
            PASSWORD,
            &apitorrent_auth::ClientMeta::default(),
        )
        .unwrap();
    format!("apitorrent-session={}", s.secret)
}

/// The secret in an invitation link's fragment.
fn secret_of(link: &str) -> String {
    let (_, token) = link.split_once("#token=").expect("a #token= fragment");
    token.to_owned()
}

async fn invite(app: &axum::Router, cookie: &str, body: Value) -> Resp {
    post(app, "/api/v1/invitations", body, Some(cookie)).await
}

/// Invite and enroll someone with a password; returns their cookie.
async fn enroll_with_password(app: &axum::Router, cookie: &str, name: &str, body: Value) -> String {
    let r = invite(app, cookie, body).await;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.json());
    let created: CreatedInvitation = r.typed();
    let r = post(
        app,
        "/api/v1/auth/enroll/password",
        json!({ "token": secret_of(&created.link), "username": name, "password": PASSWORD }),
        None,
    )
    .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.json());
    r.cookie()
}

async fn new_space(app: &axum::Router, cookie: &str, slug: &str) {
    let r = post(
        app,
        "/api/v1/spaces",
        json!({ "slug": slug, "name": slug.to_uppercase() }),
        Some(cookie),
    )
    .await;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.json());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_invitation_enrolls_a_passkey_once() {
    let root = tempfile::tempdir().unwrap();
    let core = core(root.path()).await;
    // Passkeys only: the admin is bootstrapped with one too.
    let f = AuthFixture::new(root.path(), PasswordMode::Off, &[]);
    let app = app(&core, AuthMode::Enabled(f.auth.clone()));
    let mut admin_key = SoftAuthenticator::default();
    let start = post(
        &app,
        "/api/v1/auth/setup/start",
        json!({ "token": f.setup_token(), "username": "admin" }),
        None,
    )
    .await
    .json();
    let cred = admin_key.register(&start["options"], ORIGIN).unwrap();
    let admin = post(
        &app,
        "/api/v1/auth/setup/finish",
        json!({ "ceremonyId": start["ceremonyId"], "credential": cred }),
        None,
    )
    .await
    .cookie();
    new_space(&app, &admin, "tv").await;

    let r = invite(
        &app,
        &admin,
        json!({ "spaces": [{ "slug": "tv", "role": "user" }], "note": "for Bob" }),
    )
    .await;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.json());
    assert_eq!(r.headers["cache-control"], "no-store");
    let created: CreatedInvitation = r.typed();
    assert!(
        created
            .link
            .starts_with("http://localhost:8742/enroll#token="),
        "{}",
        created.link
    );
    assert_eq!(created.invitation.spaces[0].slug, "tv");
    assert_eq!(created.invitation.note.as_deref(), Some("for Bob"));
    let token = secret_of(&created.link);

    // The page asks what the link grants, without signing in.
    let r = post(
        &app,
        "/api/v1/auth/enroll/info",
        json!({ "token": token }),
        None,
    )
    .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.json());
    let info = r.json();
    assert_eq!(info["kind"], "invitation");
    assert_eq!(info["spaces"][0]["name"], "TV");
    assert_eq!(info["passwordMode"], "off");
    // Passwords aren't allowed here.
    let r = post(
        &app,
        "/api/v1/auth/enroll/password",
        json!({ "token": token, "username": "bob", "password": PASSWORD }),
        None,
    )
    .await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);

    // A passkey it is.
    let mut bob_key = SoftAuthenticator::default();
    let start = post(
        &app,
        "/api/v1/auth/enroll/start",
        json!({ "token": token, "username": "Bob", "displayName": "Bob B" }),
        None,
    )
    .await;
    assert_eq!(start.status, StatusCode::OK, "{}", start.json());
    let start = start.json();
    assert_eq!(start["options"]["user"]["name"], "bob");
    assert_eq!(
        start["options"]["authenticatorSelection"]["residentKey"],
        "required"
    );
    let cred = bob_key.register(&start["options"], ORIGIN).unwrap();
    let r = post(
        &app,
        "/api/v1/auth/enroll/finish",
        json!({ "ceremonyId": start["ceremonyId"], "credential": cred, "passkeyName": "Phone" }),
        None,
    )
    .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.json());
    assert_eq!(r.json()["user"]["name"], "bob");
    assert_eq!(r.json()["user"]["role"], "user");
    let bob = r.cookie();
    // Bob lands in the space he was invited to, and nowhere else.
    let ctx: SpaceContext = get(&app, "/api/v1/space", &bob).await.typed();
    assert_eq!(ctx.current.slug, "tv");
    assert_eq!(ctx.spaces.len(), 1);
    assert!(!ctx.server_admin);
    assert_eq!(
        get(&app, "/s/default/api/v1/torrents", &bob).await.status,
        StatusCode::NOT_FOUND
    );

    // The link worked once.
    let r = post(
        &app,
        "/api/v1/auth/enroll/info",
        json!({ "token": token }),
        None,
    )
    .await;
    assert_eq!(r.status, StatusCode::UNAUTHORIZED);
    assert_eq!(r.code(), "invalid_link");
    let r = post(
        &app,
        "/api/v1/auth/enroll/start",
        json!({ "token": token, "username": "eve" }),
        None,
    )
    .await;
    assert_eq!(r.code(), "invalid_link");

    // Bob signs in again with that passkey, usernameless.
    let start = post(&app, "/api/v1/auth/login/passkey/start", json!({}), None)
        .await
        .json();
    let cred = bob_key.authenticate(&start["options"], ORIGIN).unwrap();
    let r = post(
        &app,
        "/api/v1/auth/login/passkey/finish",
        json!({ "ceremonyId": start["ceremonyId"], "credential": cred }),
        None,
    )
    .await;
    assert_eq!(r.json()["user"]["name"], "bob");

    // ----- Recovery: Bob lost his phone -----
    let users: Vec<ManagedUser> = get(&app, "/api/v1/users", &admin).await.typed();
    let bob_id = users.iter().find(|u| u.name == "bob").unwrap().id.clone();
    assert_eq!(users.iter().find(|u| u.name == "bob").unwrap().passkeys, 1);
    let r = post(
        &app,
        &format!("/api/v1/users/{bob_id}/recovery"),
        json!({}),
        Some(&admin),
    )
    .await;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.json());
    let recovery: CreatedInvitation = r.typed();
    let token = secret_of(&recovery.link);
    let info = post(
        &app,
        "/api/v1/auth/enroll/info",
        json!({ "token": token }),
        None,
    )
    .await
    .json();
    assert_eq!(info["kind"], "recovery");
    assert_eq!(info["username"], "bob");
    let mut new_key = SoftAuthenticator::default();
    let start = post(
        &app,
        "/api/v1/auth/enroll/start",
        json!({ "token": token, "username": "ignored" }),
        None,
    )
    .await
    .json();
    assert_eq!(start["options"]["user"]["name"], "bob");
    let cred = new_key.register(&start["options"], ORIGIN).unwrap();
    let r = post(
        &app,
        "/api/v1/auth/enroll/finish",
        json!({ "ceremonyId": start["ceremonyId"], "credential": cred }),
        None,
    )
    .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.json());
    assert_eq!(r.json()["user"]["name"], "bob");
    // The old session ended; the old passkey no longer signs in.
    assert_eq!(
        get(&app, "/api/v1/torrents", &bob).await.status,
        StatusCode::UNAUTHORIZED
    );
    let start = post(&app, "/api/v1/auth/login/passkey/start", json!({}), None)
        .await
        .json();
    let cred = bob_key.authenticate(&start["options"], ORIGIN).unwrap();
    let r = post(
        &app,
        "/api/v1/auth/login/passkey/finish",
        json!({ "ceremonyId": start["ceremonyId"], "credential": cred }),
        None,
    )
    .await;
    assert_eq!(r.status, StatusCode::UNAUTHORIZED);
    core.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn space_admins_invite_only_into_their_own_space() {
    let root = tempfile::tempdir().unwrap();
    let core = core(root.path()).await;
    let f = AuthFixture::new(root.path(), PasswordMode::Fallback, &[]);
    let app = app(&core, AuthMode::Enabled(f.auth.clone()));
    let admin = bootstrap(&f);
    new_space(&app, &admin, "tv").await;
    new_space(&app, &admin, "movies").await;
    let sam = enroll_with_password(
        &app,
        &admin,
        "sam",
        json!({ "spaces": [{ "slug": "tv", "role": "admin" }, { "slug": "movies", "role": "user" }] }),
    )
    .await;

    // Into tv, as any space role: yes, even from a page in movies, where Sam only uses.
    let r = post(
        &app,
        "/s/movies/api/v1/invitations",
        json!({ "spaces": [{ "slug": "tv", "role": "admin" }] }),
        Some(&sam),
    )
    .await;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.json());
    let mine: CreatedInvitation = r.typed();
    // Into a space they only use, one that doesn't exist, or with a server role: no.
    for body in [
        json!({ "spaces": [{ "slug": "movies", "role": "user" }] }),
        json!({ "spaces": [{ "slug": "default", "role": "user" }] }),
        json!({ "spaces": [{ "slug": "nope", "role": "user" }] }),
        json!({ "spaces": [{ "slug": "tv", "role": "user" }, { "slug": "movies", "role": "user" }] }),
        json!({ "role": "admin", "spaces": [{ "slug": "tv", "role": "user" }] }),
        json!({ "role": "viewer", "spaces": [{ "slug": "tv", "role": "user" }] }),
        json!({ "spaces": [] }),
    ] {
        let r = invite(&app, &sam, body.clone()).await;
        assert!(
            r.status == StatusCode::FORBIDDEN || r.status == StatusCode::BAD_REQUEST,
            "{body}: {} {}",
            r.status,
            r.json()
        );
    }
    // A server admin naming an unknown space gets a 404.
    let r = invite(
        &app,
        &admin,
        json!({ "spaces": [{ "slug": "nope", "role": "user" }] }),
    )
    .await;
    assert_eq!(r.status, StatusCode::NOT_FOUND);

    // An API token can't invite, even with full scope: it needs a browser session.
    let token = f
        .auth
        .create_token_in(
            &f.auth
                .authenticate_session(sam.split_once('=').unwrap().1)
                .unwrap()
                .unwrap(),
            core.space_by_slug("tv").unwrap().id,
            &apitorrent_auth::CreateTokenRequest {
                name: "script".into(),
                scope: apitorrent_auth::TokenScope::Full,
                expires_in_days: None,
            },
            &apitorrent_auth::ClientMeta::default(),
        )
        .unwrap()
        .token;
    let r = send(
        &app,
        Req::post("/api/v1/invitations")
            .bearer(&token)
            .json(&json!({ "spaces": [{ "slug": "tv", "role": "user" }] })),
    )
    .await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);

    // Sam sees tv's invitations and members, not movies' or the server's.
    let r = get(&app, "/api/v1/spaces/tv/invitations", &sam).await;
    assert_eq!(r.status, StatusCode::OK);
    assert!(
        r.json()
            .as_array()
            .unwrap()
            .iter()
            .all(|i| i["spaces"].as_array().unwrap().len() == 1)
    );
    assert_eq!(
        get(&app, "/api/v1/spaces/movies/invitations", &sam)
            .await
            .status,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        get(&app, "/api/v1/spaces/default/invitations", &sam)
            .await
            .status,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        get(&app, "/api/v1/invitations", &sam).await.status,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        get(&app, "/api/v1/users", &sam).await.status,
        StatusCode::FORBIDDEN
    );
    let r = get(&app, "/api/v1/spaces/tv/members", &sam).await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(r.json()[0]["name"], "sam");
    assert_eq!(
        get(&app, "/api/v1/spaces/movies/members", &sam)
            .await
            .status,
        StatusCode::FORBIDDEN
    );
    // Space settings stay with server admins.
    assert_eq!(
        get(&app, "/api/v1/spaces/tv", &sam).await.status,
        StatusCode::FORBIDDEN
    );

    // Sam revokes their own invitation; a used one can't be revoked.
    let r = call(
        &app,
        "DELETE",
        &format!("/api/v1/invitations/{}", mine.invitation.id),
        json!({}),
        &sam,
    )
    .await;
    assert_eq!(r.status, StatusCode::NO_CONTENT);
    let r = post(
        &app,
        "/api/v1/auth/enroll/info",
        json!({ "token": secret_of(&mine.link) }),
        None,
    )
    .await;
    assert_eq!(r.code(), "invalid_link");

    // Sam manages tv's members: changes a role, removes one; adding a user isn't theirs.
    let bob = enroll_with_password(
        &app,
        &sam,
        "bob",
        json!({ "spaces": [{ "slug": "tv", "role": "user" }] }),
    )
    .await;
    let bob_id = get(&app, "/api/v1/auth/status", &bob).await.json()["user"]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let r = call(
        &app,
        "PUT",
        &format!("/api/v1/spaces/tv/members/{bob_id}"),
        json!({ "role": "viewer" }),
        &sam,
    )
    .await;
    assert_eq!(r.status, StatusCode::NO_CONTENT);
    // Bob is a viewer now: tv is read-only to him.
    let r = post(&app, "/s/tv/api/v1/torrents", json!({}), Some(&bob)).await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
    let r = call(
        &app,
        "PUT",
        &format!("/api/v1/spaces/movies/members/{bob_id}"),
        json!({ "role": "user" }),
        &sam,
    )
    .await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
    let r = call(
        &app,
        "DELETE",
        &format!("/api/v1/spaces/tv/members/{bob_id}"),
        json!({}),
        &sam,
    )
    .await;
    assert_eq!(r.status, StatusCode::NO_CONTENT);
    assert_eq!(
        get(&app, "/api/v1/space", &bob).await.status,
        StatusCode::NOT_FOUND,
        "bob is in no space now"
    );
    core.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn expired_revoked_and_wrong_links_fail() {
    let root = tempfile::tempdir().unwrap();
    let core = core(root.path()).await;
    let f = AuthFixture::new(root.path(), PasswordMode::Fallback, &[]);
    let app = app(&core, AuthMode::Enabled(f.auth.clone()));
    let admin = bootstrap(&f);
    let body = json!({ "spaces": [{ "slug": "default", "role": "user" }], "expiresInDays": 1 });
    let short: CreatedInvitation = invite(&app, &admin, body.clone()).await.typed();
    let revoked: CreatedInvitation = invite(&app, &admin, body.clone()).await.typed();
    let r = call(
        &app,
        "DELETE",
        &format!("/api/v1/invitations/{}", revoked.invitation.id),
        json!({}),
        &admin,
    )
    .await;
    assert_eq!(r.status, StatusCode::NO_CONTENT);
    let r = call(
        &app,
        "DELETE",
        &format!("/api/v1/invitations/{}", revoked.invitation.id),
        json!({}),
        &admin,
    )
    .await;
    assert_eq!(r.status, StatusCode::CONFLICT);
    let try_link = |link: String| {
        let app = app.clone();
        async move {
            post(
                &app,
                "/api/v1/auth/enroll/password",
                json!({ "token": secret_of(&link), "username": "someone", "password": PASSWORD }),
                None,
            )
            .await
        }
    };
    let r = try_link(revoked.link.clone()).await;
    assert_eq!(r.status, StatusCode::UNAUTHORIZED);
    assert_eq!(r.code(), "invalid_link");
    f.advance(DAY + Duration::from_secs(1));
    let r = try_link(short.link.clone()).await;
    assert_eq!(r.code(), "invalid_link");
    // The list says why.
    let r = get(&app, "/api/v1/invitations", &admin).await;
    let states: Vec<String> = r
        .json()
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["state"].as_str().unwrap().to_owned())
        .collect();
    assert!(states.contains(&"expired".to_owned()), "{states:?}");
    assert!(states.contains(&"revoked".to_owned()), "{states:?}");
    // Lifetimes are 1 to 30 days. (The admin signs in again: a day has passed.)
    let admin = format!(
        "apitorrent-session={}",
        f.auth
            .password_login(
                "admin",
                PASSWORD,
                &apitorrent_auth::ClientMeta::default(),
                None
            )
            .unwrap()
            .secret
    );
    let r = invite(
        &app,
        &admin,
        json!({ "spaces": [{ "slug": "default", "role": "user" }], "expiresInDays": 31 }),
    )
    .await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);

    // Guessing is rate-limited per address, like setup and sign-in.
    let mut last = StatusCode::OK;
    for _ in 0..12 {
        let r = send(
            &app,
            Req::post("/api/v1/auth/enroll/info")
                .browser()
                .peer("192.0.2.9")
                .json(&json!({ "token": "a".repeat(43) })),
        )
        .await;
        last = r.status;
    }
    assert_eq!(last, StatusCode::TOO_MANY_REQUESTS);
    core.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn disabling_and_deleting_users() {
    let root = tempfile::tempdir().unwrap();
    let core = core(root.path()).await;
    let f = AuthFixture::new(root.path(), PasswordMode::Fallback, &[]);
    let app = app(&core, AuthMode::Enabled(f.auth.clone()));
    let admin = bootstrap(&f);
    let bob = enroll_with_password(
        &app,
        &admin,
        "bob",
        json!({ "spaces": [{ "slug": "default", "role": "user" }] }),
    )
    .await;
    // Bob makes an API token for an app.
    let r = post(
        &app,
        "/api/v1/auth/tokens",
        json!({ "name": "sonarr", "scope": "full" }),
        Some(&bob),
    )
    .await;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.json());
    let bob_token = r.json()["token"].as_str().unwrap().to_owned();
    let torrents = |t: &str| {
        let app = app.clone();
        let t = t.to_owned();
        async move {
            send(&app, Req::get("/api/v1/torrents").bearer(&t))
                .await
                .status
        }
    };
    assert_eq!(torrents(&bob_token).await, StatusCode::OK);

    let users: Vec<ManagedUser> = get(&app, "/api/v1/users", &admin).await.typed();
    assert_eq!(users.len(), 2);
    let me = users.iter().find(|u| u.you).unwrap();
    assert_eq!(me.name, "admin");
    let bob_row = users.iter().find(|u| u.name == "bob").unwrap();
    assert_eq!(bob_row.tokens, 1);
    assert!(bob_row.has_password);
    assert_eq!(bob_row.spaces[0].slug, "default");
    let bob_id = bob_row.id.clone();
    let admin_id = me.id.clone();

    // Disabled: his session and token stop at once.
    let r = call(
        &app,
        "PATCH",
        &format!("/api/v1/users/{bob_id}"),
        json!({ "disabled": true }),
        &admin,
    )
    .await;
    assert_eq!(r.status, StatusCode::NO_CONTENT, "{}", r.json());
    assert_eq!(torrents(&bob_token).await, StatusCode::UNAUTHORIZED);
    assert_eq!(
        get(&app, "/api/v1/torrents", &bob).await.status,
        StatusCode::UNAUTHORIZED
    );
    let r = post(
        &app,
        "/api/v1/auth/login/password",
        json!({ "username": "bob", "password": PASSWORD }),
        None,
    )
    .await;
    assert_eq!(r.status, StatusCode::UNAUTHORIZED);
    // Enabled again: the token works; he signs in afresh.
    let r = call(
        &app,
        "PATCH",
        &format!("/api/v1/users/{bob_id}"),
        json!({ "disabled": false }),
        &admin,
    )
    .await;
    assert_eq!(r.status, StatusCode::NO_CONTENT);
    assert_eq!(torrents(&bob_token).await, StatusCode::OK);

    // Promoted to server admin, he manages users; demoted, he doesn't.
    let r = call(
        &app,
        "PATCH",
        &format!("/api/v1/users/{bob_id}"),
        json!({ "role": "admin" }),
        &admin,
    )
    .await;
    assert_eq!(r.status, StatusCode::NO_CONTENT);
    let bob = post(
        &app,
        "/api/v1/auth/login/password",
        json!({ "username": "bob", "password": PASSWORD }),
        None,
    )
    .await
    .cookie();
    assert_eq!(
        get(&app, "/api/v1/users", &bob).await.status,
        StatusCode::OK
    );
    call(
        &app,
        "PATCH",
        &format!("/api/v1/users/{bob_id}"),
        json!({ "role": "user" }),
        &admin,
    )
    .await;
    assert_eq!(
        get(&app, "/api/v1/users", &bob).await.status,
        StatusCode::FORBIDDEN
    );

    // Never yourself.
    for (method, body) in [
        ("DELETE", json!({})),
        ("PATCH", json!({ "disabled": true })),
        ("PATCH", json!({ "role": "user" })),
    ] {
        let r = call(
            &app,
            method,
            &format!("/api/v1/users/{admin_id}"),
            body,
            &admin,
        )
        .await;
        assert_eq!(r.status, StatusCode::FORBIDDEN, "{method}");
    }
    // Managing users needs a recent sign-in.
    f.advance(Duration::from_secs(11 * 60));
    let r = call(
        &app,
        "DELETE",
        &format!("/api/v1/users/{bob_id}"),
        json!({}),
        &admin,
    )
    .await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
    assert_eq!(r.code(), "reauth_required");
    let r = post(
        &app,
        "/api/v1/auth/reauth/password",
        json!({ "password": PASSWORD }),
        Some(&admin),
    )
    .await;
    let admin = r.cookie();
    // Deleted: everything of his goes.
    let r = call(
        &app,
        "DELETE",
        &format!("/api/v1/users/{bob_id}"),
        json!({}),
        &admin,
    )
    .await;
    assert_eq!(r.status, StatusCode::NO_CONTENT);
    assert_eq!(torrents(&bob_token).await, StatusCode::UNAUTHORIZED);
    let users: Vec<ManagedUser> = get(&app, "/api/v1/users", &admin).await.typed();
    assert_eq!(users.len(), 1);
    core.shutdown().await.unwrap();
}
