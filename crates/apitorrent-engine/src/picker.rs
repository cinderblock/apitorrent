//! Deciding which blocks to request from which peer.
//!
//! The picker is pure state with no I/O, so its behaviour is deterministic for a given seed
//! and can be tested exhaustively.
//!
//! Order of preference:
//!
//! 1. Blocks of pieces already in progress (most complete first), to finish pieces quickly.
//!    A finished piece can be verified, written and shared, and frees its buffer.
//! 2. New pieces, highest priority first, then rarest first. Within equal rarity the order
//!    is random, so peers spread out over the swarm. Sequential mode replaces this with
//!    index order, for streaming.
//! 3. Endgame: once every missing block is requested, blocks already requested from slow
//!    peers are requested again from others, at most [`ENDGAME_DUPLICATES`] times.
//!
//! A peer on parole (suspected of sending bad data) is picked *exclusively*: it only gets
//! whole pieces that nobody else downloads, so if one fails it is clear who sent it.
//!
//! New pieces come from `order`, which holds every wanted piece that is neither complete
//! nor in progress, sorted by `(priority, availability)`. It is split into buckets per key,
//! so an availability change moves a piece one bucket with a single swap.

use std::collections::HashMap;

use rand::SeedableRng;
use rand::rngs::StdRng;
use rand::seq::SliceRandom;

use apitorrent_wire::BlockRef;

use crate::bitfield::Bitfield;
use crate::layout::{BLOCK_LEN, Layout};

/// Identifies a peer connection to the picker.
pub type PeerKey = u64;

/// Priorities run from 0 (don't download) to `MAX_PRIORITY`. The default is 4.
pub const MAX_PRIORITY: u8 = 7;
pub const DEFAULT_PRIORITY: u8 = 4;

/// How many peers may have the same block outstanding in endgame.
pub const ENDGAME_DUPLICATES: usize = 2;

/// Availability above this value is ordered as if equal. Rarity only matters at the low
/// end, and the cap keeps the bucket count small.
const AVAILABILITY_CAP: u32 = 255;
const KEYS_PER_PRIORITY: u32 = AVAILABILITY_CAP + 1;
const KEY_COUNT: usize = (MAX_PRIORITY as u32 * KEYS_PER_PRIORITY) as usize;
const NOT_IN_ORDER: u32 = u32::MAX;

#[derive(Debug, Clone, PartialEq, Eq)]
enum Block {
    Missing,
    Requested(Vec<PeerKey>),
    Received,
}

#[derive(Debug, Clone)]
struct Partial {
    blocks: Vec<Block>,
    received: u32,
    /// The only peer allowed to supply this piece, for a peer on parole.
    owner: Option<PeerKey>,
}

impl Partial {
    fn missing(&self) -> bool {
        self.blocks.iter().any(|b| matches!(b, Block::Missing))
    }
}

/// What happened when a block arrived.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Received {
    /// The block was new. `cancel` lists other peers it was also requested from (endgame),
    /// which should be sent a `cancel`. `piece_complete` means every block of the piece is
    /// now in, and the piece can be verified.
    New {
        cancel: Vec<PeerKey>,
        piece_complete: bool,
    },
    /// We already had it, or never asked for it. Drop the data.
    Unwanted,
}

#[derive(Debug, Clone)]
pub struct Picker {
    layout: Layout,
    have: Bitfield,
    priority: Vec<u8>,
    availability: Vec<u32>,
    seeds: u32,
    partial: HashMap<u32, Partial>,
    order: Vec<u32>,
    pos: Vec<u32>,
    bucket_start: Vec<u32>,
    sequential: bool,
    rng: StdRng,
    /// Wanted pieces that may not be started yet (their hashes haven't arrived).
    held: Bitfield,
    /// Pieces with priority above 0, and how many of those we have.
    wanted_total: u32,
    wanted_have: u32,
}

