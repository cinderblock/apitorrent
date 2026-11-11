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

#[test]
fn privileged_actions_outside_the_crate() {
    let f = fixture(PasswordMode::Fallback);
    let admin = f.admin();
    f.auth.require_recent(&admin).unwrap();
    f.auth.require_recent(&Principal::local()).unwrap();
    // A token never passes, however fresh.
    let created = f
        .auth
        .create_token(
            &admin,
            &CreateTokenRequest {
                name: "script".into(),
                scope: TokenScope::Full,
                expires_in_days: None,
            },
            &meta(),
        )
        .unwrap();
    let token = f.auth.authenticate_token(&created.token).unwrap().unwrap();
    assert!(matches!(
        f.auth.require_recent(&token),
        Err(AuthError::Forbidden(_))
    ));
    f.clock.advance(Duration::from_secs(11 * 60));
    assert!(matches!(
        f.auth.require_recent(&admin),
        Err(AuthError::ReauthRequired)
    ));
    f.auth
        .audit("plugin_approved", &admin, &meta(), "rss 1.0.0");
    let kinds: Vec<String> = f
        .auth
        .db()
        .prepare("SELECT kind FROM auth_events ORDER BY id")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<std::result::Result<_, _>>()
        .unwrap();
    assert_eq!(kinds.last().map(String::as_str), Some("plugin_approved"));
}

#[test]
fn space_roles_and_bound_tokens() {
    use apitorrent_types::spaces::SpaceRole;
    let f = fixture(PasswordMode::Fallback);
    let admin = f.admin();
    // The first admin is a member of `default` from setup only through the server-admin
    // role; a second, ordinary user (there is no invitation API yet) starts in no space.
    let bob = store::UserRow {
        id: "bob-id".into(),
        name: "bob".into(),
        display_name: "Bob".into(),
        role: Role::User,
        password_hash: None,
        disabled: false,
    };
    store::insert_user(&f.auth.db(), &bob, 1).unwrap();
    let bob_p = Principal {
        user: bob.info(),
        credential: Credential::Local,
    };
    assert!(f.auth.memberships("bob-id").unwrap().is_empty());
    // Server admins are admins of every space, members or not.
    assert_eq!(
        f.auth.space_role(&admin, 7).unwrap(),
        Some(SpaceRole::Admin)
    );

    // Only a server admin manages members.
    let bob_session = Principal {
        credential: Credential::Token {
            id: "x".into(),
            scope: TokenScope::Full,
            space: 2,
        },
        ..bob_p.clone()
    };
    assert!(matches!(
        f.auth
            .set_member(&bob_session, 2, "bob-id", SpaceRole::Admin, &meta()),
        Err(AuthError::Forbidden(_))
    ));
    f.auth
        .set_member(&admin, 2, "bob-id", SpaceRole::User, &meta())
        .unwrap();
    assert_eq!(
        f.auth.memberships("bob-id").unwrap(),
        [(2, SpaceRole::User)]
    );
    assert_eq!(f.auth.members(2).unwrap()[0].name, "bob");

    // A token works in its own space only, and a read token is a viewer there.
    assert_eq!(
        f.auth.space_role(&bob_session, 2).unwrap(),
        Some(SpaceRole::User)
    );
    assert_eq!(f.auth.space_role(&bob_session, 1).unwrap(), None);
    let reader = Principal {
        credential: Credential::Token {
            id: "y".into(),
            scope: TokenScope::Read,
            space: 2,
        },
        ..bob_p
    };
    assert_eq!(
        f.auth.space_role(&reader, 2).unwrap(),
        Some(SpaceRole::Viewer)
    );

    // Tokens are made in a space and say so.
    let made = f
        .auth
        .create_token_in(
            &admin,
            2,
            &CreateTokenRequest {
                name: "sonarr".into(),
                scope: TokenScope::Full,
                expires_in_days: None,
            },
            &meta(),
        )
        .unwrap();
    assert_eq!(made.info.space, 2);
    let p = f.auth.authenticate_token(&made.token).unwrap().unwrap();
    assert_eq!(p.token_space(), Some(2));

    // Leaving a space ends the user's tokens there; forgetting a space ends everyone's.
    f.auth.remove_member(&admin, 2, "bob-id", &meta()).unwrap();
    assert!(f.auth.memberships("bob-id").unwrap().is_empty());
    f.auth.set_last_space(&admin.user.id, 2).unwrap();
    assert_eq!(f.auth.last_space(&admin.user.id).unwrap(), Some(2));
    f.auth.forget_space(2).unwrap();
    assert!(f.auth.authenticate_token(&made.token).unwrap().is_none());
    assert_eq!(f.auth.last_space(&admin.user.id).unwrap(), None);
}

