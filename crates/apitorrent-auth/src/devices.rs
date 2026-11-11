//! Device pairing: the desktop thin client signs in as a named, revocable device instead of
//! with a pasted API token.
//!
//! The flow is OAuth's device authorization grant (RFC 8628), adapted:
//!
//! 1. The app asks, without signing in ([`Auth::start_device_request`]). It gets a poll
//!    secret (256 bits, kept by the app) and a short user code (40 bits, shown to the
//!    person). When the app pinned the server's key, it says which; a daemon serving TLS
//!    itself refuses a pin that isn't its key, and binds the device to it.
//! 2. The person approves from a signed-in browser session that authenticated recently
//!    ([`Auth::approve_device_request`]), after comparing the code, the device's name and
//!    its address. Looking codes up is rate-limited per address like sign-in.
//! 3. The app polls ([`Auth::poll_device_request`]) and receives the credential once
//!    (`apd_` + 256 bits); only its SHA-256 is stored.
//!
//! Requests live in memory for ten minutes. A device credential works like an API token of
//! the approver (scope `read` or `full`, bound to the approver's space), can't manage
//! credentials, and is listed and revoked on the Account page. A bound device works only
//! while the daemon serves that key: re-keying the server means pairing devices again,
//! which their pinned trust would need anyway.

use std::time::Duration;

use rusqlite::{Connection, OptionalExtension, params};

use apitorrent_types::auth::{
    ApproveDeviceRequest, DeviceInfo, DevicePollResponse, DeviceRequestInfo, DeviceRequestStart,
    DeviceRequestStarted, TokenScope,
};
use apitorrent_types::spaces::SpaceId;

use crate::clock::ms;
use crate::secret::{self, Digest32};
use crate::store::{scope_str, user_by_id};
use crate::{
    Auth, AuthError, ClientMeta, Credential, Principal, Result, account_session, clean_label, limit,
};

/// The prefix of every device credential.
pub const DEVICE_PREFIX: &str = "apd_";

/// How long a pairing request waits for approval.
const REQUEST_TTL: Duration = Duration::from_secs(10 * 60);
/// How often the app should poll.
const POLL_INTERVAL_SECS: u32 = 2;
/// Pending requests beyond this are refused.
const MAX_REQUESTS: usize = 256;
/// `last_used_at` is written at most this often.
const TOUCH_EVERY: Duration = Duration::from_secs(60);
/// RFC 4648 base32: no `0`, `1` or `8`, so those can be read as `O`, `I` and `B`.
const CODE_ALPHABET: &[u8; 32] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";

#[derive(Debug, Clone)]
enum State {
    Pending,
    Approved {
        user_id: String,
        scope: TokenScope,
        space: SpaceId,
    },
    Denied,
}

#[derive(Debug, Clone)]
pub(crate) struct PendingDevice {
    poll_hash: Digest32,
    /// Normalised: eight characters of [`CODE_ALPHABET`].
    code: String,
    name: String,
    platform: String,
    server_key: Option<Digest32>,
    ip: Option<String>,
    requested_at: i64,
    expires_at: i64,
    state: State,
}

/// A user code from eight random characters.
fn new_code() -> String {
    secret::random::<8>()
        .iter()
        .map(|b| char::from(CODE_ALPHABET[usize::from(b & 31)]))
        .collect()
}

/// `ABCDEFGH` as `ABCD-EFGH`.
fn format_code(code: &str) -> String {
    format!("{}-{}", &code[..4], &code[4..])
}

/// Undo formatting and look-alikes: case, `-`, spaces, and `0`/`1`/`8` for `O`/`I`/`B`.
fn normalize_code(code: &str) -> String {
    code.chars()
        .filter(|c| !c.is_whitespace() && *c != '-')
        .map(|c| match c.to_ascii_uppercase() {
            '0' => 'O',
            '1' => 'I',
            '8' => 'B',
            c => c,
        })
        .collect()
}