impl Picker {
    /// A picker for a torrent where we already have the pieces in `have`.
    pub fn new(layout: Layout, have: Bitfield, seed: u64) -> Self {
        let n = layout.piece_count();
        assert_eq!(have.len(), n);
        let mut p = Self {
            layout,
            have,
            priority: vec![DEFAULT_PRIORITY; n as usize],
            availability: vec![0; n as usize],
            seeds: 0,
            partial: HashMap::new(),
            order: Vec::new(),
            pos: vec![NOT_IN_ORDER; n as usize],
            bucket_start: vec![0; KEY_COUNT + 1],
            sequential: false,
            rng: StdRng::seed_from_u64(seed),
            held: Bitfield::new(n),
            wanted_total: 0,
            wanted_have: 0,
        };
        p.rebuild_order();
        p.recount();
        p
    }

    pub fn have(&self) -> &Bitfield {
        &self.have
    }

    pub fn layout(&self) -> &Layout {
        &self.layout
    }

    pub fn set_sequential(&mut self, on: bool) {
        self.sequential = on;
    }

    /// Whether we still want piece `piece`.
    pub fn is_wanted(&self, piece: u32) -> bool {
        !self.have.get(piece) && self.priority[piece as usize] > 0
    }

    /// Whether every piece with non-zero priority is complete.
    pub fn is_finished(&self) -> bool {
        self.wanted_have == self.wanted_total
    }

    /// Pieces with priority above 0, and how many of them we have.
    pub fn wanted(&self) -> (u32, u32) {
        (self.wanted_total, self.wanted_have)
    }

    fn recount(&mut self) {
        let (mut total, mut have) = (0, 0);
        for p in 0..self.layout.piece_count() {
            if self.priority[p as usize] > 0 {
                total += 1;
                have += u32::from(self.have.get(p));
            }
        }
        self.wanted_total = total;
        self.wanted_have = have;
    }

    /// Record that `piece` is now had.
    fn gained(&mut self, piece: u32) {
        if self.have.set(piece) && self.priority[piece as usize] > 0 {
            self.wanted_have += 1;
        }
    }

    /// Whether a peer with `has` has anything we want.
    pub fn is_interesting(&self, has: &Bitfield) -> bool {
        has.iter_ones().any(|p| self.is_wanted(p))
    }

    pub fn availability(&self, piece: u32) -> u32 {
        self.availability[piece as usize] + self.seeds
    }

    pub fn priority(&self, piece: u32) -> u8 {
        self.priority[piece as usize]
    }

    pub fn set_priority(&mut self, piece: u32, prio: u8) {
        let prio = prio.min(MAX_PRIORITY);
        if self.priority[piece as usize] == prio {
            return;
        }
        self.remove_from_order(piece);
        let was_wanted = self.priority[piece as usize] > 0;
        self.priority[piece as usize] = prio;
        let had = u32::from(self.have.get(piece));
        match (was_wanted, prio > 0) {
            (false, true) => {
                self.wanted_total += 1;
                self.wanted_have += had;
            }
            (true, false) => {
                self.wanted_total -= 1;
                self.wanted_have -= had;
            }
            _ => {}
        }
        self.insert_into_order(piece);
    }

    /// Set many priorities at once; cheaper than repeated [`Picker::set_priority`].
    pub fn set_priorities(&mut self, prios: &[u8]) {
        assert_eq!(prios.len(), self.priority.len());
        for (p, &v) in self.priority.iter_mut().zip(prios) {
            *p = v.min(MAX_PRIORITY);
        }
        self.rebuild_order();
        self.recount();
    }

    // ----- availability -------------------------------------------------------------

    pub fn peer_seed_added(&mut self) {
        self.seeds += 1;
    }

    pub fn peer_seed_removed(&mut self) {
        self.seeds = self.seeds.saturating_sub(1);
    }

