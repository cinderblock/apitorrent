//! One uTP connection as a sans-IO state machine.
//!
//! The connection never touches a socket or a clock. The driver feeds it packets
//! ([`handle_packet`](Connection::handle_packet)) and timer events
//! ([`handle_timeout`](Connection::handle_timeout)), asks it for datagrams to send
//! ([`poll_transmit`](Connection::poll_transmit)) and for its next deadline
//! ([`poll_timeout`](Connection::poll_timeout)), and moves application bytes in and out
//! ([`write`](Connection::write), [`read`](Connection::read)). Every call takes `now`, so
//! tests run on simulated time.
//!
//! What it implements: the SYN handshake with retries, cumulative and selective ACKs,
//! fast retransmit (three duplicate ACKs, or three later packets selectively acked),
//! retransmission timeouts with exponential backoff (RFC 6298), LEDBAT congestion control,
//! the peer's receive window with zero-window probes, FIN in both directions, and RESET.

use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};

use crate::config::Config;
use crate::congestion::{Ledbat, Rtt};
use crate::packet::{self, HEADER_LEN, Header, Packet, PacketType, SelectiveAck, seq};

/// The most packets in flight at once. Well under half the sequence space, so sequence
/// comparisons never become ambiguous.
const MAX_IN_FLIGHT: usize = 4096;

/// The furthest ahead of the next expected packet that we buffer out-of-order data.
const MAX_REORDER: u16 = 4096;

/// Duplicate ACKs, or later packets selectively acked, that mark a packet lost.
const DUP_ACK_THRESHOLD: u32 = 3;

/// Why a connection ended badly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, thiserror::Error)]
pub enum ConnError {
    #[error("the peer refused the connection")]
    Refused,
    #[error("the peer reset the connection")]
    Reset,
    #[error("the connection timed out")]
    TimedOut,
}

impl From<ConnError> for std::io::Error {
    fn from(e: ConnError) -> Self {
        let kind = match e {
            ConnError::Refused => std::io::ErrorKind::ConnectionRefused,
            ConnError::Reset => std::io::ErrorKind::ConnectionReset,
            ConnError::TimedOut => std::io::ErrorKind::TimedOut,
        };
        Self::new(kind, e)
    }
}

/// Where a connection is in its life.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    /// Our SYN is out; waiting for the peer's ACK.
    SynSent,
    /// Open. Either side may have sent its FIN.
    Connected,
    /// Both FINs sent and acknowledged.
    Closed,
    /// Ended by a reset or a timeout.
    Failed(ConnError),
}

/// A snapshot of a connection's transport state.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Stats {
    pub congestion_window: usize,
    pub peer_window: usize,
    pub bytes_in_flight: usize,
    pub packets_in_flight: usize,
    pub srtt: Option<Duration>,
    pub rto: Duration,
    pub queuing_delay: Option<Duration>,
    pub packets_sent: u64,
    pub packets_retransmitted: u64,
    pub packets_received: u64,
}

#[derive(Debug)]
#[allow(clippy::struct_excessive_bools)]
struct Sent {
    seq: u16,
    ty: PacketType,
    payload: Vec<u8>,
    sent_at: Option<Instant>,
    transmissions: u32,
    /// Waiting to be (re)sent.
    pending: bool,
    sacked: bool,
    /// Counted in `flight_bytes`.
    counted: bool,
    /// Already fast-retransmitted; don't do it again before a timeout.
    fast_resent: bool,
}

impl Sent {
    fn wire_size(&self) -> usize {
        HEADER_LEN + self.payload.len()
    }
}

