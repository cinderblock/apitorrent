//! A session against a hand-driven peer speaking raw wire messages, for protocol details
//! a session-to-session transfer can't pin down.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use apitorrent_engine::bitfield::Bitfield;
use apitorrent_engine::identity::{ClientProfile, IdentityRules};
use apitorrent_engine::{AddTorrent, Session, SessionConfig, TorrentHandle};
use apitorrent_metainfo::{FileSource, Torrent, TorrentBuilder, Version};
use apitorrent_wire::extended::{
    self, ExtensionHandshake, HolepunchError, HolepunchMessage, PexFlags, PexMessage, names,
};
use apitorrent_wire::{BlockRef, Decoder, HANDSHAKE_LEN, Handshake, Message, PeerId, Reserved};
use bytes::{Bytes, BytesMut};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

const PIECE: u32 = 64 * 1024;
const BLOCK: u32 = 16 * 1024;

fn data(len: usize) -> Vec<u8> {
    let mut x: u64 = 0x2545_f491_4f6c_dd1d;
    let mut out = Vec::with_capacity(len + 8);
    while out.len() < len {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        out.extend_from_slice(&x.to_le_bytes());
    }
    out.truncate(len);
    out
}

/// How the session under test is set up.
struct Setup {
    version: Version,
    private: bool,
    /// Has every piece (a seed), or none.
    seed: bool,
    utp: bool,
    identity: Option<Arc<ClientProfile>>,
}

impl Default for Setup {
    fn default() -> Self {
        Self {
            version: Version::V1,
            private: false,
            seed: true,
            utp: false,
            identity: None,
        }
    }
}

struct Under {
    session: Session,
    handle: TorrentHandle,
    torrent: Arc<Torrent>,
    bytes: Vec<u8>,
}

/// A session with one torrent of one file of `len` bytes in 64 KiB pieces.
async fn start(dir: &Path, len: usize, setup: Setup) -> Under {
    let bytes = data(len);
    if setup.seed {
        std::fs::write(dir.join("f.bin"), &bytes).unwrap();
    }
    let built = TorrentBuilder::new("f.bin", setup.version)
        .piece_length(u64::from(PIECE))
        .private(setup.private)
        .file(FileSource::from_bytes(&[], bytes.clone()))
        .build()
        .unwrap();
    let torrent = Arc::new(Torrent::from_bytes(&built).unwrap());
    let session = Session::start(SessionConfig {
        listen: "127.0.0.1:0".parse().unwrap(),
        utp: setup.utp,
        prefer_utp: setup.utp,
        utp_connect_timeout: Duration::from_secs(1),
        encryption: apitorrent_mse::Policy::Disabled,
        ..SessionConfig::default()
    })
    .await
    .unwrap();
    session.set_identity_rules(IdentityRules {
        default: setup.identity,
        rules: Vec::new(),
    });
    let pieces = u32::try_from(torrent.info.piece_count()).unwrap();
    let handle = session
        .add_torrent(AddTorrent {
            have: (!setup.seed).then(|| Bitfield::new(pieces)),
            ..AddTorrent::new(torrent.clone(), dir.to_path_buf())
        })
        .unwrap();
    if setup.seed {
        handle.wait_finished().await;
    }
    Under {
        session,
        handle,
        torrent,
        bytes,
    }
}

/// An extension handshake offering `ut_holepunch` and `ut_pex` on our ids, with a listen
/// port.
fn holepunch_ext(port: u16) -> ExtensionHandshake {
    let mut ext = ExtensionHandshake {
        listen_port: Some(port),
        ..Default::default()
    };
    ext.extensions.insert(names::UT_HOLEPUNCH.into(), HP);
    ext.extensions.insert(names::UT_PEX.into(), 1);
    ext
}

/// Our id for `ut_holepunch` messages, in [`holepunch_ext`].
const HP: u8 = 9;

/// Our end of a connection, driven by hand.
struct RawPeer {
    stream: TcpStream,
    decoder: Decoder,
    /// The session's BitTorrent handshake.
    handshake: Handshake,
    /// The session's extension handshake, once it came.
    theirs: Option<ExtensionHandshake>,
}