// ----- Invitations, recovery links and user management -----

mod enrollment {
    use apitorrent_types::spaces::SpaceRole;
    use apitorrent_types::users::{EnrollmentKind, EnrollmentState};

    use super::*;

    const PW: &str = "correct horse battery";

    /// Enroll with a link and a password; returns the new user's session.
    fn enroll(f: &Fixture, secret: &str, name: &str) -> Result<Principal> {
        let s = f
            .auth
            .enroll_with_password(secret, name, None, PW, &meta(), None)?;
        Ok(f.auth.authenticate_session(&s.secret)?.unwrap())
    }

    fn invite(f: &Fixture, actor: &Principal, spaces: &[(i64, SpaceRole)]) -> IssuedLink {
        f.auth
            .create_invitation(actor, Role::User, spaces, None, Some("for tests"), &meta())
            .unwrap()
    }

    fn token(f: &Fixture, p: &Principal, space: i64) -> String {
        f.auth
            .create_token_in(
                p,
                space,
                &CreateTokenRequest {
                    name: "app".into(),
                    scope: TokenScope::Full,
                    expires_in_days: None,
                },
                &meta(),
            )
            .unwrap()
            .token
    }

    #[test]
    fn an_invitation_creates_a_user_once() {
        let f = fixture(PasswordMode::Fallback);
        let admin = f.admin();
        let link = invite(&f, &admin, &[(2, SpaceRole::User), (3, SpaceRole::Viewer)]);
        assert_eq!(link.secret.len(), 43);
        assert_eq!(link.record.kind, EnrollmentKind::Invitation);
        assert_eq!(link.record.state, EnrollmentState::Pending);
        assert_eq!(
            link.record.expires_at - link.record.created_at,
            7 * 86_400_000,
            "seven days by default"
        );
        // Only its hash is stored.
        let stored: Vec<u8> = f
            .auth
            .db()
            .query_row("SELECT secret_hash FROM enrollments", [], |r| r.get(0))
            .unwrap();
        assert_eq!(stored, secret::sha256(link.secret.as_bytes()));

        let info = f.auth.enroll_info(&link.secret, &meta()).unwrap();
        assert_eq!(info.spaces.len(), 2);
        assert_eq!(info.created_by.as_deref(), Some("admin"));

        // The username must be free.
        assert!(matches!(
            enroll(&f, &link.secret, "admin"),
            Err(AuthError::Conflict(_))
        ));
        let bob = enroll(&f, &link.secret, "Bob").unwrap();
        assert_eq!(bob.user.name, "bob");
        assert_eq!(bob.user.role, Role::User);
        assert_eq!(
            f.auth.memberships(&bob.user.id).unwrap(),
            [(2, SpaceRole::User), (3, SpaceRole::Viewer)]
        );
        assert_eq!(f.auth.last_space(&bob.user.id).unwrap(), Some(2));

        // It works once.
        assert!(matches!(
            enroll(&f, &link.secret, "carol"),
            Err(AuthError::InvalidLink)
        ));
        assert!(matches!(
            f.auth.enroll_info(&link.secret, &meta()),
            Err(AuthError::InvalidLink)
        ));
        let all = f.auth.enrollments(&admin, None).unwrap();
        assert_eq!(all[0].state, EnrollmentState::Used);
        assert_eq!(all[0].user_name.as_deref(), Some("bob"));
    }