/// One uTP connection.
#[derive(Debug)]
#[allow(clippy::struct_excessive_bools)]
pub struct Connection {
    cfg: Config,
    state: State,
    epoch: Instant,
    send_id: u16,
    recv_id: u16,
    /// The sequence number of the next packet we send.
    seq_nr: u16,
    /// The last packet received in order.
    ack_nr: u16,
    /// For an accepted connection: the peer's SYN, to recognise retransmissions of it.
    syn_seq: Option<u16>,
    /// Whether the peer is known to have our first sequence number. An accepted connection
    /// sends no data until the initiator's first packet after its SYN shows it got our
    /// STATE: that STATE tells it where our stream starts, and must not be overtaken.
    confirmed: bool,
    /// Answer the SYN (or a retransmission of it) with a STATE.
    syn_ack_needed: bool,

    send_queue: VecDeque<u8>,
    in_flight: VecDeque<Sent>,
    flight_bytes: usize,
    flight_payload: usize,
    peer_wnd: usize,
    window_limited: bool,
    probe: bool,
    close_requested: bool,
    fin_sent: bool,
    fin_acked: bool,

    readable: VecDeque<u8>,
    reorder: HashMap<u16, Vec<u8>>,
    reorder_bytes: usize,
    peer_fin: Option<u16>,
    eof: bool,
    ack_needed: bool,
    last_adv_wnd: usize,
    reply_micro: u32,

    rtt: Rtt,
    ledbat: Ledbat,
    rto_deadline: Option<Instant>,
    connect_deadline: Option<Instant>,
    timeouts: u32,
    last_send: Instant,
    last_ack: u16,
    dup_acks: u32,
    recovery_end: Option<u16>,

    /// After a clean close, how long we keep answering retransmitted FINs.
    linger_until: Option<Instant>,
    released: bool,

    packets_sent: u64,
    packets_retransmitted: u64,
    packets_received: u64,
}

impl Connection {
    fn new(cfg: &Config, now: Instant, epoch: Instant, state: State) -> Self {
        let cfg = cfg.sanitized();
        let mss = cfg.packet_size;
        Self {
            rtt: Rtt::new(cfg.initial_rto, cfg.min_rto, cfg.max_rto),
            ledbat: Ledbat::new(mss, cfg.send_buffer, cfg.target_delay),
            last_adv_wnd: cfg.recv_buffer,
            peer_wnd: cfg.recv_buffer,
            state,
            epoch,
            send_id: 0,
            recv_id: 0,
            seq_nr: 0,
            ack_nr: 0,
            syn_seq: None,
            confirmed: false,
            syn_ack_needed: false,
            send_queue: VecDeque::new(),
            in_flight: VecDeque::new(),
            flight_bytes: 0,
            flight_payload: 0,
            window_limited: false,
            probe: false,
            close_requested: false,
            fin_sent: false,
            fin_acked: false,
            readable: VecDeque::new(),
            reorder: HashMap::new(),
            reorder_bytes: 0,
            peer_fin: None,
            eof: false,
            ack_needed: false,
            reply_micro: 0,
            rto_deadline: None,
            connect_deadline: None,
            timeouts: 0,
            last_send: now,
            last_ack: 0,
            dup_acks: 0,
            recovery_end: None,
            linger_until: None,
            released: false,
            packets_sent: 0,
            packets_retransmitted: 0,
            packets_received: 0,
            cfg,
        }
    }

    /// Open a connection: queue a SYN. `recv_id` is the connection id the peer will send
    /// to us (ours to send with is `recv_id + 1`); `initial_seq` is our first sequence
    /// number. `epoch` is the zero of the microsecond timestamps.
    pub fn connect(
        cfg: &Config,
        now: Instant,
        epoch: Instant,
        recv_id: u16,
        initial_seq: u16,
    ) -> Self {
        let mut c = Self::new(cfg, now, epoch, State::SynSent);
        c.recv_id = recv_id;
        c.send_id = recv_id.wrapping_add(1);
        c.seq_nr = initial_seq.wrapping_add(1);
        c.last_ack = initial_seq.wrapping_sub(1);
        c.in_flight.push_back(Sent {
            seq: initial_seq,
            ty: PacketType::Syn,
            payload: Vec::new(),
            sent_at: None,
            transmissions: 0,
            pending: true,
            sacked: false,
            counted: false,
            fast_resent: false,
        });
        c.connect_deadline = Some(now + c.cfg.connect_timeout);
        c.confirmed = true;
        c
    }