impl RawPeer {
    /// Connect with the Fast and extension bits set and send our extension handshake.
    /// `n` tells the peer ids of several raw peers apart.
    async fn connect(
        addr: std::net::SocketAddr,
        torrent: &Torrent,
        ext: ExtensionHandshake,
        n: u8,
    ) -> Self {
        let mut stream = TcpStream::connect(addr).await.unwrap();
        let ours = Handshake {
            reserved: Reserved::default()
                .with_fast(true)
                .with_extension_protocol(true),
            info_hash: torrent.info_hashes().get_v1().unwrap(),
            peer_id: {
                let mut id = *b"-XX0000-rawpeer00000";
                id[19] = b'0' + n;
                PeerId(id)
            },
        };
        stream.write_all(&ours.encode()).await.unwrap();
        let mut buf = [0u8; HANDSHAKE_LEN];
        stream.read_exact(&mut buf).await.unwrap();
        let theirs = Handshake::decode(&buf).unwrap();
        assert_eq!(theirs.info_hash, ours.info_hash);
        let mut peer = RawPeer {
            stream,
            decoder: Decoder::new(1 << 20),
            handshake: theirs,
            theirs: None,
        };
        peer.send(&[Message::Extended {
            id: 0,
            payload: Bytes::from(ext.encode()),
        }])
        .await;
        peer
    }

    async fn send(&mut self, messages: &[Message]) {
        let mut out = BytesMut::new();
        for m in messages {
            m.encode(&mut out);
        }
        self.stream.write_all(&out).await.unwrap();
    }

    /// The next message other than a keep-alive or the extension handshake (kept in
    /// `theirs`), within 10 s.
    async fn recv(&mut self) -> Message {
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                match self.decoder.next().unwrap() {
                    Some(Message::KeepAlive) => {}
                    Some(Message::Extended { id: 0, payload }) => {
                        self.theirs = Some(ExtensionHandshake::decode(&payload).unwrap());
                    }
                    Some(m) => return m,
                    None => {
                        let n = self.stream.read_buf(self.decoder.buffer()).await.unwrap();
                        assert!(n > 0, "the session closed the connection");
                    }
                }
            }
        })
        .await
        .expect("no message within 10 s")
    }

    /// The session's extension handshake, reading until it comes.
    async fn their_ext(&mut self) -> ExtensionHandshake {
        tokio::time::timeout(Duration::from_secs(10), async {
            while self.theirs.is_none() {
                match self.decoder.next().unwrap() {
                    Some(Message::Extended { id: 0, payload }) => {
                        self.theirs = Some(ExtensionHandshake::decode(&payload).unwrap());
                    }
                    Some(_) => {}
                    None => {
                        let n = self.stream.read_buf(self.decoder.buffer()).await.unwrap();
                        assert!(n > 0, "the session closed the connection");
                    }
                }
            }
        })
        .await
        .expect("no extension handshake within 10 s");
        self.theirs.clone().unwrap()
    }

    /// Whether the session closes the connection within 10 s.
    async fn is_closed(&mut self) -> bool {
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                match self.stream.read_buf(self.decoder.buffer()).await {
                    Ok(0) | Err(_) => return,
                    Ok(_) => {}
                }
            }
        })
        .await
        .is_ok()
    }

    /// The next `ut_holepunch` message (sent on our id [`HP`]), skipping others.
    async fn holepunch(&mut self) -> HolepunchMessage {
        loop {
            if let Message::Extended { id: HP, payload } = self.recv().await {
                return HolepunchMessage::decode(&payload).unwrap();
            }
        }
    }

    /// Every message (but keep-alives) that comes within `wait`.
    async fn recv_for(&mut self, wait: Duration) -> Vec<Message> {
        let mut got = Vec::new();
        let _ = tokio::time::timeout(wait, async {
            loop {
                match self.decoder.next().unwrap() {
                    Some(Message::KeepAlive) => {}
                    Some(m) => got.push(m),
                    None => {
                        if self.stream.read_buf(self.decoder.buffer()).await.unwrap() == 0 {
                            return;
                        }
                    }
                }
            }
        })
        .await;
        got
    }

    /// Messages until `done` says stop, failing after 10 s without one.
    async fn recv_until(&mut self, mut done: impl FnMut(&Message) -> bool) -> Vec<Message> {
        let mut got = Vec::new();
        loop {
            let m = self.recv().await;
            let stop = done(&m);
            got.push(m);
            if stop {
                return got;
            }
        }
    }
}

/// Every block of the torrent, in order.
fn all_blocks(len: usize) -> Vec<BlockRef> {
    let len = u32::try_from(len).unwrap();
    (0..len.div_ceil(BLOCK))
        .map(|i| {
            let start = i * BLOCK;
            BlockRef {
                piece: start / PIECE,
                offset: start % PIECE,
                length: BLOCK.min(len - start),
            }
        })
        .collect()
}

