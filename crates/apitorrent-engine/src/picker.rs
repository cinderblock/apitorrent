//! Deciding which blocks to request from which peer.
//!
//! The picker is pure state with no I/O, so its behaviour is deterministic for a given seed
//! and can be tested exhaustively.
//!
//! Order of preference:
//!
//! 1. Blocks of pieces already in progress, to finish pieces quickly. A finished piece can
//!    be verified, written and shared, and frees its buffer. The piece a stream's reader
//!    is waiting for goes first, then the most complete.
//! 2. New pieces, highest priority first. Within a priority, files in order (when a file
//!    order is set): the first unfinished file's pieces, then the second's, and so on.
//!    Then rarest first; within equal rarity the order is random, so peers spread out over
//!    the swarm. A stream (a file read while it downloads) takes its pieces in read order
//!    instead of rarest first.
//! 3. Endgame: once every missing block is requested, blocks already requested from slow
//!    peers are requested again from others, at most [`ENDGAME_DUPLICATES`] times.
//!
//! A peer on parole (suspected of sending bad data) is picked *exclusively*: it only gets
//! whole pieces that nobody else downloads, so if one fails it is clear who sent it.
//!
//! New pieces come from `order`, which holds every wanted piece that is neither complete
//! nor in progress, sorted by `(priority, tier, availability)`. It is split into buckets per
//! key, so an availability change moves a piece one bucket with a single swap. A piece's
//! tier is the place of its earliest file among the unfinished files of the file order,
//! capped at `TIERS - 1`; without a file order every tier is 0.
//!
//! With first and last pieces first on, the first and last piece of every wanted file
//! come before the rest of their priority, so a file can be previewed (a player reads a
//! container's header and often its index at the end) before it is complete.
//!
//! The file order is a strong preference, not a rule: a peer that has none of the first
//! file's missing pieces is still asked for the next file's, and so on.

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
/// How many places of the file order are told apart. The files after the first
/// `TIERS - 1` unfinished ones share the last tier; as files finish, later ones move up.
const TIERS: u32 = 8;
const KEYS_PER_TIER: u32 = AVAILABILITY_CAP + 1;
const KEYS_PER_PRIORITY: u32 = TIERS * KEYS_PER_TIER;
const KEY_COUNT: usize = (MAX_PRIORITY as u32 * KEYS_PER_PRIORITY) as usize;
const NOT_IN_ORDER: u32 = u32::MAX;

/// A stream fetches the start and the end of its file before anything else: players read
/// a container's header first, and many formats keep their index at the end (an MP4
/// `moov` box written last, Matroska cues).
const STREAM_HEAD: u64 = 1024 * 1024;
/// How much of the end of a streamed file is fetched first: this fraction of the file,
/// within the bounds below.
const STREAM_TAIL_DIVISOR: u64 = 200;
const STREAM_TAIL_MIN: u64 = 2 * 1024 * 1024;
const STREAM_TAIL_MAX: u64 = 16 * 1024 * 1024;

/// A file read while it downloads: its pieces are fetched in read order.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Stream {
    file: usize,
    /// The byte in the file the reader needs next (its playhead).
    from: u64,
}

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

