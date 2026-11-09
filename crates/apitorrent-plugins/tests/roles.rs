//! What the ecosystem plugins need from the host: their own sign-in (`tokens:verify` and
//! plugin-authenticated routes), app info and the wider torrent controls, the
//! `tracker-adapter` and `dns-provider` roles, and first-party bundles offered at startup.

mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use serde_json::{Value, json};

use apitorrent_plugin_api::{
    AppError, Credentials, PackQuery, PluginApiScope, PluginCaller, PluginCapabilities,
    PluginError, PluginHttpRequest, PluginRole, PluginRoute, PluginRouteAuth, PluginService,
    PluginStatus, TorrentControl, TrackerAdapters, UnregisteredQuery, VerifiedToken,
};
use apitorrent_plugins::dns::PluginDnsProvider;
use apitorrent_plugins::tracker::PluginTrackerAdapters;
use apitorrent_plugins::{HostConfig, PluginHost};
use apitorrent_tls::acme::{DnsProvider, TxtRecord};
use apitorrent_types::auth::Role;
use apitorrent_types::{AddSource, QueueMove, SeedLimitAction, SeedLimits};

use common::{FakeApp, bundle, call, manifest, probe_route, run, wait_status};

/// Tokens `apt_full` and `apt_read`; `apt_full` can be revoked.
struct FakeCredentials {
    full_live: Arc<AtomicBool>,
}

impl Credentials for FakeCredentials {
    fn verify_token(&self, token: &str) -> Result<Option<VerifiedToken>, AppError> {
        let caller = |may_write| PluginCaller {
            user_id: "u1".to_owned(),
            user_name: "alice".to_owned(),
            role: Role::Admin,
            may_write,
        };
        Ok(match token {
            "apt_full" => {
                let live = self.full_live.clone();
                live.load(Ordering::SeqCst).then(|| {
                    VerifiedToken::new(caller(true), "t1".to_owned(), move || {
                        live.load(Ordering::SeqCst)
                    })
                })
            }
            "apt_read" => Some(VerifiedToken::new(caller(false), "t2".to_owned(), || true)),
            _ => None,
        })
    }
}

fn host_with(
    dir: &std::path::Path,
    app: Arc<FakeApp>,
    creds: Option<FakeCredentials>,
) -> PluginHost {
    let mut config = HostConfig::new(dir);
    config.max_turn = Duration::from_millis(500);
    config.min_schedule_interval = Duration::ZERO;
    config.http_timeout = Duration::from_secs(20);
    config.credentials = creds.map(|c| Arc::new(c) as Arc<dyn Credentials>);
    PluginHost::start(config, app).expect("host starts")
}

fn route(path: &str, methods: &[&str], auth: PluginRouteAuth) -> PluginRoute {
    PluginRoute {
        path: path.to_owned(),
        methods: methods.iter().map(|m| (*m).to_owned()).collect(),
        auth,
    }
}

async fn request(
    h: &PluginHost,
    id: &str,
    method: &str,
    path: &str,
    body: &str,
    caller: Option<PluginCaller>,
) -> Result<Value, PluginError> {
    let r = h
        .http(
            id.to_owned(),
            PluginHttpRequest {
                method: method.to_owned(),
                path: path.to_owned(),
                query: None,
                url: format!("http://localhost:8742/api/v1/plugins/{id}{path}"),
                headers: Vec::new(),
                body: body.as_bytes().to_vec(),
                caller,
            },
        )
        .await?;
    assert_eq!(r.status, 200, "{}", String::from_utf8_lossy(&r.body));
    Ok(serde_json::from_slice(&r.body).unwrap())
}