/// A peer may queue as many requests as our `reqq` allows (far more than are read from disk
/// at once), and every one is answered with the right bytes. A cancelled request gets
/// exactly one answer too, a piece or a reject (BEP 6).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_deep_request_queue_is_served_and_cancels_are_answered() {
    let dir = tempfile::tempdir().unwrap();
    let len = 16 * 1024 * 1024 + 5000;
    let Under {
        session,
        handle: seed,
        torrent,
        bytes,
    } = start(dir.path(), len, Setup::default()).await;
    let mut peer = RawPeer::connect(
        session.listen_addr(),
        &torrent,
        ExtensionHandshake::default(),
        1,
    )
    .await;
    peer.send(&[Message::HaveNone, Message::Interested]).await;
    peer.recv_until(|m| matches!(m, Message::Unchoke)).await;
    let reqq = peer.theirs.as_ref().and_then(|e| e.request_queue).unwrap();
    assert_eq!(reqq, 2000);

    let blocks = all_blocks(len);
    assert!(blocks.len() > 1000 && blocks.len() < reqq as usize);
    let requests: Vec<Message> = blocks.iter().map(|&b| Message::Request(b)).collect();
    // Cancel the last few right away: most likely still queued, but either way each gets
    // one answer.
    let cancelled: Vec<BlockRef> = blocks[blocks.len() - 8..].to_vec();
    let cancels: Vec<Message> = cancelled.iter().map(|&b| Message::Cancel(b)).collect();
    peer.send(&requests).await;
    peer.send(&cancels).await;

    let mut answered = std::collections::HashMap::new();
    while answered.len() < blocks.len() {
        match peer.recv().await {
            Message::Piece {
                piece,
                offset,
                data,
            } => {
                let start = (piece * PIECE + offset) as usize;
                assert!(
                    data[..] == bytes[start..start + data.len()],
                    "block {piece}/{offset}"
                );
                let b = BlockRef {
                    piece,
                    offset,
                    length: u32::try_from(data.len()).unwrap(),
                };
                assert!(answered.insert(b, true).is_none(), "{b:?} answered twice");
            }
            Message::Reject(b) => {
                assert!(cancelled.contains(&b), "{b:?} rejected but not cancelled");
                assert!(answered.insert(b, false).is_none(), "{b:?} answered twice");
            }
            m => panic!("unexpected {m:?}"),
        }
    }
    let rejected = answered.values().filter(|&&sent| !sent).count();
    assert!(rejected <= cancelled.len());
    seed.shutdown().await;
}

/// The handshakes follow the identity: reserved bits, `reqq`, and whether libtorrent's
/// extensions are offered (`WireStyle` in the engine).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn handshakes_follow_the_identity() {
    let deluge = Arc::new(ClientProfile::deluge("2.1.1", "2.0.10.0").unwrap());
    let transmission = Arc::new(ClientProfile::transmission("4.0.6").unwrap());
    // (identity, peer id prefix, DHT bit, v2 bit, reqq, libtorrent's extensions). The
    // session's DHT is off and the torrent is hybrid.
    for (identity, prefix, dht, v2, reqq, lt) in [
        (None, &b"-AT"[..], false, true, 2000, true),
        (Some(deluge), b"-DE211s-", true, true, 2000, true),
        (Some(transmission), b"-TR4060-", false, false, 512, false),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let u = start(
            dir.path(),
            300_000,
            Setup {
                version: Version::Hybrid,
                identity,
                ..Setup::default()
            },
        )
        .await;
        let mut p = RawPeer::connect(
            u.session.listen_addr(),
            &u.torrent,
            ExtensionHandshake::default(),
            1,
        )
        .await;
        let r = p.handshake.reserved;
        let what = String::from_utf8_lossy(prefix).into_owned();
        assert!(p.handshake.peer_id.0.starts_with(prefix), "{what}");
        assert!(r.fast() && r.extension_protocol(), "{what}");
        assert_eq!((r.dht(), r.v2()), (dht, v2), "{what}: DHT and v2 bits");
        let ext = p.their_ext().await;
        assert_eq!(ext.request_queue, Some(reqq), "{what}");
        for name in [names::UPLOAD_ONLY, names::LT_DONTHAVE, names::UT_HOLEPUNCH] {
            assert_eq!(ext.id_of(name).is_some(), lt, "{what}: {name}");
        }
        assert!(ext.upload_only, "{what}: a seed downloads nothing more");
        u.handle.shutdown().await;
    }
}

