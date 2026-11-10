//! Distribution channels between two instances in one test, over 127.0.0.1 with a
//! self-signed certificate the subscriber pins from its pairing code: the publisher serves
//! a signed feed, the subscriber verifies it against the pinned publisher key and its own
//! rule takes the entries. A fake publisher then serves a feed whose revision goes
//! backwards, one with a bad signature, and one for another channel: all are refused.
//! Nothing here skips certificate verification.

mod common;

use apitorrent_types::spaces::DEFAULT_SPACE;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use base64::Engine as _;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

use apitorrent_auth::{ClientMeta, CreateTokenRequest, PasswordMode, TokenScope};
use apitorrent_core::Core;
use apitorrent_core::automation::ChannelKey;
use apitorrent_server::{AuthMode, TlsServe, TlsServer};
use apitorrent_tls::{CertSource, CertStore};
use apitorrent_types::automation::{
    CHANNEL_FORMAT, CandidateFilter, ChannelDocument, ChannelEntry, ChannelSelector, ChannelSpec,
    CreateChannelRequest, RuleItemState, RuleSpec, SubscriptionInfo, SubscriptionSpec,
    SubscriptionTrust,
};
use apitorrent_types::{AddSource, AddTorrentRequest};
use common::{AuthFixture, PASSWORD, core, server_config};

const B64: base64::engine::GeneralPurpose = base64::engine::general_purpose::STANDARD;

/// A self-signed certificate for 127.0.0.1: (chain PEM, key PEM, pairing code).
fn self_signed() -> (String, String, String) {
    let key = rcgen::KeyPair::generate().unwrap();
    let cert = rcgen::CertificateParams::new(vec!["localhost".into(), "127.0.0.1".into()])
        .unwrap()
        .self_signed(&key)
        .unwrap();
    let info = apitorrent_tls::CertInfo::from_der(cert.der()).unwrap();
    (cert.pem(), key.serialize_pem(), info.pairing_code())
}

/// A full and a read-only API token for the fixture's admin.
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
                    name: format!("{scope:?}"),
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

/// A publisher: a core behind built-in TLS with a self-signed certificate.
async fn publisher(root: &std::path::Path) -> (Core, SocketAddr, String, String) {
    let core = core(root).await;
    let f = AuthFixture::new(root, PasswordMode::Fallback, &[]);
    let (_full, read) = tokens(&f);
    let (chain, key, code) = self_signed();
    let store = CertStore::new();
    store.install(
        apitorrent_tls::load_pem(chain.as_bytes(), key.as_bytes()).unwrap(),
        CertSource::SelfSigned,
    );
    let tls = TlsServe {
        store,
        acme: None,
        http_redirect: None,
        https_port: 0,
    };
    let server = TlsServer::bind(core.clone(), server_config(AuthMode::Enabled(f.auth)), tls)
        .await
        .unwrap();
    let addr = server.local_addr().unwrap();
    tokio::spawn(server.run(std::future::pending()));
    (core, addr, code, read)
}

fn torrent(name: &str, label: &str) -> AddTorrentRequest {
    let t = apitorrent_metainfo::TorrentBuilder::new(name, apitorrent_metainfo::Version::Hybrid)
        .piece_length(16 * 1024)
        .file(apitorrent_metainfo::FileSource::from_bytes(
            &[],
            name.as_bytes().repeat(100),
        ))
        .build()
        .unwrap();
    AddTorrentRequest {
        source: AddSource::TorrentFile {
            data: B64.encode(t),
        },
        save_path: None,
        paused: true,
        label: Some(label.into()),
        file_priorities: None,
        unwanted_policy: apitorrent_types::UnwantedPolicy::default(),
    }
}

fn take_everything() -> RuleSpec {
    RuleSpec {
        name: "mirror".into(),
        enabled: true,
        filter: CandidateFilter::default(),
        order: apitorrent_types::automation::VersionOrder {
            scheme: apitorrent_types::automation::VersionScheme::Natural,
            field: apitorrent_types::automation::VersionField::Title,
        },
        keep_latest: 0,
        label: Some("mirrored".into()),
        save_path: None,
        files: apitorrent_types::automation::FileSelection::default(),
        superseded: apitorrent_types::automation::SupersededAction::default(),
        paused: true,
        alias: None,
    }
}

