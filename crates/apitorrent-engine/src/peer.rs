//! Peer connection tasks: socket I/O only.
//!
//! A connection does the handshake, then shuttles messages between the socket and its
//! torrent's actor. Every protocol decision is made by the actor.

use std::net::SocketAddr;
use std::time::Duration;

use bytes::BytesMut;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::sync::mpsc;

use apitorrent_metainfo::Sha1Hash;
use apitorrent_wire::{Decoder, HANDSHAKE_LEN, Handshake, Message, PeerId, Reserved};

use crate::picker::PeerKey;

pub(crate) const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
pub(crate) const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
/// A peer that sends nothing (not even a keep-alive) for this long is dropped.
const IDLE_TIMEOUT: Duration = Duration::from_secs(150);
/// We send a keep-alive after this long without sending anything.
const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(60);

/// From a connection to its torrent.
#[derive(Debug)]
pub(crate) enum PeerEvent {
    /// The handshake is done and the connection is ready for messages.
    Ready {
        reserved: Reserved,
        peer_id: PeerId,
        tx: mpsc::UnboundedSender<PeerCmd>,
    },
    Message(Message),
    /// The connection is gone. Always the last event for a key.
    Closed {
        reason: String,
    },
}

/// From a torrent to a connection.
#[derive(Debug)]
pub(crate) enum PeerCmd {
    Send(Message),
    Close,
}

pub(crate) type EventTx = mpsc::Sender<(PeerKey, PeerEvent)>;

/// Connect to `addr`, handshake, and run the connection until it closes.
pub(crate) async fn run_outbound(
    addr: SocketAddr,
    ours: Handshake,
    key: PeerKey,
    max_message: u32,
    events: EventTx,
) {
    let result = async {
        let stream = tokio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect(addr))
            .await
            .map_err(|_| "connect timed out".to_owned())?
            .map_err(|e| format!("connect failed: {e}"))?;
        let _ = stream.set_nodelay(true);
        let mut stream = stream;
        stream
            .write_all(&ours.encode())
            .await
            .map_err(|e| format!("handshake write failed: {e}"))?;
        let theirs = read_handshake(&mut stream).await?;
        if theirs.info_hash != ours.info_hash {
            return Err("peer answered for a different torrent".to_owned());
        }
        if theirs.peer_id == ours.peer_id {
            return Err("connected to ourselves".to_owned());
        }
        Ok((stream, theirs))
    }
    .await;
    match result {
        Ok((stream, theirs)) => {
            run_connected(
                stream,
                theirs.reserved,
                theirs.peer_id,
                key,
                max_message,
                events,
            )
            .await;
        }
        Err(reason) => {
            let _ = events.send((key, PeerEvent::Closed { reason })).await;
        }
    }
}

/// Run an accepted connection whose handshake has already been read; ours is sent here.
pub(crate) async fn run_inbound(
    mut stream: TcpStream,
    theirs: Handshake,
    ours: Handshake,
    key: PeerKey,
    max_message: u32,
    events: EventTx,
) {
    if theirs.peer_id == ours.peer_id {
        let reason = "connected to ourselves".to_owned();
        let _ = events.send((key, PeerEvent::Closed { reason })).await;
        return;
    }
    if let Err(e) = stream.write_all(&ours.encode()).await {
        let reason = format!("handshake write failed: {e}");
        let _ = events.send((key, PeerEvent::Closed { reason })).await;
        return;
    }
    run_connected(
        stream,
        theirs.reserved,
        theirs.peer_id,
        key,
        max_message,
        events,
    )
    .await;
}

/// Read a full handshake, with a timeout.
pub(crate) async fn read_handshake(stream: &mut TcpStream) -> Result<Handshake, String> {
    let mut buf = [0u8; HANDSHAKE_LEN];
    tokio::time::timeout(HANDSHAKE_TIMEOUT, stream.read_exact(&mut buf))
        .await
        .map_err(|_| "handshake timed out".to_owned())?
        .map_err(|e| format!("handshake read failed: {e}"))?;
    Handshake::decode(&buf).map_err(|e| format!("bad handshake: {e}"))
}

