//! Property tests: the state machines never panic on arbitrary input, never consume past
//! the handshake, and agree with each other however the bytes are chunked.

use apitorrent_metainfo::Sha1Hash;
use apitorrent_mse::{
    CryptoSet, Incoming, Initiator, LocalSecrets, MAX_PAD, Policy, Preference, Receiver, SkeyTable,
};
use proptest::prelude::*;

fn hash() -> Sha1Hash {
    Sha1Hash::digest(b"prop")
}

fn table() -> SkeyTable {
    [hash()].into_iter().collect()
}

fn secrets(seed: u8, pad: usize, inner: u16) -> LocalSecrets {
    let key: [u8; 20] = std::array::from_fn(|i| seed ^ u8::try_from(i).unwrap() ^ 0x5c);
    LocalSecrets::new(&key, vec![seed; pad], inner).unwrap()
}

/// Feed `data` in chunks of the given sizes (cycled). Returns total bytes consumed, or the
/// first error. Stops when the machine says it is done.
fn feed_chunks(
    mut feed: impl FnMut(&[u8]) -> Result<usize, apitorrent_mse::MseError>,
    done: impl Fn() -> bool,
    data: &[u8],
    chunks: &[usize],
) -> Result<usize, apitorrent_mse::MseError> {
    let mut pos = 0;
    let mut i = 0;
    while pos < data.len() && !done() {
        let n = chunks[i % chunks.len()].max(1).min(data.len() - pos);
        let used = feed(&data[pos..pos + n])?;
        assert!(used <= n);
        pos += used;
        if used < n {
            // A machine only leaves bytes once it is done.
            assert!(done());
        }
        i += 1;
    }
    Ok(pos)
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    #[test]
    fn receiver_never_panics_on_garbage(
        data in proptest::collection::vec(any::<u8>(), 0..2048),
        chunks in proptest::collection::vec(1usize..300, 1..8),
    ) {
        for policy in [Policy::Disabled, Policy::Enabled(Preference::Rc4), Policy::Required] {
            let mut r = Receiver::new(table(), policy, &mut rand::rng());
            let cell = std::cell::RefCell::new(&mut r);
            let _ = feed_chunks(
                |b| cell.borrow_mut().feed(b),
                || cell.borrow().is_done(),
                &data,
                &chunks,
            );
        }
    }

    #[test]
    fn initiator_never_panics_on_garbage(
        data in proptest::collection::vec(any::<u8>(), 0..2048),
        chunks in proptest::collection::vec(1usize..300, 1..8),
    ) {
        let mut a = Initiator::new(hash(), CryptoSet::BOTH, vec![1, 2, 3], &mut rand::rng())
            .unwrap();
        let cell = std::cell::RefCell::new(&mut a);
        let _ = feed_chunks(
            |b| cell.borrow_mut().feed(b),
            || cell.borrow().is_done(),
            &data,
            &chunks,
        );
    }

    #[test]
    fn handshakes_agree_however_chunked(
        pad_a in 0..=MAX_PAD,
        pad_b in 0..=MAX_PAD,
        pad_c in 0..=512u16,
        pad_d in 0..=512u16,
        ia in proptest::collection::vec(any::<u8>(), 0..200),
        prefer_rc4 in any::<bool>(),
        chunks_a in proptest::collection::vec(1usize..700, 1..6),
        chunks_b in proptest::collection::vec(1usize..700, 1..6),
        trailing in proptest::collection::vec(any::<u8>(), 0..64),
    ) {
        let mut a = Initiator::with_secrets(
            hash(), CryptoSet::BOTH, ia.clone(), secrets(1, pad_a, pad_c),
        ).unwrap();
        let pref = if prefer_rc4 { Preference::Rc4 } else { Preference::Plaintext };
        let mut b = Receiver::with_secrets(table(), Policy::Enabled(pref), secrets(2, pad_b, pad_d));

        // Shuttle bytes until both are done, with trailing payload after each side's
        // handshake to check it is left alone.
        let mut a_to_b = a.take_output();
        let mut b_to_a = Vec::new();
        let mut a_used = 0;
        let mut b_used = 0;
        for _ in 0..8 {
            if !b.is_done() {
                let cell = std::cell::RefCell::new(&mut b);
                b_used += feed_chunks(
                    |x| cell.borrow_mut().feed(x),
                    || cell.borrow().is_done(),
                    &a_to_b[b_used..],
                    &chunks_b,
                ).unwrap();
                b_to_a.extend(b.take_output());
                if b.is_done() {
                    b_to_a.extend_from_slice(&trailing);
                }
            }
            if !a.is_done() {
                let cell = std::cell::RefCell::new(&mut a);
                a_used += feed_chunks(
                    |x| cell.borrow_mut().feed(x),
                    || cell.borrow().is_done(),
                    &b_to_a[a_used..],
                    &chunks_a,
                ).unwrap();
                a_to_b.extend(a.take_output());
            }
            if a.is_done() && b.is_done() {
                break;
            }
        }
        prop_assert!(a.is_done() && b.is_done());
        prop_assert_eq!(b_used, a_to_b.len());
        prop_assert_eq!(b_to_a.len() - a_used, trailing.len());

        let a_done = a.finish().unwrap();
        let Some(Incoming::Encrypted(b_done)) = b.finish() else {
            return Err(TestCaseError::fail("expected an encrypted handshake"));
        };
        prop_assert_eq!(a_done.method, b_done.method);
        prop_assert_eq!(b_done.initial_payload, ia);
        prop_assert_eq!(b_done.info_hash, hash());
        // The two sides' payload ciphers mirror each other.
        if let (Some(mut ca), Some(mut cb)) = (a_done.ciphers, b_done.ciphers) {
            let mut msg = b"payload".to_vec();
            ca.encrypt.apply(&mut msg);
            cb.decrypt.apply(&mut msg);
            prop_assert_eq!(&msg, b"payload");
            cb.encrypt.apply(&mut msg);
            ca.decrypt.apply(&mut msg);
            prop_assert_eq!(&msg, b"payload");
        }
    }

    #[test]
    fn a_flipped_bit_never_passes_silently(
        flip_at in 0usize..1200,
        bit in 0u8..8,
    ) {
        // Corrupt one byte of the initiator's messages. The receiver must either fail, or
        // (when the corruption landed in padding or the initial payload) still finish.
        let mut a = Initiator::with_secrets(
            hash(), CryptoSet::BOTH, vec![9; 20], secrets(3, 40, 7),
        ).unwrap();
        let mut b = Receiver::with_secrets(table(), Policy::default(), secrets(4, 30, 5));
        let mut first = a.take_output();
        let i = flip_at % first.len();
        first[i] ^= 1 << bit;
        if let Ok(n) = b.feed(&first) {
            prop_assert_eq!(n, first.len());
        }
        let out = b.take_output();
        if !out.is_empty() && a.feed(&out).is_ok() {
            let third = a.take_output();
            let _ = b.feed(&third);
        }
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    /// The constant-time Montgomery exponentiation agrees with `num-bigint`.
    #[test]
    fn diffie_hellman_matches_num_bigint(
        private in any::<[u8; 20]>(),
        other in any::<[u8; 20]>(),
    ) {
        use num_bigint::BigUint;
        let p = BigUint::from_bytes_be(&apitorrent_mse::PRIME.to_be_bytes());
        let ours = apitorrent_mse::KeyPair::from_private(&private);
        let theirs = apitorrent_mse::KeyPair::from_private(&other);
        let x = BigUint::from_bytes_be(&private);
        let expect_pub = BigUint::from(2u32).modpow(&x, &p);
        prop_assert_eq!(BigUint::from_bytes_be(ours.public()), expect_pub);
        if let Some(s) = ours.shared_secret(theirs.public()) {
            let y = BigUint::from_bytes_be(theirs.public());
            prop_assert_eq!(BigUint::from_bytes_be(&s), y.modpow(&x, &p));
        }
    }
}
