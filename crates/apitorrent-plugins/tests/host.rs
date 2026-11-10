//! The plugin host end to end: real V8 isolates running small plugins.

mod common;

use std::time::Duration;

use serde_json::{Value, json};

use apitorrent_plugin_api::{
    AppEvent, PluginApiScope, PluginCapabilities, PluginError, PluginRole, PluginService,
    PluginStatus,
};
use apitorrent_types::TorrentState;

use common::{FakeApp, bundle, call, host, manifest, probe, probe_route, run, serve, wait_status};

fn probe_caps() -> PluginCapabilities {
    PluginCapabilities {
        routes: vec![probe_route()],
        ..PluginCapabilities::default()
    }
}

/// A plugin whose `/probe` route returns what `body` (a function body) returns.
fn probing(id: &str, caps: PluginCapabilities, body: &str) -> apitorrent_plugin_api::PluginBundle {
    bundle(
        manifest(id, "1.0.0", caps),
        &format!(
            "export default {{ start(api) {{ api.http.route('GET', '/probe', async () => {{ {body} }}); }} }};"
        ),
    )
}

/// Run `body` inside a try and report `{ ok, name, message }`.
fn attempt(body: &str) -> String {
    format!(
        "try {{ {body}; return {{ ok: true }}; }} catch (e) {{ return {{ ok: false, name: e.name, message: String(e.message) }}; }}"
    )
}

#[tokio::test(flavor = "multi_thread")]
async fn globals_are_the_curated_set() {
    let dir = tempfile::tempdir().unwrap();
    let h = host(dir.path(), FakeApp::new());
    run(
        &h,
        probing(
            "globals",
            probe_caps(),
            r"
            const builtins = new Set(Object.getOwnPropertyNames(
                Object.getPrototypeOf(Object.getPrototypeOf(globalThis)) ?? {}));
            const own = Object.getOwnPropertyNames(globalThis).sort();
            const u = new URL('../b?x=1#h', 'https://user@Example.COM:443/a/c');
            u.searchParams.append('q', 'a b&c');
            const enc = new TextEncoder().encode('héllo');
            const latin = new TextDecoder('windows-1252').decode(new Uint8Array([0x93, 0x41, 0x94]));
            const digest = Array.from(new Uint8Array(await crypto.subtle.digest('SHA-256', new TextEncoder().encode('abc'))))
                .map((b) => b.toString(16).padStart(2, '0')).join('');
            const key = await crypto.subtle.importKey('raw', new TextEncoder().encode('Jefe'),
                { name: 'HMAC', hash: 'SHA-256' }, false, ['sign', 'verify']);
            const sig = await crypto.subtle.sign('HMAC', key, new TextEncoder().encode('what do ya want for nothing?'));
            const timer = await new Promise((r) => setTimeout(() => r('fired'), 5));
            const ctl = new AbortController();
            let aborted = null;
            ctl.signal.addEventListener('abort', () => { aborted = ctl.signal.reason.name; });
            ctl.abort();
            const h = new Headers([['X-A', '1'], ['x-a', '2'], ['Set-Cookie', 'a'], ['set-cookie', 'b']]);
            const res = Response.json({ n: 1 }, { status: 201 });
            const clone = structuredClone({ d: new Date(0), m: new Map([[1, 2]]) });
            return {
                own,
                missing: ['Deno', '__bootstrap', '__infra', 'WebAssembly', 'SharedArrayBuffer', 'Atomics', 'process', 'require']
                    .filter((n) => n in globalThis),
                url: u.href, origin: u.origin, search: u.searchParams.toString(),
                bytes: enc.length, latin, digest,
                hmacBytes: new Uint8Array(sig).length,
                timer, aborted,
                headerA: h.get('x-a'), cookies: h.getSetCookie(),
                status: res.status, json: await res.json(),
                cloned: clone.m.get(1) === 2 && clone.d.getTime() === 0,
                uuid: /^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/.test(crypto.randomUUID()),
                b64: btoa('hi') + atob('aGk='),
                evalWorks: typeof eval,
            };
            ",
        ),
    )
    .await;
    let v = probe(&h, "globals").await;
    assert_eq!(v["missing"], json!([]), "{v:#}");
    let own: Vec<String> = serde_json::from_value(v["own"].clone()).unwrap();
    for name in [
        "fetch",
        "URL",
        "URLSearchParams",
        "TextEncoder",
        "TextDecoder",
        "setTimeout",
        "clearTimeout",
        "setInterval",
        "clearInterval",
        "structuredClone",
        "AbortController",
        "AbortSignal",
        "Headers",
        "Request",
        "Response",
        "crypto",
        "console",
        "queueMicrotask",
    ] {
        assert!(own.iter().any(|o| o == name), "{name} missing from {own:?}");
    }
    assert_eq!(v["url"], "https://user@example.com/b?x=1&q=a+b%26c#h");
    assert_eq!(v["origin"], "https://example.com");
    assert_eq!(v["bytes"], 6);
    assert_eq!(v["latin"], "\u{201c}A\u{201d}");
    assert_eq!(
        v["digest"],
        "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
    );
    assert_eq!(v["hmacBytes"], 32);
    assert_eq!(v["timer"], "fired");
    assert_eq!(v["aborted"], "AbortError");
    assert_eq!(v["headerA"], "1, 2");
    assert_eq!(v["cookies"], json!(["a", "b"]));
    assert_eq!(v["status"], 201);
    assert_eq!(v["json"], json!({ "n": 1 }));
    assert_eq!(v["cloned"], true);
    assert_eq!(v["uuid"], true);
    assert_eq!(v["b64"], "aGk=hi");
}