    #[test]
    fn expired_and_revoked_links_fail() {
        let f = fixture(PasswordMode::Fallback);
        let admin = f.admin();
        let short = f
            .auth
            .create_invitation(
                &admin,
                Role::User,
                &[(1, SpaceRole::User)],
                Some(1),
                None,
                &meta(),
            )
            .unwrap();
        let revoked = invite(&f, &admin, &[(1, SpaceRole::User)]);
        f.auth
            .revoke_enrollment(&admin, &revoked.record.id, &meta())
            .unwrap();
        assert!(matches!(
            f.auth
                .revoke_enrollment(&admin, &revoked.record.id, &meta()),
            Err(AuthError::Conflict(_))
        ));
        assert!(matches!(
            enroll(&f, &revoked.secret, "a"),
            Err(AuthError::InvalidLink)
        ));
        // Lifetimes are bounded.
        for days in [0, 31] {
            assert!(matches!(
                f.auth
                    .create_invitation(&admin, Role::User, &[], Some(days), None, &meta()),
                Err(AuthError::InvalidRequest(_))
            ));
        }
        f.clock.advance(Duration::from_secs(86_400 + 1));
        assert!(matches!(
            enroll(&f, &short.secret, "b"),
            Err(AuthError::InvalidLink)
        ));
        let states: Vec<_> = f
            .auth
            .enrollments(&admin, None)
            .unwrap()
            .into_iter()
            .map(|r| r.state)
            .collect();
        assert!(states.contains(&EnrollmentState::Expired));
        assert!(states.contains(&EnrollmentState::Revoked));
    }

