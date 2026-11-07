//! `MseStream` over in-memory duplex pipes: both crypto methods, plaintext pass-through,
//! policy mismatches, timeouts, and bulk transfers with integrity checks.

use std::time::Duration;

use apitorrent_metainfo::Sha1Hash;
use apitorrent_mse::{
    CryptoMethod, Encryption, MseConfig, MseError, MseReadHalf, MseStream, MseWriteHalf, Policy,
    Preference, SkeyTable,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt, DuplexStream, duplex};

const PLAIN_FIRST: Policy = Policy::Enabled(Preference::Plaintext);
const RC4_FIRST: Policy = Policy::Enabled(Preference::Rc4);

fn hash(n: u8) -> Sha1Hash {
    Sha1Hash::digest(&[n])
}

fn table() -> SkeyTable {
    [hash(1), hash(2), hash(3)].into_iter().collect()
}

fn bt_handshake(info_hash: &Sha1Hash) -> Vec<u8> {
    let mut h = b"\x13BitTorrent protocol".to_vec();
    h.extend_from_slice(&[0; 8]);
    h.extend_from_slice(info_hash.as_bytes());
    h.extend_from_slice(b"-AT0001-000000000000");
    h
}

fn config(policy: Policy) -> MseConfig {
    MseConfig {
        policy,
        handshake_timeout: Duration::from_secs(5),
    }
}

type Pair = (
    Result<MseStream<DuplexStream>, MseError>,
    Result<MseStream<DuplexStream>, MseError>,
);

async fn pair(outbound: Policy, inbound: Policy, info_hash: Sha1Hash, ia: &[u8]) -> Pair {
    pair_with(64 * 1024, outbound, inbound, info_hash, ia).await
}

async fn pair_with(
    pipe: usize,
    outbound: Policy,
    inbound: Policy,
    info_hash: Sha1Hash,
    ia: &[u8],
) -> Pair {
    let (a, b) = duplex(pipe);
    let ia = ia.to_vec();
    let connect =
        tokio::spawn(async move { MseStream::connect(a, info_hash, &ia, config(outbound)).await });
    let accepted = MseStream::accept(b, table(), config(inbound)).await;
    (connect.await.unwrap(), accepted)
}

async fn exchange(mut a: MseStream<DuplexStream>, mut b: MseStream<DuplexStream>, len: usize) {
    let data: Vec<u8> = (0..len)
        .map(|i| u8::try_from(i * 31 % 251).unwrap())
        .collect();
    let expect = data.clone();
    let writer = tokio::spawn(async move {
        a.write_all(&data).await.unwrap();
        a.flush().await.unwrap();
        let mut back = vec![0u8; 5];
        a.read_exact(&mut back).await.unwrap();
        assert_eq!(&back, b"thank");
    });
    let mut got = vec![0u8; len];
    b.read_exact(&mut got).await.unwrap();
    assert_eq!(Sha1Hash::digest(&got), Sha1Hash::digest(&expect));
    b.write_all(b"thank").await.unwrap();
    b.flush().await.unwrap();
    writer.await.unwrap();
}

#[tokio::test]
async fn rc4_round_trip_with_initial_payload() {
    let ia = bt_handshake(&hash(2));
    let (a, b) = pair(RC4_FIRST, RC4_FIRST, hash(2), &ia).await;
    let (a, mut b) = (a.unwrap(), b.unwrap());
    assert_eq!(a.encryption(), Encryption::Mse(CryptoMethod::Rc4));
    assert_eq!(b.encryption(), Encryption::Mse(CryptoMethod::Rc4));
    assert_eq!(b.info_hash(), Some(hash(2)));
    // The initial payload comes out first, decrypted.
    let mut got = vec![0u8; ia.len()];
    b.read_exact(&mut got).await.unwrap();
    assert_eq!(got, ia);
    exchange(a, b, 1 << 20).await;
}

#[tokio::test]
async fn plaintext_method_round_trip() {
    let (a, b) = pair(PLAIN_FIRST, PLAIN_FIRST, hash(1), b"").await;
    let (a, b) = (a.unwrap(), b.unwrap());
    assert_eq!(a.encryption(), Encryption::Mse(CryptoMethod::Plaintext));
    assert_eq!(b.encryption(), Encryption::Mse(CryptoMethod::Plaintext));
    exchange(a, b, 300_000).await;
}

