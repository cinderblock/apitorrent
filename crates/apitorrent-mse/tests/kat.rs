//! Known-answer tests: a full handshake transcript derived from an independent Python
//! implementation of the spec (big-integer `pow`, `hashlib.sha1`, a textbook RC4 checked
//! against the classic `Key`/`Plaintext` vector), with fixed keys and paddings.
//!
//! Inputs: `Xa = 01..14`, `Xb = 21..34` (20 bytes each), `PadA = a0..a4`, `PadB = b0 b1 b2`,
//! `len(PadC) = 4`, `len(PadD) = 2`, `SKEY = SHA1("apitorrent mse kat")`, IA = a BitTorrent
//! handshake, `crypto_provide = 3`, `crypto_select = 2`. After the handshake A sends
//! `hello from A` and B sends `hello from B`, both RC4-encrypted.

use apitorrent_metainfo::Sha1Hash;
use apitorrent_mse::{
    CryptoMethod, CryptoSet, Incoming, Initiator, KeyPair, LocalSecrets, Policy, Preference,
    Receiver, SkeyTable,
};

const SKEY: &str = "95aa62f02779513ae0929dbd641d344591b1cb32";
const SECRET: &str = "cd8536d5f2c98f01f8f5a648f69f6f2d6a6aafd044666b01fc8beb81f63ddbea0a862b2b\
                      6306431f7a308b063f795d181ca711b22ab0602a9c4b5d1c53269837773ff53a4de47d9a\
                      ace6b6a1b789ea37077507842742eb7581ea180b16748826";
const YA: &str = "96e112dab29e8c5272accb9b17b26887ce54a144a4e3b697c7d159b7a817e556b0918db2b4c\
                  658e02a87f7e5fb14b18a553e084cbf3dad2d30f16596ccb982d406258c61b30c5c1dae2ddc\
                  60bdbd48d79896312aad63238c39e1a633821eb693";
const A_TO_B: &str = "96e112dab29e8c5272accb9b17b26887ce54a144a4e3b697c7d159b7a817e556b0918db2\
    b4c658e02a87f7e5fb14b18a553e084cbf3dad2d30f16596ccb982d406258c61b30c5c1dae2ddc60bdbd48d798\
    96312aad63238c39e1a633821eb693a0a1a2a3a4361e3668027ecec9d1086d5f839d191781e8de17049944a4a3\
    7875d0f9bfd798207bf5c8704ddf5b947065da5ae73cbf6f8d7e454065561a02e400e787219cbece8b47d7f545\
    9eb28712245f9113ecc730dce203ce731fbcbea36637070017898d3f61293e99822f1df653bbd0c893434e302b\
    36cdf8b18539e951996709b3f49bd5bb9249b5c5817679d872";
const B_TO_A: &str = "8f9c9f400fe9b3258f3e48598a95c7805cc90c995cd770283322679d132ebdae09b75eea\
    dc01de698ef86945cf38314a95fad08c2ad5641802bd5f658eb3ea0db2712c04aa5efed03deecc5104f75d869d\
    2d197e4336c61f0d71d763bee99416b0b1b2ab4f30ea76cf7c78b601dada5a6e949af02f8ceee9af595dba1c8a\
    cd";

fn unhex(s: &str) -> Vec<u8> {
    hex::decode(s).expect("valid hex")
}

fn skey() -> Sha1Hash {
    Sha1Hash::from_slice(&unhex(SKEY)).unwrap()
}

fn ia() -> Vec<u8> {
    let mut ia = b"\x13BitTorrent protocol".to_vec();
    ia.extend_from_slice(&[0; 8]);
    ia.extend_from_slice(skey().as_bytes());
    ia.extend_from_slice(b"-AT0001-123456789012");
    ia
}

fn seq(start: u8, len: u8) -> [u8; 20] {
    assert_eq!(len, 20);
    std::array::from_fn(|i| start + u8::try_from(i).unwrap())
}

fn initiator() -> Initiator {
    let secrets = LocalSecrets::new(&seq(0x01, 20), (0xa0..0xa5).collect(), 4).unwrap();
    Initiator::with_secrets(skey(), CryptoSet::BOTH, ia(), secrets).unwrap()
}

fn receiver() -> Receiver<SkeyTable> {
    let secrets = LocalSecrets::new(&seq(0x21, 20), vec![0xb0, 0xb1, 0xb2], 2).unwrap();
    let table: SkeyTable = [skey()].into_iter().collect();
    Receiver::with_secrets(table, Policy::Enabled(Preference::Rc4), secrets)
}

#[test]
fn shared_secret_matches_the_reference() {
    let a = KeyPair::from_private(&seq(0x01, 20));
    let b = KeyPair::from_private(&seq(0x21, 20));
    assert_eq!(hex::encode(a.public()), YA);
    assert_eq!(hex::encode(a.shared_secret(b.public()).unwrap()), SECRET);
}

#[test]
fn initiator_sends_the_reference_bytes() {
    let mut a = initiator();
    let mut sent = a.take_output();
    let b_to_a = unhex(B_TO_A);
    let used = a.feed(&b_to_a).unwrap();
    assert!(a.is_done());
    sent.extend(a.take_output());

    let a_to_b = unhex(A_TO_B);
    let handshake_len = a_to_b.len() - b"hello from A".len();
    assert_eq!(hex::encode(&sent), hex::encode(&a_to_b[..handshake_len]));

    let done = a.finish().unwrap();
    assert_eq!(done.method, CryptoMethod::Rc4);
    let mut ciphers = done.ciphers.unwrap();
    // What's left of B's bytes is its encrypted payload.
    let mut payload = b_to_a[used..].to_vec();
    ciphers.decrypt.apply(&mut payload);
    assert_eq!(payload, b"hello from B");
    let mut ours = b"hello from A".to_vec();
    ciphers.encrypt.apply(&mut ours);
    assert_eq!(ours, a_to_b[handshake_len..]);
}

#[test]
fn receiver_sends_the_reference_bytes() {
    let mut b = receiver();
    let a_to_b = unhex(A_TO_B);
    let used = b.feed(&a_to_b).unwrap();
    assert!(b.is_done());
    let sent = b.take_output();
    let b_to_a = unhex(B_TO_A);
    let handshake_len = b_to_a.len() - b"hello from B".len();
    assert_eq!(hex::encode(&sent), hex::encode(&b_to_a[..handshake_len]));

    let Some(Incoming::Encrypted(done)) = b.finish() else {
        panic!("expected an encrypted handshake");
    };
    assert_eq!(done.info_hash, skey());
    assert_eq!(done.initial_payload, ia());
    let mut ciphers = done.ciphers.unwrap();
    let mut payload = a_to_b[used..].to_vec();
    ciphers.decrypt.apply(&mut payload);
    assert_eq!(payload, b"hello from A");
}

#[test]
fn byte_at_a_time_gives_the_same_result() {
    let mut b = receiver();
    let a_to_b = unhex(A_TO_B);
    let mut used = 0;
    for (i, byte) in a_to_b.iter().enumerate() {
        if b.is_done() {
            break;
        }
        assert_eq!(b.feed(std::slice::from_ref(byte)).unwrap(), 1, "byte {i}");
        used += 1;
    }
    assert_eq!(used, a_to_b.len() - b"hello from A".len());
    let out = b.take_output();
    let mut a = initiator();
    a.take_output();
    for byte in &out {
        assert!(!a.is_done());
        assert_eq!(a.feed(std::slice::from_ref(byte)).unwrap(), 1);
    }
    assert!(a.is_done());
    assert_eq!(hex::encode(&out), &B_TO_A[..out.len() * 2]);
}
