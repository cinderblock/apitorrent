//! Store-level tests that don't need an authenticator. Passkey ceremonies are covered end to
//! end in `apitorrent-server`'s tests, which drive a software authenticator over HTTP.

use std::sync::Mutex;

use super::*;

const CHEAP: PasswordCost = PasswordCost {
    memory_kib: 256,
    iterations: 1,
    parallelism: 1,
};

struct Fixture {
    _dir: tempfile::TempDir,
    auth: Auth,
    clock: Arc<ManualClock>,
    tokens: Arc<Mutex<Vec<String>>>,
    token_file: PathBuf,
}

fn fixture(mode: PasswordMode) -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let clock = Arc::new(ManualClock::new(1_700_000_000_000));
    let tokens = Arc::new(Mutex::new(Vec::new()));
    let seen = tokens.clone();
    let token_file = dir.path().join("setup-token");
    let mut config = AuthConfig::new(dir.path().join("auth.db"));
    config.password_mode = mode;
    config.password_cost = CHEAP;
    config.clock = clock.clone();
    config.setup_token_file = Some(token_file.clone());
    config.on_setup_token = Some(Arc::new(move |t: &SetupToken| {
        seen.lock().unwrap().push(t.token.clone());
    }));
    let auth = Auth::open(config).unwrap();
    Fixture {
        _dir: dir,
        auth,
        clock,
        tokens,
        token_file,
    }
}

fn meta() -> ClientMeta {
    ClientMeta {
        ip: Some("192.0.2.7".parse().unwrap()),
        user_agent: Some("tests".into()),
    }
}

impl Fixture {
    fn last_token(&self) -> String {
        self.tokens.lock().unwrap().last().unwrap().clone()
    }

    /// Set up an admin with a password; returns their session.
    fn admin(&self) -> Principal {
        let s = self
            .auth
            .setup_with_password(
                &self.last_token(),
                "Admin",
                None,
                "correct horse battery",
                &meta(),
            )
            .unwrap();
        self.auth.authenticate_session(&s.secret).unwrap().unwrap()
    }
}

#[test]
fn setup_token_lifecycle() {
    let f = fixture(PasswordMode::Fallback);
    assert!(f.auth.setup_required().unwrap());
    let first = f.last_token();
    let on_disk = std::fs::read_to_string(&f.token_file).unwrap();
    assert_eq!(on_disk.trim(), first);

    // It expires, and a new one replaces it.
    f.clock.advance(Duration::from_secs(3601));
    f.auth.tick().unwrap();
    let second = f.last_token();
    assert_ne!(first, second);
    assert!(matches!(
        f.auth
            .setup_with_password(&first, "a", None, "correct horse battery", &meta()),
        Err(AuthError::InvalidSetupToken)
    ));

    // Ten wrong guesses replace it too, from any address.
    for i in 0..10u8 {
        let m = ClientMeta {
            ip: Some(IpAddr::from([198, 51, 100, i])),
            user_agent: None,
        };
        let r = f
            .auth
            .setup_with_password("nope", "a", None, "correct horse battery", &m);
        assert!(matches!(r, Err(AuthError::InvalidSetupToken)), "{r:?}");
    }
    let third = f.last_token();
    assert_ne!(second, third);

    // Formatting is forgiven: case, dashes and spaces.
    let sloppy = third.replace('-', " ").to_uppercase();
    let s = f
        .auth
        .setup_with_password(&sloppy, "Admin", None, "correct horse battery", &meta())
        .unwrap();
    assert!(!s.pending_second_factor);
    assert!(!f.auth.setup_required().unwrap());
    assert!(
        !f.token_file.exists(),
        "the token file is removed once used"
    );
    assert!(matches!(
        f.auth
            .setup_with_password(&third, "b", None, "correct horse battery", &meta()),
        Err(AuthError::AlreadySetUp)
    ));
    // No new tokens once set up.
    let issued = f.tokens.lock().unwrap().len();
    f.clock.advance(Duration::from_secs(7200));
    f.auth.tick().unwrap();
    assert_eq!(f.tokens.lock().unwrap().len(), issued);
}

#[test]
fn password_setup_needs_fallback_mode() {
    let f = fixture(PasswordMode::Off);
    assert!(matches!(
        f.auth.setup_with_password(
            &f.last_token(),
            "admin",
            None,
            "correct horse battery",
            &meta()
        ),
        Err(AuthError::Unavailable(_))
    ));
}

#[test]
fn validation() {
    let f = fixture(PasswordMode::Fallback);
    let t = f.last_token();
    for bad in ["", "-x", "has space", "ünïcode", &"a".repeat(65)] {
        assert!(matches!(
            f.auth
                .setup_with_password(&t, bad, None, "correct horse battery", &meta()),
            Err(AuthError::InvalidRequest(_))
        ));
    }
    assert!(matches!(
        f.auth
            .setup_with_password(&t, "admin", None, "short", &meta()),
        Err(AuthError::InvalidRequest(_))
    ));
}

