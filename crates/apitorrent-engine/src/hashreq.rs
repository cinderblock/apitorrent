//! BEP 52 hash requests: getting a file's piece layer from peers, and giving ours.
//!
//! A v2 torrent's `.torrent` carries each file's piece layer, but a magnet link's info
//! dictionary doesn't. Without the layer a piece of a multi-piece file can't be verified,
//! so it's fetched from peers in chunks of up to 512 hashes, each with the uncle hashes
//! that prove it against the file's `pieces root`.
//!
//! The spec is ambiguous by one about which proof layers are left out of a response. The
//! check here doesn't depend on it: whatever uncle hashes arrive are folded upward from the
//! chunk's subtree root, and the chunk is accepted only if that lands exactly on the root.
//! (A response with one redundant leading uncle is tried both ways.)

use apitorrent_metainfo::Sha256Hash;
use apitorrent_metainfo::merkle::{BLOCK_SIZE, pad_hash, root_from_layer};
use apitorrent_wire::HashRequest;

/// The most hashes asked for at once (BEP 52: "SHOULD NOT be greater than 512").
pub const MAX_CHUNK: u32 = 512;

/// The shape of one file's tree at the piece layer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LayerShape {
    pub root: Sha256Hash,
    /// Pieces in the file: the real length of its piece layer.
    pub pieces: u32,
    /// The piece layer's width once padded to a power of two.
    pub width: u32,
    /// The base layer number of the piece layer (0 is the 16 KiB leaves).
    pub base_layer: u32,
    /// The hash standing in for pieces beyond the end of the file.
    pub pad: Sha256Hash,
}

impl LayerShape {
    /// The shape for a file of `length` bytes in a torrent with `piece_length` pieces.
    /// `None` for files that fit in one piece: they have no piece layer.
    pub fn new(root: Sha256Hash, length: u64, piece_length: u64) -> Option<Self> {
        if length <= piece_length {
            return None;
        }
        let blocks_per_piece = piece_length / BLOCK_SIZE;
        let leaves = length.div_ceil(BLOCK_SIZE).next_power_of_two();
        let width = u32::try_from((leaves / blocks_per_piece).max(1)).ok()?;
        let pieces = u32::try_from(length.div_ceil(piece_length)).ok()?;
        let base_layer = blocks_per_piece.trailing_zeros();
        Some(Self {
            root,
            pieces,
            width,
            base_layer,
            pad: pad_hash(base_layer),
        })
    }

    /// The tree's height above the piece layer.
    pub fn height(&self) -> u32 {
        self.width.trailing_zeros()
    }

    /// Hashes per request.
    pub fn chunk_len(&self) -> u32 {
        self.width.clamp(2, MAX_CHUNK)
    }

    /// How many requests cover the layer.
    pub fn chunks(&self) -> u32 {
        self.pieces.div_ceil(self.chunk_len())
    }

    /// The request for chunk `chunk`. A chunk that is the whole tree needs no proof.
    pub fn request(&self, chunk: u32) -> HashRequest {
        let length = self.chunk_len();
        HashRequest {
            pieces_root: self.root,
            base_layer: self.base_layer,
            index: chunk * length,
            length,
            proof_layers: if length >= self.width {
                0
            } else {
                self.height()
            },
        }
    }

    /// Check a response to [`LayerShape::request`]. Returns the chunk's hashes (the
    /// padding beyond the file included) if they prove out against the root.
    pub fn verify(&self, req: &HashRequest, hashes: &[Sha256Hash]) -> Option<Vec<Sha256Hash>> {
        if req.pieces_root != self.root
            || req.base_layer != self.base_layer
            || !req.length.is_power_of_two()
            || req.length < 2
            || !req.index.is_multiple_of(req.length)
        {
            return None;
        }
        let len = req.length as usize;
        if hashes.len() < len {
            return None;
        }
        let (base, uncles) = hashes.split_at(len);
        let subtree = root_from_layer(base, len, self.pad);
        let level = req.length.trailing_zeros();
        let needed = self.height().checked_sub(level)? as usize;
        let attempts: &[&[Sha256Hash]] = if uncles.len() == needed + 1 {
            &[uncles, &uncles[1..]]
        } else {
            &[uncles]
        };
        for &uncles in attempts {
            if uncles.len() != needed {
                continue;
            }
            let mut node = subtree;
            let mut pos = req.index / req.length;
            for uncle in uncles {
                node = if pos.is_multiple_of(2) {
                    Sha256Hash::pair(&node, uncle)
                } else {
                    Sha256Hash::pair(uncle, &node)
                };
                pos /= 2;
            }
            if node == self.root {
                return Some(base.to_vec());
            }
        }
        None
    }

