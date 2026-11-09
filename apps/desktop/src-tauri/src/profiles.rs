//! Saved connection profiles: each one Remote (a daemon somewhere else) or Local (the core
//! running inside the app).
//!
//! Profiles live in `profiles.json` in the app's config directory, written atomically. API
//! tokens are never in that file: they go to the OS keychain (see [`SecretStore`]).

use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use apitorrent_client::Trust;

use crate::mapping::PathMapping;

/// The format version of `profiles.json`.
pub const FILE_VERSION: u32 = 1;

pub type ProfileId = String;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Profile {
    pub id: ProfileId,
    pub name: String,
    #[serde(flatten)]
    pub kind: ProfileKind,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "camelCase")]
pub enum ProfileKind {
    Remote(RemoteProfile),
    Local(LocalProfile),
}

/// A daemon reached over the network.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RemoteProfile {
    /// `https://…`, or `http://localhost…` (an SSH tunnel, say).
    pub base_url: String,
    pub trust: Trust,
    /// Remote prefix → local path, longest prefix first when translating.
    #[serde(default)]
    pub mappings: Vec<PathMapping>,
}

/// The core, running inside the app.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LocalProfile {
    /// Where the databases live. Defaults to a per-profile directory in the app's data.
    #[serde(default)]
    pub data_dir: Option<PathBuf>,
    /// The default save path. Defaults to `<Downloads>/apitorrent`.
    #[serde(default)]
    pub download_dir: Option<PathBuf>,
    /// The BitTorrent port (TCP and uTP).
    #[serde(default = "default_peer_port")]
    pub peer_port: u16,
    /// Also serve the web UI and API on the network, like the daemon.
    #[serde(default)]
    pub lan: Option<LanSettings>,
}

fn default_peer_port() -> u16 {
    6881
}

/// Serving the web UI and API beyond this machine. The daemon's rules apply: sign-in is
/// always required, and plain HTTP needs a trusted reverse proxy in front or an explicit
/// acceptance of the risk.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LanSettings {
    pub listen: SocketAddr,
    /// Reverse proxies whose forwarding headers are believed.
    #[serde(default)]
    pub trusted_proxies: Vec<String>,
    /// The `https://` URLs browsers use (through the proxy). Passkeys work on these.
    #[serde(default)]
    pub public_urls: Vec<String>,
    /// Serve plain HTTP on the network without a proxy.
    #[serde(default)]
    pub allow_insecure_lan: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProfileFile {
    pub version: u32,
    /// The profile the app connects with at start.
    #[serde(default)]
    pub active: Option<ProfileId>,
    #[serde(default)]
    pub profiles: Vec<Profile>,
}

#[derive(Debug, thiserror::Error)]
pub enum ProfileError {
    #[error("{0}")]
    Invalid(String),
    #[error("no profile with id {0}")]
    NotFound(String),
    #[error(
        "profiles.json is from a newer version of apitorrent (format {0}); it is left \
         untouched, and changes can't be saved until that version runs again"
    )]
    TooNew(u32),
    #[error("could not read or write the profiles: {0}")]
    Io(#[from] std::io::Error),
    #[error("profiles.json is damaged: {0}")]
    Corrupt(String),
}

impl Profile {
    /// Check a profile before saving it.
    pub fn validate(&self) -> Result<(), ProfileError> {
        if self.name.trim().is_empty() {
            return Err(ProfileError::Invalid("give the profile a name".into()));
        }
        if self.name.len() > 200 {
            return Err(ProfileError::Invalid("the name is too long".into()));
        }
        match &self.kind {
            ProfileKind::Remote(r) => {
                apitorrent_client::remote::parse_base_url(&r.base_url)
                    .map_err(|e| ProfileError::Invalid(e.to_string()))?;
                apitorrent_client::trust::verifier(&r.trust)
                    .map_err(|e| ProfileError::Invalid(e.to_string()))?;
                for m in &r.mappings {
                    m.validate().map_err(ProfileError::Invalid)?;
                }
            }
            ProfileKind::Local(l) => {
                if l.peer_port == 0 {
                    return Err(ProfileError::Invalid("choose a BitTorrent port".into()));
                }
                if let Some(lan) = &l.lan {
                    if lan.listen.ip().is_loopback() {
                        return Err(ProfileError::Invalid(
                            "the network address is a loopback address; the app's own window \
                             doesn't need one, so pick a LAN address or turn network access off"
                                .into(),
                        ));
                    }
                    if lan.trusted_proxies.is_empty() && !lan.allow_insecure_lan {
                        return Err(ProfileError::Invalid(
                            "serving on the network needs a trusted TLS reverse proxy, or \
                             accepting plain HTTP explicitly"
                                .into(),
                        ));
                    }
                    apitorrent_server::TrustedProxies::parse(&lan.trusted_proxies)
                        .map_err(ProfileError::Invalid)?;
                    for u in &lan.public_urls {
                        let url = url::Url::parse(u)
                            .map_err(|e| ProfileError::Invalid(format!("{u}: {e}")))?;
                        if url.scheme() != "https" {
                            return Err(ProfileError::Invalid(format!(
                                "{u}: public URLs must be https://"
                            )));
                        }
                    }
                }
            }
        }
        Ok(())
    }
}