    pub fn peer_has(&mut self, piece: u32) {
        self.availability[piece as usize] += 1;
        let avail = self.availability[piece as usize];
        if self.pos[piece as usize] != NOT_IN_ORDER && avail <= AVAILABILITY_CAP {
            let key = self.key(piece);
            self.move_up(piece, key - 1);
        }
    }

    pub fn peer_lost(&mut self, piece: u32) {
        let avail = self.availability[piece as usize];
        if avail == 0 {
            return;
        }
        self.availability[piece as usize] -= 1;
        if self.pos[piece as usize] != NOT_IN_ORDER && avail <= AVAILABILITY_CAP {
            let key = self.key(piece);
            self.move_down(piece, key + 1);
        }
    }

    pub fn peer_bitfield(&mut self, has: &Bitfield) {
        for p in has.iter_ones() {
            self.peer_has(p);
        }
    }

    pub fn peer_bitfield_lost(&mut self, has: &Bitfield) {
        for p in has.iter_ones() {
            self.peer_lost(p);
        }
    }

    // ----- picking --------------------------------------------------------------------

    /// Choose up to `max` blocks to request from `peer`, which has the pieces in `has`,
    /// starting at most `max_new` pieces that aren't in progress yet (the memory budget).
    pub fn pick(
        &mut self,
        peer: PeerKey,
        has: &Bitfield,
        max: usize,
        max_new: usize,
    ) -> Vec<BlockRef> {
        self.pick_inner(peer, has, max, max_new, false)
    }

    /// Like [`Picker::pick`], for a peer on parole: it gets only whole pieces of its own,
    /// which no other peer will be asked for, and takes no part in endgame.
    pub fn pick_exclusive(
        &mut self,
        peer: PeerKey,
        has: &Bitfield,
        max: usize,
        max_new: usize,
    ) -> Vec<BlockRef> {
        self.pick_inner(peer, has, max, max_new, true)
    }

    fn pick_inner(
        &mut self,
        peer: PeerKey,
        has: &Bitfield,
        max: usize,
        max_new: usize,
        exclusive: bool,
    ) -> Vec<BlockRef> {
        let mut out = Vec::new();
        if max == 0 {
            return out;
        }
        let allowed = |part: &Partial| match part.owner {
            Some(owner) => owner == peer,
            None => !exclusive,
        };

        // 1. Pieces in progress, most complete first.
        let mut partials: Vec<(u32, u32)> = self
            .partial
            .iter()
            .filter(|(p, part)| has.get(**p) && part.missing() && allowed(part))
            .map(|(p, part)| (*p, part.received))
            .collect();
        partials.sort_unstable_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        for (piece, _) in partials {
            self.take_missing(piece, peer, max, &mut out);
            if out.len() >= max {
                return out;
            }
        }

        // 2. New pieces.
        let mut started = 0;
        while out.len() < max && started < max_new {
            let Some(piece) = self.next_fresh(has) else {
                break;
            };
            self.start_piece(piece);
            if exclusive {
                self.partial.get_mut(&piece).expect("just started").owner = Some(peer);
            }
            started += 1;
            self.take_missing(piece, peer, max, &mut out);
        }
        if out.len() >= max || self.has_fresh() || exclusive {
            return out;
        }

        // 3. Endgame.
        let mut candidates: Vec<(u32, u32)> = self
            .partial
            .iter()
            .filter(|(p, part)| has.get(**p) && part.owner.is_none())
            .map(|(p, part)| (*p, part.received))
            .collect();
        candidates.sort_unstable_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        let layout = &self.layout;
        for (piece, _) in candidates {
            let part = self.partial.get_mut(&piece).expect("partial exists");
            for (i, block) in part.blocks.iter_mut().enumerate() {
                if out.len() >= max {
                    return out;
                }
                if let Block::Requested(peers) = block
                    && peers.len() < ENDGAME_DUPLICATES
                    && !peers.contains(&peer)
                {
                    peers.push(peer);
                    out.push(block_ref(layout, piece, u32::try_from(i).expect("fits")));
                }
            }
        }
        out
    }

