//! ACME DNS-01 through a `dns-provider` plugin: the first-party `cloudflare-dns` plugin,
//! built with Bun, in the real plugin host, against a mock of Cloudflare's API on
//! 127.0.0.1. Nothing talks to the real Cloudflare or to any real DNS.
//!
//! - `cloudflare_plugin_publishes_through_the_host` runs everywhere: the provider glue,
//!   the plugin and the mock API.
//! - `acme_issues_a_wildcard_with_dns_01` (ignored by default) also runs Pebble and
//!   `pebble-challtestsrv` (see `apitorrent-server/tests/pebble/mod.rs`): the mock API
//!   publishes the records it is given to `pebble-challtestsrv`'s DNS server, the ACME
//!   manager waits until that server serves them, and Pebble validates against it.
//!
//!   ```text
//!   cargo test -p apitorrentd --test acme_dns01 -- --ignored
//!   ```

mod common;
#[path = "../../apitorrent-server/tests/pebble/mod.rs"]
mod pebble;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Json;
use axum::extract::{Path as UrlPath, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::routing::{delete, get};
use serde_json::{Value, json};

use apitorrent_plugin_api::{
    AddOutcome, AppApi, AppError, AppEvent, AppInfo, BoxFuture, PluginService, PluginStatus,
    TorrentControl,
};
use apitorrent_plugins::dns::PluginDnsProvider;
use apitorrent_plugins::{HostConfig, PluginHost};
use apitorrent_tls::CertStore;
use apitorrent_tls::acme::{AcmeManager, ChallengeKind, DnsProvider, TxtRecord};
use apitorrent_types::{AddTorrentRequest, TorrentDetail, TorrentId, TorrentSummary};

use common::build_plugin;
use pebble::{Pebble, wait_for_acme_cert};

const TOKEN: &str = "cf-scoped-test-token";

/// The plugin host needs an app; DNS providers ask nothing of it.
struct NoApp(tokio::sync::broadcast::Sender<AppEvent>);

impl AppApi for NoApp {
    fn info(&self) -> AppInfo {
        AppInfo {
            version: "test".into(),
            download_dir: String::new(),
            seed_limits: apitorrent_types::SeedLimits::default(),
            queue: apitorrent_types::QueueSettings::default(),
            peer_port: 0,
        }
    }
    fn torrents(&self) -> Vec<TorrentSummary> {
        Vec::new()
    }
    fn torrent(&self, _: TorrentId) -> BoxFuture<Result<TorrentDetail, AppError>> {
        Box::pin(async { Err(AppError::new("not_found", "none")) })
    }
    fn add_torrent(&self, _: AddTorrentRequest) -> BoxFuture<Result<AddOutcome, AppError>> {
        Box::pin(async { Err(AppError::new("unsupported", "none")) })
    }
    fn control(&self, _: TorrentId, _: TorrentControl) -> BoxFuture<Result<(), AppError>> {
        Box::pin(async { Err(AppError::new("unsupported", "none")) })
    }
    fn remove(&self, _: TorrentId, _: bool) -> BoxFuture<Result<(), AppError>> {
        Box::pin(async { Err(AppError::new("unsupported", "none")) })
    }
    fn events(&self) -> tokio::sync::broadcast::Receiver<AppEvent> {
        self.0.subscribe()
    }
}

#[derive(Debug, Clone)]
struct MockRecord {
    id: String,
    name: String,
    content: String,
}

/// Cloudflare's v4 API as far as the plugin uses it: one zone, `apitorrent.test`. With a
/// Pebble, records it holds are also served by `pebble-challtestsrv`'s DNS.
#[derive(Clone)]
struct MockCloudflare {
    records: Arc<Mutex<Vec<MockRecord>>>,
    log: Arc<Mutex<Vec<String>>>,
    pebble: Option<Arc<Pebble>>,
}

fn cf_ok(result: Value) -> (StatusCode, Json<Value>) {
    let mut body = json!({ "success": true, "errors": [] });
    body["result"] = result;
    (StatusCode::OK, Json(body))
}

fn cf_err(status: StatusCode, code: u32, message: &str) -> (StatusCode, Json<Value>) {
    (
        status,
        Json(
            json!({ "success": false, "errors": [{ "code": code, "message": message }], "result": null }),
        ),
    )
}

fn authorised(h: &HeaderMap) -> bool {
    h.get("authorization").and_then(|v| v.to_str().ok()) == Some(&format!("Bearer {TOKEN}"))
}

fn unquote(s: &str) -> String {
    s.trim_matches('"').to_owned()
}

impl MockCloudflare {
    /// Serve the TXT values held for `name` (and only those) from the Pebble DNS.
    async fn sync_dns(&self, name: &str) {
        let Some(p) = &self.pebble else { return };
        let values: Vec<String> = self
            .records
            .lock()
            .unwrap()
            .iter()
            .filter(|r| r.name == name)
            .map(|r| unquote(&r.content))
            .collect();
        p.clear_txt(name).await;
        for v in values {
            p.set_txt(name, &v).await;
        }
    }

    async fn serve(self) -> String {
        async fn zones(
            State(m): State<MockCloudflare>,
            headers: HeaderMap,
            Query(q): Query<std::collections::HashMap<String, String>>,
        ) -> (StatusCode, Json<Value>) {
            if !authorised(&headers) {
                return cf_err(StatusCode::FORBIDDEN, 10000, "Authentication error");
            }
            let name = q.get("name").cloned().unwrap_or_default();
            m.log.lock().unwrap().push(format!("zones {name}"));
            if name == "apitorrent.test" {
                cf_ok(json!([{ "id": "zone-1", "name": "apitorrent.test" }]))
            } else {
                cf_ok(json!([]))
            }
        }
        async fn records(
            State(m): State<MockCloudflare>,
            headers: HeaderMap,
            UrlPath(zone): UrlPath<String>,
            Query(q): Query<std::collections::HashMap<String, String>>,
            body: Option<Json<Value>>,
        ) -> (StatusCode, Json<Value>) {
            if !authorised(&headers) {
                return cf_err(StatusCode::FORBIDDEN, 10000, "Authentication error");
            }
            if zone != "zone-1" {
                return cf_err(StatusCode::NOT_FOUND, 7003, "Could not route to zone");
            }
            if let Some(Json(b)) = body {
                let name = b["name"].as_str().unwrap_or_default().to_owned();
                let content = b["content"].as_str().unwrap_or_default().to_owned();
                assert_eq!(b["type"], "TXT");
                assert!(content.starts_with('"'), "quoted TXT content: {content}");
                let rec = {
                    let mut all = m.records.lock().unwrap();
                    let rec = MockRecord {
                        id: format!("rec-{}", all.len() + 1),
                        name: name.clone(),
                        content,
                    };
                    all.push(rec.clone());
                    rec
                };
                m.log.lock().unwrap().push(format!("create {name}"));
                m.sync_dns(&name).await;
                return cf_ok(
                    json!({ "id": rec.id, "name": rec.name, "type": "TXT", "content": rec.content }),
                );
            }
            let name = q.get("name").cloned().unwrap_or_default();
            let list: Vec<Value> = m
                .records
                .lock()
                .unwrap()
                .iter()
                .filter(|r| r.name == name)
                .map(|r| json!({ "id": r.id, "name": r.name, "type": "TXT", "content": r.content }))
                .collect();
            cf_ok(Value::Array(list))
        }
        async fn remove(
            State(m): State<MockCloudflare>,
            headers: HeaderMap,
            UrlPath((_zone, id)): UrlPath<(String, String)>,
        ) -> (StatusCode, Json<Value>) {
            if !authorised(&headers) {
                return cf_err(StatusCode::FORBIDDEN, 10000, "Authentication error");
            }
            let name = {
                let mut all = m.records.lock().unwrap();
                let Some(i) = all.iter().position(|r| r.id == id) else {
                    return cf_err(StatusCode::NOT_FOUND, 81044, "Record does not exist.");
                };
                all.remove(i).name
            };
            m.log.lock().unwrap().push(format!("delete {name}"));
            m.sync_dns(&name).await;
            cf_ok(json!({ "id": id }))
        }
        let app = axum::Router::new()
            .route("/client/v4/zones", get(zones))
            .route(
                "/client/v4/zones/{zone}/dns_records",
                get(records).post(records),
            )
            .route("/client/v4/zones/{zone}/dns_records/{id}", delete(remove))
            .with_state(self);
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(l, app).await });
        format!("http://{addr}/client/v4")
    }
}