/// A peer that wants nothing more, told in the handshake or later by `upload_only`, is
/// dropped by a seed: neither has anything for the other.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_seed_drops_peers_that_want_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let u = start(dir.path(), 300_000, Setup::default()).await;
    let addr = u.session.listen_addr();

    let in_handshake = ExtensionHandshake {
        upload_only: true,
        ..Default::default()
    };
    let mut p = RawPeer::connect(addr, &u.torrent, in_handshake, 1).await;
    p.send(&[Message::HaveNone]).await;
    assert!(p.is_closed().await, "upload_only in the handshake");

    let mut p = RawPeer::connect(addr, &u.torrent, ExtensionHandshake::default(), 2).await;
    p.send(&[Message::HaveNone, Message::Interested]).await;
    let id = p.their_ext().await.id_of(names::UPLOAD_ONLY).unwrap();
    p.recv_until(|m| matches!(m, Message::Unchoke)).await;
    p.send(&[Message::Extended {
        id,
        payload: Bytes::from(extended::encode_upload_only(true)),
    }])
    .await;
    assert!(p.is_closed().await, "an upload_only message");
    u.handle.shutdown().await;
}

/// A seed that says `lt_donthave` for a piece has every request for it cancelled, and
/// isn't asked for it again.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn lt_donthave_cancels_requests_for_the_piece() {
    let dir = tempfile::tempdir().unwrap();
    let u = start(
        dir.path(),
        4 * 1024 * 1024,
        Setup {
            seed: false,
            ..Setup::default()
        },
    )
    .await;
    let mut p = RawPeer::connect(
        u.session.listen_addr(),
        &u.torrent,
        ExtensionHandshake::default(),
        1,
    )
    .await;
    p.send(&[Message::HaveAll, Message::Unchoke]).await;
    let id = p.their_ext().await.id_of(names::LT_DONTHAVE).unwrap();
    // We answer nothing, so the session's requests pile up.
    let mut asked = Vec::new();
    while asked.len() < 32 {
        if let Message::Request(b) = p.recv().await {
            asked.push(b);
        }
    }
    let piece = asked[0].piece;
    p.send(&[Message::Extended {
        id,
        payload: Bytes::from(extended::encode_dont_have(piece)),
    }])
    .await;
    let mut cancelled = std::collections::HashSet::new();
    let mut after_cancel = Vec::new();
    for m in p.recv_for(Duration::from_secs(2)).await {
        match m {
            Message::Request(b) if cancelled.is_empty() => asked.push(b),
            Message::Request(b) => after_cancel.push(b),
            Message::Cancel(b) => {
                assert_eq!(b.piece, piece, "{b:?}");
                cancelled.insert(b);
            }
            _ => {}
        }
    }
    let for_piece: std::collections::HashSet<BlockRef> =
        asked.iter().filter(|b| b.piece == piece).copied().collect();
    assert_eq!(cancelled, for_piece);
    assert!(
        after_cancel.iter().all(|b| b.piece != piece),
        "asked again: {after_cancel:?}"
    );
    u.handle.shutdown().await;
}

/// As a relay (BEP 55), the session introduces two peers it is connected to, each to the
/// other's address, and says why when it can't.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_rendezvous_introduces_both_peers() {
    let dir = tempfile::tempdir().unwrap();
    let relay = start(dir.path(), 300_000, Setup::default()).await;
    let addr = relay.session.listen_addr();
    let local = |port: u16| std::net::SocketAddr::from(([127, 0, 0, 1], port));
    let mut asker = RawPeer::connect(addr, &relay.torrent, holepunch_ext(40001), 1).await;
    let mut target = RawPeer::connect(addr, &relay.torrent, holepunch_ext(40002), 2).await;
    let no_holepunch = ExtensionHandshake {
        listen_port: Some(40003),
        ..Default::default()
    };
    let mut plain = RawPeer::connect(addr, &relay.torrent, no_holepunch, 3).await;
    let id = asker.their_ext().await.id_of(names::UT_HOLEPUNCH).unwrap();
    target.their_ext().await;
    plain.their_ext().await;
    let rendezvous = |to| Message::Extended {
        id,
        payload: Bytes::from(HolepunchMessage::Rendezvous(to).encode()),
    };
    // The relay may not have read the target's extension handshake yet: ask until it has.
    let answer = loop {
        asker.send(&[rendezvous(local(40002))]).await;
        match asker.holepunch().await {
            HolepunchMessage::Error(
                _,
                HolepunchError::NotConnected | HolepunchError::NoSupport,
            ) => {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            m => break m,
        }
    };
    assert_eq!(answer, HolepunchMessage::Connect(local(40002)));
    assert_eq!(
        target.holepunch().await,
        HolepunchMessage::Connect(local(40001))
    );

    asker.send(&[rendezvous(local(40001))]).await;
    assert_eq!(
        asker.holepunch().await,
        HolepunchMessage::Error(local(40001), HolepunchError::NoSelf)
    );
    asker.send(&[rendezvous(local(40999))]).await;
    assert_eq!(
        asker.holepunch().await,
        HolepunchMessage::Error(local(40999), HolepunchError::NotConnected)
    );
    asker.send(&[rendezvous(local(40003))]).await;
    assert_eq!(
        asker.holepunch().await,
        HolepunchMessage::Error(local(40003), HolepunchError::NoSupport)
    );
    relay.handle.shutdown().await;
}