    fn take_missing(&mut self, piece: u32, peer: PeerKey, max: usize, out: &mut Vec<BlockRef>) {
        let mut taken = Vec::new();
        {
            let part = self.partial.get_mut(&piece).expect("partial exists");
            for (i, block) in part.blocks.iter_mut().enumerate() {
                if out.len() + taken.len() >= max {
                    break;
                }
                if *block == Block::Missing {
                    *block = Block::Requested(vec![peer]);
                    taken.push(u32::try_from(i).expect("fits"));
                }
            }
        }
        out.extend(taken.into_iter().map(|b| block_ref(&self.layout, piece, b)));
    }

    fn has_fresh(&self) -> bool {
        if self.sequential {
            (0..self.layout.piece_count())
                .any(|p| self.is_wanted(p) && !self.partial.contains_key(&p) && !self.held.get(p))
        } else {
            !self.order.is_empty()
        }
    }

    fn next_fresh(&self, has: &Bitfield) -> Option<u32> {
        if self.sequential {
            return (0..self.layout.piece_count()).find(|&p| {
                self.is_wanted(p)
                    && !self.partial.contains_key(&p)
                    && !self.held.get(p)
                    && has.get(p)
            });
        }
        self.order.iter().copied().find(|&p| has.get(p))
    }

    fn start_piece(&mut self, piece: u32) {
        self.remove_from_order(piece);
        let blocks = self.layout.blocks_in_piece(piece) as usize;
        self.partial.insert(
            piece,
            Partial {
                blocks: vec![Block::Missing; blocks],
                received: 0,
                owner: None,
            },
        );
    }

    // ----- results ----------------------------------------------------------------------

    pub fn block_received(&mut self, peer: PeerKey, block: &BlockRef) -> Received {
        let Some(part) = self.partial.get_mut(&block.piece) else {
            return Received::Unwanted;
        };
        if !block.offset.is_multiple_of(BLOCK_LEN) {
            return Received::Unwanted;
        }
        let index = (block.offset / BLOCK_LEN) as usize;
        let Some(slot) = part.blocks.get_mut(index) else {
            return Received::Unwanted;
        };
        let cancel = match std::mem::replace(slot, Block::Received) {
            Block::Received => return Received::Unwanted,
            Block::Missing => Vec::new(),
            Block::Requested(peers) => peers.into_iter().filter(|&p| p != peer).collect(),
        };
        part.received += 1;
        let piece_complete = part.received as usize == part.blocks.len();
        Received::New {
            cancel,
            piece_complete,
        }
    }

    /// A request will not be answered: the peer rejected it, choked us, or disconnected.
    pub fn request_failed(&mut self, peer: PeerKey, block: &BlockRef) {
        let Some(part) = self.partial.get_mut(&block.piece) else {
            return;
        };
        let Some(slot) = part.blocks.get_mut((block.offset / BLOCK_LEN) as usize) else {
            return;
        };
        if let Block::Requested(peers) = slot {
            peers.retain(|&p| p != peer);
            if peers.is_empty() {
                *slot = Block::Missing;
            }
        }
    }

    /// Forget every outstanding request to `peer`, and release the pieces it owned.
    pub fn peer_gone(&mut self, peer: PeerKey) {
        for part in self.partial.values_mut() {
            if part.owner == Some(peer) {
                part.owner = None;
            }
            for slot in &mut part.blocks {
                if let Block::Requested(peers) = slot {
                    peers.retain(|&p| p != peer);
                    if peers.is_empty() {
                        *slot = Block::Missing;
                    }
                }
            }
        }
    }

    /// The result of hashing a completed piece.
    pub fn piece_verified(&mut self, piece: u32, valid: bool) {
        self.partial.remove(&piece);
        if valid {
            self.gained(piece);
        } else {
            self.insert_into_order(piece);
        }
    }