    /// Answer a peer's SYN: the connection is open at once, and an ACK is queued.
    pub fn accept(
        cfg: &Config,
        now: Instant,
        epoch: Instant,
        syn: &Header,
        initial_seq: u16,
    ) -> Self {
        let mut c = Self::new(cfg, now, epoch, State::Connected);
        c.send_id = syn.connection_id;
        c.recv_id = syn.connection_id.wrapping_add(1);
        c.seq_nr = initial_seq;
        c.last_ack = initial_seq.wrapping_sub(1);
        c.ack_nr = syn.seq_nr;
        c.syn_seq = Some(syn.seq_nr);
        c.peer_wnd = usize::try_from(syn.wnd_size).unwrap_or(usize::MAX);
        c.reply_micro = c.micros(now).wrapping_sub(syn.timestamp);
        c.syn_ack_needed = true;
        c
    }

    /// The id the peer sends to us with: the key to route incoming packets by.
    pub fn recv_id(&self) -> u16 {
        self.recv_id
    }

    /// The id we send with.
    pub fn send_id(&self) -> u16 {
        self.send_id
    }

    pub fn state(&self) -> State {
        self.state
    }

    pub fn is_connected(&self) -> bool {
        self.state == State::Connected
    }

    /// Whether the connection is over: closed cleanly, reset or timed out.
    pub fn is_finished(&self) -> bool {
        matches!(self.state, State::Closed | State::Failed(_))
    }

    /// Whether the driver may forget the connection. A cleanly closed one lingers for a
    /// couple of round trips to acknowledge a retransmitted FIN.
    pub fn can_release(&self) -> bool {
        self.released || matches!(self.state, State::Failed(_))
    }

    pub fn error(&self) -> Option<ConnError> {
        match self.state {
            State::Failed(e) => Some(e),
            _ => None,
        }
    }

    /// Our FIN has been acknowledged: everything we wrote was delivered.
    pub fn is_write_closed(&self) -> bool {
        self.fin_acked
    }

    /// The peer's FIN arrived and every byte before it has been read.
    pub fn is_read_closed(&self) -> bool {
        self.eof && self.readable.is_empty()
    }

    pub fn stats(&self) -> Stats {
        Stats {
            congestion_window: self.ledbat.window(),
            peer_window: self.peer_wnd,
            bytes_in_flight: self.flight_bytes,
            packets_in_flight: self.in_flight.len(),
            srtt: self.rtt.srtt(),
            rto: self.rtt.rto(),
            queuing_delay: self.ledbat.queuing_delay(),
            packets_sent: self.packets_sent,
            packets_retransmitted: self.packets_retransmitted,
            packets_received: self.packets_received,
        }
    }

    fn micros(&self, now: Instant) -> u32 {
        let us = now.saturating_duration_since(self.epoch).as_micros();
        #[allow(clippy::cast_possible_truncation)]
        let wrapped = us as u32;
        wrapped
    }

    fn max_payload(&self) -> usize {
        self.cfg.packet_size - HEADER_LEN
    }

    fn adv_window(&self) -> usize {
        self.cfg
            .recv_buffer
            .saturating_sub(self.readable.len() + self.reorder_bytes)
    }

    /// How many more bytes [`write`](Self::write) would take now.
    pub fn send_capacity(&self) -> usize {
        if self.close_requested || self.is_finished() {
            return 0;
        }
        self.cfg
            .send_buffer
            .saturating_sub(self.send_queue.len() + self.flight_payload)
    }

