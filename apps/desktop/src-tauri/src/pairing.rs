//! Pairing this app as a device of a daemon (see `apitorrent_auth::devices`), instead of
//! pasting an API token.
//!
//! The app asks the server (without signing in), shows the short code it gets, and offers
//! to open the server's `/pair` page in the browser, where a signed-in session approves it.
//! The app polls; once approved, the credential arrives here, in Rust. It never reaches the
//! page: it waits in memory until the profile is saved, then goes to the keychain like a
//! token. The poll secret never leaves Rust either.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use apitorrent_client::{Remote, Trust};
use apitorrent_types::auth::{
    DeviceInfo, DevicePollRequest, DevicePollResponse, DeviceRequestStart, DeviceRequestStarted,
};

/// What a profile records about the device it is paired as (not secret: the credential is
/// in the keychain).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PairedDevice {
    /// The device's id on the server, for signing out.
    pub id: String,
    pub name: String,
}

/// A pairing in progress or done, by an id the page holds.
struct Pairing {
    remote: Remote,
    approve_url: String,
    poll_secret: String,
    started: Instant,
    /// Set once approved: the credential and the device it is.
    done: Option<(String, PairedDevice)>,
}

/// Pairings are dropped this long after they start, collected or not.
const KEEP: Duration = Duration::from_secs(20 * 60);

#[derive(Default)]
pub struct Pairings(Mutex<HashMap<String, Pairing>>);

/// What the page is told when pairing starts.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PairingStarted {
    pub pairing_id: String,
    /// The code to compare in the browser (`ABCD-EFGH`).
    pub user_code: String,
    pub expires_at: i64,
    pub interval_secs: u32,
    /// The device will be bound to the server's key (pinned trust).
    pub bound: bool,
    /// The server's page to approve it on.
    pub approve_url: String,
}

/// Where a pairing stands, for the page.
#[derive(Debug, Clone, Serialize)]
#[serde(
    tag = "status",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum PairingStatus {
    Pending,
    /// Approved; saving the profile stores the credential.
    Approved {
        device_name: String,
    },
    Denied,
    Expired,
}

/// `<base>/pair#code=…`: the server's own page, in its own origin, with the code to look up.
pub fn approve_url(base: &url::Url, user_code: &str) -> String {
    let mut u = base.join("pair").expect("`pair` is a relative URL");
    u.set_fragment(Some(&format!("code={user_code}")));
    u.to_string()
}

/// "apitorrent 0.3.0 on windows".
pub fn platform() -> String {
    format!(
        "apitorrent {} on {}",
        env!("CARGO_PKG_VERSION"),
        std::env::consts::OS
    )
}

/// This computer's name, as a default device name.
pub fn default_device_name() -> String {
    ["COMPUTERNAME", "HOSTNAME"]
        .iter()
        .find_map(|v| std::env::var(v).ok().filter(|n| !n.trim().is_empty()))
        .unwrap_or_else(|| "This computer".to_owned())
}

/// The key the app pinned, for the server to check and bind to.
pub fn pinned_key(trust: &Trust) -> Option<String> {
    match trust {
        Trust::Pinned { spki_sha256 } => Some(spki_sha256.to_hex()),
        _ => None,
    }
}

fn error_message(status: u16, body: &[u8]) -> String {
    serde_json::from_slice::<serde_json::Value>(body)
        .ok()
        .and_then(|v| v.get("message").and_then(|m| m.as_str()).map(str::to_owned))
        .unwrap_or_else(|| format!("the server answered {status}"))
}

async fn post<T: serde::de::DeserializeOwned>(
    remote: &Remote,
    path: &str,
    body: &impl Serialize,
) -> Result<T, String> {
    let body = serde_json::to_vec(body).map_err(|e| e.to_string())?;
    let r = remote
        .request("POST", path, Some(body), Some("application/json"))
        .await
        .map_err(|e| e.to_string())?;
    match r.status {
        200 | 201 => serde_json::from_slice(&r.body).map_err(|e| e.to_string()),
        404 => Err(
            "this server doesn't offer device pairing (it may be older): paste an API token instead"
                .into(),
        ),
        s => Err(error_message(s, &r.body)),
    }
}

impl Pairings {
    fn prune(map: &mut HashMap<String, Pairing>) {
        map.retain(|_, p| p.started.elapsed() < KEEP);
    }

    /// Ask `base_url` (trusted as `trust`) to pair this app as `name`.
    pub async fn start(
        &self,
        base_url: &str,
        trust: &Trust,
        name: &str,
    ) -> Result<PairingStarted, String> {
        let remote = Remote::new(base_url, trust, None).map_err(|e| e.to_string())?;
        let started: DeviceRequestStarted = post(
            &remote,
            "/api/v1/auth/devices/requests",
            &DeviceRequestStart {
                name: name.trim().to_owned(),
                platform: platform(),
                server_key: pinned_key(trust),
            },
        )
        .await?;
        let id = crate::profiles::new_id();
        let view = PairingStarted {
            pairing_id: id.clone(),
            approve_url: approve_url(remote.base_url(), &started.user_code),
            user_code: started.user_code,
            expires_at: started.expires_at,
            interval_secs: started.interval_secs.max(1),
            bound: started.bound,
        };
        let mut map = self.0.lock().expect("not poisoned");
        Self::prune(&mut map);
        map.insert(
            id,
            Pairing {
                remote,
                approve_url: view.approve_url.clone(),
                poll_secret: started.poll_secret,
                started: Instant::now(),
                done: None,
            },
        );
        Ok(view)
    }

