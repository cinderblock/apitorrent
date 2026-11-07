//! The MSE handshake as two sans-IO state machines, one per side.
//!
//! ```text
//! 1 A->B: Ya, PadA
//! 2 B->A: Yb, PadB
//! 3 A->B: HASH('req1', S), HASH('req2', SKEY) xor HASH('req3', S),
//!         ENCRYPT(VC, crypto_provide, len(PadC), PadC, len(IA)), ENCRYPT(IA)
//! 4 B->A: ENCRYPT(VC, crypto_select, len(PadD), PadD), ENCRYPT2(payload)
//! 5 A->B: ENCRYPT2(payload)
//! ```
//!
//! Both machines work the same way. [`feed`](Initiator::feed) takes received bytes and
//! returns how many it consumed: it never consumes past the end of the handshake, so the
//! bytes it leaves are the start of the payload stream, still encrypted if RC4 was
//! selected. [`take_output`](Initiator::take_output) returns bytes to send. Once
//! [`is_done`](Initiator::is_done), [`finish`](Initiator::finish) yields the result.
//!
//! Every read is bounded: public keys are 96 bytes, each padding is at most 512 bytes, the
//! synchronisation searches stop after 512 + 20 (receiver) or 512 + 8 (initiator) bytes, and
//! the initial payload is at most 65 535 bytes.

use std::collections::HashMap;
use std::sync::Arc;

use apitorrent_metainfo::Sha1Hash;
use rand::Rng;

use crate::crypto::{self, Ciphers, KEY_LEN, KeyPair, PRIVATE_KEY_LEN, PublicKey, SharedSecret};
use crate::error::MseError;

/// The verification constant: eight zero bytes.
pub const VC: [u8; 8] = [0; 8];

/// The most padding any of the four pads may carry.
pub const MAX_PAD: usize = 512;

/// The longest initial payload (IA): its length is a 16-bit field.
pub const MAX_INITIAL_PAYLOAD: usize = u16::MAX as usize;

/// The first 20 bytes of a plaintext BitTorrent handshake.
pub const BT_PREFIX: &[u8; 20] = b"\x13BitTorrent protocol";

/// How far past the initiator's public key the receiver looks for `HASH('req1', S)`.
pub const REQ1_SYNC_LIMIT: usize = MAX_PAD + 20;

/// How far past the receiver's public key the initiator looks for the encrypted VC.
pub const VC_SYNC_LIMIT: usize = MAX_PAD + VC.len();

/// How the payload is carried once the handshake is over.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CryptoMethod {
    /// The handshake was obfuscated, the payload is not.
    Plaintext,
    /// The payload is RC4-encrypted too.
    Rc4,
}

impl CryptoMethod {
    /// The bit for this method in `crypto_provide` and `crypto_select`.
    pub const fn bit(self) -> u32 {
        match self {
            Self::Plaintext => 0x01,
            Self::Rc4 => 0x02,
        }
    }
}

/// A set of crypto methods, as in the `crypto_provide` field.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct CryptoSet(u32);

impl CryptoSet {
    pub const PLAINTEXT: Self = Self(0x01);
    pub const RC4: Self = Self(0x02);
    pub const BOTH: Self = Self(0x03);

    /// The set as received; bits for unknown methods are kept but never selected.
    pub const fn from_bits(bits: u32) -> Self {
        Self(bits)
    }

    pub const fn bits(self) -> u32 {
        self.0
    }

    pub const fn contains(self, method: CryptoMethod) -> bool {
        self.0 & method.bit() != 0
    }
}

/// Which method to pick when the other side offers both.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum Preference {
    /// Obfuscate the handshake only, which saves the RC4 work on the payload.
    Plaintext,
    /// Encrypt the whole stream.
    #[default]
    Rc4,
}

/// Whether to use encryption, for one direction of connections.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Policy {
    /// Plain BitTorrent only. Outgoing connections don't use MSE; incoming MSE is refused.
    Disabled,
    /// Outgoing connections use MSE and offer both methods. Incoming connections may use
    /// MSE or plain BitTorrent. When both methods are offered, the preference picks one.
    Enabled(Preference),
    /// Outgoing connections offer RC4 only. Incoming plain BitTorrent, and MSE without RC4,
    /// are refused.
    Required,
}

impl Default for Policy {
    fn default() -> Self {
        Self::Enabled(Preference::default())
    }
}