/// A new random profile id.
pub fn new_id() -> ProfileId {
    let bytes: [u8; 8] = rand::random();
    bytes.iter().fold(String::with_capacity(16), |mut s, b| {
        use std::fmt::Write as _;
        let _ = write!(s, "{b:02x}");
        s
    })
}

/// `profiles.json`, loaded and saved as a whole.
#[derive(Debug)]
pub struct ProfileStore {
    path: PathBuf,
    file: ProfileFile,
    /// The file is from a newer version (this format): nothing may be written over it.
    read_only: Option<u32>,
}

impl ProfileStore {
    /// Load the profiles at `path`. A missing file is an empty store. A damaged file is
    /// moved aside (`profiles.json.damaged`) so it can be recovered, and the store starts
    /// empty; the error is returned alongside so the UI can say so.
    pub fn open(path: impl Into<PathBuf>) -> (Self, Option<ProfileError>) {
        let path = path.into();
        let empty = ProfileFile {
            version: FILE_VERSION,
            ..ProfileFile::default()
        };
        match std::fs::read(&path) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => (Self::new(path, empty), None),
            Err(e) => (Self::new(path, empty), Some(e.into())),
            Ok(bytes) => match parse(&bytes) {
                Ok(file) => (Self::new(path, file), None),
                Err(ProfileError::TooNew(v)) => {
                    // Leave a newer app's file alone; saving would downgrade it.
                    let mut store = Self::new(path, empty);
                    store.read_only = Some(v);
                    (store, Some(ProfileError::TooNew(v)))
                }
                Err(e) => {
                    let aside = path.with_extension("json.damaged");
                    let _ = std::fs::rename(&path, &aside);
                    (Self::new(path, empty), Some(e))
                }
            },
        }
    }

    fn new(path: PathBuf, file: ProfileFile) -> Self {
        Self {
            path,
            file,
            read_only: None,
        }
    }

    pub fn profiles(&self) -> &[Profile] {
        &self.file.profiles
    }

    pub fn get(&self, id: &str) -> Option<&Profile> {
        self.file.profiles.iter().find(|p| p.id == id)
    }

    pub fn active(&self) -> Option<&Profile> {
        self.file.active.as_deref().and_then(|id| self.get(id))
    }

    /// Insert or replace a profile (by id), and save.
    pub fn upsert(&mut self, profile: Profile) -> Result<(), ProfileError> {
        profile.validate()?;
        let mut next = self.file.clone();
        match next.profiles.iter_mut().find(|p| p.id == profile.id) {
            Some(slot) => *slot = profile,
            None => next.profiles.push(profile),
        }
        self.commit(next)
    }

    pub fn remove(&mut self, id: &str) -> Result<Profile, ProfileError> {
        let mut next = self.file.clone();
        let at = next
            .profiles
            .iter()
            .position(|p| p.id == id)
            .ok_or_else(|| ProfileError::NotFound(id.to_owned()))?;
        let removed = next.profiles.remove(at);
        if next.active.as_deref() == Some(id) {
            next.active = None;
        }
        self.commit(next)?;
        Ok(removed)
    }

    pub fn set_active(&mut self, id: Option<&str>) -> Result<(), ProfileError> {
        if let Some(id) = id
            && self.get(id).is_none()
        {
            return Err(ProfileError::NotFound(id.to_owned()));
        }
        let mut next = self.file.clone();
        next.active = id.map(str::to_owned);
        self.commit(next)
    }

    /// Save `next`, and only then make it current: a failed write changes nothing.
    fn commit(&mut self, next: ProfileFile) -> Result<(), ProfileError> {
        if let Some(v) = self.read_only {
            return Err(ProfileError::TooNew(v));
        }
        write_atomically(
            &self.path,
            &serde_json::to_vec_pretty(&next).expect("serialises"),
        )?;
        self.file = next;
        Ok(())
    }
}