#[tokio::test(flavor = "multi_thread")]
async fn plugins_sign_clients_in_with_api_tokens() {
    let dir = tempfile::tempdir().unwrap();
    let full_live = Arc::new(AtomicBool::new(true));
    let h = host_with(
        dir.path(),
        FakeApp::new(),
        Some(FakeCredentials {
            full_live: full_live.clone(),
        }),
    );
    let caps = PluginCapabilities {
        api: vec![PluginApiScope::TokensVerify],
        routes: vec![
            route("/login", &["POST"], PluginRouteAuth::Plugin),
            route("/whoami", &["POST"], PluginRouteAuth::Plugin),
            route("/logout", &["POST"], PluginRouteAuth::Plugin),
            route("/mine", &["GET"], PluginRouteAuth::Api),
        ],
        roles: vec![PluginRole::ApiCompat],
        ..PluginCapabilities::default()
    };
    let code = r"export default { start(api) {
        api.http.route('POST', '/login', async (req) => (await api.auth.login(await req.text())));
        api.http.route('POST', '/whoami', async (req, ctx) => ({
            caller: ctx.caller, session: await api.auth.session(await req.text()) }));
        api.http.route('POST', '/logout', async (req) => ({ ended: await api.auth.logout(await req.text()) }));
        api.http.route('GET', '/mine', async (req, ctx) => ({ caller: ctx.caller }));
    } };";
    run(&h, bundle(manifest("compat", "1.0.0", caps), code)).await;

    // The server asks how each route authenticates.
    assert_eq!(
        h.route_auth("compat", "POST", "/login"),
        Some(PluginRouteAuth::Plugin)
    );
    assert_eq!(
        h.route_auth("compat", "GET", "/mine"),
        Some(PluginRouteAuth::Api)
    );
    assert_eq!(h.route_auth("compat", "GET", "/login"), None);
    assert_eq!(h.route_auth("other", "GET", "/mine"), None);
    // An ordinary route never runs without a caller, whoever calls the host.
    assert_eq!(
        request(&h, "compat", "GET", "/mine", "", None)
            .await
            .unwrap_err(),
        PluginError::Unauthenticated
    );

    // Unknown tokens and non-tokens get no session.
    for bad in ["apt_nope", "", "password"] {
        assert_eq!(
            request(&h, "compat", "POST", "/login", bad, None)
                .await
                .unwrap(),
            Value::Null
        );
    }
    let login = request(&h, "compat", "POST", "/login", "apt_full", None)
        .await
        .unwrap();
    let session = login["session"].as_str().unwrap().to_owned();
    assert_eq!(session.len(), 64, "32 random bytes in hex");
    assert_eq!(login["caller"]["userName"], "alice");
    assert_eq!(login["caller"]["mayWrite"], true);
    let who = request(&h, "compat", "POST", "/whoami", &session, None)
        .await
        .unwrap();
    assert_eq!(
        who["caller"],
        Value::Null,
        "a plugin-authenticated request without sign-in"
    );
    assert_eq!(who["session"]["userId"], "u1");
    // A read-only token's session says it may not write.
    let read = request(&h, "compat", "POST", "/login", "apt_read", None)
        .await
        .unwrap();
    assert_eq!(read["caller"]["mayWrite"], false);

    // Revoking the token ends the session at once.
    full_live.store(false, Ordering::SeqCst);
    let who = request(&h, "compat", "POST", "/whoami", &session, None)
        .await
        .unwrap();
    assert_eq!(who["session"], Value::Null);
    full_live.store(true, Ordering::SeqCst);
    let who = request(&h, "compat", "POST", "/whoami", &session, None)
        .await
        .unwrap();
    assert_eq!(who["session"], Value::Null, "an ended session stays ended");

    // Logging out ends a session too.
    let s2 = request(&h, "compat", "POST", "/login", "apt_full", None)
        .await
        .unwrap()["session"]
        .as_str()
        .unwrap()
        .to_owned();
    let out = request(&h, "compat", "POST", "/logout", &s2, None)
        .await
        .unwrap();
    assert_eq!(out["ended"], true);
    let who = request(&h, "compat", "POST", "/whoami", &s2, None)
        .await
        .unwrap();
    assert_eq!(who["session"], Value::Null);
}

#[tokio::test(flavor = "multi_thread")]
async fn sign_in_needs_the_scope_and_working_authentication() {
    let dir = tempfile::tempdir().unwrap();
    // No credentials: authentication is off.
    let h = host_with(dir.path(), FakeApp::new(), None);
    let caps = |api| PluginCapabilities {
        api,
        routes: vec![route("/login", &["POST"], PluginRouteAuth::Plugin)],
        roles: vec![PluginRole::ApiCompat],
        ..PluginCapabilities::default()
    };
    let code = r"export default { start(api) {
        api.http.route('POST', '/login', async () => {
            try { return { ok: await api.auth.login('apt_full') }; }
            catch (e) { return { name: e.name, code: e.code ?? null }; }
        });
    } };";
    run(
        &h,
        bundle(
            manifest("a", "1.0.0", caps(vec![PluginApiScope::TokensVerify])),
            code,
        ),
    )
    .await;
    run(&h, bundle(manifest("b", "1.0.0", caps(vec![])), code)).await;
    let a = request(&h, "a", "POST", "/login", "", None).await.unwrap();
    assert_eq!(
        a,
        json!({ "name": "ApitorrentError", "code": "unavailable" })
    );
    let b = request(&h, "b", "POST", "/login", "", None).await.unwrap();
    assert_eq!(b["name"], "NotCapable");
}