    /// Queue bytes to send. Returns how many were taken (0 when the buffer is full).
    pub fn write(&mut self, data: &[u8]) -> Result<usize, std::io::Error> {
        if let Some(e) = self.error() {
            return Err(e.into());
        }
        if self.close_requested || self.state == State::Closed {
            return Err(std::io::ErrorKind::BrokenPipe.into());
        }
        let n = data.len().min(self.send_capacity());
        self.send_queue.extend(&data[..n]);
        Ok(n)
    }

    /// Take received bytes. `Ok(None)` when none are ready yet; `Ok(Some(0))` at the end
    /// of the peer's stream.
    pub fn read(&mut self, buf: &mut [u8]) -> Result<Option<usize>, std::io::Error> {
        if !self.readable.is_empty() {
            let n = buf.len().min(self.readable.len());
            for (dst, src) in buf.iter_mut().zip(self.readable.drain(..n)) {
                *dst = src;
            }
            // Tell the peer when its window opens up again.
            let wnd = self.adv_window();
            let mss = self.cfg.packet_size;
            if (self.last_adv_wnd < mss && wnd >= mss)
                || wnd >= self.last_adv_wnd + self.cfg.recv_buffer / 2
            {
                self.ack_needed = true;
            }
            return Ok(Some(n));
        }
        if self.eof {
            return Ok(Some(0));
        }
        if let Some(e) = self.error() {
            return Err(e.into());
        }
        Ok(None)
    }

    /// Whether [`read`](Self::read) has something to return (data, end of stream or an
    /// error).
    pub fn is_readable(&self) -> bool {
        !self.readable.is_empty() || self.eof || self.error().is_some()
    }

    /// Send a FIN once everything queued has gone out. Writes are refused from now on.
    pub fn close(&mut self) {
        self.close_requested = true;
    }

    /// Whether [`poll_transmit`](Self::poll_transmit) might return something.
    pub fn wants_transmit(&self) -> bool {
        if self.is_finished() {
            return self.ack_needed;
        }
        self.ack_needed
            || self.syn_ack_needed
            || self.in_flight.iter().any(|p| p.pending)
            || !self.send_queue.is_empty()
            || (self.close_requested && !self.fin_sent)
    }

    /// Process a packet the driver routed to this connection.
    pub fn handle_packet(&mut self, now: Instant, pkt: &Packet<'_>) {
        let h = &pkt.header;
        self.packets_received += 1;
        match self.state {
            State::Failed(_) => return,
            State::Closed => {
                // Keep acknowledging retransmitted FINs until the driver drops us.
                if matches!(h.ty, PacketType::Fin | PacketType::Data) {
                    self.ack_needed = true;
                }
                return;
            }
            State::SynSent | State::Connected => {}
        }
        self.timeouts = 0;
        self.reply_micro = self.micros(now).wrapping_sub(h.timestamp);
        match h.ty {
            PacketType::Reset => {
                self.fail(if self.state == State::SynSent {
                    ConnError::Refused
                } else {
                    ConnError::Reset
                });
                return;
            }
            PacketType::Syn => {
                if self.syn_seq == Some(h.seq_nr) && !self.confirmed {
                    // Our STATE was lost.
                    self.syn_ack_needed = true;
                }
                return;
            }
            PacketType::Data | PacketType::Fin | PacketType::State => {}
        }
        if self.state == State::SynSent {
            // Only the STATE answering our SYN tells us where the peer's stream starts.
            // Its data may overtake it, and taking a data packet's number as the start
            // would skip whatever came before; the peer resends those later.
            let syn = self.in_flight.front().map(|p| p.seq);
            if h.ty != PacketType::State || syn != Some(h.ack_nr) {
                return;
            }
            self.state = State::Connected;
            self.connect_deadline = None;
            // A STATE carries the peer's next sequence number.
            self.ack_nr = h.seq_nr.wrapping_sub(1);
            // Confirm at once, so the peer may start sending.
            self.ack_needed = true;
        }
        self.confirmed = true;
        self.syn_ack_needed = false;
        let old_wnd = self.peer_wnd;
        self.peer_wnd = usize::try_from(h.wnd_size).unwrap_or(usize::MAX);
        if h.timestamp_diff != 0 {
            self.ledbat.on_delay_sample(now, h.timestamp_diff);
        }
        let dup_candidate = h.ty == PacketType::State && pkt.payload.is_empty();
        self.process_ack(now, h.ack_nr, pkt.sack.as_ref(), dup_candidate);

        // The peer's window reopened while a probe sits unanswered: resend it now rather
        // than waiting out the backed-off timer.
        let mss = self.cfg.packet_size;
        if old_wnd < mss
            && self.peer_wnd >= mss
            && let Some(front) = self.in_flight.front_mut()
            && !front.sacked
            && !front.pending
        {
            front.pending = true;
            if front.counted {
                front.counted = false;
                self.flight_bytes -= front.wire_size();
            }
        }

        match h.ty {
            PacketType::Data => self.on_data(h.seq_nr, pkt.payload),
            PacketType::Fin => self.on_fin(h.seq_nr),
            _ => {}
        }
        if self.fin_acked && self.eof {
            self.state = State::Closed;
            self.rto_deadline = None;
            let linger = (self.rtt.rto() * 2).max(Duration::from_secs(1));
            self.linger_until = Some(now + linger);
        }
    }

