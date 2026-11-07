//! A software WebAuthn authenticator and client, enough to drive real ceremonies against
//! the server: ES256 credentials, `none` attestation, user verification, signature
//! counters, discoverable credentials and `excludeCredentials`.
//!
//! It takes the server's options JSON (`PublicKeyCredential*OptionsJSON`) and returns the
//! response JSON a browser library such as `@simplewebauthn/browser` would send, so the
//! tests exercise the exact wire format the web UI uses.

use data_encoding::BASE64URL_NOPAD;
use p256::ecdsa::signature::Signer;
use p256::ecdsa::{Signature, SigningKey};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

const FLAG_UP: u8 = 0x01;
const FLAG_UV: u8 = 0x04;
const FLAG_AT: u8 = 0x40;

#[derive(Debug)]
pub struct Credential {
    pub id: Vec<u8>,
    pub rp_id: String,
    pub user_handle: Vec<u8>,
    key: SigningKey,
    counter: u32,
}

#[derive(Debug, Default)]
pub struct SoftAuthenticator {
    pub credentials: Vec<Credential>,
    /// Report user verification (a PIN or biometric). Real passkeys always do.
    pub skip_user_verification: bool,
}

fn b64(data: &[u8]) -> String {
    BASE64URL_NOPAD.encode(data)
}

fn unb64(s: &str) -> Vec<u8> {
    BASE64URL_NOPAD.decode(s.as_bytes()).expect("base64url")
}

fn random<const N: usize>() -> [u8; N] {
    let mut b = [0u8; N];
    getrandom::fill(&mut b).unwrap();
    b
}

// ----- Minimal CBOR (RFC 8949) encoding: just what attestation objects need. -----

fn cbor_head(out: &mut Vec<u8>, major: u8, n: u64) {
    let m = major << 5;
    match n {
        0..=23 => out.push(m | u8::try_from(n).unwrap()),
        24..=0xff => {
            out.push(m | 0x18);
            out.push(u8::try_from(n).unwrap());
        }
        0x100..=0xffff => {
            out.push(m | 0x19);
            out.extend_from_slice(&u16::try_from(n).unwrap().to_be_bytes());
        }
        _ => {
            out.push(m | 0x1a);
            out.extend_from_slice(&u32::try_from(n).unwrap().to_be_bytes());
        }
    }
}

fn cbor_int(out: &mut Vec<u8>, v: i64) {
    if v >= 0 {
        cbor_head(out, 0, v.unsigned_abs());
    } else {
        cbor_head(out, 1, (-1 - v).unsigned_abs());
    }
}

fn cbor_bytes(out: &mut Vec<u8>, b: &[u8]) {
    cbor_head(out, 2, b.len() as u64);
    out.extend_from_slice(b);
}

fn cbor_text(out: &mut Vec<u8>, s: &str) {
    cbor_head(out, 3, s.len() as u64);
    out.extend_from_slice(s.as_bytes());
}

/// The credential public key as a `COSE_Key` (EC2, P-256, ES256).
fn cose_key(key: &SigningKey) -> Vec<u8> {
    let point = key.verifying_key().to_sec1_point(false);
    let xy = point.as_bytes();
    assert_eq!(xy.len(), 65);
    let mut out = Vec::new();
    cbor_head(&mut out, 5, 5);
    cbor_int(&mut out, 1); // kty
    cbor_int(&mut out, 2); // EC2
    cbor_int(&mut out, 3); // alg
    cbor_int(&mut out, -7); // ES256
    cbor_int(&mut out, -1); // crv
    cbor_int(&mut out, 1); // P-256
    cbor_int(&mut out, -2); // x
    cbor_bytes(&mut out, &xy[1..33]);
    cbor_int(&mut out, -3); // y
    cbor_bytes(&mut out, &xy[33..65]);
    out
}

fn client_data(kind: &str, challenge: &str, origin: &str) -> Vec<u8> {
    json!({
        "type": kind,
        "challenge": challenge,
        "origin": origin,
        "crossOrigin": false,
    })
    .to_string()
    .into_bytes()
}

impl SoftAuthenticator {
    fn flags(&self) -> u8 {
        if self.skip_user_verification {
            FLAG_UP
        } else {
            FLAG_UP | FLAG_UV
        }
    }