/// The short code people compare for a server key; the same as the daemon prints
/// (`apitorrent_tls::pairing_code`): the first 100 bits of the SPKI SHA-256 in base32.
pub(crate) fn key_code(spki_sha256: &Digest32) -> String {
    let b32 = data_encoding::BASE32_NOPAD.encode(spki_sha256);
    b32.as_bytes()[..20]
        .chunks(4)
        .map(|c| std::str::from_utf8(c).expect("base32 is ASCII"))
        .collect::<Vec<_>>()
        .join("-")
}

fn parse_key(hex_key: &str) -> Result<Digest32> {
    let bytes = hex::decode(hex_key.trim()).map_err(|_| {
        AuthError::InvalidRequest("serverKey must be a SHA-256 in hex (64 characters)".into())
    })?;
    bytes.try_into().map_err(|_| {
        AuthError::InvalidRequest("serverKey must be a SHA-256 in hex (64 characters)".into())
    })
}

fn platform_label(platform: &str) -> Result<String> {
    let p = platform.trim();
    if p.chars().count() > 128 || p.chars().any(char::is_control) {
        return Err(AuthError::InvalidRequest(
            "the platform must be at most 128 characters without control characters".into(),
        ));
    }
    Ok(if p.is_empty() {
        "unknown".into()
    } else {
        p.to_owned()
    })
}

const DEVICE_COLS: &str = "id, name, platform, scope, space_id, created_at, last_used_at, \
                           last_ip, server_key";

fn device_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<DeviceInfo> {
    let key: Option<Vec<u8>> = r.get(8)?;
    Ok(DeviceInfo {
        id: r.get(0)?,
        name: r.get(1)?,
        platform: r.get(2)?,
        scope: if r.get::<_, String>(3)? == "full" {
            TokenScope::Full
        } else {
            TokenScope::Read
        },
        space: r.get(4)?,
        created_at: r.get(5)?,
        last_used_at: r.get(6)?,
        last_ip: r.get(7)?,
        server_key_code: key
            .and_then(|k| Digest32::try_from(k.as_slice()).ok())
            .map(|k| key_code(&k)),
        current: false,
    })
}

/// A device row, as authentication reads it.
struct AuthRow {
    id: String,
    user_id: String,
    scope: String,
    space: SpaceId,
    last_used: Option<i64>,
    bound: Option<Vec<u8>>,
}

/// Whether the device still exists (for long-lived sockets).
pub(crate) fn exists(c: &Connection, user: &str, id: &str) -> rusqlite::Result<bool> {
    c.query_row(
        "SELECT 1 FROM devices WHERE id = ?1 AND user_id = ?2",
        params![id, user],
        |_| Ok(()),
    )
    .optional()
    .map(|r| r.is_some())
}

impl Auth {
    fn purge_device_requests(&self, map: &mut Vec<PendingDevice>) {
        let now = self.now();
        map.retain(|r| r.expires_at > now);
    }