    fn process_ack(&mut self, now: Instant, ack: u16, sack: Option<&SelectiveAck>, dup: bool) {
        let valid = match self.in_flight.front() {
            Some(oldest) => seq::le(oldest.seq.wrapping_sub(1), ack) && seq::lt(ack, self.seq_nr),
            None => ack == self.seq_nr.wrapping_sub(1),
        };
        if !valid {
            return;
        }
        let mut acked_bytes = 0;
        let mut sample = None;
        let mut progressed = false;
        while let Some(front) = self.in_flight.front() {
            if !seq::le(front.seq, ack) {
                break;
            }
            let p = self.in_flight.pop_front().expect("front exists");
            progressed = true;
            if p.counted {
                self.flight_bytes -= p.wire_size();
            }
            if !p.sacked {
                acked_bytes += p.wire_size();
            }
            self.flight_payload -= p.payload.len();
            if p.transmissions == 1
                && let Some(t) = p.sent_at
            {
                sample = Some(now.saturating_duration_since(t));
            }
            if p.ty == PacketType::Fin {
                self.fin_acked = true;
            }
        }
        if let Some(sack) = sack {
            let base = ack.wrapping_add(2);
            for p in &mut self.in_flight {
                let i = usize::from(p.seq.wrapping_sub(base));
                if p.sacked || !sack.get(i) || p.transmissions == 0 {
                    continue;
                }
                p.sacked = true;
                p.pending = false;
                if p.counted {
                    p.counted = false;
                    self.flight_bytes -= p.wire_size();
                }
                acked_bytes += p.wire_size();
                if p.transmissions == 1
                    && let Some(t) = p.sent_at
                {
                    sample = Some(now.saturating_duration_since(t));
                }
            }
        }

        if progressed {
            self.dup_acks = 0;
        } else if dup && ack == self.last_ack && !self.in_flight.is_empty() {
            self.dup_acks += 1;
        }
        self.last_ack = ack;

        let mut lost = false;
        if self.dup_acks >= DUP_ACK_THRESHOLD {
            self.dup_acks = 0;
            if let Some(front) = self.in_flight.front_mut()
                && Self::mark_lost(front, &mut self.flight_bytes)
            {
                lost = true;
            }
        }
        if sack.is_some() {
            let mut later_sacked = 0;
            for p in self.in_flight.iter_mut().rev() {
                if p.sacked {
                    later_sacked += 1;
                } else if later_sacked >= DUP_ACK_THRESHOLD
                    && Self::mark_lost(p, &mut self.flight_bytes)
                {
                    lost = true;
                }
            }
        }
        if let Some(end) = self.recovery_end
            && seq::le(end, ack)
        {
            self.recovery_end = None;
        }
        if lost && self.recovery_end.is_none() {
            self.ledbat.on_loss();
            self.recovery_end = Some(self.seq_nr.wrapping_sub(1));
        }

        if let Some(s) = sample {
            self.rtt.sample(s);
        }
        self.ledbat.on_ack(acked_bytes, self.window_limited);
        if progressed {
            self.rto_deadline = (!self.in_flight.is_empty()).then(|| now + self.rtt.rto());
        }
    }