#[tokio::test]
async fn the_receivers_preference_wins() {
    let (a, b) = pair(PLAIN_FIRST, RC4_FIRST, hash(1), b"").await;
    assert_eq!(a.unwrap().encryption(), Encryption::Mse(CryptoMethod::Rc4));
    assert_eq!(b.unwrap().encryption(), Encryption::Mse(CryptoMethod::Rc4));

    let (a, b) = pair(RC4_FIRST, PLAIN_FIRST, hash(1), b"").await;
    assert_eq!(
        a.unwrap().encryption(),
        Encryption::Mse(CryptoMethod::Plaintext)
    );
    assert_eq!(
        b.unwrap().encryption(),
        Encryption::Mse(CryptoMethod::Plaintext)
    );
}

#[tokio::test]
async fn required_means_rc4() {
    let (a, b) = pair(Policy::Required, PLAIN_FIRST, hash(3), b"").await;
    assert_eq!(a.unwrap().encryption(), Encryption::Mse(CryptoMethod::Rc4));
    let b = b.unwrap();
    assert_eq!(b.encryption(), Encryption::Mse(CryptoMethod::Rc4));
}

#[tokio::test]
async fn plaintext_handshake_passes_through() {
    let hs = bt_handshake(&hash(1));
    let (a, b) = pair(Policy::Disabled, RC4_FIRST, hash(1), &hs).await;
    let (a, mut b) = (a.unwrap(), b.unwrap());
    assert_eq!(a.encryption(), Encryption::None);
    assert_eq!(b.encryption(), Encryption::None);
    assert_eq!(b.info_hash(), None);
    let mut got = vec![0u8; hs.len()];
    b.read_exact(&mut got).await.unwrap();
    assert_eq!(got, hs);
    exchange(a, b, 100_000).await;
}

#[tokio::test]
async fn disabled_on_both_sides_is_plain_bittorrent() {
    let hs = bt_handshake(&hash(1));
    let (a, b) = pair(Policy::Disabled, Policy::Disabled, hash(1), &hs).await;
    let (a, mut b) = (a.unwrap(), b.unwrap());
    let mut got = vec![0u8; hs.len()];
    b.read_exact(&mut got).await.unwrap();
    assert_eq!(got, hs);
    exchange(a, b, 10_000).await;
}

#[tokio::test]
async fn required_refuses_plaintext() {
    let hs = bt_handshake(&hash(1));
    let (_, b) = pair(Policy::Disabled, Policy::Required, hash(1), &hs).await;
    assert!(matches!(b, Err(MseError::PlaintextRefused)), "{b:?}");
}

#[tokio::test]
async fn disabled_refuses_mse() {
    let (a, b) = pair(RC4_FIRST, Policy::Disabled, hash(1), b"").await;
    assert!(matches!(b, Err(MseError::EncryptionRefused)), "{b:?}");
    // The receiver dropped the connection, so the initiator sees it close.
    assert!(
        matches!(a, Err(MseError::Closed | MseError::Io(_))),
        "{a:?}"
    );
}

#[tokio::test]
async fn required_receiver_refuses_a_plaintext_only_offer() {
    // An initiator that offers only plaintext (not something our policies produce, but
    // other clients can).
    let (a, b) = duplex(4096);
    let init = tokio::spawn(async move {
        let mut machine = apitorrent_mse::Initiator::new(
            hash(1),
            apitorrent_mse::CryptoSet::PLAINTEXT,
            Vec::new(),
            &mut rand::rng(),
        )
        .unwrap();
        let mut a = a;
        a.write_all(&machine.take_output()).await.unwrap();
        let mut buf = [0u8; 1024];
        loop {
            let n = a.read(&mut buf).await.unwrap_or(0);
            if n == 0 {
                return;
            }
            let _ = machine.feed(&buf[..n]);
            let out = machine.take_output();
            if !out.is_empty() && a.write_all(&out).await.is_err() {
                return;
            }
        }
    });
    let b = MseStream::accept(b, table(), config(Policy::Required)).await;
    assert!(matches!(b, Err(MseError::NoCommonMethod { .. })), "{b:?}");
    drop(b);
    init.await.unwrap();
}

#[tokio::test]
async fn unknown_torrent_is_refused() {
    let (a, b) = pair(RC4_FIRST, RC4_FIRST, hash(99), b"").await;
    assert!(matches!(b, Err(MseError::UnknownTorrent)), "{b:?}");
    assert!(a.is_err());
}

#[tokio::test]
async fn a_silent_peer_times_out() {
    let (a, _b) = duplex(4096);
    let cfg = MseConfig {
        policy: RC4_FIRST,
        handshake_timeout: Duration::from_millis(200),
    };
    let r = MseStream::connect(a, hash(1), b"", cfg).await;
    assert!(matches!(r, Err(MseError::Timeout)), "{r:?}");

    let (_a, b) = duplex(4096);
    let r = MseStream::accept(b, table(), cfg).await;
    assert!(matches!(r, Err(MseError::Timeout)), "{r:?}");
}