/// Told by a relay to connect (BEP 55), the session dials the address over uTP straight
/// away.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_holepunch_connect_dials_over_utp() {
    let dir = tempfile::tempdir().unwrap();
    let u = start(
        dir.path(),
        300_000,
        Setup {
            seed: false,
            utp: true,
            ..Setup::default()
        },
    )
    .await;
    let target = apitorrent_utp::UtpSocket::bind("127.0.0.1:0".parse().unwrap())
        .await
        .unwrap();
    let mut relay =
        RawPeer::connect(u.session.listen_addr(), &u.torrent, holepunch_ext(40001), 1).await;
    let id = relay.their_ext().await.id_of(names::UT_HOLEPUNCH).unwrap();
    relay
        .send(&[Message::Extended {
            id,
            payload: Bytes::from(HolepunchMessage::Connect(target.local_addr()).encode()),
        }])
        .await;
    let (mut stream, _) = tokio::time::timeout(Duration::from_secs(10), target.accept())
        .await
        .expect("no uTP connection within 10 s")
        .unwrap();
    let mut buf = [0u8; HANDSHAKE_LEN];
    stream.read_exact(&mut buf).await.unwrap();
    let theirs = Handshake::decode(&buf).unwrap();
    assert_eq!(Some(theirs.info_hash), u.torrent.info_hashes().get_v1());
    u.handle.shutdown().await;
}

/// A holepunch-capable peer that PEX named and that can't be dialed is asked of the peer
/// that named it (BEP 55 rendezvous).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unreachable_peer_is_asked_of_its_introducer() {
    let dir = tempfile::tempdir().unwrap();
    let u = start(
        dir.path(),
        300_000,
        Setup {
            seed: false,
            utp: true,
            ..Setup::default()
        },
    )
    .await;
    // A loopback port nobody listens on.
    let closed = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap();
    let mut relay =
        RawPeer::connect(u.session.listen_addr(), &u.torrent, holepunch_ext(40001), 1).await;
    let pex = relay.their_ext().await.id_of(names::UT_PEX).unwrap();
    relay
        .send(&[Message::Extended {
            id: pex,
            payload: Bytes::from(
                PexMessage {
                    added: vec![(closed, PexFlags(0x08 | 0x04))],
                    dropped: Vec::new(),
                }
                .encode(),
            ),
        }])
        .await;
    assert_eq!(
        relay.holepunch().await,
        HolepunchMessage::Rendezvous(closed)
    );
    u.handle.shutdown().await;
}

/// Two sessions that dial each other at the same moment (as a holepunch does) end up with
/// one connection, the same one at both ends.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dialing_each_other_leaves_one_connection() {
    let (dir_a, dir_b) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let leech = || Setup {
        seed: false,
        ..Setup::default()
    };
    let a = start(dir_a.path(), 300_000, leech()).await;
    let b = start(dir_b.path(), 300_000, leech()).await;
    tokio::join!(
        a.handle.add_peers(vec![b.session.listen_addr()]),
        b.handle.add_peers(vec![a.session.listen_addr()]),
    );
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if !a.handle.peers().await.is_empty() && !b.handle.peers().await.is_empty() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("no connection within 10 s");
    // Both dials have finished their handshakes by now.
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert_eq!(a.handle.peers().await.len(), 1);
    assert_eq!(b.handle.peers().await.len(), 1);
    a.handle.shutdown().await;
    b.handle.shutdown().await;
}