async fn run_connected(
    stream: TcpStream,
    reserved: Reserved,
    peer_id: PeerId,
    key: PeerKey,
    max_message: u32,
    events: EventTx,
) {
    let (read, write) = stream.into_split();
    let (tx, rx) = mpsc::unbounded_channel();
    if events
        .send((
            key,
            PeerEvent::Ready {
                reserved,
                peer_id,
                tx,
            },
        ))
        .await
        .is_err()
    {
        return;
    }
    let mut writer = tokio::spawn(write_loop(write, rx));
    let reader = read_loop(read, key, max_message, &events);
    let reason = tokio::select! {
        r = reader => r,
        w = &mut writer => match w {
            Ok(Ok(())) => "closed by us".to_owned(),
            Ok(Err(e)) => e,
            Err(e) => format!("writer task failed: {e}"),
        },
    };
    writer.abort();
    let _ = events.send((key, PeerEvent::Closed { reason })).await;
}

async fn read_loop(
    mut read: OwnedReadHalf,
    key: PeerKey,
    max_message: u32,
    events: &EventTx,
) -> String {
    let mut decoder = Decoder::new(max_message);
    loop {
        loop {
            match decoder.next() {
                Ok(Some(m)) => {
                    if events.send((key, PeerEvent::Message(m))).await.is_err() {
                        return "torrent stopped".to_owned();
                    }
                }
                Ok(None) => break,
                Err(e) => return format!("protocol error: {e}"),
            }
        }
        let buf = decoder.buffer();
        if buf.capacity() - buf.len() < 16 * 1024 {
            buf.reserve(64 * 1024);
        }
        match tokio::time::timeout(IDLE_TIMEOUT, read.read_buf(buf)).await {
            Err(_) => return "peer went silent".to_owned(),
            Ok(Ok(0)) => return "closed by peer".to_owned(),
            Ok(Ok(_)) => {}
            Ok(Err(e)) => return format!("read failed: {e}"),
        }
    }
}

async fn write_loop(
    mut write: OwnedWriteHalf,
    mut rx: mpsc::UnboundedReceiver<PeerCmd>,
) -> Result<(), String> {
    let mut out = BytesMut::with_capacity(64 * 1024);
    loop {
        let cmd = match tokio::time::timeout(KEEPALIVE_INTERVAL, rx.recv()).await {
            Err(_) => PeerCmd::Send(Message::KeepAlive),
            Ok(None | Some(PeerCmd::Close)) => {
                let _ = write.shutdown().await;
                return Ok(());
            }
            Ok(Some(cmd)) => cmd,
        };
        let PeerCmd::Send(m) = cmd else {
            unreachable!()
        };
        m.encode(&mut out);
        // Batch whatever else is already queued into the same write.
        while out.len() < 256 * 1024 {
            match rx.try_recv() {
                Ok(PeerCmd::Send(m)) => m.encode(&mut out),
                Ok(PeerCmd::Close) => {
                    write
                        .write_all(&out)
                        .await
                        .map_err(|e| format!("write failed: {e}"))?;
                    let _ = write.shutdown().await;
                    return Ok(());
                }
                Err(_) => break,
            }
        }
        write
            .write_all(&out)
            .await
            .map_err(|e| format!("write failed: {e}"))?;
        out.clear();
    }
}

/// The handshake we send for a torrent.
pub(crate) fn our_handshake(info_hash: Sha1Hash, peer_id: PeerId, v2: bool) -> Handshake {
    Handshake {
        reserved: Reserved::default()
            .with_extension_protocol(true)
            .with_fast(true)
            .with_v2(v2),
        info_hash,
        peer_id,
    }
}