#[tokio::test(flavor = "multi_thread")]
async fn one_plugin_cannot_see_another_plugins_kv() {
    let dir = tempfile::tempdir().unwrap();
    let h = host(dir.path(), FakeApp::new());
    let caps = PluginCapabilities {
        kv: true,
        ..probe_caps()
    };
    run(
        &h,
        probing(
            "alice",
            caps.clone(),
            "await api.kv.set('secret', { owner: 'alice' }); return { mine: await api.kv.get('secret'), keys: await api.kv.list() };",
        ),
    )
    .await;
    run(
        &h,
        probing(
            "bob",
            caps,
            "const before = await api.kv.get('secret'); await api.kv.set('other', 1); return { before: before ?? null, keys: await api.kv.list() };",
        ),
    )
    .await;
    let a = probe(&h, "alice").await;
    assert_eq!(a["mine"], json!({ "owner": "alice" }));
    let b = probe(&h, "bob").await;
    assert_eq!(b["before"], Value::Null, "bob read alice's value: {b:#}");
    assert_eq!(b["keys"], json!(["other"]));
    // Alice's data is untouched by Bob's writes, and survives a restart.
    h.set_enabled("alice".into(), false).await.unwrap();
    h.set_enabled("alice".into(), true).await.unwrap();
    wait_status(&h, "alice", PluginStatus::Running).await;
    assert_eq!(probe(&h, "alice").await["keys"], json!(["secret"]));
}

#[tokio::test(flavor = "multi_thread")]
async fn undeclared_capabilities_fail() {
    let dir = tempfile::tempdir().unwrap();
    let h = host(dir.path(), FakeApp::new());
    let port = serve(|_| (200, vec![], b"ok".to_vec())).await;
    let checks = [
        ("kv", attempt("await api.kv.set('k', 1)")),
        (
            "add",
            attempt("await api.torrents.add({ magnet: 'magnet:?xt=urn:btih:' + '0'.repeat(40) })"),
        ),
        ("list", attempt("await api.torrents.list()")),
        (
            "events",
            attempt("api.events.on('torrent.added', () => {})"),
        ),
        (
            "schedule",
            attempt("api.schedule.every('x', 1000, () => {})"),
        ),
        (
            "route",
            attempt("api.http.route('POST', '/other', () => null)"),
        ),
        ("role", attempt("api.roles.provide('dns-provider', {})")),
        (
            "fetch",
            attempt(&format!("await fetch('http://127.0.0.1:{port}/')")),
        ),
    ];
    let body = checks
        .iter()
        .map(|(k, a)| format!("'{k}': await (async () => {{ {a} }})()"))
        .collect::<Vec<_>>()
        .join(", ");
    run(
        &h,
        probing("bare", probe_caps(), &format!("return {{ {body} }};")),
    )
    .await;
    let v = probe(&h, "bare").await;
    for (k, _) in &checks {
        assert_eq!(v[k]["ok"], false, "{k} should be denied: {v:#}");
        assert_eq!(v[k]["name"], "NotCapable", "{k}: {v:#}");
    }
    // Undeclared routes never reach the plugin at all.
    assert!(matches!(
        call(&h, "bare", "GET", "/elsewhere", b"").await,
        Err(PluginError::NoRoute)
    ));
    assert!(matches!(
        call(&h, "bare", "DELETE", "/probe", b"").await,
        Err(PluginError::NoRoute)
    ));
}