impl Policy {
    /// Whether an outgoing connection starts with an MSE handshake.
    pub const fn initiates_mse(self) -> bool {
        !matches!(self, Self::Disabled)
    }

    /// Whether an outgoing connection whose MSE handshake failed may be retried as plain
    /// BitTorrent. Some peers close an MSE connection they don't understand.
    pub const fn retry_plaintext(self) -> bool {
        matches!(self, Self::Enabled(_))
    }

    /// What an outgoing MSE handshake offers.
    pub const fn provide(self) -> CryptoSet {
        match self {
            Self::Disabled => CryptoSet::PLAINTEXT,
            Self::Enabled(_) => CryptoSet::BOTH,
            Self::Required => CryptoSet::RC4,
        }
    }

    /// Whether an incoming plain BitTorrent handshake is allowed.
    pub const fn accepts_plaintext(self) -> bool {
        !matches!(self, Self::Required)
    }

    /// Whether an incoming MSE handshake is allowed.
    pub const fn accepts_mse(self) -> bool {
        !matches!(self, Self::Disabled)
    }

    /// The method to answer an incoming offer with, if any is acceptable.
    pub const fn select(self, offered: CryptoSet) -> Option<CryptoMethod> {
        let plain = offered.contains(CryptoMethod::Plaintext);
        let rc4 = offered.contains(CryptoMethod::Rc4);
        match self {
            Self::Disabled => None,
            Self::Required => {
                if rc4 {
                    Some(CryptoMethod::Rc4)
                } else {
                    None
                }
            }
            Self::Enabled(pref) => match (plain, rc4, pref) {
                (true, true, Preference::Plaintext) | (true, false, _) => {
                    Some(CryptoMethod::Plaintext)
                }
                (_, true, _) => Some(CryptoMethod::Rc4),
                (false, false, _) => None,
            },
        }
    }
}

/// One side's random material for a handshake: the private key and the paddings.
#[derive(Clone)]
pub struct LocalSecrets {
    keys: KeyPair,
    pad: Vec<u8>,
    inner_pad_len: u16,
}

impl std::fmt::Debug for LocalSecrets {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LocalSecrets")
            .field("keys", &self.keys)
            .field("pad_len", &self.pad.len())
            .field("inner_pad_len", &self.inner_pad_len)
            .finish()
    }
}

impl LocalSecrets {
    /// Fresh secrets: a random private key and random padding lengths up to 512 bytes.
    /// The outer pad (`PadA` or `PadB`) is random bytes. The inner pad (`PadC` or `PadD`)
    /// is zeros, as the spec asks, and is encrypted.
    pub fn generate<R: rand::CryptoRng + ?Sized>(rng: &mut R) -> Self {
        let keys = KeyPair::generate(rng);
        let mut pad = vec![0u8; rng.random_range(0..=MAX_PAD)];
        rng.fill_bytes(&mut pad);
        #[allow(clippy::cast_possible_truncation)]
        let inner_pad_len = rng.random_range(0..=MAX_PAD) as u16;
        Self {
            keys,
            pad,
            inner_pad_len,
        }
    }

    /// Fixed secrets, for reproducible tests. `None` when a pad is longer than 512 bytes.
    pub fn new(
        private_key: &[u8; PRIVATE_KEY_LEN],
        pad: Vec<u8>,
        inner_pad_len: u16,
    ) -> Option<Self> {
        if pad.len() > MAX_PAD || usize::from(inner_pad_len) > MAX_PAD {
            return None;
        }
        Some(Self {
            keys: KeyPair::from_private(private_key),
            pad,
            inner_pad_len,
        })
    }

    pub fn public_key(&self) -> &PublicKey {
        self.keys.public()
    }
}

/// Finds the torrent an incoming connection is for, given `HASH('req2', SKEY)`.
///
/// [`SkeyTable`] is a ready-made index; the engine can also implement this over its own
/// torrent map, with [`req2`](crate::req2) computing the key for each info-hash.
pub trait SkeyLookup {
    fn lookup(&self, req2: &[u8; 20]) -> Option<Sha1Hash>;
}

impl<T: SkeyLookup + ?Sized> SkeyLookup for &T {
    fn lookup(&self, req2: &[u8; 20]) -> Option<Sha1Hash> {
        (**self).lookup(req2)
    }
}