    /// Mark a sent packet for fast retransmission. False when it doesn't qualify.
    fn mark_lost(p: &mut Sent, flight_bytes: &mut usize) -> bool {
        if p.sacked || p.fast_resent || p.pending || p.transmissions == 0 {
            return false;
        }
        p.pending = true;
        p.fast_resent = true;
        if p.counted {
            p.counted = false;
            *flight_bytes -= p.wire_size();
        }
        true
    }

    fn on_data(&mut self, seq_nr: u16, payload: &[u8]) {
        self.ack_needed = true;
        if payload.is_empty() {
            return;
        }
        if let Some(fin) = self.peer_fin
            && !seq::lt(seq_nr, fin)
        {
            return;
        }
        let next = self.ack_nr.wrapping_add(1);
        let Some(ahead) = seq::distance(next, seq_nr) else {
            return; // a duplicate: the ACK queued above answers it
        };
        if ahead >= MAX_REORDER
            || self.reorder.contains_key(&seq_nr)
            || self.readable.len() + self.reorder_bytes + payload.len() > self.cfg.recv_buffer
        {
            return;
        }
        if ahead == 0 {
            self.readable.extend(payload);
            self.ack_nr = seq_nr;
        } else {
            self.reorder_bytes += payload.len();
            self.reorder.insert(seq_nr, payload.to_vec());
        }
        self.advance();
    }

    fn on_fin(&mut self, seq_nr: u16) {
        self.ack_needed = true;
        if self.peer_fin.is_none() && seq::lt(self.ack_nr, seq_nr) {
            self.peer_fin = Some(seq_nr);
        }
        self.advance();
    }

    /// Move buffered packets that are now in order into the readable stream.
    fn advance(&mut self) {
        loop {
            let next = self.ack_nr.wrapping_add(1);
            if let Some(p) = self.reorder.remove(&next) {
                self.reorder_bytes -= p.len();
                self.readable.extend(p);
                self.ack_nr = next;
            } else if self.peer_fin == Some(next) {
                self.ack_nr = next;
                self.eof = true;
                break;
            } else {
                break;
            }
        }
    }

    fn fail(&mut self, e: ConnError) {
        // Everything we sent has been acknowledged except our FIN, and the peer's FIN has
        // arrived: both directions are complete. The peer may simply have forgotten the
        // connection (its linger ended) before our FIN retransmission reached it, so it
        // answers with a reset, or not at all. That's a clean close, not a failure.
        let only_fin_unacked = self.fin_sent
            && self
                .in_flight
                .iter()
                .all(|p| p.ty == PacketType::Fin || p.sacked);
        if self.eof
            && only_fin_unacked
            && matches!(e, ConnError::TimedOut | ConnError::Reset)
            && self.state == State::Connected
        {
            self.fin_acked = true;
            self.in_flight.clear();
            self.flight_bytes = 0;
            self.flight_payload = 0;
            self.state = State::Closed;
            self.rto_deadline = None;
            self.connect_deadline = None;
            self.linger_until = None;
            self.released = true;
            return;
        }
        self.state = State::Failed(e);
        self.rto_deadline = None;
        self.connect_deadline = None;
    }

    /// Abort: queue a RESET and fail with [`ConnError::Reset`].
    pub fn reset(&mut self, now: Instant) -> Option<Vec<u8>> {
        if self.is_finished() {
            return None;
        }
        let mut out = Vec::with_capacity(HEADER_LEN);
        let h = self.header(PacketType::Reset, self.seq_nr, now);
        packet::encode(&h, None, &[], &mut out);
        self.fail(ConnError::Reset);
        Some(out)
    }