#[tokio::test]
async fn garbage_is_refused_without_reading_forever() {
    // 96 bytes of a "public key" and then endless junk: the receiver gives up once the
    // req1 search window is exhausted.
    let (mut a, b) = duplex(64 * 1024);
    let junk = tokio::spawn(async move {
        let block = [0x5au8; 1024];
        for _ in 0..64 {
            if a.write_all(&block).await.is_err() {
                return;
            }
        }
    });
    let r = MseStream::accept(b, table(), config(RC4_FIRST)).await;
    assert!(matches!(r, Err(MseError::SyncNotFound)), "{r:?}");
    drop(r);
    junk.await.unwrap();
}

#[tokio::test]
async fn split_halves_work_independently() {
    let (a, b) = pair(Policy::Required, RC4_FIRST, hash(1), b"").await;
    let (a, b) = (a.unwrap(), b.unwrap());
    let (io, read, write) = a.into_parts();
    let (r, w) = tokio::io::split(io);
    let mut ar = MseReadHalf::new(r, read);
    let mut aw = MseWriteHalf::new(w, write);
    let (io, read, write) = b.into_parts();
    let (r, w) = tokio::io::split(io);
    let mut br = MseReadHalf::new(r, read);
    let mut bw = MseWriteHalf::new(w, write);

    let to_b: Vec<u8> = (0..200_000u32).map(|i| (i % 253) as u8).collect();
    let to_a: Vec<u8> = (0..150_000u32).map(|i| (i % 241) as u8).collect();
    let (ta, tb) = (to_b.clone(), to_a.clone());
    let w1 = tokio::spawn(async move {
        aw.write_all(&ta).await.unwrap();
        aw.flush().await.unwrap();
    });
    let w2 = tokio::spawn(async move {
        bw.write_all(&tb).await.unwrap();
        bw.flush().await.unwrap();
    });
    let mut got_b = vec![0u8; to_b.len()];
    let mut got_a = vec![0u8; to_a.len()];
    let (r1, r2) = tokio::join!(br.read_exact(&mut got_b), ar.read_exact(&mut got_a));
    r1.unwrap();
    r2.unwrap();
    w1.await.unwrap();
    w2.await.unwrap();
    assert_eq!(Sha1Hash::digest(&got_b), Sha1Hash::digest(&to_b));
    assert_eq!(Sha1Hash::digest(&got_a), Sha1Hash::digest(&to_a));
}

#[tokio::test]
async fn many_handshakes_in_parallel() {
    let mut tasks = Vec::new();
    for i in 0..32u8 {
        tasks.push(tokio::spawn(async move {
            let policy = if i % 2 == 0 { RC4_FIRST } else { PLAIN_FIRST };
            let (a, b) = pair(policy, policy, hash(1 + i % 3), &[i; 68]).await;
            let mut b = b.unwrap();
            let mut got = [0u8; 68];
            b.read_exact(&mut got).await.unwrap();
            assert_eq!(got, [i; 68]);
            exchange(a.unwrap(), b, 4096).await;
        }));
    }
    for t in tasks {
        t.await.unwrap();
    }
}

#[tokio::test]
async fn transfers_across_the_pipe_capacity() {
    // Sizes around the 64 KiB pipe and write-chunk size, where partial writes and
    // backpressure meet the encrypted pending buffer.
    for len in [65_535usize, 65_536, 65_537, 70_000, 131_073] {
        let (a, b) = pair(Policy::Required, Policy::Required, hash(1), b"").await;
        exchange(a.unwrap(), b.unwrap(), len).await;
    }
}

#[tokio::test]
async fn a_tiny_pipe_forces_partial_reads_and_writes() {
    for policy in [Policy::Required, PLAIN_FIRST] {
        let ia = bt_handshake(&hash(3));
        let (a, b) = pair_with(13, policy, policy, hash(3), &ia).await;
        let a_state = a.as_ref().map(|_| ()).map_err(|e| format!("{e:?}"));
        let mut b =
            b.unwrap_or_else(|e| panic!("accept: {e:?} (connect: {a_state:?}) with {policy:?}"));
        let mut got = vec![0u8; ia.len()];
        b.read_exact(&mut got).await.unwrap();
        assert_eq!(got, ia);
        exchange(a.unwrap(), b, 20_000).await;
    }
}