    /// Mark a piece as had without downloading it (resume data, a recheck, or a file
    /// matched from the content library).
    pub fn mark_have(&mut self, piece: u32) {
        self.partial.remove(&piece);
        self.remove_from_order(piece);
        self.gained(piece);
    }

    /// Drop a piece we had (it failed a recheck).
    pub fn mark_missing(&mut self, piece: u32) {
        if self.have.clear(piece) {
            if self.priority[piece as usize] > 0 {
                self.wanted_have -= 1;
            }
            self.insert_into_order(piece);
        }
    }

    /// Hold a wanted piece back from being started (or release it). A held piece still
    /// counts as wanted, so the torrent isn't finished while it is missing.
    pub fn set_held(&mut self, piece: u32, held: bool) {
        if held {
            if self.held.set(piece) {
                self.remove_from_order(piece);
            }
        } else if self.held.clear(piece) {
            self.insert_into_order(piece);
        }
    }

    /// Pieces started and not yet verified: being downloaded, or waiting to be hashed and
    /// written. Each holds a piece buffer.
    pub fn pieces_in_progress(&self) -> usize {
        self.partial.len()
    }

    // ----- ordering internals -----------------------------------------------------------

    fn in_order_set(&self, piece: u32) -> bool {
        self.is_wanted(piece) && !self.partial.contains_key(&piece) && !self.held.get(piece)
    }

    fn key(&self, piece: u32) -> u32 {
        let prio = u32::from(self.priority[piece as usize]);
        debug_assert!(prio > 0);
        let avail = self.availability[piece as usize].min(AVAILABILITY_CAP);
        (u32::from(MAX_PRIORITY) - prio) * KEYS_PER_PRIORITY + avail
    }

    fn rebuild_order(&mut self) {
        let n = self.layout.piece_count();
        let mut pieces: Vec<u32> = (0..n).filter(|&p| self.in_order_set(p)).collect();
        pieces.shuffle(&mut self.rng);
        pieces.sort_by_key(|&p| self.key(p));
        self.pos.fill(NOT_IN_ORDER);
        self.bucket_start.fill(0);
        let mut counts = vec![0u32; KEY_COUNT];
        for (i, &p) in pieces.iter().enumerate() {
            self.pos[p as usize] = u32::try_from(i).expect("fits");
            counts[self.key(p) as usize] += 1;
        }
        let mut acc = 0;
        for (k, c) in counts.iter().enumerate() {
            self.bucket_start[k] = acc;
            acc += c;
        }
        self.bucket_start[KEY_COUNT] = acc;
        self.order = pieces;
    }

    fn place(&mut self, at: u32, piece: u32) {
        self.order[at as usize] = piece;
        self.pos[piece as usize] = at;
    }

    /// `piece` moved from bucket `old` to `old + 1`.
    fn move_up(&mut self, piece: u32, old: u32) {
        let old = old as usize;
        let last = self.bucket_start[old + 1] - 1;
        let at = self.pos[piece as usize];
        let other = self.order[last as usize];
        self.place(at, other);
        self.place(last, piece);
        self.bucket_start[old + 1] -= 1;
    }

    /// `piece` moved from bucket `old` to `old - 1`.
    fn move_down(&mut self, piece: u32, old: u32) {
        let old = old as usize;
        let first = self.bucket_start[old];
        let at = self.pos[piece as usize];
        let other = self.order[first as usize];
        self.place(at, other);
        self.place(first, piece);
        self.bucket_start[old] += 1;
    }

