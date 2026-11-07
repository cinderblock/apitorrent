//! A tokio stream that runs the MSE handshake and then encrypts and decrypts the payload.

use std::io;
use std::pin::Pin;
use std::task::{Context, Poll, ready};
use std::time::Duration;

use apitorrent_metainfo::Sha1Hash;
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt, ReadBuf};

use crate::crypto::Rc4Cipher;
use crate::error::MseError;
use crate::handshake::{CryptoMethod, Incoming, Initiator, Policy, Receiver, SkeyLookup};

/// The default time limit for a whole handshake.
pub const DEFAULT_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(20);

/// The most bytes encrypted per write call. Bounds the buffer held for a pending write.
const MAX_WRITE_CHUNK: usize = 64 * 1024;

/// The read size during the handshake. The handshake never consumes past its end, so
/// whatever it leaves of one read (at most this much) is the start of the payload.
const HANDSHAKE_READ: usize = 1024;

/// Settings for one connection's handshake.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MseConfig {
    pub policy: Policy,
    /// The time limit for the whole handshake, including the first payload read.
    pub handshake_timeout: Duration,
}

impl Default for MseConfig {
    fn default() -> Self {
        Self {
            policy: Policy::default(),
            handshake_timeout: DEFAULT_HANDSHAKE_TIMEOUT,
        }
    }
}

impl MseConfig {
    pub fn new(policy: Policy) -> Self {
        Self {
            policy,
            ..Self::default()
        }
    }
}

/// What the handshake settled on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Encryption {
    /// No MSE at all: plain BitTorrent.
    None,
    /// MSE, with the payload carried by this method.
    Mse(CryptoMethod),
}

impl Encryption {
    /// Whether the payload is encrypted.
    pub fn is_rc4(self) -> bool {
        self == Self::Mse(CryptoMethod::Rc4)
    }
}

/// The read direction: buffered payload bytes from the handshake, then the decryptor.
#[derive(Debug, Default)]
pub struct ReadState {
    prefix: Vec<u8>,
    prefix_pos: usize,
    decrypt: Option<Rc4Cipher>,
}

impl ReadState {
    fn poll_read<R: AsyncRead + Unpin + ?Sized>(
        &mut self,
        io: &mut R,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        if self.prefix_pos < self.prefix.len() {
            let n = (self.prefix.len() - self.prefix_pos).min(buf.remaining());
            buf.put_slice(&self.prefix[self.prefix_pos..self.prefix_pos + n]);
            self.prefix_pos += n;
            if self.prefix_pos == self.prefix.len() {
                self.prefix = Vec::new();
                self.prefix_pos = 0;
            }
            return Poll::Ready(Ok(()));
        }
        let before = buf.filled().len();
        ready!(Pin::new(io).poll_read(cx, buf))?;
        if let Some(c) = &mut self.decrypt {
            c.apply(&mut buf.filled_mut()[before..]);
        }
        Poll::Ready(Ok(()))
    }
}

/// The write direction: the encryptor and any encrypted bytes not yet written.
#[derive(Debug, Default)]
pub struct WriteState {
    encrypt: Option<Rc4Cipher>,
    pending: Vec<u8>,
    pending_pos: usize,
}

impl WriteState {
    fn poll_drain<W: AsyncWrite + Unpin + ?Sized>(
        &mut self,
        io: &mut W,
        cx: &mut Context<'_>,
    ) -> Poll<io::Result<()>> {
        while self.pending_pos < self.pending.len() {
            let n = ready!(Pin::new(&mut *io).poll_write(cx, &self.pending[self.pending_pos..]))?;
            if n == 0 {
                return Poll::Ready(Err(io::ErrorKind::WriteZero.into()));
            }
            self.pending_pos += n;
        }
        self.pending.clear();
        self.pending_pos = 0;
        Poll::Ready(Ok(()))
    }

    fn poll_write<W: AsyncWrite + Unpin + ?Sized>(
        &mut self,
        io: &mut W,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        if self.encrypt.is_none() {
            return Pin::new(io).poll_write(cx, buf);
        }
        // Bytes are encrypted once and then owned here, so a write that is accepted stays
        // accepted: the keystream has moved past it.
        if self.pending_pos < self.pending.len() {
            ready!(self.poll_drain(io, cx))?;
        }
        if buf.is_empty() {
            return Poll::Ready(Ok(0));
        }
        let n = buf.len().min(MAX_WRITE_CHUNK);
        let cipher = self.encrypt.as_mut().expect("checked above");
        self.pending.clear();
        self.pending_pos = 0;
        self.pending.extend_from_slice(&buf[..n]);
        cipher.apply(&mut self.pending);
        // Start sending now; an error or a full socket shows up on the next call.
        if let Poll::Ready(Ok(m)) = Pin::new(io).poll_write(cx, &self.pending) {
            self.pending_pos = m;
        }
        Poll::Ready(Ok(n))
    }