#[derive(Debug)]
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
    /// The file order (first first), or `None` for no preference between files.
    file_order: Option<Vec<usize>>,
    /// Each piece's tier (see the module docs).
    tier: Vec<u8>,
    /// Per file, how many of its pieces we lack. A file with none missing is complete.
    file_missing: Vec<u32>,
    /// Files that became complete since [`Picker::take_completed_files`] last ran.
    completed: Vec<usize>,
    streams: Vec<Stream>,
    /// Fetch each wanted file's first and last piece before the rest of its priority.
    first_last: bool,
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
            file_order: None,
            tier: vec![0; n as usize],
            file_missing: Vec::new(),
            completed: Vec::new(),
            streams: Vec::new(),
            first_last: false,
            rng: StdRng::seed_from_u64(seed),
            held: Bitfield::new(n),
            wanted_total: 0,
            wanted_have: 0,
        };
        p.file_missing = (0..p.layout.files().len())
            .map(|f| {
                let missing = p.layout.file_pieces(f).filter(|&q| !p.have.get(q)).count();
                u32::try_from(missing).expect("fits")
            })
            .collect();
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

    /// Prefer files in this order (first first) over rarity, among pieces of the same
    /// priority. Files left out come after those listed. `None` turns the preference off.
    pub fn set_file_order(&mut self, order: Option<Vec<usize>>) {
        let files = self.layout.files().len();
        let order = order.map(|o| o.into_iter().filter(|&f| f < files).collect::<Vec<_>>());
        if self.file_order == order {
            return;
        }
        self.file_order = order;
        self.retier();
    }

    /// Read file `file` while it downloads, needing byte `from` next: its pieces are
    /// fetched in read order, from `from` to the end, then from the start up to `from`
    /// (after its first and last few pieces, see [`STREAM_HEAD`]). Moving `from` (a player
    /// seeking) moves where fetching continues. `None` stops streaming the file.
    pub fn set_stream(&mut self, file: usize, from: Option<u64>) {
        if file >= self.layout.files().len() {
            return;
        }
        self.streams.retain(|s| s.file != file);
        if let Some(from) = from {
            let from = from.min(self.layout.files()[file].length.saturating_sub(1));
            self.streams.push(Stream { file, from });
        }
    }

    /// Fetch each wanted file's first and last piece before the rest of its priority, for
    /// previews.
    pub fn set_first_last(&mut self, on: bool) {
        self.first_last = on;
    }

    pub fn first_last(&self) -> bool {
        self.first_last
    }

    /// The files being streamed, and the byte each needs next.
    pub fn streams(&self) -> Vec<(usize, u64)> {
        self.streams.iter().map(|s| (s.file, s.from)).collect()
    }

    /// Whether we have every piece holding a byte of file `file`.
    pub fn file_complete(&self, file: usize) -> bool {
        self.file_missing.get(file).is_some_and(|&m| m == 0)
    }

    /// Files that became complete since the last call (every piece holding any of their
    /// bytes is verified), oldest first. Files complete from the start aren't listed.
    pub fn take_completed_files(&mut self) -> Vec<usize> {
        std::mem::take(&mut self.completed)
    }

    /// The files holding any byte of `piece`, pad and empty files left out.
    fn files_of(&self, piece: u32) -> Vec<usize> {
        let start = self.layout.piece_offset(piece);
        let end = start + u64::from(self.layout.piece_size(piece));
        let files = self.layout.files();
        let first = files.partition_point(|f| f.offset + f.length <= start);
        (first..files.len())
            .take_while(|&f| files[f].offset < end)
            .filter(|&f| !files[f].pad && files[f].length > 0)
            .collect()
    }

    /// Recompute every piece's tier from the file order, and re-sort.
    fn retier(&mut self) {
        self.tier.fill(0);
        if let Some(order) = &self.file_order {
            let last = u8::try_from(TIERS - 1).expect("fits");
            self.tier.fill(last);
            let open = order.iter().filter(|&&f| self.file_missing[f] > 0);
            for (place, &f) in open.take(usize::from(last)).enumerate() {
                let place = u8::try_from(place).expect("fits");
                for p in self.layout.file_pieces(f) {
                    let t = &mut self.tier[p as usize];
                    *t = (*t).min(place);
                }
            }
        }
        self.rebuild_order();
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
        if !self.have.set(piece) {
            return;
        }
        if self.priority[piece as usize] > 0 {
            self.wanted_have += 1;
        }
        let mut retier = false;
        for f in self.files_of(piece) {
            self.file_missing[f] -= 1;
            if self.file_missing[f] == 0 {
                self.completed.push(f);
                // The files behind it in the order move up a place.
                retier |= self.file_order.is_some();
            }
        }
        if retier {
            self.retier();
        }
    }

    /// Record that `piece`, which was had, no longer is.
    fn lost(&mut self, piece: u32) {
        let mut retier = false;
        for f in self.files_of(piece) {
            retier |= self.file_missing[f] == 0 && self.file_order.is_some();
            self.file_missing[f] += 1;
        }
        if retier {
            self.retier();
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

        // 1. Pieces in progress: those a stream's reader is waiting for, then the most
        //    complete.
        let waited_for = self.stream_needs();
        let mut partials: Vec<(bool, u32, u32)> = self
            .partial
            .iter()
            .filter(|(p, part)| has.get(**p) && part.missing() && allowed(part))
            .map(|(p, part)| (waited_for.contains(p), part.received, *p))
            .collect();
        partials.sort_unstable_by(|a, b| b.0.cmp(&a.0).then(b.1.cmp(&a.1)).then(a.2.cmp(&b.2)));
        for (_, _, piece) in partials {
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
        !self.order.is_empty()
    }

    /// The next piece to start for a peer with `has`: the best in `order`, unless a
    /// stream's next piece is at least as good (the same priority and tier, or better).
    fn next_fresh(&self, has: &Bitfield) -> Option<u32> {
        let mut best = self.order.iter().copied().find(|&p| has.get(p));
        if self.first_last
            && let Some(edge) = self.next_edge(has)
            && best.is_none_or(|b| self.priority[edge as usize] >= self.priority[b as usize])
        {
            best = Some(edge);
        }
        let class = |p: u32| self.key(p) / KEYS_PER_TIER;
        let streamed = self
            .streams
            .iter()
            .filter_map(|s| {
                self.stream_order(s)
                    .find(|&p| self.pos[p as usize] != NOT_IN_ORDER && has.get(p))
            })
            .min_by_key(|&p| class(p));
        match (best, streamed) {
            (Some(b), Some(s)) if class(s) <= class(b) => Some(s),
            (b, s) => b.or(s),
        }
    }

    /// The first wanted, not yet started first or last piece of a file that the peer has,
    /// by priority, then file by file.
    fn next_edge(&self, has: &Bitfield) -> Option<u32> {
        (0..self.layout.files().len())
            .filter(|&f| self.layout.files()[f].length > 0)
            .flat_map(|f| {
                let pieces = self.layout.file_pieces(f);
                [pieces.start, pieces.end.saturating_sub(1)]
            })
            .filter(|&p| self.pos[p as usize] != NOT_IN_ORDER && has.get(p))
            .max_by_key(|&p| (self.priority[p as usize], std::cmp::Reverse(p)))
    }

    /// The piece holding byte `byte` of file `file` (clamped to the file).
    fn piece_at(&self, file: usize, byte: u64) -> u32 {
        let span = &self.layout.files()[file];
        let at = span.offset + byte.min(span.length.saturating_sub(1));
        u32::try_from(at / self.layout.piece_length()).expect("piece index fits u32")
    }

    /// A stream's pieces in the order its reader wants them: the file's start and end,
    /// then from the playhead to the end, then from the start to the playhead. Pieces
    /// already had are skipped; a piece may come up twice (the first time takes it).
    fn stream_order(&self, s: &Stream) -> impl Iterator<Item = u32> + '_ {
        let len = self.layout.files()[s.file].length;
        let pieces = self.layout.file_pieces(s.file);
        let tail = (len / STREAM_TAIL_DIVISOR).clamp(STREAM_TAIL_MIN, STREAM_TAIL_MAX);
        let head = pieces.start..(self.piece_at(s.file, STREAM_HEAD - 1) + 1).min(pieces.end);
        let tail = self.piece_at(s.file, len.saturating_sub(tail))..pieces.end;
        let at = self.piece_at(s.file, s.from);
        head.chain(tail)
            .chain(at..pieces.end)
            .chain(pieces.start..at)
            .filter(|&p| !self.have.get(p))
    }

    /// For each stream, the first piece from its playhead on that we don't have: what its
    /// reader is waiting for.
    fn stream_needs(&self) -> Vec<u32> {
        self.streams
            .iter()
            .filter_map(|s| {
                let end = self.layout.file_pieces(s.file).end;
                (self.piece_at(s.file, s.from)..end).find(|&p| !self.have.get(p))
            })
            .collect()
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
            self.lost(piece);
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

    /// Whether `piece` has been started and isn't verified yet.
    pub fn in_progress(&self, piece: u32) -> bool {
        self.partial.contains_key(&piece)
    }

    /// Start a wanted piece with some of its blocks already in hand (read from a file the
    /// content library supplied, say): only the rest is requested. Being in progress, it
    /// is finished before new pieces are started. Returns whether every block is in, so
    /// the piece can be verified straight away. Does nothing (and returns `false`) for a
    /// piece that is had, unwanted or already in progress.
    pub fn prefill(&mut self, piece: u32, blocks: &[u32]) -> bool {
        if !self.is_wanted(piece) || self.partial.contains_key(&piece) {
            return false;
        }
        self.start_piece(piece);
        let part = self.partial.get_mut(&piece).expect("just started");
        for &b in blocks {
            if let Some(slot) = part.blocks.get_mut(b as usize)
                && *slot != Block::Received
            {
                *slot = Block::Received;
                part.received += 1;
            }
        }
        part.received as usize == part.blocks.len()
    }

    // ----- ordering internals -----------------------------------------------------------

    fn in_order_set(&self, piece: u32) -> bool {
        self.is_wanted(piece) && !self.partial.contains_key(&piece) && !self.held.get(piece)
    }

    fn key(&self, piece: u32) -> u32 {
        let prio = u32::from(self.priority[piece as usize]);
        debug_assert!(prio > 0);
        let tier = u32::from(self.tier[piece as usize]);
        let avail = self.availability[piece as usize].min(AVAILABILITY_CAP);
        (u32::from(MAX_PRIORITY) - prio) * KEYS_PER_PRIORITY + tier * KEYS_PER_TIER + avail
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

    /// A prefilled piece asks only for the blocks it lacks, and goes before new pieces.
    #[test]
    fn prefilled_pieces_request_only_missing_blocks() {
        let mut p = picker(4);
        assert!(!p.prefill(2, &[0]), "two blocks a piece, one prefilled");
        assert!(p.in_progress(2));
        assert!(!p.prefill(2, &[1]), "already in progress");
        p.check_invariants();
        let blocks = p.pick(1, &Bitfield::full(4), 4, 0);
        assert_eq!(blocks.len(), 1, "{blocks:?}");
        assert_eq!((blocks[0].piece, blocks[0].offset), (2, BLOCK_LEN));
        assert!(matches!(
            p.block_received(1, &blocks[0]),
            Received::New {
                piece_complete: true,
                ..
            }
        ));
        p.piece_verified(2, true);
        assert!(!p.prefill(2, &[0]), "had");
        // Every block in hand: ready to verify at once.
        assert!(p.prefill(3, &[0, 1]));
        p.check_invariants();
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

    /// `files` files of `pieces_each` whole pieces of 32 KiB.
    fn multi_file(files: usize, pieces_each: u32) -> Picker {
        let piece_len = 32 * 1024;
        let mut b = TorrentBuilder::new("t", Version::V1).piece_length(piece_len);
        let names: Vec<String> = (0..files).map(|i| format!("f{i}")).collect();
        for name in &names {
            let len = usize::try_from(u64::from(pieces_each) * piece_len).unwrap();
            b = b.file(FileSource::from_bytes(&[name.as_str()], vec![0; len]));
        }
        let t = Torrent::from_bytes(&b.build().unwrap()).unwrap();
        let layout = Layout::from_info(&t.info);
        let n = layout.piece_count();
        Picker::new(layout, Bitfield::new(n), 7)
    }

    #[test]
    fn first_and_last_pieces_come_first() {
        let mut p = multi_file(3, 4);
        let all = Bitfield::full(12);
        p.set_first_last(true);
        let got = starts(&mut p, &all, 6);
        assert_eq!(got, [0, 3, 4, 7, 8, 11]);
        p.check_invariants();
        // A skipped file's ends aren't fetched; a higher priority's ends come first.
        let mut p = multi_file(3, 4);
        p.set_first_last(true);
        p.set_priorities(
            &[DEFAULT_PRIORITY; 4]
                .iter()
                .chain(&[0; 4])
                .chain(&[7; 4])
                .copied()
                .collect::<Vec<_>>(),
        );
        assert_eq!(starts(&mut p, &all, 2), [8, 11]);
        assert_eq!(starts(&mut p, &all, 1), [9]);
        let next = starts(&mut p, &all, 3);
        assert_eq!(next[..1], [10]);
        assert!(next[1..].contains(&0) && next[1..].contains(&3), "{next:?}");
    }

    /// Start pieces one at a time for a peer with `has`, returning them in order.
    fn starts(p: &mut Picker, has: &Bitfield, count: usize) -> Vec<u32> {
        let mut out = Vec::new();
        for _ in 0..count {
            let blocks = p.pick(1, has, 1000, 1);
            let Some(b) = blocks.first() else { break };
            assert!(blocks.iter().all(|x| x.piece == b.piece), "{blocks:?}");
            out.push(b.piece);
        }
        out
    }

    #[test]
    fn file_order_takes_the_first_unfinished_file_first() {
        let mut p = multi_file(3, 4);
        let all = Bitfield::full(12);
        // Make file 0 the rarest, so rarity alone would pick it first.
        p.peer_bitfield(&has(12, &[4, 5, 6, 7, 8, 9, 10, 11]));
        p.set_file_order(Some(vec![2, 0, 1]));
        p.check_invariants();
        let mut got = starts(&mut p, &all, 12);
        let (a, rest) = got.split_at_mut(4);
        let (b, c) = rest.split_at_mut(4);
        for part in [&mut *a, &mut *b, &mut *c] {
            part.sort_unstable();
        }
        assert_eq!(got, [8, 9, 10, 11, 0, 1, 2, 3, 4, 5, 6, 7]);
        p.check_invariants();
    }

    /// A preference, not a rule: a peer without the first file's pieces still gets work.
    #[test]
    fn file_order_uses_peers_that_lack_the_first_file() {
        let mut p = multi_file(3, 4);
        p.set_file_order(Some(vec![0, 1, 2]));
        let only_last = has(12, &[8, 9, 10, 11]);
        let got = starts(&mut p, &only_last, 1);
        assert!((8..12).contains(&got[0]), "{got:?}");
    }

    #[test]
    fn priority_beats_file_order() {
        let mut p = multi_file(3, 4);
        p.set_file_order(Some(vec![0, 1, 2]));
        for piece in 8..12 {
            p.set_priority(piece, MAX_PRIORITY);
        }
        p.check_invariants();
        let mut got = starts(&mut p, &Bitfield::full(12), 4);
        got.sort_unstable();
        assert_eq!(got, [8, 9, 10, 11]);
    }

    /// Only the first few unfinished files are told apart; as files complete, later files
    /// move up into their own places.
    #[test]
    fn completed_files_are_reported_and_later_files_move_up() {
        let mut p = multi_file(10, 1);
        p.set_file_order(Some((0..10).collect()));
        assert_eq!(p.tier[8], 7);
        assert_eq!(p.tier[9], 7);
        p.mark_have(0);
        assert!(p.file_complete(0));
        assert!(!p.file_complete(1));
        assert_eq!(p.take_completed_files(), [0]);
        assert!(p.take_completed_files().is_empty());
        assert_eq!(p.tier[1], 0);
        assert_eq!(p.tier[7], 6);
        assert_eq!(p.tier[8], 7);
        p.mark_missing(0);
        assert!(!p.file_complete(0));
        assert_eq!(p.tier[0], 0);
        assert_eq!(p.tier[7], 7);
        p.check_invariants();
    }

    /// A piece spanning two files counts toward both.
    #[test]
    fn a_shared_piece_completes_both_files() {
        let bytes = TorrentBuilder::new("t", Version::V1)
            .piece_length(32 * 1024)
            .file(FileSource::from_bytes(&["a"], vec![0; 40 * 1024]))
            .file(FileSource::from_bytes(&["b"], vec![0; 24 * 1024]))
            .build()
            .unwrap();
        let layout = Layout::from_info(&Torrent::from_bytes(&bytes).unwrap().info);
        let mut p = Picker::new(layout, Bitfield::new(2), 7);
        p.mark_have(1);
        assert!(p.take_completed_files() == [1], "b lies in piece 1 only");
        p.mark_have(0);
        assert_eq!(p.take_completed_files(), [0]);
    }

    /// One 10 MiB file in 256 KiB pieces (40 pieces).
    fn stream_picker() -> Picker {
        Picker::new(layout(40, 256 * 1024), Bitfield::new(40), 7)
    }

    #[test]
    fn a_stream_fetches_start_end_then_from_the_playhead() {
        let mut p = stream_picker();
        let all = Bitfield::full(40);
        p.set_stream(0, Some(20 * 256 * 1024));
        let got = starts(&mut p, &all, 40);
        let expected: Vec<u32> = (0..4).chain(31..40).chain(20..31).chain(4..20).collect();
        assert_eq!(got, expected);
        p.check_invariants();
    }

    /// Seeking moves where fetching continues.
    #[test]
    fn a_stream_follows_its_playhead() {
        let mut p = stream_picker();
        let all = Bitfield::full(40);
        p.set_stream(0, Some(0));
        assert_eq!(
            starts(&mut p, &all, 16),
            (0..4).chain(31..40).chain(4..7).collect::<Vec<_>>()
        );
        p.set_stream(0, Some(25 * 256 * 1024 + 7));
        assert_eq!(p.streams(), [(0, 25 * 256 * 1024 + 7)]);
        assert_eq!(starts(&mut p, &all, 2), [25, 26]);
        p.set_stream(0, None);
        assert!(p.streams().is_empty());
    }

    /// The piece a reader waits for is finished before more complete pieces.
    #[test]
    fn the_piece_a_reader_waits_for_goes_first() {
        let mut p = stream_picker();
        let all = Bitfield::full(40);
        assert!(!p.prefill(10, &(0..12).collect::<Vec<_>>()));
        assert!(!p.prefill(20, &[]));
        p.set_stream(0, Some(20 * 256 * 1024));
        let blocks = p.pick(1, &all, 1, 0);
        assert_eq!(blocks[0].piece, 20);
        p.set_stream(0, None);
        let blocks = p.pick(1, &all, 1, 0);
        assert_eq!(blocks[0].piece, 10);
    }

    /// A stream yields to a higher priority, and to an earlier file in the file order.
    #[test]
    fn a_stream_keeps_to_priority_and_file_order() {
        let mut p = multi_file(2, 4);
        let all = Bitfield::full(8);
        p.set_stream(1, Some(0));
        assert_eq!(starts(&mut p, &all, 1), [4], "ties go to the stream");
        p.set_file_order(Some(vec![0, 1]));
        assert!((0..4).contains(&starts(&mut p, &all, 1)[0]));
        p.set_file_order(None);
        for piece in 0..4 {
            p.set_priority(piece, MAX_PRIORITY);
        }
        assert!((0..4).contains(&starts(&mut p, &all, 1)[0]));
        for piece in 0..4 {
            p.set_priority(piece, 0);
        }
        assert_eq!(starts(&mut p, &all, 1), [5]);
        p.check_invariants();
    }

    #[test]
    fn file_order_survives_random_churn() {
        use rand::RngExt;
        let mut p = multi_file(9, 3);
        let n = 27;
        let mut rng = StdRng::seed_from_u64(7);
        for _ in 0..3_000 {
            let piece = rng.random_range(0..n);
            match rng.random_range(0..8) {
                0 => p.peer_has(piece),
                1 => p.peer_lost(piece),
                2 => p.set_priority(piece, rng.random_range(0..=MAX_PRIORITY)),
                3 => p.mark_have(piece),
                4 => p.mark_missing(piece),
                5 => {
                    let mut order: Vec<usize> = (0..9).collect();
                    order.shuffle(&mut rng);
                    p.set_file_order(rng.random_bool(0.8).then_some(order));
                }
                6 => p.set_stream(
                    rng.random_range(0..9),
                    rng.random_bool(0.7).then(|| rng.random_range(0..96 * 1024)),
                ),
                _ => {
                    let _ = p.pick(rng.random_range(0..3), &Bitfield::full(n), 4, 1);
                }
            }
            p.check_invariants();
            for f in 0..9 {
                let missing = p.layout.file_pieces(f).any(|q| !p.have.get(q));
                assert_eq!(p.file_complete(f), !missing);
            }
        }
    }

    #[test]
    fn ordering_survives_random_churn() {
        use rand::RngExt;
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