    fn insert_into_order(&mut self, piece: u32) {
        if self.pos[piece as usize] != NOT_IN_ORDER || !self.in_order_set(piece) {
            return;
        }
        let key = self.key(piece) as usize;
        let mut hole = u32::try_from(self.order.len()).expect("fits");
        self.order.push(piece);
        // Shift the first element of each later bucket to its end, opening a slot at the
        // end of bucket `key`.
        for b in (key + 1..KEY_COUNT).rev() {
            let first = self.bucket_start[b];
            if first < hole {
                let moved = self.order[first as usize];
                self.place(hole, moved);
                hole = first;
            }
            self.bucket_start[b] += 1;
        }
        self.bucket_start[KEY_COUNT] = u32::try_from(self.order.len()).expect("fits");
        self.place(hole, piece);
    }

    fn remove_from_order(&mut self, piece: u32) {
        let at = self.pos[piece as usize];
        if at == NOT_IN_ORDER {
            return;
        }
        let key = self.key_for_position(at);
        // Fill the hole from the end of its bucket, then pull each later bucket's last
        // element back into the gap left at its start.
        let mut hole = at;
        let last = self.bucket_start[key + 1] - 1;
        if last != hole {
            let moved = self.order[last as usize];
            self.place(hole, moved);
            hole = last;
        }
        for b in key + 1..KEY_COUNT {
            self.bucket_start[b] -= 1;
            let last = self.bucket_start[b + 1] - 1;
            if last != hole && last > hole {
                let moved = self.order[last as usize];
                self.place(hole, moved);
                hole = last;
            }
        }
        self.bucket_start[KEY_COUNT] -= 1;
        debug_assert_eq!(hole as usize, self.order.len() - 1);
        self.order.pop();
        self.pos[piece as usize] = NOT_IN_ORDER;
    }

    /// The bucket holding order position `at`. Priority changes can make `key()` stale for a
    /// piece still in the order, so removal uses the position instead.
    fn key_for_position(&self, at: u32) -> usize {
        self.bucket_start.partition_point(|&s| s <= at) - 1
    }

    #[cfg(test)]
    fn check_invariants(&self) {
        for (i, &p) in self.order.iter().enumerate() {
            assert_eq!(self.pos[p as usize] as usize, i);
            assert!(self.in_order_set(p), "piece {p} should not be in order");
            let k = self.key(p) as usize;
            assert!(
                self.bucket_start[k] as usize <= i && i < self.bucket_start[k + 1] as usize,
                "piece {p} at {i} outside bucket {k}"
            );
        }
        let expected = (0..self.layout.piece_count())
            .filter(|&p| self.in_order_set(p))
            .count();
        assert_eq!(self.order.len(), expected);
    }
}