fn parse(bytes: &[u8]) -> Result<ProfileFile, ProfileError> {
    #[derive(Deserialize)]
    struct Version {
        version: u32,
    }
    let v: Version =
        serde_json::from_slice(bytes).map_err(|e| ProfileError::Corrupt(e.to_string()))?;
    if v.version > FILE_VERSION {
        return Err(ProfileError::TooNew(v.version));
    }
    serde_json::from_slice(bytes).map_err(|e| ProfileError::Corrupt(e.to_string()))
}

/// Write to a temporary file in the same directory, flush it, then rename it over `path`.
pub(crate) fn write_atomically(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write as _;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("tmp");
    {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, path).inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })
}

/// Where secrets (API tokens) are kept: the OS keychain in the app, memory in tests.
pub trait SecretStore: Send + Sync {
    fn get(&self, key: &str) -> Result<Option<String>, String>;
    fn set(&self, key: &str, secret: &str) -> Result<(), String>;
    fn delete(&self, key: &str) -> Result<(), String>;
}

/// The keychain entry name of a profile's API token.
pub fn token_key(profile: &str) -> String {
    format!("profile:{profile}:api-token")
}

/// The OS keychain: Credential Manager on Windows, Keychain on macOS, the Secret Service on
/// Linux.
#[derive(Debug, Default)]
pub struct Keychain;

const KEYCHAIN_SERVICE: &str = "apitorrent-desktop";

impl SecretStore for Keychain {
    fn get(&self, key: &str) -> Result<Option<String>, String> {
        let entry = keyring::Entry::new(KEYCHAIN_SERVICE, key).map_err(|e| e.to_string())?;
        match entry.get_password() {
            Ok(s) => Ok(Some(s)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(e) => Err(e.to_string()),
        }
    }

    fn set(&self, key: &str, secret: &str) -> Result<(), String> {
        keyring::Entry::new(KEYCHAIN_SERVICE, key)
            .and_then(|e| e.set_password(secret))
            .map_err(|e| e.to_string())
    }

    fn delete(&self, key: &str) -> Result<(), String> {
        let entry = keyring::Entry::new(KEYCHAIN_SERVICE, key).map_err(|e| e.to_string())?;
        match entry.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(e) => Err(e.to_string()),
        }
    }
}

/// Secrets in memory, for tests.
#[cfg(test)]
#[derive(Debug, Default)]
pub struct MemorySecrets(std::sync::Mutex<std::collections::HashMap<String, String>>);

#[cfg(test)]
impl SecretStore for MemorySecrets {
    fn get(&self, key: &str) -> Result<Option<String>, String> {
        Ok(self.0.lock().expect("not poisoned").get(key).cloned())
    }
    fn set(&self, key: &str, secret: &str) -> Result<(), String> {
        self.0
            .lock()
            .expect("not poisoned")
            .insert(key.to_owned(), secret.to_owned());
        Ok(())
    }
    fn delete(&self, key: &str) -> Result<(), String> {
        self.0.lock().expect("not poisoned").remove(key);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use apitorrent_client::SpkiHash;

    fn remote(id: &str) -> Profile {
        Profile {
            id: id.into(),
            name: "Seedbox".into(),
            kind: ProfileKind::Remote(RemoteProfile {
                base_url: "https://seedbox.example".into(),
                trust: Trust::Pinned {
                    spki_sha256: SpkiHash([1; 32]),
                },
                mappings: vec![PathMapping {
                    remote: "/srv/torrents".into(),
                    local: PathBuf::from("Z:\\torrents"),
                }],
            }),
        }
    }

    fn local(id: &str) -> Profile {
        Profile {
            id: id.into(),
            name: "This PC".into(),
            kind: ProfileKind::Local(LocalProfile {
                data_dir: None,
                download_dir: None,
                peer_port: 6881,
                lan: None,
            }),
        }
    }

    #[test]
    fn profiles_round_trip_through_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("profiles.json");
        let (mut store, err) = ProfileStore::open(&path);
        assert!(err.is_none());
        assert!(store.profiles().is_empty());
        store.upsert(remote("a")).unwrap();
        store.upsert(local("b")).unwrap();
        store.set_active(Some("a")).unwrap();

        let (again, err) = ProfileStore::open(&path);
        assert!(err.is_none());
        assert_eq!(again.profiles(), store.profiles());
        assert_eq!(again.active().unwrap().id, "a");

        // Replacing keeps the order and the id.
        let mut renamed = remote("a");
        renamed.name = "Seedbox 2".into();
        store.upsert(renamed).unwrap();
        assert_eq!(store.profiles().len(), 2);
        assert_eq!(store.profiles()[0].name, "Seedbox 2");

        // Removing the active profile clears `active`.
        store.remove("a").unwrap();
        assert!(store.active().is_none());
        assert!(matches!(store.remove("a"), Err(ProfileError::NotFound(_))));
        assert!(!dir.path().join("profiles.tmp").exists());
    }