impl<T: SkeyLookup + ?Sized> SkeyLookup for Arc<T> {
    fn lookup(&self, req2: &[u8; 20]) -> Option<Sha1Hash> {
        (**self).lookup(req2)
    }
}

impl<H: std::hash::BuildHasher> SkeyLookup for HashMap<[u8; 20], Sha1Hash, H> {
    fn lookup(&self, req2: &[u8; 20]) -> Option<Sha1Hash> {
        self.get(req2).copied()
    }
}

/// Known info-hashes, indexed by `HASH('req2', info_hash)`.
#[derive(Debug, Clone, Default)]
pub struct SkeyTable(HashMap<[u8; 20], Sha1Hash>);

impl SkeyTable {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(&mut self, info_hash: Sha1Hash) {
        self.0.insert(crypto::req2(&info_hash), info_hash);
    }

    pub fn remove(&mut self, info_hash: &Sha1Hash) -> bool {
        self.0.remove(&crypto::req2(info_hash)).is_some()
    }

    pub fn contains(&self, info_hash: &Sha1Hash) -> bool {
        self.0.contains_key(&crypto::req2(info_hash))
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl FromIterator<Sha1Hash> for SkeyTable {
    fn from_iter<I: IntoIterator<Item = Sha1Hash>>(iter: I) -> Self {
        let mut t = Self::new();
        for h in iter {
            t.insert(h);
        }
        t
    }
}

impl SkeyLookup for SkeyTable {
    fn lookup(&self, req2: &[u8; 20]) -> Option<Sha1Hash> {
        self.0.get(req2).copied()
    }
}

/// A completed MSE handshake.
#[derive(Debug)]
pub struct Established {
    /// How the payload is carried from here on.
    pub method: CryptoMethod,
    /// The torrent the connection is for.
    pub info_hash: Sha1Hash,
    /// For the receiver: the initiator's initial payload (IA), decrypted. It is the start
    /// of the payload stream, typically the BitTorrent handshake. Empty for the initiator.
    pub initial_payload: Vec<u8>,
    /// The payload ciphers, when the method is RC4.
    pub ciphers: Option<Ciphers>,
}

/// How an incoming connection began.
#[derive(Debug)]
#[allow(clippy::large_enum_variant)] // one per connection, moved once
pub enum Incoming {
    /// A plain BitTorrent handshake. `replay` holds the bytes the receiver consumed while
    /// telling the two apart; they are the start of the plaintext stream.
    Plaintext { replay: Vec<u8> },
    /// A completed MSE handshake.
    Encrypted(Established),
}

/// Copy bytes from `input[*pos..]` into `buf` until it holds `want`. True once full.
fn fill(buf: &mut Vec<u8>, want: usize, input: &[u8], pos: &mut usize) -> bool {
    let take = want.saturating_sub(buf.len()).min(input.len() - *pos);
    buf.extend_from_slice(&input[*pos..*pos + take]);
    *pos += take;
    buf.len() >= want
}

/// Append to `buf` (up to `limit` bytes in all) and look for `marker`, which may straddle
/// earlier input. On a match, returns how many bytes of `input` the marker ended at and
/// leaves `input` positioned just past it. Errors once `limit` bytes hold no match.
fn sync(
    buf: &mut Vec<u8>,
    marker: &[u8],
    limit: usize,
    input: &[u8],
    pos: &mut usize,
) -> Result<bool, MseError> {
    let prev = buf.len();
    let take = (limit - prev).min(input.len() - *pos);
    buf.extend_from_slice(&input[*pos..*pos + take]);
    // Only windows that end in the new bytes can be new matches.
    let start = (prev + 1).saturating_sub(marker.len());
    if let Some(k) = buf[start..]
        .windows(marker.len())
        .position(|w| w == marker)
        .map(|k| k + start)
    {
        *pos += k + marker.len() - prev;
        return Ok(true);
    }
    *pos += take;
    if buf.len() >= limit {
        return Err(MseError::SyncNotFound);
    }
    Ok(false)
}

fn encode_tail(method_bits: u32, pad_len: u16, extra: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(VC.len() + 6 + usize::from(pad_len) + extra.len());
    out.extend_from_slice(&VC);
    out.extend_from_slice(&method_bits.to_be_bytes());
    out.extend_from_slice(&pad_len.to_be_bytes());
    out.resize(out.len() + usize::from(pad_len), 0);
    out.extend_from_slice(extra);
    out
}

fn read_u16(b: &[u8]) -> u16 {
    u16::from_be_bytes([b[0], b[1]])
}

fn read_u32(b: &[u8]) -> u32 {
    u32::from_be_bytes([b[0], b[1], b[2], b[3]])
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InitiatorState {
    PublicKey,
    SyncVc,
    Select,
    PadD {
        remaining: usize,
        method: CryptoMethod,
    },
    Done,
    Failed,
}

/// The side that opens the connection (A).
#[derive(Debug)]
pub struct Initiator {
    info_hash: Sha1Hash,
    provide: CryptoSet,
    secrets: LocalSecrets,
    initial_payload: Vec<u8>,
    state: InitiatorState,
    buf: Vec<u8>,
    out: Vec<u8>,
    ciphers: Option<Ciphers>,
    vc_marker: [u8; 8],
    result: Option<Established>,
}

impl Initiator {
    /// Start a handshake for `info_hash`, offering `provide`, with fresh secrets.
    /// `initial_payload` (IA, at most 65 535 bytes) rides along in the third message,
    /// always RC4-encrypted; it is usually our BitTorrent handshake.
    pub fn new<R: rand::CryptoRng + ?Sized>(
        info_hash: Sha1Hash,
        provide: CryptoSet,
        initial_payload: Vec<u8>,
        rng: &mut R,
    ) -> Result<Self, MseError> {
        Self::with_secrets(
            info_hash,
            provide,
            initial_payload,
            LocalSecrets::generate(rng),
        )
    }

    /// As [`new`](Self::new), with given secrets.
    pub fn with_secrets(
        info_hash: Sha1Hash,
        provide: CryptoSet,
        initial_payload: Vec<u8>,
        secrets: LocalSecrets,
    ) -> Result<Self, MseError> {
        if initial_payload.len() > MAX_INITIAL_PAYLOAD {
            return Err(MseError::InitialPayloadTooLong(initial_payload.len()));
        }
        if !provide.contains(CryptoMethod::Plaintext) && !provide.contains(CryptoMethod::Rc4) {
            return Err(MseError::NoCommonMethod { offered: provide });
        }
        let mut out = Vec::with_capacity(KEY_LEN + secrets.pad.len());
        out.extend_from_slice(secrets.keys.public());
        out.extend_from_slice(&secrets.pad);
        Ok(Self {
            info_hash,
            provide,
            secrets,
            initial_payload,
            state: InitiatorState::PublicKey,
            buf: Vec::with_capacity(KEY_LEN),
            out,
            ciphers: None,
            vc_marker: [0; 8],
            result: None,
        })
    }

    /// Bytes to send. Empty when there is nothing new.
    pub fn take_output(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.out)
    }

    pub fn has_output(&self) -> bool {
        !self.out.is_empty()
    }

    pub fn is_done(&self) -> bool {
        self.state == InitiatorState::Done
    }

    /// The result, once the handshake is done. Returns `None` before that, or if it was
    /// already taken.
    pub fn finish(&mut self) -> Option<Established> {
        self.result.take()
    }

    /// Consume received bytes. Returns how many were used; the rest belong to the payload.
    pub fn feed(&mut self, input: &[u8]) -> Result<usize, MseError> {
        let r = self.step(input);
        if r.is_err() {
            self.state = InitiatorState::Failed;
        }
        r
    }

    fn step(&mut self, input: &[u8]) -> Result<usize, MseError> {
        let mut pos = 0;
        loop {
            match self.state {
                InitiatorState::Failed => return Err(MseError::AlreadyFailed),
                InitiatorState::Done => return Ok(pos),
                InitiatorState::PublicKey => {
                    if !fill(&mut self.buf, KEY_LEN, input, &mut pos) {
                        return Ok(pos);
                    }
                    let theirs: PublicKey = self.buf[..].try_into().expect("96 bytes");
                    self.buf.clear();
                    let secret = self
                        .secrets
                        .keys
                        .shared_secret(&theirs)
                        .ok_or(MseError::InvalidPublicKey)?;
                    self.send_step3(&secret);
                    self.state = InitiatorState::SyncVc;
                }
                InitiatorState::SyncVc => {
                    if !sync(
                        &mut self.buf,
                        &self.vc_marker,
                        VC_SYNC_LIMIT,
                        input,
                        &mut pos,
                    )? {
                        return Ok(pos);
                    }
                    self.buf.clear();
                    // Keep the decryptor in step: it has now covered the VC.
                    let mut vc = self.vc_marker;
                    self.ciphers_mut().decrypt.apply(&mut vc);
                    self.state = InitiatorState::Select;
                }
                InitiatorState::Select => {
                    if !fill(&mut self.buf, 6, input, &mut pos) {
                        return Ok(pos);
                    }
                    let mut field = [0u8; 6];
                    field.copy_from_slice(&self.buf);
                    self.buf.clear();
                    self.ciphers_mut().decrypt.apply(&mut field);
                    let bits = read_u32(&field[..4]);
                    let method = match bits {
                        0x01 => CryptoMethod::Plaintext,
                        0x02 => CryptoMethod::Rc4,
                        _ => return Err(MseError::InvalidSelection(bits)),
                    };
                    if !self.provide.contains(method) {
                        return Err(MseError::InvalidSelection(bits));
                    }
                    let pad = usize::from(read_u16(&field[4..]));
                    if pad > MAX_PAD {
                        return Err(MseError::PaddingTooLong(pad));
                    }
                    self.state = InitiatorState::PadD {
                        remaining: pad,
                        method,
                    };
                }
                InitiatorState::PadD { remaining, method } => {
                    let take = remaining.min(input.len() - pos);
                    let mut scratch = [0u8; MAX_PAD];
                    scratch[..take].copy_from_slice(&input[pos..pos + take]);
                    self.ciphers_mut().decrypt.apply(&mut scratch[..take]);
                    pos += take;
                    if take < remaining {
                        self.state = InitiatorState::PadD {
                            remaining: remaining - take,
                            method,
                        };
                        return Ok(pos);
                    }
                    let ciphers = self.ciphers.take();
                    self.result = Some(Established {
                        method,
                        info_hash: self.info_hash,
                        initial_payload: Vec::new(),
                        ciphers: match method {
                            CryptoMethod::Rc4 => ciphers,
                            CryptoMethod::Plaintext => None,
                        },
                    });
                    self.state = InitiatorState::Done;
                }
            }
        }
    }

    fn ciphers_mut(&mut self) -> &mut Ciphers {
        self.ciphers
            .as_mut()
            .expect("ciphers exist after the key exchange")
    }

    fn send_step3(&mut self, secret: &SharedSecret) {
        let mut ciphers = Ciphers::initiator(secret, &self.info_hash);
        // The receiver's encrypted VC is its first 8 keystream bytes (VC is all zeros).
        let mut marker = VC;
        crypto::Rc4Cipher::new(&crypto::key_b(secret, &self.info_hash)).apply(&mut marker);
        self.vc_marker = marker;

        self.out.extend_from_slice(&crypto::req1(secret));
        self.out
            .extend_from_slice(&crypto::masked_req2(&self.info_hash, secret));
        let ia = std::mem::take(&mut self.initial_payload);
        #[allow(clippy::cast_possible_truncation)]
        let ia_len = (ia.len() as u16).to_be_bytes();
        let mut tail = encode_tail(self.provide.bits(), self.secrets.inner_pad_len, &ia_len);
        tail.extend_from_slice(&ia);
        ciphers.encrypt.apply(&mut tail);
        self.out.extend_from_slice(&tail);
        self.ciphers = Some(ciphers);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReceiverState {
    PublicKey,
    SyncReq1,
    Skey,
    Header,
    PadC { remaining: usize },
    IaLen,
    Ia { len: usize },
    Done,
    Failed,
}

/// The side that accepted the connection (B).
#[derive(Debug)]
pub struct Receiver<L> {
    lookup: L,
    policy: Policy,
    secrets: LocalSecrets,
    state: ReceiverState,
    buf: Vec<u8>,
    out: Vec<u8>,
    secret: Option<SharedSecret>,
    req1_marker: [u8; 20],
    info_hash: Option<Sha1Hash>,
    provide: CryptoSet,
    ciphers: Option<Ciphers>,
    result: Option<Incoming>,
}

impl<L: SkeyLookup> Receiver<L> {
    /// Wait for an incoming handshake under `policy`, finding torrents through `lookup`.
    pub fn new<R: rand::CryptoRng + ?Sized>(lookup: L, policy: Policy, rng: &mut R) -> Self {
        Self::with_secrets(lookup, policy, LocalSecrets::generate(rng))
    }

    /// As [`new`](Self::new), with given secrets.
    pub fn with_secrets(lookup: L, policy: Policy, secrets: LocalSecrets) -> Self {
        Self {
            lookup,
            policy,
            secrets,
            state: ReceiverState::PublicKey,
            buf: Vec::with_capacity(KEY_LEN),
            out: Vec::new(),
            secret: None,
            req1_marker: [0; 20],
            info_hash: None,
            provide: CryptoSet::from_bits(0),
            ciphers: None,
            result: None,
        }
    }

    /// Bytes to send. Empty when there is nothing new.
    pub fn take_output(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.out)
    }

    pub fn has_output(&self) -> bool {
        !self.out.is_empty()
    }

    pub fn is_done(&self) -> bool {
        self.state == ReceiverState::Done
    }

    /// The result, once the handshake is done. Returns `None` before that, or if it was
    /// already taken.
    pub fn finish(&mut self) -> Option<Incoming> {
        self.result.take()
    }

    /// Consume received bytes. Returns how many were used; the rest belong to the payload.
    pub fn feed(&mut self, input: &[u8]) -> Result<usize, MseError> {
        let r = self.step(input);
        if r.is_err() {
            self.state = ReceiverState::Failed;
        }
        r
    }

    fn step(&mut self, input: &[u8]) -> Result<usize, MseError> {
        let mut pos = 0;
        loop {
            match self.state {
                ReceiverState::Failed => return Err(MseError::AlreadyFailed),
                ReceiverState::Done => return Ok(pos),
                ReceiverState::PublicKey => {
                    let full = fill(&mut self.buf, KEY_LEN, input, &mut pos);
                    let n = self.buf.len().min(BT_PREFIX.len());
                    if self.buf[..n] == BT_PREFIX[..n] {
                        if n < BT_PREFIX.len() {
                            return Ok(pos);
                        }
                        if !self.policy.accepts_plaintext() {
                            return Err(MseError::PlaintextRefused);
                        }
                        self.result = Some(Incoming::Plaintext {
                            replay: std::mem::take(&mut self.buf),
                        });
                        self.state = ReceiverState::Done;
                        continue;
                    }
                    if !self.policy.accepts_mse() {
                        return Err(MseError::EncryptionRefused);
                    }
                    if !full {
                        return Ok(pos);
                    }
                    let theirs: PublicKey = self.buf[..].try_into().expect("96 bytes");
                    self.buf.clear();
                    let secret = self
                        .secrets
                        .keys
                        .shared_secret(&theirs)
                        .ok_or(MseError::InvalidPublicKey)?;
                    self.req1_marker = crypto::req1(&secret);
                    self.secret = Some(secret);
                    self.out.extend_from_slice(self.secrets.keys.public());
                    self.out.extend_from_slice(&self.secrets.pad);
                    self.state = ReceiverState::SyncReq1;
                }
                ReceiverState::SyncReq1 => {
                    if !sync(
                        &mut self.buf,
                        &self.req1_marker,
                        REQ1_SYNC_LIMIT,
                        input,
                        &mut pos,
                    )? {
                        return Ok(pos);
                    }
                    self.buf.clear();
                    self.state = ReceiverState::Skey;
                }
                ReceiverState::Skey => {
                    if !fill(&mut self.buf, 20, input, &mut pos) {
                        return Ok(pos);
                    }
                    let masked: [u8; 20] = self.buf[..].try_into().expect("20 bytes");
                    self.buf.clear();
                    let secret = self.secret.expect("secret is set after the key exchange");
                    let req2 = crypto::xor20(&masked, &crypto::req3(&secret));
                    let info_hash = self.lookup.lookup(&req2).ok_or(MseError::UnknownTorrent)?;
                    self.ciphers = Some(Ciphers::receiver(&secret, &info_hash));
                    self.info_hash = Some(info_hash);
                    self.state = ReceiverState::Header;
                }
                ReceiverState::Header => {
                    if !fill(&mut self.buf, 14, input, &mut pos) {
                        return Ok(pos);
                    }
                    let mut field = [0u8; 14];
                    field.copy_from_slice(&self.buf);
                    self.buf.clear();
                    self.ciphers_mut().decrypt.apply(&mut field);
                    if field[..8] != VC {
                        return Err(MseError::BadVerificationConstant);
                    }
                    self.provide = CryptoSet::from_bits(read_u32(&field[8..12]));
                    let pad = usize::from(read_u16(&field[12..]));
                    if pad > MAX_PAD {
                        return Err(MseError::PaddingTooLong(pad));
                    }
                    if self.policy.select(self.provide).is_none() {
                        return Err(MseError::NoCommonMethod {
                            offered: self.provide,
                        });
                    }
                    self.state = ReceiverState::PadC { remaining: pad };
                }
                ReceiverState::PadC { remaining } => {
                    let take = remaining.min(input.len() - pos);
                    let mut scratch = [0u8; MAX_PAD];
                    scratch[..take].copy_from_slice(&input[pos..pos + take]);
                    self.ciphers_mut().decrypt.apply(&mut scratch[..take]);
                    pos += take;
                    if take < remaining {
                        self.state = ReceiverState::PadC {
                            remaining: remaining - take,
                        };
                        return Ok(pos);
                    }
                    self.state = ReceiverState::IaLen;
                }
                ReceiverState::IaLen => {
                    if !fill(&mut self.buf, 2, input, &mut pos) {
                        return Ok(pos);
                    }
                    let mut field = [self.buf[0], self.buf[1]];
                    self.buf.clear();
                    self.ciphers_mut().decrypt.apply(&mut field);
                    self.state = ReceiverState::Ia {
                        len: usize::from(read_u16(&field)),
                    };
                }
                ReceiverState::Ia { len } => {
                    if !fill(&mut self.buf, len, input, &mut pos) {
                        return Ok(pos);
                    }
                    let mut ia = std::mem::take(&mut self.buf);
                    self.ciphers_mut().decrypt.apply(&mut ia);
                    self.complete(ia);
                }
            }
        }
    }

    fn ciphers_mut(&mut self) -> &mut Ciphers {
        self.ciphers
            .as_mut()
            .expect("ciphers exist after the key exchange")
    }

    fn complete(&mut self, initial_payload: Vec<u8>) {
        let method = self
            .policy
            .select(self.provide)
            .expect("checked when the header arrived");
        let mut tail = encode_tail(method.bit(), self.secrets.inner_pad_len, &[]);
        self.ciphers_mut().encrypt.apply(&mut tail);
        self.out.extend_from_slice(&tail);
        let ciphers = self.ciphers.take();
        self.result = Some(Incoming::Encrypted(Established {
            method,
            info_hash: self.info_hash.expect("set with the ciphers"),
            initial_payload,
            ciphers: match method {
                CryptoMethod::Rc4 => ciphers,
                CryptoMethod::Plaintext => None,
            },
        }));
        self.state = ReceiverState::Done;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selection_follows_policy() {
        use CryptoMethod::{Plaintext, Rc4};
        use CryptoSet as S;
        let plain_first = Policy::Enabled(Preference::Plaintext);
        let rc4_first = Policy::Enabled(Preference::Rc4);
        assert_eq!(plain_first.select(S::BOTH), Some(Plaintext));
        assert_eq!(rc4_first.select(S::BOTH), Some(Rc4));
        assert_eq!(rc4_first.select(S::PLAINTEXT), Some(Plaintext));
        assert_eq!(plain_first.select(S::RC4), Some(Rc4));
        assert_eq!(Policy::Required.select(S::BOTH), Some(Rc4));
        assert_eq!(Policy::Required.select(S::PLAINTEXT), None);
        assert_eq!(Policy::Disabled.select(S::BOTH), None);
        assert_eq!(rc4_first.select(S::from_bits(0x10)), None);
    }

    #[test]
    fn sync_finds_a_marker_split_across_inputs() {
        let marker = b"MARK";
        let mut buf = Vec::new();
        let mut pos = 0;
        assert!(!sync(&mut buf, marker, 64, b"xxxxMA", &mut pos).unwrap());
        assert_eq!(pos, 6);
        let mut pos = 0;
        assert!(sync(&mut buf, marker, 64, b"RKpayload", &mut pos).unwrap());
        assert_eq!(pos, 2);
    }

    #[test]
    fn sync_gives_up_at_the_limit() {
        let mut buf = Vec::new();
        let mut pos = 0;
        let junk = [0u8; 100];
        assert!(matches!(
            sync(&mut buf, b"MARK", 50, &junk, &mut pos),
            Err(MseError::SyncNotFound)
        ));
        assert_eq!(pos, 50);
    }
}