    /// Ask the server whether the pairing was approved.
    pub async fn poll(&self, id: &str) -> Result<PairingStatus, String> {
        let (remote, secret) = {
            let map = self.0.lock().expect("not poisoned");
            let p = map.get(id).ok_or("this pairing is over; start again")?;
            if let Some((_, d)) = &p.done {
                return Ok(PairingStatus::Approved {
                    device_name: d.name.clone(),
                });
            }
            (p.remote.clone(), p.poll_secret.clone())
        };
        let r: DevicePollResponse = post(
            &remote,
            "/api/v1/auth/devices/poll",
            &DevicePollRequest {
                poll_secret: secret,
            },
        )
        .await?;
        let mut map = self.0.lock().expect("not poisoned");
        Ok(match r {
            DevicePollResponse::Pending => PairingStatus::Pending,
            DevicePollResponse::Approved { credential, device } => {
                let DeviceInfo {
                    id: dev_id, name, ..
                } = device;
                if let Some(p) = map.get_mut(id) {
                    p.done = Some((
                        credential,
                        PairedDevice {
                            id: dev_id,
                            name: name.clone(),
                        },
                    ));
                }
                PairingStatus::Approved { device_name: name }
            }
            DevicePollResponse::Denied => {
                map.remove(id);
                PairingStatus::Denied
            }
            DevicePollResponse::Expired => {
                map.remove(id);
                PairingStatus::Expired
            }
        })
    }

    /// The approved credential, taken (for saving it with a profile), checked against the
    /// server URL the profile is saved with.
    pub fn take(&self, id: &str, base_url: &str) -> Result<(String, PairedDevice), String> {
        let mut map = self.0.lock().expect("not poisoned");
        let same = map.get(id).is_some_and(|p| {
            apitorrent_client::remote::parse_base_url(base_url)
                .is_ok_and(|u| &u == p.remote.base_url())
        });
        if !same {
            return Err(
                "the pairing was for another server URL; pair again after changing the URL".into(),
            );
        }
        match map.remove(id) {
            Some(Pairing {
                done: Some(done), ..
            }) => Ok(done),
            Some(p) => {
                map.insert(id.to_owned(), p);
                Err("the device isn't approved yet".into())
            }
            None => Err("this pairing is over; start again".into()),
        }
    }

    /// The server's approval page for a pairing, while it lasts.
    pub fn approve_url(&self, id: &str) -> Option<String> {
        self.0
            .lock()
            .expect("not poisoned")
            .get(id)
            .map(|p| p.approve_url.clone())
    }

    /// Give up on a pairing (the editor closed).
    pub fn cancel(&self, id: &str) {
        self.0.lock().expect("not poisoned").remove(id);
    }
}

/// Sign a device out on its server (best effort, when its profile is removed or re-paired).
pub async fn sign_out(base_url: &str, trust: &Trust, credential: &str, device_id: &str) {
    let Ok(remote) = Remote::new(
        base_url,
        trust,
        Some(apitorrent_client::Token::new(credential)),
    ) else {
        return;
    };
    let path = format!("/api/v1/auth/devices/{device_id}");
    let call = remote.request("DELETE", &path, None, None);
    match tokio::time::timeout(Duration::from_secs(10), call).await {
        Ok(Ok(r)) if r.status == 204 || r.status == 404 || r.status == 401 => {}
        Ok(Ok(r)) => tracing::warn!(status = r.status, "the server didn't remove the device"),
        Ok(Err(e)) => tracing::warn!(error = %e, "couldn't reach the server to remove the device"),
        Err(_) => tracing::warn!("removing the device on the server timed out"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_approval_page_is_on_the_servers_origin() {
        let base = apitorrent_client::remote::parse_base_url("https://nas.lan:8742").unwrap();
        assert_eq!(
            approve_url(&base, "ABCD-EFGH"),
            "https://nas.lan:8742/pair#code=ABCD-EFGH"
        );
        let prefixed =
            apitorrent_client::remote::parse_base_url("https://example.com/torrents").unwrap();
        assert_eq!(
            approve_url(&prefixed, "ABCD-EFGH"),
            "https://example.com/torrents/pair#code=ABCD-EFGH"
        );
    }

    #[test]
    fn only_pinned_trust_sends_a_key() {
        assert_eq!(pinned_key(&Trust::Os), None);
        let hash = apitorrent_client::SpkiHash::parse(&"a".repeat(64)).unwrap();
        assert_eq!(
            pinned_key(&Trust::Pinned { spki_sha256: hash }),
            Some("a".repeat(64))
        );
        assert!(platform().starts_with("apitorrent "));
        assert!(!default_device_name().is_empty());
    }

    #[test]
    fn server_errors_are_shown_by_their_message() {
        assert_eq!(
            error_message(400, br#"{"code":"invalid_request","message":"nope"}"#),
            "nope"
        );
        assert_eq!(error_message(502, b"<html>"), "the server answered 502");
    }
}