    #[test]
    fn the_file_format_is_stable() {
        let json = serde_json::to_value(remote("a")).unwrap();
        assert_eq!(
            json,
            serde_json::json!({
                "id": "a",
                "name": "Seedbox",
                "mode": "remote",
                "baseUrl": "https://seedbox.example",
                "trust": {"kind": "pinned", "spkiSha256": "01".repeat(32)},
                "mappings": [{"remote": "/srv/torrents", "local": "Z:\\torrents"}],
            })
        );
        // Optional fields may be left out of hand-written files.
        let p: Profile = serde_json::from_value(serde_json::json!({
            "id": "b", "name": "Here", "mode": "local"
        }))
        .unwrap();
        assert_eq!(p, local("b").tap_name("Here"));
    }

    trait TapName {
        fn tap_name(self, n: &str) -> Self;
    }
    impl TapName for Profile {
        fn tap_name(mut self, n: &str) -> Self {
            self.name = n.into();
            self
        }
    }

    #[test]
    fn damaged_files_are_kept_aside() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("profiles.json");
        std::fs::write(&path, b"{ not json").unwrap();
        let (store, err) = ProfileStore::open(&path);
        assert!(matches!(err, Some(ProfileError::Corrupt(_))));
        assert!(store.profiles().is_empty());
        assert_eq!(
            std::fs::read(dir.path().join("profiles.json.damaged")).unwrap(),
            b"{ not json"
        );
    }

    #[test]
    fn newer_files_are_never_overwritten() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("profiles.json");
        let newer = br#"{"version": 99, "profiles": [{"id": "x", "future": true}]}"#;
        std::fs::write(&path, newer).unwrap();
        let (mut store, err) = ProfileStore::open(&path);
        assert!(matches!(err, Some(ProfileError::TooNew(99))));
        assert!(matches!(
            store.upsert(local("b")),
            Err(ProfileError::TooNew(99))
        ));
        assert_eq!(std::fs::read(&path).unwrap(), newer);
    }

    #[test]
    fn invalid_profiles_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        let (mut store, _) = ProfileStore::open(dir.path().join("profiles.json"));
        let mut p = remote("a");
        p.name = "  ".into();
        assert!(store.upsert(p).is_err());

        let mut p = remote("a");
        if let ProfileKind::Remote(r) = &mut p.kind {
            r.base_url = "http://seedbox.example".into();
        }
        assert!(store.upsert(p).is_err(), "plain HTTP to another machine");

        let mut p = remote("a");
        if let ProfileKind::Remote(r) = &mut p.kind {
            r.trust = Trust::PrivateCa {
                ca_pem: "nope".into(),
            };
        }
        assert!(store.upsert(p).is_err(), "a CA file without certificates");

        let mut p = local("b");
        if let ProfileKind::Local(l) = &mut p.kind {
            l.lan = Some(LanSettings {
                listen: "192.168.1.10:8742".parse().unwrap(),
                trusted_proxies: vec![],
                public_urls: vec![],
                allow_insecure_lan: false,
            });
        }
        assert!(
            store.upsert(p.clone()).is_err(),
            "LAN without proxy or consent"
        );
        if let ProfileKind::Local(l) = &mut p.kind {
            l.lan.as_mut().unwrap().allow_insecure_lan = true;
        }
        store.upsert(p).unwrap();
        assert_eq!(store.profiles().len(), 1, "failed saves changed nothing");
    }

    #[test]
    fn memory_secrets() {
        let s = MemorySecrets::default();
        let k = token_key("a");
        assert_eq!(s.get(&k).unwrap(), None);
        s.set(&k, "apt_x").unwrap();
        assert_eq!(s.get(&k).unwrap().as_deref(), Some("apt_x"));
        s.delete(&k).unwrap();
        s.delete(&k).unwrap();
        assert_eq!(s.get(&k).unwrap(), None);
    }

    #[test]
    fn ids_are_random() {
        assert_ne!(new_id(), new_id());
        assert_eq!(new_id().len(), 16);
    }
}
