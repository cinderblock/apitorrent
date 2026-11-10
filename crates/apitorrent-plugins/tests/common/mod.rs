//! Shared test helpers: a fake app, bundle building, plugin route calls and a tiny HTTP
//! server on 127.0.0.1.

#![allow(dead_code)]

use std::fmt::Write as _;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::sync::broadcast;

use apitorrent_plugin_api::{
    AddOutcome, AppApi, AppError, AppEvent, AppInfo, BoxFuture, PLUGIN_API_VERSION,
    PLUGIN_BUNDLE_FORMAT, PluginBundle, PluginCaller, PluginCapabilities, PluginHttpRequest,
    PluginHttpResponse, PluginManifest, PluginRouteAuth, PluginService, PluginStatus,
    TorrentControl,
};
use apitorrent_plugins::{HostConfig, PluginHost};
use apitorrent_types::auth::Role;
use apitorrent_types::{
    AddSource, AddTorrentRequest, TorrentDetail, TorrentId, TorrentState, TorrentSummary,
};

/// An app that records what plugins ask of it.
pub struct FakeApp {
    pub added: Mutex<Vec<AddTorrentRequest>>,
    pub controls: Mutex<Vec<(TorrentId, TorrentControl)>>,
    pub events: broadcast::Sender<AppEvent>,
    /// Release candidates submitted, with their source.
    pub candidates: Mutex<Vec<(String, apitorrent_types::automation::ReleaseCandidate)>>,
}

impl FakeApp {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            added: Mutex::new(Vec::new()),
            controls: Mutex::new(Vec::new()),
            events: broadcast::channel(64).0,
            candidates: Mutex::new(Vec::new()),
        })
    }

    pub fn magnets(&self) -> Vec<String> {
        self.added
            .lock()
            .unwrap()
            .iter()
            .filter_map(|r| match &r.source {
                AddSource::Magnet { uri } => Some(uri.clone()),
                AddSource::TorrentFile { .. } => None,
            })
            .collect()
    }
}

pub fn summary(id: &str, state: TorrentState) -> TorrentSummary {
    TorrentSummary {
        id: id.to_owned(),
        space: 1,
        name: format!("torrent {id}"),
        info_hash_v1: Some(id.to_owned()),
        info_hash_v2: None,
        state,
        progress: 1.0,
        size: 5_000_000_000,
        pieces_have: 10,
        pieces_total: 10,
        downloaded: 0,
        uploaded: 0,
        download_rate: 0,
        upload_rate: 0,
        peers: 0,
        seeds: 0,
        added_at: 1_700_000_000_000,
        label: None,
        save_path: "/downloads".to_owned(),
        private: false,
        error: None,
        queue_position: 0,
        group: None,
        superseded_by: None,
    }
}

impl AppApi for FakeApp {
    fn info(&self) -> AppInfo {
        AppInfo {
            version: "0.1.0".to_owned(),
            download_dir: "/downloads".to_owned(),
            seed_limits: apitorrent_types::SeedLimits::default(),
            queue: apitorrent_types::QueueSettings::default(),
            peer_port: 6881,
        }
    }

    fn torrents(&self) -> Vec<TorrentSummary> {
        vec![summary("aa", TorrentState::Seeding)]
    }

    fn torrent(&self, id: TorrentId) -> BoxFuture<Result<TorrentDetail, AppError>> {
        Box::pin(async move { Err(AppError::new("not_found", format!("no torrent {id}"))) })
    }

    fn add_torrent(&self, req: AddTorrentRequest) -> BoxFuture<Result<AddOutcome, AppError>> {
        let mut added = self.added.lock().unwrap();
        let duplicate = added.iter().any(|r| r.source == req.source);
        if !duplicate {
            added.push(req);
        }
        let id = (!duplicate).then(|| format!("{:040}", added.len()));
        Box::pin(async move { Ok(AddOutcome { id, duplicate }) })
    }

    fn control(&self, id: TorrentId, action: TorrentControl) -> BoxFuture<Result<(), AppError>> {
        self.controls.lock().unwrap().push((id, action));
        Box::pin(async { Ok(()) })
    }

    fn remove(&self, _id: TorrentId, _delete: bool) -> BoxFuture<Result<(), AppError>> {
        Box::pin(async { Ok(()) })
    }

    fn events(&self) -> broadcast::Receiver<AppEvent> {
        self.events.subscribe()
    }

    fn submit_candidates(
        &self,
        source: &str,
        candidates: Vec<apitorrent_types::automation::ReleaseCandidate>,
    ) -> BoxFuture<Result<apitorrent_types::automation::SubmitCandidatesOutcome, AppError>> {
        let accepted = u32::try_from(candidates.len()).unwrap();
        self.candidates
            .lock()
            .unwrap()
            .extend(candidates.into_iter().map(|c| (source.to_owned(), c)));
        Box::pin(async move {
            Ok(apitorrent_types::automation::SubmitCandidatesOutcome {
                accepted,
                rejected: Vec::new(),
            })
        })
    }
}