    #[test]
    fn wrong_links_count_toward_lockout() {
        let f = fixture(PasswordMode::Fallback);
        f.admin();
        for _ in 0..10 {
            assert!(matches!(
                f.auth.enroll_info(&"x".repeat(43), &meta()),
                Err(AuthError::InvalidLink)
            ));
        }
        assert!(matches!(
            f.auth.enroll_info(&"x".repeat(43), &meta()),
            Err(AuthError::RateLimited(_))
        ));
        let failures: i64 = f
            .auth
            .db()
            .query_row(
                "SELECT COUNT(*) FROM auth_events WHERE kind = 'enroll_failed'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(failures, 10);
    }

    #[test]
    fn space_admins_invite_only_into_their_spaces() {
        let f = fixture(PasswordMode::Fallback);
        let admin = f.admin();
        let link = invite(&f, &admin, &[(2, SpaceRole::Admin), (3, SpaceRole::Viewer)]);
        let sam = enroll(&f, &link.secret, "sam").unwrap();
        assert!(!sam.server_admin());

        // Into a space they administer, as `user`: yes.
        let ok = f
            .auth
            .create_invitation(
                &sam,
                Role::User,
                &[(2, SpaceRole::Admin)],
                None,
                None,
                &meta(),
            )
            .unwrap();
        // Another space, one they only view, or a server role: no.
        for (role, spaces) in [
            (Role::User, vec![(4, SpaceRole::User)]),
            (Role::User, vec![(3, SpaceRole::User)]),
            (Role::User, vec![(2, SpaceRole::User), (4, SpaceRole::User)]),
            (Role::Admin, vec![(2, SpaceRole::User)]),
            (Role::Viewer, vec![(2, SpaceRole::User)]),
            (Role::User, vec![]),
        ] {
            let r = f
                .auth
                .create_invitation(&sam, role, &spaces, None, None, &meta());
            assert!(
                matches!(
                    r,
                    Err(AuthError::Forbidden(_) | AuthError::InvalidRequest(_))
                ),
                "{role:?} {spaces:?}: {r:?}"
            );
        }
        // A token can't invite, even a server admin's.
        let admin_token = token(&f, &admin, 1);
        let p = f.auth.authenticate_token(&admin_token).unwrap().unwrap();
        assert!(matches!(
            f.auth
                .create_invitation(&p, Role::User, &[], None, None, &meta()),
            Err(AuthError::Forbidden(_))
        ));
        // Space 2's list shows the invitations into 2, and only that space.
        let mine = f.auth.enrollments(&sam, Some(2)).unwrap();
        assert_eq!(mine.len(), 2);
        assert!(
            mine.iter()
                .all(|r| r.spaces.len() == 1 && r.spaces[0].0 == 2)
        );
        assert!(matches!(
            f.auth.enrollments(&sam, Some(3)),
            Err(AuthError::Forbidden(_))
        ));
        assert!(matches!(
            f.auth.enrollments(&sam, None),
            Err(AuthError::Forbidden(_))
        ));
        // They may revoke their own kind, not one that also grants another space.
        let wide = invite(&f, &admin, &[(2, SpaceRole::User), (4, SpaceRole::User)]);
        assert!(matches!(
            f.auth.revoke_enrollment(&sam, &wide.record.id, &meta()),
            Err(AuthError::Forbidden(_))
        ));
        let elsewhere = invite(&f, &admin, &[(4, SpaceRole::User)]);
        assert!(matches!(
            f.auth
                .revoke_enrollment(&sam, &elsewhere.record.id, &meta()),
            Err(AuthError::NotFound)
        ));
        f.auth
            .revoke_enrollment(&sam, &ok.record.id, &meta())
            .unwrap();

        // Members: a space admin changes and removes existing members of their space only.
        let bob_link = invite(&f, &admin, &[(2, SpaceRole::User)]);
        let bob = enroll(&f, &bob_link.secret, "bob").unwrap();
        f.auth
            .set_member(&sam, 2, &bob.user.id, SpaceRole::Viewer, &meta())
            .unwrap();
        assert!(matches!(
            f.auth
                .set_member(&sam, 4, &bob.user.id, SpaceRole::User, &meta()),
            Err(AuthError::Forbidden(_))
        ));
        // Adding someone who isn't a member is a server admin's.
        assert!(matches!(
            f.auth
                .set_member(&sam, 2, &admin.user.id, SpaceRole::User, &meta()),
            Err(AuthError::NotFound)
        ));
        f.auth
            .remove_member(&sam, 2, &bob.user.id, &meta())
            .unwrap();
        // Deleting a space forgets its invitations.
        let pending = invite(&f, &admin, &[(5, SpaceRole::User)]);
        f.auth.forget_space(5).unwrap();
        let rec = f.auth.enroll_info(&pending.secret, &meta()).unwrap();
        assert!(rec.spaces.is_empty());
    }

    #[test]
    fn disabling_ends_sessions_and_suspends_tokens() {
        let f = fixture(PasswordMode::Fallback);
        let admin = f.admin();
        let link = invite(&f, &admin, &[(1, SpaceRole::User)]);
        let bob = enroll(&f, &link.secret, "bob").unwrap();
        let bob_token = token(&f, &bob, 1);
        let bob_token_p = f.auth.authenticate_token(&bob_token).unwrap().unwrap();
        let mut revoked = f.auth.revocations();
        revoked.mark_unchanged();

        f.auth
            .set_user_disabled(&admin, &bob.user.id, true, &meta())
            .unwrap();
        assert!(revoked.has_changed().unwrap(), "open sockets re-check");
        assert!(!f.auth.still_valid(&bob).unwrap());
        assert!(!f.auth.still_valid(&bob_token_p).unwrap());
        assert!(f.auth.authenticate_token(&bob_token).unwrap().is_none());
        assert!(matches!(
            f.auth.password_login("bob", PW, &meta(), None),
            Err(AuthError::Unauthorized)
        ));
        let users = f.auth.managed_users(&admin).unwrap();
        let b = users.iter().find(|u| u.info.name == "bob").unwrap();
        assert!(b.disabled);
        assert_eq!(b.tokens, 1);

        // Enabled again: the token works, the old session stays gone.
        f.auth
            .set_user_disabled(&admin, &bob.user.id, false, &meta())
            .unwrap();
        assert!(f.auth.authenticate_token(&bob_token).unwrap().is_some());
        assert!(!f.auth.still_valid(&bob).unwrap());
        f.auth.password_login("bob", PW, &meta(), None).unwrap();
    }

    #[test]
    fn admins_cant_lock_themselves_or_everyone_out() {
        let f = fixture(PasswordMode::Fallback);
        let admin = f.admin();
        let id = admin.user.id.clone();
        for r in [
            f.auth.set_user_disabled(&admin, &id, true, &meta()),
            f.auth.delete_user(&admin, &id, &meta()),
            f.auth.set_user_role(&admin, &id, Role::User, &meta()),
        ] {
            assert!(matches!(r, Err(AuthError::Forbidden(_))), "{r:?}");
        }
        // The synthetic local owner isn't a user, so only the last-admin rule stops it.
        let local = Principal::local();
        assert!(matches!(
            f.auth.delete_user(&local, &id, &meta()),
            Err(AuthError::Conflict(_))
        ));
        assert!(matches!(
            f.auth.set_user_disabled(&local, &id, true, &meta()),
            Err(AuthError::Conflict(_))
        ));
        assert!(matches!(
            f.auth.set_user_role(&local, &id, Role::Viewer, &meta()),
            Err(AuthError::Conflict(_))
        ));

        // Another admin can be demoted and deleted; their tokens go with them.
        let second = f
            .auth
            .create_invitation(&admin, Role::Admin, &[], None, None, &meta())
            .unwrap();
        let eve = enroll(&f, &second.secret, "eve").unwrap();
        assert!(eve.server_admin());
        let eve_token = token(&f, &eve, 1);
        f.auth
            .set_user_role(&admin, &eve.user.id, Role::Viewer, &meta())
            .unwrap();
        let eve = f.auth.authenticate_token(&eve_token).unwrap().unwrap();
        assert!(!eve.server_admin());
        // Not a server admin: manages nobody.
        assert!(matches!(
            f.auth.managed_users(&eve),
            Err(AuthError::Forbidden(_))
        ));
        f.auth.delete_user(&admin, &eve.user.id, &meta()).unwrap();
        assert!(f.auth.authenticate_token(&eve_token).unwrap().is_none());
        assert!(matches!(
            f.auth.delete_user(&admin, &eve.user.id, &meta()),
            Err(AuthError::NotFound)
        ));
        // Managing users needs a recent sign-in.
        f.clock.advance(Duration::from_secs(11 * 60));
        let zed_link = invite(&f, &local, &[(1, SpaceRole::User)]);
        let zed = enroll(&f, &zed_link.secret, "zed").unwrap();
        assert!(matches!(
            f.auth
                .set_user_disabled(&admin, &zed.user.id, true, &meta()),
            Err(AuthError::ReauthRequired)
        ));
    }

    #[test]
    fn recovery_replaces_credentials() {
        let f = fixture(PasswordMode::Fallback);
        let admin = f.admin();
        let link = invite(&f, &admin, &[(1, SpaceRole::User)]);
        let bob = enroll(&f, &link.secret, "bob").unwrap();
        let bob_token = token(&f, &bob, 1);
        let first = f
            .auth
            .create_recovery(&admin, &bob.user.id, None, &meta())
            .unwrap();
        let second = f
            .auth
            .create_recovery(&admin, &bob.user.id, Some(2), &meta())
            .unwrap();
        assert_eq!(second.record.kind, EnrollmentKind::Recovery);
        // A new link replaces the one still waiting.
        assert!(matches!(
            f.auth.enroll_info(&first.secret, &meta()),
            Err(AuthError::InvalidLink)
        ));
        let info = f.auth.enroll_info(&second.secret, &meta()).unwrap();
        assert_eq!(info.user_name.as_deref(), Some("bob"));

        // The username given is ignored; the new password replaces the old one, and every
        // session ends. API tokens stay.
        let s = f
            .auth
            .enroll_with_password(
                &second.secret,
                "mallory",
                None,
                "a brand new password",
                &meta(),
                None,
            )
            .unwrap();
        let again = f.auth.authenticate_session(&s.secret).unwrap().unwrap();
        assert_eq!(again.user.id, bob.user.id);
        assert!(!f.auth.still_valid(&bob).unwrap());
        assert!(f.auth.authenticate_token(&bob_token).unwrap().is_some());
        assert!(f.auth.password_login("bob", PW, &meta(), None).is_err());
        f.auth
            .password_login("bob", "a brand new password", &meta(), None)
            .unwrap();
        assert!(
            store::user_by_name(&f.auth.db(), "mallory")
                .unwrap()
                .is_none()
        );

        // A disabled user's link doesn't work until they are enabled.
        let third = f
            .auth
            .create_recovery(&admin, &bob.user.id, None, &meta())
            .unwrap();
        f.auth
            .set_user_disabled(&admin, &bob.user.id, true, &meta())
            .unwrap();
        assert!(matches!(
            f.auth.enroll_info(&third.secret, &meta()),
            Err(AuthError::Conflict(_))
        ));
        // Only server admins issue them.
        assert!(matches!(
            f.auth
                .create_recovery(&again, &admin.user.id, None, &meta()),
            Err(AuthError::Forbidden(_))
        ));
    }

    fn sign_in(f: &Fixture, name: &str) -> Principal {
        let s = f.auth.password_login(name, PW, &meta(), None).unwrap();
        f.auth.authenticate_session(&s.secret).unwrap().unwrap()
    }

    #[test]
    fn existing_users_join_invitations() {
        let f = fixture(PasswordMode::Fallback);
        let admin = f.admin();
        let sam_link = invite(&f, &admin, &[(2, SpaceRole::Admin), (3, SpaceRole::User)]);
        let sam = enroll(&f, &sam_link.secret, "sam").unwrap();
        let bob_link = invite(&f, &admin, &[(3, SpaceRole::Viewer)]);
        let bob = enroll(&f, &bob_link.secret, "bob").unwrap();
        let users_before = f.auth.users().unwrap().len();

        // A space admin invites into their space; bob, signed in, takes it into his account.
        let link = invite(&f, &sam, &[(2, SpaceRole::User)]);
        assert!(crate::joinable(
            &f.auth.enroll_info(&link.secret, &meta()).unwrap()
        ));
        let granted = f
            .auth
            .accept_invitation(&bob, &link.secret, &meta())
            .unwrap();
        assert_eq!(granted, [(2, SpaceRole::User)]);
        assert_eq!(
            f.auth.memberships(&bob.user.id).unwrap(),
            [(2, SpaceRole::User), (3, SpaceRole::Viewer)]
        );
        assert_eq!(f.auth.users().unwrap().len(), users_before, "no new user");
        assert_eq!(f.auth.last_space(&bob.user.id).unwrap(), Some(2));
        let all = f.auth.enrollments(&admin, None).unwrap();
        let rec = all.iter().find(|r| r.id == link.record.id).unwrap();
        assert_eq!(rec.state, EnrollmentState::Used);
        assert_eq!(rec.user_name.as_deref(), Some("bob"));
        // Once only, by joining or enrolling.
        assert!(matches!(
            f.auth.accept_invitation(&bob, &link.secret, &meta()),
            Err(AuthError::InvalidLink)
        ));
        assert!(matches!(
            enroll(&f, &link.secret, "carol"),
            Err(AuthError::InvalidLink)
        ));

        // Never a lower role; an invitation that gives nothing new stays unused.
        let lower = invite(
            &f,
            &admin,
            &[(2, SpaceRole::Viewer), (3, SpaceRole::Viewer)],
        );
        assert!(matches!(
            f.auth.accept_invitation(&bob, &lower.secret, &meta()),
            Err(AuthError::Conflict(_))
        ));
        assert_eq!(
            f.auth.memberships(&bob.user.id).unwrap(),
            [(2, SpaceRole::User), (3, SpaceRole::Viewer)]
        );
        let higher = invite(&f, &admin, &[(2, SpaceRole::Viewer), (3, SpaceRole::User)]);
        assert_eq!(
            f.auth
                .accept_invitation(&bob, &higher.secret, &meta())
                .unwrap(),
            [(3, SpaceRole::User)]
        );
        enroll(&f, &lower.secret, "dave").unwrap();

        // The instance role never changes: a viewer stays a viewer.
        f.auth
            .set_user_role(&admin, &bob.user.id, Role::Viewer, &meta())
            .unwrap();
        let bob = sign_in(&f, "bob");
        let more = invite(&f, &admin, &[(4, SpaceRole::User)]);
        f.auth
            .accept_invitation(&bob, &more.secret, &meta())
            .unwrap();
        assert_eq!(
            store::user_by_id(&f.auth.db(), &bob.user.id)
                .unwrap()
                .unwrap()
                .role,
            Role::Viewer
        );
        // An invitation that makes a server admin, and a recovery link, can't be joined.
        let admin_link = f
            .auth
            .create_invitation(
                &admin,
                Role::Admin,
                &[(5, SpaceRole::User)],
                None,
                None,
                &meta(),
            )
            .unwrap();
        assert!(!crate::joinable(&admin_link.record));
        assert!(matches!(
            f.auth.accept_invitation(&bob, &admin_link.secret, &meta()),
            Err(AuthError::Conflict(_))
        ));
        let recovery = f
            .auth
            .create_recovery(&admin, &sam.user.id, None, &meta())
            .unwrap();
        assert!(matches!(
            f.auth.accept_invitation(&bob, &recovery.secret, &meta()),
            Err(AuthError::InvalidRequest(_))
        ));
        // An API token can't join, even with a good link.
        let tok = token(&f, &sam, 2);
        let tok = f.auth.authenticate_token(&tok).unwrap().unwrap();
        let pending = invite(&f, &admin, &[(6, SpaceRole::User)]);
        assert!(matches!(
            f.auth.accept_invitation(&tok, &pending.secret, &meta()),
            Err(AuthError::Forbidden(_))
        ));
        // Wrong links count as failures, as for enrolling.
        assert!(matches!(
            f.auth.accept_invitation(&bob, &"x".repeat(43), &meta()),
            Err(AuthError::InvalidLink)
        ));
        let joined: i64 = f
            .auth
            .db()
            .query_row(
                "SELECT COUNT(*) FROM auth_events WHERE kind = 'joined'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(joined, 3);
    }

    #[test]
    fn the_last_space_admin_stays() {
        let f = fixture(PasswordMode::Fallback);
        let admin = f.admin();
        let sam_link = invite(&f, &admin, &[(2, SpaceRole::Admin)]);
        let sam = enroll(&f, &sam_link.secret, "sam").unwrap();
        // A server admin who is also listed as the space's admin doesn't count.
        f.auth
            .set_member(&admin, 2, &admin.user.id, SpaceRole::Admin, &meta())
            .unwrap();
        for r in [
            f.auth
                .set_member(&sam, 2, &sam.user.id, SpaceRole::User, &meta()),
            f.auth.remove_member(&sam, 2, &sam.user.id, &meta()),
        ] {
            assert!(matches!(r, Err(AuthError::Conflict(_))), "{r:?}");
        }
        // Staying admin is fine.
        f.auth
            .set_member(&sam, 2, &sam.user.id, SpaceRole::Admin, &meta())
            .unwrap();
        let tia_link = invite(&f, &admin, &[(2, SpaceRole::Admin)]);
        let tia = enroll(&f, &tia_link.secret, "tia").unwrap();
        // A disabled admin doesn't count either.
        f.auth
            .set_user_disabled(&admin, &tia.user.id, true, &meta())
            .unwrap();
        assert!(matches!(
            f.auth.remove_member(&sam, 2, &sam.user.id, &meta()),
            Err(AuthError::Conflict(_))
        ));
        // With a second admin of the space, either may step down.
        f.auth
            .set_user_disabled(&admin, &tia.user.id, false, &meta())
            .unwrap();
        f.auth
            .set_member(&sam, 2, &sam.user.id, SpaceRole::User, &meta())
            .unwrap();
        // Now tia is the last one; a server admin can still remove her.
        let tia = sign_in(&f, "tia");
        assert!(matches!(
            f.auth.remove_member(&tia, 2, &tia.user.id, &meta()),
            Err(AuthError::Conflict(_))
        ));
        f.auth
            .remove_member(&admin, 2, &tia.user.id, &meta())
            .unwrap();
        // A server admin is never stopped, even in a space they belong to.
        f.auth
            .remove_member(&admin, 2, &admin.user.id, &meta())
            .unwrap();
    }

    #[test]
    fn passwords_need_fallback_mode() {
        let f = fixture(PasswordMode::SecondFactor);
        let local = Principal::local();
        let link = invite(&f, &local, &[(1, SpaceRole::User)]);
        assert!(matches!(
            enroll(&f, &link.secret, "bob"),
            Err(AuthError::Unavailable(_))
        ));
    }
}
