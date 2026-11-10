//! `/api/v1/system/update`, with a fake updater (the real one is tested in
//! `apitorrent-update`).

mod common;

use std::sync::{Arc, Mutex};

use axum::http::StatusCode;
use serde_json::json;

use apitorrent_auth::{ClientMeta, CreateTokenRequest, PasswordMode, TokenScope};
use apitorrent_server::{
    AuthMode, ServerConfig, UpdateFailure, UpdateFailureKind, UpdateFuture, UpdateService, router,
};
use apitorrent_types::update::{UpdatePhase, UpdateSettings, UpdateStatus};

use common::{AuthFixture, PASSWORD, Req, core, send, server_config};

struct Fake {
    status: Mutex<UpdateStatus>,
    fail_install: bool,
}

impl Fake {
    fn new(fail_install: bool) -> Arc<Self> {
        Arc::new(Self {
            status: Mutex::new(UpdateStatus {
                current_version: "1.0.0".into(),
                latest: None,
                available: false,
                installs_enabled: true,
                installs_disabled_reason: None,
                checked_ms: None,
                last_error: None,
                phase: UpdatePhase::Idle,
                settings: UpdateSettings::default(),
            }),
            fail_install,
        })
    }
}

impl UpdateService for Fake {
    fn status(&self) -> UpdateStatus {
        self.status.lock().unwrap().clone()
    }
    fn check(&self) -> UpdateFuture<Result<UpdateStatus, UpdateFailure>> {
        let mut s = self.status.lock().unwrap();
        s.checked_ms = Some(1);
        let s = s.clone();
        Box::pin(async move { Ok(s) })
    }
    fn install(&self) -> UpdateFuture<Result<UpdateStatus, UpdateFailure>> {
        let r = if self.fail_install {
            Err(UpdateFailure {
                code: "update_bad_signature".into(),
                message: "the update's signature is not valid".into(),
                kind: UpdateFailureKind::Failed,
            })
        } else {
            let mut s = self.status.lock().unwrap();
            s.phase = UpdatePhase::Restarting;
            Ok(s.clone())
        };
        Box::pin(async move { r })
    }
    fn set_settings(&self, settings: UpdateSettings) -> Result<UpdateStatus, UpdateFailure> {
        let mut s = self.status.lock().unwrap();
        s.settings = settings;
        Ok(s.clone())
    }
}

fn config(auth: AuthMode, fake: &Arc<Fake>) -> ServerConfig {
    ServerConfig {
        updates: Some(fake.clone() as Arc<dyn UpdateService>),
        ..server_config(auth)
    }
}

/// A full-scope and a read-only admin token.
fn tokens(f: &AuthFixture) -> (String, String) {
    let meta = ClientMeta::default();
    let s = f
        .auth
        .setup_with_password(&f.setup_token(), "admin", None, PASSWORD, &meta)
        .unwrap();
    let p = f.auth.authenticate_session(&s.secret).unwrap().unwrap();
    let make = |scope| {
        f.auth
            .create_token(
                &p,
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
    (make(TokenScope::Full), make(TokenScope::Read))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn admins_check_and_install() {
    let root = tempfile::tempdir().unwrap();
    let core = core(root.path()).await;
    let f = AuthFixture::new(root.path(), PasswordMode::Fallback, &[]);
    let fake = Fake::new(false);
    let app = router(
        core.clone(),
        &config(AuthMode::Enabled(f.auth.clone()), &fake),
    );
    let (full, read) = tokens(&f);

    let r = send(&app, Req::get("/api/v1/system/update")).await;
    assert_eq!(r.status, StatusCode::UNAUTHORIZED);

    let r = send(&app, Req::get("/api/v1/system/update").bearer(&read)).await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(r.typed::<UpdateStatus>().current_version, "1.0.0");

    // A read-only credential can't change anything.
    let r = send(
        &app,
        Req::post("/api/v1/system/update/install").bearer(&read),
    )
    .await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
    assert_eq!(r.code(), "read_only");

    let r = send(&app, Req::post("/api/v1/system/update/check").bearer(&full)).await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(r.typed::<UpdateStatus>().checked_ms, Some(1));

    let r = send(
        &app,
        Req::new("PUT", "/api/v1/system/update/settings")
            .bearer(&full)
            .json(&json!({ "checkDaily": false, "autoInstall": true })),
    )
    .await;
    assert_eq!(r.status, StatusCode::OK);
    assert!(r.typed::<UpdateStatus>().settings.auto_install);

    let r = send(
        &app,
        Req::post("/api/v1/system/update/install").bearer(&full),
    )
    .await;
    assert_eq!(r.status, StatusCode::ACCEPTED);
    assert_eq!(r.typed::<UpdateStatus>().phase, UpdatePhase::Restarting);

    // The info endpoint advertises it.
    let r = send(&app, Req::get("/api/v1/info")).await;
    let caps = r.json()["capabilities"].clone();
    assert!(caps.as_array().unwrap().contains(&json!("updates")));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_failed_install_is_reported() {
    let root = tempfile::tempdir().unwrap();
    let core = core(root.path()).await;
    let f = AuthFixture::new(root.path(), PasswordMode::Fallback, &[]);
    let fake = Fake::new(true);
    let app = router(
        core.clone(),
        &config(AuthMode::Enabled(f.auth.clone()), &fake),
    );
    let (full, _) = tokens(&f);
    let r = send(
        &app,
        Req::post("/api/v1/system/update/install").bearer(&full),
    )
    .await;
    assert_eq!(r.status, StatusCode::BAD_GATEWAY);
    assert_eq!(r.code(), "update_bad_signature");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn without_an_updater_the_endpoints_say_so() {
    let root = tempfile::tempdir().unwrap();
    let core = core(root.path()).await;
    let app = router(core.clone(), &server_config(AuthMode::DisabledOnLoopback));
    let r = send(&app, Req::get("/api/v1/system/update")).await;
    assert_eq!(r.status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(r.code(), "updates_unavailable");
    let r = send(&app, Req::get("/api/v1/info")).await;
    assert!(
        !r.json()["capabilities"]
            .as_array()
            .unwrap()
            .contains(&json!("updates"))
    );
}