/// A host for tests: short watchdog, no minimum schedule interval, small memory limits.
pub fn host(dir: &std::path::Path, app: Arc<FakeApp>) -> PluginHost {
    let mut config = HostConfig::new(dir);
    config.max_turn = Duration::from_millis(500);
    config.min_schedule_interval = Duration::ZERO;
    config.heap_limit = 32 * 1024 * 1024;
    config.buffer_limit = 16 * 1024 * 1024;
    config.http_timeout = Duration::from_secs(20);
    PluginHost::start(config, app).expect("host starts")
}

pub fn manifest(id: &str, version: &str, caps: PluginCapabilities) -> PluginManifest {
    PluginManifest {
        id: id.to_owned(),
        name: id.to_owned(),
        version: version.to_owned(),
        description: String::new(),
        author: None,
        homepage: None,
        api_version: PLUGIN_API_VERSION,
        capabilities: caps,
        settings: None,
        space_aware: false,
    }
}

pub fn bundle(manifest: PluginManifest, code: &str) -> PluginBundle {
    PluginBundle {
        format: PLUGIN_BUNDLE_FORMAT,
        manifest,
        code: code.to_owned(),
    }
}

/// A probe route every test plugin can declare.
pub fn probe_route() -> apitorrent_plugin_api::PluginRoute {
    apitorrent_plugin_api::PluginRoute {
        path: "/probe".to_owned(),
        methods: vec!["GET".to_owned(), "POST".to_owned()],
        auth: PluginRouteAuth::Api,
    }
}

/// Install, approve and enable a bundle, then wait until it runs.
pub async fn run(host: &PluginHost, bundle: PluginBundle) {
    let id = bundle.manifest.id.clone();
    let version = bundle.manifest.version.clone();
    let r = host.install(bundle).await.expect("install");
    if r.needs_approval {
        host.approve(id.clone(), version).await.expect("approve");
    }
    host.set_enabled(id.clone(), true).await.expect("enable");
    wait_status(host, &id, PluginStatus::Running).await;
}

pub async fn wait_status(host: &PluginHost, id: &str, want: PluginStatus) {
    for _ in 0..400 {
        let info = host.get(id).expect("plugin exists");
        if info.status == want {
            return;
        }
        assert!(
            !(want == PluginStatus::Running && info.status == PluginStatus::Failed),
            "{id} failed instead of running: {:?}\nlogs: {:#?}",
            info.error,
            host.logs(id, 1, None).unwrap()
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!(
        "{id} never reached {want:?}: {:?}\nlogs: {:#?}",
        host.get(id).unwrap(),
        host.logs(id, 1, None).unwrap()
    );
}

pub fn caller() -> PluginCaller {
    PluginCaller {
        user_id: "u1".to_owned(),
        user_name: "admin".to_owned(),
        role: Role::Admin,
        may_write: true,
    }
}

pub async fn call(
    host: &PluginHost,
    id: &str,
    method: &str,
    path: &str,
    body: &[u8],
) -> Result<PluginHttpResponse, apitorrent_plugin_api::PluginError> {
    host.http(
        id.to_owned(),
        PluginHttpRequest {
            method: method.to_owned(),
            path: path.to_owned(),
            query: None,
            url: format!("http://localhost:8742/api/v1/plugins/{id}{path}"),
            headers: vec![("content-type".to_owned(), "application/json".to_owned())],
            body: body.to_vec(),
            caller: Some(caller()),
            space: 1,
        },
    )
    .await
}

/// GET the plugin's `/probe` route and parse its JSON.
pub async fn probe(host: &PluginHost, id: &str) -> serde_json::Value {
    let r = match call(host, id, "GET", "/probe", b"").await {
        Ok(r) => r,
        Err(e) => {
            tokio::time::sleep(Duration::from_secs(2)).await;
            panic!(
                "probe of {id} failed: {e:?}
info: {:?}
logs: {:#?}",
                host.get(id),
                host.logs(id, 1, None)
            )
        }
    };
    assert_eq!(r.status, 200, "{}", String::from_utf8_lossy(&r.body));
    serde_json::from_slice(&r.body).expect("probe returns JSON")
}

/// A minimal HTTP/1.1 server on 127.0.0.1 that answers each request with `respond(path)`:
/// (status, extra headers, body). Returns its port.
pub async fn serve<F>(respond: F) -> u16
where
    F: Fn(&str) -> (u16, Vec<(String, String)>, Vec<u8>) + Send + Sync + 'static,
{
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let respond = Arc::new(respond);
    tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else {
                return;
            };
            let respond = respond.clone();
            tokio::spawn(async move {
                let mut buf = Vec::new();
                let mut chunk = [0u8; 4096];
                while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
                    match sock.read(&mut chunk).await {
                        Ok(0) | Err(_) => return,
                        Ok(n) => buf.extend_from_slice(&chunk[..n]),
                    }
                }
                let head = String::from_utf8_lossy(&buf).into_owned();
                let path = head.split_whitespace().nth(1).unwrap_or("/").to_owned();
                let (status, headers, body) = respond(&path);
                let mut out = format!(
                    "HTTP/1.1 {status} X\r\ncontent-length: {}\r\nconnection: close\r\n",
                    body.len()
                );
                for (k, v) in headers {
                    let _ = write!(out, "{k}: {v}\r\n");
                }
                out.push_str("\r\n");
                let _ = sock.write_all(out.as_bytes()).await;
                let _ = sock.write_all(&body).await;
                let _ = sock.shutdown().await;
            });
        }
    });
    port
}