#[test]
fn session_expiry_and_rotation() {
    let f = fixture(PasswordMode::Fallback);
    let admin = f.admin();
    assert_eq!(admin.user.name, "admin");
    assert_eq!(admin.user.role, Role::Admin);
    assert!(admin.may_write());

    // Each login is a new session; the one it replaces ends.
    let s2 = f
        .auth
        .password_login("admin", "correct horse battery", &meta(), Some(&admin))
        .unwrap();
    let Credential::Session(old) = &admin.credential else {
        panic!()
    };
    assert!(!f.auth.still_valid(&admin).unwrap(), "{old:?} was replaced");
    let p2 = f.auth.authenticate_session(&s2.secret).unwrap().unwrap();

    // Idle expiry: activity within the idle window keeps it alive...
    for _ in 0..4 {
        f.clock.advance(Duration::from_secs(6 * 24 * 3600));
        assert!(f.auth.authenticate_session(&s2.secret).unwrap().is_some());
    }
    // ...but not past the absolute lifetime (30 days).
    f.clock.advance(Duration::from_secs(6 * 24 * 3600));
    assert!(f.auth.authenticate_session(&s2.secret).unwrap().is_none());
    assert!(!f.auth.still_valid(&p2).unwrap());

    // Idle for longer than 7 days.
    let s3 = f
        .auth
        .password_login("admin", "correct horse battery", &meta(), None)
        .unwrap();
    f.clock.advance(Duration::from_secs(7 * 24 * 3600 + 1));
    assert!(f.auth.authenticate_session(&s3.secret).unwrap().is_none());

    // Logout ends it.
    let s4 = f
        .auth
        .password_login("admin", "correct horse battery", &meta(), None)
        .unwrap();
    let p4 = f.auth.authenticate_session(&s4.secret).unwrap().unwrap();
    f.auth.logout(&p4, &meta()).unwrap();
    assert!(f.auth.authenticate_session(&s4.secret).unwrap().is_none());

    // Garbage never matches.
    assert!(f.auth.authenticate_session("x").unwrap().is_none());
    assert!(
        f.auth
            .authenticate_session(&"A".repeat(43))
            .unwrap()
            .is_none()
    );
}

#[test]
fn session_listing_and_revocation() {
    let f = fixture(PasswordMode::Fallback);
    let admin = f.admin();
    let other = f
        .auth
        .password_login("admin", "correct horse battery", &meta(), None)
        .unwrap();
    let list = f.auth.list_sessions(&admin).unwrap();
    assert_eq!(list.len(), 2);
    assert_eq!(list.iter().filter(|s| s.current).count(), 1);
    assert_eq!(list[0].user_agent.as_deref(), Some("tests"));
    let other_id = list.iter().find(|s| !s.current).unwrap().id.clone();
    f.auth.revoke_session(&admin, &other_id, &meta()).unwrap();
    assert!(
        f.auth
            .authenticate_session(&other.secret)
            .unwrap()
            .is_none()
    );
    assert!(matches!(
        f.auth.revoke_session(&admin, &other_id, &meta()),
        Err(AuthError::NotFound)
    ));
}

#[test]
fn api_tokens() {
    let f = fixture(PasswordMode::Fallback);
    let admin = f.admin();
    let req = |scope, days| CreateTokenRequest {
        name: "script".into(),
        scope,
        expires_in_days: days,
    };
    let read = f
        .auth
        .create_token(&admin, &req(TokenScope::Read, None), &meta())
        .unwrap();
    assert!(read.token.starts_with(TOKEN_PREFIX));
    assert!(read.token.starts_with(&read.info.prefix));
    let full = f
        .auth
        .create_token(&admin, &req(TokenScope::Full, Some(1)), &meta())
        .unwrap();

    let pr = f.auth.authenticate_token(&read.token).unwrap().unwrap();
    assert!(pr.may_read());
    assert!(!pr.may_write());
    let pf = f.auth.authenticate_token(&full.token).unwrap().unwrap();
    assert!(pf.may_write());

    // Tokens can't manage credentials, even full ones.
    assert!(matches!(
        f.auth
            .create_token(&pf, &req(TokenScope::Full, None), &meta()),
        Err(AuthError::Forbidden(_))
    ));
    assert!(matches!(
        f.auth.list_passkeys(&pf),
        Err(AuthError::Forbidden(_))
    ));

    let listed = f.auth.list_tokens(&admin).unwrap();
    assert_eq!(listed.len(), 2);
    assert!(listed.iter().any(|t| t.last_used_at.is_some()));

    // Expiry.
    f.clock.advance(Duration::from_secs(86_401));
    assert!(f.auth.authenticate_token(&full.token).unwrap().is_none());
    assert!(!f.auth.still_valid(&pf).unwrap());

    // Revocation.
    f.auth.revoke_token(&admin, &read.info.id, &meta()).unwrap();
    assert!(f.auth.authenticate_token(&read.token).unwrap().is_none());
    assert!(!f.auth.still_valid(&pr).unwrap());

    assert!(f.auth.authenticate_token("apt_nope").unwrap().is_none());
    assert!(f.auth.authenticate_token("nope").unwrap().is_none());
}