fn subscription(
    url: &str,
    channel: &str,
    token: &str,
    pin: &str,
    key: Option<String>,
) -> SubscriptionSpec {
    SubscriptionSpec {
        name: "seedbox".into(),
        url: url.into(),
        channel: channel.into(),
        token: Some(token.into()),
        trust: SubscriptionTrust::Pinned { pin: pin.into() },
        publisher_key: key,
        interval_minutes: 60,
        enabled: true,
    }
}

async fn refreshed(core: &Core, id: &str) -> SubscriptionInfo {
    core.refresh_subscription(DEFAULT_SPACE, id).await.unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_subscriber_mirrors_a_pinned_publisher() {
    let pub_root = tempfile::tempdir().unwrap();
    let sub_root = tempfile::tempdir().unwrap();
    let (publisher, addr, code, read_token) = publisher(pub_root.path()).await;
    let url = format!("https://127.0.0.1:{}", addr.port());

    let first = publisher
        .add(DEFAULT_SPACE, torrent("alpha", "iso"))
        .await
        .unwrap();
    publisher
        .add(DEFAULT_SPACE, torrent("other", "misc"))
        .await
        .unwrap();
    let channel = publisher
        .create_channel(
            DEFAULT_SPACE,
            CreateChannelRequest {
                id: "isos".into(),
                channel: ChannelSpec {
                    name: "ISOs".into(),
                    description: "Linux images".into(),
                    selector: ChannelSelector::Label {
                        label: "iso".into(),
                    },
                    include_metainfo: true,
                    enabled: true,
                },
            },
        )
        .await
        .unwrap();
    assert_eq!(channel.revision, 1);
    assert_eq!(channel.entries, 1);

    let subscriber = core(sub_root.path()).await;
    let rule = subscriber
        .create_rule(DEFAULT_SPACE, take_everything())
        .await
        .unwrap();
    // Paired with the short code the publisher shows; the publisher's channel key pinned.
    let sub = subscriber
        .create_subscription(
            DEFAULT_SPACE,
            subscription(
                &url,
                "isos",
                &read_token,
                &code,
                Some(publisher.channel_public_key()),
            ),
        )
        .await
        .unwrap();
    let SubscriptionTrust::Pinned { pin } = &sub.trust else {
        panic!("{:?}", sub.trust)
    };
    assert_eq!(pin.len(), 64, "the code became the full key hash");
    assert!(
        !sub.has_token || !format!("{sub:?}").contains(&read_token),
        "never shown"
    );

    let info = refreshed(&subscriber, &sub.id).await;
    assert_eq!(info.last_error, None);
    assert_eq!(info.revision, Some(1));
    assert_eq!(info.entries, 1);
    let candidates = subscriber
        .candidates(DEFAULT_SPACE, Some(format!("channel:{}", sub.id)))
        .await
        .unwrap();
    assert_eq!(candidates.len(), 1);
    assert!(
        candidates[0].has_metainfo,
        "the .torrent came with the feed"
    );

    // The subscriber's own rule takes the entry: the same torrent, from the feed's metainfo.
    subscriber.run_rules().await;
    let r = subscriber.rule(DEFAULT_SPACE, &rule.id).await.unwrap();
    assert_eq!(r.items.len(), 1);
    assert_eq!(r.items[0].state, RuleItemState::Active);
    assert_eq!(r.items[0].torrent_id.as_deref(), Some(first.as_str()));
    let mirrored = subscriber
        .detail(&apitorrent_core::Key::new(DEFAULT_SPACE, first.clone()))
        .await
        .unwrap();
    assert_eq!(mirrored.summary.label.as_deref(), Some("mirrored"));
    assert_eq!(
        mirrored.files.len(),
        1,
        "added from metainfo, not a bare magnet"
    );

    // Nothing changed: same revision. A new entry: the revision moves on.
    assert_eq!(refreshed(&subscriber, &sub.id).await.revision, Some(1));
    let second = publisher
        .add(DEFAULT_SPACE, torrent("beta", "iso"))
        .await
        .unwrap();
    let info = refreshed(&subscriber, &sub.id).await;
    assert_eq!(
        (info.revision, info.entries, info.last_error),
        (Some(2), 2, None)
    );
    subscriber.run_rules().await;
    assert!(
        subscriber
            .detail(&apitorrent_core::Key::new(DEFAULT_SPACE, second.clone()))
            .await
            .is_ok()
    );

    // A wrong certificate pin never reaches the feed.
    let wrong_pin = subscriber
        .create_subscription(
            DEFAULT_SPACE,
            subscription(&url, "isos", &read_token, &"ab".repeat(32), None),
        )
        .await
        .unwrap();
    let info = refreshed(&subscriber, &wrong_pin.id).await;
    assert!(info.revision.is_none());
    assert!(info.last_error.is_some(), "{info:?}");
    // A pairing code that doesn't name the publisher's key is refused at once.
    assert!(
        subscriber
            .create_subscription(
                DEFAULT_SPACE,
                subscription(&url, "isos", &read_token, "AAAA-AAAA-AAAA-AAAA-AAAA", None)
            )
            .await
            .is_err()
    );
    // A publisher key other than the pinned one is refused.
    let other_key = ChannelKey::load_or_create(&sub_root.path().join("other.p8")).unwrap();
    let wrong_key = subscriber
        .create_subscription(
            DEFAULT_SPACE,
            subscription(
                &url,
                "isos",
                &read_token,
                &code,
                Some(other_key.public_base64()),
            ),
        )
        .await
        .unwrap();
    let info = refreshed(&subscriber, &wrong_key.id).await;
    assert!(
        info.last_error
            .as_deref()
            .unwrap_or_default()
            .contains("different key"),
        "{info:?}"
    );
    // Without a token the publisher refuses.
    let mut anonymous = subscription(&url, "isos", "x", &code, None);
    anonymous.token = None;
    let anonymous = subscriber
        .create_subscription(DEFAULT_SPACE, anonymous)
        .await
        .unwrap();
    let info = refreshed(&subscriber, &anonymous.id).await;
    assert!(
        info.last_error
            .as_deref()
            .unwrap_or_default()
            .contains("401"),
        "{info:?}"
    );

    // Deleting a subscription forgets its candidates; the torrents stay.
    subscriber
        .delete_subscription(DEFAULT_SPACE, &sub.id)
        .await
        .unwrap();
    assert!(
        subscriber
            .candidates(DEFAULT_SPACE, Some(format!("channel:{}", sub.id)))
            .await
            .unwrap()
            .is_empty()
    );
    assert!(
        subscriber
            .detail(&apitorrent_core::Key::new(DEFAULT_SPACE, first.clone()))
            .await
            .is_ok()
    );
}

/// A TLS server that answers every request with the current `body` as JSON.
async fn fake_publisher(chain: String, key: String) -> (SocketAddr, Arc<Mutex<Vec<u8>>>) {
    let certs: Vec<rustls::pki_types::CertificateDer<'static>> =
        rustls::pki_types::pem::PemObject::pem_slice_iter(chain.as_bytes())
            .collect::<Result<_, _>>()
            .unwrap();
    let key: rustls::pki_types::PrivateKeyDer<'static> =
        rustls::pki_types::pem::PemObject::from_pem_slice(key.as_bytes()).unwrap();
    let config = rustls::ServerConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .unwrap()
    .with_no_client_auth()
    .with_single_cert(certs, key)
    .unwrap();
    let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(config));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let body: Arc<Mutex<Vec<u8>>> = Arc::default();
    let serving = body.clone();
    tokio::spawn(async move {
        loop {
            let Ok((tcp, _)) = listener.accept().await else {
                return;
            };
            let (acceptor, body) = (acceptor.clone(), serving.clone());
            tokio::spawn(async move {
                let Ok(mut tls) = acceptor.accept(tcp).await else {
                    return;
                };
                let mut head = Vec::new();
                let mut buf = [0u8; 1024];
                while !head.windows(4).any(|w| w == b"\r\n\r\n") {
                    match tls.read(&mut buf).await {
                        Ok(0) | Err(_) => return,
                        Ok(n) => head.extend_from_slice(&buf[..n]),
                    }
                }
                let body = body.lock().unwrap().clone();
                let resp = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\
                     connection: close\r\n\r\n",
                    body.len()
                );
                let _ = tls.write_all(resp.as_bytes()).await;
                let _ = tls.write_all(&body).await;
                let _ = tls.shutdown().await;
            });
        }
    });
    (addr, body)
}