#[tokio::test(flavor = "multi_thread")]
async fn granted_api_calls_reach_the_app() {
    let dir = tempfile::tempdir().unwrap();
    let app = FakeApp::new();
    let h = host(dir.path(), app.clone());
    let caps = PluginCapabilities {
        api: vec![
            PluginApiScope::TorrentsAdd,
            PluginApiScope::TorrentsRead,
            PluginApiScope::TorrentsControl,
        ],
        ..probe_caps()
    };
    let magnet = format!("magnet:?xt=urn:btih:{}", "1".repeat(40));
    run(
        &h,
        probing(
            "adder",
            caps,
            &format!(
                r"
                const first = await api.torrents.add({{ magnet: '{magnet}', label: 'tv', savePath: 'tv/shows' }});
                const again = await api.torrents.add({{ magnet: '{magnet}' }});
                const escape = await (async () => {{ {} }})();
                const list = await api.torrents.list();
                await api.torrents.setLabel('aa', 'done');
                let missing = null;
                try {{ await api.torrents.get('nope'); }} catch (e) {{ missing = [e.name, e.code]; }}
                return {{ first, again, escape, size: list[0].size, missing }};
                ",
                attempt("await api.torrents.add({ magnet: 'magnet:?xt=urn:btih:' + '2'.repeat(40), savePath: '../outside' })")
            ),
        ),
    )
    .await;
    let v = probe(&h, "adder").await;
    assert_eq!(v["first"]["duplicate"], false);
    assert_eq!(v["again"], json!({ "id": null, "duplicate": true }));
    assert_eq!(v["escape"]["name"], "NotCapable", "{v:#}");
    // 64-bit numbers arrive as plain numbers.
    assert_eq!(v["size"], 5_000_000_000_u64);
    assert_eq!(v["missing"], json!(["ApitorrentError", "not_found"]));
    assert_eq!(app.magnets(), [magnet]);
    let added = app.added.lock().unwrap()[0].clone();
    assert_eq!(added.label.as_deref(), Some("tv"));
    assert_eq!(added.save_path.as_deref(), Some("tv/shows"));
    assert_eq!(app.controls.lock().unwrap().len(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn the_watchdog_stops_a_runaway_plugin() {
    let dir = tempfile::tempdir().unwrap();
    let h = host(dir.path(), FakeApp::new());
    run(&h, probing("spinner", probe_caps(), "for (;;) {}")).await;
    let started = std::time::Instant::now();
    let r = call(&h, "spinner", "GET", "/probe", b"").await;
    assert!(r.is_err(), "a runaway handler can't answer");
    assert!(started.elapsed() < Duration::from_secs(10));
    wait_status(&h, "spinner", PluginStatus::Failed).await;
    let info = h.get("spinner").unwrap();
    let error = info.error.unwrap();
    assert!(error.contains("watchdog"), "{error}");
    let logs = h.logs("spinner", 1, None).unwrap();
    assert!(
        logs.iter()
            .any(|l| l.host && l.message.contains("watchdog")),
        "{logs:#?}"
    );

    // Top-level code is watched too.
    h.install(bundle(
        manifest("toplevel", "1", probe_caps()),
        "while (true) {} export default {};",
    ))
    .await
    .unwrap();
    h.approve("toplevel".into(), "1".into()).await.unwrap();
    h.set_enabled("toplevel".into(), true).await.unwrap();
    wait_status(&h, "toplevel", PluginStatus::Failed).await;
    assert!(
        h.get("toplevel")
            .unwrap()
            .error
            .unwrap()
            .contains("watchdog")
    );

    // Re-enabling restarts it (and the watchdog stops it again).
    h.set_enabled("spinner".into(), true).await.unwrap();
    wait_status(&h, "spinner", PluginStatus::Running).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn memory_limits_hold() {
    let dir = tempfile::tempdir().unwrap();
    let h = host(dir.path(), FakeApp::new());
    // ArrayBuffers live outside the JavaScript heap; their own budget (16 MiB in tests)
    // refuses the allocation, and the plugin survives to catch it.
    run(
        &h,
        probing(
            "buffers",
            probe_caps(),
            r"
            const keep = [];
            try { for (let i = 0; i < 64; i++) keep.push(new Uint8Array(1024 * 1024)); }
            catch (e) { return { held: keep.length, error: e.name }; }
            return { held: keep.length, error: null };
            ",
        ),
    )
    .await;
    let v = probe(&h, "buffers").await;
    assert_eq!(v["error"], "RangeError", "{v:#}");
    let held = v["held"].as_u64().unwrap();
    assert!(
        (8..=16).contains(&held),
        "held {held} MiB under a 16 MiB budget"
    );
    // Freed buffers return to the budget.
    assert_eq!(probe(&h, "buffers").await["error"], "RangeError");

    // The heap limit ends a plugin that keeps allocating objects.
    run(
        &h,
        probing(
            "hog",
            probe_caps(),
            "const keep = []; for (;;) keep.push({ a: Math.random(), b: [1, 2, 3], c: 'x' + Math.random() });",
        ),
    )
    .await;
    let _ = call(&h, "hog", "GET", "/probe", b"").await;
    wait_status(&h, "hog", PluginStatus::Failed).await;
    let error = h.get("hog").unwrap().error.unwrap();
    assert!(
        error.contains("memory") || error.contains("watchdog"),
        "{error}"
    );
    // Other plugins carry on.
    assert_eq!(probe(&h, "buffers").await["error"], "RangeError");
}

#[tokio::test(flavor = "multi_thread")]
async fn fetch_is_checked_by_the_host() {
    let dir = tempfile::tempdir().unwrap();
    let h = host(dir.path(), FakeApp::new());
    let other = serve(|_| (200, vec![], b"other".to_vec())).await;
    let port = serve(move |path| match path {
        "/hello" => (
            200,
            vec![("x-test".into(), "yes".into())],
            b"hello".to_vec(),
        ),
        "/hop" => (302, vec![("location".into(), "/hello".into())], vec![]),
        "/escape" => (
            302,
            vec![("location".into(), format!("http://127.0.0.1:{other}/"))],
            vec![],
        ),
        "/big" => (200, vec![], vec![b'x'; 40 * 1024 * 1024]),
        _ => (404, vec![], vec![]),
    })
    .await;
    // A TLS server with a self-signed certificate.
    let tls_port = self_signed_server().await;
    let caps = PluginCapabilities {
        network: vec![
            format!("http://127.0.0.1:{port}"),
            format!("127.0.0.1:{tls_port}"),
        ],
        ..probe_caps()
    };
    let base = format!("http://127.0.0.1:{port}");
    run(
        &h,
        probing(
            "fetcher",
            caps,
            &format!(
                r"
                const r = await fetch('{base}/hello');
                const hop = await fetch('{base}/hop');
                const manual = await fetch('{base}/hop', {{ redirect: 'manual' }});
                const results = {{}};
                const check = async (name, f) => {{
                    try {{ await f(); results[name] = 'ok'; }}
                    catch (e) {{ results[name] = e.name + ': ' + e.message; }}
                }};
                await check('otherPort', () => fetch('http://127.0.0.1:{other}/'));
                await check('escape', () => fetch('{base}/escape'));
                await check('https', () => fetch('https://127.0.0.1:{tls_port}/'));
                await check('big', () => fetch('{base}/big'));
                await check('file', () => fetch('file:///etc/passwd'));
                await check('aborted', () => fetch('{base}/hello', {{ signal: AbortSignal.abort() }}));
                return {{
                    status: r.status, text: await r.text(), header: r.headers.get('x-test'),
                    hop: [hop.status, hop.redirected, hop.url, await hop.text()],
                    manual: [manual.status, manual.headers.get('location')],
                    results,
                }};
                "
            ),
        ),
    )
    .await;
    let v = probe(&h, "fetcher").await;
    assert_eq!(v["status"], 200);
    assert_eq!(v["text"], "hello");
    assert_eq!(v["header"], "yes");
    assert_eq!(
        v["hop"],
        json!([200, true, format!("{base}/hello"), "hello"])
    );
    assert_eq!(v["manual"], json!([302, "/hello"]));
    let results = &v["results"];
    let starts = |k: &str, prefix: &str| {
        let s = results[k].as_str().unwrap_or_default();
        assert!(s.starts_with(prefix), "{k}: {s}");
    };
    starts("otherPort", "NotCapable");
    // The redirect target isn't granted, even though the first hop was.
    starts("escape", "NotCapable");
    // Strict TLS: a self-signed certificate fails, granted or not.
    starts("https", "TypeError");
    starts("big", "TypeError");
    starts("file", "TypeError");
    starts("aborted", "AbortError");
    let tls = results["https"].as_str().unwrap().to_lowercase();
    assert!(
        tls.contains("certificate")
            || tls.contains("unknownissuer")
            || tls.contains("invalid peer"),
        "{tls}"
    );
}

/// A TLS server on 127.0.0.1 with a certificate no trust store accepts.
async fn self_signed_server() -> u16 {
    use std::sync::Arc;
    let cert = rcgen::generate_simple_self_signed(vec!["127.0.0.1".to_owned()]).unwrap();
    let key =
        tokio_rustls::rustls::pki_types::PrivateKeyDer::try_from(cert.signing_key.serialize_der())
            .unwrap();
    let config = tokio_rustls::rustls::ServerConfig::builder_with_provider(Arc::new(
        tokio_rustls::rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .unwrap()
    .with_no_client_auth()
    .with_single_cert(vec![cert.cert.der().clone()], key)
    .unwrap();
    let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(config));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        while let Ok((sock, _)) = listener.accept().await {
            let acceptor = acceptor.clone();
            tokio::spawn(async move {
                let _ = acceptor.accept(sock).await;
            });
        }
    });
    port
}

#[tokio::test(flavor = "multi_thread")]
async fn install_approve_and_upgrade() {
    let dir = tempfile::tempdir().unwrap();
    let h = host(dir.path(), FakeApp::new());
    let v1 = probing(
        "upg",
        probe_caps(),
        "return { version: api.plugin.version };",
    );
    let r = h.install(v1.clone()).await.unwrap();
    assert!(r.needs_approval);
    assert_eq!(r.plugin.status, PluginStatus::AwaitingApproval);
    assert_eq!(
        r.plugin.pending.as_ref().unwrap().new_capabilities,
        [
            "serve GET /probe under its API path",
            "serve POST /probe under its API path"
        ]
    );
    assert!(matches!(
        h.set_enabled("upg".into(), true).await,
        Err(PluginError::Conflict(_))
    ));
    assert!(matches!(
        h.approve("upg".into(), "9.9.9".into()).await,
        Err(PluginError::Conflict(_))
    ));
    // Installing the same bundle again is harmless; the same version with other code isn't.
    assert!(h.install(v1.clone()).await.unwrap().needs_approval);
    let mut tampered = v1.clone();
    tampered.code.push_str("\n// changed");
    assert!(matches!(
        h.install(tampered).await,
        Err(PluginError::Conflict(_))
    ));
    run(&h, v1).await;
    assert_eq!(probe(&h, "upg").await["version"], "1.0.0");

    // A new version within the grants replaces the running one at once.
    let mut v2 = probing(
        "upg",
        probe_caps(),
        "return { version: api.plugin.version };",
    );
    v2.manifest.version = "1.1.0".into();
    let r = h.install(v2).await.unwrap();
    assert!(!r.needs_approval);
    wait_status(&h, "upg", PluginStatus::Running).await;
    assert_eq!(probe(&h, "upg").await["version"], "1.1.0");

    // One that asks for more waits, and the running version carries on.
    let mut v3 = probing(
        "upg",
        PluginCapabilities {
            kv: true,
            network: vec!["*.example.org".into()],
            ..probe_caps()
        },
        "return { version: api.plugin.version };",
    );
    v3.manifest.version = "2.0.0".into();
    let r = h.install(v3).await.unwrap();
    assert!(r.needs_approval);
    assert_eq!(r.plugin.status, PluginStatus::Running);
    assert_eq!(
        r.plugin.pending.as_ref().unwrap().new_capabilities,
        ["connect to https://*.example.org", "store its own data"]
    );
    assert_eq!(probe(&h, "upg").await["version"], "1.1.0");
    h.approve("upg".into(), "2.0.0".into()).await.unwrap();
    wait_status(&h, "upg", PluginStatus::Running).await;
    assert_eq!(probe(&h, "upg").await["version"], "2.0.0");
    let info = h.get("upg").unwrap();
    assert!(info.granted.kv);
    assert!(info.pending.is_none());

    // A restarted host brings it back up.
    drop(h);
    let h = host(dir.path(), FakeApp::new());
    wait_status(&h, "upg", PluginStatus::Running).await;
    assert_eq!(probe(&h, "upg").await["version"], "2.0.0");
    h.uninstall("upg".into()).await.unwrap();
    assert!(matches!(h.get("upg"), Err(PluginError::NotFound)));
    assert!(h.list().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn settings_are_validated_and_secrets_hidden() {
    let dir = tempfile::tempdir().unwrap();
    let h = host(dir.path(), FakeApp::new());
    let mut b = probing("conf", probe_caps(), "return api.settings.get();");
    b.manifest.settings = Some(json!({
        "type": "object",
        "properties": {
            "interval": { "type": "integer", "minimum": 5, "default": 15 },
            "token": { "type": "string", "writeOnly": true },
        }
    }));
    b.code = "let seen = []; export default { start(api) { api.settings.onChange((s) => seen.push(s.interval)); \
              api.http.route('GET', '/probe', () => ({ now: api.settings.get(), seen })); } };"
        .into();
    run(&h, b).await;
    assert_eq!(probe(&h, "conf").await["now"], json!({ "interval": 15 }));
    let s = h
        .set_settings(
            "conf".into(),
            1,
            json!({ "interval": 30, "token": "hunter2" }),
        )
        .await
        .unwrap();
    assert_eq!(s.values, json!({ "interval": 30 }));
    assert_eq!(s.secrets_set, ["token"]);
    // The plugin sees the secret and the change, without a restart.
    tokio::time::sleep(Duration::from_millis(100)).await;
    let v = probe(&h, "conf").await;
    assert_eq!(v["now"], json!({ "interval": 30, "token": "hunter2" }));
    assert_eq!(v["seen"], json!([30]));
    // Leaving a secret out keeps it; null clears it.
    let s = h
        .set_settings("conf".into(), 1, json!({ "interval": 20 }))
        .await
        .unwrap();
    assert_eq!(s.secrets_set, ["token"]);
    let s = h
        .set_settings("conf".into(), 1, json!({ "token": null }))
        .await
        .unwrap();
    assert!(s.secrets_set.is_empty());
    for bad in [
        json!({ "interval": 1 }),
        json!({ "nope": true }),
        json!([1]),
    ] {
        assert!(matches!(
            h.set_settings("conf".into(), 1, bad).await,
            Err(PluginError::InvalidRequest(_))
        ));
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn schedules_events_and_roles() {
    let dir = tempfile::tempdir().unwrap();
    let app = FakeApp::new();
    let h = host(dir.path(), app.clone());
    let caps = PluginCapabilities {
        schedule: true,
        api: vec![PluginApiScope::EventsRead],
        roles: vec![PluginRole::Source],
        ..probe_caps()
    };
    let mut b = probing("busy", caps, "");
    b.code = r"
        let ticks = 0; const events = [];
        export default { start(api) {
            api.schedule.every('tick', 50, () => { ticks++; });
            api.events.on('torrent.finished', (e) => events.push(e.torrent.id));
            api.events.on('*', (e) => events.push(e.type));
            api.roles.provide('source', { candidates(limit) { return [{ id: 'c1', limit }]; } });
            api.http.route('GET', '/probe', () => ({ ticks, events }));
        } };"
        .into();
    run(&h, b).await;
    tokio::time::sleep(Duration::from_millis(400)).await;
    app.events
        .send(AppEvent::TorrentFinished {
            torrent: common::summary("bb", TorrentState::Seeding),
        })
        .unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    let v = probe(&h, "busy").await;
    assert!(v["ticks"].as_u64().unwrap() >= 3, "{v:#}");
    assert_eq!(v["events"], json!(["bb", "torrent.finished"]));
    let c = h
        .call_role("busy", PluginRole::Source, "candidates", json!([5]))
        .await
        .unwrap();
    assert_eq!(c, json!([{ "id": "c1", "limit": 5 }]));
    assert!(
        h.call_role("busy", PluginRole::Source, "missing", json!([]))
            .await
            .is_err()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn errors_are_logged_not_fatal() {
    let dir = tempfile::tempdir().unwrap();
    let h = host(dir.path(), FakeApp::new());
    let mut b = probing("noisy", probe_caps(), "");
    b.code = r"
        export default { start(api) {
            console.log('hello', { a: 1 }, [1, 2]);
            setTimeout(() => { throw new Error('timer boom'); }, 1);
            Promise.reject(new Error('unhandled boom'));
            api.http.route('GET', '/probe', () => { throw new Error('handler boom'); });
        } };"
        .into();
    run(&h, b).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(call(&h, "noisy", "GET", "/probe", b"").await.is_err());
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(h.get("noisy").unwrap().status, PluginStatus::Running);
    let logs: Vec<String> = h
        .logs("noisy", 1, None)
        .unwrap()
        .into_iter()
        .map(|l| l.message)
        .collect();
    for want in [
        "hello { a: 1 } [1, 2]",
        "timer boom",
        "unhandled boom",
        "handler boom",
    ] {
        assert!(
            logs.iter().any(|l| l.contains(want)),
            "{want} not in {logs:#?}"
        );
    }
    // A plugin whose start() throws fails with the reason.
    let mut bad = probing("broken", probe_caps(), "");
    bad.code = "export default { start() { throw new Error('no config'); } };".into();
    h.install(bad).await.unwrap();
    h.approve("broken".into(), "1.0.0".into()).await.unwrap();
    h.set_enabled("broken".into(), true).await.unwrap();
    wait_status(&h, "broken", PluginStatus::Failed).await;
    assert!(
        h.get("broken")
            .unwrap()
            .error
            .unwrap()
            .contains("no config")
    );
}

/// A `source` plugin hands candidates to the app, and the app fetches a candidate's link
/// through the host with that plugin's grants; plugins without the role can't submit.
#[tokio::test(flavor = "multi_thread")]
async fn sources_submit_candidates_and_links_are_fetched_with_their_grants() {
    use apitorrent_plugin_api::SourceFetcher as _;
    let dir = tempfile::tempdir().unwrap();
    let app = FakeApp::new();
    let h = host(dir.path(), app.clone());
    let port = serve(|path| match path {
        "/r.torrent" => (200, vec![], b"d4:infod4:name1:xee".to_vec()),
        _ => (404, vec![], vec![]),
    })
    .await;
    let base = format!("http://127.0.0.1:{port}");
    let caps = PluginCapabilities {
        network: vec![base.clone()],
        roles: vec![PluginRole::Source],
        ..probe_caps()
    };
    let submit = format!(
        "return await api.candidates.submit([{{ id: 'r1', title: 'Release 1', version: '1.0',          links: {{ torrent: '{base}/r.torrent' }}, metadata: {{ arch: 'amd64' }} }}]);"
    );
    run(&h, probing("src", caps, &submit)).await;
    let v = probe(&h, "src").await;
    assert_eq!(v, json!({ "accepted": 1, "rejected": [] }));
    let got = app.candidates.lock().unwrap().clone();
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].0, "src");
    assert_eq!(got[0].1.version.as_deref(), Some("1.0"));
    assert_eq!(got[0].1.metadata["arch"], "amd64");

    // The app fetches the link as the plugin: within its grants, and logged.
    let body = h
        .fetch_as("src", 1, &format!("{base}/r.torrent"), 1024)
        .await
        .unwrap();
    assert_eq!(body, b"d4:infod4:name1:xee");
    let logs = h.logs("src", 1, None).unwrap();
    assert!(
        logs.iter().any(|l| l.message.contains("for a rule")),
        "{logs:#?}"
    );
    // Outside its grants, too large, or an error status: refused.
    let refused = h
        .fetch_as("src", 1, "http://127.0.0.1:1/x", 1024)
        .await
        .unwrap_err();
    assert_eq!(refused.code, "not_capable");
    assert!(
        h.fetch_as("src", 1, &format!("{base}/r.torrent"), 4)
            .await
            .is_err()
    );
    assert!(
        h.fetch_as("src", 1, &format!("{base}/missing"), 1024)
            .await
            .is_err()
    );
    assert!(
        h.fetch_as("nobody", 1, &format!("{base}/r.torrent"), 1024)
            .await
            .is_err()
    );

    // Without the `source` role, submitting is refused.
    run(
        &h,
        probing(
            "plain",
            probe_caps(),
            &attempt("await api.candidates.submit([])"),
        ),
    )
    .await;
    let v = probe(&h, "plain").await;
    assert_eq!(v["name"], "NotCapable", "{v:#}");
    // And a plugin that isn't a source can't have links fetched for it either.
    assert!(
        h.fetch_as("plain", 1, &format!("{base}/r.torrent"), 1024)
            .await
            .is_err()
    );
}