    /// An app asks to be paired. `served_key` is the SPKI SHA-256 of the certificate this
    /// daemon serves over its own TLS, if it does.
    pub fn start_device_request(
        &self,
        req: &DeviceRequestStart,
        served_key: Option<[u8; 32]>,
        meta: &ClientMeta,
    ) -> Result<DeviceRequestStarted> {
        self.limit_address(meta)?;
        let name = clean_label(&req.name, "device names")?;
        let platform = platform_label(&req.platform)?;
        let pinned = req.server_key.as_deref().map(parse_key).transpose()?;
        let server_key = match (pinned, served_key) {
            (Some(p), Some(s)) if !secret::digests_equal(&p, &s) => {
                self.log(
                    "device_request_refused",
                    None,
                    meta,
                    &format!(
                        "{name}: pinned {} but this server serves {}",
                        key_code(&p),
                        key_code(&s)
                    ),
                );
                return Err(AuthError::InvalidRequest(format!(
                    "the app pinned the key with code {}, but this server serves {}: something \
                     between the app and the server answered instead. Don't pair over this \
                     network until that is explained.",
                    key_code(&p),
                    key_code(&s)
                )));
            }
            // Bound only when this daemon can see the key it serves; behind a TLS proxy the
            // pin is the proxy's.
            (Some(p), Some(_)) => Some(p),
            _ => None,
        };
        let now = self.now();
        let poll_secret = secret::token256();
        let mut map = self.inner.device_requests.lock().expect("device lock");
        self.purge_device_requests(&mut map);
        if map.len() >= MAX_REQUESTS {
            return Err(AuthError::RateLimited(Duration::from_secs(60)));
        }
        let code = loop {
            let c = new_code();
            if !map.iter().any(|r| r.code == c) {
                break c;
            }
        };
        let expires_at = now + ms(REQUEST_TTL);
        map.push(PendingDevice {
            poll_hash: secret::sha256(poll_secret.as_bytes()),
            code: code.clone(),
            name: name.clone(),
            platform,
            server_key,
            ip: meta.ip_string(),
            requested_at: now,
            expires_at,
            state: State::Pending,
        });
        drop(map);
        self.log("device_requested", None, meta, &name);
        Ok(DeviceRequestStarted {
            poll_secret,
            user_code: format_code(&code),
            expires_at,
            interval_secs: POLL_INTERVAL_SECS,
            bound: server_key.is_some(),
        })
    }