#[tokio::test(flavor = "multi_thread")]
async fn app_info_and_the_wider_torrent_controls() {
    let dir = tempfile::tempdir().unwrap();
    let app = FakeApp::new();
    let h = host_with(dir.path(), app.clone(), None);
    let caps = PluginCapabilities {
        api: vec![
            PluginApiScope::TorrentsRead,
            PluginApiScope::TorrentsControl,
        ],
        routes: vec![probe_route()],
        ..PluginCapabilities::default()
    };
    let code = r"export default { start(api) {
        api.http.route('GET', '/probe', async () => {
            const info = await api.app.info();
            await api.torrents.setSeedLimits('aa', { ratio: 1.5, seedMinutes: 90 });
            await api.torrents.setSeedLimits('aa', null);
            await api.torrents.setRateLimits('aa', { download: 1000, upload: 2000 });
            await api.torrents.queue('aa', 'top');
            let bad = null;
            try { await api.torrents.queue('aa', 'sideways'); } catch (e) { bad = e.name; }
            return { info, bad };
        });
    } };";
    run(&h, bundle(manifest("ctl", "1.0.0", caps), code)).await;
    let r = call(&h, "ctl", "GET", "/probe", b"").await.unwrap();
    let v: Value = serde_json::from_slice(&r.body).unwrap();
    assert_eq!(v["info"]["downloadDir"], "/downloads");
    assert_eq!(v["info"]["peerPort"], 6881);
    assert_eq!(v["bad"], "TypeError");
    let controls: Vec<TorrentControl> = app
        .controls
        .lock()
        .unwrap()
        .iter()
        .map(|(_, c)| c.clone())
        .collect();
    assert_eq!(
        controls,
        [
            TorrentControl::SetSeedLimits(Some(SeedLimits {
                ratio: 1.5,
                seed_minutes: 90,
                action: SeedLimitAction::Pause,
            })),
            TorrentControl::SetSeedLimits(None),
            TorrentControl::SetRateLimits {
                download: 1000,
                upload: 2000
            },
            TorrentControl::Queue(QueueMove::Top),
        ]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn tracker_adapters_answer_for_their_trackers() {
    let dir = tempfile::tempdir().unwrap();
    let h = host_with(dir.path(), FakeApp::new(), None);
    let caps = PluginCapabilities {
        roles: vec![PluginRole::TrackerAdapter],
        ..PluginCapabilities::default()
    };
    let code = r"
    class Adapter {
        trackers = ['tracker.example.org', '*.example.net'];
        isUnregistered(q) { return q.message.includes('trumped by pack'); }
        findPack(q) {
            if (q.name === 'magnet please') return { magnet: 'magnet:?xt=urn:btih:' + 'ab'.repeat(20) };
            if (q.files.length === 2 && q.files[1].length === 7) return { torrent: new Uint8Array([1, 2, 3]) };
            return null;
        }
    }
    export default { start(api) { api.roles.provide('tracker-adapter', new Adapter()); } };";
    run(&h, bundle(manifest("adapter", "1.0.0", caps), code)).await;
    let logs = h.logs("adapter", None).unwrap();
    assert!(
        logs.iter().any(
            |l| l.message == "adapts behaviour for trackers tracker.example.org, *.example.net"
        ),
        "{logs:#?}"
    );

    let a = PluginTrackerAdapters::new(h.clone());
    assert!(a.covers("udp://tracker.example.org:6969/announce"));
    assert!(a.covers("https://x.example.net/a/announce?passkey=1"));
    assert!(!a.covers("https://example.net/announce"));
    assert!(!a.covers("http://other.org/announce"));
    let q = |tracker: &str, message: &str| UnregisteredQuery {
        torrent: "aa".into(),
        name: "Show.S01E01".into(),
        tracker: tracker.into(),
        message: message.into(),
    };
    let t = "udp://tracker.example.org:6969/announce";
    assert_eq!(a.unregistered(q(t, "trumped by pack")).await, Some(true));
    // The built-in patterns don't apply to a covered tracker.
    assert_eq!(
        a.unregistered(q(t, "Unregistered torrent")).await,
        Some(false)
    );
    assert_eq!(
        a.unregistered(q("http://other.org/a", "trumped by pack"))
            .await,
        None
    );

    let pack = |name: &str, lengths: &[u64]| PackQuery {
        torrent: "aa".into(),
        name: name.into(),
        info_hash_v1: Some("aa".repeat(20)),
        info_hash_v2: None,
        tracker: t.into(),
        files: lengths
            .iter()
            .enumerate()
            .map(|(i, l)| apitorrent_plugin_api::PackFile {
                path: format!("f{i}"),
                length: *l,
            })
            .collect(),
    };
    assert_eq!(
        a.find_pack(pack("magnet please", &[1])).await,
        Some(AddSource::Magnet {
            uri: format!("magnet:?xt=urn:btih:{}", "ab".repeat(20))
        })
    );
    assert_eq!(
        a.find_pack(pack("x", &[1, 7])).await,
        Some(AddSource::TorrentFile {
            data: "AQID".into()
        })
    );
    assert_eq!(a.find_pack(pack("x", &[1])).await, None);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_tracker_adapter_must_name_valid_trackers() {
    let dir = tempfile::tempdir().unwrap();
    let h = host_with(dir.path(), FakeApp::new(), None);
    let caps = PluginCapabilities {
        roles: vec![PluginRole::TrackerAdapter],
        ..PluginCapabilities::default()
    };
    let code = "export default { start(api) { api.roles.provide('tracker-adapter', { trackers: ['https://x/'] }); } };";
    let id = "bad-adapter";
    let r = h
        .install(bundle(manifest(id, "1.0.0", caps), code))
        .await
        .unwrap();
    assert!(r.needs_approval);
    h.approve(id.into(), "1.0.0".into()).await.unwrap();
    h.set_enabled(id.into(), true).await.unwrap();
    wait_status(&h, id, PluginStatus::Failed).await;
    assert!(
        h.get(id)
            .unwrap()
            .error
            .unwrap()
            .contains("must be a host name")
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_dns_provider_plugin_publishes_acme_records() {
    let dir = tempfile::tempdir().unwrap();
    let h = host_with(dir.path(), FakeApp::new(), None);
    let provider = PluginDnsProvider::new("dns");
    let record = TxtRecord {
        domain: "example.com".into(),
        name: "_acme-challenge.example.com".into(),
        value: "token-value".into(),
    };
    // Not attached to a host: plugins are off.
    let e = provider.publish_txt(&record).await.unwrap_err();
    assert!(e.0.contains("plugins are turned off"), "{e}");
    provider.attach(h.clone());

    let caps = PluginCapabilities {
        routes: vec![probe_route()],
        roles: vec![PluginRole::DnsProvider],
        ..PluginCapabilities::default()
    };
    let code = r"
    const records = [];
    export default { start(api) {
        api.roles.provide('dns-provider', {
            async setTxt(r) { records.push(['set', r.domain, r.name, r.value]); },
            async removeTxt(r) {
                if (r.value === 'fail') throw new Error('the DNS API said no');
                records.push(['remove', r.domain, r.name, r.value]);
            },
        });
        api.http.route('GET', '/probe', () => records);
    } };";
    run(&h, bundle(manifest("dns", "1.0.0", caps), code)).await;
    provider.publish_txt(&record).await.unwrap();
    provider.remove_txt(&record).await.unwrap();
    let failing = TxtRecord {
        value: "fail".into(),
        ..record.clone()
    };
    let e = provider.remove_txt(&failing).await.unwrap_err();
    assert!(e.0.contains("the DNS API said no"), "{e}");
    let r = call(&h, "dns", "GET", "/probe", b"").await.unwrap();
    let v: Value = serde_json::from_slice(&r.body).unwrap();
    assert_eq!(
        v,
        json!([
            [
                "set",
                "example.com",
                "_acme-challenge.example.com",
                "token-value"
            ],
            [
                "remove",
                "example.com",
                "_acme-challenge.example.com",
                "token-value"
            ],
        ])
    );

    // A plugin without the role is refused with a reason.
    let other = PluginDnsProvider::new("nope");
    other.attach(h.clone());
    let e = other.publish_txt(&record).await.unwrap_err();
    assert!(e.0.contains("nope"), "{e}");
}

#[tokio::test(flavor = "multi_thread")]
async fn first_party_bundles_are_offered_once() {
    let dir = tempfile::tempdir().unwrap();
    let h = host_with(dir.path(), FakeApp::new(), None);
    let code = "export default { start() {} };";
    let v = |version: &str| {
        bundle(
            manifest("first", version, PluginCapabilities::default()),
            code,
        )
    };

    h.offer_bundled(vec![v("1.0.0")]).await;
    let info = h.get("first").unwrap();
    assert_eq!(
        info.status,
        PluginStatus::AwaitingApproval,
        "offered, not approved"
    );
    assert_eq!(info.pending.unwrap().manifest.version, "1.0.0");

    // Approved and running; the same version offered again changes nothing.
    h.approve("first".into(), "1.0.0".into()).await.unwrap();
    h.set_enabled("first".into(), true).await.unwrap();
    wait_status(&h, "first", PluginStatus::Running).await;
    h.offer_bundled(vec![v("1.0.0")]).await;
    assert_eq!(h.get("first").unwrap().active.unwrap().version, "1.0.0");

    // A newer version within the grants upgrades it; an older one is never offered.
    h.offer_bundled(vec![v("1.1.0")]).await;
    assert_eq!(h.get("first").unwrap().active.unwrap().version, "1.1.0");
    h.offer_bundled(vec![v("1.0.9")]).await;
    assert_eq!(h.get("first").unwrap().active.unwrap().version, "1.1.0");

    // Uninstalled by an admin: not offered again, even in a newer version.
    h.uninstall("first".into()).await.unwrap();
    h.offer_bundled(vec![v("1.2.0")]).await;
    assert_eq!(h.get("first").unwrap_err(), PluginError::NotFound);
}