    fn poll_flush<W: AsyncWrite + Unpin + ?Sized>(
        &mut self,
        io: &mut W,
        cx: &mut Context<'_>,
    ) -> Poll<io::Result<()>> {
        ready!(self.poll_drain(io, cx))?;
        Pin::new(io).poll_flush(cx)
    }

    fn poll_shutdown<W: AsyncWrite + Unpin + ?Sized>(
        &mut self,
        io: &mut W,
        cx: &mut Context<'_>,
    ) -> Poll<io::Result<()>> {
        ready!(self.poll_drain(io, cx))?;
        Pin::new(io).poll_shutdown(cx)
    }
}

/// A connection after its MSE handshake (or a plain one that skipped it).
///
/// Reads yield the peer's payload in the clear, starting with anything that arrived during
/// the handshake (for an accepted connection, the initiator's initial payload). Writes are
/// encrypted when RC4 was selected. To split it without a lock (for example over
/// `TcpStream::into_split`), take it apart with [`into_parts`](Self::into_parts) and wrap
/// the halves in [`MseReadHalf`] and [`MseWriteHalf`].
#[derive(Debug)]
pub struct MseStream<S> {
    io: S,
    read: ReadState,
    write: WriteState,
    encryption: Encryption,
    info_hash: Option<Sha1Hash>,
}

impl<S: AsyncRead + AsyncWrite + Unpin> MseStream<S> {
    /// Open an outgoing connection's encryption for `info_hash`.
    ///
    /// `initial_payload` is sent first: inside the handshake (IA) when MSE is used, or as
    /// plain bytes when the policy is [`Policy::Disabled`]. Passing our BitTorrent
    /// handshake here saves a round trip.
    pub async fn connect(
        io: S,
        info_hash: Sha1Hash,
        initial_payload: &[u8],
        config: MseConfig,
    ) -> Result<Self, MseError> {
        let mut io = io;
        if !config.policy.initiates_mse() {
            tokio::time::timeout(config.handshake_timeout, async {
                io.write_all(initial_payload).await?;
                io.flush().await
            })
            .await
            .map_err(|_| MseError::Timeout)??;
            return Ok(Self::plain(io, Vec::new(), Some(info_hash)));
        }
        let mut machine = Initiator::new(
            info_hash,
            config.policy.provide(),
            initial_payload.to_vec(),
            &mut rand::rng(),
        )?;
        let rest = tokio::time::timeout(config.handshake_timeout, drive(&mut io, &mut machine))
            .await
            .map_err(|_| MseError::Timeout)??;
        let done = machine.finish().expect("the handshake is done");
        Ok(Self::established(io, done, Vec::new(), &rest))
    }

    /// Run the receiving side of a handshake on an accepted connection.
    ///
    /// Detects a plain BitTorrent handshake and passes it through when the policy allows;
    /// the bytes read to tell them apart are replayed to the first reads.
    pub async fn accept<L: SkeyLookup>(
        io: S,
        lookup: L,
        config: MseConfig,
    ) -> Result<Self, MseError> {
        let mut io = io;
        let mut machine = Receiver::new(lookup, config.policy, &mut rand::rng());
        let rest = tokio::time::timeout(config.handshake_timeout, drive(&mut io, &mut machine))
            .await
            .map_err(|_| MseError::Timeout)??;
        Ok(match machine.finish().expect("the handshake is done") {
            Incoming::Plaintext { mut replay } => {
                replay.extend_from_slice(&rest);
                Self::plain(io, replay, None)
            }
            Incoming::Encrypted(mut done) => {
                let ia = std::mem::take(&mut done.initial_payload);
                Self::established(io, done, ia, &rest)
            }
        })
    }
}

impl<S> MseStream<S> {
    fn plain(io: S, prefix: Vec<u8>, info_hash: Option<Sha1Hash>) -> Self {
        Self {
            io,
            read: ReadState {
                prefix,
                prefix_pos: 0,
                decrypt: None,
            },
            write: WriteState::default(),
            encryption: Encryption::None,
            info_hash,
        }
    }

    fn established(
        io: S,
        done: crate::handshake::Established,
        mut prefix: Vec<u8>,
        rest: &[u8],
    ) -> Self {
        let (encrypt, mut decrypt) = match done.ciphers {
            Some(c) => (Some(c.encrypt), Some(c.decrypt)),
            None => (None, None),
        };
        let start = prefix.len();
        prefix.extend_from_slice(rest);
        if let Some(d) = &mut decrypt {
            d.apply(&mut prefix[start..]);
        }
        Self {
            io,
            read: ReadState {
                prefix,
                prefix_pos: 0,
                decrypt,
            },
            write: WriteState {
                encrypt,
                pending: Vec::new(),
                pending_pos: 0,
            },
            encryption: Encryption::Mse(done.method),
            info_hash: Some(done.info_hash),
        }
    }

    /// What the handshake settled on.
    pub fn encryption(&self) -> Encryption {
        self.encryption
    }

    /// The torrent the connection is for, when MSE named it (or we initiated). `None` for
    /// an accepted plaintext connection: read its BitTorrent handshake to find out.
    pub fn info_hash(&self) -> Option<Sha1Hash> {
        self.info_hash
    }

