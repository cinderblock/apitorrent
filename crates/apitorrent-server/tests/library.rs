//! `/api/v1/library`: dedupe status and "deduplicate now", and library folders. Server
//! admins only.

mod common;

use axum::http::StatusCode;
use serde_json::json;

use apitorrent_auth::{ClientMeta, PasswordMode, Role};
use apitorrent_server::AuthMode;
use apitorrent_types::library::{DedupeMode, LibraryFolder, LibraryStatus};
use apitorrent_types::spaces::{DEFAULT_SPACE, SpaceRole};

use common::{AuthFixture, Req, app, core, send};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn admins_manage_library_folders_and_dedupe() {
    let root = tempfile::tempdir().unwrap();
    let core = core(root.path()).await;
    let f = AuthFixture::new(root.path(), PasswordMode::Fallback, &[]);
    let app = app(&core, AuthMode::Enabled(f.auth.clone()));
    let admin = f.admin_token();

    let resp = send(&app, Req::get("/api/v1/library")).await;
    assert_eq!(resp.status, StatusCode::UNAUTHORIZED);
    let st: LibraryStatus = send(&app, Req::get("/api/v1/library").bearer(&admin))
        .await
        .typed();
    assert_eq!(st.dedupe.mode, DedupeMode::Report);
    assert!(st.folders.is_empty());

    let media = root.path().join("media");
    std::fs::create_dir_all(&media).unwrap();
    std::fs::write(media.join("a.mkv"), b"abc").unwrap();
    let resp = send(
        &app,
        Req::new("POST", "/api/v1/library/folders")
            .bearer(&admin)
            .json(&json!({ "path": media.to_string_lossy() })),
    )
    .await;
    assert_eq!(resp.status, StatusCode::CREATED, "{}", resp.json());
    let folder: LibraryFolder = resp.typed();
    for bad in ["relative", "/definitely/not/here/at/all"] {
        let resp = send(
            &app,
            Req::new("POST", "/api/v1/library/folders")
                .bearer(&admin)
                .json(&json!({ "path": bad })),
        )
        .await;
        assert_eq!(resp.status, StatusCode::BAD_REQUEST, "{bad}");
    }
    let resp = send(
        &app,
        Req::new(
            "POST",
            format!("/api/v1/library/folders/{}/scan", folder.id),
        )
        .bearer(&admin),
    )
    .await;
    assert_eq!(resp.status, StatusCode::NO_CONTENT);
    let resp = send(
        &app,
        Req::new("POST", "/api/v1/library/folders/999/scan").bearer(&admin),
    )
    .await;
    assert_eq!(resp.status, StatusCode::NOT_FOUND);

    let resp = send(
        &app,
        Req::new("POST", "/api/v1/library/dedupe").bearer(&admin),
    )
    .await;
    assert_eq!(resp.status, StatusCode::ACCEPTED);

    // A space's admin isn't a server admin: the library is the instance's.
    let meta = ClientMeta::default();
    let s = f
        .auth
        .password_login("admin", common::PASSWORD, &meta, None)
        .unwrap();
    let boss = f.auth.authenticate_session(&s.secret).unwrap().unwrap();
    let link = f
        .auth
        .create_invitation(
            &boss,
            Role::User,
            &[(DEFAULT_SPACE, SpaceRole::Admin)],
            None,
            None,
            &meta,
        )
        .unwrap();
    let sam = f
        .auth
        .enroll_with_password(&link.secret, "sam", None, common::PASSWORD, &meta, None)
        .unwrap();
    let cookie = format!("apitorrent-session={}", sam.secret);
    let resp = send(&app, Req::get("/api/v1/library").browser().cookie(&cookie)).await;
    assert_eq!(resp.status, StatusCode::FORBIDDEN);
    let resp = send(
        &app,
        Req::new("POST", "/api/v1/library/dedupe")
            .browser()
            .cookie(&cookie),
    )
    .await;
    assert_eq!(resp.status, StatusCode::FORBIDDEN);
    let resp = send(
        &app,
        Req::new("DELETE", format!("/api/v1/library/folders/{}", folder.id))
            .browser()
            .cookie(&cookie),
    )
    .await;
    assert_eq!(resp.status, StatusCode::FORBIDDEN);

    let resp = send(
        &app,
        Req::new("DELETE", format!("/api/v1/library/folders/{}", folder.id)).bearer(&admin),
    )
    .await;
    assert_eq!(resp.status, StatusCode::NO_CONTENT);
    let resp = send(
        &app,
        Req::new("DELETE", format!("/api/v1/library/folders/{}", folder.id)).bearer(&admin),
    )
    .await;
    assert_eq!(resp.status, StatusCode::NOT_FOUND);
    assert!(media.join("a.mkv").exists(), "the folder's files stay");

    // With dedupe off, "deduplicate now" is refused.
    let mut settings = core.settings();
    settings.library.dedupe = DedupeMode::Off;
    core.set_settings(settings).await.unwrap();
    let resp = send(
        &app,
        Req::new("POST", "/api/v1/library/dedupe").bearer(&admin),
    )
    .await;
    assert_eq!(resp.status, StatusCode::BAD_REQUEST);
    core.shutdown().await.unwrap();
}