    /// The app polls. An approved request becomes a device here, and its credential is
    /// returned this once.
    pub fn poll_device_request(
        &self,
        poll_secret: &str,
        meta: &ClientMeta,
    ) -> Result<DevicePollResponse> {
        let hash = secret::sha256(poll_secret.as_bytes());
        let now = self.now();
        let mut map = self.inner.device_requests.lock().expect("device lock");
        let Some(i) = map
            .iter()
            .position(|r| secret::digests_equal(&r.poll_hash, &hash))
        else {
            return Ok(DevicePollResponse::Expired);
        };
        if map[i].expires_at <= now {
            map.swap_remove(i);
            return Ok(DevicePollResponse::Expired);
        }
        let (user_id, scope, space) = match &map[i].state {
            State::Pending => return Ok(DevicePollResponse::Pending),
            State::Denied => {
                map.swap_remove(i);
                return Ok(DevicePollResponse::Denied);
            }
            State::Approved {
                user_id,
                scope,
                space,
            } => (user_id.clone(), *scope, *space),
        };
        let req = map.swap_remove(i);
        drop(map);

        let credential = format!("{DEVICE_PREFIX}{}", secret::token256());
        let id = secret::public_id();
        let conn = self.db();
        // The approver may have been deleted or disabled in the meantime.
        if user_by_id(&conn, &user_id)?.is_none_or(|u| u.disabled) {
            return Ok(DevicePollResponse::Denied);
        }
        conn.execute(
            "INSERT INTO devices (id, user_id, name, platform, scope, space_id, credential_hash,
                                  server_key, created_at, last_used_at, last_ip)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, NULL, NULL)",
            params![
                id,
                user_id,
                req.name,
                req.platform,
                scope_str(scope),
                space,
                secret::sha256(credential.as_bytes()).as_slice(),
                req.server_key.as_ref().map(<[u8; 32]>::as_slice),
                now,
            ],
        )?;
        self.log_with(&conn, "device_paired", Some(&user_id), meta, &req.name);
        let device = conn.query_row(
            &format!("SELECT {DEVICE_COLS} FROM devices WHERE id = ?1"),
            [&id],
            device_row,
        )?;
        Ok(DevicePollResponse::Approved {
            credential,
            device: DeviceInfo {
                current: true,
                ..device
            },
        })
    }

    /// Check a user code from a signed-in browser session: it must name a pending request.
    /// Wrong codes count against the address like wrong passwords. Returns the code,
    /// normalised.
    fn check_device_code(
        &self,
        p: &Principal,
        user_code: &str,
        meta: &ClientMeta,
    ) -> Result<String> {
        account_session(p)?;
        self.limit_address(meta)?;
        let code = normalize_code(user_code);
        let found = self.with_pending(&code, |_| ()).is_some();
        if found {
            return Ok(code);
        }
        let addr = limit::address_key(meta.ip);
        if self.inner.limiter.fail(&addr, &limit::ADDRESS) {
            self.log("lockout", Some(&p.user.id), meta, &addr);
        }
        Err(AuthError::InvalidRequest(
            "no device is waiting with that code: check it against the app, or start pairing \
             again there (codes last ten minutes)"
                .into(),
        ))
    }

    /// Run `f` on the live, still pending request with this (normalised) code.
    fn with_pending<T>(&self, code: &str, f: impl FnOnce(&mut PendingDevice) -> T) -> Option<T> {
        let now = self.now();
        let mut map = self.inner.device_requests.lock().expect("device lock");
        self.purge_device_requests(&mut map);
        map.iter_mut()
            .find(|r| r.code == code && r.expires_at > now && matches!(r.state, State::Pending))
            .map(f)
    }

    /// What a pending request is, so the person can compare it with their app.
    pub fn lookup_device_request(
        &self,
        p: &Principal,
        user_code: &str,
        meta: &ClientMeta,
    ) -> Result<DeviceRequestInfo> {
        let code = self.check_device_code(p, user_code, meta)?;
        self.with_pending(&code, |r| DeviceRequestInfo {
            user_code: format_code(&r.code),
            name: r.name.clone(),
            platform: r.platform.clone(),
            ip: r.ip.clone(),
            requested_at: r.requested_at,
            expires_at: r.expires_at,
            server_key_code: r.server_key.as_ref().map(key_code),
        })
        .ok_or(AuthError::NotFound)
    }

    /// Approve a pending request as a device of `p`, working in `space` with `scope`. Needs
    /// a recent sign-in, as creating an API token does.
    pub fn approve_device_request(
        &self,
        p: &Principal,
        space: SpaceId,
        req: &ApproveDeviceRequest,
        meta: &ClientMeta,
    ) -> Result<()> {
        self.recent_session(p)?;
        let code = self.check_device_code(p, &req.user_code, meta)?;
        let name = self
            .with_pending(&code, |r| {
                r.state = State::Approved {
                    user_id: p.user.id.clone(),
                    scope: req.scope,
                    space,
                };
                r.name.clone()
            })
            .ok_or(AuthError::NotFound)?;
        self.log(
            "device_approved",
            Some(&p.user.id),
            meta,
            &format!("{name} ({})", scope_str(req.scope)),
        );
        Ok(())
    }

    /// Refuse a pending request; the app is told so on its next poll.
    pub fn deny_device_request(
        &self,
        p: &Principal,
        user_code: &str,
        meta: &ClientMeta,
    ) -> Result<()> {
        let code = self.check_device_code(p, user_code, meta)?;
        self.with_pending(&code, |r| r.state = State::Denied);
        self.log("device_denied", Some(&p.user.id), meta, &code);
        Ok(())
    }

    /// The principal for a device credential. `served_key` is the key this daemon serves
    /// over its own TLS, if it does; a bound device works only while it is that key.
    pub fn authenticate_device(
        &self,
        credential: &str,
        served_key: Option<[u8; 32]>,
        meta: &ClientMeta,
    ) -> Result<Option<Principal>> {
        if !credential.starts_with(DEVICE_PREFIX) || credential.len() > 128 {
            return Ok(None);
        }
        let hash = secret::sha256(credential.as_bytes());
        let conn = self.db();
        let row = conn
            .query_row(
                "SELECT id, user_id, scope, space_id, last_used_at, server_key FROM devices
                 WHERE credential_hash = ?1",
                [hash.as_slice()],
                |r| {
                    Ok(AuthRow {
                        id: r.get(0)?,
                        user_id: r.get(1)?,
                        scope: r.get(2)?,
                        space: r.get(3)?,
                        last_used: r.get(4)?,
                        bound: r.get(5)?,
                    })
                },
            )
            .optional()?;
        let Some(AuthRow {
            id,
            user_id,
            scope,
            space,
            last_used,
            bound,
        }) = row
        else {
            return Ok(None);
        };
        if let Some(bound) = bound {
            let matches = served_key.is_some_and(|s| s.as_slice() == bound.as_slice());
            if !matches {
                tracing::warn!(
                    device = id,
                    "a device bound to another server key tried to sign in; it must be paired again"
                );
                return Ok(None);
            }
        }
        let now = self.now();
        if last_used.is_none_or(|t| now - t >= ms(TOUCH_EVERY)) {
            conn.execute(
                "UPDATE devices SET last_used_at = ?2, last_ip = ?3 WHERE id = ?1",
                params![id, now, meta.ip_string()],
            )?;
        }
        let Some(user) = user_by_id(&conn, &user_id)?.filter(|u| !u.disabled) else {
            return Ok(None);
        };
        Ok(Some(Principal {
            user: user.info(),
            credential: Credential::Device {
                id,
                scope: if scope == "full" {
                    TokenScope::Full
                } else {
                    TokenScope::Read
                },
                space,
            },
        }))
    }

    /// The caller's paired devices. A browser session sees all of them; a device sees only
    /// itself (the app shows how it is paired).
    pub fn list_devices(&self, p: &Principal) -> Result<Vec<DeviceInfo>> {
        let current = if let Credential::Device { id, .. } = &p.credential {
            Some(id.as_str())
        } else {
            account_session(p)?;
            None
        };
        let conn = self.db();
        let mut stmt = conn.prepare(&format!(
            "SELECT {DEVICE_COLS} FROM devices WHERE user_id = ?1 ORDER BY created_at"
        ))?;
        let all = stmt
            .query_map([&p.user.id], device_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(all
            .into_iter()
            .map(|d| DeviceInfo {
                current: current == Some(d.id.as_str()),
                ..d
            })
            .filter(|d| current.is_none() || d.current)
            .collect())
    }

    /// Remove a device: from the owner's browser session, or by the device itself (signing
    /// out of the app). Open sync sockets of the device close.
    pub fn revoke_device(&self, p: &Principal, id: &str, meta: &ClientMeta) -> Result<()> {
        match &p.credential {
            Credential::Device { id: own, .. } if own == id => {}
            Credential::Device { .. } => {
                return Err(AuthError::Forbidden("a device can only remove itself"));
            }
            _ => {
                account_session(p)?;
            }
        }
        let conn = self.db();
        let n = conn.execute(
            "DELETE FROM devices WHERE id = ?1 AND user_id = ?2",
            params![id, p.user.id],
        )?;
        if n == 0 {
            return Err(AuthError::NotFound);
        }
        self.bump_revocations();
        self.log_with(&conn, "device_revoked", Some(&p.user.id), meta, id);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_are_eight_characters_and_forgiving() {
        let c = new_code();
        assert_eq!(c.len(), 8);
        assert!(c.bytes().all(|b| CODE_ALPHABET.contains(&b)));
        assert_eq!(format_code("ABCDEFGH"), "ABCD-EFGH");
        assert_eq!(normalize_code(" abcd-0i8h "), "ABCDOIBH");
    }

    #[test]
    fn key_codes_match_the_daemons() {
        // apitorrent_tls::pairing_code of 32 zero bytes.
        assert_eq!(key_code(&[0; 32]), "AAAA-AAAA-AAAA-AAAA-AAAA");
        let k: Digest32 = core::array::from_fn(|i| u8::try_from(i).unwrap());
        assert_eq!(key_code(&k).len(), 24);
        assert!(parse_key(&hex::encode(k)).is_ok());
        assert!(parse_key("abcd").is_err());
        assert!(parse_key("zz").is_err());
    }
}