    pub fn get_ref(&self) -> &S {
        &self.io
    }

    pub fn get_mut(&mut self) -> &mut S {
        &mut self.io
    }

    /// Take the stream apart, to split the transport and wrap each half.
    ///
    /// Any encrypted bytes not yet written are lost, so flush first.
    pub fn into_parts(self) -> (S, ReadState, WriteState) {
        (self.io, self.read, self.write)
    }
}

/// The parts of the two handshake machines that the driver needs.
trait Machine {
    fn feed(&mut self, input: &[u8]) -> Result<usize, MseError>;
    fn take_output(&mut self) -> Vec<u8>;
    fn is_done(&self) -> bool;
}

impl Machine for Initiator {
    fn feed(&mut self, input: &[u8]) -> Result<usize, MseError> {
        Initiator::feed(self, input)
    }
    fn take_output(&mut self) -> Vec<u8> {
        Initiator::take_output(self)
    }
    fn is_done(&self) -> bool {
        Initiator::is_done(self)
    }
}

impl<L: SkeyLookup> Machine for Receiver<L> {
    fn feed(&mut self, input: &[u8]) -> Result<usize, MseError> {
        Receiver::feed(self, input)
    }
    fn take_output(&mut self) -> Vec<u8> {
        Receiver::take_output(self)
    }
    fn is_done(&self) -> bool {
        Receiver::is_done(self)
    }
}

/// Run a handshake machine to completion, reading and writing at the same time so that
/// neither side can stall the other on a full buffer. Returns the bytes read past the end
/// of the handshake.
async fn drive<S, M>(io: &mut S, machine: &mut M) -> Result<Vec<u8>, MseError>
where
    S: AsyncRead + AsyncWrite + Unpin,
    M: Machine,
{
    let mut out = Vec::new();
    let mut out_pos = 0;
    let mut buf = [0u8; HANDSHAKE_READ];
    let mut rest = Vec::new();
    std::future::poll_fn(|cx| {
        loop {
            let mut progressed = false;
            if out_pos == out.len() {
                out = machine.take_output();
                out_pos = 0;
            }
            while out_pos < out.len() {
                match Pin::new(&mut *io).poll_write(cx, &out[out_pos..]) {
                    Poll::Ready(Ok(0)) => return Poll::Ready(Err(MseError::Closed)),
                    Poll::Ready(Ok(n)) => {
                        out_pos += n;
                        progressed = true;
                    }
                    Poll::Ready(Err(e)) => return Poll::Ready(Err(e.into())),
                    Poll::Pending => break,
                }
            }
            if machine.is_done() {
                if out_pos < out.len() {
                    return Poll::Pending;
                }
                ready!(Pin::new(&mut *io).poll_flush(cx))?;
                return Poll::Ready(Ok(std::mem::take(&mut rest)));
            }
            let mut rb = ReadBuf::new(&mut buf);
            match Pin::new(&mut *io).poll_read(cx, &mut rb) {
                Poll::Ready(Ok(())) => {
                    let n = rb.filled().len();
                    if n == 0 {
                        return Poll::Ready(Err(MseError::Closed));
                    }
                    let used = machine.feed(&buf[..n])?;
                    rest.extend_from_slice(&buf[used..n]);
                    progressed = true;
                }
                Poll::Ready(Err(e)) => return Poll::Ready(Err(e.into())),
                Poll::Pending => {}
            }
            if !progressed {
                return Poll::Pending;
            }
        }
    })
    .await
}

impl<S: AsyncRead + Unpin> AsyncRead for MseStream<S> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        this.read.poll_read(&mut this.io, cx, buf)
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for MseStream<S> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        this.write.poll_write(&mut this.io, cx, buf)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        this.write.poll_flush(&mut this.io, cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        this.write.poll_shutdown(&mut this.io, cx)
    }
}

/// The read half of a split [`MseStream`].
#[derive(Debug)]
pub struct MseReadHalf<R> {
    io: R,
    state: ReadState,
}

impl<R> MseReadHalf<R> {
    pub fn new(io: R, state: ReadState) -> Self {
        Self { io, state }
    }

    pub fn into_inner(self) -> R {
        self.io
    }
}

impl<R: AsyncRead + Unpin> AsyncRead for MseReadHalf<R> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        this.state.poll_read(&mut this.io, cx, buf)
    }
}

/// The write half of a split [`MseStream`].
#[derive(Debug)]
pub struct MseWriteHalf<W> {
    io: W,
    state: WriteState,
}

impl<W> MseWriteHalf<W> {
    pub fn new(io: W, state: WriteState) -> Self {
        Self { io, state }
    }
}

impl<W: AsyncWrite + Unpin> AsyncWrite for MseWriteHalf<W> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        this.state.poll_write(&mut this.io, cx, buf)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        this.state.poll_flush(&mut this.io, cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        this.state.poll_shutdown(&mut this.io, cx)
    }
}