fn doc(channel: &str, revision: u64, entries: Vec<ChannelEntry>) -> ChannelDocument {
    ChannelDocument {
        format: CHANNEL_FORMAT,
        channel: channel.into(),
        name: "Fake".into(),
        description: String::new(),
        revision,
        issued_at: 1,
        entries,
    }
}

fn entry(n: u8) -> ChannelEntry {
    let hash = format!("{n:02x}").repeat(20);
    ChannelEntry {
        id: hash.clone(),
        title: format!("release {n}"),
        version: Some(format!("1.{n}")),
        published: None,
        metadata: serde_json::Map::new(),
        magnet: format!("magnet:?xt=urn:btih:{hash}"),
        torrent: None,
        size: 0,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bad_signatures_and_backward_revisions_are_refused() {
    let root = tempfile::tempdir().unwrap();
    let (chain, key_pem, code) = self_signed();
    let (addr, body) = fake_publisher(chain, key_pem).await;
    let key = ChannelKey::load_or_create(&root.path().join("publisher.p8")).unwrap();
    let serve = |feed: &apitorrent_types::automation::SignedChannel| {
        *body.lock().unwrap() = serde_json::to_vec(feed).unwrap();
    };
    let subscriber = core(root.path()).await;
    let url = format!("https://127.0.0.1:{}", addr.port());
    // Nothing pinned: the first feed's key is pinned (it came over the pinned TLS link).
    let sub = subscriber
        .create_subscription(
            DEFAULT_SPACE,
            subscription(&url, "fake", "token", &code, None),
        )
        .await
        .unwrap();

    serve(&key.sign(&doc("fake", 5, vec![entry(1)])));
    let info = refreshed(&subscriber, &sub.id).await;
    assert_eq!(
        (info.revision, info.last_error.clone()),
        (Some(5), None),
        "{info:?}"
    );
    assert_eq!(info.publisher_key, Some(key.public_base64()));

    // The revision goes backwards: refused, nothing changes.
    serve(&key.sign(&doc("fake", 4, vec![entry(1), entry(2)])));
    let info = refreshed(&subscriber, &sub.id).await;
    assert_eq!(info.revision, Some(5));
    assert!(
        info.last_error
            .as_deref()
            .unwrap_or_default()
            .contains("backwards"),
        "{info:?}"
    );

    // A tampered payload under a real signature: refused.
    let mut forged = key.sign(&doc("fake", 6, vec![entry(1)]));
    forged.payload =
        B64.encode(serde_json::to_vec(&doc("fake", 6, vec![entry(1), entry(3)])).unwrap());
    serve(&forged);
    let info = refreshed(&subscriber, &sub.id).await;
    assert_eq!(info.revision, Some(5));
    assert!(
        info.last_error
            .as_deref()
            .unwrap_or_default()
            .contains("signature"),
        "{info:?}"
    );

    // Signed by another key, even validly: refused (the first key stays pinned).
    let other = ChannelKey::load_or_create(&root.path().join("other.p8")).unwrap();
    serve(&other.sign(&doc("fake", 7, vec![entry(4)])));
    let info = refreshed(&subscriber, &sub.id).await;
    assert_eq!(info.revision, Some(5));
    assert!(
        info.last_error
            .as_deref()
            .unwrap_or_default()
            .contains("different key"),
        "{info:?}"
    );

    // A valid feed for another channel: refused.
    serve(&key.sign(&doc("elsewhere", 8, vec![entry(5)])));
    let info = refreshed(&subscriber, &sub.id).await;
    assert_eq!(info.revision, Some(5));

    // Only what the accepted feed listed became a candidate.
    let ids: Vec<String> = subscriber
        .candidates(DEFAULT_SPACE, Some(format!("channel:{}", sub.id)))
        .await
        .unwrap()
        .into_iter()
        .map(|c| c.candidate.id)
        .collect();
    assert_eq!(ids, vec![entry(1).id]);

    // A newer, properly signed revision is accepted.
    serve(&key.sign(&doc("fake", 9, vec![entry(1), entry(6)])));
    let info = refreshed(&subscriber, &sub.id).await;
    assert_eq!(
        (info.revision, info.entries, info.last_error),
        (Some(9), 2, None)
    );
    tokio::time::sleep(Duration::from_millis(10)).await;
}

/// The REST surface: rules with a dry run, candidates, channels and their feed, with the
/// scopes `authn` and the routes enforce.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_rest_api_and_its_scopes() {
    use common::{Req, send};
    use serde_json::json;
    let root = tempfile::tempdir().unwrap();
    let core = core(root.path()).await;
    let f = AuthFixture::new(root.path(), PasswordMode::Fallback, &[]);
    let (full, read) = tokens(&f);
    let app = apitorrent_server::router(core.clone(), &server_config(AuthMode::Enabled(f.auth)));

    assert_eq!(send(&app, Req::get("/api/v1/rules")).await.status, 401);
    core.submit_candidates(
        None,
        "plugin:test",
        vec![apitorrent_types::automation::ReleaseCandidate {
            id: "r1".into(),
            title: "Release 1.2".into(),
            version: Some("1.2".into()),
            published: None,
            links: apitorrent_types::automation::CandidateLinks {
                magnet: Some(format!("magnet:?xt=urn:btih:{}", "ab".repeat(20))),
                ..Default::default()
            },
            metadata: serde_json::Map::new(),
        }],
    )
    .await
    .unwrap();
    let r = send(
        &app,
        Req::get("/api/v1/candidates?source=plugin:test").bearer(&read),
    )
    .await;
    assert_eq!(r.status, 200);
    assert_eq!(r.json()[0]["candidate"]["id"], "r1");

    let rule = json!({ "name": "test", "match": { "sources": ["plugin:test"] }, "paused": true });
    let dry = send(
        &app,
        Req::post("/api/v1/rules/dry-run")
            .bearer(&full)
            .json(&json!({ "rule": rule })),
    )
    .await;
    assert_eq!(dry.status, 200, "{}", String::from_utf8_lossy(&dry.body));
    assert_eq!(dry.json()["entries"][0]["verdict"], "take");
    // A read token can look but not change.
    let r = send(&app, Req::post("/api/v1/rules").bearer(&read).json(&rule)).await;
    assert_eq!(r.status, 403);
    let bad = json!({ "name": "bad", "match": { "title": { "mode": "regex", "pattern": "(" } } });
    let r = send(&app, Req::post("/api/v1/rules").bearer(&full).json(&bad)).await;
    assert_eq!(
        (r.status.as_u16(), r.code()),
        (400, "invalid_request".into())
    );
    let r = send(&app, Req::post("/api/v1/rules").bearer(&full).json(&rule)).await;
    assert_eq!(r.status, 201, "{}", String::from_utf8_lossy(&r.body));
    let id = r.json()["id"].as_str().unwrap().to_owned();
    let r = send(&app, Req::post("/api/v1/rules/run").bearer(&full)).await;
    assert_eq!(r.status, 200);
    assert_eq!(r.json()[0]["items"][0]["state"], "active");
    let r = send(&app, Req::get(format!("/api/v1/rules/{id}")).bearer(&read)).await;
    assert_eq!(r.json()["rule"]["keepLatest"], 1);

    let channel =
        json!({ "id": "test", "name": "Test", "selector": { "kind": "rule", "ruleId": id } });
    let r = send(
        &app,
        Req::post("/api/v1/channels").bearer(&full).json(&channel),
    )
    .await;
    assert_eq!(r.status, 201, "{}", String::from_utf8_lossy(&r.body));
    assert_eq!(r.json()["entries"], 1);
    let r = send(&app, Req::get("/api/v1/channels/test/feed").bearer(&read)).await;
    assert_eq!(r.status, 200);
    let feed: apitorrent_types::automation::SignedChannel = r.typed();
    let (doc, _) =
        apitorrent_core::automation::channel::verify(&feed, Some(&core.channel_public_key()))
            .unwrap();
    assert_eq!(
        doc.entries[0].version.as_deref(),
        Some("1.2"),
        "versions carry over"
    );
    let r = send(
        &app,
        Req::post("/api/v1/channels").bearer(&full).json(&channel),
    )
    .await;
    assert_eq!(r.status, 409, "ids are unique");
    let r = send(
        &app,
        Req::get("/api/v1/channels/missing/feed").bearer(&read),
    )
    .await;
    assert_eq!(r.status, 404);
    let r = send(
        &app,
        Req::new("DELETE", format!("/api/v1/rules/{id}")).bearer(&full),
    )
    .await;
    assert_eq!(r.status, 204);
}