    /// `navigator.credentials.create()`: make a credential for `options` as the page at
    /// `origin`. Fails like a browser would if one of ours is in `excludeCredentials`.
    pub fn register(&mut self, options: &Value, origin: &str) -> Result<Value, String> {
        let rp_id = options["rp"]["id"].as_str().ok_or("no rp.id")?.to_owned();
        let challenge = options["challenge"].as_str().ok_or("no challenge")?;
        let user_handle = unb64(options["user"]["id"].as_str().ok_or("no user.id")?);
        let algs: Vec<i64> = options["pubKeyCredParams"]
            .as_array()
            .ok_or("no pubKeyCredParams")?
            .iter()
            .filter_map(|p| p["alg"].as_i64())
            .collect();
        if !algs.contains(&-7) {
            return Err("ES256 not offered".into());
        }
        if let Some(excluded) = options["excludeCredentials"].as_array() {
            for e in excluded {
                let id = unb64(e["id"].as_str().unwrap_or_default());
                if self
                    .credentials
                    .iter()
                    .any(|c| c.id == id && c.rp_id == rp_id)
                {
                    return Err("InvalidStateError: already registered".into());
                }
            }
        }
        let key = loop {
            if let Ok(k) = SigningKey::from_slice(&random::<32>()) {
                break k;
            }
        };
        let id = random::<16>().to_vec();

        let mut auth_data = Sha256::digest(rp_id.as_bytes()).to_vec();
        auth_data.push(self.flags() | FLAG_AT);
        auth_data.extend_from_slice(&0u32.to_be_bytes());
        auth_data.extend_from_slice(&[0u8; 16]); // AAGUID
        auth_data.extend_from_slice(&u16::try_from(id.len()).unwrap().to_be_bytes());
        auth_data.extend_from_slice(&id);
        auth_data.extend_from_slice(&cose_key(&key));

        let mut att = Vec::new();
        cbor_head(&mut att, 5, 3);
        cbor_text(&mut att, "fmt");
        cbor_text(&mut att, "none");
        cbor_text(&mut att, "attStmt");
        cbor_head(&mut att, 5, 0);
        cbor_text(&mut att, "authData");
        cbor_bytes(&mut att, &auth_data);

        let cdj = client_data("webauthn.create", challenge, origin);
        self.credentials.push(Credential {
            id: id.clone(),
            rp_id,
            user_handle,
            key,
            counter: 0,
        });
        Ok(json!({
            "id": b64(&id),
            "rawId": b64(&id),
            "type": "public-key",
            "response": {
                "clientDataJSON": b64(&cdj),
                "attestationObject": b64(&att),
                "transports": ["internal"],
            },
            "clientExtensionResults": {},
            "authenticatorAttachment": "platform",
        }))
    }

    /// `navigator.credentials.get()` with the `which`-th usable credential: one for this RP,
    /// in `allowCredentials` if the server sent a list.
    pub fn authenticate_with(
        &mut self,
        options: &Value,
        origin: &str,
        which: usize,
    ) -> Result<Value, String> {
        let rp_id = options["rpId"].as_str().ok_or("no rpId")?.to_owned();
        let challenge = options["challenge"].as_str().ok_or("no challenge")?;
        let allowed: Vec<Vec<u8>> = options["allowCredentials"]
            .as_array()
            .map(|a| {
                a.iter()
                    .map(|c| unb64(c["id"].as_str().unwrap_or_default()))
                    .collect()
            })
            .unwrap_or_default();
        let flags = self.flags();
        let cred = self
            .credentials
            .iter_mut()
            .filter(|c| c.rp_id == rp_id && (allowed.is_empty() || allowed.contains(&c.id)))
            .nth(which)
            .ok_or("NotAllowedError: no matching credential")?;
        cred.counter += 1;

        let mut auth_data = Sha256::digest(rp_id.as_bytes()).to_vec();
        auth_data.push(flags);
        auth_data.extend_from_slice(&cred.counter.to_be_bytes());
        let cdj = client_data("webauthn.get", challenge, origin);
        let mut signed = auth_data.clone();
        signed.extend_from_slice(&Sha256::digest(&cdj));
        let sig: Signature = cred.key.sign(&signed);
        Ok(json!({
            "id": b64(&cred.id),
            "rawId": b64(&cred.id),
            "type": "public-key",
            "response": {
                "authenticatorData": b64(&auth_data),
                "clientDataJSON": b64(&cdj),
                "signature": b64(sig.to_der().as_bytes()),
                "userHandle": b64(&cred.user_handle),
            },
            "clientExtensionResults": {},
            "authenticatorAttachment": "platform",
        }))
    }

    pub fn authenticate(&mut self, options: &Value, origin: &str) -> Result<Value, String> {
        self.authenticate_with(options, origin, 0)
    }
}