fn block_ref(layout: &Layout, piece: u32, block: u32) -> BlockRef {
    BlockRef {
        piece,
        offset: block * BLOCK_LEN,
        length: layout.block_size(piece, block),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use apitorrent_metainfo::{FileSource, Torrent, TorrentBuilder, Version};

    fn layout(pieces: u32, piece_len: u64) -> Layout {
        let bytes = TorrentBuilder::new("t", Version::V1)
            .piece_length(piece_len)
            .file(FileSource::from_bytes(
                &[],
                vec![0; usize::try_from(u64::from(pieces) * piece_len - 100).unwrap()],
            ))
            .build()
            .unwrap();
        Layout::from_info(&Torrent::from_bytes(&bytes).unwrap().info)
    }

    fn picker(pieces: u32) -> Picker {
        Picker::new(layout(pieces, 32 * 1024), Bitfield::new(pieces), 7)
    }

    fn has(n: u32, pieces: &[u32]) -> Bitfield {
        let mut b = Bitfield::new(n);
        for &p in pieces {
            b.set(p);
        }
        b
    }

    #[test]
    fn picks_rarest_first() {
        let mut p = picker(4);
        p.peer_bitfield(&Bitfield::full(4));
        p.peer_bitfield(&has(4, &[0, 1, 3]));
        p.peer_bitfield(&has(4, &[0, 1]));
        p.check_invariants();
        // Piece 2 has availability 1, piece 3 has 2, pieces 0 and 1 have 3.
        let blocks = p.pick(1, &Bitfield::full(4), 2, usize::MAX);
        assert!(blocks.iter().all(|b| b.piece == 2), "{blocks:?}");
        let blocks = p.pick(1, &Bitfield::full(4), 2, usize::MAX);
        assert!(blocks.iter().all(|b| b.piece == 3), "{blocks:?}");
        p.check_invariants();
    }

    #[test]
    fn finishes_partial_pieces_before_starting_new_ones() {
        let mut p = picker(4);
        let first = p.pick(1, &Bitfield::full(4), 1, usize::MAX);
        let piece = first[0].piece;
        let second = p.pick(2, &Bitfield::full(4), 1, usize::MAX);
        assert_eq!(second[0].piece, piece);
        assert_ne!(second[0].offset, first[0].offset);
    }

    /// With no room for new pieces, only pieces in progress are picked, and once those are
    /// fully requested the peer gets nothing more.
    #[test]
    fn max_new_caps_pieces_started() {
        let mut p = picker(4);
        let all = Bitfield::full(4);
        let blocks = p.pick(1, &all, 100, 1);
        let pieces: std::collections::HashSet<u32> = blocks.iter().map(|b| b.piece).collect();
        assert_eq!(pieces.len(), 1, "only one piece started: {blocks:?}");
        assert_eq!(p.pieces_in_progress(), 1);
        assert!(
            p.pick(2, &all, 100, 0).is_empty(),
            "no room and nothing missing"
        );
        p.request_failed(1, &blocks[0]);
        let again = p.pick(2, &all, 100, 0);
        assert_eq!(
            again,
            [blocks[0]],
            "a missing block of a started piece is still picked"
        );
    }

    #[test]
    fn held_pieces_stay_wanted_but_are_not_picked() {
        let mut p = picker(3);
        let all = Bitfield::full(3);
        p.set_held(0, true);
        p.set_held(1, true);
        p.check_invariants();
        let blocks = p.pick(1, &all, 1000, usize::MAX);
        assert!(blocks.iter().all(|b| b.piece == 2), "{blocks:?}");
        assert!(!p.is_finished());
        assert_eq!(p.wanted(), (3, 0));
        p.set_held(0, false);
        let blocks = p.pick(2, &all, 1000, usize::MAX);
        assert!(blocks.iter().any(|b| b.piece == 0), "{blocks:?}");
        assert!(blocks.iter().all(|b| b.piece != 1), "{blocks:?}");
        p.check_invariants();
    }

    #[test]
    fn wanted_counts_track_priorities_and_pieces() {
        let mut p = picker(4);
        assert_eq!(p.wanted(), (4, 0));
        p.mark_have(1);
        assert_eq!(p.wanted(), (4, 1));
        p.set_priority(1, 0);
        p.set_priority(2, 0);
        assert_eq!(p.wanted(), (2, 0));
        p.set_priority(1, 4);
        assert_eq!(p.wanted(), (3, 1));
        p.mark_missing(1);
        assert_eq!(p.wanted(), (3, 0));
        p.set_priorities(&[0, 0, 0, 7]);
        assert_eq!(p.wanted(), (1, 0));
        assert!(!p.is_finished());
        p.mark_have(3);
        assert!(p.is_finished());
    }

    /// A paroled peer's pieces are its own: nobody else is asked for them, and it isn't
    /// asked for anyone else's, even in endgame.
    #[test]
    fn exclusive_pieces_are_not_shared() {
        let mut p = picker(2);
        let all = Bitfield::full(2);
        let mine = p.pick_exclusive(1, &all, 100, 1);
        let piece = mine[0].piece;
        assert!(mine.iter().all(|b| b.piece == piece), "one whole piece");
        p.request_failed(1, &mine[0]);
        let other = p.pick(2, &all, 100, usize::MAX);
        assert!(
            other.iter().all(|b| b.piece != piece),
            "others don't touch it: {other:?}"
        );
        // Everything is requested now; endgame for peer 3 must skip the owned piece.
        let dup = p.pick(3, &all, 100, usize::MAX);
        assert!(dup.iter().all(|b| b.piece != piece), "{dup:?}");
        // Once the owner leaves, the piece is open to everyone.
        p.peer_gone(1);
        let freed = p.pick(2, &all, 100, usize::MAX);
        assert!(freed.iter().any(|b| b.piece == piece), "{freed:?}");
    }

    #[test]
    fn priorities_win_over_rarity() {
        let mut p = picker(4);
        p.peer_bitfield(&has(4, &[0, 1, 2]));
        p.set_priority(3, 1);
        p.set_priority(0, MAX_PRIORITY);
        p.check_invariants();
        let blocks = p.pick(1, &Bitfield::full(4), 1, usize::MAX);
        assert_eq!(blocks[0].piece, 0);
        p.set_priority(1, 0);
        p.check_invariants();
        assert!(!p.is_wanted(1));
    }

    #[test]
    fn full_download_then_endgame() {
        let n = 3;
        let mut p = picker(n);
        let all = Bitfield::full(n);
        let mut requested = Vec::new();
        loop {
            let got = p.pick(1, &all, 4, usize::MAX);
            if got.is_empty() {
                break;
            }
            requested.extend(got);
        }
        // 3 pieces of 2 blocks each.
        assert_eq!(requested.len(), 6);
        // Peer 2 now gets endgame duplicates.
        let dup = p.pick(2, &all, 10, usize::MAX);
        assert_eq!(dup.len(), 6);
        // And a third peer gets nothing: the duplicate limit is reached.
        assert_eq!(
            p.pick(3, &all, 10, usize::MAX),
            [] as [apitorrent_wire::BlockRef; 0]
        );

        for b in &requested {
            match p.block_received(1, b) {
                Received::New {
                    cancel,
                    piece_complete,
                } => {
                    assert_eq!(cancel, [2]);
                    if piece_complete {
                        p.piece_verified(b.piece, true);
                    }
                }
                Received::Unwanted => panic!("block should be new"),
            }
        }
        assert!(p.is_finished());
        assert_eq!(p.block_received(2, &requested[0]), Received::Unwanted);
    }

    #[test]
    fn failed_piece_is_downloaded_again() {
        let mut p = picker(1);
        let all = Bitfield::full(1);
        let blocks = p.pick(1, &all, 10, usize::MAX);
        for b in &blocks {
            p.block_received(1, b);
        }
        p.piece_verified(0, false);
        p.check_invariants();
        assert_eq!(p.pick(2, &all, 10, usize::MAX).len(), blocks.len());
    }

    #[test]
    fn disconnect_releases_requests() {
        let mut p = picker(1);
        let all = Bitfield::full(1);
        let blocks = p.pick(1, &all, 10, usize::MAX);
        p.peer_gone(1);
        assert_eq!(p.pick(2, &all, 10, usize::MAX), blocks);
    }

    #[test]
    fn sequential_mode() {
        let mut p = picker(5);
        p.set_sequential(true);
        p.peer_bitfield(&has(5, &[4]));
        let blocks = p.pick(1, &Bitfield::full(5), 1, usize::MAX);
        assert_eq!(blocks[0].piece, 0);
    }

    #[test]
    fn ordering_survives_random_churn() {
        use rand::Rng;
        let n = 64;
        let mut p = picker(n);
        let mut rng = StdRng::seed_from_u64(99);
        for _ in 0..5_000 {
            let piece = rng.random_range(0..n);
            match rng.random_range(0..5) {
                0 => p.peer_has(piece),
                1 => p.peer_lost(piece),
                2 => p.set_priority(piece, rng.random_range(0..=MAX_PRIORITY)),
                3 => p.mark_have(piece),
                _ => p.mark_missing(piece),
            }
            p.check_invariants();
        }
    }
}