/// A host running `cloudflare-dns`, configured for the mock, and a provider attached to it.
async fn provider(
    dir: &std::path::Path,
    api_base: &str,
    token: &str,
) -> (PluginHost, Arc<PluginDnsProvider>) {
    let bundle = build_plugin("cloudflare-dns", &dir.join("dist"));
    let mut config = HostConfig::new(dir.join("plugins"));
    config.http_timeout = Duration::from_secs(20);
    let host = PluginHost::start(
        config,
        Arc::new(NoApp(tokio::sync::broadcast::channel(4).0)),
    )
    .unwrap();
    let r = host.install(bundle).await.unwrap();
    let wants = r.plugin.pending.unwrap().new_capabilities;
    assert!(
        wants.iter().any(|w| w.contains("publish DNS records")),
        "{wants:?}"
    );
    host.approve("cloudflare-dns".into(), "1.0.0".into())
        .await
        .unwrap();
    host.set_settings(
        "cloudflare-dns".into(),
        json!({ "apiToken": token, "apiBase": api_base }),
    )
    .await
    .unwrap();
    host.set_enabled("cloudflare-dns".into(), true)
        .await
        .unwrap();
    for _ in 0..200 {
        if host.get("cloudflare-dns").unwrap().status == PluginStatus::Running {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    let p = PluginDnsProvider::new("cloudflare-dns");
    p.attach(host.clone());
    (host, p)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cloudflare_plugin_publishes_through_the_host() {
    let dir = tempfile::tempdir().unwrap();
    let mock = MockCloudflare {
        records: Arc::default(),
        log: Arc::default(),
        pebble: None,
    };
    let base = mock.clone().serve().await;
    let (host, p) = provider(dir.path(), &base, TOKEN).await;
    let rec = |v: &str| TxtRecord {
        domain: "dns.apitorrent.test".into(),
        name: "_acme-challenge.dns.apitorrent.test".into(),
        value: v.into(),
    };
    p.publish_txt(&rec("one")).await.unwrap();
    p.publish_txt(&rec("two")).await.unwrap();
    p.remove_txt(&rec("one")).await.unwrap();
    let left: Vec<String> = mock
        .records
        .lock()
        .unwrap()
        .iter()
        .map(|r| r.content.clone())
        .collect();
    assert_eq!(left, ["\"two\""]);
    assert_eq!(
        mock.log.lock().unwrap()[..3],
        [
            "zones dns.apitorrent.test",
            "zones apitorrent.test",
            "create _acme-challenge.dns.apitorrent.test",
        ]
    );
    host.shutdown().await;

    // A wrong token: Cloudflare's refusal reaches the ACME manager as the error.
    let dir = tempfile::tempdir().unwrap();
    let (host, p) = provider(dir.path(), &base, "wrong").await;
    let e = p.publish_txt(&rec("three")).await.unwrap_err();
    assert!(e.0.contains("Authentication error"), "{e}");
    host.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs pebble and pebble-challtestsrv (see apitorrent-server/tests/pebble/mod.rs)"]
async fn acme_issues_a_wildcard_with_dns_01() {
    let dir = tempfile::tempdir().unwrap();
    // DNS-01 needs no listener of ours; the HTTP and TLS ports are never used.
    let pebble = Arc::new(Pebble::start(&dir.path().join("pebble"), 1, 1).await);
    let mock = MockCloudflare {
        records: Arc::default(),
        log: Arc::default(),
        pebble: Some(pebble.clone()),
    };
    let base = mock.clone().serve().await;
    let (host, provider) = provider(dir.path(), &base, TOKEN).await;

    let store = CertStore::new();
    let domains = ["dns.apitorrent.test", "*.dns.apitorrent.test"];
    let mut config = pebble.config(
        &domains,
        &dir.path().join("acme"),
        ChallengeKind::Dns01(provider),
    );
    // Pebble's DNS server stands in for the zone's name servers.
    config.dns_propagation.resolvers = vec![pebble.dns];
    config.dns_propagation.interval = Duration::from_millis(200);
    config.dns_propagation.timeout = Duration::from_secs(30);
    config.validate().unwrap();
    let manager = AcmeManager::new(store.clone(), config).unwrap();
    let status = manager.status();
    let handle = manager.start();
    wait_for_acme_cert(&store, &domains, &pebble.logs).await;
    handle.stop();
    assert_eq!(status.borrow().last_error, None);

    // Both records (the wildcard's and the base name's share a name) were published and
    // removed again.
    let log = mock.log.lock().unwrap().clone();
    let count = |what: &str| log.iter().filter(|l| l.as_str() == what).count();
    assert_eq!(
        count("create _acme-challenge.dns.apitorrent.test"),
        2,
        "{log:?}"
    );
    assert_eq!(
        count("delete _acme-challenge.dns.apitorrent.test"),
        2,
        "{log:?}"
    );
    assert!(mock.records.lock().unwrap().is_empty());
    host.shutdown().await;
}