    /// Answer a peer's request from our copy of the layer. `None` means reject: we only
    /// serve the piece layer, and only well-formed requests within the tree.
    pub fn answer(&self, req: &HashRequest, layer: &[Sha256Hash]) -> Option<Vec<Sha256Hash>> {
        if req.pieces_root != self.root
            || req.base_layer != self.base_layer
            || layer.len() != self.pieces as usize
            || !req.length.is_power_of_two()
            || req.length < 2
            || req.length > MAX_CHUNK
            || !req.index.is_multiple_of(req.length)
            || u64::from(req.index) + u64::from(req.length) > u64::from(self.width)
        {
            return None;
        }
        // Every level of the padded tree, from the piece layer up to the root.
        let mut levels: Vec<Vec<Sha256Hash>> = Vec::new();
        let mut row: Vec<Sha256Hash> = layer.to_vec();
        row.resize(self.width as usize, self.pad);
        levels.push(row);
        while levels.last().is_some_and(|r| r.len() > 1) {
            let r = levels.last().expect("non-empty");
            let next = r
                .chunks(2)
                .map(|p| Sha256Hash::pair(&p[0], &p[1]))
                .collect();
            levels.push(next);
        }
        let start = req.index as usize;
        let mut out = levels[0][start..start + req.length as usize].to_vec();
        let level = req.length.trailing_zeros();
        let top = req.proof_layers.min(self.height());
        let mut pos = req.index / req.length;
        for l in level..top {
            let sibling = (pos ^ 1) as usize;
            out.push(levels[l as usize][sibling]);
            pos /= 2;
        }
        Some(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use apitorrent_metainfo::merkle::root_from_piece_layer;

    fn layer(n: u32) -> Vec<Sha256Hash> {
        (0..n)
            .map(|i| Sha256Hash::digest(&i.to_le_bytes()))
            .collect()
    }

    /// A file of `pieces` 64 KiB pieces.
    fn shape(pieces: u32) -> (LayerShape, Vec<Sha256Hash>) {
        let piece_length = 64 * 1024;
        let length = u64::from(pieces) * piece_length - 100;
        let l = layer(pieces);
        let root = root_from_piece_layer(&l, length, piece_length);
        (LayerShape::new(root, length, piece_length).unwrap(), l)
    }

    #[test]
    fn small_files_have_no_layer() {
        assert!(LayerShape::new(Sha256Hash::ZERO, 64 * 1024, 64 * 1024).is_none());
    }

    #[test]
    fn whole_tree_in_one_request() {
        let (s, l) = shape(5);
        assert_eq!((s.width, s.chunk_len(), s.chunks()), (8, 8, 1));
        let req = s.request(0);
        assert_eq!(req.proof_layers, 0);
        let answer = s.answer(&req, &l).unwrap();
        let got = s.verify(&req, &answer).unwrap();
        assert_eq!(&got[..5], &l[..]);
        assert!(got[5..].iter().all(|h| *h == s.pad));
    }

    #[test]
    fn chunked_with_proofs() {
        let (s, l) = shape(1300);
        assert_eq!((s.width, s.chunk_len(), s.chunks()), (2048, 512, 3));
        let mut all = Vec::new();
        for c in 0..s.chunks() {
            let req = s.request(c);
            let answer = s.answer(&req, &l).unwrap();
            assert_eq!(answer.len(), 512 + 2, "two uncles from 512 up to 2048");
            all.extend(s.verify(&req, &answer).expect("chunk proves out"));
        }
        assert_eq!(&all[..1300], &l[..]);
    }

    #[test]
    fn tampering_is_caught() {
        let (s, l) = shape(1300);
        let req = s.request(1);
        let mut answer = s.answer(&req, &l).unwrap();
        answer[7] = Sha256Hash::digest(b"evil");
        assert!(s.verify(&req, &answer).is_none());
        let mut answer = s.answer(&req, &l).unwrap();
        let last = answer.len() - 1;
        answer[last] = Sha256Hash::digest(b"evil");
        assert!(s.verify(&req, &answer).is_none());
        let short = &s.answer(&req, &l).unwrap()[..513];
        assert!(s.verify(&req, short).is_none());
    }

    #[test]
    fn a_redundant_leading_uncle_is_tolerated() {
        let (s, l) = shape(1300);
        let req = s.request(2);
        let mut answer = s.answer(&req, &l).unwrap();
        answer.insert(512, Sha256Hash::digest(b"redundant"));
        assert!(s.verify(&req, &answer).is_some());
    }

    #[test]
    fn bad_requests_are_rejected() {
        let (s, l) = shape(1300);
        let mut req = s.request(0);
        req.index = 3;
        assert!(s.answer(&req, &l).is_none());
        let mut req = s.request(0);
        req.length = 1024;
        assert!(s.answer(&req, &l).is_none());
        let mut req = s.request(0);
        req.base_layer = 0;
        assert!(s.answer(&req, &l).is_none());
        let mut req = s.request(0);
        req.index = 2048;
        assert!(s.answer(&req, &l).is_none());
    }
}