    /// The earliest time [`handle_timeout`](Self::handle_timeout) has work to do.
    pub fn poll_timeout(&self) -> Option<Instant> {
        if self.state == State::Closed && !self.released {
            return self.linger_until;
        }
        if self.is_finished() {
            return None;
        }
        let keepalive =
            (self.state == State::Connected).then(|| self.last_send + self.cfg.keepalive);
        [self.rto_deadline, self.connect_deadline, keepalive]
            .into_iter()
            .flatten()
            .min()
    }

    /// Run the timers that are due.
    pub fn handle_timeout(&mut self, now: Instant) {
        if self.state == State::Closed {
            if self.linger_until.is_some_and(|t| now >= t) {
                self.released = true;
            }
            return;
        }
        if self.is_finished() {
            return;
        }
        if let Some(d) = self.connect_deadline
            && now >= d
        {
            self.fail(ConnError::TimedOut);
            return;
        }
        if let Some(d) = self.rto_deadline
            && now >= d
        {
            if self.in_flight.is_empty() {
                self.rto_deadline = None;
                if !self.send_queue.is_empty() || (self.close_requested && !self.fin_sent) {
                    self.probe = true;
                }
            } else {
                self.timeouts += 1;
                if self.state != State::SynSent && self.timeouts > self.cfg.max_timeouts {
                    self.fail(ConnError::TimedOut);
                    return;
                }
                self.rtt.backoff();
                if self.state != State::SynSent {
                    self.ledbat.on_timeout();
                }
                for p in &mut self.in_flight {
                    if p.sacked {
                        continue;
                    }
                    p.pending = true;
                    p.fast_resent = false;
                    if p.counted {
                        p.counted = false;
                        self.flight_bytes -= p.wire_size();
                    }
                }
                self.dup_acks = 0;
                self.recovery_end = None;
                self.rto_deadline = Some(now + self.rtt.rto());
            }
        }
        if self.state == State::Connected && now >= self.last_send + self.cfg.keepalive {
            self.ack_needed = true;
        }
    }

    fn header(&self, ty: PacketType, seq_nr: u16, now: Instant) -> Header {
        Header {
            ty,
            connection_id: if ty == PacketType::Syn {
                self.recv_id
            } else {
                self.send_id
            },
            timestamp: self.micros(now),
            timestamp_diff: self.reply_micro,
            wnd_size: u32::try_from(self.adv_window()).unwrap_or(u32::MAX),
            seq_nr,
            ack_nr: if ty == PacketType::Syn {
                0
            } else {
                self.ack_nr
            },
        }
    }

    fn can_send(&self, size: usize) -> bool {
        let window = self.ledbat.window().min(self.peer_wnd);
        self.flight_bytes + size <= window || (self.flight_bytes == 0 && self.probe)
    }