#[test]
fn sensitive_changes_need_recent_auth() {
    let f = fixture(PasswordMode::Fallback);
    let admin = f.admin();
    f.clock.advance(Duration::from_secs(11 * 60));
    let req = CreateTokenRequest {
        name: "late".into(),
        scope: TokenScope::Read,
        expires_in_days: None,
    };
    let admin = f
        .auth
        .authenticate_session(
            &f.auth
                .password_login("admin", "correct horse battery", &meta(), Some(&admin))
                .unwrap()
                .secret,
        )
        .unwrap()
        .unwrap();
    f.auth.create_token(&admin, &req, &meta()).unwrap();
    f.clock.advance(Duration::from_secs(11 * 60));
    let admin = f
        .auth
        .authenticate_session(
            &f.auth
                .password_login("admin", "correct horse battery", &meta(), Some(&admin))
                .unwrap()
                .secret,
        )
        .unwrap()
        .unwrap();
    f.clock.advance(Duration::from_secs(11 * 60));
    let stale = admin;
    assert!(matches!(
        f.auth.create_token(&stale, &req, &meta()),
        Err(AuthError::ReauthRequired)
    ));
    assert!(matches!(
        f.auth
            .set_password(&stale, "another long password", &meta()),
        Err(AuthError::ReauthRequired)
    ));
    // Re-authenticating rotates the session and opens the window again.
    let fresh = f
        .auth
        .reauth_with_password(&stale, "correct horse battery", &meta())
        .unwrap();
    assert!(!f.auth.still_valid(&stale).unwrap());
    let fresh = f.auth.authenticate_session(&fresh.secret).unwrap().unwrap();
    f.auth.create_token(&fresh, &req, &meta()).unwrap();
    // The only sign-in method can't be removed.
    assert!(matches!(
        f.auth.remove_password(&fresh, &meta()),
        Err(AuthError::Conflict(_))
    ));
}

#[test]
fn password_lockout() {
    let f = fixture(PasswordMode::Fallback);
    f.admin();
    // Different addresses, so only the account lockout is in play.
    let from = |i: u8| ClientMeta {
        ip: Some(IpAddr::from([203, 0, 113, i])),
        user_agent: None,
    };
    for i in 0..5 {
        assert!(matches!(
            f.auth
                .password_login("admin", "wrong password!", &from(i), None),
            Err(AuthError::Unauthorized)
        ));
    }
    // Locked: even the right password is refused for now.
    assert!(matches!(
        f.auth
            .password_login("admin", "correct horse battery", &from(9), None),
        Err(AuthError::RateLimited(_))
    ));
    f.clock.advance(Duration::from_secs(61));
    f.auth
        .password_login("admin", "correct horse battery", &from(9), None)
        .unwrap();

    // Unknown users fail the same way.
    assert!(matches!(
        f.auth
            .password_login("nobody", "correct horse battery", &from(10), None),
        Err(AuthError::Unauthorized)
    ));
}

#[test]
fn address_lockout() {
    let f = fixture(PasswordMode::Fallback);
    f.admin();
    let mut limited = false;
    for i in 0..20 {
        let name = format!("user{i}");
        match f
            .auth
            .password_login(&name, "wrong password!", &meta(), None)
        {
            Err(AuthError::Unauthorized) => {}
            Err(AuthError::RateLimited(_)) => {
                limited = true;
                break;
            }
            other => panic!("{other:?}"),
        }
    }
    assert!(limited, "one address can't keep guessing");
}

#[test]
fn password_modes() {
    let f = fixture(PasswordMode::Off);
    assert!(matches!(
        f.auth
            .password_login("admin", "correct horse battery", &meta(), None),
        Err(AuthError::Unavailable(_))
    ));
    let f = fixture(PasswordMode::SecondFactor);
    assert!(matches!(
        f.auth
            .password_login("admin", "correct horse battery", &meta(), None),
        Err(AuthError::Unavailable(_))
    ));
}

#[test]
fn relying_parties() {
    let dir = tempfile::tempdir().unwrap();
    let open = |urls: &[&str]| {
        let mut c = AuthConfig::new(dir.path().join("auth.db"));
        c.public_urls = urls.iter().map(|u| Url::parse(u).unwrap()).collect();
        Auth::open(c)
    };
    let a = open(&["https://torrents.example.com"]).unwrap();
    assert!(a.passkeys_available("localhost:8742"));
    assert!(a.passkeys_available("Torrents.Example.com"));
    assert!(!a.passkeys_available("127.0.0.1:8742"));
    assert!(!a.passkeys_available("other.example.com"));
    assert!(open(&["http://torrents.example.com"]).is_err());
    assert!(open(&["https://192.0.2.1"]).is_err());
    assert_eq!(host_without_port("[::1]:8742"), "[::1]");
    assert_eq!(host_without_port("LocalHost:1"), "localhost");
}