    /// The next datagram to send, if any. Call until it returns `None`.
    pub fn poll_transmit(&mut self, now: Instant) -> Option<Vec<u8>> {
        match self.state {
            State::Failed(_) => return None,
            State::Closed => return self.ack_needed.then(|| self.send_state(now)),
            State::SynSent | State::Connected => {}
        }
        if self.syn_ack_needed {
            self.syn_ack_needed = false;
            return Some(self.send_state(now));
        }
        // Retransmissions and first sends of queued control packets, oldest first.
        if let Some(i) = self.in_flight.iter().position(|p| p.pending) {
            let size = self.in_flight[i].wire_size();
            if self.state == State::SynSent || self.can_send(size) {
                return Some(self.transmit(i, now));
            }
            self.window_limited = true;
            return self.ack_needed.then(|| self.send_state(now));
        }
        if self.state == State::Connected
            && self.confirmed
            && !self.send_queue.is_empty()
            && self.in_flight.len() < MAX_IN_FLIGHT
        {
            let window = self.ledbat.window().min(self.peer_wnd);
            let room = window
                .saturating_sub(self.flight_bytes)
                .saturating_sub(HEADER_LEN);
            let want = self.send_queue.len().min(self.max_payload());
            // Fill the window, but don't dribble out tiny packets into a nearly full one.
            let n = if want <= room {
                want
            } else if room >= self.max_payload() / 2 {
                room
            } else if self.flight_bytes == 0 && self.probe {
                want
            } else {
                0
            };
            if n > 0 {
                self.window_limited = n < self.send_queue.len();
                let payload: Vec<u8> = self.send_queue.drain(..n).collect();
                return Some(self.push_and_send(PacketType::Data, payload, now));
            }
            self.window_limited = true;
        } else {
            self.window_limited = false;
        }
        if self.state == State::Connected
            && self.confirmed
            && self.close_requested
            && !self.fin_sent
            && self.send_queue.is_empty()
            && self.in_flight.len() < MAX_IN_FLIGHT
        {
            self.fin_sent = true;
            return Some(self.push_and_send(PacketType::Fin, Vec::new(), now));
        }
        self.ack_needed.then(|| self.send_state(now))
    }

    fn push_and_send(&mut self, ty: PacketType, payload: Vec<u8>, now: Instant) -> Vec<u8> {
        self.flight_payload += payload.len();
        self.in_flight.push_back(Sent {
            seq: self.seq_nr,
            ty,
            payload,
            sent_at: None,
            transmissions: 0,
            pending: true,
            sacked: false,
            counted: false,
            fast_resent: false,
        });
        self.seq_nr = self.seq_nr.wrapping_add(1);
        self.transmit(self.in_flight.len() - 1, now)
    }

    fn transmit(&mut self, i: usize, now: Instant) -> Vec<u8> {
        let (ty, seq_nr) = (self.in_flight[i].ty, self.in_flight[i].seq);
        let h = self.header(ty, seq_nr, now);
        let p = &mut self.in_flight[i];
        if p.transmissions > 0 {
            self.packets_retransmitted += 1;
        }
        p.transmissions += 1;
        p.sent_at = Some(now);
        p.pending = false;
        if !p.counted {
            p.counted = true;
            self.flight_bytes += p.wire_size();
        }
        let mut out = Vec::with_capacity(p.wire_size());
        packet::encode(&h, None, &p.payload, &mut out);
        self.probe = false;
        if self.rto_deadline.is_none() {
            self.rto_deadline = Some(now + self.rtt.rto());
        }
        self.after_send(now);
        // A data packet carries our ACK, unless out-of-order data needs a SACK.
        if self.reorder.is_empty() {
            self.ack_needed = false;
        }
        out
    }

    fn send_state(&mut self, now: Instant) -> Vec<u8> {
        self.ack_needed = false;
        let h = self.header(PacketType::State, self.seq_nr, now);
        let sack = self.build_sack();
        let mut out = Vec::with_capacity(HEADER_LEN + 34);
        packet::encode(&h, sack.as_ref(), &[], &mut out);
        self.after_send(now);
        out
    }

    fn after_send(&mut self, now: Instant) {
        self.packets_sent += 1;
        self.last_send = now;
        self.last_adv_wnd = self.adv_window();
    }

    fn build_sack(&self) -> Option<SelectiveAck> {
        if self.reorder.is_empty() {
            return None;
        }
        let base = self.ack_nr.wrapping_add(2);
        let offsets = self
            .reorder
            .keys()
            .map(|&s| usize::from(s.wrapping_sub(base)))
            .filter(|&i| i < packet::MAX_SACK_BYTES * 8);
        let mut sack = SelectiveAck::with_capacity(offsets.clone().max()? + 1);
        for i in offsets {
            sack.set(i);
        }
        Some(sack)
    }
}
